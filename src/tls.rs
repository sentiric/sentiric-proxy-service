// sentiric-proxy-service/src/tls.rs
use crate::config::AppConfig;
use anyhow::{Context, Result};
use tokio::fs;
use tonic::transport::{Certificate, Identity, ServerTlsConfig};

pub async fn load_server_tls_config(config: &AppConfig) -> Result<ServerTlsConfig> {
    let identity = {
        let cert = fs::read(&config.cert_path).await.context("Sunucu sertifikası okunamadı")?;
        let key = fs::read(&config.key_path).await.context("Sunucu anahtarı okunamadı")?;
        Identity::from_pem(cert, key)
    };
    
    let client_ca_cert = {
        let ca = fs::read(&config.ca_path).await.context("CA sertifikası okunamadı")?;
        Certificate::from_pem(ca)
    };

    Ok(ServerTlsConfig::new()
        .identity(identity)
        .client_ca_root(client_ca_cert))
}