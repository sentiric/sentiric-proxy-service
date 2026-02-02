// sentiric-proxy-service/src/sip/engine.rs

use crate::config::AppConfig;
use crate::grpc::client::InternalClients;
use crate::sip::server::{ProxyState, DEFAULT_SIP_PORT};
use crate::sip::utils;
use sentiric_contracts::sentiric::sip::v1::RegisterRequest;
use sentiric_sip_core::{
    utils as sip_core_utils, 
    Header, HeaderName, Method, SipPacket,
    SipRouter 
};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tonic::Request;
use tracing::{error, info, instrument, debug, warn};
use redis::AsyncCommands;
use rand; // Cargo.toml'da rand = "0.8" olmalı

pub type RedisConn = Arc<Mutex<redis::aio::MultiplexedConnection>>;

pub struct ProxyEngine {
    clients: Arc<Mutex<InternalClients>>,
    config: Arc<AppConfig>,
    state: Arc<ProxyState>,
    redis: RedisConn,
}

impl ProxyEngine {
    pub fn new(
        clients: Arc<Mutex<InternalClients>>,
        config: Arc<AppConfig>,
        state: Arc<ProxyState>,
        redis: RedisConn,
    ) -> Self {
        Self { clients, config, state, redis }
    }

    /// Ana Paket İşleme Döngüsü
    /// Gelen paketi analiz eder, NAT düzeltmesi yapar ve İstek/Yanıt ayrımına gider.
    #[instrument(skip(self, packet), fields(method = %packet.method, call_id = %utils::get_header(packet, HeaderName::CallId)))]
    pub async fn process_packet(
        &self,
        packet: &mut SipPacket,
        src_addr: SocketAddr,
    ) -> Option<(SipPacket, Option<SocketAddr>)> {
        // 1. NAT Düzeltmesi (SipRouter - Core)
        // Gelen paketin Via başlığına "received" ve "rport" parametrelerini işler.
        if packet.is_request {
            SipRouter::fix_nat_via(packet, src_addr);
        }

        debug!("📦 [PROXY] Paket İşleniyor. Yön: {}", if packet.is_request { "REQUEST" } else { "RESPONSE" });

        if packet.is_request {
            self.handle_request(packet, src_addr).await
        } else {
            self.handle_response(packet).await
        }
    }

    /// İstek (Request) İşleme Mantığı
    async fn handle_request(&self, packet: &mut SipPacket, src_addr: SocketAddr) -> Option<(SipPacket, Option<SocketAddr>)> {
        // A. REGISTER İstekleri -> Registrar Servisine (gRPC)
        if packet.method == Method::Register {
            return self.handle_register(packet, src_addr).await;
        }

        // B. Dialog İçi İstekler (ACK, BYE, Re-INVITE) -> Loose Routing
        // Eğer Route header varsa, RFC 3261 Loose Routing kurallarına göre hareket et.
        let has_route_header = packet.headers.iter().any(|h| h.name == HeaderName::Route);
        if has_route_header {
            return self.handle_loose_routing(packet).await;
        }

        // C. ACK İstekleri (Route Header yoksa) -> Redis'ten hedef bul
        // ACK, bir transaction başlatmaz ama yönlendirilmesi gerekir.
        if packet.method == Method::Ack {
             let call_id = utils::get_header(packet, HeaderName::CallId);
             let to_tag = self.extract_tag_from_header(&utils::get_header(packet, HeaderName::To));
             
             // Redis anahtarı: proxy:route:{call_id}:{to_tag}
             // Eğer tag yoksa "callee" placeholder'ına bak.
             let target_key = if !to_tag.is_empty() {
                 format!("proxy:route:{}:{}", call_id, to_tag)
             } else {
                 format!("proxy:route:{}:callee", call_id)
             };

             let mut conn = self.redis.lock().await;
             if let Ok(target_str) = conn.get::<_, String>(&target_key).await {
                 if let Ok(target_addr) = target_str.parse::<SocketAddr>() {
                     debug!("➡️ [ACK] Redis'ten hedef bulundu: {}", target_addr);
                     return Some((packet.clone(), Some(target_addr)));
                 }
             }
             warn!("⚠️ [ACK] Hedef bulunamadı, paket düşürülüyor.");
             return None;
        }
        
        // D. İlk İstekler (Initial INVITE) -> B2BUA'ya Yönlendir (Default AI Gateway)
        // Proxy burada karar vermez, trafiği B2BUA'ya yıkar.
        let target_host = &self.config.b2bua_sip_addr;
        
        match self.state.resolve_b2bua_addr(target_host).await {
            Ok(target_addr) => {
                info!(target = %target_addr, "➡️ [INVITE] Yeni çağrı B2BUA'ya (AI Gateway) yönlendiriliyor.");

                // 1. Record-Route Ekle (Yolun bizden geçmesini garanti et)
                SipRouter::add_record_route(packet, &self.config.proxy_advertised_host, self.config.sip_port);
                
                // 2. Via Ekle (B2BUA yanıtı bize dönsün)
                SipRouter::add_via(packet, &self.config.proxy_advertised_host, self.config.sip_port, "UDP");

                // 3. Durumu Redis'e Kaydet (Stateful Proxy)
                // Yanıt geldiğinde veya ACK geldiğinde kimin kim olduğunu bilmemiz lazım.
                let call_id = utils::get_header(packet, HeaderName::CallId);
                
                // İstemci tarafı (Caller) -> Yanıt buraya dönecek
                // Anahtar: proxy:route:{call_id}:client_via
                let client_key = format!("proxy:route:{}:client", call_id);
                
                // Hedef tarafı (Callee/B2BUA) -> İstek buraya gitti
                // Anahtar: proxy:route:{call_id}:callee
                let target_key = format!("proxy:route:{}:callee", call_id);

                let mut conn = self.redis.lock().await;
                // 300 saniye (5 dakika) TTL yeterlidir
                let _: () = conn.set_ex(&client_key, src_addr.to_string(), 300).await.unwrap_or_default();
                let _: () = conn.set_ex(&target_key, target_addr.to_string(), 300).await.unwrap_or_default();
        
                Some((packet.clone(), Some(target_addr)))
            }
            Err(e) => {
                error!("❌ B2BUA adresi çözümlenemedi (DNS/Config Hatası): {}: {}", target_host, e);
                Some((self.create_response(packet, 503, "Service Unavailable"), Some(src_addr)))
            }
        }
    }

