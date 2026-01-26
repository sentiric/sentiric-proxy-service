// sentiric-proxy-service/src/sip/engine.rs

use crate::config::AppConfig;
use crate::grpc::client::InternalClients;
use crate::sip::server::ProxyState;
use crate::sip::utils;
use sentiric_contracts::sentiric::dialplan::v1::ResolveDialplanRequest;
use sentiric_contracts::sentiric::sip::v1::{LookupContactRequest, RegisterRequest};
use sentiric_sip_core::{utils as sip_core_utils, Header, HeaderName, Method, SipPacket};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tonic::Request;
use tracing::{debug, error, info, instrument, warn};
use uuid::Uuid;

pub struct ProxyEngine {
    clients: Arc<Mutex<InternalClients>>,
    config: Arc<AppConfig>,
    state: Arc<ProxyState>,
}

impl ProxyEngine {
    pub fn new(
        clients: Arc<Mutex<InternalClients>>,
        config: Arc<AppConfig>,
        state: Arc<ProxyState>,
    ) -> Self {
        Self {
            clients,
            config,
            state,
        }
    }

    /// Gelen SIP paketlerini işler ve (Yanıt Paketi, Hedef Adres) döndürür.
    #[instrument(skip(self, packet), fields(method = %packet.method, is_request = packet.is_request, call_id = %utils::get_header(packet, HeaderName::CallId)))]
    pub async fn process_packet(
        &self,
        packet: &mut SipPacket,
        src_addr: SocketAddr,
    ) -> Option<(SipPacket, Option<SocketAddr>)> {
        if packet.is_request {
            match packet.method {
                Method::Register => {
                    if let Some(resp) = self.handle_register(packet, src_addr).await {
                        // REGISTER yanıtı her zaman isteğin geldiği kaynağa geri döner.
                        return Some((resp, Some(src_addr)));
                    }
                    None
                }
                Method::Invite => self.handle_invite(packet, src_addr).await,
                _ => self.handle_passthrough_request(packet).await,
            }
        } else {
            // Yanıtlar her zaman Via başlığına göre yönlendirilir.
            self.handle_response(packet).await
        }
    }

    /// REGISTER isteklerini işler, NAT'ı düzeltir ve registrar servisine iletir.
    async fn handle_register(&self, packet: &SipPacket, src_addr: SocketAddr) -> Option<SipPacket> {
        let to_header = utils::get_header(packet, HeaderName::To);
        let contact_header = utils::get_header(packet, HeaderName::Contact);
        let aor = sip_core_utils::extract_aor(&to_header);
        let username = self
            .extract_username_from_uri(&aor)
            .unwrap_or_else(|| "unknown".to_string());

        // NAT Traversal: Contact header'daki adresi, paketin geldiği gerçek IP ve port ile değiştir.
        let real_contact_uri = format!("sip:{}@{}:{}", username, src_addr.ip(), src_addr.port());
        info!(
            "REGISTER (NAT Fixed): AOR='{}', Claimed='{}', Actual='{}'",
            aor, contact_header, real_contact_uri
        );

        let mut clients = self.clients.lock().await;
        let req = Request::new(RegisterRequest {
            sip_uri: aor,
            contact_uri: real_contact_uri,
            expires: 3600,
        });

        match clients.registrar.register(req).await {
            Ok(_) => {
                info!("Registrar: Kayıt başarılı.");
                Some(self.create_response(packet, 200, "OK"))
            }
            Err(e) => {
                error!(error = %e, "Registrar hatası");
                Some(self.create_response(packet, 500, "Internal Server Error"))
            }
        }
    }

