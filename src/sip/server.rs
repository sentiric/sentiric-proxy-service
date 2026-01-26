// sentiric-proxy-service/src/sip/server.rs

use crate::config::AppConfig;
use crate::grpc::client::InternalClients;
use crate::sip::engine::ProxyEngine;
use anyhow::{anyhow, Result};
use sentiric_sip_core::{parser, SipTransport};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::lookup_host;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, warn};

// YENİ: DNS Cache ve Paylaşılan Durum
#[derive(Default)]
struct DnsCache {
    addr: Option<(SocketAddr, Instant)>,
}

pub struct ProxyState {
    b2bua_cache: Mutex<DnsCache>,
}

impl ProxyState {
    pub fn new() -> Self {
        Self {
            b2bua_cache: Mutex::new(DnsCache::default()),
        }
    }

    // B2BUA adresini TTL ile önbellekten çözer.
    pub async fn resolve_b2bua_addr(&self, hostname: &str) -> Result<SocketAddr> {
        let mut cache = self.b2bua_cache.lock().await;
        let now = Instant::now();

        if let Some((addr, timestamp)) = cache.addr {
            if now.duration_since(timestamp) < Duration::from_secs(60) {
                debug!("B2BUA DNS cache hit: {}", addr);
                return Ok(addr);
            }
        }

        info!("B2BUA DNS cache miss or stale, resolving: {}", hostname);
        let addr = lookup_host(hostname)
            .await?
            .next()
            .ok_or_else(|| anyhow!("'{}' için DNS kaydı bulunamadı", hostname))?;
        
        cache.addr = Some((addr, now));
        Ok(addr)
    }
}

pub struct SipServer {
    config: Arc<AppConfig>,
    transport: Arc<SipTransport>,
    engine: ProxyEngine,
}

impl SipServer {
    pub async fn new(
        config: Arc<AppConfig>,
        clients: Arc<Mutex<InternalClients>>,
        state: Arc<ProxyState>,
    ) -> Result<Self> {
        let bind_addr = format!("{}:{}", config.sip_bind_ip, config.sip_port);
        let transport = SipTransport::new(&bind_addr).await?;

        Ok(Self {
            config: config.clone(),
            transport: Arc::new(transport),
            engine: ProxyEngine::new(clients, config, state),
        })
    }

    pub async fn run(self, mut shutdown_rx: mpsc::Receiver<()>) {
        info!(
            "📡 Proxy SIP Listener aktif: {}:{}",
            self.config.sip_bind_ip, self.config.sip_port
        );

        let mut buf = vec![0u8; 65535];
        let socket = self.transport.get_socket();

        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    info!("🛑 SIP Server kapatılıyor...");
                    break;
                }

                res = socket.recv_from(&mut buf) => {
                    match res {
                        Ok((len, src_addr)) => {
                            let data = &buf[..len];

                            match parser::parse(data) {
                                Ok(mut packet) => {
                                    if let Some((resp_packet, target_addr_opt)) = self.engine.process_packet(&mut packet, src_addr).await {
                                        if let Some(dest) = target_addr_opt {
                                            let resp_bytes = resp_packet.to_bytes();
                                            if let Err(e) = self.transport.send(&resp_bytes, dest).await {
                                                error!("SIP paketi gönderilemedi {}: {}", dest, e);
                                            }
                                        }
                                        // target_addr_opt None ise, yanıt gerekmiyor demektir.
                                    }
                                },
                                Err(e) => {
                                    // Keep-alive veya boş paketleri görmezden gel
                                    if len > 4 {
                                        warn!("Hatalı SIP paketi {}: {}", src_addr, e);
                                    }
                                }
                            }
                        },
                        Err(e) => {
                            error!("UDP alma hatası: {}", e);
                        }
                    }
                }
            }
        }
    }
}