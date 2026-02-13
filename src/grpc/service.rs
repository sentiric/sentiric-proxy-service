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
use tracing::{info, error, instrument, warn};
use std::sync::Arc;
use tokio::sync::Mutex;
use crate::config::AppConfig;
use crate::grpc::client::InternalClients;

pub struct MyProxyService {
    config: Arc<AppConfig>,
    // DEĞİŞİKLİK: İstemciler başlangıçta olmayabilir (Option).
    // Mutex ile thread-safe, Arc ile paylaşılabilir.
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
        
        // 1. In-Dialog İstekleri (Loose Routing) - Bağımlılık gerektirmez
        // Diyalog içindeki mesajlar (ACK, BYE) doğrudan yönlendirilir, veritabanı sorgusu yapılmaz.
        if req.is_in_dialog {
            // Basit temizlik: <sip:...> -> sip:...
            let target_uri = req.destination_uri.replace('<', "").replace('>', "");
            return Ok(Response::new(GetNextHopResponse {
                uri: target_uri,
                gateway_id: "direct-route-in-dialog".to_string(),
            }));
        }

        // 2. REGISTER İstekleri - Registrar'a yönlendirilir
        // Config'den okunduğu için bağımlılık gerektirmez.
        if req.method == "REGISTER" {
            return Ok(Response::new(GetNextHopResponse {
                uri: self.config.registrar_sip_addr.clone(),
                gateway_id: "sentiric-registrar-local".to_string(),
            }));
        }
        
        // 3. Dialplan Sorgusu (INVITE vb.) - Bağımlılık Gerektirir
        // Önce istemcilerin bağlı olup olmadığını kontrol et.
        
        // Mutex Guard'ı al
        let clients_guard = self.clients.lock().await;
        
        // Guard içindeki Option'ı kontrol et
        let clients = match &*clients_guard {
            Some(c) => c,
            None => {
                // Eğer henüz bağlanılmadıysa, servisi unavailable olarak işaretle.
                // Bu, Kubernetes/Docker'ın trafiği kesmesini veya client'ın beklemesini sağlar.
                warn!("⚠️ System Initializing: Dependencies not yet connected.");
                return Err(Status::unavailable("Proxy Service is initializing..."));
            }
        };

        // Buraya geldiysek bağlantılar hazırdır.
        let destination_user = sip_utils::extract_username_from_uri(&req.destination_uri);
        let caller_id = sip_utils::extract_username_from_uri(&req.from_uri);
        
        // İstemcileri klonla (InternalClients yapısı hafiftir, channel kopyalanır)
        let mut dialplan_client = clients.dialplan.clone();
        let mut registrar_client = clients.registrar.clone();
        
        // Guard'ı hemen serbest bırak (Drop) ki kilit süresi uzamasın
        drop(clients_guard);

        // Dialplan Sorgusu
        let dialplan_req = Request::new(ResolveDialplanRequest {
            caller_contact_value: caller_id,
            destination_number: destination_user.clone(),
        });

        match dialplan_client.resolve_dialplan(dialplan_req).await {
            Ok(res) => {
                let resolution = res.into_inner();
                // Opsiyonel alan kontrolü
                let action = match resolution.action {
                    Some(a) => a,
                    None => {
                        error!("❌ Dialplan returned no action.");
                        return Err(Status::internal("Dialplan logic error"));
                    }
                };
                
                let action_type = ActionType::try_from(action.r#type).unwrap_or(ActionType::Unspecified);
                info!("🧠 [DIALPLAN] Action: {:?}", action_type);

                match action_type {
                    ActionType::BridgeCall => {
                        let lookup_req = Request::new(LookupContactRequest { sip_uri: req.destination_uri });
                        if let Ok(lookup_res) = registrar_client.lookup_contact(lookup_req).await {
                            if let Some(target) = lookup_res.into_inner().contact_uris.first() {
                                return Ok(Response::new(GetNextHopResponse {
                                    uri: target.clone(),
                                    gateway_id: "sentiric-internal-user".to_string(),
                                }));
                            }
                        }
                        // Fallback: Kullanıcı bulunamazsa B2BUA (Sesli Posta vb. için)
                        Ok(Response::new(GetNextHopResponse {
                            uri: self.config.b2bua_sip_addr.clone(),
                            gateway_id: "sentiric-b2bua-fallback".to_string(),
                        }))
                    },
                    _ => { // Diğer tüm aksiyonlar (Echo, AI vb.) B2BUA'ya gider
                        Ok(Response::new(GetNextHopResponse {
                            uri: self.config.b2bua_sip_addr.clone(),
                            gateway_id: "sentiric-ai-gateway".to_string(),
                        }))
                    }
                }
            },
            Err(e) => {
                error!("❌ Dialplan Service Error: {}. Falling back to B2BUA.", e);
                // Failsafe: Dialplan çalışmıyorsa bile trafiği B2BUA'ya akıt.
                Ok(Response::new(GetNextHopResponse {
                    uri: self.config.b2bua_sip_addr.clone(),
                    gateway_id: "sentiric-failsafe-b2bua".to_string(),
                }))
            }
        }
    }
}