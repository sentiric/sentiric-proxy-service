// sentiric-proxy-service/src/grpc/service.rs
use sentiric_contracts::sentiric::sip::v1::{
    proxy_service_server::ProxyService,
    GetNextHopRequest, GetNextHopResponse,
};
use tonic::{Request, Response, Status};
use tracing::{info, instrument};

// Bu, bir SIP Proxy'sinin alacağı tek gRPC isteği (Diğer servisler bu servisten SIP ile konuşmak istediğinde)
pub struct MyProxyService {}

#[tonic::async_trait]
impl ProxyService for MyProxyService {
    
    #[instrument(skip_all, fields(dest_uri = %request.get_ref().destination_uri))]
    async fn get_next_hop(
        &self,
        request: Request<GetNextHopRequest>,
    ) -> Result<Response<GetNextHopResponse>, Status> {
        info!("GetNextHop RPC isteği alındı. SIP mesajı analiz ediliyor...");
        let req = request.into_inner(); // req değişkeni artık kullanılıyor
        
        // Basit bir placeholder yönlendirme mantığı: Her şeyi B2BUA'ya gönder.
        let next_hop = GetNextHopResponse {
            uri: "sentiric-b2bua-service:12081".to_string(), 
            gateway_id: "sentiric-b2bua".to_string(),
        };

        Ok(Response::new(next_hop))
    }
}