// sentiric-proxy-service/src/sip/engine.rs

use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, warn, error, instrument};
use sentiric_sip_core::{SipPacket, Method, HeaderName, Header}; // Header eklendi
use sentiric_contracts::sentiric::sip::v1::{RegisterRequest, InitiateCallRequest};
use tonic::Request;
use crate::grpc::client::InternalClients;
use crate::sip::utils;

pub struct ProxyEngine {
    clients: Arc<Mutex<InternalClients>>,
}

impl ProxyEngine {
    pub fn new(clients: Arc<Mutex<InternalClients>>) -> Self {
        Self { clients }
    }

    #[instrument(skip(self, packet), fields(method = %packet.method))]
    pub async fn process_packet(&self, packet: &SipPacket) -> Option<SipPacket> {
        match packet.method {
            Method::Register => self.handle_register(packet).await,
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
                // DÜZELTME: Yanıtı oluştururken headerları kopyalıyoruz.
                Some(self.create_response(packet, 200, "OK"))
            }
            Err(e) => {
                error!("Registrar Hatası: {}", e);
                Some(self.create_response(packet, 500, "Internal Server Error"))
            }
        }
    }

    async fn handle_invite(&self, packet: &SipPacket) -> Option<SipPacket> {
        // --- LOOP DETECTION ---
        let user_agent = utils::get_header(packet, HeaderName::UserAgent);
        if user_agent.contains("Sentiric_B2BUA") {
            info!("🔄 Giden Çağrı Tespit Edildi (Outbound Traffic). Forwarding...");
            return None; 
        }

        // --- INBOUND CALL HANDLING ---
        let from = utils::get_header(packet, HeaderName::From);
        let to = utils::get_header(packet, HeaderName::To);
        let call_id = utils::get_header(packet, HeaderName::CallId);

        info!("INVITE Process (Inbound): From={}, To={}", from, to);

        let mut clients = self.clients.lock().await;

        let req = Request::new(InitiateCallRequest {
            call_id: call_id,
            from_uri: from,
            to_uri: to,
        });

        match clients.b2bua.initiate_call(req).await {
            Ok(res) => {
                let inner = res.into_inner();
                if inner.success {
                    info!("B2BUA: Çağrı başlatıldı. ID: {}", inner.new_call_id);
                    Some(self.create_response(packet, 100, "Trying"))
                } else {
                    Some(self.create_response(packet, 403, "Forbidden"))
                }
            }
            Err(e) => {
                error!("B2BUA Hatası: {}", e);
                Some(self.create_response(packet, 503, "Service Unavailable"))
            }
        }
    }

    // --- YARDIMCI FONKSİYON: Yanıt Oluşturucu ---
    // RFC 3261 Gereği: Via, From, To, Call-ID ve CSeq başlıkları kopyalanmalıdır.
    fn create_response(&self, req: &SipPacket, code: u16, reason: &str) -> SipPacket {
        let mut resp = SipPacket::new_response(code, reason.to_string());
        
        // 1. Kritik Başlıkları Kopyala
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

        // 2. Server Header Ekle
        resp.headers.push(Header::new(HeaderName::Server, "Sentiric Proxy".to_string()));
        
        // 3. Content-Length (Otomatik 0)
        resp.headers.push(Header::new(HeaderName::ContentLength, "0".to_string()));

        resp
    }
}