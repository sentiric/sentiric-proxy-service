// sentiric-proxy-service/src/grpc/service.rs

use sentiric_contracts::sentiric::sip::v1::{
    proxy_service_server::ProxyService,
    GetNextHopRequest, GetNextHopResponse,
    LookupContactRequest,
};
use sentiric_contracts::sentiric::dialplan::v1::{
    ResolveDialplanRequest,
};
use tonic::{Request, Response, Status};
use tracing::{info, warn, error, instrument};
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

    /// SIP URI'den kullanıcı adını (veya telefon numarasını) ayıklar.
    /// Örn: "sip:1001@sentiric.cloud" -> "1001"
    fn extract_username(&self, uri: &str) -> String {
        let clean = uri.trim();
        // Şemayı at (sip: veya sips:)
        let without_scheme = if let Some(idx) = clean.find(':') { &clean[idx+1..] } else { clean };
        // Domain'i at (@ sonrası)
        let user_part = if let Some(idx) = without_scheme.find('@') { &without_scheme[..idx] } else { without_scheme };
        
        // Parametreleri ve < > karakterlerini temizle
        let pure_user = if let Some(idx) = user_part.find(';') { &user_part[..idx] } else { user_part };
        
        pure_user.replace('<', "").replace('>', "").trim().to_string()
    }
}

#[tonic::async_trait]
impl ProxyService for MyProxyService {
    
    /// SBC tarafından çağrılır. Bir SIP paketinin nereye gideceğini söyler.
    #[instrument(skip_all, fields(dest = %request.get_ref().destination_uri, method = %request.get_ref().method))]
    async fn get_next_hop(
        &self,
        request: Request<GetNextHopRequest>,
    ) -> Result<Response<GetNextHopResponse>, Status> {
        let req = request.into_inner();
        let destination_user = self.extract_username(&req.destination_uri);
        
        // ---------------------------------------------------------------------
        // 1. REGISTER İsteği (Özel Durum)
        // ---------------------------------------------------------------------
        // Kayıt istekleri her zaman yerel Proxy'nin SIP portuna (13074) gelmelidir.
        // ProxyEngine, bu isteği alıp Registrar gRPC servisine çevirecektir.
        if req.method == "REGISTER" {
            info!("📝 [ROUTE] REGISTER isteği -> Local Proxy (Registrar Gateway)");
            return Ok(Response::new(GetNextHopResponse {
                uri: self.config.registrar_sip_addr.clone(), // Kendi adresimiz
                gateway_id: "sentiric-registrar-local".to_string(),
            }));
        }

        // ---------------------------------------------------------------------
        // 2. INVITE ve Diğer İstekler: DIALPLAN SORGUSU
        // ---------------------------------------------------------------------
        let mut clients = self.clients.lock().await;
        
        // Arayan bilgisini (From) çözümlemek karmaşık olabilir, şimdilik "anonymous" 
        // gönderiyoruz. Dialplan servisi bilinmeyen numaraları "Guest" olarak işler.
        let dialplan_req = Request::new(ResolveDialplanRequest {
            caller_contact_value: "anonymous".to_string(), 
            destination_number: destination_user.clone(),
        });

        // Dialplan servisine sor: "Bu numarayla ne yapayım?"
        let routing_decision = match clients.dialplan.resolve_dialplan(dialplan_req).await {
            Ok(res) => res.into_inner(),
            Err(e) => {
                error!("❌ Dialplan servisine ulaşılamadı: {}", e);
                // Failsafe: Dialplan yoksa B2BUA'ya gönder (O da hata mesajı çalar)
                return Ok(Response::new(GetNextHopResponse {
                    uri: self.config.b2bua_sip_addr.clone(),
                    gateway_id: "sentiric-failsafe".to_string(),
                }));
            }
        };

        // Dialplan'dan gelen aksiyonu al
        let action_name = routing_decision.action.as_ref().map(|a| a.action.as_str()).unwrap_or("UNKNOWN");
        info!("🧠 [DIALPLAN] Karar: {} (Destination: {})", action_name, destination_user);

        // ---------------------------------------------------------------------
        // 3. AKSİYONA GÖRE YÖNLENDİRME
        // ---------------------------------------------------------------------
        match action_name {
            // A. DAHİLİ ARAMA (P2P - Bridge)
            "BRIDGE_CALL" => {
                // Hedef bir iç abone. Registrar'a sorup anlık IP'sini bulmalıyız.
                let lookup_req = Request::new(LookupContactRequest {
                    sip_uri: req.destination_uri.clone(), // Tam SIP URI gönder
                });

                match clients.registrar.lookup_contact(lookup_req).await {
                    Ok(lookup_res) => {
                        let uris = lookup_res.into_inner().contact_uris;
                        // İlk bulunan contact adresine yönlendir
                        if let Some(target_contact) = uris.first() {
                            info!("🏠 [ROUTE] Dahili Abone Bulundu -> {}", target_contact);
                            return Ok(Response::new(GetNextHopResponse {
                                uri: target_contact.clone(),
                                gateway_id: "sentiric-internal-user".to_string(),
                            }));
                        } else {
                            warn!("⚠️ [ROUTE] Abone ({}) tanımlı ama Offline (Registrar boş döndü).", destination_user);
                            // Offline ise B2BUA'ya at, sesli posta veya anons devreye girsin.
                            return Ok(Response::new(GetNextHopResponse {
                                uri: self.config.b2bua_sip_addr.clone(),
                                gateway_id: "sentiric-offline-handler".to_string(),
                            }));
                        }
                    },
                    Err(e) => {
                        error!("❌ Registrar Lookup Hatası: {}", e);
                        return Err(Status::internal("Registrar Error"));
                    }
                }
            },
            
            // B. DIŞ HAT veya AI KONUŞMASI (B2BUA)
            // Bu aksiyonlar iş mantığı gerektirir, medya sunucusu üzerinden geçer.
            "START_AI_CONVERSATION" | "PROCESS_GUEST_CALL" | "PLAY_ANNOUNCEMENT" | "START_ECHO_TEST" => {
                info!("🤖 [ROUTE] AI/Medya İşlemi -> B2BUA");
                return Ok(Response::new(GetNextHopResponse {
                    uri: self.config.b2bua_sip_addr.clone(),
                    gateway_id: "sentiric-ai-gateway".to_string(),
                }));
            },

            // C. BİLİNMEYEN DURUM
            _ => {
                warn!("⚠️ [ROUTE] Bilinmeyen Aksiyon: {}. Varsayılan olarak B2BUA deneniyor.", action_name);
                return Ok(Response::new(GetNextHopResponse {
                    uri: self.config.b2bua_sip_addr.clone(),
                    gateway_id: "sentiric-fallback".to_string(),
                }));
            }
        }
    }
}