// sentiric-proxy-service/src/sip/engine.rs

use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, warn, error, instrument};
use sentiric_sip_core::{SipPacket, Method, HeaderName};
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
            _ => None, // Diğer metodlar şimdilik drop ediliyor veya B2BUA'ya bırakılıyor
        }
    }

    async fn handle_register(&self, packet: &SipPacket) -> Option<SipPacket> {
        let to_header = utils::get_header(packet, HeaderName::To);
        let contact = utils::get_header(packet, HeaderName::Contact);
        
        // SIP URI'yi temizle (AOR)
        let aor = utils::extract_aor(&to_header); // sip:1001@domain
        let contact_uri = utils::extract_aor(&contact); // gercek ip:port

        info!("REGISTER Process: AOR={}, Contact={}", aor, contact_uri);

        let mut clients = self.clients.lock().await;
        
        let req = Request::new(RegisterRequest {
            sip_uri: aor,
            contact_uri: contact_uri,
            expires: 3600, // Varsayılan
        });

        match clients.registrar.register(req).await {
            Ok(_) => {
                info!("Registrar: Kayıt başarılı.");
                // 200 OK Döndür
                Some(SipPacket::new_response(200, "OK".to_string()))
            }
            Err(e) => {
                error!("Registrar Hatası: {}", e);
                Some(SipPacket::new_response(500, "Internal Server Error".to_string()))
            }
        }
    }

    async fn handle_invite(&self, packet: &SipPacket) -> Option<SipPacket> {
        let from = utils::get_header(packet, HeaderName::From);
        let to = utils::get_header(packet, HeaderName::To);
        let call_id = utils::get_header(packet, HeaderName::CallId);

        info!("INVITE Process: From={}, To={}", from, to);

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
                    // 100 Trying döndür (Asenkron işlem devam edecek)
                    Some(SipPacket::new_response(100, "Trying".to_string()))
                } else {
                    Some(SipPacket::new_response(403, "Forbidden".to_string()))
                }
            }
            Err(e) => {
                error!("B2BUA Hatası: {}", e);
                Some(SipPacket::new_response(503, "Service Unavailable".to_string()))
            }
        }
    }
}