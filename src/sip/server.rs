// sentiric-proxy-service/src/sip/server.rs

use crate::config::AppConfig;
use crate::grpc::client::InternalClients;
use crate::sip::engine::{ProxyEngine, RedisConn};
use anyhow::{anyhow, Result};
use sentiric_sip_core::{parser, SipTransport};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::lookup_host;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, warn};

pub const DEFAULT_SIP_PORT: u16 = 5060;

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

    pub async fn resolve_b2bua_addr(&self, hostname: &str) -> Result<SocketAddr> {
        if let Ok(addr) = hostname.parse::<SocketAddr>() {
            return Ok(addr);
        }

        let mut cache = self.b2bua_cache.lock().await;
        let now = Instant::now();

        if let Some((addr, timestamp)) = cache.addr {
            if now.duration_since(timestamp) < Duration::from_secs(60) {
                return Ok(addr);
            }
        }

        info!("DNS Çözümleniyor: {}", hostname);
        let addr = lookup_host(hostname)
            .await?
            .next()
            .ok_or_else(|| anyhow!("DNS kaydı yok: {}", hostname))?;
        
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
        redis: RedisConn,
    ) -> Result<Self> {
        let bind_addr = format!("{}:{}", config.sip_bind_ip, config.sip_port);
        let transport = SipTransport::new(&bind_addr).await?;

        Ok(Self {
            config: config.clone(),
            transport: Arc::new(transport),
            engine: ProxyEngine::new(clients, config, state, redis),
        })
    }

    pub async fn run(self, mut shutdown_rx: mpsc::Receiver<()>) {
        info!("📡 Proxy SIP Dinleyicisi Aktif: {}:{}", self.config.sip_bind_ip, self.config.sip_port);

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
                            if len < 4 { continue; }
                            
                            // Keep-alive (CRLF) filtresi
                            if len <= 4 && buf[..len].iter().all(|&b| b == b'\r' || b == b'\n' || b == 0) {
                                debug!("💤 Keep-Alive paketi (Yoksayıldı) -> {}", src_addr);
                                continue;
                            }

                            let data = &buf[..len];

                            match parser::parse(data) {
                                Ok(mut packet) => {
                                    if let Some((resp_packet, target_addr_opt)) = self.engine.process_packet(&mut packet, src_addr).await {
                                        if let Some(dest) = target_addr_opt {
                                            let resp_bytes = resp_packet.to_bytes();
                                            if let Err(e) = self.transport.send(&resp_bytes, dest).await {
                                                error!("🔥 SIP paketi gönderilemedi {}: {}", dest, e);
                                            }
                                        }
                                    }
                                },
                                Err(e) => {
                                    warn!("⚠️ Bozuk SIP paketi {}: {}", src_addr, e);
                                }
                            }
                        },
                        Err(e) => {
                            error!("🔥 UDP Socket Hatası: {}", e);
                        }
                    }
                }
            }
        }
    }
}