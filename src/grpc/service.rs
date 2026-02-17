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
        hedef = %request.get_ref().destination_uri, 
        metod = %request.get_ref().method,
        diyalog_ici = %request.get_ref().is_in_dialog
    ))]
    async fn get_next_hop(
        &self,
        request: Request<GetNextHopRequest>,
    ) -> Result<Response<GetNextHopResponse>, Status> {
        let req = request.into_inner();
        let destination_user = sip_utils::extract_username_from_uri(&req.destination_uri).to_lowercase();

        // --- [BORÇ ÖDENDİ]: DİNAMİK İÇ SERVİS YÖNLENDİRMESİ ---
        if self.config.internal_service_users.contains(&destination_user) {
            info!("⚡ [CORE-ROUTING] Dahili servis kullanıcısı tespit edildi: {}. Yönlendirme: {}", 
                destination_user, self.config.b2bua_sip_addr);
            return Ok(Response::new(GetNextHopResponse {
                uri: self.config.b2bua_sip_addr.clone(),
                gateway_id: format!("core-internal-{}", destination_user),
            }));
        }
        // -----------------------------------------------------
        
        if req.is_in_dialog {
            debug!("⏩ Diyalog İçi İstek: Hedefe doğrudan yönlendiriliyor.");
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
                warn!("⚠️ Proxy henüz hazır değil (bağımlı servisler bekleniyor)");
                return Err(Status::unavailable("Proxy servisi başlatılıyor..."));
            }
        };

        let caller_id = sip_utils::extract_username_from_uri(&req.from_uri);
        let mut dialplan_client = clients.dialplan.clone();
        let mut registrar_client = clients.registrar.clone();
        drop(clients_guard);

        let dialplan_req = Request::new(ResolveDialplanRequest {
            caller_contact_value: caller_id.clone(),
            destination_number: destination_user.clone(),
        });

        match dialplan_client.resolve_dialplan(dialplan_req).await {
            Ok(res) => {
                let resolution = res.into_inner();
                let action = resolution.action.as_ref().ok_or_else(|| Status::internal("Dialplan action missing"))?;
                let action_type = ActionType::try_from(action.r#type).unwrap_or(ActionType::Unspecified);
                
                info!("🧠 [DIALPLAN] Karar: {:?} (Arayan: {}, Aranan: {})", action_type, caller_id, destination_user);

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
                error!("❌ Dialplan Hatası: {}. Failsafe olarak B2BUA'ya yönlendiriliyor.", e);
                Ok(Response::new(GetNextHopResponse {
                    uri: self.config.b2bua_sip_addr.clone(),
                    gateway_id: "failsafe-b2bua".to_string(),
                }))
            }
        }
    }
}