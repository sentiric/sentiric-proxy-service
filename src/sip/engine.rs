// sentiric-proxy-service/src/sip/engine.rs

use crate::config::AppConfig;
use crate::sip::server::{ProxyState, DEFAULT_SIP_PORT};
use crate::sip::handlers::routing::{RoutingHandler, RedisConn};
use crate::grpc::service::MyProxyService;

use sentiric_sip_core::{
    HeaderName, Method, SipPacket,
    SipRouter,
    TransactionEngine, TransactionAction, SipTransaction,
    utils as sip_core_utils
};
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{error, info, debug, instrument, warn};
use dashmap::DashMap;
use std::str::FromStr;
// [KRİTİK DÜZELTME]: Eksik olan trait import edildi. E0599 ve E0282 hatalarını bu çözer.
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
                return Some((SipPacket::create_response_for(packet, 482, "Loop Detected".into()), Some(src_addr)));
            }
            
            if SipRouter::decrement_max_forwards(packet).is_err() {
                warn!(event="SIP_MAX_FORWARDS", sip.call_id=%call_id, "🛑 Maksimum atlama sınırına ulaşıldı.");
                return Some((SipPacket::create_response_for(packet, 483, "Too Many Hops".into()), Some(src_addr)));
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
        let from_uri = packet.get_header_value(HeaderName::From).cloned().unwrap_or_default();
        let dest_uri = packet.uri.clone();
        let in_dialog = packet.is_in_dialog_request();
        let call_id = packet.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();

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
                    return Some((SipPacket::create_response_for(packet, 404, "Not Found".into()), Some(src_addr)));
                }
            },
            Err(e) => {
                error!(event="ROUTING_LOGIC_ERROR", sip.call_id=%call_id, error=%e, "Yönlendirme mantığı hatası");
                return Some((SipPacket::create_response_for(packet, 503, "Service Unavailable".into()), Some(src_addr)));
            }
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