// sentiric-proxy-service/src/app.rs
use crate::config::AppConfig;
use crate::error::ServiceError;
use crate::grpc::service::MyProxyService;
use crate::tls::load_server_tls_config;
use anyhow::{Context, Result, anyhow}; 
use sentiric_contracts::sentiric::sip::v1::proxy_service_server::ProxyServiceServer;
use std::convert::Infallible;
use std::env;
use std::sync::Arc;
use tokio::{net::UdpSocket, sync::mpsc};
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

// UDP dinleyicisi için yer tutucu fonksiyon.
async fn spawn_sip_udp_listener(config: Arc<AppConfig>, mut shutdown_rx: mpsc::Receiver<()>) -> Result<()> {
    info!(address = %config.sip_listen_addr, "UDP SIP dinleyici başlatılıyor...");
    
    let sock = UdpSocket::bind(config.sip_listen_addr).await
        .map_err(|e| anyhow!(ServiceError::Io(e)))?;
        
    let mut buf = [0; 65535];
    
    loop {
        tokio::select! {
            // Kapatma sinyali
            _ = shutdown_rx.recv() => {
                info!("UDP dinleyici kapatma sinyali aldı.");
                break;
            },
            // Gelen paket
            result = sock.recv_from(&mut buf) => {
                match result {
                    Ok((len, addr)) => {
                        info!(remote_addr = %addr, len = len, "SIP UDP paketi alındı. İşleniyor (Şimdilik atlanıyor)...");
                    },
                    Err(e) => {
                        error!(error = %e, "UDP soket hatası.");
                        if e.kind() != std::io::ErrorKind::Interrupted {
                            return Err(anyhow!(ServiceError::Io(e)));
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

impl App {
    pub async fn bootstrap() -> Result<Self> {
        dotenvy::dotenv().ok();
        let config = Arc::new(AppConfig::load_from_env().context("Konfigürasyon dosyası yüklenemedi")?);

        let rust_log_env = env::var("RUST_LOG")
            .unwrap_or_else(|_| config.rust_log.clone());
        
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
        
        // TODO: gRPC Client'ları (Registrar, B2BUA) burada başlatılacaktır.

        Ok(Self { config })
    }

    pub async fn run(self) -> Result<()> {
        let (grpc_shutdown_tx, mut grpc_shutdown_rx) = mpsc::channel(1);
        let (udp_shutdown_tx, udp_shutdown_rx) = mpsc::channel(1);
        let (http_shutdown_tx, http_shutdown_rx) = tokio::sync::oneshot::channel();
        
        // --- UDP Listener'ı Başlat ---
        let udp_listener_handle = tokio::spawn(spawn_sip_udp_listener(self.config.clone(), udp_shutdown_rx));

        // --- gRPC Sunucusunu Başlat ---
        let grpc_config = self.config.clone();
        let grpc_server_handle = tokio::spawn(async move {
            let tls_config = load_server_tls_config(&grpc_config).await.expect("TLS yapılandırması başarısız");
            let grpc_service = MyProxyService {}; 
            
            info!(address = %grpc_config.grpc_listen_addr, "Güvenli gRPC sunucusu dinlemeye başlıyor...");
            
            GrpcServer::builder()
                .tls_config(tls_config).expect("TLS yapılandırma hatası")
                .add_service(ProxyServiceServer::new(grpc_service))
                .serve_with_shutdown(grpc_config.grpc_listen_addr, async {
                    grpc_shutdown_rx.recv().await;
                    info!("gRPC sunucusu için kapatma sinyali alındı.");
                })
                .await
                .context("gRPC sunucusu hatayla sonlandı")
        });

        // --- HTTP Sunucusunu Başlat (Health Check) ---
        let http_config = self.config.clone();
        let http_server_handle = tokio::spawn(async move {
            let addr = http_config.http_listen_addr;
            let make_svc = make_service_fn(|_conn| async {
                Ok::<_, Infallible>(service_fn(handle_http_request))
            });

            let server = HttpServer::bind(&addr)
                .serve(make_svc)
                .with_graceful_shutdown(async {
                    http_shutdown_rx.await.ok();
                });
            
            info!(address = %addr, "HTTP sağlık kontrol sunucusu dinlemeye başlıyor...");
            if let Err(e) = server.await {
                error!(error = %e, "HTTP sunucusu hatayla sonlandı");
            }
        });

        let ctrl_c = async { tokio::signal::ctrl_c().await.expect("Ctrl+C dinleyicisi kurulamadı"); };
        
        // Bu blokta res, JoinHandle'ın çıktısıdır: Result<Result<...>, JoinError>
        tokio::select! {
            res = grpc_server_handle => {
                match res {
                    Ok(inner_res) => {
                        if let Err(e) = inner_res {
                            return Err(e);
                        }
                        error!("gRPC sunucusu beklenmedik şekilde sonlandı!");
                    }
                    Err(join_err) => {
                        return Err(anyhow!("gRPC sunucu görevi panic'ledi: {join_err}"));
                    }
                }
            },
            res = udp_listener_handle => {
                match res {
                    Ok(inner_res) => {
                        if let Err(e) = inner_res {
                            return Err(e);
                        }
                        error!("UDP dinleyici beklenmedik şekilde sonlandı!");
                    }
                    Err(join_err) => {
                        return Err(anyhow!("UDP dinleyici görevi panic'ledi: {join_err}"));
                    }
                }
            },
            res = http_server_handle => {
                match res {
                    Ok(_) => {
                        error!("HTTP sunucusu beklenmedik şekilde sonlandı!");
                    }
                    Err(join_err) => {
                        return Err(anyhow!("HTTP sunucu görevi panic'ledi: {join_err}"));
                    }
                }
            },
            _ = ctrl_c => {
                // Ctrl+C alındı, graceful shutdown
            },
        }


        warn!("Kapatma sinyali alındı. Graceful shutdown başlatılıyor...");
        let _ = grpc_shutdown_tx.send(()).await;
        let _ = udp_shutdown_tx.send(()).await;
        let _ = http_shutdown_tx.send(());
        
        info!("Servis başarıyla durduruldu.");
        Ok(())
    }
}