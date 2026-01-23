// sentiric-proxy-service/src/sip/engine.rs

use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, error, instrument, warn};
// DÜZELTME: sip-core'dan utils'i import et
use sentiric_sip_core::{SipPacket, Method, HeaderName, Header, utils as sip_core_utils}; 
use sentiric_contracts::sentiric::sip::v1::RegisterRequest;
use tonic::Request;
use crate::grpc::client::InternalClients;
// DÜZELTME: Yerel utils'i import etmeye devam et (sadece get_header için)
use crate::sip::utils;
use crate::config::AppConfig;
use tokio::net::lookup_host;
use std::net::SocketAddr;
use uuid::Uuid;

pub struct ProxyEngine {
    clients: Arc<Mutex<InternalClients>>,
    config: Arc<AppConfig>,
}

impl ProxyEngine {
    pub fn new(clients: Arc<Mutex<InternalClients>>, config: Arc<AppConfig>) -> Self {
        Self { clients, config }
    }

    /// Gelen SIP paketlerini işler. Request veya Response olmasına göre yönlendirme yapar.
    #[instrument(skip(self, packet), fields(method = %packet.method, is_request = packet.is_request))]
    pub async fn process_packet(&self, packet: &mut SipPacket, src_addr: SocketAddr) -> Option<(SipPacket, Option<SocketAddr>)> {
        
        if packet.is_request {
            // --- REQUEST HANDLING ---
            match packet.method {
                Method::Register => {
                    // REGISTER işlemleri gRPC üzerinden Registrar'a gider, SIP yanıtı döner.
                    if let Some(resp) = self.handle_register(packet).await {
                        return Some((resp, None)); // Response to sender (User)
                    }
                    None
                },
                Method::Invite => self.handle_invite(packet, src_addr).await,
                _ => self.handle_passthrough_request(packet, src_addr).await, 
            }
        } else {
            // --- RESPONSE HANDLING (100 Trying, 200 OK, etc.) ---
            // B2BUA'dan gelen yanıtları User'a iletmek için.
            self.handle_response(packet).await
        }
    }

