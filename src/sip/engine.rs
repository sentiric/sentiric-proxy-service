// sentiric-proxy-service/src/sip/engine.rs

use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, warn, error, instrument};
use sentiric_sip_core::{SipPacket, Method, HeaderName, Header}; 
use sentiric_contracts::sentiric::sip::v1::RegisterRequest;
use tonic::Request;
use crate::grpc::client::InternalClients;
use crate::sip::utils;
use crate::config::AppConfig;
use tokio::net::lookup_host;

pub struct ProxyEngine {
    clients: Arc<Mutex<InternalClients>>,
    config: Arc<AppConfig>,
}

impl ProxyEngine {
    pub fn new(clients: Arc<Mutex<InternalClients>>, config: Arc<AppConfig>) -> Self {
        Self { clients, config }
    }

    #[instrument(skip(self, packet), fields(method = %packet.method))]
    pub async fn process_packet(&self, packet: &SipPacket) -> Option<(SipPacket, Option<std::net::SocketAddr>)> {
        match packet.method {
            Method::Register => {
                if let Some(resp) = self.handle_register(packet).await {
                    return Some((resp, None)); // Yanıtı gönderene (SBC) geri dön
                }
                None
            },
            Method::Invite => self.handle_invite(packet).await,
            _ => None,
        }
    }

    async fn handle_register(&self, packet: &SipPacket) -> Option<SipPacket> {
        let to_header = utils::get_header(packet, HeaderName::To);
        let contact = utils::get_header(packet, HeaderName::Contact);
        
        let aor = utils::extract_aor(&to_header); 
        let contact_uri = utils::extract_aor(&contact); 

        info!("REGISTER Process: AOR={}, Contact={}", aor, contact_uri);

        let mut clients = self.clients.lock().await;
        
        let req = Request::new(RegisterRequest {
            sip_uri: aor,
            contact_uri: contact_uri,
            expires: 3600, 
        });

        match clients.registrar.register(req).await {
            Ok(_) => {
                info!("Registrar: Kayıt başarılı.");
                Some(self.create_response(packet, 200, "OK"))
            }
            Err(e) => {
                error!("Registrar Hatası: {}", e);
                Some(self.create_response(packet, 500, "Internal Server Error"))
            }
        }
    }

    async fn handle_invite(&self, packet: &SipPacket) -> Option<(SipPacket, Option<std::net::SocketAddr>)> {
        // --- LOOP DETECTION ---
        let user_agent = utils::get_header(packet, HeaderName::UserAgent);
        if user_agent.contains("Sentiric_B2BUA") {
            info!("🔄 Giden Çağrı (Outbound Traffic). Passthrough...");
            return None; 
        }

        let from = utils::get_header(packet, HeaderName::From);
        let to = utils::get_header(packet, HeaderName::To);
        
        info!("INVITE Forwarding -> B2BUA (SIP): From={}, To={}", from, to);

        // B2BUA SIP Adresini Çözümle
        let b2bua_target = match lookup_host(&self.config.b2bua_sip_addr).await {
            Ok(mut addrs) => addrs.next(),
            Err(e) => {
                error!("DNS Resolution Error (B2BUA): {}", e);
                return Some((self.create_response(packet, 500, "Internal Error"), None));
            }
        };

        if let Some(target) = b2bua_target {
            // Paketi olduğu gibi B2BUA'ya ilet (Transparent Proxy)
            // Not: Geri dönüş değerinde (Packet, Target) döndürüyoruz.
            // Target varsa, o adrese forward eder. Target None ise, kaynağa yanıt döner.
            return Some((packet.clone(), Some(target)));
        }

        Some((self.create_response(packet, 503, "Service Unavailable"), None))
    }

    // --- YARDIMCI FONKSİYON: Yanıt Oluşturucu ---
    fn create_response(&self, req: &SipPacket, code: u16, reason: &str) -> SipPacket {
        let mut resp = SipPacket::new_response(code, reason.to_string());
        
        for h in &req.headers {
            match h.name {
                HeaderName::Via | 
                HeaderName::From | 
                HeaderName::To | 
                HeaderName::CallId | 
                HeaderName::CSeq => {
                    resp.headers.push(h.clone());
                },
                _ => {}
            }
        }
        resp.headers.push(Header::new(HeaderName::Server, "Sentiric Proxy".to_string()));
        resp.headers.push(Header::new(HeaderName::ContentLength, "0".to_string()));
        resp
    }
}