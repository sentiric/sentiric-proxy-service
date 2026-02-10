// sentiric-proxy-service/src/sip/engine.rs

use crate::config::AppConfig;
use crate::grpc::client::InternalClients;
use crate::sip::server::{ProxyState, DEFAULT_SIP_PORT};
use crate::sip::handlers::routing::{RoutingHandler, RedisConn};
use sentiric_contracts::sentiric::sip::v1::RegisterRequest;
use sentiric_sip_core::{
    HeaderName, Method, SipPacket,
    SipRouter,
    TransactionEngine, TransactionAction, SipTransaction,
    utils as sip_core_utils
};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tonic::Request;
use tracing::{info, instrument, warn};
use dashmap::DashMap;

pub type TransactionStore = Arc<DashMap<String, SipTransaction>>;

pub struct ProxyEngine {
    clients: Arc<Mutex<InternalClients>>,
    config: Arc<AppConfig>,
    state: Arc<ProxyState>,
    router: RoutingHandler,
    transactions: TransactionStore,
}

impl ProxyEngine {
    pub fn new(clients: Arc<Mutex<InternalClients>>, config: Arc<AppConfig>, state: Arc<ProxyState>, redis: RedisConn) -> Self {
        Self { 
            clients, 
            config, 
            state, 
            router: RoutingHandler::new(redis),
            transactions: Arc::new(DashMap::new())
        }
    }

