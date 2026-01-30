// sentiric-proxy-service/src/sip/engine.rs

use crate::config::AppConfig;
use crate::grpc::client::InternalClients;
use crate::sip::server::ProxyState;
use crate::sip::utils;
// DÜZELTME: Kullanılmayan `LookupContactRequest` kaldırıldı.
use sentiric_contracts::sentiric::sip::v1::RegisterRequest;
use sentiric_sip_core::{utils as sip_core_utils, Header, HeaderName, Method, SipPacket};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tonic::Request;
use tracing::{error, info, instrument, warn};
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
            match packet.method {
                Method::Register => self.handle_register(packet, src_addr).await,
                Method::Invite => self.handle_invite(packet, src_addr).await,
                _ => self.handle_in_dialog_request(packet).await,
            }
        } else {
            self.handle_response(packet).await
        }
    }

    async fn handle_register(&self, packet: &SipPacket, src_addr: SocketAddr) -> Option<(SipPacket, Option<SocketAddr>)> {
        let to_header = utils::get_header(packet, HeaderName::To);
        let aor = sip_core_utils::extract_aor(&to_header);
        let username = self.extract_username_from_uri(&aor).unwrap_or_else(|| "unknown".to_string());
        let real_contact_uri = format!("sip:{}@{}:{}", username, src_addr.ip(), src_addr.port());

        let mut clients = self.clients.lock().await;
        let req = Request::new(RegisterRequest { sip_uri: aor, contact_uri: real_contact_uri, expires: 3600 });

        match clients.registrar.register(req).await {
            Ok(_) => Some((self.create_response(packet, 200, "OK"), Some(src_addr))),
            Err(_) => Some((self.create_response(packet, 500, "Internal Server Error"), Some(src_addr))),
        }
    }

    async fn handle_invite(&self, packet: &mut SipPacket, src_addr: SocketAddr) -> Option<(SipPacket, Option<SocketAddr>)> {
        let call_id = utils::get_header(packet, HeaderName::CallId);
        let from_tag = self.extract_tag_from_header(&utils::get_header(packet, HeaderName::From));
        let to = utils::get_header(packet, HeaderName::To);
        let callee_username = self.extract_username_from_uri(&sip_core_utils::extract_aor(&to)).unwrap_or_default();

        let target_host = if callee_username == "9998" {
            info!("🎯 [PROXY] Yönlendirme Kararı (INVITE): PROBE");
            &self.config.probe_sip_addr
        } else {
            info!("🎯 [PROXY] Yönlendirme Kararı (INVITE): B2BUA (Default)");
            &self.config.b2bua_sip_addr
        };

        match self.state.resolve_b2bua_addr(target_host).await {
            Ok(target_addr) => {
                let redis_key = format!("proxy:route:{}:{}", call_id, from_tag);
                let mut conn = self.redis.lock().await;
                let _: redis::RedisResult<()> = conn.set_ex(redis_key.clone(), target_addr.to_string(), 300).await;
                info!("💾 [PROXY-STATE] Cache SET: {} -> {}", redis_key, target_addr);

                self.add_via_header(packet);
                Some((packet.clone(), Some(target_addr)))
            }
            Err(e) => {
                error!("❌ Hedef çözümlenemedi: {}: {}", target_host, e);
                Some((self.create_response(packet, 503, "Service Unavailable"), Some(src_addr)))
            }
        }
    }

    async fn handle_in_dialog_request(&self, packet: &mut SipPacket) -> Option<(SipPacket, Option<SocketAddr>)> {
        let call_id = utils::get_header(packet, HeaderName::CallId);
        let from_tag = self.extract_tag_from_header(&utils::get_header(packet, HeaderName::From));
        let redis_key = format!("proxy:route:{}:{}", call_id, from_tag);
        let mut conn = self.redis.lock().await;

        match conn.get::<_, String>(redis_key.clone()).await {
            Ok(target_str) => {
                if let Ok(target_addr) = target_str.parse::<SocketAddr>() {
                    info!("✅ [PROXY-STATE] CACHE HIT: {} -> {}", redis_key, target_addr);
                    self.add_via_header(packet);
                    return Some((packet.clone(), Some(target_addr)));
                }
            }
            Err(_) => {
                warn!("⚠️ [PROXY-STATE] CACHE MISS: {}. Varsayılan hedefe yönlendiriliyor.", redis_key);
            }
        }

        if let Ok(b2bua_addr) = self.state.resolve_b2bua_addr(&self.config.b2bua_sip_addr).await {
            self.add_via_header(packet);
            Some((packet.clone(), Some(b2bua_addr)))
        } else {
            None
        }
    }
    
    async fn handle_response(&self, packet: &mut SipPacket) -> Option<(SipPacket, Option<SocketAddr>)> {
        if !packet.headers.is_empty() && packet.headers[0].name == HeaderName::Via {
            packet.headers.remove(0);
        } else {
            return None;
        }
        if let Some(next_via) = packet.headers.iter().find(|h| h.name == HeaderName::Via) {
            if let Some(target) = self.parse_via_address(&next_via.value) {
                return Some((packet.clone(), Some(target)));
            }
        }
        None
    }

    fn add_via_header(&self, packet: &mut SipPacket) {
        let via_header = sentiric_sip_core::builder::build_via_header(&self.config.proxy_advertised_host, self.config.sip_port, "UDP");
        packet.headers.insert(0, via_header);
    }

    fn create_response(&self, req: &SipPacket, code: u16, reason: &str) -> SipPacket {
        let mut resp = SipPacket::new_response(code, reason.to_string());
        for h in &req.headers {
            if matches!(h.name, HeaderName::Via | HeaderName::From | HeaderName::To | HeaderName::CallId | HeaderName::CSeq) {
                resp.headers.push(h.clone());
            }
        }
        resp.headers.push(Header::new(HeaderName::Server, "Sentiric/1.1 Proxy".to_string()));
        resp.headers.push(Header::new(HeaderName::ContentLength, "0".to_string()));
        resp
    }

    fn extract_username_from_uri(&self, uri: &str) -> Option<String> {
        let clean = uri.trim_start_matches('<').trim_start_matches("sip:");
        let end_idx = clean.find('@').or_else(|| clean.find(':')).unwrap_or(clean.len());
        Some(clean[..end_idx].to_string())
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
    
    fn parse_via_address(&self, via_val: &str) -> Option<SocketAddr> {
        let parts: Vec<&str> = via_val.split_whitespace().collect();
        if parts.len() < 2 { return None; }
        let params: Vec<&str> = parts[1].split(';').collect();
        let mut host_port = params[0].to_string();
        let mut rport: Option<&str> = None;
        let mut received: Option<&str> = None;
        for param in &params[1..] {
            if let Some((k, v)) = param.split_once('=') {
                if k == "received" { received = Some(v); }
                if k == "rport" { rport = Some(v); }
            }
        }
        if let (Some(rec), Some(rp)) = (received, rport) {
            if let Ok(addr) = format!("{}:{}", rec, rp).parse() { return Some(addr); }
        }
        if !host_port.contains(':') { host_port = format!("{}:5060", host_port); }
        host_port.parse().ok()
    }
}