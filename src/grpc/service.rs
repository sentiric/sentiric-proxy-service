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
use tracing::{info, error, warn, instrument}; // warn eklendi
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
    /// Örn: "sip:+90555...@domain" -> "+90555..."
    fn extract_username(&self, uri_str: &str) -> String {
        match SipUri::from_str(uri_str) {
            Ok(uri) => uri.user.unwrap_or_else(|| "anonymous".to_string()),
            Err(_) => {
                // Fallback: Basit string manipülasyonu
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
        
        // --- IDENTITY RESOLUTION LOGIC (v2.1) ---
        // 1. SBC'den gelen 'from_uri' var mı?
        let caller_id = if !req.from_uri.is_empty() {
            let extracted = self.extract_username(&req.from_uri);
            info!("🆔 Identity Resolved via SIP Header: {} -> {}", req.from_uri, extracted);
            extracted
        } else {
            // 2. Yoksa (Legacy SBC), kaynak IP'ye dön (Eski davranış, ama logla uyar)
            warn!("⚠️ Missing 'from_uri' from SBC. Fallback to Source IP identity.");
            if !req.source_ip.is_empty() { 
                req.source_ip.clone() 
            } else { 
                "anonymous".to_string() 
            }
        };

        // 1. REGISTER Yönlendirmesi
        if req.method == "REGISTER" {
            return Ok(Response::new(GetNextHopResponse {
                uri: self.config.registrar_sip_addr.clone(),
                gateway_id: "sentiric-registrar-local".to_string(),
            }));
        }

        // 2. DIALPLAN SORGUSU (Artık Gerçek Kimlikle)
        let mut clients = self.clients.lock().await;
        
        let dialplan_req = Request::new(ResolveDialplanRequest {
            caller_contact_value: caller_id.clone(), // GERÇEK NUMARA BURADA
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
        info!("🧠 [DIALPLAN] Caller: {} -> Dest: {} => Action: {}", caller_id, destination_user, action_name);

        // 3. AKSİYON MANTIĞI
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
                        } else {
                            warn!("⚠️ Internal user {} not found in Registrar. Sending to B2BUA Fallback.", destination_user);
                        }
                    },
                    Err(e) => error!("❌ Registrar Lookup Error: {}", e),
                }
                
                // Offline fallback -> B2BUA (Sesli Posta vb.)
                Ok(Response::new(GetNextHopResponse {
                    uri: self.config.b2bua_sip_addr.clone(),
                    gateway_id: "sentiric-b2bua-fallback".to_string(),
                }))
            },
            
            _ => {
                // AI, IVR, Outbound, Echo Test -> B2BUA
                Ok(Response::new(GetNextHopResponse {
                    uri: self.config.b2bua_sip_addr.clone(),
                    gateway_id: "sentiric-ai-gateway".to_string(),
                }))
            }
        }
    }
}