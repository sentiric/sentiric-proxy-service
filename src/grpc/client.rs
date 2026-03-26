// Dosya: sentiric-sip-proxy-service/src/grpc/client.rs

use crate::config::AppConfig;
use anyhow::Result; // [FIX]: Kullanılmayan "Context" importu silindi
use sentiric_contracts::sentiric::sip::v1::registrar_service_client::RegistrarServiceClient;
use sentiric_contracts::sentiric::sip::v1::b2bua_service_client::B2buaServiceClient;
use sentiric_contracts::sentiric::dialplan::v1::dialplan_service_client::DialplanServiceClient;
use sentiric_contracts::sentiric::user::v1::user_service_client::UserServiceClient; 

use tonic::transport::{Channel, ClientTlsConfig, Certificate, Identity, Endpoint};
use tracing::{info, warn, error};

#[derive(Clone)]
pub struct InternalClients {
    pub registrar: RegistrarServiceClient<Channel>,
    pub b2bua: B2buaServiceClient<Channel>,
    pub dialplan: DialplanServiceClient<Channel>,
    pub user: UserServiceClient<Channel>, 
}

impl InternalClients {
    pub async fn connect(config: &AppConfig) -> Result<Self> {
        info!(event="GRPC_CLIENTS_START", "🔌 İç servislere bağlanılıyor (mTLS + Lazy Connect)...");

        // [ARCH-COMPLIANCE] mTLS failure policy: CA_PATH zorunlu. Güvensiz mod YASAK!
        if config.ca_path.is_empty() {
            error!(event="MTLS_CONFIG_MISSING", "CA_PATH tanımlanmamış. Sistem başlatılamaz.");
            anyhow::bail!("[ARCH-COMPLIANCE] mTLS CA_PATH zorunludur.");
        }

        // [ARCH-COMPLIANCE] mTLS sertifikası yüklenemezse fallback yasaktır, panic/bail fırlat.
        let tls_config = match load_tls_config(config).await {
            Ok(cfg) => cfg,
            Err(e) => {
                error!(event="MTLS_LOAD_FAIL", error=%e, "mTLS sertifikaları yüklenemedi. Güvensiz moda geçiş YASAKTIR.");
                anyhow::bail!("[ARCH-COMPLIANCE] mTLS sertifika yükleme hatası: {}", e);
            }
        };

        // SNI Adları Sertifikalarla Tam Uyumlu!
        let registrar_channel = connect_endpoint(&config.registrar_grpc_url, "sip-registrar-service", &tls_config).await?;
        let b2bua_channel = connect_endpoint(&config.b2bua_grpc_url, "sip-b2bua-service", &tls_config).await?;
        let dialplan_channel = connect_endpoint(&config.dialplan_grpc_url, "dialplan-service", &tls_config).await?;
        let user_channel = connect_endpoint(&config.user_service_grpc_url, "user-service", &tls_config).await?;

        info!(event="GRPC_CLIENTS_READY", "✅ Tüm dış gRPC istemcileri yapılandırıldı (Lazy Mode).");

        Ok(Self {
            registrar: RegistrarServiceClient::new(registrar_channel),
            b2bua: B2buaServiceClient::new(b2bua_channel),
            dialplan: DialplanServiceClient::new(dialplan_channel),
            user: UserServiceClient::new(user_channel),
        })
    }
}

async fn connect_endpoint(url: &str, server_name: &str, tls_config: &ClientTlsConfig) -> Result<Channel> {
    let target_url = if url.starts_with("http") {
        if url.starts_with("http://") {
             warn!(event="INSECURE_URL_FIXED", url=%url, "⚠️ Güvensiz URL tespit edildi ({}), HTTPS'e zorlanıyor.", url);
             url.replace("http://", "https://")
        } else {
            url.to_string()
        }
    } else {
        format!("https://{}", url)
    };

    let mut endpoint = Endpoint::from_shared(target_url)?;

    let tls_with_sni = tls_config.clone().domain_name(server_name);
    endpoint = endpoint.tls_config(tls_with_sni)?;

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