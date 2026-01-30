// sentiric-proxy-service/src/sip/engine.rs

use crate::config::AppConfig;
use crate::grpc::client::InternalClients;
use crate::sip::server::ProxyState;
use crate::sip::utils;
use sentiric_contracts::sentiric::sip::v1::RegisterRequest;
use sentiric_sip_core::{utils as sip_core_utils, Header, HeaderName, Method, SipPacket};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tonic::Request;
use tracing::{error, info, instrument, warn, debug};
use redis::AsyncCommands;

// Redis Connection Tipi
pub type RedisConn = Arc<Mutex<redis::aio::MultiplexedConnection>>;

pub struct ProxyEngine {
    clients: Arc<Mutex<InternalClients>>,
    config: Arc<AppConfig>,
    state: Arc<ProxyState>,
    redis: RedisConn,
}

impl ProxyEngine {
    pub fn new(
        clients: Arc<Mutex<InternalClients>>,
        config: Arc<AppConfig>,
        state: Arc<ProxyState>,
        redis: RedisConn,
    ) -> Self {
        Self { clients, config, state, redis }
    }

    #[instrument(skip(self, packet), fields(method = %packet.method, call_id = %utils::get_header(packet, HeaderName::CallId)))]
    pub async fn process_packet(
        &self,
        packet: &mut SipPacket,
        src_addr: SocketAddr,
    ) -> Option<(SipPacket, Option<SocketAddr>)> {
        if packet.is_request {
            self.handle_request(packet, src_addr).await
        } else {
            self.handle_response(packet, src_addr).await
        }
    }

    async fn handle_request(&self, packet: &mut SipPacket, src_addr: SocketAddr) -> Option<(SipPacket, Option<SocketAddr>)> {
        // REGISTER stateless olarak işlenir
        if packet.method == Method::Register {
            return self.handle_register(packet, src_addr).await;
        }

        // Route başlığı varsa, bu bir diyalog içi istektir (ACK, BYE, re-INVITE)
        if packet.headers.iter().any(|h| h.name == HeaderName::Route) {
            return self.handle_in_dialog_request(packet, src_addr).await;
        }

        // Diğer tüm istekler (özellikle ilk INVITE) yeni bir diyalog başlatır
        if packet.method == Method::Invite {
            return self.handle_initial_invite(packet, src_addr).await;
        }

        // Eşleşmeyen diğer istekler için varsayılan yönlendirme
        warn!("Stateless fallback for unexpected request method: {}", packet.method);
        let target_host = &self.config.b2bua_sip_addr;
        if let Ok(target_addr) = self.state.resolve_b2bua_addr(target_host).await {
            Some((packet.clone(), Some(target_addr)))
        } else {
            None
        }
    }

    async fn handle_register(&self, packet: &SipPacket, src_addr: SocketAddr) -> Option<(SipPacket, Option<SocketAddr>)> {
        let to_header = utils::get_header(packet, HeaderName::To);
        let aor = sip_core_utils::extract_aor(&to_header);
        let username = sip_core_utils::extract_username_from_uri(&aor);
        let real_contact_uri = format!("sip:{}@{}:{}", username, src_addr.ip(), src_addr.port());

        let mut clients = self.clients.lock().await;
        let req = Request::new(RegisterRequest { sip_uri: aor, contact_uri: real_contact_uri, expires: 3600 });

        match clients.registrar.register(req).await {
            Ok(_) => Some((self.create_response(packet, 200, "OK"), Some(src_addr))),
            Err(_) => Some((self.create_response(packet, 500, "Internal Server Error"), Some(src_addr))),
        }
    }

    async fn handle_initial_invite(&self, packet: &mut SipPacket, src_addr: SocketAddr) -> Option<(SipPacket, Option<SocketAddr>)> {
        let call_id = utils::get_header(packet, HeaderName::CallId);
        let from_tag = self.extract_tag_from_header(&utils::get_header(packet, HeaderName::From));
        let to_aor = sip_core_utils::extract_aor(&utils::get_header(packet, HeaderName::To));
        let callee_username = sip_core_utils::extract_username_from_uri(&to_aor);

        let target_host = if callee_username == "9998" {
            info!("🎯 [PROXY-ROUTE] INVITE -> PROBE");
            &self.config.probe_sip_addr
        } else {
            info!("🎯 [PROXY-ROUTE] INVITE -> B2BUA (Default)");
            &self.config.b2bua_sip_addr
        };

        let target_addr = match self.state.resolve_b2bua_addr(target_host).await {
            Ok(addr) => addr,
            Err(e) => {
                error!("❌ Hedef çözümlenemedi: {}: {}", target_host, e);
                return Some((self.create_response(packet, 503, "Service Unavailable"), Some(src_addr)));
            }
        };

        // Rota bilgilerini Redis'e yaz
        let leg_a_key = format!("proxy:route:{}:{}", call_id, from_tag);
        let leg_b_key = format!("proxy:route:{}:{}", call_id, "callee"); // To-tag henüz bilinmiyor

        let mut conn = self.redis.lock().await;
        let _: () = conn.set_ex(&leg_a_key, src_addr.to_string(), 300).await.unwrap_or_default();
        let _: () = conn.set_ex(&leg_b_key, target_addr.to_string(), 300).await.unwrap_or_default();
        debug!("💾 [PROXY-STATE] CACHE SET: {} -> {}", leg_a_key, src_addr);
        debug!("💾 [PROXY-STATE] CACHE SET: {} -> {}", leg_b_key, target_addr);

        // Kendimizi sinyal yoluna ekle
        self.add_record_route(packet);
        self.add_via_header(packet);

        Some((packet.clone(), Some(target_addr)))
    }

