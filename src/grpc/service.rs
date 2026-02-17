// sentiric-proxy-service/src/grpc/service.rs

use sentiric_contracts::sentiric::sip::v1::{
    proxy_service_server::ProxyService,
    GetNextHopRequest, GetNextHopResponse,
    LookupContactRequest,
};
use sentiric_contracts::sentiric::dialplan::v1::{
    ResolveDialplanRequest, ResolveDialplanResponse, ActionType
};
use sentiric_sip_core::utils as sip_utils;

use tonic::{Request, Response, Status};
use tracing::{error, instrument, debug};
use std::sync::Arc;
use tokio::sync::Mutex;
use std::time::{Instant, Duration};
use dashmap::DashMap;
use crate::config::AppConfig;
use crate::grpc::client::InternalClients;

// Önbellek Yapısı: (Arayan+Aranan) -> (Sonuç, Oluşturulma Zamanı)
type DialplanCache = DashMap<String, (ResolveDialplanResponse, Instant)>;

pub struct MyProxyService {
    config: Arc<AppConfig>,
    clients: Arc<Mutex<Option<InternalClients>>>,
    cache: DialplanCache, // L1 Cache
}

impl MyProxyService {
    pub fn new(config: Arc<AppConfig>, clients: Arc<Mutex<Option<InternalClients>>>) -> Self {
        Self { 
            config, 
            clients,
            cache: DashMap::new(),
        }
    }
}

#[tonic::async_trait]
impl ProxyService for MyProxyService {
    
    #[instrument(skip_all, fields(
        hedef = %request.get_ref().destination_uri, 
        metod = %request.get_ref().method
    ))]
    async fn get_next_hop(
        &self,
        request: Request<GetNextHopRequest>,
    ) -> Result<Response<GetNextHopResponse>, Status> {
        let req = request.into_inner();
        let destination_user = sip_utils::extract_username_from_uri(&req.destination_uri).to_lowercase();
        let caller_id = sip_utils::extract_username_from_uri(&req.from_uri);

        // 1. CORE-ROUTING (B2BUA/AI) Hızlı Yol
        if self.config.internal_service_users.contains(&destination_user) {
            return Ok(Response::new(GetNextHopResponse {
                uri: self.config.b2bua_sip_addr.clone(),
                gateway_id: format!("core-internal-{}", destination_user),
            }));
        }
        
        // 2. DIYALOG İÇİ (ACK/BYE) Hızlı Yol
        if req.is_in_dialog {
            let target_uri = req.destination_uri.replace('<', "").replace('>', "");
            return Ok(Response::new(GetNextHopResponse {
                uri: target_uri,
                gateway_id: "direct-route-in-dialog".to_string(),
            }));
        }

        // 3. REGISTER Hızlı Yol
        if req.method == "REGISTER" {
            return Ok(Response::new(GetNextHopResponse {
                uri: self.config.registrar_sip_addr.clone(),
                gateway_id: "registrar-local".to_string(),
            }));
        }
        
        // 4. DIALPLAN CACHE KONTROLÜ (Latency Killer)
        let cache_key = format!("{}:{}", caller_id, destination_user);
        if let Some(cached) = self.cache.get(&cache_key) {
            let (res, ts) = cached.value();
            if ts.elapsed() < Duration::from_secs(300) { // 5 dakika cache
                debug!("⚡ [CACHE-HIT] Dialplan retrieved from cache for {}", cache_key);
                return self.resolve_action(res, &req.destination_uri).await;
            }
        }

        // 5. SLOW PATH (gRPC Zinciri)
        let clients_guard = self.clients.lock().await;
        let clients = clients_guard.as_ref().ok_or_else(|| Status::unavailable("Proxy starting..."))?;
        let mut dialplan_client = clients.dialplan.clone();
        drop(clients_guard);

        let dialplan_req = Request::new(ResolveDialplanRequest {
            caller_contact_value: caller_id.clone(),
            destination_number: destination_user.clone(),
        });

        match dialplan_client.resolve_dialplan(dialplan_req).await {
            Ok(res) => {
                let resolution = res.into_inner();
                // Cache'e yaz
                self.cache.insert(cache_key, (resolution.clone(), Instant::now()));
                self.resolve_action(&resolution, &req.destination_uri).await
            },
            Err(e) => {
                error!("❌ Dialplan Error: {}. Fallback to B2BUA.", e);
                Ok(Response::new(GetNextHopResponse {
                    uri: self.config.b2bua_sip_addr.clone(),
                    gateway_id: "failsafe-b2bua".to_string(),
                }))
            }
        }
    }
}

impl MyProxyService {
    // Aksiyon çözme mantığını gRPC metodundan ayırdık (Refactoring for SOC)
    async fn resolve_action(&self, resolution: &ResolveDialplanResponse, dest_uri: &str) -> Result<Response<GetNextHopResponse>, Status> {
        let action = resolution.action.as_ref().ok_or_else(|| Status::internal("Action missing"))?;
        let action_type = ActionType::try_from(action.r#type).unwrap_or(ActionType::Unspecified);

        match action_type {
            ActionType::BridgeCall => {
                // Registrar sorgusu (Bu da ileride cache'lenebilir)
                let clients_guard = self.clients.lock().await;
                let mut registrar_client = clients_guard.as_ref().unwrap().registrar.clone();
                drop(clients_guard);

                let lookup_req = Request::new(LookupContactRequest { sip_uri: dest_uri.to_string() });
                if let Ok(lookup_res) = registrar_client.lookup_contact(lookup_req).await {
                    if let Some(target) = lookup_res.into_inner().contact_uris.first() {
                        return Ok(Response::new(GetNextHopResponse {
                            uri: target.clone(),
                            gateway_id: "internal-p2p".to_string(),
                        }));
                    }
                }
                Ok(Response::new(GetNextHopResponse {
                    uri: self.config.b2bua_sip_addr.clone(),
                    gateway_id: "b2bua-fallback".to_string(),
                }))
            },
            _ => {
                Ok(Response::new(GetNextHopResponse {
                    uri: self.config.b2bua_sip_addr.clone(),
                    gateway_id: "sentiric-ai-gateway".to_string(),
                }))
            }
        }
    }
}