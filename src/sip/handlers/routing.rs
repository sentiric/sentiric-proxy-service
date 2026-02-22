// sentiric-proxy-service/src/sip/handlers/routing.rs
use std::sync::Arc;
use tokio::sync::Mutex;
use redis::AsyncCommands;
use std::net::SocketAddr;
use tracing::{debug, warn};

pub type RedisConn = Arc<Mutex<redis::aio::MultiplexedConnection>>;

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

        let mut conn = self.redis.lock().await;
        match conn.get::<_, String>(&target_key).await {
            Ok(target_str) => {
                if let Ok(addr) = target_str.parse::<SocketAddr>() {
                    debug!("➡️ [ROUTING] Redis hedef bulundu: {} -> {}", target_key, addr);
                    return Some(addr);
                }
            },
            Err(_) => {
                warn!("⚠️ [ROUTING] Redis anahtarı bulunamadı: {}", target_key);
            }
        }
        None
    }

    // [HATA 1 ÇÖZÜMÜ A]: Redis'ten istemci IP'sini çeken yardımcı fonksiyon.
    pub async fn get_client_source(&self, call_id: &str) -> Option<SocketAddr> {
        let client_key = format!("proxy:route:{}:client", call_id);
        let mut conn = self.redis.lock().await;
        if let Ok(target_str) = conn.get::<_, String>(&client_key).await {
            return target_str.parse::<SocketAddr>().ok();
        }
        None
    }

    pub async fn register_call_route(&self, call_id: &str, src_addr: SocketAddr, target_addr: SocketAddr) {
        let client_key = format!("proxy:route:{}:client", call_id);
        let target_key = format!("proxy:route:{}:callee", call_id);

        let mut conn = self.redis.lock().await;
        let _: () = conn.set_ex(&client_key, src_addr.to_string(), 300).await.unwrap_or_default();
        let _: () = conn.set_ex(&target_key, target_addr.to_string(), 300).await.unwrap_or_default();
    }

    pub async fn update_dialog_state(&self, call_id: &str, to_tag: &str) {
        if to_tag.is_empty() { return; }

        let old_key = format!("proxy:route:{}:callee", call_id);
        let new_key = format!("proxy:route:{}:{}", call_id, to_tag);
        
        let mut conn = self.redis.lock().await;
        let _: redis::RedisResult<()> = conn.rename(&old_key, &new_key).await;
        let _: () = conn.expire(&new_key, 3600).await.unwrap_or_default();
        
        debug!("💾 [ROUTING] Dialog Tag Güncellendi: {} -> {}", call_id, to_tag);
    }
}