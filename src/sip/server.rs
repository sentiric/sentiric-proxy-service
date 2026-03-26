// sentiric-proxy-service/src/sip/server.rs

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
use tokio::sync::mpsc; 
use tracing::{debug, error, info, warn};
use dashmap::DashMap;

pub const DEFAULT_SIP_PORT: u16 = 5060;

pub struct ProxyState {
    dns_cache: DashMap<String, (SocketAddr, Instant)>,
}

impl ProxyState {
    pub fn new() -> Self {
        Self {
            dns_cache: DashMap::new(),
        }
    }

    pub async fn resolve_addr(&self, hostname: &str) -> Result<SocketAddr> {
        if let Ok(addr) = hostname.parse::<SocketAddr>() {
            return Ok(addr);
        }

        let now = Instant::now();
        
        if let Some(cached) = self.dns_cache.get(hostname) {
            let (addr, timestamp) = *cached;
            if now.duration_since(timestamp) < Duration::from_secs(60) {
                return Ok(addr);
            }
        }

        debug!(event="DNS_RESOLVE_NETWORK", host=%hostname, "DNS ağdan çözümleniyor...");
        let addr = lookup_host(hostname)
            .await?
            .next()
            .ok_or_else(|| anyhow!("DNS kaydı bulunamadı: {}", hostname))?;
        
        self.dns_cache.insert(hostname.to_string(), (addr, now));
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
                            
                            if len <= 4 && buf[..len].iter().all(|&b| b == b'\r' || b == b'\n' || b == 0) {
                                continue;
                            }

                            let data = &buf[..len];

                            match parser::parse(data) {
                                Ok(mut packet) => {
                                    let call_id = packet.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
                                    
                                    // [ARCH-COMPLIANCE] TYPE FIX: status_code is natively u16
                                    let method = if packet.is_request() { 
                                        packet.method.as_str().to_string() 
                                    } else { 
                                        format!("RESPONSE/{}", packet.status_code) 
                                    };
                                    
                                    debug!(
                                        event = "SIP_PACKET_RECEIVED",
                                        sip.call_id = %call_id,
                                        sip.method = %method,
                                        net.src.ip = %src_addr.ip(),
                                        net.src.port = src_addr.port(),
                                        "📥 SIP paketi alındı"
                                    );

                                    if let Some((resp_packet, target_addr_opt)) = self.engine.process_packet(&mut packet, src_addr).await {
                                        if let Some(dest) = target_addr_opt {
                                            
                                            //[ARCH-COMPLIANCE] TYPE FIX
                                            let resp_method = if resp_packet.is_request() { 
                                                resp_packet.method.as_str().to_string() 
                                            } else { 
                                                format!("RESPONSE/{}", resp_packet.status_code) 
                                            };

                                            info!(
                                                event = "SIP_PACKET_SENT",
                                                sip.call_id = %call_id,
                                                sip.method = %resp_method,
                                                net.dst.ip = %dest.ip(),
                                                net.dst.port = dest.port(),
                                                "📤[PROXY->NEXT] Paket iletiliyor"
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