// src/sip/server.rs

use crate::config::AppConfig;
use crate::sip::engine::ProxyEngine;
use crate::sip::handlers::routing::RedisConn; 
use crate::grpc::service::MyProxyService;
use anyhow::{anyhow, Result};
use sentiric_sip_core::{parser, SipTransport, HeaderName};
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
    #[allow(dead_code)] 
    b2bua_cache: Mutex<DnsCache>,
}

impl ProxyState {
    pub fn new() -> Self {
        Self {
            b2bua_cache: Mutex::new(DnsCache::default()),
        }
    }

    pub async fn resolve_addr(&self, hostname: &str) -> Result<SocketAddr> {
        if let Ok(addr) = hostname.parse::<SocketAddr>() {
            return Ok(addr);
        }

        debug!(event="DNS_RESOLVE", host=%hostname, "DNS Çözümleniyor");
        let addr = lookup_host(hostname)
            .await?
            .next()
            .ok_or_else(|| anyhow!("DNS kaydı bulunamadı: {}", hostname))?;
        
        Ok(addr)
    }

    #[allow(dead_code)]
    pub async fn resolve_b2bua_addr(&self, hostname: &str) -> Result<SocketAddr> {
        let mut cache = self.b2bua_cache.lock().await;
        let now = Instant::now();

        if let Some((addr, timestamp)) = cache.addr {
            if now.duration_since(timestamp) < Duration::from_secs(60) {
                return Ok(addr);
            }
        }

        let addr = self.resolve_addr(hostname).await?;
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
        state: Arc<ProxyState>,
        redis: RedisConn,
        routing_logic: Arc<MyProxyService>,
    ) -> Result<Self> {
        let bind_addr = format!("{}:{}", config.sip_bind_ip, config.sip_port);
        let transport = SipTransport::new(&bind_addr).await?;

        Ok(Self {
            config: config.clone(),
            transport: Arc::new(transport),
            engine: ProxyEngine::new(config, state, redis, routing_logic),
        })
    }

    pub async fn run(self, mut shutdown_rx: mpsc::Receiver<()>) {
        info!(
            event = "SIP_SERVER_ACTIVE",
            bind = %format!("{}:{}", self.config.sip_bind_ip, self.config.sip_port),
            "📡 Proxy SIP Dinleyicisi Aktif"
        );

        let mut buf = vec![0u8; 65535];
        let socket = self.transport.get_socket();

        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    info!(event="SIP_SHUTDOWN", "SIP Sunucusu kapatılıyor...");
                    break;
                }

                res = socket.recv_from(&mut buf) => {
                    match res {
                        Ok((len, src_addr)) => {
                            if len < 4 { continue; }
                            
                            // Keep-Alive (CRLF) paketlerini yoksay
                            if len <= 4 && buf[..len].iter().all(|&b| b == b'\r' || b == b'\n' || b == 0) {
                                continue;
                            }

                            let data = &buf[..len];

                            match parser::parse(data) {
                                Ok(mut packet) => {
                                    // 1. INGRESS LOG (SUTS v4.0)
                                    // Observer için en önemli log. Trace başlangıcı.
                                    let call_id = packet.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
                                    let method = packet.method.as_str();
                                    
                                    debug!(
                                        event = "SIP_PACKET_RECEIVED",
                                        sip.call_id = %call_id, // -> Trace ID olacak
                                        sip.method = %method,
                                        net.src.ip = %src_addr.ip(),
                                        net.src.port = src_addr.port(),
                                        "📥 SIP paketi alındı"
                                    );

                                    // 2. ENGINE PROCESS
                                    if let Some((resp_packet, target_addr_opt)) = self.engine.process_packet(&mut packet, src_addr).await {
                                        if let Some(dest) = target_addr_opt {
                                            // 3. EGRESS LOG (SUTS v4.0)
                                            let resp_method = resp_packet.method.as_str();
                                            info!(
                                                event = "SIP_PACKET_SENT",
                                                sip.call_id = %call_id,
                                                sip.method = %resp_method,
                                                net.dst.ip = %dest.ip(),
                                                net.dst.port = dest.port(),
                                                "📤 [PROXY->NEXT] Paket iletiliyor"
                                            );

                                            let resp_bytes = resp_packet.to_bytes();
                                            if let Err(e) = self.transport.send(&resp_bytes, dest).await {
                                                error!(
                                                    event = "SIP_SEND_ERROR",
                                                    sip.call_id = %call_id,
                                                    net.dst.ip = %dest.ip(),
                                                    error = %e,
                                                    "🔥 SIP gönderim hatası"
                                                );
                                            }
                                        }
                                    }
                                },
                                Err(e) => {
                                    warn!(event="SIP_PARSE_ERROR", src=%src_addr, error=%e, "⚠️ Bozuk SIP paketi");
                                }
                            }
                        },
                        Err(e) => {
                            error!(event="UDP_ERROR", error=%e, "🔥 UDP Soket Hatası");
                        }
                    }
                }
            }
        }
    }
}