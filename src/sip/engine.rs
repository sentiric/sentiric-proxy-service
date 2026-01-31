// sentiric-proxy-service/src/sip/engine.rs

use crate::config::AppConfig;
use crate::grpc::client::InternalClients;
use crate::sip::server::{ProxyState, DEFAULT_SIP_PORT};
use crate::sip::utils;
use sentiric_contracts::sentiric::sip::v1::RegisterRequest;
use sentiric_sip_core::{utils as sip_core_utils, Header, HeaderName, Method, SipPacket};
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
        // 1. REGISTER İsteği (Stateful, Registrar'a gider)
        if packet.method == Method::Register {
            return self.handle_register(packet, src_addr).await;
        }

        // 2. Diyalog İçi İstekler (ACK, BYE, RE-INVITE) - Route header veya To-tag kontrolü
        let to_tag = self.extract_tag_from_header(&utils::get_header(packet, HeaderName::To));
        let has_route_header = packet.headers.iter().any(|h| h.name == HeaderName::Route);

        if !to_tag.is_empty() || has_route_header {
            debug!("🔄 [PROXY-HANDLE] Diyalog içi istek (To-Tag veya Route var): {}", packet.method);
            return self.handle_in_dialog_request(packet, src_addr).await;
        }
        
        // 3. Yeni Çağrı Başlatma (Initial INVITE)
        if packet.method == Method::Invite {
            debug!("📞 [PROXY-HANDLE] Yeni INVITE isteği.");
            return self.handle_initial_invite(packet, src_addr).await;
        }

        // 4. Diğer İstekler (OPTIONS, MESSAGE vb.) - Varsayılan olarak B2BUA'ya
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
        
        // NAT Arkasındaki Client'ı bul
        let client_addr = self.parse_via_address(&utils::get_header(packet, HeaderName::Via)).unwrap_or(src_addr);
        
        // Registrar'a "Gerçek Ulaşılabilir Adres"i (IP:Port) Contact olarak bildiriyoruz.
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
        
        // İstemcinin NAT adresini çözümle
        let client_addr = self.parse_via_address(&utils::get_header(packet, HeaderName::Via)).unwrap_or(src_addr);

        // Routing Logic
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

        // Redis'e State Kaydet (Stateful Proxy)
        let client_leg_key = format!("proxy:route:{}:{}", call_id, from_tag);
        let target_leg_key_placeholder = format!("proxy:route:{}:{}", call_id, "callee");

        let mut conn = self.redis.lock().await;
        // Hata durumunda log bas ama akışı kesme (Redis optional sayılabilir mi? Hayır, ama fail-open deneyebiliriz)
        if let Err(e) = conn.set_ex::<_, _, ()>(&client_leg_key, client_addr.to_string(), 300).await {
            error!("Redis Error (Set Client Leg): {}", e);
        }
        if let Err(e) = conn.set_ex::<_, _, ()>(&target_leg_key_placeholder, target_addr.to_string(), 300).await {
             error!("Redis Error (Set Target Leg): {}", e);
        }
        
        debug!("💾 [PROXY-STATE] CACHE SET (Leg A): {} -> {}", client_leg_key, client_addr);
        debug!("💾 [PROXY-STATE] CACHE SET (Leg B Placeholder): {} -> {}", target_leg_key_placeholder, target_addr);

        // Header Manipülasyonu
        self.add_record_route(packet);
        self.add_via_header(packet);

        Some((packet.clone(), Some(target_addr)))
    }

    async fn handle_in_dialog_request(&self, packet: &mut SipPacket, src_addr: SocketAddr) -> Option<(SipPacket, Option<SocketAddr>)> {
        // Loose Routing: Route header varsa en üsttekini (kendimizinkini) kaldır.
        if !packet.headers.is_empty() && packet.headers[0].name == HeaderName::Route {
             packet.headers.remove(0);
        }

        let call_id = utils::get_header(packet, HeaderName::CallId);
        let from_tag = self.extract_tag_from_header(&utils::get_header(packet, HeaderName::From));
        let to_tag = self.extract_tag_from_header(&utils::get_header(packet, HeaderName::To));

        // Hedef routing key: Çağrı ID + To Tag
        // Eğer To Tag yoksa (henüz cevap gelmediyse), "callee" placeholder'ına bak.
        let target_redis_key = if !to_tag.is_empty() {
            format!("proxy:route:{}:{}", call_id, to_tag)
        } else {
            format!("proxy:route:{}:{}", call_id, "callee")
        };
        
        debug!("- [PROXY-STATE] In-dialog lookup using from_tag: {}, to_tag: {}", from_tag, to_tag);

        let mut conn = self.redis.lock().await;
        match conn.get::<_, String>(&target_redis_key).await {
            Ok(target_str) => {
                if let Ok(target_addr) = target_str.parse() {
                    debug!("✅ [PROXY-STATE] CACHE HIT (In-Dialog Request): {} -> {}", target_redis_key, target_addr);
                    self.add_via_header(packet);
                    Some((packet.clone(), Some(target_addr)))
                } else {
                    warn!("⚠️ [PROXY-STATE] CACHE HATA (In-Dialog Request): Geçersiz adres formatı: {}", target_str);
                    Some((self.create_response(packet, 500, "Internal Server Error"), Some(src_addr)))
                }
            }
            Err(e) => {
                warn!("⚠️ [PROXY-STATE] CACHE MISS (In-Dialog Request): {}. Hata: {}. İstek düşürülüyor.", target_redis_key, e);
                // Cache miss durumunda 481 dönmek standarttır.
                Some((self.create_response(packet, 481, "Call/Transaction Does Not Exist"), Some(src_addr)))
            }
        }
    }
    
    async fn handle_response(&self, packet: &mut SipPacket) -> Option<(SipPacket, Option<SocketAddr>)> {
        // Response Routing: Via başlıklarını kullanarak geri döner.
        // En üstteki Via (bizim eklediğimiz) kaldırılır.
        if packet.headers.is_empty() || packet.headers[0].name != HeaderName::Via {
            warn!("⚠️ [PROXY-HANDLE] Yanıt paketinde kendi Via başlığı bulunamadı.");
            return None;
        }
        packet.headers.remove(0);

        let call_id = utils::get_header(packet, HeaderName::CallId);
        let from_tag = self.extract_tag_from_header(&utils::get_header(packet, HeaderName::From));
        let to_tag = self.extract_tag_from_header(&utils::get_header(packet, HeaderName::To));

        // 200 OK geldiğinde ve To-tag oluştuğunda, "callee" placeholder'ını güncelle.
        if packet.status_code >= 200 && !to_tag.is_empty() {
            let old_key = format!("proxy:route:{}:{}", call_id, "callee");
            let new_key = format!("proxy:route:{}:{}", call_id, to_tag);
            let mut conn = self.redis.lock().await;
            
            // Eğer rename başarısız olursa (key yoksa), yeni key oluşturmayı dene veya yoksay.
            match conn.rename_nx::<_, _, i32>(&old_key, &new_key).await {
                Ok(_) => debug!("💾 [PROXY-STATE] CACHE RENAME (To-Tag Update): {} -> {}", old_key, new_key),
                Err(e) => warn!("⚠️ [PROXY-STATE] CACHE RENAME ERROR: {}", e),
            }
        }
        
        // Yanıtın gideceği yer = İsteği yapan taraf (From Tag ile ilişkili kayıt)
        let target_redis_key = format!("proxy:route:{}:{}", call_id, from_tag);

        let mut conn = self.redis.lock().await;
        match conn.get::<_, String>(&target_redis_key).await {
            Ok(target_str) => {
                if let Ok(target_addr) = target_str.parse() {
                    debug!("✅ [PROXY-STATE] CACHE HIT (Response Routing): {} -> {}", target_redis_key, target_addr);
                    Some((packet.clone(), Some(target_addr)))
                } else {
                    warn!("⚠️ [PROXY-STATE] CACHE HATA (Response Routing): Geçersiz adres formatı: {}", target_str);
                    None
                }
            }
            Err(_) => {
                // Redis'te bulamazsak Via başlığına güvenmek zorundayız (Fallback)
                warn!("⚠️ [PROXY-STATE] CACHE MISS (Response Routing): {}. Via'dan fallback deniyor.", target_redis_key);
                if let Some(next_via) = packet.headers.iter().find(|h| h.name == HeaderName::Via) {
                    return self.parse_via_address(&next_via.value).map(|target| (packet.clone(), Some(target)));
                }
                None
            }
        }
    }
    
    fn add_via_header(&self, packet: &mut SipPacket) {
        let via_header = sentiric_sip_core::builder::build_via_header(
            &self.config.proxy_advertised_host, 
            self.config.sip_port, 
            "UDP"
        );
        debug!("Adding Via Header: {}", via_header.value);
        packet.headers.insert(0, via_header);
    }
    
    fn add_record_route(&self, packet: &mut SipPacket) {
        // Record-Route: Proxy'nin diyalog boyunca kalıcı olmasını sağlar.
        // Format: <sip:proxy-host:port;lr> (lr = loose routing)
        let rr_value = format!("<sip:{}:{};lr>", self.config.proxy_advertised_host, self.config.sip_port);
        let rr_header = Header::new(HeaderName::RecordRoute, rr_value);
        
        packet.headers.insert(0, rr_header);
        debug!("Adding Record-Route Header: {}", packet.headers[0].value);
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

    // RFC 3261 Compliant Via Parser (SBC'deki gibi robust)
    fn parse_via_address(&self, via_val: &str) -> Option<SocketAddr> {
        let parts: Vec<&str> = via_val.split_whitespace().collect();
        if parts.len() < 2 { return None; }
        
        let protocol_part = parts[1];
        let params: Vec<&str> = protocol_part.split(';').collect();
        let mut host_part = params[0].to_string(); 
        
        let mut rport: Option<String> = None;
        let mut received: Option<String> = None;

        for param in &params[1..] {
             let p_trim = param.trim();
            if let Some((k, v)) = p_trim.split_once('=') {
                if k == "received" { received = Some(v.to_string()); }
                if k == "rport" { rport = Some(v.to_string()); }
            } else if p_trim == "rport" {
                // Değersiz rport parametresi (rport;)
                rport = Some("".to_string());
            }
        }

        // 1. NAT Traversal: received ve rport varsa bunları kullan
        if let (Some(rec), Some(rp)) = (received, rport) {
            // rport değeri boşsa bile IP:Port kombinasyonunda kullanıcaz (Genellikle SBC doldurur ama biz yine de bakalım)
            // Eğer rport boşsa, received portunu bilemeyiz, o yüzden standart portu deneriz ama bu riskli.
            // Proxy genellikle received IP ve rport'u kullanır. 
            // Eğer rport boş geldiyse, bu SBC'den değil doğrudan client'tan gelmiş olabilir.
            if !rp.is_empty() {
                return format!("{}:{}", rec, rp).parse().ok();
            }
             // Eğer rport var ama değer yoksa, ve received varsa, received IP'sini kullan ama portu header'dan al (veya 5060 varsay)
            if host_part.contains(':') {
                let port_part = host_part.split(':').last().unwrap();
                return format!("{}:{}", rec, port_part).parse().ok();
            }
            return format!("{}:{}", rec, DEFAULT_SIP_PORT).parse().ok();
        }

        // 2. Direct Routing: Host kısmını kullan
        if !host_part.contains(':') {
             host_part = format!("{}:{}", host_part, DEFAULT_SIP_PORT);
        }
        host_part.parse().ok()
    }
}