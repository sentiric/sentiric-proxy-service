// sentiric-proxy-service/src/grpc/service.rs

use sentiric_contracts::sentiric::sip::v1::{
    proxy_service_server::ProxyService,
    GetNextHopRequest, GetNextHopResponse,
    LookupContactRequest,
};
use sentiric_contracts::sentiric::dialplan::v1::{
    ResolveDialplanRequest,
};
use sentiric_sip_core::SipUri;
use std::str::FromStr;

use tonic::{Request, Response, Status};
use tracing::{error, instrument};
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
    
    #[instrument(skip_all, fields(dest = %request.get_ref().destination_uri, method = %request.get_ref().method))]
    async fn get_next_hop(
        &self,
        request: Request<GetNextHopRequest>,
    ) -> Result<Response<GetNextHopResponse>, Status> {
        let req = request.into_inner();
        let destination_user = self.extract_username(&req.destination_uri);
        
        // MİMARİ DÜZELTME: Arayan bilgisini (From) belirle
        // source_ip bilgisi SBC tarafından iletilen fiziksel IP'dir.
        // Dialplan'a gerçek arayanı (From) göndermeliyiz. 
        // Şimdilik SBC'den From gelmediği için source_ip veya anonymous kullanılır.
        // Ancak ileride kontrat güncellenerek FromUri eklenmelidir.
        let caller_info = if req.source_ip.is_empty() { "anonymous".to_string() } else { req.source_ip.clone() };

        if req.method == "REGISTER" {
            return Ok(Response::new(GetNextHopResponse {
                uri: self.config.registrar_sip_addr.clone(),
                gateway_id: "sentiric-registrar-local".to_string(),
            }));
        }

        let mut clients = self.clients.lock().await;
        
        // Dialplan'a "Arayan Kim?" bilgisini gönderiyoruz.
        let dialplan_req = Request::new(ResolveDialplanRequest {
            caller_contact_value: caller_info, 
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

        let action_name = routing_decision.action.as_ref().map(|a| a.action.as_str()).unwrap_or("UNKNOWN");

        match action_name {
            "BRIDGE_CALL" => {
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
                    Err(e) => error!("❌ Registrar error: {}", e),
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