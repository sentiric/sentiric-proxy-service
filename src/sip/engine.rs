// Dosya: sentiric-sip-proxy-service/src/sip/engine.rs
use crate::config::AppConfig;
use crate::sip::server::ProxyState; 
use crate::sip::handlers::routing::{RoutingHandler, RedisConn};
use crate::grpc::service::MyProxyService;

use sentiric_sip_core::{
    Header, HeaderName, Method, SipPacket,
    SipRouter,
    TransactionEngine, TransactionAction, SipTransaction,
    utils as sip_utils, 
    builder::SipResponseFactory
};
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{error, info, debug, instrument, warn};
use dashmap::DashMap;

use sentiric_contracts::sentiric::sip::v1::proxy_service_server::ProxyService;

pub type TransactionStore = Arc<DashMap<String, SipTransaction>>;

pub struct ProxyEngine {
    config: Arc<AppConfig>,
    state: Arc<ProxyState>, 
    _router: RoutingHandler, 
    transactions: TransactionStore,
    routing_logic: Arc<MyProxyService>,
}

impl ProxyEngine {
    pub fn new(
        config: Arc<AppConfig>, 
        state: Arc<ProxyState>, 
        redis: RedisConn,
        routing_logic: Arc<MyProxyService>
    ) -> Self {
        Self { 
            config, 
            state, 
            _router: RoutingHandler::new(redis),
            transactions: Arc::new(DashMap::new()),
            routing_logic,
        }
    }

    #[instrument(skip(self, packet))]
    pub async fn process_packet(&self, packet: &mut SipPacket, src_addr: SocketAddr) -> Option<(SipPacket, Option<SocketAddr>)> {
        let call_id = packet.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();

        if packet.is_request() {
            let is_in_dialog = packet.is_in_dialog_request();
            
            if !is_in_dialog && SipRouter::detect_loop(packet, &self.config.proxy_advertised_host, self.config.sip_port) {
                warn!(event="SIP_LOOP_DETECTED", sip.call_id=%call_id, "🔄 Döngü tespit edildi, paket durduruldu.");
                return Some((SipResponseFactory::create_error(packet, 482, "Loop Detected"), Some(src_addr)));
            }
            
            if SipRouter::decrement_max_forwards(packet).is_err() {
                warn!(event="SIP_MAX_FORWARDS", sip.call_id=%call_id, "🛑 Maksimum atlama sınırına ulaşıldı.");
                return Some((SipResponseFactory::create_error(packet, 483, "Too Many Hops"), Some(src_addr)));
            }

            if packet.method != Method::Ack {
                let branch = packet.get_header_value(HeaderName::Via)
                    .and_then(|v| v.split("branch=").nth(1))
                    .and_then(|v| v.split(';').next())
                    .unwrap_or("unknown_branch");

                let tx_key = format!("{}:{}:{:?}", call_id, branch, packet.method);
                
                let action = if let Some(tx) = self.transactions.get(&tx_key) {
                    TransactionEngine::check(&Some(tx.clone()), packet)
                } else {
                    if let Some(new_tx) = SipTransaction::new(packet) { self.transactions.insert(tx_key.clone(), new_tx); }
                    TransactionAction::ForwardToApp
                };

                match action {
                    TransactionAction::Retransmit(cached_resp) => {
                        debug!(event="SIP_RETRANSMIT", sip.call_id=%call_id, "Tekrar eden paket, cache'den yanıtlanıyor.");
                        return Some((cached_resp, Some(src_addr)));
                    },
                    TransactionAction::Ignore => return None,
                    TransactionAction::ForwardToApp => {
                        packet.headers.retain(|h| {
                            if h.name == HeaderName::Route {
                                !h.value.contains(&self.config.proxy_advertised_host) && 
                                !h.value.contains(&self.config.public_ip)
                            } else {
                                true
                            }
                        });

                        let response_tuple = self.handle_request(packet, src_addr).await;
                        
                        if let Some((ref resp_packet, _)) = response_tuple {
                            if let Some(mut tx) = self.transactions.get_mut(&tx_key) {
                                tx.update_with_response(resp_packet);
                            }
                        }
                        
                        return response_tuple;
                    }
                }
            }

            self.handle_request(packet, src_addr).await
        } else {
            self.handle_response(packet).await
        }
    }

