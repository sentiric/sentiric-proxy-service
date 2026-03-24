// Dosya: sentiric-sip-proxy-service/src/sip/handlers/routing.rs
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use std::net::SocketAddr;
use tracing::{debug, warn}; // unused debug uyarısı giderildi, kullanılıyor.

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
        let result: redis::RedisResult<String> = conn.get(&target_key).await;
        
        match result {
            Ok(target_str) => {
                if let Ok(addr) = target_str.parse::<SocketAddr>() {
                    return Some(addr);
                }
            },
            Err(_) => { warn!("⚠️[ROUTING] Redis anahtarı bulunamadı: {}", target_key); }
        }
        None
    }

    pub async fn get_client_source(&self, call_id: &str) -> Option<SocketAddr> {
        //[DÜZELTME]: Tutarlılık için `caller` kullanılıyor.
        let client_key = format!("proxy:route:{}:caller", call_id);
        let mut conn = self.redis.clone();
        
        let result: redis::RedisResult<String> = conn.get(&client_key).await;
        if let Ok(target_str) = result {
            return target_str.parse::<SocketAddr>().ok();
        }
        None
    }

    pub async fn register_call_route(&self, call_id: &str, src_addr: SocketAddr, target_addr: SocketAddr) {
        let caller_key = format!("proxy:route:{}:caller", call_id);
        let callee_key = format!("proxy:route:{}:callee", call_id);

        let mut conn = self.redis.clone();
        let _: redis::RedisResult<()> = conn.set_ex(&caller_key, src_addr.to_string(), 3600).await;
        let _: redis::RedisResult<()> = conn.set_ex(&callee_key, target_addr.to_string(), 3600).await;
    }

    // [YENİ] In-Dialog İki Yönlü P2P Rota Çözücü
    pub async fn resolve_in_dialog_target(&self, call_id: &str, real_src_addr: SocketAddr) -> Option<SocketAddr> {
        let caller_key = format!("proxy:route:{}:caller", call_id);
        let callee_key = format!("proxy:route:{}:callee", call_id);
        let mut conn = self.redis.clone();
        
        let caller_str: redis::RedisResult<String> = conn.get(&caller_key).await;
        let callee_str: redis::RedisResult<String> = conn.get(&callee_key).await;
        
        let caller_addr = caller_str.ok().and_then(|s| s.parse::<SocketAddr>().ok());
        let callee_addr = callee_str.ok().and_then(|s| s.parse::<SocketAddr>().ok());

        if let (Some(c_er), Some(c_ee)) = (caller_addr, callee_addr) {
            debug!(event="P2P_ROUTE_LOOKUP", sip.call_id=%call_id, src=%real_src_addr, caller=%c_er, callee=%c_ee, "Redis çift yönlü eşleşme yapılıyor");
            // İstek "Aranan (Callee)" taraftan geldiyse "Arayan (Caller)" tarafına gönder. Aksi halde tam tersi.
            if real_src_addr.ip() == c_ee.ip() {
                return Some(c_er);
            } else {
                return Some(c_ee);
            }
        }
        
        callee_addr
    }
}