    #[instrument(skip(self, packet))]
    pub async fn process_packet(&self, packet: &mut SipPacket, src_addr: SocketAddr) -> Option<(SipPacket, Option<SocketAddr>)> {
        // --- 1. Loop Detection (RFC 3261 Section 16.3) ---
        // Proxy kendi adresini Via headerlarında görürse, döngü var demektir.
        let own_signature = format!("{}:{}", self.config.proxy_advertised_host, self.config.sip_port);
        for via in &packet.headers {
            if via.name == HeaderName::Via && via.value.contains(&own_signature) {
                warn!("🔄 Loop Detected! Packet dropped. Signature found: {}", own_signature);
                return Some((SipPacket::create_response_for(packet, 482, "Loop Detected".into()), Some(src_addr)));
            }
        }

        // --- 2. Max-Forwards Check ---
        // Her hop Max-Forwards değerini 1 azaltmalıdır.
        if packet.is_request {
            let mut mf_val = 70; // Default RFC değeri
            let mut mf_idx = None;

            for (i, h) in packet.headers.iter().enumerate() {
                if h.name == HeaderName::MaxForwards {
                    if let Ok(v) = h.value.parse::<i32>() {
                        mf_val = v;
                        mf_idx = Some(i);
                    }
                    break;
                }
            }

            mf_val -= 1;
            if mf_val <= 0 {
                warn!("🛑 Max-Forwards reached 0. Dropping packet.");
                return Some((SipPacket::create_response_for(packet, 483, "Too Many Hops".into()), Some(src_addr)));
            }

            // Header'ı güncelle
            if let Some(idx) = mf_idx {
                packet.headers[idx].value = mf_val.to_string();
            } else {
                // Eğer header yoksa ekle (opsiyonel ama iyi pratik)
                packet.headers.push(sentiric_sip_core::Header::new(HeaderName::MaxForwards, mf_val.to_string()));
            }
        }

        // --- 3. NAT Traversal & Processing ---
        if packet.is_request {
            SipRouter::fix_nat_via(packet, src_addr);
        }

        if packet.is_request && packet.method != Method::Ack {
            let call_id = packet.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
            let tx_key = format!("{}:{:?}", call_id, packet.method);
            
            let action = if let Some(tx) = self.transactions.get(&tx_key) {
                TransactionEngine::check(&Some(tx.clone()), packet)
            } else {
                if let Some(new_tx) = SipTransaction::new(packet) {
                    self.transactions.insert(tx_key, new_tx);
                }
                TransactionAction::ForwardToApp
            };

            match action {
                TransactionAction::Retransmit(cached_resp) => {
                    info!("🔄 [TX-CORE] Retransmitting for {}", call_id);
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
        // REGISTER İstekleri
        if packet.method == Method::Register {
            if let Some(res) = self.handle_register(packet, src_addr).await {
                self.update_tx_state(packet, &res.0);
                return Some(res);
            }
        }

        // Loose Routing (Route Header)
        if packet.headers.iter().any(|h| h.name == HeaderName::Route) {
            return self.handle_loose_routing(packet).await;
        }

        // Initial INVITE -> B2BUA
        if packet.method == Method::Invite {
            match self.state.resolve_b2bua_addr(&self.config.b2bua_sip_addr).await {
                Ok(target_addr) => {
                    SipRouter::add_record_route(packet, &self.config.proxy_advertised_host, self.config.sip_port);
                    SipRouter::add_via(packet, &self.config.proxy_advertised_host, self.config.sip_port, "UDP");

                    let call_id = packet.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
                    self.router.register_call_route(&call_id, src_addr, target_addr).await;
            
                    return Some((packet.clone(), Some(target_addr)));
                }
                Err(_) => {
                    return Some((SipPacket::create_response_for(packet, 503, "Service Unavailable".into()), Some(src_addr)));
                }
            }
        }
        
        None
    }

    /// [E0599 FIX] REGISTER isteğini Registrar servisine gRPC ile iletir.
    async fn handle_register(&self, packet: &SipPacket, src_addr: SocketAddr) -> Option<(SipPacket, Option<SocketAddr>)> {
        let to_header = packet.get_header_value(HeaderName::To).cloned().unwrap_or_default();
        let aor = sip_core_utils::extract_aor(&to_header);
        let username = sip_core_utils::extract_username_from_uri(&aor);
        
        let via_val = packet.get_header_value(HeaderName::Via).cloned().unwrap_or_default();
        let client_addr = SipRouter::resolve_response_target(&via_val, DEFAULT_SIP_PORT).unwrap_or(src_addr);
        let real_contact_uri = format!("sip:{}@{}:{}", username, client_addr.ip(), client_addr.port());

        let expires = packet.get_header_value(HeaderName::Other("Expires".to_string()))
            .and_then(|v| v.parse::<i32>().ok())
            .unwrap_or(3600);

        let mut clients = self.clients.lock().await;
        let req = Request::new(RegisterRequest { 
            sip_uri: aor.clone(), 
            contact_uri: real_contact_uri, 
            expires 
        });

        match clients.registrar.register(req).await {
            Ok(_) => {
                let resp = SipPacket::create_response_for(packet, 200, "OK".into());
                Some((resp, Some(src_addr)))
            },
            Err(_) => {
                let resp = SipPacket::create_response_for(packet, 500, "Registrar Error".into());
                Some((resp, Some(src_addr)))
            }
        }
    }

    async fn handle_loose_routing(&self, packet: &mut SipPacket) -> Option<(SipPacket, Option<SocketAddr>)> {
        if let Some(route_header) = packet.headers.iter().find(|h| h.name == HeaderName::Route).cloned() {
             if let Some(target_addr) = sip_core_utils::extract_socket_addr(&route_header.value) {
                 packet.headers.retain(|h| h.name != HeaderName::Route);
                 SipRouter::add_via(packet, &self.config.proxy_advertised_host, self.config.sip_port, "UDP");
                 return Some((packet.clone(), Some(target_addr)));
             }
        }
        None
    }

    fn update_tx_state(&self, req: &SipPacket, resp: &SipPacket) {
        let call_id = req.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
        let tx_key = format!("{}:{:?}", call_id, req.method);
        if let Some(mut tx) = self.transactions.get_mut(&tx_key) {
            tx.update_with_response(resp);
        }
    }

    async fn handle_response(&self, packet: &mut SipPacket) -> Option<(SipPacket, Option<SocketAddr>)> {
        if SipRouter::strip_top_via(packet).is_none() { return None; }
        
        if let Some(next_via) = packet.headers.iter().find(|h| h.name == HeaderName::Via) {
            if let Some(target) = SipRouter::resolve_response_target(&next_via.value, DEFAULT_SIP_PORT) {
                return Some((packet.clone(), Some(target)));
            }
        }
        None
    }
}