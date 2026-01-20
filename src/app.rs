// sentiric-proxy-service/src/app.rs
use crate::config::AppConfig;
use crate::grpc::service::MyProxyService;
use crate::grpc::client::InternalClients;
use crate::tls::load_server_tls_config;
use crate::sip::server::SipServer;
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

        let rust_log_env = env::var("RUST_LOG").unwrap_or_else(|_| config.rust_log.clone());
        let env_filter = EnvFilter::try_from_default_env().or_else(|_| EnvFilter::try_new(&rust_log_env))?;
        let subscriber = Registry::default().with(env_filter);
        
        if config.env == "development" {
            subscriber.with(fmt::layer().with_target(true).with_line_number(true)).init();
        } else {
            subscriber.with(fmt::layer().json().with_current_span(true).with_span_list(true)).init();
        }

        info!(
            service_name = "sentiric-proxy-service",
            version = %config.service_version,
            profile = %config.env,
            "🚀 Servis başlatılıyor..."
        );
        
        Ok(Self { config })
    }

    pub async fn run(self) -> Result<()> {
        let (shutdown_tx, mut shutdown_rx) = mpsc::channel(1);
        let (sip_shutdown_tx, sip_shutdown_rx) = mpsc::channel(1);
        let (http_shutdown_tx, http_shutdown_rx) = tokio::sync::oneshot::channel();

        // 1. gRPC İstemcilerini Başlat
        let clients = Arc::new(Mutex::new(InternalClients::connect(&self.config).await?));

        // 2. SIP Sunucusunu Başlat
        let sip_config = self.config.clone();
        let sip_server = SipServer::new(sip_config, clients.clone()).await?;
        let sip_handle = tokio::spawn(async move {
            sip_server.run(sip_shutdown_rx).await;
        });

        // 3. gRPC Sunucusunu Başlat
        let grpc_config = self.config.clone();
        let grpc_server_handle = tokio::spawn(async move {
            let tls_config = load_server_tls_config(&grpc_config).await.expect("TLS hatası");
            let grpc_service = MyProxyService {}; 
            
            info!(address = %grpc_config.grpc_listen_addr, "gRPC sunucusu başlatılıyor...");
            
            GrpcServer::builder()
                .tls_config(tls_config).expect("TLS yapılandırma hatası")
                .add_service(ProxyServiceServer::new(grpc_service))
                .serve_with_shutdown(grpc_config.grpc_listen_addr, async {
                    shutdown_rx.recv().await;
                })
                .await
                .context("gRPC sunucusu çöktü")
        });

        // 4. HTTP Sunucusunu Başlat
        let http_config = self.config.clone();
        let http_server_handle = tokio::spawn(async move {
            let addr = http_config.http_listen_addr;
            let make_svc = make_service_fn(|_conn| async {
                Ok::<_, Infallible>(service_fn(handle_http_request))
            });
            let server = HttpServer::bind(&addr).serve(make_svc).with_graceful_shutdown(async {
                http_shutdown_rx.await.ok();
            });
            info!(address = %addr, "HTTP sağlık kontrolü aktif.");
            if let Err(e) = server.await { error!(error = %e, "HTTP sunucusu hatası"); }
        });

        let ctrl_c = async { tokio::signal::ctrl_c().await.expect("Ctrl+C hatası"); };
        
        tokio::select! {
            res = grpc_server_handle => { if let Err(e) = res? { error!("gRPC Error: {}", e); } },
            res = sip_handle => { error!("SIP Server durdu"); },
            res = http_server_handle => { error!("HTTP Server durdu"); },
            _ = ctrl_c => { warn!("Kapatma sinyali alındı."); },
        }

        let _ = shutdown_tx.send(()).await;
        let _ = sip_shutdown_tx.send(()).await;
        let _ = http_shutdown_tx.send(());
        
        info!("Servis durduruldu.");
        Ok(())
    }
}