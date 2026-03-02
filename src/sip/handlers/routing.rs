// src/sip/handlers/routing.rs
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use std::net::SocketAddr;
use tracing::{debug, warn};

pub type RedisConn = ConnectionManager;

pub struct RoutingHandler {
    redis: RedisConn,
}

impl RoutingHandler {
    pub fn new(redis: RedisConn) -> Self {
        Self { redis }
    }

    pub async fn resolve_ack_target(&self, call_id: &str, to_tag: &str) -> Option<SocketAddr> {
        let target_key = if !to_tag.is_empty() {
            format!("proxy:route:{}:{}", call_id, to_tag)
        } else {
            format!("proxy:route:{}:callee", call_id)
        };

        let mut conn = self.redis.clone();
        
        // [DÜZELTME]: Açık tip bildirimi yapıldı (E0282 Hatasını çözer)
        let result: redis::RedisResult<String> = conn.get(&target_key).await;
        
        match result {
            Ok(target_str) => {
                if let Ok(addr) = target_str.parse::<SocketAddr>() {
                    debug!("➡️ [ROUTING] Redis hedef bulundu: {} -> {}", target_key, addr);
                    return Some(addr);
                }
            },
            Err(_) => {
                warn!("⚠️[ROUTING] Redis anahtarı bulunamadı: {}", target_key);
            }
        }
        None
    }

    pub async fn get_client_source(&self, call_id: &str) -> Option<SocketAddr> {
        let client_key = format!("proxy:route:{}:client", call_id);
        let mut conn = self.redis.clone();
        
        //[DÜZELTME]: Açık tip bildirimi yapıldı
        let result: redis::RedisResult<String> = conn.get(&client_key).await;
        if let Ok(target_str) = result {
            return target_str.parse::<SocketAddr>().ok();
        }
        None
    }

    pub async fn register_call_route(&self, call_id: &str, src_addr: SocketAddr, target_addr: SocketAddr) {
        let client_key = format!("proxy:route:{}:client", call_id);
        let target_key = format!("proxy:route:{}:callee", call_id);

        let mut conn = self.redis.clone();
        
        // [DÜZELTME]: unwrap_or_default() yerine RedisResult ile sessiz atama yapıldı
        let _: redis::RedisResult<()> = conn.set_ex(&client_key, src_addr.to_string(), 300).await;
        let _: redis::RedisResult<()> = conn.set_ex(&target_key, target_addr.to_string(), 300).await;
    }

    pub async fn update_dialog_state(&self, call_id: &str, to_tag: &str) {
        if to_tag.is_empty() { return; }

        let old_key = format!("proxy:route:{}:callee", call_id);
        let new_key = format!("proxy:route:{}:{}", call_id, to_tag);
        
        let mut conn = self.redis.clone();
        
        // [DÜZELTME]: Açık tip bildirimi yapıldı
        let _: redis::RedisResult<()> = conn.rename(&old_key, &new_key).await;
        let _: redis::RedisResult<()> = conn.expire(&new_key, 3600).await;
        
        debug!("💾 [ROUTING] Dialog Tag Güncellendi: {} -> {}", call_id, to_tag);
    }
}