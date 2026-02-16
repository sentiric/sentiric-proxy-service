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
use tracing::{info, error, instrument, debug, warn};
use std::sync::Arc;
use tokio::sync::Mutex;
use crate::config::AppConfig;
use crate::grpc::client::InternalClients;

pub struct MyProxyService {
    config: Arc<AppConfig>,
    clients: Arc<Mutex<Option<InternalClients>>>,
}

impl MyProxyService {
    pub fn new(config: Arc<AppConfig>, clients: Arc<Mutex<Option<InternalClients>>>) -> Self {
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
        let destination_user = sip_utils::extract_username_from_uri(&req.destination_uri);

        // [CRITICAL FIX]: B2BUA Yönlendirme Kuralı (ACK Loop Prevention)
        // Eğer hedef kullanıcı 'b2bua' ise (veya benzer sistem aktörleri),
        // Public IP'ye bakmaksızın paketi doğrudan iç ağdaki B2BUA servisine yönlendir.
        // Bu, SBC'nin Contact header rewrite yapması sonucu oluşan döngüyü kırar.
        if destination_user == "b2bua" {
            debug!("⚡ Special Route: 'b2bua' user detected. Forcing route to internal B2BUA service.");
            return Ok(Response::new(GetNextHopResponse {
                uri: self.config.b2bua_sip_addr.clone(),
                gateway_id: "force-internal-b2bua".to_string(),
            }));
        }
        
        if req.is_in_dialog {
            debug!("⏩ In-Dialog Request: Directly routing to target URI.");
            let target_uri = req.destination_uri.replace('<', "").replace('>', "");
            return Ok(Response::new(GetNextHopResponse {
                uri: target_uri,
                gateway_id: "direct-route-in-dialog".to_string(),
            }));
        }

        if req.method == "REGISTER" {
            return Ok(Response::new(GetNextHopResponse {
                uri: self.config.registrar_sip_addr.clone(),
                gateway_id: "registrar-local".to_string(),
            }));
        }
        
        let clients_guard = self.clients.lock().await;
        let clients = match &*clients_guard {
            Some(c) => c,
            None => {
                warn!("⚠️ Proxy not ready (clients none)");
                return Err(Status::unavailable("Proxy is initializing"));
            }
        };

        let caller_id = sip_utils::extract_username_from_uri(&req.from_uri);
        
        let mut dialplan_client = clients.dialplan.clone();
        let mut registrar_client = clients.registrar.clone();
        drop(clients_guard);

        let dialplan_req = Request::new(ResolveDialplanRequest {
            caller_contact_value: caller_id,
            destination_number: destination_user.clone(),
        });

        match dialplan_client.resolve_dialplan(dialplan_req).await {
            Ok(res) => {
                let resolution = res.into_inner();
                let action = resolution.action.as_ref().unwrap();
                let action_type = ActionType::try_from(action.r#type).unwrap_or(ActionType::Unspecified);
                
                info!("🧠 [DIALPLAN] Action: {:?}", action_type);

                match action_type {
                    ActionType::BridgeCall => {
                        let lookup_req = Request::new(LookupContactRequest { sip_uri: req.destination_uri });
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
            },
            Err(e) => {
                error!("❌ Dialplan Error: {}. Failsafe to B2BUA.", e);
                Ok(Response::new(GetNextHopResponse {
                    uri: self.config.b2bua_sip_addr.clone(),
                    gateway_id: "failsafe-b2bua".to_string(),
                }))
            }
        }
    }
}