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

    /// SIP URI'den kullanıcı adını güvenli şekilde ayıklar.
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
        
        // --- IDENTITY RESOLUTION LOGIC (v1.15.0) ---
        let caller_id = if !req.from_uri.is_empty() {
            let extracted = self.extract_username(&req.from_uri);
            info!("🆔 Identity Resolved: {} -> {}", req.from_uri, extracted);
            extracted
        } else {
            warn!("⚠️ Missing 'from_uri' from source. Using IP fallback: {}", req.source_ip);
            req.source_ip.clone()
        };

        // 1. REGISTER Yönlendirmesi
        if req.method == "REGISTER" {
            return Ok(Response::new(GetNextHopResponse {
                uri: self.config.registrar_sip_addr.clone(),
                gateway_id: "sentiric-registrar-local".to_string(),
            }));
        }

        // 2. DIALPLAN SORGUSU (Zenginleştirilmiş Kimlik ile)
        let mut clients = self.clients.lock().await;
        
        let dialplan_req = Request::new(ResolveDialplanRequest {
            caller_contact_value: caller_id.clone(),
            destination_number: destination_user.clone(),
        });

        let routing_decision = match clients.dialplan.resolve_dialplan(dialplan_req).await {
            Ok(res) => res.into_inner(),
            Err(e) => {
                error!("❌ Dialplan Service Error: {}", e);
                return Ok(Response::new(GetNextHopResponse {
                    uri: self.config.b2bua_sip_addr.clone(),
                    gateway_id: "sentiric-failsafe-b2bua".to_string(),
                }));
            }
        };

        // [v1.15.0 FIX]: Variant adı 'ActionTypeUnspecified' değil 'Unspecified' olmalıdır.
        let action = routing_decision.action.as_ref().unwrap();
        let action_type = ActionType::try_from(action.r#type).unwrap_or(ActionType::Unspecified);

        info!("🧠 [DIALPLAN] Decision: {:?} for {} -> {}", action_type, caller_id, destination_user);

        // 3. AKSİYON MANTIĞI
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
                        } else {
                            warn!("⚠️ Internal user {} not found. Sending to B2BUA Fallback.", destination_user);
                        }
                    },
                    Err(e) => error!("❌ Registrar Lookup Error: {}", e),
                }
                
                Ok(Response::new(GetNextHopResponse {
                    uri: self.config.b2bua_sip_addr.clone(),
                    gateway_id: "sentiric-b2bua-fallback".to_string(),
                }))
            },
            
            // Unspecified, StartAiConversation, EchoTest, PlayStaticAnnouncement -> B2BUA
            _ => {
                Ok(Response::new(GetNextHopResponse {
                    uri: self.config.b2bua_sip_addr.clone(),
                    gateway_id: "sentiric-ai-gateway".to_string(),
                }))
            }
        }
    }
}