    async fn handle_request(&self, packet: &mut SipPacket, src_addr: SocketAddr) -> Option<(SipPacket, Option<SocketAddr>)> {
        if packet.method == Method::Register {
            return self.handle_register(packet, src_addr).await;
        }

        let from_uri = packet.get_header_value(HeaderName::From).cloned().unwrap_or_default();
        let dest_uri = packet.uri.clone();
        let in_dialog = packet.is_in_dialog_request();
        let call_id = packet.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
        
        //[ARCH-COMPLIANCE] TYPE FIX: unwrap_or(0) kaldırıldı
        let method = if packet.is_request() { packet.method.as_str().to_string() } else { format!("RESPONSE/{}", packet.status_code) };

        //[ARCH-COMPLIANCE]: IN-DIALOG STATEFUL ROUTING (P2P ve B2BUA İçin Ortak Çözüm)
        if in_dialog && packet.method != Method::Cancel {
            // [CRITICAL FIX]: SBC kendi Via'sını eklediği için, gerçek kaynağı bulmak için SBC'nin Via'sını atlayıp İKİNCİ Via'ya bakıyoruz.
            let via_count = packet.headers.iter().filter(|h| h.name == HeaderName::Via).count();
            let real_src_ip = if via_count > 1 {
                packet.headers.iter().filter(|h| h.name == HeaderName::Via).nth(1)
                    .and_then(|v| SipRouter::resolve_response_target(&v.value, 5060))
                    .unwrap_or(src_addr)
            } else {
                packet.get_header_value(HeaderName::Via)
                    .and_then(|v| SipRouter::resolve_response_target(v, 5060))
                    .unwrap_or(src_addr)
            };
                
            if let Some(real_peer) = self._router.resolve_in_dialog_target(&call_id, real_src_ip).await {
                info!(event="SIP_INBOUND_IN_DIALOG", sip.call_id=%call_id, target=%real_peer, "In-Dialog İstek Redis rotasıyla stateful yönlendiriliyor");
                
                let dest_user = sip_utils::extract_username_from_uri(&dest_uri);
                packet.uri = format!("sip:{}@{}:{}", dest_user, real_peer.ip(), real_peer.port());
                
                SipRouter::add_via(packet, &self.config.proxy_advertised_host, self.config.sip_port, "UDP");
                
                // P2P paketini dış dünyaya ulaştırması için onu bize getiren kaynağa (SBC'ye) iade ediyoruz.
                return Some((packet.clone(), Some(src_addr)));
            }
        }

        let mut request = tonic::Request::new(
            sentiric_contracts::sentiric::sip::v1::GetNextHopRequest {
                destination_uri: dest_uri.clone(),
                source_ip: src_addr.ip().to_string(),
                method: packet.method.as_str().to_string(),
                from_uri,
                is_in_dialog: in_dialog,
            }
        );

        //[ARCH-COMPLIANCE] Kesin Timeout ZORUNLULUĞU
        request.set_timeout(std::time::Duration::from_secs(3));

        if !call_id.is_empty() {
             if let Ok(meta_val) = tonic::metadata::MetadataValue::try_from(call_id.as_str()) {
                 request.metadata_mut().insert("x-trace-id", meta_val);
             }
        }

        match self.routing_logic.get_next_hop(request).await {
            Ok(res) => {
                let inner_res = res.into_inner();
                let next_hop_uri = inner_res.uri; 
                let gateway_id = inner_res.gateway_id;

                info!(
                    event = "SIP_ROUTE_DECISION",
                    sip.call_id = %call_id,
                    sip.method = %method,
                    route.target = %next_hop_uri,
                    route.gateway = %gateway_id,
                    "🗺️ Yönlendirme kararı verildi"
                );

                // [ARCH-COMPLIANCE] Motorun DNS çözümlemesi çağrısı güncellendi
                let target_addr = if gateway_id == "internal-p2p" || gateway_id == "direct-route-in-dialog" {
                    packet.uri = next_hop_uri.replace("<", "").replace(">", "");
                    self._router.get_client_source(&call_id).await.or(Some(src_addr))
                } else if let Some(extracted_socket) = sip_utils::extract_socket_addr(&next_hop_uri) {
                    Some(extracted_socket)
                } else {
                    // [ARCH-COMPLIANCE] call_id argümanı eklendi
                    match self.state.resolve_addr(&next_hop_uri, &call_id).await {
                        Ok(addr) => Some(addr),
                        Err(e) => {
                            error!(event="DNS_FAIL", sip.call_id=%call_id, target=%next_hop_uri, error=%e, "Hedef çözümlenemedi");
                            None
                        }
                    }
                };

                if let Some(target) = target_addr {
                    let real_src = packet.get_header_value(HeaderName::Via)
                        .and_then(|v| SipRouter::resolve_response_target(v, 5060))
                        .unwrap_or(src_addr);
                    let real_dst = sip_utils::extract_socket_addr(&next_hop_uri).unwrap_or(target);
                    
                    self._router.register_call_route(&call_id, real_src, real_dst).await;

                    if packet.method == Method::Invite {
                        SipRouter::add_record_route(packet, &self.config.public_ip, 5060);
                    }
                    SipRouter::add_via(packet, &self.config.proxy_advertised_host, self.config.sip_port, "UDP");
                    return Some((packet.clone(), Some(target)));
                } else {
                    return Some((SipResponseFactory::create_error(packet, 404, "Not Found"), Some(src_addr)));
                }
            },
            Err(e) => {
                error!(event="ROUTING_LOGIC_ERROR", sip.call_id=%call_id, error=%e, "Yönlendirme mantığı hatası");
                return Some((SipResponseFactory::create_error(packet, 503, "Service Unavailable"), Some(src_addr)));
            }
        }
    }

