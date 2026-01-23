// sentiric-proxy-service/src/sip/engine.rs

use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, error, instrument};
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
                    return Some((resp, None));
                }
                None
            },
            Method::Invite => self.handle_invite(packet).await,
            // Diğer metodlar (ACK, BYE vb.) için passthrough logic eklenmeli
            _ => self.handle_passthrough(packet).await, 
        }
    }

    async fn handle_register(&self, packet: &SipPacket) -> Option<SipPacket> {
        let to_header = utils::get_header(packet, HeaderName::To);
        let contact = utils::get_header(packet, HeaderName::Contact);
        
        let aor = utils::extract_aor(&to_header); 
        let contact_uri = utils::extract_aor(&contact); 

        info!("REGISTER Process: AOR='{}', Contact='{}'", aor, contact_uri);

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
        // --- OUTBOUND TRAFFIC (B2BUA -> User) ---
        let user_agent = utils::get_header(packet, HeaderName::UserAgent);
        if user_agent.contains("Sentiric B2BUA") {
            // B2BUA'dan geliyor, hedef kullanıcıya (Leg B) gitmeli.
            // Hedef adres, paketin Request-URI (ilk satır) kısmındadır.
            // Örn: INVITE sip:1001@192.168.1.50:5060 SIP/2.0
            
            // Basit bir parsing ile hedef IP:Port'u buluyoruz.
            if let Some(target_addr) = self.extract_target_addr(&packet.uri) {
                info!("🔄 Outbound INVITE Routing: B2BUA -> {}", target_addr);
                return Some((packet.clone(), Some(target_addr)));
            } else {
                error!("❌ Outbound INVITE hedef adresi çözülemedi: {}", packet.uri);
                return None;
            }
        }

        // --- INBOUND TRAFFIC (User -> B2BUA) ---
        let from = utils::get_header(packet, HeaderName::From);
        let to = utils::get_header(packet, HeaderName::To);
        
        info!("➡️ Inbound INVITE Routing: User -> B2BUA: From={}, To={}", from, to);

        let b2bua_target = match lookup_host(&self.config.b2bua_sip_addr).await {
            Ok(mut addrs) => addrs.next(),
            Err(e) => {
                error!("DNS Resolution Error (B2BUA): {}", e);
                return Some((self.create_response(packet, 500, "Internal Error"), None));
            }
        };

        if let Some(target) = b2bua_target {
            return Some((packet.clone(), Some(target)));
        }

        Some((self.create_response(packet, 503, "Service Unavailable"), None))
    }

    // ACK, BYE gibi diğer paketler için genel yönlendirme
    async fn handle_passthrough(&self, packet: &SipPacket) -> Option<(SipPacket, Option<std::net::SocketAddr>)> {
        // Eğer B2BUA'dan geliyorsa -> User'a
        // Eğer User'dan geliyorsa -> B2BUA'ya
        // Bu ayrımı yapmak için yine User-Agent veya Via başlıklarına bakılabilir.
        // Basitlik için şimdilik User -> B2BUA varsayıyoruz (Inbound ağırlıklı).
        // Gerçek implementasyonda stateful proxy logic gerekir.
        
        let user_agent = utils::get_header(packet, HeaderName::UserAgent);
        if user_agent.contains("Sentiric B2BUA") {
             if let Some(target_addr) = self.extract_target_addr(&packet.uri) {
                 return Some((packet.clone(), Some(target_addr)));
             }
        } else {
             // User -> B2BUA
             if let Ok(mut addrs) = lookup_host(&self.config.b2bua_sip_addr).await {
                 if let Some(target) = addrs.next() {
                     return Some((packet.clone(), Some(target)));
                 }
             }
        }
        None
    }

    fn create_response(&self, req: &SipPacket, code: u16, reason: &str) -> SipPacket {
        let mut resp = SipPacket::new_response(code, reason.to_string());
        for h in &req.headers {
            match h.name {
                HeaderName::Via | HeaderName::From | HeaderName::To | HeaderName::CallId | HeaderName::CSeq => {
                    resp.headers.push(h.clone());
                },
                _ => {}
            }
        }
        resp.headers.push(Header::new(HeaderName::Server, "Sentiric Proxy".to_string()));
        resp.headers.push(Header::new(HeaderName::ContentLength, "0".to_string()));
        resp
    }

    // URI stringinden (sip:ip:port) SocketAddr üretir
    fn extract_target_addr(&self, uri: &str) -> Option<std::net::SocketAddr> {
        // sip:user@192.168.1.5:5060 -> 192.168.1.5:5060
        // sip:192.168.1.5:5060 -> 192.168.1.5:5060
        
        let clean = uri.trim_start_matches("sip:");
        let host_port_part = if let Some(at_idx) = clean.find('@') {
            &clean[at_idx+1..]
        } else {
            clean
        };

        // Parametreleri at (;transport=udp gibi)
        let host_port = if let Some(semi_idx) = host_port_part.find(';') {
            &host_port_part[..semi_idx]
        } else {
            host_port_part
        };

        host_port.parse().ok()
    }
}