    async fn handle_register(&self, packet: &SipPacket) -> Option<SipPacket> {
        let to_header = utils::get_header(packet, HeaderName::To);
        let contact = utils::get_header(packet, HeaderName::Contact);
        
        // DÜZELTME: Merkezi kütüphanedeki fonksiyon çağrılıyor
        let aor = sip_core_utils::extract_aor(&to_header); 
        let contact_uri = sip_core_utils::extract_aor(&contact); 

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

    async fn handle_invite(&self, packet: &mut SipPacket, src_addr: SocketAddr) -> Option<(SipPacket, Option<SocketAddr>)> {
        // --- YENİ YÖNLENDİRME MANTIĞI ---

        // B2BUA'dan gelen paketleri ayırt etmenin en güvenilir yolu,
        // paketin geldiği IP adresinin B2BUA'nın bilinen adresi olup olmadığını kontrol etmektir.
        // Docker içinde bu, servis adıyla çözümlenen IP'dir.
        
        let b2bua_hostname = self.config.b2bua_sip_addr.split(':').next().unwrap_or("");
        let is_from_b2bua = if let Ok(mut addrs) = lookup_host(b2bua_hostname).await {
            addrs.any(|a| a.ip() == src_addr.ip())
        } else {
            false
        };

        if is_from_b2bua {
            // --- OUTBOUND TRAFFIC (B2BUA -> User) ---
            // Bu paket B2BUA'dan geliyor, hedef kullanıcıya (Leg B) gitmeli.
            // Hedef adres, paketin Request-URI (ilk satır) kısmındadır.
            // Örn: INVITE sip:1001@188.119.23.175:5060 SIP/2.0
            
            if let Some(target_addr) = self.extract_target_addr(&packet.uri) {
                info!("🔄 Outbound INVITE Routing: B2BUA -> {}", target_addr);
                self.add_via_header(packet); // Proxy'nin Via'sını ekle
                return Some((packet.clone(), Some(target_addr)));
            } else {
                error!("❌ Outbound INVITE hedef adresi (Request-URI) çözülemedi: {}", packet.uri);
                return None;
            }
        } else {
            // --- INBOUND TRAFFIC (User -> B2BUA) ---
            // Bu paket dış dünyadan (SBC/User) geliyor.
            let from = utils::get_header(packet, HeaderName::From);
            let to = utils::get_header(packet, HeaderName::To);
            
            info!("➡️ Inbound INVITE Routing: User -> B2BUA: From={}, To={}", from, to);

            if let Ok(mut addrs) = lookup_host(&self.config.b2bua_sip_addr).await {
                if let Some(target) = addrs.next() {
                    self.add_via_header(packet);
                    return Some((packet.clone(), Some(target)));
                }
            }

            error!("CRITICAL: B2BUA adresi '{}' çözümlenemedi.", self.config.b2bua_sip_addr);
            return Some((self.create_response(packet, 503, "Service Unavailable"), None));
        }
    }

    async fn handle_passthrough_request(&self, packet: &mut SipPacket, _src_addr: SocketAddr) -> Option<(SipPacket, Option<SocketAddr>)> {
        // Genel Request Yönlendirme (ACK, BYE, CANCEL)
        // Basit mantık: User-Agent kontrolü ile yön belirle
        
        let user_agent = utils::get_header(packet, HeaderName::UserAgent);
        
        if user_agent.contains("Sentiric B2BUA") {
             // B2BUA -> User
             if let Some(target_addr) = self.extract_target_addr(&packet.uri) {
                 self.add_via_header(packet);
                 return Some((packet.clone(), Some(target_addr)));
             }
        } else {
             // User -> B2BUA
             if let Ok(mut addrs) = lookup_host(&self.config.b2bua_sip_addr).await {
                 if let Some(target) = addrs.next() {
                     self.add_via_header(packet);
                     return Some((packet.clone(), Some(target)));
                 }
             }
        }
        None
    }

    async fn handle_response(&self, packet: &mut SipPacket) -> Option<(SipPacket, Option<SocketAddr>)> {
        let status = packet.status_code;
        // info!("⬅️ Response Received: {} {}", status, packet.reason);

        // 1. En üstteki Via başlığını (BİZİM eklediğimiz) çıkar.
        if !packet.headers.is_empty() && packet.headers[0].name == HeaderName::Via {
            let _my_via = packet.headers.remove(0);
        } else {
            warn!("Response packet missing Via header! Cannot route back. Dropping.");
            return None;
        }

        // 2. Sıradaki Via başlığına bak (Bu, paketin asıl sahibidir - User veya B2BUA)
        if let Some(client_via) = packet.headers.iter().find(|h| h.name == HeaderName::Via) {
            if let Some(target) = self.parse_via_address(&client_via.value) {
                info!("↩️ Routing Response ({}) to: {}", status, target);
                return Some((packet.clone(), Some(target)));
            } else {
                error!("Could not parse target from Via header: {}", client_via.value);
            }
        } else {
            warn!("No secondary Via header found. Cannot route response.");
        }

        None
    }

    fn add_via_header(&self, packet: &mut SipPacket) {
        let branch = format!("z9hG4bK-proxy-{}", Uuid::new_v4());
        let via_val = format!("SIP/2.0/UDP {}:{};branch={}", 
            "proxy-service", 
            self.config.sip_port,
            branch
        );
        packet.headers.insert(0, Header::new(HeaderName::Via, via_val));
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

    fn extract_target_addr(&self, uri: &str) -> Option<SocketAddr> {
        let clean = uri.trim_start_matches("sip:");
        let host_port_part = if let Some(at_idx) = clean.find('@') {
            &clean[at_idx+1..]
        } else {
            clean
        };
        let host_port = if let Some(semi_idx) = host_port_part.find(';') {
            &host_port_part[..semi_idx]
        } else {
            host_port_part
        };
        if !host_port.contains(':') {
             format!("{}:5060", host_port).parse().ok()
        } else {
             host_port.parse().ok()
        }
    }

    fn parse_via_address(&self, via_val: &str) -> Option<SocketAddr> {
        let parts: Vec<&str> = via_val.split_whitespace().collect();
        if parts.len() < 2 { return None; }
        
        let protocol_part = parts[1]; 
        let params: Vec<&str> = protocol_part.split(';').collect();
        let host_port = params[0];
        
        let (mut host, mut port) = if let Some((h, p)) = host_port.rsplit_once(':') {
            (h.to_string(), p.to_string())
        } else {
            (host_port.to_string(), "5060".to_string())
        };

        for param in &params[1..] {
            if let Some((k, v)) = param.split_once('=') {
                if k == "received" { host = v.to_string(); }
                if k == "rport" { port = v.to_string(); }
            }
        }

        format!("{}:{}", host, port).parse().ok()
    }
}