    /// INVITE isteklerini, kaynağına ve dialplan sonucuna göre akıllıca yönlendirir.
    async fn handle_invite(
        &self,
        packet: &mut SipPacket,
        src_addr: SocketAddr,
    ) -> Option<(SipPacket, Option<SocketAddr>)> {
        // B2BUA adresini cache'den veya DNS'ten al
        let b2bua_addr = match self.state.resolve_b2bua_addr(&self.config.b2bua_sip_addr).await {
            Ok(addr) => addr,
            Err(e) => {
                error!(error = %e, "KRİTİK: B2BUA adresi çözümlenemedi.");
                return Some((self.create_response(packet, 503, "Service Unavailable"), Some(src_addr)));
            }
        };

        // Outbound Çağrı: Paket B2BUA'dan geliyorsa, hedef URI'a yönlendirilir.
        if src_addr.ip() == b2bua_addr.ip() {
            if let Some(target_addr) = self.extract_target_addr(&packet.uri) {
                info!("🔄 Outbound INVITE: B2BUA -> {}", target_addr);
                self.add_via_header(packet);
                return Some((packet.clone(), Some(target_addr)));
            }
            error!("❌ Outbound INVITE hedef adresi çözülemedi: {}", packet.uri);
            return None;
        }

        // Inbound Çağrı: Paket dış dünyadan geliyorsa, dialplan'e sorulur.
        let from = utils::get_header(packet, HeaderName::From);
        let to = utils::get_header(packet, HeaderName::To);
        let caller_aor = sip_core_utils::extract_aor(&from);
        let callee_aor = sip_core_utils::extract_aor(&to);
        let caller = self.extract_username_from_uri(&caller_aor).unwrap_or(caller_aor);
        let callee = self.extract_username_from_uri(&callee_aor).unwrap_or(callee_aor.clone());

        info!("➡️ Inbound INVITE: {} -> {}", caller, callee);

        let dialplan_result = {
            let mut clients = self.clients.lock().await;
            clients
                .dialplan
                .resolve_dialplan(Request::new(ResolveDialplanRequest {
                    caller_contact_value: caller,
                    destination_number: callee,
                }))
                .await
        };

        match dialplan_result {
            Ok(response) => {
                let resp = response.into_inner();
                let action = resp.action.map(|a| a.action).unwrap_or_default();
                info!("🧠 Dialplan Kararı: '{}' (PlanID: {})", action, resp.dialplan_id);

                match action.as_str() {
                    "BRIDGE_CALL" => self.route_to_internal_peer(packet, &callee_aor, src_addr).await,
                    "START_AI_CONVERSATION" | "PROCESS_GUEST_CALL" | "PLAY_ANNOUNCEMENT" => {
                        info!("🤖 AI Routing: Çağrı B2BUA'ya yönlendiriliyor. ({})", b2bua_addr);
                        self.add_via_header(packet);
                        Some((packet.clone(), Some(b2bua_addr)))
                    }
                    _ => {
                        warn!("⚠️ Bilinmeyen Dialplan Aksiyonu: {}", action);
                        Some((self.create_response(packet, 501, "Not Implemented"), Some(src_addr)))
                    }
                }
            }
            Err(e) => {
                error!(error = %e, "Dialplan servisi hatası");
                Some((self.create_response(packet, 503, "Dialplan Service Unavailable"), Some(src_addr)))
            }
        }
    }

    /// Dahili bir aboneye yönlendirme mantığı.
    async fn route_to_internal_peer(&self, packet: &mut SipPacket, callee_aor: &str, src_addr: SocketAddr) -> Option<(SipPacket, Option<SocketAddr>)> {
        let lookup_result = {
            let mut clients = self.clients.lock().await;
            clients
                .registrar
                .lookup_contact(Request::new(LookupContactRequest {
                    sip_uri: callee_aor.to_string(),
                }))
                .await
        };

        match lookup_result {
            // DÜZELTME: E0507 - Match guard kaldırıldı, sahiplik hatası giderildi.
            Ok(lookup_resp) => {
                let contacts = lookup_resp.into_inner().contact_uris;
                if !contacts.is_empty() {
                    let contact_uri = &contacts[0];
                    if let Some(target_addr) = self.extract_target_addr(contact_uri) {
                        if target_addr == src_addr {
                            warn!("⚠️ Loop Detected: Hedef ({}) ile Kaynak ({}) aynı.", target_addr, src_addr);
                            return Some((self.create_response(packet, 482, "Loop Detected"), Some(src_addr)));
                        }
                        info!("✅ Dahili Yönlendirme (Bridge): {} -> {}", callee_aor, target_addr);
                        self.add_via_header(packet);
                        return Some((packet.clone(), Some(target_addr)));
                    }
                    warn!("❌ Geçersiz contact URI: {}", contact_uri);
                    Some((self.create_response(packet, 404, "User Found But Unreachable"), Some(src_addr)))
                } else {
                    warn!("❌ Dialplan BRIDGE dedi ama abone ({}) kayıtlı değil.", callee_aor);
                    Some((self.create_response(packet, 404, "User Not Found"), Some(src_addr)))
                }
            }
            Err(e) => {
                error!(error = %e, "Registrar hatası");
                Some((self.create_response(packet, 500, "Internal Server Error"), Some(src_addr)))
            }
        }
    }

