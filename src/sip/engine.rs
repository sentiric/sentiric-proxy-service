// sentiric-proxy-service/src/sip/engine.rs

use crate::config::AppConfig;
use crate::grpc::client::InternalClients;
use crate::sip::server::{ProxyState, DEFAULT_SIP_PORT};
use crate::sip::handlers::routing::{RoutingHandler, RedisConn};
use crate::sip::utils::get_header; // Yardımcı fonksiyon

use sentiric_sip_core::{
    HeaderName, Method, SipPacket,
    SipRouter,
    TransactionEngine, TransactionAction, SipTransaction,
    utils as sip_core_utils
};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, instrument, warn, debug, error};
use dashmap::DashMap;

pub type TransactionStore = Arc<DashMap<String, SipTransaction>>;

pub struct ProxyEngine {
    clients: Arc<Mutex<InternalClients>>,
    config: Arc<AppConfig>,
    // Bu alanlar şu an aktif kullanılmıyor ama Dependency Injection 
    // yapısını bozmamak ve ileride kullanmak üzere '_' ile saklıyoruz.
    _state: Arc<ProxyState>, 
    _router: RoutingHandler, 
    transactions: TransactionStore,
}

impl ProxyEngine {
    pub fn new(clients: Arc<Mutex<InternalClients>>, config: Arc<AppConfig>, state: Arc<ProxyState>, redis: RedisConn) -> Self {
        Self { 
            clients, 
            config, 
            _state: state, 
            _router: RoutingHandler::new(redis),
            transactions: Arc::new(DashMap::new())
        }
    }

    #[instrument(skip(self, packet))]
    pub async fn process_packet(&self, packet: &mut SipPacket, src_addr: SocketAddr) -> Option<(SipPacket, Option<SocketAddr>)> {
        // 1. Döngü Tespiti
        if SipRouter::detect_loop(packet, &self.config.proxy_advertised_host, self.config.sip_port) {
            warn!("🔄 Loop Detected! Packet dropped.");
            return Some((SipPacket::create_response_for(packet, 482, "Loop Detected".into()), Some(src_addr)));
        }
        
        // 2. Max-Forwards Kontrolü
        if SipRouter::decrement_max_forwards(packet).is_err() {
            warn!("🛑 Max-Forwards reached 0. Dropping packet.");
            return Some((SipPacket::create_response_for(packet, 483, "Too Many Hops".into()), Some(src_addr)));
        }
        
        // 3. NAT Düzeltmesi (Sadece istekler için)
        if packet.is_request() {
            SipRouter::fix_nat_via(packet, src_addr);
        }

        // 4. Transaction Yönetimi (Retransmission engelleme)
        if packet.is_request() && packet.method != Method::Ack {
            let call_id = packet.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
            let tx_key = format!("{}:{:?}", call_id, packet.method);
            
            let action = if let Some(tx) = self.transactions.get(&tx_key) {
                TransactionEngine::check(&Some(tx.clone()), packet)
            } else {
                if let Some(new_tx) = SipTransaction::new(packet) { self.transactions.insert(tx_key, new_tx); }
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

        // 5. İşleme
        if packet.is_request() {
            self.handle_request(packet, src_addr).await
        } else {
            self.handle_response(packet).await
        }
    }

    async fn handle_request(&self, packet: &mut SipPacket, src_addr: SocketAddr) -> Option<(SipPacket, Option<SocketAddr>)> {
        let from_uri = get_header(packet, HeaderName::From);
        let dest_uri = packet.uri.clone();
        
        // Diyalog içi bir istek mi? (ACK, BYE vb.)
        let in_dialog = packet.is_in_dialog_request();

        // Kararı gRPC servisine sor (Loopback)
        let request = tonic::Request::new(
            sentiric_contracts::sentiric::sip::v1::GetNextHopRequest {
                destination_uri: dest_uri,
                source_ip: src_addr.ip().to_string(),
                method: packet.method.as_str().to_string(),
                from_uri,
                is_in_dialog: in_dialog,
            }
        );

        let mut grpc_clients = self.clients.lock().await;
        
        match grpc_clients.proxy.get_next_hop(request).await {
            Ok(res) => {
                let inner_res = res.into_inner();
                let next_hop_uri = inner_res.uri; 
                debug!("🎯 Next Hop Resolved: {} (GW: {})", next_hop_uri, inner_res.gateway_id);

                if let Some(target_addr) = sip_core_utils::extract_socket_addr(&next_hop_uri) {
                    // İlk isteklerde Record-Route ekle ki yolumuz belli olsun
                    if packet.method == Method::Invite {
                        SipRouter::add_record_route(packet, &self.config.proxy_advertised_host, self.config.sip_port);
                    }
                    SipRouter::add_via(packet, &self.config.proxy_advertised_host, self.config.sip_port, "UDP");
                    return Some((packet.clone(), Some(target_addr)));
                } else {
                    warn!("Could not parse next hop URI to SocketAddr: {}", next_hop_uri);
                    return Some((SipPacket::create_response_for(packet, 404, "Not Found".into()), Some(src_addr)));
                }
            },
            Err(e) => {
                error!("🔥 Routing Logic Failed (gRPC): {}", e);
                return Some((SipPacket::create_response_for(packet, 503, "Service Unavailable".into()), Some(src_addr)));
            }
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