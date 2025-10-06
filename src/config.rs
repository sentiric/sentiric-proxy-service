// sentiric-proxy-service/src/config.rs
use anyhow::{Context, Result};
use std::env;
use std::net::SocketAddr;

#[derive(Debug)]
pub struct AppConfig {
    pub grpc_listen_addr: SocketAddr,
    pub http_listen_addr: SocketAddr,
    pub sip_listen_addr: SocketAddr, // SIP UDP dinleme adresi
    
    pub env: String,
    pub rust_log: String,
    pub service_version: String,
    
    // TLS Yolları
    pub cert_path: String,
    pub key_path: String,
    pub ca_path: String,
    
    // Bağımlılıklar (Şimdilik yer tutucu)
    pub registrar_service_url: String,
    pub b2bua_service_url: String,
}

impl AppConfig {
    pub fn load_from_env() -> Result<Self> {
        let grpc_port = env::var("PROXY_SERVICE_GRPC_PORT").unwrap_or_else(|_| "12071".to_string());
        let http_port = env::var("PROXY_SERVICE_HTTP_PORT").unwrap_or_else(|_| "12070".to_string());
        let sip_port = env::var("PROXY_SERVICE_SIP_PORT").unwrap_or_else(|_| "5060".to_string());
        
        let grpc_addr: SocketAddr = format!("[::]:{}", grpc_port).parse()?;
        let http_addr: SocketAddr = format!("[::]:{}", http_port).parse()?;
        let sip_addr: SocketAddr = format!("0.0.0.0:{}", sip_port).parse()?;
            
        Ok(AppConfig {
            grpc_listen_addr: grpc_addr,
            http_listen_addr: http_addr, 
            sip_listen_addr: sip_addr,

            registrar_service_url: env::var("REGISTRAR_SERVICE_TARGET_GRPC_URL").unwrap_or_else(|_| "registrar-service:12061".to_string()),
            b2bua_service_url: env::var("B2BUA_SERVICE_TARGET_GRPC_URL").unwrap_or_else(|_| "b2bua-service:12081".to_string()),
            
            env: env::var("ENV").unwrap_or_else(|_| "production".to_string()),
            rust_log: env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()),
            service_version: env::var("SERVICE_VERSION").unwrap_or_else(|_| "0.1.0".to_string()),
            
            // TODO: Bu yollar config repo'da PROXY_SERVICE olarak güncellenmelidir.
            cert_path: env::var("PROXY_SERVICE_CERT_PATH").context("ZORUNLU: PROXY_SERVICE_CERT_PATH eksik")?,
            key_path: env::var("PROXY_SERVICE_KEY_PATH").context("ZORUNLU: PROXY_SERVICE_KEY_PATH eksik")?,
            ca_path: env::var("GRPC_TLS_CA_PATH").context("ZORUNLU: GRPC_TLS_CA_PATH eksik")?,
        })
    }
}