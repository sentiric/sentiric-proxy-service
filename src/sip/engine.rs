// sentiric-proxy-service/src/sip/engine.rs

use crate::config::AppConfig;
use crate::grpc::client::InternalClients;
use crate::sip::server::{ProxyState, DEFAULT_SIP_PORT};
use crate::sip::utils;
use crate::sip::handlers::routing::{RoutingHandler, RedisConn}; // ✅ YENİ
use sentiric_contracts::sentiric::sip::v1::RegisterRequest;
use sentiric_sip_core::{
    utils as sip_core_utils, 
    Header, HeaderName, Method, SipPacket,
    SipRouter,
    // ✅ Transaction Yetenekleri
    TransactionEngine, TransactionAction, SipTransaction
};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tonic::Request;
use tracing::{error, info, instrument, debug};
use rand;
use dashmap::DashMap;

// Aktif işlemleri (Transactions) bellekte tutmak için basit bir store
// Proxy stateless olmaya çalışsa da, retransmission yönetimi için kısa süreli state gerekir.
pub type TransactionStore = Arc<DashMap<String, SipTransaction>>;

pub struct ProxyEngine {
    clients: Arc<Mutex<InternalClients>>,
    config: Arc<AppConfig>,
    state: Arc<ProxyState>,
    // routing_handler, redis bağlantısını kapsüller
    router: RoutingHandler,
    // Aktif transactionları tutar
    transactions: TransactionStore,
}

impl ProxyEngine {
    pub fn new(
        clients: Arc<Mutex<InternalClients>>,
        config: Arc<AppConfig>,
        state: Arc<ProxyState>,
        redis: RedisConn,
    ) -> Self {
        Self { 
            clients, 
            config, 
            state, 
            router: RoutingHandler::new(redis),
            transactions: Arc::new(DashMap::new())
        }
    }

    #[instrument(skip(self, packet), fields(method = %packet.method, call_id = %utils::get_header(packet, HeaderName::CallId)))]
    pub async fn process_packet(
        &self,
        packet: &mut SipPacket,
        src_addr: SocketAddr,
    ) -> Option<(SipPacket, Option<SocketAddr>)> {
        
        // 1. NAT Düzeltmesi
        if packet.is_request {
            SipRouter::fix_nat_via(packet, src_addr);
        }

        debug!("📦 [PROXY] Paket İşleniyor. Yön: {}", if packet.is_request { "REQUEST" } else { "RESPONSE" });

        // 2. Transaction Kontrolü (Sadece Requestler için)
        if packet.is_request && packet.method != Method::Ack {
            let call_id = utils::get_header(packet, HeaderName::CallId);
            // Basit transaction key: Call-ID + Method (Gerçekte Branch ID daha doğru olur ama Proxy için yeterli)
            let tx_key = format!("{}:{:?}", call_id, packet.method);
            
            // Eğer transaction yoksa oluştur, varsa kontrol et
            let action = if let Some(tx) = self.transactions.get(&tx_key) {
                TransactionEngine::check(&Some(tx.clone()), packet)
            } else {
                // Yeni transaction başlat
                if let Some(new_tx) = SipTransaction::new(packet) {
                    self.transactions.insert(tx_key, new_tx);
                }
                TransactionAction::ForwardToApp
            };

            match action {
                TransactionAction::RetransmitResponse(cached_resp) => {
                    info!("🔄 [RETRANSMIT] Proxy önbelleğinden yanıt dönülüyor -> {}", src_addr);
                    return Some((cached_resp, Some(src_addr)));
                },
                TransactionAction::Ignore => return None,
                TransactionAction::ForwardToApp => {}
            }
        }

        if packet.is_request {
            self.handle_request(packet, src_addr).await
        } else {
            self.handle_response(packet).await
        }
    }

    async fn handle_request(&self, packet: &mut SipPacket, src_addr: SocketAddr) -> Option<(SipPacket, Option<SocketAddr>)> {
        // A. REGISTER İstekleri
        if packet.method == Method::Register {
            return self.handle_register(packet, src_addr).await;
        }

        // B. Loose Routing (Route Header)
        if packet.headers.iter().any(|h| h.name == HeaderName::Route) {
            return self.handle_loose_routing(packet).await;
        }

        // C. ACK İstekleri -> Routing Handler
        if packet.method == Method::Ack {
             let call_id = utils::get_header(packet, HeaderName::CallId);
             let to_tag = self.extract_tag_from_header(&utils::get_header(packet, HeaderName::To));
             
             if let Some(target) = self.router.resolve_ack_target(&call_id, &to_tag).await {
                 return Some((packet.clone(), Some(target)));
             }
             return None;
        }
        
        // D. Initial INVITE -> B2BUA
        let target_host = &self.config.b2bua_sip_addr;
        match self.state.resolve_b2bua_addr(target_host).await {
            Ok(target_addr) => {
                info!(target = %target_addr, "➡️ [INVITE] Çağrı B2BUA'ya yönlendiriliyor.");

                SipRouter::add_record_route(packet, &self.config.proxy_advertised_host, self.config.sip_port);
                SipRouter::add_via(packet, &self.config.proxy_advertised_host, self.config.sip_port, "UDP");

                // Routing Handler ile kaydet
                let call_id = utils::get_header(packet, HeaderName::CallId);
                self.router.register_call_route(&call_id, src_addr, target_addr).await;
        
                Some((packet.clone(), Some(target_addr)))
            }
            Err(e) => {
                error!("❌ B2BUA adresi çözülemedi: {}", e);
                Some((self.create_response(packet, 503, "Service Unavailable"), Some(src_addr)))
            }
        }
    }

