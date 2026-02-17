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
use tracing::{error, info, debug, instrument}; // [FIX]: debug eklendi
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
        if packet.is_request() {
            // [KRİTİK DÜZELTME]: Diyalog içi (ACK, BYE, CANCEL) paketlerde döngü kontrolünü esnet.
            let is_in_dialog = packet.is_in_dialog_request();
            
            // 1. Döngü Tespiti (Sadece yeni istekler - INVITE, REGISTER - için sıkı denetim)
            if !is_in_dialog && SipRouter::detect_loop(packet, &self.config.proxy_advertised_host, self.config.sip_port) {
                info!("🔄 Döngü tespit edildi, paket durduruldu.");
                return Some((SipPacket::create_response_for(packet, 482, "Döngü Tespit Edildi".into()), Some(src_addr)));
            }
            
            // 2. Max-Forwards Kontrolü
            if SipRouter::decrement_max_forwards(packet).is_err() {
                info!("🛑 Maksimum atlama sınırına ulaşıldı.");
                return Some((SipPacket::create_response_for(packet, 483, "Çok Fazla Atlama".into()), Some(src_addr)));
            }

            // 3. NAT Düzeltmesi
            SipRouter::fix_nat_via(packet, src_addr);

            // 4. Transaction Yönetimi (Retransmission engelleme)
            if packet.method != Method::Ack {
                let call_id = packet.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
                let tx_key = format!("{}:{:?}", call_id, packet.method);
                
                let action = if let Some(tx) = self.transactions.get(&tx_key) {
                    TransactionEngine::check(&Some(tx.clone()), packet)
                } else {
                    if let Some(new_tx) = SipTransaction::new(packet) { self.transactions.insert(tx_key, new_tx); }
                    TransactionAction::ForwardToApp
                };

                match action {
                    TransactionAction::Retransmit(cached_resp) => return Some((cached_resp, Some(src_addr))),
                    TransactionAction::Ignore => return None,
                    TransactionAction::ForwardToApp => {}
                }
            }

            // [YENİ MANTIK]: Eğer Route başlığı bizi gösteriyorsa, onu temizle (Consume Route)
            packet.headers.retain(|h| {
                if h.name == HeaderName::Route {
                    // Eğer Route başlığı bizi veya SBC'yi işaret ediyorsa, bu başlığı tüketiyoruz.
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

        let request = tonic::Request::new(
            sentiric_contracts::sentiric::sip::v1::GetNextHopRequest {
                destination_uri: dest_uri,
                source_ip: src_addr.ip().to_string(),
                method: packet.method.as_str().to_string(),
                from_uri,
                is_in_dialog: in_dialog,
            }
        );

        match self.routing_logic.get_next_hop(request).await {
            Ok(res) => {
                let inner_res = res.into_inner();
                let next_hop_uri = inner_res.uri; 

                // [UYUM]: SIP URI'den temiz host:port ayıklama
                let target_addr = if let Some(extracted_socket) = sip_core_utils::extract_socket_addr(&next_hop_uri) {
                    Some(extracted_socket)
                } else {
                    match self.state.resolve_addr(&next_hop_uri).await {
                        Ok(addr) => Some(addr),
                        Err(e) => {
                            error!("❌ Hedef çözümlenemedi ({}): {}", next_hop_uri, e);
                            None
                        }
                    }
                };

                if let Some(target) = target_addr {
                    if packet.method == Method::Invite {
                        SipRouter::add_record_route(packet, &self.config.public_ip, 5060);
                    }
                    SipRouter::add_via(packet, &self.config.proxy_advertised_host, self.config.sip_port, "UDP");
                    info!("🚀 Paket yönlendiriliyor ({}): {}", packet.method, target);
                    return Some((packet.clone(), Some(target)));
                } else {
                    error!("🔥 Hedef erişilemez durumda: {}", next_hop_uri);
                    return Some((SipPacket::create_response_for(packet, 404, "Not Found".into()), Some(src_addr)));
                }
            },
            Err(e) => {
                error!("🔥 Yönlendirme mantığı hatası: {}", e);
                return Some((SipPacket::create_response_for(packet, 503, "Hizmet Dışı".into()), Some(src_addr)));
            }
        }
    }

    async fn handle_response(&self, packet: &mut SipPacket) -> Option<(SipPacket, Option<SocketAddr>)> {
        if SipRouter::strip_top_via(packet).is_none() { return None; }
        
        if let Some(next_via) = packet.headers.iter().find(|h| h.name == HeaderName::Via) {
            if let Some(target) = SipRouter::resolve_response_target(&next_via.value, DEFAULT_SIP_PORT) {
                debug!("🔙 Yanıt geri gönderiliyor: {}", target);
                return Some((packet.clone(), Some(target)));
            }
        }
        None
    }
}