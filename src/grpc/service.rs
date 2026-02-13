// sentiric-proxy-service/src/grpc/service.rs

use sentiric_contracts::sentiric::sip::v1::{
    proxy_service_server::ProxyService,
    GetNextHopRequest, GetNextHopResponse,
    LookupContactRequest,
};
use sentiric_contracts::sentiric::dialplan::v1::{
    ResolveDialplanRequest, ActionType
};
use sentiric_sip_core::utils as sip_utils;

use tonic::{Request, Response, Status};
use tracing::{info, error, instrument, debug};
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
}

#[tonic::async_trait]
impl ProxyService for MyProxyService {
    
    #[instrument(skip_all, fields(
        dest = %request.get_ref().destination_uri, 
        method = %request.get_ref().method,
        in_dialog = %request.get_ref().is_in_dialog
    ))]
    async fn get_next_hop(
        &self,
        request: Request<GetNextHopRequest>,
    ) -> Result<Response<GetNextHopResponse>, Status> {
        let req = request.into_inner();
        
        // --- [CRITICAL REFACTOR] ---
        // 1. In-Dialog ise, Dialplan'ı bypass et ve doğrudan hedefe git (Loose Routing)
        if req.is_in_dialog {
            debug!("⏩ [LOOSE-ROUTE] In-Dialog Request ({}) bypassing Dialplan.", req.method);
            let target_uri = req.destination_uri.replace('<', "").replace('>', "");
            return Ok(Response::new(GetNextHopResponse {
                uri: target_uri,
                gateway_id: "direct-route-in-dialog".to_string(),
            }));
        }

        // 2. REGISTER ise, Registrar'a git
        if req.method == "REGISTER" {
            debug!("⏩ [SYSTEM-ROUTE] REGISTER request routed to Registrar.");
            return Ok(Response::new(GetNextHopResponse {
                uri: self.config.registrar_sip_addr.clone(),
                gateway_id: "sentiric-registrar-local".to_string(),
            }));
        }
        
        // --- Sadece yeni diyaloglar (örn: INVITE) için Dialplan sorgusu ---
        let destination_user = sip_utils::extract_username_from_uri(&req.destination_uri);
        let caller_id = sip_utils::extract_username_from_uri(&req.from_uri);
        
        let mut clients = self.clients.lock().await;
        
        let dialplan_req = Request::new(ResolveDialplanRequest {
            caller_contact_value: caller_id,
            destination_number: destination_user.clone(),
        });

        match clients.dialplan.resolve_dialplan(dialplan_req).await {
            Ok(res) => {
                let resolution = res.into_inner();
                let action = resolution.action.as_ref().unwrap();
                let action_type = ActionType::try_from(action.r#type).unwrap_or(ActionType::Unspecified);
                info!("🧠 [DIALPLAN] Action: {:?}", action_type);

                match action_type {
                    ActionType::BridgeCall => {
                        let lookup_req = Request::new(LookupContactRequest { sip_uri: req.destination_uri });
                        if let Ok(lookup_res) = clients.registrar.lookup_contact(lookup_req).await {
                            if let Some(target) = lookup_res.into_inner().contact_uris.first() {
                                return Ok(Response::new(GetNextHopResponse {
                                    uri: target.clone(),
                                    gateway_id: "sentiric-internal-user".to_string(),
                                }));
                            }
                        }
                        // Fallback
                        Ok(Response::new(GetNextHopResponse {
                            uri: self.config.b2bua_sip_addr.clone(),
                            gateway_id: "sentiric-b2bua-fallback".to_string(),
                        }))
                    },
                    _ => { // Diğer tüm aksiyonlar (Echo, AI vb.) B2BUA'ya
                        Ok(Response::new(GetNextHopResponse {
                            uri: self.config.b2bua_sip_addr.clone(),
                            gateway_id: "sentiric-ai-gateway".to_string(),
                        }))
                    }
                }
            },
            Err(e) => {
                error!("❌ Dialplan Error: {}. Failsafe to B2BUA.", e);
                Ok(Response::new(GetNextHopResponse {
                    uri: self.config.b2bua_sip_addr.clone(),
                    gateway_id: "sentiric-failsafe-b2bua".to_string(),
                }))
            }
        }
    }
}