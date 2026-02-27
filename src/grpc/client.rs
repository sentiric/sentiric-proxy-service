// sentiric-proxy-service/src/grpc/client.rs

use crate::config::AppConfig;
use anyhow::Result; 
use sentiric_contracts::sentiric::sip::v1::registrar_service_client::RegistrarServiceClient;
use sentiric_contracts::sentiric::sip::v1::b2bua_service_client::B2buaServiceClient;
use sentiric_contracts::sentiric::dialplan::v1::dialplan_service_client::DialplanServiceClient;

use tonic::transport::{Channel, ClientTlsConfig, Certificate, Identity};
use std::time::Duration;
use tracing::{info, warn};

#[derive(Clone)]
pub struct InternalClients {
    pub registrar: RegistrarServiceClient<Channel>,
    pub b2bua: B2buaServiceClient<Channel>,
    pub dialplan: DialplanServiceClient<Channel>,
}

impl InternalClients {
    pub async fn connect(config: &AppConfig) -> Result<Self> {
        info!("🔌 İç servislere bağlanılıyor (mTLS + Aggressive KeepAlive)...");

        let registrar_channel = create_secure_channel(&config.registrar_grpc_url, "registrar-service", config).await?;
        let b2bua_channel = create_secure_channel(&config.b2bua_grpc_url, "b2bua-service", config).await?;
        let dialplan_channel = create_secure_channel(&config.dialplan_grpc_url, "dialplan-service", config).await?;

        info!("✅ Tüm dış gRPC istemcileri başarıyla oluşturuldu.");

        Ok(Self {
            registrar: RegistrarServiceClient::new(registrar_channel),
            b2bua: B2buaServiceClient::new(b2bua_channel),
            dialplan: DialplanServiceClient::new(dialplan_channel),
        })
    }
}

async fn create_secure_channel(url: &str, server_name: &str, config: &AppConfig) -> Result<Channel> {
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

    let cert = tokio::fs::read(&config.cert_path).await?;
    let key = tokio::fs::read(&config.key_path).await?;
    let identity = Identity::from_pem(cert, key);
    let ca_cert = tokio::fs::read(&config.ca_path).await?;
    let ca_certificate = Certificate::from_pem(ca_cert);

    let tls_config = ClientTlsConfig::new()
        .domain_name(server_name)
        .ca_certificate(ca_certificate)
        .identity(identity);

    info!("🔗 Bağlantı deneniyor: {} (Timeout: 3s)", server_name);

    // [KRİTİK GÜNCELLEME]: Resilience & Timeout Ayarları
    // Bu ayarlar servisin "donmasını" engeller.
    let channel = Channel::from_shared(target_url)?
        .connect_timeout(Duration::from_secs(3))         // Bağlantı 3sn içinde kurulamazsa hata ver
        .timeout(Duration::from_secs(3))                 // İstek 3sn içinde bitmezse iptal et
        .keep_alive_while_idle(true)                     // Boşta olsa bile ping at
        .http2_keep_alive_interval(Duration::from_secs(10)) // 10 saniyede bir canlılık kontrolü
        .keep_alive_timeout(Duration::from_secs(3))      // Keepalive yanıtı gelmezse 3sn sonra kopar
        .tcp_keepalive(Some(Duration::from_secs(10)))    // TCP seviyesinde canlılık
        .tls_config(tls_config)?
        .connect()
        .await?;

    info!("✅ gRPC bağlantısı başarılı: {}", server_name);

    Ok(channel)
}