    async fn handle_register(&self, packet: &SipPacket, src_addr: SocketAddr) -> Option<(SipPacket, Option<SocketAddr>)> {
        let to_header = utils::get_header(packet, HeaderName::To);
        let aor = sip_core_utils::extract_aor(&to_header);
        let username = sip_core_utils::extract_username_from_uri(&aor);
        
        // Via analizi
        let via_val = utils::get_header(packet, HeaderName::Via);
        let client_addr = SipRouter::resolve_response_target(&via_val, DEFAULT_SIP_PORT).unwrap_or(src_addr);
        let real_contact_uri = format!("sip:{}@{}:{}", username, client_addr.ip(), client_addr.port());

        info!("📝 [REGISTER] Kullanıcı: {}", username);

        let mut clients = self.clients.lock().await;
        let expires_str = utils::get_header(packet, HeaderName::Other("Expires".to_string()));
        let expires = expires_str.parse::<i32>().unwrap_or(3600);

        let req = Request::new(RegisterRequest { 
            sip_uri: aor.clone(), 
            contact_uri: real_contact_uri, 
            expires 
        });

        match clients.registrar.register(req).await {
            Ok(_) => {
                let mut resp = self.create_response(packet, 200, "OK");
                if let Some(contact) = packet.get_header_value(HeaderName::Contact) {
                    resp.headers.retain(|h| h.name != HeaderName::Contact);
                    resp.headers.push(Header::new(HeaderName::Contact, contact.clone()));
                }
                // Tag ekle
                if let Some(to_h) = resp.headers.iter_mut().find(|h| h.name == HeaderName::To) {
                    if !to_h.value.contains(";tag=") {
                        let tag = format!("{:x}", rand::random::<u32>());
                        to_h.value.push_str(&format!(";tag={}", tag));
                    }
                }
                
                // Transaction güncelle (Cevabı kaydet)
                let call_id = utils::get_header(packet, HeaderName::CallId);
                let tx_key = format!("{}:{:?}", call_id, packet.method);
                if let Some(mut tx) = self.transactions.get_mut(&tx_key) {
                    tx.update_on_response(&resp);
                }

                Some((resp, Some(src_addr)))
            },
            Err(e) => {
                error!("❌ Registrar Servis Hatası: {}", e);
                Some((self.create_response(packet, 500, "Internal Server Error"), Some(src_addr)))
            }
        }
    }

    async fn handle_loose_routing(&self, packet: &mut SipPacket) -> Option<(SipPacket, Option<SocketAddr>)> {
        if let Some(route_header) = packet.headers.iter().find(|h| h.name == HeaderName::Route) {
             if let Some(target_addr) = sip_core_utils::extract_socket_addr(&route_header.value) {
                 debug!("🔄 [LOOSE ROUTING] -> {}", target_addr);
                 packet.headers.remove(0); 
                 SipRouter::add_via(packet, &self.config.proxy_advertised_host, self.config.sip_port, "UDP");
                 return Some((packet.clone(), Some(target_addr)));
             }
        }
        None
    }
    
    async fn handle_response(&self, packet: &mut SipPacket) -> Option<(SipPacket, Option<SocketAddr>)> {
        if SipRouter::strip_top_via(packet).is_none() {
            return None;
        }

        // Routing Handler ile state güncelle
        if packet.status_code >= 200 && packet.status_code < 300 {
            let call_id = utils::get_header(packet, HeaderName::CallId);
            let to_tag = self.extract_tag_from_header(&utils::get_header(packet, HeaderName::To));
            self.router.update_dialog_state(&call_id, &to_tag).await;
        }
        
        if let Some(next_via) = packet.headers.iter().find(|h| h.name == HeaderName::Via) {
            if let Some(target) = SipRouter::resolve_response_target(&next_via.value, DEFAULT_SIP_PORT) {
                return Some((packet.clone(), Some(target)));
            }
        }
        None
    }

    fn create_response(&self, req: &SipPacket, code: u16, reason: &str) -> SipPacket {
        let mut resp = SipPacket::create_response_for(req, code, reason.to_string());
        resp.headers.push(Header::new(HeaderName::Server, "Sentiric-Proxy/1.3.1".to_string()));
        resp.headers.push(Header::new(HeaderName::ContentLength, "0".to_string()));
        resp
    }

    fn extract_tag_from_header(&self, header_val: &str) -> String {
        if let Some(tag_start) = header_val.find(";tag=") {
            let rest = &header_val[tag_start + 5..];
            if let Some(tag_end) = rest.find(';') {
                return rest[..tag_end].to_string();
            }
            return rest.to_string();
        }
        String::new()
    }
}