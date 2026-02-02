// sentiric-proxy-service/src/app.rs

use crate::config::AppConfig;
use crate::grpc::service::MyProxyService;
use crate::grpc::client::InternalClients;
use crate::tls::load_server_tls_config;
use crate::sip::server::{SipServer, ProxyState};
use anyhow::{Context, Result};
use sentiric_contracts::sentiric::sip::v1::proxy_service_server::ProxyServiceServer;
use std::convert::Infallible;
use std::env;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tonic::transport::Server as GrpcServer; 
use tracing::{error, info, warn};
use tracing_subscriber::{fmt, prelude::*, EnvFilter, Registry};

// [DÜZELTME] Hyper kütüphanesi eklendi (Cargo.toml'da var olmalı)
use hyper::{
    service::{make_service_fn, service_fn},
    Body, Request, Response, Server as HttpServer, StatusCode,
};

pub struct App {
    config: Arc<AppConfig>,
}

// Basit Health Check Handler
async fn handle_http_request(_req: Request<Body>) -> Result<Response<Body>, Infallible> {
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"status":"ok", "service": "proxy-service"}"#))
        .unwrap())
}

impl App {
    pub async fn bootstrap() -> Result<Self> {
        // Ortam değişkenlerini yükle
        dotenvy::dotenv().ok();
        let config = Arc::new(AppConfig::load_from_env().context("Konfigürasyon yüklenemedi")?);

        // Loglamayı başlat
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
        // Kapatma sinyalleri için kanallar
        let (shutdown_tx, mut shutdown_rx) = mpsc::channel(1);
        let (sip_shutdown_tx, sip_shutdown_rx) = mpsc::channel(1);
        let (http_shutdown_tx, http_shutdown_rx) = tokio::sync::oneshot::channel();

        // 1. Redis Bağlantısı (Stateful Proxy için kritik)
        info!("Redis'e bağlanılıyor: {}", self.config.redis_url);
        let redis_client = redis::Client::open(self.config.redis_url.as_str())
            .context("Redis URL hatalı")?;
        let redis_conn = redis_client.get_multiplexed_async_connection().await
            .context("Redis bağlantısı kurulamadı")?;
        let redis_conn = Arc::new(Mutex::new(redis_conn));
        
        // 2. Paylaşılan Durum ve gRPC İstemcileri
        let clients = Arc::new(Mutex::new(InternalClients::connect(&self.config).await?));
        let state = Arc::new(ProxyState::new()); 

        // 3. SIP Sunucusunu Başlat (UDP)
        let sip_config = self.config.clone();
        let sip_server = SipServer::new(sip_config, clients.clone(), state, redis_conn).await?;
        let sip_handle = tokio::spawn(async move {
            sip_server.run(sip_shutdown_rx).await;
        });

        // 4. gRPC Sunucusunu Başlat (TCP/TLS)
        let grpc_config = self.config.clone();
        let grpc_clients_clone = clients.clone(); // Servise client erişimi veriyoruz (Dialplan sorgusu için)
        
        let grpc_server_handle = tokio::spawn(async move {
            let tls_config = load_server_tls_config(&grpc_config).await.expect("TLS hatası");
            
            // Servis oluşturulurken client manager da veriliyor
            let grpc_service = MyProxyService::new(grpc_config.clone(), grpc_clients_clone); 
            
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

        // 5. HTTP Sunucusunu Başlat (Health Check)
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

        // Kapatma Sinyali Bekle (Ctrl+C)
        let ctrl_c = async { tokio::signal::ctrl_c().await.expect("Ctrl+C hatası"); };
        
        tokio::select! {
            res = grpc_server_handle => { if let Err(e) = res? { error!("gRPC Error: {}", e); } },
            _res = sip_handle => { error!("SIP Server durdu"); },
            _res = http_server_handle => { error!("HTTP Server durdu"); },
            _ = ctrl_c => { warn!("Kapatma sinyali alındı."); },
        }

        // Graceful Shutdown Tetikle
        let _ = shutdown_tx.send(()).await;
        let _ = sip_shutdown_tx.send(()).await;
        let _ = http_shutdown_tx.send(());
        
        info!("Servis durduruldu.");
        Ok(())
    }
}