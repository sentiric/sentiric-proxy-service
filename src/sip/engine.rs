// sentiric-proxy-service/src/sip/engine.rs

use crate::config::AppConfig;
use crate::grpc::client::InternalClients;
use crate::sip::server::{ProxyState, DEFAULT_SIP_PORT};
use crate::sip::utils;
use sentiric_contracts::sentiric::sip::v1::RegisterRequest;
use sentiric_sip_core::{
    utils as sip_core_utils, 
    Header, HeaderName, Method, SipPacket,
    SipRouter 
};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tonic::Request;
use tracing::{error, info, instrument, warn, debug};
use redis::AsyncCommands;

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
        debug!("🔫 [TRACE-PROXY] Paket Alındı. Method: {}, Src: {}", packet.method, src_addr);

        if packet.is_request {
            self.handle_request(packet, src_addr).await
        } else {
            self.handle_response(packet).await
        }
    }

    async fn handle_request(&self, packet: &mut SipPacket, src_addr: SocketAddr) -> Option<(SipPacket, Option<SocketAddr>)> {
        if packet.method == Method::Register {
            return self.handle_register(packet, src_addr).await;
        }

        let to_tag = self.extract_tag_from_header(&utils::get_header(packet, HeaderName::To));
        let has_route_header = packet.headers.iter().any(|h| h.name == HeaderName::Route);

        if !to_tag.is_empty() || has_route_header {
            debug!("🔄 [PROXY-HANDLE] Diyalog içi istek (To-Tag veya Route var): {}", packet.method);
            return self.handle_in_dialog_request(packet, src_addr).await;
        }
        
        if packet.method == Method::Invite {
            debug!("📞 [PROXY-HANDLE] Yeni INVITE isteği.");
            return self.handle_initial_invite(packet, src_addr).await;
        }

        warn!("⚠️ [PROXY-HANDLE] Bilinmeyen/Stateless istek: {}. Varsayılan B2BUA'ya yönlendiriliyor.", packet.method);
        let target_host = &self.config.b2bua_sip_addr;
        match self.state.resolve_b2bua_addr(target_host).await {
            Ok(target_addr) => Some((packet.clone(), Some(target_addr))),
            Err(e) => {
                error!("❌ Hedef çözümlenemedi: {}: {}", target_host, e);
                Some((self.create_response(packet, 503, "Service Unavailable"), Some(src_addr)))
            }
        }
    }

    async fn handle_register(&self, packet: &SipPacket, src_addr: SocketAddr) -> Option<(SipPacket, Option<SocketAddr>)> {
        let to_header = utils::get_header(packet, HeaderName::To);
        let aor = sip_core_utils::extract_aor(&to_header);
        let username = sip_core_utils::extract_username_from_uri(&aor);
        
        let via_val = utils::get_header(packet, HeaderName::Via);
        let client_addr = SipRouter::resolve_response_target(&via_val, DEFAULT_SIP_PORT).unwrap_or(src_addr);
        
        // Kullanıcının gerçek IP'sini (client_addr) Contact URI olarak kaydediyoruz.
        // Böylece Proxy, kullanıcıya geri dönebilir.
        let real_contact_uri = format!("sip:{}@{}:{}", username, client_addr.ip(), client_addr.port());

        let mut clients = self.clients.lock().await;
        let req = Request::new(RegisterRequest { sip_uri: aor, contact_uri: real_contact_uri, expires: 3600 });

        match clients.registrar.register(req).await {
            Ok(_) => Some((self.create_response(packet, 200, "OK"), Some(client_addr))),
            Err(e) => {
                error!("Registrar Service error: {}", e);
                Some((self.create_response(packet, 500, "Internal Server Error"), Some(client_addr)))
            }
        }
    }

    async fn handle_initial_invite(&self, packet: &mut SipPacket, src_addr: SocketAddr) -> Option<(SipPacket, Option<SocketAddr>)> {
        let call_id = utils::get_header(packet, HeaderName::CallId);
        let from_tag = self.extract_tag_from_header(&utils::get_header(packet, HeaderName::From));
        let to_aor = sip_core_utils::extract_aor(&utils::get_header(packet, HeaderName::To));
        let callee_username = sip_core_utils::extract_username_from_uri(&to_aor);
        
        let via_val = utils::get_header(packet, HeaderName::Via);
        let client_addr = SipRouter::resolve_response_target(&via_val, DEFAULT_SIP_PORT).unwrap_or(src_addr);

        // Routing Kararı
        let target_host = if callee_username == "9998" {
            info!("🎯 [PROXY-ROUTE] INVITE -> PROBE (9998)");
            &self.config.probe_sip_addr
        } else {
            info!("🎯 [PROXY-ROUTE] INVITE -> B2BUA (Default)");
            &self.config.b2bua_sip_addr
        };

        let target_addr = match self.state.resolve_b2bua_addr(target_host).await {
            Ok(addr) => addr,
            Err(e) => {
                error!("❌ Hedef çözümlenemedi: {}: {}", target_host, e);
                return Some((self.create_response(packet, 503, "Service Unavailable"), Some(client_addr)));
            }
        };

        // Redis State Update
        let client_leg_key = format!("proxy:route:{}:{}", call_id, from_tag);
        let target_leg_key_placeholder = format!("proxy:route:{}:{}", call_id, "callee");

        let mut conn = self.redis.lock().await;
        let _: () = conn.set_ex(&client_leg_key, client_addr.to_string(), 300).await.unwrap_or_default();
        let _: () = conn.set_ex(&target_leg_key_placeholder, target_addr.to_string(), 300).await.unwrap_or_default();
        
        // --- CORE KULLANIMI ---
        SipRouter::add_record_route(packet, &self.config.proxy_advertised_host, self.config.sip_port);
        SipRouter::add_via(packet, &self.config.proxy_advertised_host, self.config.sip_port, "UDP");

        Some((packet.clone(), Some(target_addr)))
    }

    async fn handle_in_dialog_request(&self, packet: &mut SipPacket, src_addr: SocketAddr) -> Option<(SipPacket, Option<SocketAddr>)> {
        // Loose Routing Logic
        if !packet.headers.is_empty() && packet.headers[0].name == HeaderName::Route {
             packet.headers.remove(0);
        }

        let call_id = utils::get_header(packet, HeaderName::CallId);
        let to_tag = self.extract_tag_from_header(&utils::get_header(packet, HeaderName::To));

        let target_redis_key = if !to_tag.is_empty() {
            format!("proxy:route:{}:{}", call_id, to_tag)
        } else {
            format!("proxy:route:{}:{}", call_id, "callee")
        };
        
        let mut conn = self.redis.lock().await;
        match conn.get::<_, String>(&target_redis_key).await {
            Ok(target_str) => {
                if let Ok(target_addr) = target_str.parse() {
                    // --- CORE KULLANIMI ---
                    SipRouter::add_via(packet, &self.config.proxy_advertised_host, self.config.sip_port, "UDP");
                    Some((packet.clone(), Some(target_addr)))
                } else {
                    Some((self.create_response(packet, 500, "Internal Server Error"), Some(src_addr)))
                }
            }
            Err(_) => {
                Some((self.create_response(packet, 481, "Call Does Not Exist"), Some(src_addr)))
            }
        }
    }
    
    async fn handle_response(&self, packet: &mut SipPacket) -> Option<(SipPacket, Option<SocketAddr>)> {
        // --- CORE KULLANIMI ---
        if SipRouter::strip_top_via(packet).is_none() {
            warn!("⚠️ [PROXY-HANDLE] Yanıt paketinde Via başlığı bulunamadı.");
            return None;
        }

        let call_id = utils::get_header(packet, HeaderName::CallId);
        let from_tag = self.extract_tag_from_header(&utils::get_header(packet, HeaderName::From));
        let to_tag = self.extract_tag_from_header(&utils::get_header(packet, HeaderName::To));

        // Late Binding (200 OK ile To-Tag eşleşmesi)
        if packet.status_code >= 200 && !to_tag.is_empty() {
            let old_key = format!("proxy:route:{}:{}", call_id, "callee");
            let new_key = format!("proxy:route:{}:{}", call_id, to_tag);
            let mut conn = self.redis.lock().await;
            let _: redis::RedisResult<()> = conn.rename_nx(&old_key, &new_key).await;
        }
        
        let target_redis_key = format!("proxy:route:{}:{}", call_id, from_tag);

        let mut conn = self.redis.lock().await;
        match conn.get::<_, String>(&target_redis_key).await {
            Ok(target_str) => {
                if let Ok(target_addr) = target_str.parse() {
                    Some((packet.clone(), Some(target_addr)))
                } else {
                    None
                }
            }
            Err(_) => {
                if let Some(next_via) = packet.headers.iter().find(|h| h.name == HeaderName::Via) {
                    return SipRouter::resolve_response_target(&next_via.value, DEFAULT_SIP_PORT)
                        .map(|target| (packet.clone(), Some(target)));
                }
                None
            }
        }
    }

    fn create_response(&self, req: &SipPacket, code: u16, reason: &str) -> SipPacket {
        // --- CORE KULLANIMI ---
        let mut resp = SipPacket::create_response_for(req, code, reason.to_string());
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