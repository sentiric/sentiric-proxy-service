// sentiric-proxy-service/src/config.rs

use anyhow::{Context, Result};
use std::env;
use std::net::SocketAddr;

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub grpc_listen_addr: SocketAddr,
    pub http_listen_addr: SocketAddr,
    pub sip_bind_ip: String,
    pub sip_port: u16,
    pub proxy_advertised_host: String,
    pub public_ip: String,
    
    // Bağımlı Servisler (gRPC)
    pub registrar_grpc_url: String,
    pub b2bua_grpc_url: String,
    pub dialplan_grpc_url: String,
    
    // SIP Hedefleri
    pub b2bua_sip_addr: String,
    pub registrar_sip_addr: String,
    
    // [YENİ] Teknik Borç Çözümü: İç ağa zorlanacak kullanıcı adları
    pub internal_service_users: Vec<String>,

    pub redis_url: String,
    pub env: String,
    pub rust_log: String,
    pub service_version: String,
    pub cert_path: String,
    pub key_path: String,
    pub ca_path: String,
}

impl AppConfig {
    pub fn load_from_env() -> Result<Self> {
        // ... (Mevcut port ve adres tanımları aynı kalıyor) ...
        let grpc_port = env::var("PROXY_SERVICE_GRPC_PORT").unwrap_or_else(|_| "13071".to_string());
        let http_port = env::var("PROXY_SERVICE_HTTP_PORT").unwrap_or_else(|_| "13070".to_string());
        let sip_port_str = env::var("PROXY_SERVICE_SIP_PORT").unwrap_or_else(|_| "13074".to_string());
        let sip_port = sip_port_str.parse::<u16>().context("Geçersiz SIP portu")?;
        let grpc_addr: SocketAddr = format!("[::]:{}", grpc_port).parse()?;
        let http_addr: SocketAddr = format!("[::]:{}", http_port).parse()?;
        let public_ip = env::var("PUBLIC_IP").unwrap_or_else(|_| "127.0.0.1".to_string());

        // [YENİ] Virgülle ayrılmış listeyi parse et (Örn: "b2bua,operator,ai-agent")
        let internal_users_raw = env::var("CORE_INTERNAL_SERVICE_USERS").unwrap_or_else(|_| "b2bua".to_string());
        let internal_service_users = internal_users_raw
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .collect();

        Ok(AppConfig {
            grpc_listen_addr: grpc_addr,
            http_listen_addr: http_addr, 
            sip_bind_ip: "0.0.0.0".to_string(),
            sip_port,
            proxy_advertised_host: env::var("PROXY_SERVICE_ADVERTISED_HOST").unwrap_or_else(|_| "proxy-service".to_string()),
            public_ip,
            registrar_grpc_url: env::var("REGISTRAR_SERVICE_TARGET_GRPC_URL").unwrap_or_default(),
            b2bua_grpc_url: env::var("B2BUA_SERVICE_TARGET_GRPC_URL").unwrap_or_default(),
            dialplan_grpc_url: env::var("DIALPLAN_SERVICE_TARGET_GRPC_URL").unwrap_or_default(),
            redis_url: env::var("REDIS_URL").context("REDIS_URL eksik")?,
            b2bua_sip_addr: env::var("B2BUA_SERVICE_SIP_TARGET").context("B2BUA_SERVICE_SIP_TARGET eksik")?,
            registrar_sip_addr: env::var("REGISTRAR_SERVICE_SIP_TARGET").unwrap_or_else(|_| "proxy-service:13074".to_string()),
            
            internal_service_users, // [EKLENDİ]

            env: env::var("ENV").unwrap_or_else(|_| "production".to_string()),
            rust_log: env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()),
            service_version: env::var("SERVICE_VERSION").unwrap_or_else(|_| "1.5.1".to_string()),
            cert_path: env::var("PROXY_SERVICE_CERT_PATH").context("CERT PATH eksik")?,
            key_path: env::var("PROXY_SERVICE_KEY_PATH").context("KEY PATH eksik")?,
            ca_path: env::var("GRPC_TLS_CA_PATH").context("CA PATH eksik")?,
        })
    }
}