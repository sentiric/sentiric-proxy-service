// sentiric-proxy-service/src/grpc/service.rs

use sentiric_contracts::sentiric::sip::v1::{
    proxy_service_server::ProxyService,
    GetNextHopRequest, GetNextHopResponse,
};
use tonic::{Request, Response, Status};
use tracing::{debug, instrument};
use std::sync::Arc;
use crate::config::AppConfig;

pub struct MyProxyService {
    config: Arc<AppConfig>,
}

impl MyProxyService {
    pub fn new(config: Arc<AppConfig>) -> Self {
        Self { config }
    }

    fn extract_username(&self, uri: &str) -> String {
        let clean = uri.trim();
        let without_scheme = if let Some(idx) = clean.find(':') { &clean[idx+1..] } else { clean };
        let user_part = if let Some(idx) = without_scheme.find('@') { &without_scheme[..idx] } else { without_scheme };
        if let Some(idx) = user_part.find(';') { user_part[..idx].to_string() } else { user_part.to_string() }
            .replace('<', "").replace('>', "")
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

        // --- ENVIRONMENT TABANLI YÖNLENDİRME ---
        // registrar_sip_addr config'den gelir ve aslında bu servis (proxy) adresidir.
        let (target_uri, gateway_id) = if req.method == "REGISTER" {
            // REGISTER talepleri doğrudan bu servisin SIP portuna gelmeli
            (self.config.registrar_sip_addr.clone(), "sentiric-proxy-local".to_string())
        
        } else if destination_user == "9998" || destination_user == "9999" {
            // AI servisleri
            (self.config.b2bua_sip_addr.clone(), "sentiric-b2bua-primary".to_string())
        
        } else {
            // Dahili abone aramaları yine Proxy üzerinden yönlenir
            (self.config.registrar_sip_addr.clone(), "sentiric-proxy-router".to_string())
        };

        debug!("🚀 [ROUTE] Decision: {} -> {}", req.method, target_uri);

        Ok(Response::new(GetNextHopResponse {
            uri: target_uri,
            gateway_id,
        }))
    }
}