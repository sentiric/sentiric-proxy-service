// sentiric-proxy-service/src/config.rs
use anyhow::{Context, Result};
use std::env;
use std::net::SocketAddr;

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub grpc_listen_addr: SocketAddr,
    pub http_listen_addr: SocketAddr,
    
    // SIP Network
    pub sip_bind_ip: String,
    pub sip_port: u16,
    pub proxy_advertised_host: String,
    
    // Internal Service Targets (gRPC)
    pub registrar_grpc_url: String,
    pub b2bua_grpc_url: String,
    pub dialplan_grpc_url: String,

    // [YENİ] Routing Targets (SIP Forwarding Destinations)
    pub b2bua_sip_addr: String,     // INVITE'lar için
    pub registrar_sip_addr: String, // REGISTER'lar için (Genellikle Proxy'nin kendisi)
    
    pub env: String,
    pub rust_log: String,
    pub service_version: String,
    
    // TLS
    pub cert_path: String,
    pub key_path: String,
    pub ca_path: String,
}

impl AppConfig {
    pub fn load_from_env() -> Result<Self> {
        let grpc_port = env::var("PROXY_SERVICE_GRPC_PORT").unwrap_or_else(|_| "13071".to_string());
        let http_port = env::var("PROXY_SERVICE_HTTP_PORT").unwrap_or_else(|_| "13070".to_string());
        let sip_port_str = env::var("PROXY_SERVICE_SIP_PORT").unwrap_or_else(|_| "13074".to_string());
        let sip_port = sip_port_str.parse::<u16>().context("Geçersiz SIP portu")?;
        
        let grpc_addr: SocketAddr = format!("[::]:{}", grpc_port).parse()?;
        let http_addr: SocketAddr = format!("[::]:{}", http_port).parse()?;
        
        Ok(AppConfig {
            grpc_listen_addr: grpc_addr,
            http_listen_addr: http_addr, 
            
            sip_bind_ip: "0.0.0.0".to_string(),
            sip_port,
            proxy_advertised_host: env::var("PROXY_SERVICE_ADVERTISED_HOST")
                .unwrap_or_else(|_| "proxy-service".to_string()),

            registrar_grpc_url: env::var("REGISTRAR_SERVICE_TARGET_GRPC_URL")
                .unwrap_or_else(|_| "https://registrar-service:13061".to_string()),
            b2bua_grpc_url: env::var("B2BUA_SERVICE_TARGET_GRPC_URL")
                .unwrap_or_else(|_| "https://b2bua-service:13081".to_string()),
            dialplan_grpc_url: env::var("DIALPLAN_SERVICE_TARGET_GRPC_URL")
                .unwrap_or_else(|_| "https://dialplan-service:12021".to_string()),
            
            // [HEDEFLERİ OKU]
            b2bua_sip_addr: env::var("B2BUA_SERVICE_SIP_TARGET")
                .context("ZORUNLU: B2BUA_SERVICE_SIP_TARGET eksik")?,
                
            // Varsayılan olarak Proxy'nin kendi SIP portunu kullanır (13074)
            registrar_sip_addr: env::var("REGISTRAR_SERVICE_SIP_TARGET")
                .unwrap_or_else(|_| "proxy-service:13074".to_string()),
            
            env: env::var("ENV").unwrap_or_else(|_| "production".to_string()),
            rust_log: env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()),
            service_version: env::var("SERVICE_VERSION").unwrap_or_else(|_| "1.1.0".to_string()),
            
            cert_path: env::var("PROXY_SERVICE_CERT_PATH").context("ZORUNLU: PROXY_SERVICE_CERT_PATH eksik")?,
            key_path: env::var("PROXY_SERVICE_KEY_PATH").context("ZORUNLU: PROXY_SERVICE_KEY_PATH eksik")?,
            ca_path: env::var("GRPC_TLS_CA_PATH").context("ZORUNLU: GRPC_TLS_CA_PATH eksik")?,
        })
    }
}