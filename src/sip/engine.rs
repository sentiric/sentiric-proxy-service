// Dosya: sentiric-sip-proxy-service/src/sip/engine.rs
use crate::config::AppConfig;
use crate::sip::server::{ProxyState, DEFAULT_SIP_PORT};
use crate::sip::handlers::routing::{RoutingHandler, RedisConn};
use crate::grpc::service::MyProxyService;

use sentiric_sip_core::{
    Header, HeaderName, Method, SipPacket,
    SipRouter,
    TransactionEngine, TransactionAction, SipTransaction,
    utils as sip_core_utils,
    builder::SipResponseFactory
};
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{error, info, debug, instrument, warn};
use dashmap::DashMap;
use std::str::FromStr;
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
                let tx_key = format!("{}:{:?}", call_id, packet.method);
                let action = if let Some(tx) = self.transactions.get(&tx_key) {
                    TransactionEngine::check(&Some(tx.clone()), packet)
                } else {
                    if let Some(new_tx) = SipTransaction::new(packet) { self.transactions.insert(tx_key, new_tx); }
                    TransactionAction::ForwardToApp
                };

                match action {
                    TransactionAction::Retransmit(cached_resp) => {
                        debug!(event="SIP_RETRANSMIT", sip.call_id=%call_id, "Tekrar eden paket, cache'den yanıtlanıyor.");
                        return Some((cached_resp, Some(src_addr)));
                    },
                    TransactionAction::Ignore => return None,
                    TransactionAction::ForwardToApp => {}
                }
            }

            packet.headers.retain(|h| {
                if h.name == HeaderName::Route {
                    !h.value.contains(&self.config.proxy_advertised_host) && 
                    !h.value.contains(&self.config.public_ip)
                } else {
                    true
                }
            });

            self.handle_request(packet, src_addr).await
        } else {
            self.handle_response(packet).await
        }
    }

    async fn handle_request(&self, packet: &mut SipPacket, src_addr: SocketAddr) -> Option<(SipPacket, Option<SocketAddr>)> {
        // [MİMARİ DÜZELTME]: REGISTER Paketleri gRPC ile Registrar'a iletilir ve döngü (Loop) kırılır.
        if packet.method == Method::Register {
            return self.handle_register(packet, src_addr).await;
        }

        let from_uri = packet.get_header_value(HeaderName::From).cloned().unwrap_or_default();
        let dest_uri = packet.uri.clone();
        let in_dialog = packet.is_in_dialog_request();
        let call_id = packet.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();

        let is_from_b2bua = if let Ok(mut addrs) = tokio::net::lookup_host(&self.config.b2bua_sip_addr).await {
            addrs.any(|a| a.ip() == src_addr.ip())
        } else {
            false
        };

        if in_dialog && packet.method != Method::Cancel && is_from_b2bua {
            if let Some(sbc_addr) = self._router.get_client_source(&call_id).await {
                info!(event="SIP_OUTBOUND_IN_DIALOG", sip.call_id=%call_id, target=%sbc_addr, "Yönlendirme SBC üzerinden dışarı yapılıyor");
                SipRouter::add_via(packet, &self.config.proxy_advertised_host, self.config.sip_port, "UDP");
                return Some((packet.clone(), Some(sbc_addr)));
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

        if !call_id.is_empty() {
             if let Ok(meta_val) = tonic::metadata::MetadataValue::from_str(&call_id) {
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
                    sip.method = %packet.method.as_str(),
                    route.target = %next_hop_uri,
                    route.gateway = %gateway_id,
                    "🗺️ Yönlendirme kararı verildi"
                );

                let target_addr = if let Some(extracted_socket) = sip_core_utils::extract_socket_addr(&next_hop_uri) {
                    Some(extracted_socket)
                } else {
                    match self.state.resolve_addr(&next_hop_uri).await {
                        Ok(addr) => Some(addr),
                        Err(e) => {
                            error!(event="DNS_FAIL", sip.call_id=%call_id, target=%next_hop_uri, error=%e, "Hedef çözümlenemedi");
                            None
                        }
                    }
                };

                if let Some(target) = target_addr {
                    self._router.register_call_route(&call_id, src_addr, target).await;

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

    // [MİMARİ DÜZELTME]: Akıllı Register Yönlendirmesi
    async fn handle_register(&self, packet: &mut SipPacket, src_addr: SocketAddr) -> Option<(SipPacket, Option<SocketAddr>)> {
        let call_id = packet.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
        let to_uri = packet.get_header_value(HeaderName::To).cloned().unwrap_or_default();
        let clean_to_uri = sip_core_utils::extract_aor(&to_uri);
        
        let contact_header = packet.get_header_value(HeaderName::Contact).cloned().unwrap_or_default();
        
        let mut expires = 3600;
        if let Some(exp_str) = packet.get_header_value(HeaderName::Other("Expires".to_string())) {
            expires = exp_str.parse().unwrap_or(3600);
        } else if let Some(idx) = contact_header.find("expires=") {
            let sub = &contact_header[idx + 8..];
            let end_idx = sub.find(';').unwrap_or(sub.len());
            expires = sub[..end_idx].parse().unwrap_or(3600);
        }

        // [MİMARİ DÜZELTME]: Symmetric Latching - NAT arkasındaki cihazların IP'lerini SBC'den gelen gerçek IP ile değiştir.
        let mut actual_contact_uri = contact_header.clone();
        if let Some(via) = packet.get_header_value(HeaderName::Via) {
            if via.contains("received=") || via.contains("rport=") {
                let mut ip = String::new();
                let mut port = 5060;
                for param in via.split(';') {
                    let p_trim = param.trim();
                    if p_trim.starts_with("received=") {
                        ip = p_trim[9..].to_string();
                    } else if p_trim.starts_with("rport=") {
                        if let Ok(p) = p_trim[6..].parse::<u16>() { port = p; }
                    }
                }
                if !ip.is_empty() {
                    let username = sip_core_utils::extract_username_from_uri(&contact_header);
                    actual_contact_uri = format!("<sip:{}@{}:{}>", username, ip, port);
                    info!(event="SIP_NAT_CONTACT_FIX", sip.call_id=%call_id, old_contact=%contact_header, new_contact=%actual_contact_uri, "NAT arkası cihazın Contact adresi düzeltildi.");
                }
            }
        }

        info!(event="SIP_REGISTER_ATTEMPT", sip.call_id=%call_id, sip.uri=%clean_to_uri, "Kayıt (REGISTER) isteği alındı, Registrar servisine gRPC ile iletiliyor.");

        let clients_guard = self.routing_logic.clients.lock().await;
        if let Some(clients) = clients_guard.as_ref() {
            let mut reg_client = clients.registrar.clone();
            drop(clients_guard);

            let mut req = tonic::Request::new(sentiric_contracts::sentiric::sip::v1::RegisterRequest {
                sip_uri: clean_to_uri.clone(),
                contact_uri: actual_contact_uri.clone(),
                expires,
            });
            
            if !call_id.is_empty() {
                if let Ok(meta_val) = tonic::metadata::MetadataValue::try_from(call_id.as_str()) {
                    req.metadata_mut().insert("x-trace-id", meta_val);
                }
            }

            match reg_client.register(req).await {
                Ok(_) => {
                    info!(event="SIP_REGISTER_SUCCESS", sip.call_id=%call_id, sip.uri=%clean_to_uri, "✅ Kullanıcı başarıyla kaydedildi.");
                    let mut ok_resp = SipResponseFactory::create_200_ok(packet);
                    
                    ok_resp.headers.push(Header::new(HeaderName::Contact, actual_contact_uri));
                    if expires > 0 {
                        ok_resp.headers.push(Header::new(HeaderName::Other("Expires".to_string()), expires.to_string()));
                    }
                    
                    return Some((ok_resp, Some(src_addr)));
                }
                Err(e) => {
                    warn!(event="SIP_REGISTER_FAIL", sip.call_id=%call_id, error=%e, "❌ Kayıt reddedildi.");
                    return Some((SipResponseFactory::create_error(packet, 403, "Forbidden"), Some(src_addr)));
                }
            }
        } else {
            error!(event="SIP_REGISTER_ERROR", sip.call_id=%call_id, "Registrar Client bulunamadı!");
            return Some((SipResponseFactory::create_error(packet, 500, "Internal Server Error"), Some(src_addr)));
        }
    }

    async fn handle_response(&self, packet: &mut SipPacket) -> Option<(SipPacket, Option<SocketAddr>)> {
        if SipRouter::strip_top_via(packet).is_none() { return None; }
        
        let call_id = packet.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();

        if let Some(sbc_addr) = self._router.get_client_source(&call_id).await {
            debug!(
                event = "SIP_RESPONSE_ROUTED_SYMMETRIC",
                sip.call_id = %call_id,
                target = %sbc_addr,
                "🔙 Yanıt Redis rotası üzerinden güvenli bir şekilde SBC'ye gönderiliyor"
            );
            return Some((packet.clone(), Some(sbc_addr)));
        }

        if let Some(next_via) = packet.headers.iter().find(|h| h.name == HeaderName::Via) {
            if let Some(target) = SipRouter::resolve_response_target(&next_via.value, DEFAULT_SIP_PORT) {
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