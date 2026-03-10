// sentiric-proxy-service/src/grpc/client.rs

use crate::config::AppConfig;
use anyhow::Result; 
use sentiric_contracts::sentiric::sip::v1::registrar_service_client::RegistrarServiceClient;
use sentiric_contracts::sentiric::sip::v1::b2bua_service_client::B2buaServiceClient;
use sentiric_contracts::sentiric::dialplan::v1::dialplan_service_client::DialplanServiceClient;

use tonic::transport::{Channel, ClientTlsConfig, Certificate, Identity, Endpoint};
use tracing::{info, warn};

#[derive(Clone)]
pub struct InternalClients {
    pub registrar: RegistrarServiceClient<Channel>,
    pub b2bua: B2buaServiceClient<Channel>,
    pub dialplan: DialplanServiceClient<Channel>,
}

impl InternalClients {
    pub async fn connect(config: &AppConfig) -> Result<Self> {
        info!("🔌 İç servislere bağlanılıyor (mTLS + Lazy Connect)...");

        // TLS Config'i bir kere oluştur
        let tls_config = if !config.ca_path.is_empty() {
            match load_tls_config(config).await {
                Ok(cfg) => Some(cfg),
                Err(e) => {
                    warn!("⚠️ mTLS sertifikaları yüklenemedi, güvensiz mod denenecek: {}", e);
                    None
                }
            }
        } else {
            None
        };

        // connect_lazy ile anında Endpoint'leri bağla (Bekleme yapmaz)
        let registrar_channel = connect_endpoint(&config.registrar_grpc_url, "registrar-service", &tls_config).await?;
        let b2bua_channel = connect_endpoint(&config.b2bua_grpc_url, "b2bua-service", &tls_config).await?;
        let dialplan_channel = connect_endpoint(&config.dialplan_grpc_url, "dialplan-service", &tls_config).await?;

        info!("✅ Tüm dış gRPC istemcileri yapılandırıldı (Lazy Mode).");

        Ok(Self {
            registrar: RegistrarServiceClient::new(registrar_channel),
            b2bua: B2buaServiceClient::new(b2bua_channel),
            dialplan: DialplanServiceClient::new(dialplan_channel),
        })
    }
}

async fn connect_endpoint(url: &str, server_name: &str, tls_config: &Option<ClientTlsConfig>) -> Result<Channel> {
    let target_url = if url.starts_with("http") {
        if url.starts_with("http://") {
             warn!("⚠️ Güvensiz URL tespit edildi ({}), HTTPS'e zorlanıyor.", url);
             url.replace("http://", "https://")
        } else {
            url.to_string()
        }
    } else {
        format!("https://{}", url)
    };

    let mut endpoint = Endpoint::from_shared(target_url)?;

    if let Some(tls) = tls_config {
        // SNI (Server Name Indication) Override
        let tls_with_sni = tls.clone().domain_name(server_name);
        endpoint = endpoint.tls_config(tls_with_sni)?;
    }

    // [KRİTİK MİMARİ DEĞİŞİKLİK]: connect().await YERİNE connect_lazy()
    // Servis anında ayağa kalkar. Gerçek TCP/mTLS bağlantısı ilk çağrıda (INVITE gelince) kurulur.
    Ok(endpoint.connect_lazy())
}

async fn load_tls_config(config: &AppConfig) -> Result<ClientTlsConfig> {
    let cert = tokio::fs::read(&config.cert_path).await?;
    let key = tokio::fs::read(&config.key_path).await?;
    let identity = Identity::from_pem(cert, key);
    
    let ca_cert = tokio::fs::read(&config.ca_path).await?;
    let ca_certificate = Certificate::from_pem(ca_cert);

    Ok(ClientTlsConfig::new()
        .ca_certificate(ca_certificate)
        .identity(identity))
}