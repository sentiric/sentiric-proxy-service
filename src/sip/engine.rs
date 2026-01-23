// sentiric-proxy-service/src/sip/engine.rs

use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, error, instrument, warn};
use sentiric_sip_core::{SipPacket, Method, HeaderName, Header}; 
use sentiric_contracts::sentiric::sip::v1::RegisterRequest;
use tonic::Request;
use crate::grpc::client::InternalClients;
use crate::sip::utils;
use crate::config::AppConfig;
use tokio::net::lookup_host;
use std::net::SocketAddr;

pub struct ProxyEngine {
    clients: Arc<Mutex<InternalClients>>,
    config: Arc<AppConfig>,
}

impl ProxyEngine {
    pub fn new(clients: Arc<Mutex<InternalClients>>, config: Arc<AppConfig>) -> Self {
        Self { clients, config }
    }

    /// Gelen SIP paketlerini işler.
    #[instrument(skip(self, packet), fields(method = %packet.method, is_request = packet.is_request))]
    pub async fn process_packet(&self, packet: &mut SipPacket, src_addr: SocketAddr) -> Option<(SipPacket, Option<SocketAddr>)> {
        
        if packet.is_request {
            match packet.method {
                Method::Register => {
                    if let Some(resp) = self.handle_register(packet).await {
                        return Some((resp, None));
                    }
                    None
                },
                Method::Invite => self.handle_invite(packet, src_addr).await,
                _ => self.handle_passthrough_request(packet, src_addr).await, 
            }
        } else {
            self.handle_response(packet).await
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

    async fn handle_invite(&self, packet: &mut SipPacket, _src_addr: SocketAddr) -> Option<(SipPacket, Option<SocketAddr>)> {
        // --- OUTBOUND TRAFFIC (B2BUA -> User) ---
        let user_agent = utils::get_header(packet, HeaderName::UserAgent);
        if user_agent.contains("Sentiric B2BUA") {
            if let Some(target_addr) = self.extract_target_addr(&packet.uri) {
                info!("🔄 Outbound INVITE: B2BUA -> {}", target_addr);
                self.add_via_header(packet);
                return Some((packet.clone(), Some(target_addr)));
            } else {
                error!("❌ Outbound INVITE hedef adresi çözülemedi: {}", packet.uri);
                return None;
            }
        }

        // --- INBOUND TRAFFIC (User -> B2BUA) ---
        let from = utils::get_header(packet, HeaderName::From);
        let to = utils::get_header(packet, HeaderName::To);
        
        info!("➡️ Inbound INVITE: User -> B2BUA: From={}, To={}", from, to);

        let b2bua_target = match lookup_host(&self.config.b2bua_sip_addr).await {
            Ok(mut addrs) => addrs.next(),
            Err(e) => {
                error!("DNS Resolution Error (B2BUA: {}): {}", self.config.b2bua_sip_addr, e);
                return Some((self.create_response(packet, 500, "Internal Error"), None));
            }
        };

        if let Some(target) = b2bua_target {
            self.add_via_header(packet);
            return Some((packet.clone(), Some(target)));
        }

        Some((self.create_response(packet, 503, "Service Unavailable"), None))
    }

    async fn handle_passthrough_request(&self, packet: &mut SipPacket, _src_addr: SocketAddr) -> Option<(SipPacket, Option<SocketAddr>)> {
        let user_agent = utils::get_header(packet, HeaderName::UserAgent);
        
        if user_agent.contains("Sentiric B2BUA") {
             if let Some(target_addr) = self.extract_target_addr(&packet.uri) {
                 self.add_via_header(packet);
                 return Some((packet.clone(), Some(target_addr)));
             }
        } else {
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

        if !packet.headers.is_empty() && packet.headers[0].name == HeaderName::Via {
            let _my_via = packet.headers.remove(0);
        } else {
            warn!("Response packet missing Via header! Cannot route back. Dropping.");
            return None;
        }

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
        let branch = format!("z9hG4bK-proxy-{}", uuid::Uuid::new_v4());
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

    // DÜZELTME: Bu fonksiyon optimize edildi.
    fn parse_via_address(&self, via_val: &str) -> Option<SocketAddr> {
        // Via formatı: SIP/2.0/UDP 192.168.1.50:5060;branch=...
        
        let parts: Vec<&str> = via_val.split_whitespace().collect();
        if parts.len() < 2 { return None; }
        
        let protocol_part = parts[1]; 
        
        // Önce ; ile ayır
        let params: Vec<&str> = protocol_part.split(';').collect();
        let host_port = params[0];
        
        // Varsayılanları tanımla
        let (mut host, mut port) = if let Some((h, p)) = host_port.rsplit_once(':') {
            (h.to_string(), p.to_string())
        } else {
            (host_port.to_string(), "5060".to_string())
        };

        // rport ve received varsa öncelikli kullan (NAT Traversal)
        for param in &params[1..] {
            if let Some((k, v)) = param.split_once('=') {
                if k == "received" { host = v.to_string(); }
                if k == "rport" { port = v.to_string(); }
            }
        }

        format!("{}:{}", host, port).parse().ok()
    }
}