    async fn handle_register(&self, packet: &mut SipPacket, src_addr: SocketAddr) -> Option<(SipPacket, Option<SocketAddr>)> {
        let call_id = packet.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
        let to_uri = packet.get_header_value(HeaderName::To).cloned().unwrap_or_default();
        let clean_to_uri = sip_utils::extract_aor(&to_uri); 
        let contact_header = packet.get_header_value(HeaderName::Contact).cloned().unwrap_or_default();
        
        let mut expires = 3600;
        if let Some(exp_str) = packet.get_header_value(HeaderName::Other("Expires".to_string())) {
            expires = exp_str.parse().unwrap_or(3600);
        } else if let Some(idx) = contact_header.find("expires=") {
            let sub = &contact_header[idx + 8..];
            let end_idx = sub.find(';').unwrap_or(sub.len());
            expires = sub[..end_idx].parse().unwrap_or(3600);
        }

        let mut actual_contact_uri = contact_header.clone();
        if let Some(via) = packet.get_header_value(HeaderName::Via) {
            if via.contains("received=") || via.contains("rport=") {
                let mut ip = String::new();
                let mut port = 5060;
                for param in via.split(';') {
                    let p_trim = param.trim();
                    if p_trim.starts_with("received=") { ip = p_trim[9..].to_string(); } 
                    else if p_trim.starts_with("rport=") { if let Ok(p) = p_trim[6..].parse::<u16>() { port = p; } }
                }
                if !ip.is_empty() {
                    let username = sip_utils::extract_username_from_uri(&contact_header); 
                    actual_contact_uri = format!("<sip:{}@{}:{}>", username, ip, port);
                }
            }
        }

        let auth_header = packet.get_header_value(HeaderName::Other("Authorization".to_string()));
        let mut is_authenticated = false;
        
        if let Some(auth_val) = auth_header {
            if let Some(digest) = crate::sip::auth::DigestAuth::parse(auth_val) {
                
                let clients_guard = self.routing_logic.clients.lock().await;
                if let Some(clients) = clients_guard.as_ref() {
                    let mut user_client = clients.user.clone();
                    drop(clients_guard);

                    let mut req = tonic::Request::new(sentiric_contracts::sentiric::user::v1::GetSipCredentialsRequest {
                        sip_username: digest.username.clone(),
                        realm: digest.realm.clone(),
                    });

                    //[ARCH-COMPLIANCE] Kesin Timeout ZORUNLULUĞU
                    req.set_timeout(std::time::Duration::from_secs(2));

                    if !call_id.is_empty() {
                        if let Ok(meta_val) = tonic::metadata::MetadataValue::try_from(call_id.as_str()) {
                            req.metadata_mut().insert("x-trace-id", meta_val);
                        }
                    }

                    match user_client.get_sip_credentials(req).await {
                        Ok(res) => {
                            let ha1 = res.into_inner().ha1_hash;
                            if digest.verify(&ha1, "REGISTER") {
                                is_authenticated = true;
                                info!(event="SIP_AUTH_SUCCESS", sip.call_id=%call_id, user=%digest.username, "✅ SIP Digest doğrulaması başarılı.");
                            } else {
                                warn!(event="SIP_AUTH_FAIL", sip.call_id=%call_id, user=%digest.username, "❌ SIP Digest doğrulaması BAŞARISIZ (Yanlış şifre).");
                            }
                        }
                        Err(e) => warn!(event="SIP_AUTH_USER_FAIL", sip.call_id=%call_id, user=%digest.username, error=%e, "❌ Kullanıcı veritabanında bulunamadı."),
                    }
                }
            }
        }

        if !is_authenticated {
            let nonce = uuid::Uuid::new_v4().to_string().replace("-", "");
            let mut resp = SipResponseFactory::create_error(packet, 401, "Unauthorized");
            let www_auth = format!("Digest realm=\"{}\", nonce=\"{}\", algorithm=MD5", self.config.sip_realm, nonce);
            resp.headers.push(Header::new(HeaderName::Other("WWW-Authenticate".to_string()), www_auth));
            
            info!(event="SIP_AUTH_CHALLENGE", sip.call_id=%call_id, "🔒 401 Unauthorized gönderiliyor (Challenge).");
            return Some((resp, Some(src_addr)));
        }

        let clients_guard = self.routing_logic.clients.lock().await;
        if let Some(clients) = clients_guard.as_ref() {
            let mut reg_client = clients.registrar.clone();
            drop(clients_guard);

            let mut req = tonic::Request::new(sentiric_contracts::sentiric::sip::v1::RegisterRequest {
                sip_uri: clean_to_uri.clone(),
                contact_uri: actual_contact_uri.clone(),
                expires,
            });
            
            // [ARCH-COMPLIANCE] Kesin Timeout ZORUNLULUĞU
            req.set_timeout(std::time::Duration::from_secs(2));
            
            if !call_id.is_empty() {
                if let Ok(meta_val) = tonic::metadata::MetadataValue::try_from(call_id.as_str()) {
                    req.metadata_mut().insert("x-trace-id", meta_val);
                }
            }

            match reg_client.register(req).await {
                Ok(_) => {
                    info!(event="SIP_REGISTER_SUCCESS", sip.call_id=%call_id, sip.uri=%clean_to_uri, "✅ Kullanıcı Registrar'a kaydedildi.");
                    let mut ok_resp = SipResponseFactory::create_200_ok(packet);
                    
                    ok_resp.headers.push(Header::new(HeaderName::Contact, actual_contact_uri));
                    if expires > 0 {
                        ok_resp.headers.push(Header::new(HeaderName::Other("Expires".to_string()), expires.to_string()));
                    }
                    
                    return Some((ok_resp, Some(src_addr)));
                }
                Err(e) => {
                    warn!(event="SIP_REGISTER_FAIL", sip.call_id=%call_id, error=%e, "❌ Registrar reddetti.");
                    return Some((SipResponseFactory::create_error(packet, 403, "Forbidden"), Some(src_addr)));
                }
            }
        }
        None
    }

    async fn handle_response(&self, packet: &mut SipPacket) -> Option<(SipPacket, Option<SocketAddr>)> {
        if SipRouter::strip_top_via(packet).is_none() { return None; }
        
        let call_id = packet.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();

        if let Some(next_via) = packet.headers.iter().find(|h| h.name == HeaderName::Via) {
            if let Some(target) = SipRouter::resolve_response_target(&next_via.value, crate::sip::server::DEFAULT_SIP_PORT) {
                debug!(
                    event = "SIP_RESPONSE_ROUTED",
                    sip.call_id = %call_id,
                    target = %target,
                    "🔙 Yanıt Via üzerinden geri gönderiliyor"
                );
                return Some((packet.clone(), Some(target)));
            }
        }
        None
    }
}