    /// REGISTER İşleme Mantığı (SIP -> gRPC)
    async fn handle_register(&self, packet: &SipPacket, src_addr: SocketAddr) -> Option<(SipPacket, Option<SocketAddr>)> {
        let to_header = utils::get_header(packet, HeaderName::To);
        let aor = sip_core_utils::extract_aor(&to_header);
        let username = sip_core_utils::extract_username_from_uri(&aor);
        
        // İstemcinin gerçek adresini (NAT arkası dahil) Via'dan çöz.
        // Core kütüphanesi 'received' ve 'rport' parametrelerini zaten eklemişti.
        let via_val = utils::get_header(packet, HeaderName::Via);
        let client_addr = SipRouter::resolve_response_target(&via_val, DEFAULT_SIP_PORT).unwrap_or(src_addr);
        
        // Registrar servisine kaydedilecek gerçek Contact URI
        // Örn: sip:1001@88.234.12.1:45322
        let real_contact_uri = format!("sip:{}@{}:{}", username, client_addr.ip(), client_addr.port());

        info!("📝 [REGISTER] Kullanıcı: {}, Adres: {}", username, real_contact_uri);

        let mut clients = self.clients.lock().await;
        // Expires header'ını kontrol et, yoksa varsayılan 3600
        let expires_str = utils::get_header(packet, HeaderName::Other("Expires".to_string()));
        let expires = expires_str.parse::<i32>().unwrap_or(3600);

        let req = Request::new(RegisterRequest { 
            sip_uri: aor.clone(), 
            contact_uri: real_contact_uri, 
            expires 
        });

        match clients.registrar.register(req).await {
            Ok(_) => {
                let mut resp = self.create_response(packet, 200, "OK");
                
                // Uyumluluk: Contact başlığını güncelle (Aynala)
                if let Some(contact) = packet.get_header_value(HeaderName::Contact) {
                    resp.headers.retain(|h| h.name != HeaderName::Contact);
                    resp.headers.push(Header::new(HeaderName::Contact, contact.clone()));
                }
                
                // To headerına Tag ekle (RFC kuralı)
                if let Some(to_h) = resp.headers.iter_mut().find(|h| h.name == HeaderName::To) {
                    if !to_h.value.contains(";tag=") {
                        let tag = format!("{:x}", rand::random::<u32>());
                        to_h.value.push_str(&format!(";tag={}", tag));
                    }
                }

                resp.headers.push(Header::new(HeaderName::Other("Expires".to_string()), expires.to_string()));
                let now = chrono::Utc::now().to_rfc2822().replace("+0000", "GMT");
                resp.headers.push(Header::new(HeaderName::Other("Date".to_string()), now));

                info!(user = %username, "✅ REGISTER: Başarılı.");
                Some((resp, Some(src_addr)))
            },
            Err(e) => {
                error!("❌ Registrar Servis Hatası: {}", e);
                Some((self.create_response(packet, 500, "Internal Server Error"), Some(src_addr)))
            }
        }
    }

