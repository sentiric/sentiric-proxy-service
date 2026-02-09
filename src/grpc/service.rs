// sentiric-proxy-service/src/grpc/service.rs

use sentiric_contracts::sentiric::sip::v1::{
    proxy_service_server::ProxyService,
    GetNextHopRequest, GetNextHopResponse,
    LookupContactRequest,
};
use sentiric_contracts::sentiric::dialplan::v1::{
    ResolveDialplanRequest, ActionType
};
use sentiric_sip_core::SipUri;
use std::str::FromStr;

use tonic::{Request, Response, Status};
use tracing::{info, error, warn, instrument};
use std::sync::Arc;
use tokio::sync::Mutex;
use crate::config::AppConfig;
use crate::grpc::client::InternalClients;

pub struct MyProxyService {
    config: Arc<AppConfig>,
    clients: Arc<Mutex<InternalClients>>,
}

impl MyProxyService {
    pub fn new(config: Arc<AppConfig>, clients: Arc<Mutex<InternalClients>>) -> Self {
        Self { config, clients }
    }

    fn extract_username(&self, uri_str: &str) -> String {
        match SipUri::from_str(uri_str) {
            Ok(uri) => uri.user.unwrap_or_else(|| "anonymous".to_string()),
            Err(_) => {
                let clean = uri_str.replace('<', "").replace('>', "");
                if let Some(idx) = clean.find('@') {
                    let start = clean.find(':').map(|i| i + 1).unwrap_or(0);
                    clean[start..idx].to_string()
                } else {
                    clean
                }
            }
        }
    }
}

#[tonic::async_trait]
impl ProxyService for MyProxyService {
    
    #[instrument(skip_all, fields(
        dest = %request.get_ref().destination_uri, 
        method = %request.get_ref().method,
        src_ip = %request.get_ref().source_ip
    ))]
    async fn get_next_hop(
        &self,
        request: Request<GetNextHopRequest>,
    ) -> Result<Response<GetNextHopResponse>, Status> {
        let req = request.into_inner();
        let destination_user = self.extract_username(&req.destination_uri);
        
        // Caller ID analizi
        let caller_id = if !req.from_uri.is_empty() {
            self.extract_username(&req.from_uri)
        } else {
            req.source_ip.clone()
        };

        // --- SYSTEM ROUTES (ALTYAPI YÖNLENDİRMELERİ) ---
        // Bu blok, Dialplan'ın "b2bua" -> "2" çevrim hatasını engeller.
        
        // 1. B2BUA Direct Route
        if destination_user == "b2bua" {
            info!("🔄 [ROUTING] System Route: Direct B2BUA targeting (Bypass Dialplan).");
            return Ok(Response::new(GetNextHopResponse {
                uri: self.config.b2bua_sip_addr.clone(),
                gateway_id: "sentiric-b2bua-direct".to_string(),
            }));
        }

        // 2. REGISTER -> Registrar
        if req.method == "REGISTER" {
            return Ok(Response::new(GetNextHopResponse {
                uri: self.config.registrar_sip_addr.clone(),
                gateway_id: "sentiric-registrar-local".to_string(),
            }));
        }

        // --- BUSINESS ROUTES (DIALPLAN - İŞ MANTIĞI) ---
        
        let mut clients = self.clients.lock().await;
        
        let dialplan_req = Request::new(ResolveDialplanRequest {
            caller_contact_value: caller_id.clone(),
            destination_number: destination_user.clone(),
        });

        let routing_decision = match clients.dialplan.resolve_dialplan(dialplan_req).await {
            Ok(res) => res.into_inner(),
            Err(e) => {
                error!("❌ Dialplan Error: {}", e);
                // Dialplan çökerse Failsafe
                return Ok(Response::new(GetNextHopResponse {
                    uri: self.config.b2bua_sip_addr.clone(),
                    gateway_id: "sentiric-failsafe-b2bua".to_string(),
                }));
            }
        };

        let action = routing_decision.action.as_ref().unwrap();
        let action_type = ActionType::try_from(action.r#type).unwrap_or(ActionType::Unspecified);

        info!("🧠 [DIALPLAN] Action: {:?} Target: {}", action_type, destination_user);

        match action_type {
            ActionType::BridgeCall => {
                let lookup_req = Request::new(LookupContactRequest {
                    sip_uri: req.destination_uri.clone(),
                });

                match clients.registrar.lookup_contact(lookup_req).await {
                    Ok(lookup_res) => {
                        if let Some(target) = lookup_res.into_inner().contact_uris.first() {
                            return Ok(Response::new(GetNextHopResponse {
                                uri: target.clone(),
                                gateway_id: "sentiric-internal-user".to_string(),
                            }));
                        }
                    },
                    Err(_) => {}
                }
                
                Ok(Response::new(GetNextHopResponse {
                    uri: self.config.b2bua_sip_addr.clone(),
                    gateway_id: "sentiric-b2bua-fallback".to_string(),
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