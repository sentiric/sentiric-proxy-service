// sentiric-proxy-service/src/grpc/service.rs
use sentiric_contracts::sentiric::sip::v1::{
    proxy_service_server::ProxyService,
    GetNextHopRequest, GetNextHopResponse,
};
use tonic::{Request, Response, Status};
use tracing::{info, instrument};
use std::sync::Arc;
use crate::config::AppConfig;

pub struct MyProxyService {
    config: Arc<AppConfig>,
}

impl MyProxyService {
    pub fn new(config: Arc<AppConfig>) -> Self {
        Self { config }
    }
}

#[tonic::async_trait]
impl ProxyService for MyProxyService {
    
    #[instrument(skip_all, fields(dest_uri = %request.get_ref().destination_uri))]
    async fn get_next_hop(
        &self,
        request: Request<GetNextHopRequest>,
    ) -> Result<Response<GetNextHopResponse>, Status> {
        let req = request.into_inner();
        
        info!(
            "GetNextHop RPC isteği alındı. Kaynak IP: {}, Hedef URI: {}", 
            req.source_ip, 
            req.destination_uri
        );
        
        // --- YÖNLENDİRME MANTIĞI ---
        // Şu an için tüm bilinmeyen dış trafiği (SBC'den gelen)
        // doğrudan B2BUA'ya (AI Orkestratörü) yönlendiriyoruz.
        // İleride burada daha karmaşık Load Balancing yapılabilir.
        
        let target_sip_uri = self.config.b2bua_sip_addr.clone();
        
        info!("Yönlendirme kararı verildi -> B2BUA ({})", target_sip_uri);

        let next_hop = GetNextHopResponse {
            // Bu URI, SBC tarafından alınıp `transport.send` ile kullanılacak.
            // Örn: "b2bua-service.service.sentiric.cloud:13084"
            uri: target_sip_uri, 
            gateway_id: "sentiric-b2bua-primary".to_string(),
        };

        Ok(Response::new(next_hop))
    }
}