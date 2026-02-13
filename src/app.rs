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
use hyper::{
    service::{make_service_fn, service_fn},
    Body, Request, Response, Server as HttpServer, StatusCode,
};
use std::time::Duration; // Time için gerekli

pub struct App {
    config: Arc<AppConfig>,
}

// Sağlık Kontrolü (Liveness Probe)
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
        
        if config.env == "production" {
            subscriber.with(fmt::layer().json()).init();
        } else {
            subscriber.with(fmt::layer().compact()).init();
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

        // 1. Redis Bağlantısı (Kritik - Bu olmadan proxy state tutamaz, o yüzden burada fail olabilir)
        info!("Redis'e bağlanılıyor: {}", self.config.redis_url);
        let redis_client = redis::Client::open(self.config.redis_url.as_str())
            .context("Redis URL hatalı")?;
        
        // Redis için de basit bir retry loop koyalım
        let redis_conn = loop {
            match redis_client.get_multiplexed_async_connection().await {
                Ok(conn) => break Arc::new(Mutex::new(conn)),
                Err(e) => {
                    error!("❌ Redis bağlantı hatası: {}. 5 saniye sonra tekrar denenecek...", e);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        };
        info!("✅ Redis bağlantısı sağlandı.");

        // 2. Paylaşılan İstemci Konteyneri (Başlangıçta Boş)
        let clients_container = Arc::new(Mutex::new(None));

        // 3. gRPC Sunucusunu Başlat (ÖNCE SUNUCU!)
        let grpc_config = self.config.clone();
        let grpc_clients_ref = clients_container.clone(); // Servise referansı veriyoruz
        
        let grpc_server_handle = tokio::spawn(async move {
            let tls_config = load_server_tls_config(&grpc_config).await.expect("TLS hatası");
            
            // Servis artık Option<Clients> kabul ediyor
            let grpc_service = MyProxyService::new(grpc_config.clone(), grpc_clients_ref); 
            
            info!(address = %grpc_config.grpc_listen_addr, "🔐 gRPC sunucusu dinlemeye başlıyor...");
            
            GrpcServer::builder()
                .tls_config(tls_config).expect("TLS yapılandırma hatası")
                .add_service(ProxyServiceServer::new(grpc_service))
                .serve_with_shutdown(grpc_config.grpc_listen_addr, async {
                    shutdown_rx.recv().await;
                })
                .await
                .context("gRPC sunucusu çöktü")
        });

        // 4. Bağlantı Yöneticisi (Connection Manager Loop)
        // Bu blok, gRPC sunucusu ayağa kalktıktan sonra çalışır ve bağımlılıklara bağlanır.
        let config_clone = self.config.clone();
        let clients_container_clone = clients_container.clone(); // Doldurmak için referans
        let sip_shutdown_tx_clone = sip_shutdown_tx.clone();
        let redis_conn_clone = redis_conn.clone();
        
        // Ana akışı bloklamamak için spawn ediyoruz, ama SIP sunucusu buna bağlı.
        // Düzeltme: SIP sunucusu da clients'a ihtiyaç duyar.
        // Bu yüzden SIP sunucusunu başlatmadan önce bağlantıların kurulmasını beklemeliyiz (veya SIP sunucusu da lazy olmalı).
        // SIP Sunucusu "InternalClients" tipini istiyor, Option değil.
        // Bu yüzden burada BLOKLAYARAK (await) bağlantıyı bekleyeceğiz.
        // gRPC sunucusu ayrı thread'de olduğu için sorun olmaz.

        info!("⏳ Bağımlı servislere (Loopback dahil) bağlanılıyor...");
        
        // Sunucunun socket bind etmesi için kısa bir avans verelim
        tokio::time::sleep(Duration::from_millis(500)).await;

        let connected_clients = loop {
            match InternalClients::connect(&config_clone).await {
                Ok(c) => {
                    info!("✅ Tüm bağımlı servislere (Dialplan, Registrar, Loopback) başarıyla bağlanıldı.");
                    break c;
                },
                Err(e) => {
                    warn!("⚠️ Bağlantı hatası (Retry in 5s): {}", e);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        };

        // Bağlantı nesnesini servise enjekte et (Artık servis "unavailable" dönmeyi bırakacak)
        {
            let mut guard = clients_container_clone.lock().await;
            *guard = Some(connected_clients.clone());
        }

        // 5. SIP Sunucusunu Başlat (UDP)
        // Artık elimizde sağlam bir 'connected_clients' var.
        let sip_config = self.config.clone();
        let state = Arc::new(ProxyState::new()); 
        
        // SIP Engine için Mutex içine alıyoruz
        let sip_clients = Arc::new(Mutex::new(connected_clients));

        let sip_server = SipServer::new(sip_config, sip_clients, state, redis_conn_clone).await?;
        let sip_handle = tokio::spawn(async move {
            sip_server.run(sip_shutdown_rx).await;
        });

        // 6. HTTP Sunucusu (Health Check)
        let http_config = self.config.clone();
        let http_server_handle = tokio::spawn(async move {
            let addr = http_config.http_listen_addr;
            let make_svc = make_service_fn(|_conn| async {
                Ok::<_, Infallible>(service_fn(handle_http_request))
            });
            let server = HttpServer::bind(&addr).serve(make_svc).with_graceful_shutdown(async {
                http_shutdown_rx.await.ok();
            });
            info!(address = %addr, "🏥 HTTP sağlık kontrolü aktif.");
            if let Err(e) = server.await { error!(error = %e, "HTTP sunucusu hatası"); }
        });

        // Kapanış Sinyali
        let ctrl_c = async { tokio::signal::ctrl_c().await.expect("Ctrl+C hatası"); };
        
        tokio::select! {
            res = grpc_server_handle => { if let Err(e) = res? { error!("gRPC Error: {}", e); } },
            _res = sip_handle => { error!("SIP Server durdu"); },
            _res = http_server_handle => { error!("HTTP Server durdu"); },
            _ = ctrl_c => { warn!("Kapatma sinyali alındı."); },
        }

        // Temizlik
        let _ = shutdown_tx.send(()).await;
        let _ = sip_shutdown_tx_clone.send(()).await; // Clone kullanıyoruz çünkü orijinal move oldu
        let _ = http_shutdown_tx.send(());
        
        info!("Servis durduruldu.");
        Ok(())
    }
}