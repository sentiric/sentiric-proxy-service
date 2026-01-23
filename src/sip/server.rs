// sentiric-proxy-service/src/sip/server.rs

use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tracing::{error, info, warn};
use sentiric_sip_core::{SipTransport, parser};
use crate::config::AppConfig;
use crate::grpc::client::InternalClients;
use crate::sip::engine::ProxyEngine;

pub struct SipServer {
    config: Arc<AppConfig>,
    transport: Arc<SipTransport>,
    engine: ProxyEngine,
}

impl SipServer {
    pub async fn new(config: Arc<AppConfig>, clients: Arc<Mutex<InternalClients>>) -> anyhow::Result<Self> {
        let bind_addr = format!("{}:{}", config.sip_bind_ip, config.sip_port);
        let transport = SipTransport::new(&bind_addr).await?;
        
        Ok(Self {
            config: config.clone(),
            transport: Arc::new(transport),
            engine: ProxyEngine::new(clients, config),
        })
    }

    pub async fn run(self, mut shutdown_rx: mpsc::Receiver<()>) {
        info!("📡 Proxy SIP Listener aktif: {}:{}", self.config.sip_bind_ip, self.config.sip_port);

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
                                Ok(mut packet) => { // DÜZELTME: packet mutable yapıldı
                                    // Process
                                    if let Some((resp_packet, target_addr)) = self.engine.process_packet(&mut packet, src_addr).await {
                                        let dest = target_addr.unwrap_or(src_addr);
                                        let resp_bytes = resp_packet.to_bytes();
                                        
                                        if let Err(e) = self.transport.send(&resp_bytes, dest).await {
                                            error!("Failed to send SIP packet to {}: {}", dest, e);
                                        }
                                    }
                                },
                                Err(e) => {
                                    if len > 4 { 
                                        warn!("Malformed SIP packet from {}: {}", src_addr, e);
                                    }
                                }
                            }
                        },
                        Err(e) => {
                            error!("UDP Receive Error: {}", e);
                        }
                    }
                }
            }
        }
    }
}