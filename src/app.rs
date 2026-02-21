use crate::config::AppConfig;
use crate::grpc::service::MyProxyService;
use crate::grpc::client::InternalClients;
use crate::tls::load_server_tls_config;
use crate::sip::server::{SipServer, ProxyState};
use crate::telemetry::SutsFormatter; // YENİ
use anyhow::{Context, Result};
use sentiric_contracts::sentiric::sip::v1::proxy_service_server::ProxyServiceServer;
use std::convert::Infallible;
use std::env;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tonic::transport::Server as GrpcServer; 
use tracing::{error, info, warn};
use tracing_subscriber::{fmt, prelude::*, EnvFilter, Registry};
use hyper::{
    service::{make_service_fn, service_fn},
    Body, Request, Response, Server as HttpServer, StatusCode,
};
use std::time::Duration;

pub struct App {
    config: Arc<AppConfig>,
}

async fn handle_http_request(_req: Request<Body>) -> Result<Response<Body>, Infallible> {
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"status":"ok", "service": "proxy-service"}"#))
        .unwrap())
}

impl App {
    pub async fn bootstrap() -> Result<Self> {
        dotenvy::dotenv().ok();
        let config = Arc::new(AppConfig::load_from_env().context("Konfigürasyon yüklenemedi")?);

        // --- SUTS v4.0 LOGGING SETUP ---
        let rust_log_env = env::var("RUST_LOG").unwrap_or_else(|_| config.rust_log.clone());
        let env_filter = EnvFilter::try_from_default_env().or_else(|_| EnvFilter::try_new(&rust_log_env))?;
        let subscriber = Registry::default().with(env_filter);
        
        if config.log_format == "json" {
            let suts_formatter = SutsFormatter::new(
                "proxy-service".to_string(),
                config.service_version.clone(),
                config.env.clone(),
                config.node_hostname.clone(),
            );
            subscriber.with(fmt::layer().event_format(suts_formatter)).init();
        } else {
            subscriber.with(fmt::layer().compact()).init();
        }
        // -------------------------------

        info!(
            event = "SYSTEM_STARTUP",
            service_name = "sentiric-proxy-service",
            version = %config.service_version,
            profile = %config.env,
            "🚀 Proxy Servisi Başlatılıyor (SUTS v4.0)"
        );
        
        Ok(Self { config })
    }

    pub async fn run(self) -> Result<()> {
        
        let (_shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
        let (sip_shutdown_tx, sip_shutdown_rx) = mpsc::channel(1);
        let (http_shutdown_tx, http_shutdown_rx) = tokio::sync::oneshot::channel();
        let (grpc_stop_tx, mut grpc_stop_rx) = mpsc::channel(1);

        // 1. Redis
        info!(event="REDIS_CONNECT", url=%self.config.redis_url, "Redis'e bağlanılıyor...");
        let redis_client = redis::Client::open(self.config.redis_url.as_str())?;
        let redis_conn = loop {
            match redis_client.get_multiplexed_async_connection().await {
                Ok(conn) => break Arc::new(Mutex::new(conn)),
                Err(e) => {
                    error!(event="REDIS_ERROR", error=%e, "Redis bağlantı hatası. Tekrar deneniyor...");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        };

        // 2. Logic & Clients
        let clients_container = Arc::new(Mutex::new(None));
        let routing_logic = Arc::new(MyProxyService::new(self.config.clone(), clients_container.clone()));

        // 3. gRPC Server
        let grpc_config = self.config.clone();
        let grpc_logic_ref = routing_logic.clone();
        let grpc_server_handle = tokio::spawn(async move {
            let tls_config = load_server_tls_config(&grpc_config).await.expect("TLS hatası");
            GrpcServer::builder()
                .tls_config(tls_config).expect("TLS hatası")
                .add_service(ProxyServiceServer::from_arc(grpc_logic_ref))
                .serve_with_shutdown(grpc_config.grpc_listen_addr, async {
                    let _ = grpc_stop_rx.recv().await;
                    info!(event="GRPC_SHUTDOWN", "gRPC sunucusu kapanıyor.");
                })
                .await
                .context("gRPC sunucusu çöktü")
        });

        // 4. Client Manager
        let config_clone = self.config.clone();
        let clients_container_clone = clients_container.clone();
        tokio::spawn(async move {
            loop {
                match InternalClients::connect(&config_clone).await {
                    Ok(c) => {
                        let mut guard = clients_container_clone.lock().await;
                        *guard = Some(c);
                        info!(event="CLIENTS_CONNECTED", "Dış servis bağlantıları sağlandı.");
                        break;
                    },
                    Err(e) => {
                        warn!(event="CLIENT_CONNECT_FAIL", error=%e, "Bağlantı hatası, tekrar deneniyor...");
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
            }
        });

        // 5. SIP Server
        let sip_config = self.config.clone();
        let state = Arc::new(ProxyState::new()); 
        let sip_server = SipServer::new(sip_config, state, redis_conn, routing_logic).await?;
        let sip_handle = tokio::spawn(async move {
            sip_server.run(sip_shutdown_rx).await;
        });

        // 6. HTTP Server
        let http_config = self.config.clone();
        let http_server_handle = tokio::spawn(async move {
            let addr = http_config.http_listen_addr;
            let make_svc = make_service_fn(|_conn| async {
                Ok::<_, Infallible>(service_fn(handle_http_request))
            });
            let server = HttpServer::bind(&addr).serve(make_svc).with_graceful_shutdown(async {
                http_shutdown_rx.await.ok();
            });
            info!(event="HTTP_START", address=%addr, "HTTP sunucusu aktif.");
            if let Err(e) = server.await { error!(error=%e, "HTTP sunucusu hatası"); }
        });

        let ctrl_c = async { tokio::signal::ctrl_c().await.expect("Ctrl+C hatası"); };
        
        tokio::select! {
            res = grpc_server_handle => { if let Err(e) = res? { error!("gRPC Error: {}", e); } },
            _res = sip_handle => { error!("SIP Server durdu"); },
            _res = http_server_handle => { error!("HTTP Server durdu"); },
            _ = ctrl_c => { warn!(event="SIGINT", "Kapatma sinyali alındı."); },
            _ = shutdown_rx.recv() => { warn!("Kapatma sinyali."); }
        }

        let _ = grpc_stop_tx.send(()).await;
        let _ = sip_shutdown_tx.send(()).await;
        let _ = http_shutdown_tx.send(());
        tokio::time::sleep(Duration::from_millis(500)).await;
        
        info!(event="SYSTEM_STOPPED", "Servis durduruldu.");
        Ok(())
    }
}