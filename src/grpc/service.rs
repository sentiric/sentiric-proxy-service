// src/grpc/service.rs

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
use tracing::{error, instrument, debug, info, Span}; 
use std::sync::Arc;
use tokio::sync::Mutex;
use std::time::{Instant, Duration};
use dashmap::DashMap;
use crate::config::AppConfig;
use crate::grpc::client::InternalClients;

type DialplanCache = DashMap<String, (ResolveDialplanResponse, Instant)>;

pub struct MyProxyService {
    config: Arc<AppConfig>,
    clients: Arc<Mutex<Option<InternalClients>>>,
    cache: DialplanCache,
}

impl MyProxyService {
    pub fn new(config: Arc<AppConfig>, clients: Arc<Mutex<Option<InternalClients>>>) -> Self {
        Self { config, clients, cache: DashMap::new() }
    }
}

#[tonic::async_trait]
impl ProxyService for MyProxyService {
    
    #[instrument(skip_all, fields(sip.call_id, trace_id, to_uri = %request.get_ref().destination_uri, method = %request.get_ref().method))]
    async fn get_next_hop(&self, request: Request<GetNextHopRequest>) -> Result<Response<GetNextHopResponse>, Status> {
        
        let trace_id = request.metadata().get("x-trace-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("unknown")
            .to_string();
        
        Span::current().record("trace_id", &trace_id);
        Span::current().record("sip.call_id", &trace_id);

        let req = request.into_inner();
        let destination_user = sip_utils::extract_username_from_uri(&req.destination_uri).to_lowercase();
        let caller_id = sip_utils::extract_username_from_uri(&req.from_uri);

        // 1. Dahili Servis Kullanıcıları (Direkt Rota)
        if self.config.internal_service_users.contains(&destination_user) {
            info!(
                event="ROUTE_INTERNAL_USER", 
                user=%destination_user, 
                "Dahili servis kullanıcısı tespit edildi."
            );
            return Ok(Response::new(GetNextHopResponse {
                uri: self.config.b2bua_sip_addr.clone(),
                gateway_id: format!("core-internal-{}", destination_user),
            }));
        }
        
        // 2. In-Dialog İstekler (ACK, BYE - Direkt Rota)
        if req.is_in_dialog && req.method != "CANCEL" {
            let mut target_uri = req.destination_uri.replace('<', "").replace('>', "");
            
            // [CRITICAL FIX]: Eğer In-Dialog paketi kendi Public IP'mize geliyorsa (SBC'yi bypass eden hatalı softphone)
            // sonsuz döngüye girmemek için doğrudan B2BUA'ya yönlendir.
            if target_uri.contains(&self.config.public_ip) {
                debug!(event="ACK_LOOP_PREVENTED", "Public IP detected in dialog route, redirecting to B2BUA internal address.");
                target_uri = self.config.b2bua_sip_addr.clone();
            }

            debug!(event="ROUTE_IN_DIALOG", "Diyalog içi yönlendirme yapılıyor.");
            return Ok(Response::new(GetNextHopResponse { uri: target_uri, gateway_id: "direct-route-in-dialog".to_string() }));
        }

        // 3. Register İstekleri (Registrar'a)
        if req.method == "REGISTER" {
            return Ok(Response::new(GetNextHopResponse { uri: self.config.registrar_sip_addr.clone(), gateway_id: "registrar-local".to_string() }));
        }
        
        // 4. Cache Kontrolü
        let cache_key = format!("{}:{}", caller_id, destination_user);
        if let Some(cached) = self.cache.get(&cache_key) {
            let (res, ts) = cached.value();
            if ts.elapsed() < Duration::from_secs(300) { 
                info!(
                    event = "DIALPLAN_CACHE_HIT", 
                    cache.key = %cache_key, 
                    "⚡ Dialplan önbellekten getirildi"
                );
                return self.resolve_action(res, &req.destination_uri, &trace_id).await;
            }
        }

        // 5. Dialplan Sorgusu (Hata Toleranslı)
        let clients_guard = self.clients.lock().await;
        let clients = clients_guard.as_ref().ok_or_else(|| Status::unavailable("Proxy starting..."))?;
        let mut dialplan_client = clients.dialplan.clone();
        drop(clients_guard);

        let mut dialplan_req = Request::new(ResolveDialplanRequest {
            caller_contact_value: caller_id.clone(),
            destination_number: destination_user.clone(),
        });
        
        if trace_id != "unknown" {
             let _ = dialplan_req.metadata_mut().insert("x-trace-id", trace_id.parse().unwrap());
        }

        match dialplan_client.resolve_dialplan(dialplan_req).await {
            Ok(res) => {
                let resolution = res.into_inner();
                info!(
                    event = "DIALPLAN_CACHE_MISS", 
                    cache.key = %cache_key, 
                    "Dialplan servisinden yeni rota öğrenildi"
                );
                self.cache.insert(cache_key, (resolution.clone(), Instant::now()));
                self.resolve_action(&resolution, &req.destination_uri, &trace_id).await
            },
            Err(e) => {
                error!(event="DIALPLAN_ERROR", error=%e, "Dialplan servisine erişilemedi. Fail-Safe modu devreye girdi.");
                Ok(Response::new(GetNextHopResponse { 
                    uri: self.config.b2bua_sip_addr.clone(), 
                    gateway_id: "failsafe-proxy-fallback".to_string() 
                }))
            }
        }
    }
}

impl MyProxyService {
    async fn resolve_action(&self, resolution: &ResolveDialplanResponse, dest_uri: &str, trace_id: &str) -> Result<Response<GetNextHopResponse>, Status> {
        let action = resolution.action.as_ref().ok_or_else(|| Status::internal("Action missing"))?;
        let action_type = ActionType::try_from(action.r#type).unwrap_or(ActionType::Unspecified);

        match action_type {
            ActionType::BridgeCall => {
                let clients_guard = self.clients.lock().await;
                let mut registrar_client = clients_guard.as_ref().unwrap().registrar.clone();
                drop(clients_guard);

                let mut lookup_req = Request::new(LookupContactRequest { sip_uri: dest_uri.to_string() });
                if trace_id != "unknown" {
                     let _ = lookup_req.metadata_mut().insert("x-trace-id", trace_id.parse().unwrap());
                }

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