    /// ACK, BYE gibi diyalog içi istekleri hedeflerine yönlendirir.
    async fn handle_passthrough_request(&self, packet: &mut SipPacket) -> Option<(SipPacket, Option<SocketAddr>)> {
        if let Some(target_addr) = self.extract_target_addr(&packet.uri) {
            debug!(" पास-थ्रू Request ({}) -> {}", packet.method, target_addr);
            self.add_via_header(packet);
            Some((packet.clone(), Some(target_addr)))
        } else {
            error!("❌ Passthrough isteği için hedef URI çözülemedi: {}", packet.uri);
            None
        }
    }

    /// Gelen yanıtları, Via başlık zincirine göre bir önceki hop'a geri gönderir.
    async fn handle_response(&self, packet: &mut SipPacket) -> Option<(SipPacket, Option<SocketAddr>)> {
        // 1. Kendi Via başlığımızı paketten çıkar.
        if !packet.headers.is_empty() && packet.headers[0].name == HeaderName::Via {
            packet.headers.remove(0);
        } else {
            warn!("Via başlığı olmayan response paketi yönlendirilemiyor.");
            return None;
        }

        // 2. Zincirdeki bir sonraki (artık ilk sıradaki) Via başlığına bakarak hedefi bul.
        if let Some(next_via) = packet.headers.iter().find(|h| h.name == HeaderName::Via) {
            if let Some(target) = self.parse_via_address(&next_via.value) {
                // DÜZELTME: E0599 - .unwrap_or(0) kaldırıldı, packet.status_code (u16) doğrudan kullanıldı.
                debug!("↩️ Yönlendirme Yanıtı ({}) -> {}", packet.status_code, target);
                return Some((packet.clone(), Some(target)));
            }
        }
        warn!("Response için sonraki hop bulunamadı.");
        None
    }

    /// Pakete kendi Via başlığımızı ekler.
    fn add_via_header(&self, packet: &mut SipPacket) {
        let branch = format!("z9hG4bK-proxy-{}", Uuid::new_v4());
        let via_val = format!(
            "SIP/2.0/UDP {}:{};branch={}",
            self.config.proxy_advertised_host, self.config.sip_port, branch
        );
        packet.headers.insert(0, Header::new(HeaderName::Via, via_val));
    }

    /// Standart bir SIP yanıt paketi oluşturur.
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

    /// URI'dan kullanıcı adını (örn: "1001") çıkarır.
    fn extract_username_from_uri(&self, uri: &str) -> Option<String> {
        let clean = uri.trim_start_matches('<').trim_start_matches("sip:");
        clean.find('@').map(|at_idx| clean[..at_idx].to_string())
    }

    /// SIP URI'dan (örn: "sip:1001@1.2.3.4:5060") hedef `SocketAddr`'ı çıkarır.
    fn extract_target_addr(&self, uri: &str) -> Option<SocketAddr> {
        let clean = uri.trim_start_matches("sip:").trim_start_matches('<').trim_end_matches('>');
        let host_port_part = clean.find('@').map_or(clean, |at_idx| &clean[at_idx + 1..]);
        let host_port = host_port_part.find(';').map_or(host_port_part, |semi_idx| &host_port_part[..semi_idx]);
        
        if !host_port.contains(':') {
            format!("{}:5060", host_port).parse().ok()
        } else {
            host_port.parse().ok()
        }
    }
    
    /// Via başlığından (rport/received ile) gerçek adresi çözer.
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
            if let Ok(addr) = format!("{}:{}", rec, rp).parse() {
                return Some(addr);
            }
        }

        if !host_port.contains(':') {
            host_port = format!("{}:5060", host_port);
        }
        host_port.parse().ok()
    }
}