    async fn handle_in_dialog_request(&self, packet: &mut SipPacket, src_addr: SocketAddr) -> Option<(SipPacket, Option<SocketAddr>)> {
        // Route başlığını soy
        packet.headers.remove(0);

        let call_id = utils::get_header(packet, HeaderName::CallId);
        let from_tag = self.extract_tag_from_header(&utils::get_header(packet, HeaderName::From));
        let to_tag = self.extract_tag_from_header(&utils::get_header(packet, HeaderName::To));

        // Bu isteğin hangi bacaktan geldiğini anla (A->B mi, B->A mi)
        let (source_leg_key, target_leg_key) = if !to_tag.is_empty() {
            (format!("proxy:route:{}:{}", call_id, from_tag), format!("proxy:route:{}:{}", call_id, to_tag))
        } else {
            // ACK için To-tag henüz olmayabilir
            (format!("proxy:route:{}:{}", call_id, from_tag), format!("proxy:route:{}:{}", call_id, "callee"))
        };

        let mut conn = self.redis.lock().await;
        match conn.get::<_, String>(&target_leg_key).await {
            Ok(target_str) => {
                if let Ok(target_addr) = target_str.parse() {
                    debug!("✅ [PROXY-STATE] CACHE HIT (In-Dialog): {} -> {}", source_leg_key, target_addr);
                    self.add_via_header(packet);
                    Some((packet.clone(), Some(target_addr)))
                } else {
                    warn!("⚠️ [PROXY-STATE] CACHE HATA (In-Dialog): Geçersiz adres formatı: {}", target_str);
                    None
                }
            }
            Err(_) => {
                warn!("⚠️ [PROXY-STATE] CACHE MISS (In-Dialog): {}. İstek düşürülüyor.", target_leg_key);
                // Burada hata yanıtı döndürmek daha doğru olabilir, ancak şimdilik düşürüyoruz.
                Some((self.create_response(packet, 481, "Call/Transaction Does Not Exist"), Some(src_addr)))
            }
        }
    }
    
    async fn handle_response(&self, packet: &mut SipPacket, _src_addr: SocketAddr) -> Option<(SipPacket, Option<SocketAddr>)> {
        // En üstteki Via başlığını (kendimizinki) soy
        if packet.headers.is_empty() || packet.headers[0].name != HeaderName::Via { return None; }
        packet.headers.remove(0);

        // Yanıt 200 OK ise ve To-tag içeriyorsa, 'callee' anahtarını gerçek tag ile güncelle
        if packet.status_code == 200 {
            if let Some(to_header) = packet.get_header_value(HeaderName::To) {
                let to_tag = self.extract_tag_from_header(to_header);
                if !to_tag.is_empty() {
                    let call_id = utils::get_header(packet, HeaderName::CallId);
                    let old_key = format!("proxy:route:{}:{}", call_id, "callee");
                    let new_key = format!("proxy:route:{}:{}", call_id, to_tag);
                    let mut conn = self.redis.lock().await;
                    // Anahtarı yeniden adlandır
                    let _: () = conn.rename_nx(old_key, new_key.clone()).await.unwrap_or_default();
                    debug!("💾 [PROXY-STATE] CACHE RENAME -> {}", new_key);
                }
            }
        }
        
        let call_id = utils::get_header(packet, HeaderName::CallId);
        let from_tag = self.extract_tag_from_header(&utils::get_header(packet, HeaderName::From));
        let redis_key = format!("proxy:route:{}:{}", call_id, from_tag);

        let mut conn = self.redis.lock().await;
        match conn.get::<_, String>(&redis_key).await {
            Ok(target_str) => {
                if let Ok(target_addr) = target_str.parse() {
                    debug!("✅ [PROXY-STATE] CACHE HIT (Response): {} -> {}", redis_key, target_addr);
                    Some((packet.clone(), Some(target_addr)))
                } else {
                    warn!("⚠️ [PROXY-STATE] CACHE HATA (Response): Geçersiz adres formatı: {}", target_str);
                    None
                }
            }
            Err(_) => {
                warn!("⚠️ [PROXY-STATE] CACHE MISS (Response): {}. Yanıt düşürülüyor.", redis_key);
                None
            }
        }
    }
    
    // --- HELPER FUNCTIONS ---

    fn add_via_header(&self, packet: &mut SipPacket) {
        let via_header = sentiric_sip_core::builder::build_via_header(&self.config.proxy_advertised_host, self.config.sip_port, "UDP");
        packet.headers.insert(0, via_header);
    }
    
    fn add_record_route(&self, packet: &mut SipPacket) {
        let rr_header = Header::new(
            HeaderName::RecordRoute,
            format!("<sip:{}:{};lr>", self.config.proxy_advertised_host, self.config.sip_port)
        );
        packet.headers.push(rr_header);
    }

    fn create_response(&self, req: &SipPacket, code: u16, reason: &str) -> SipPacket {
        let mut resp = SipPacket::new_response(code, reason.to_string());
        for h in &req.headers {
            if matches!(h.name, HeaderName::Via | HeaderName::From | HeaderName::To | HeaderName::CallId | HeaderName::CSeq) {
                resp.headers.push(h.clone());
            }
        }
        resp.headers.push(Header::new(HeaderName::Server, "Sentiric/1.1 Stateful Proxy".to_string()));
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