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
    
    #[instrument(skip_all, fields(dest = %request.get_ref().destination_uri, method = %request.get_ref().method))]
    async fn get_next_hop(
        &self,
        request: Request<GetNextHopRequest>,
    ) -> Result<Response<GetNextHopResponse>, Status> {
        let req = request.into_inner();
        
        // [KARAR MANTIĞI]
        // Eğer metod "REGISTER" ise, Proxy'nin kendi SIP portuna yönlendir.
        // Proxy, bu paketi alınca `sip/engine.rs` içindeki `handle_register` ile işler.
        // Eğer başka bir şeyse (INVITE vb.), B2BUA'ya yönlendir.
        
        let (target_uri, gateway_id) = match req.method.as_str() {
            "REGISTER" => {
                info!("📍 Yönlendirme: REGISTER -> Registrar Endpoint ({})", self.config.registrar_sip_addr);
                (self.config.registrar_sip_addr.clone(), "sentiric-registrar-core".to_string())
            },
            _ => {
                info!("📍 Yönlendirme: CALL ({}) -> B2BUA Endpoint ({})", req.method, self.config.b2bua_sip_addr);
                (self.config.b2bua_sip_addr.clone(), "sentiric-b2bua-primary".to_string())
            }
        };

        Ok(Response::new(GetNextHopResponse {
            uri: target_uri,
            gateway_id,
        }))
    }
}