    /// Loose Routing (Route Header Varsa)
    async fn handle_loose_routing(&self, packet: &mut SipPacket) -> Option<(SipPacket, Option<SocketAddr>)> {
        // En üstteki Route başlığı bize aitse (lr parametresi varsa), onu çıkar ve bir sonrakine git.
        // Bu, Proxy'nin döngüye girmesini engeller.
        
        if let Some(route_header) = packet.headers.iter().find(|h| h.name == HeaderName::Route) {
             // Hedefi Route başlığından çöz
             if let Some(target_addr) = sip_core_utils::extract_socket_addr(&route_header.value) {
                 debug!("🔄 [LOOSE ROUTING] Route Header Bulundu: {} -> {}", route_header.value, target_addr);
                 
                 // İlk Route'u sil (Biz işledik)
                 packet.headers.remove(0); 
                 
                 // Kendimizi tekrar Via'ya ekle (Yanıtın dönmesi için)
                 SipRouter::add_via(packet, &self.config.proxy_advertised_host, self.config.sip_port, "UDP");
                 
                 return Some((packet.clone(), Some(target_addr)));
             }
        }
        
        // Route header var ama çözülemediyse (Bozuk paket veya saldırı)
        warn!("⚠️ [LOOSE ROUTING] Route header çözülemedi. Paket düşürülüyor.");
        None
    }
    
    /// Yanıt (Response) İşleme Mantığı
    async fn handle_response(&self, packet: &mut SipPacket) -> Option<(SipPacket, Option<SocketAddr>)> {
        // 1. Via Sıyırma (Strip Top Via)
        // En üstteki Via başlığı bize ait olmalı. Onu çıkarıyoruz ki paket bir önceki hop'a dönsün.
        if SipRouter::strip_top_via(packet).is_none() {
            warn!("⚠️ Response paketinde Via başlığı yok veya silinemedi (Bizim değil mi?).");
            return None;
        }

        // 2. Redis Güncelleme (Dialog State)
        // Eğer bu bir 200 OK yanıtı ise ve To-Tag içeriyorsa, bu tag'i kaydetmeliyiz.
        // Böylece gelecekteki ACK ve BYE istekleri doğru hedefe yönlendirilebilir.
        if packet.status_code >= 200 && packet.status_code < 300 {
            let call_id = utils::get_header(packet, HeaderName::CallId);
            let to_tag = self.extract_tag_from_header(&utils::get_header(packet, HeaderName::To));
            
            if !to_tag.is_empty() {
                // "callee" placeholder'ını gerçek tag ile güncelle
                let old_key = format!("proxy:route:{}:callee", call_id);
                let new_key = format!("proxy:route:{}:{}", call_id, to_tag);
                
                let mut conn = self.redis.lock().await;
                // Değer (Hedef IP) aynı kalır, sadece anahtar değişir (Rename)
                // Eğer hata verirse (key yoksa) çok önemli değil, yeni set yaparız.
                let _: redis::RedisResult<()> = conn.rename(&old_key, &new_key).await;
                // TTL güncelle
                let _: () = conn.expire(&new_key, 3600).await.unwrap_or_default();
                
                debug!("💾 [STATE] Dialog Tag Kaydedildi: {} -> {}", call_id, to_tag);
            }
        }
        
        // 3. Hedef Belirleme (Next Via)
        // Bir sonraki Via başlığı, yanıtı bekleyen istemcinin adresidir.
        if let Some(next_via) = packet.headers.iter().find(|h| h.name == HeaderName::Via) {
            if let Some(target) = SipRouter::resolve_response_target(&next_via.value, DEFAULT_SIP_PORT) {
                debug!("↩️ [RESPONSE] Yanıt bir önceki hop'a iletiliyor: {}", target);
                return Some((packet.clone(), Some(target)));
            }
        }
        
        warn!("⚠️ Yanıtın gönderileceği hedef (Next Via) çözülemedi. Paket düşürülüyor.");
        None
    }

    /// Hata Yanıtı Oluşturucu
    fn create_response(&self, req: &SipPacket, code: u16, reason: &str) -> SipPacket {
        let mut resp = SipPacket::create_response_for(req, code, reason.to_string());
        resp.headers.push(Header::new(HeaderName::Server, "Sentiric-Proxy/1.3.1".to_string()));
        resp.headers.push(Header::new(HeaderName::ContentLength, "0".to_string()));
        resp
    }

    /// SIP Header'dan Tag Parametresini Çıkarır
    fn extract_tag_from_header(&self, header_val: &str) -> String {
        if let Some(tag_start) = header_val.find(";tag=") {
            let rest = &header_val[tag_start + 5..];
            if let Some(tag_end) = rest.find(';') {
                return rest[..tag_end].to_string();
            }
            return rest.to_string();
        }
        String::new()
    }
}