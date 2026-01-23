// sentiric-proxy-service/src/sip/engine.rs

use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, error, instrument, warn, debug};
use sentiric_sip_core::{SipPacket, Method, HeaderName, Header, utils as sip_core_utils}; 
use sentiric_contracts::sentiric::sip::v1::{RegisterRequest, LookupContactRequest};
use tonic::Request;
use crate::grpc::client::InternalClients;
use crate::sip::utils;
use crate::config::AppConfig;
use tokio::net::lookup_host;
use std::net::SocketAddr;
use uuid::Uuid;

pub struct ProxyEngine {
    clients: Arc<Mutex<InternalClients>>,
    config: Arc<AppConfig>,
}

impl ProxyEngine {
    pub fn new(clients: Arc<Mutex<InternalClients>>, config: Arc<AppConfig>) -> Self {
        Self { clients, config }
    }

    /// Gelen SIP paketlerini işler.
    #[instrument(skip(self, packet), fields(method = %packet.method, is_request = packet.is_request))]
    pub async fn process_packet(&self, packet: &mut SipPacket, src_addr: SocketAddr) -> Option<(SipPacket, Option<SocketAddr>)> {
        
        if packet.is_request {
            match packet.method {
                Method::Register => {
                    if let Some(resp) = self.handle_register(packet, src_addr).await {
                        return Some((resp, None)); // Sender'a cevap dön
                    }
                    None
                },
                Method::Invite => self.handle_invite(packet, src_addr).await,
                _ => self.handle_passthrough_request(packet, src_addr).await, 
            }
        } else {
            self.handle_response(packet).await
        }
    }

    async fn handle_register(&self, packet: &SipPacket, src_addr: SocketAddr) -> Option<SipPacket> {
        let to_header = utils::get_header(packet, HeaderName::To);
        let contact_header = utils::get_header(packet, HeaderName::Contact);
        
        let aor = sip_core_utils::extract_aor(&to_header); 
        
        // --- KRİTİK DÜZELTME: NAT TRAVERSAL (Rport/Received Logic) ---
        // İstemcinin Contact header'ında ne yazdığına bakmaksızın (çoğunlukla yanlış/private IP olur),
        // paketin geldiği gerçek Soket Adresini (src_addr) "Contact URI" olarak kaydediyoruz.
        // Bu, INVITE gönderirken modemin açık tuttuğu doğru NAT portuna gitmemizi sağlar.
        
        // Contact header'dan kullanıcı adını (örn: "1001") ayıklamaya çalış, yoksa AOR'dan al.
        let username = if let Some(user) = self.extract_username_from_uri(&contact_header) {
            user
        } else {
            self.extract_username_from_uri(&aor).unwrap_or("unknown".to_string())
        };

        // Gerçek erişilebilir adres (Örn: 1001@188.119.23.175:45212)
        // Eğer transport tcp ise belirtmek gerekebilir ama şimdilik UDP varsayıyoruz.
        let real_contact_uri = format!("{}@{}:{}", username, src_addr.ip(), src_addr.port());

        info!("REGISTER NAT Fix: Claimed='{}' -> Registered='{}'", contact_header, real_contact_uri);

        let mut clients = self.clients.lock().await;
        
        let req = Request::new(RegisterRequest {
            sip_uri: aor,
            contact_uri: real_contact_uri, // Düzeltilmiş adres
            expires: 3600, 
        });

        match clients.registrar.register(req).await {
            Ok(_) => {
                info!("Registrar: Kayıt başarılı.");
                Some(self.create_response(packet, 200, "OK"))
            }
            Err(e) => {
                error!("Registrar Hatası: {}", e);
                Some(self.create_response(packet, 500, "Internal Server Error"))
            }
        }
    }

    async fn handle_invite(&self, packet: &mut SipPacket, src_addr: SocketAddr) -> Option<(SipPacket, Option<SocketAddr>)> {
        let b2bua_hostname = self.config.b2bua_sip_addr.split(':').next().unwrap_or("");
        
        // Loop Koruması: Kaynak IP bizim B2BUA mı?
        let is_from_b2bua = if let Ok(mut addrs) = lookup_host(b2bua_hostname).await {
            addrs.any(|a| a.ip() == src_addr.ip())
        } else {
            false
        };

        if is_from_b2bua {
            // --- OUTBOUND (B2BUA -> User) ---
            if let Some(target_addr) = self.extract_target_addr(&packet.uri) {
                info!("🔄 Outbound INVITE Routing: B2BUA -> {}", target_addr);
                self.add_via_header(packet);
                return Some((packet.clone(), Some(target_addr)));
            } else {
                error!("❌ Outbound INVITE hedef adresi çözülemedi: {}", packet.uri);
                return None;
            }
        } else {
            // --- INBOUND (User -> System) ---
            let from = utils::get_header(packet, HeaderName::From);
            let to = utils::get_header(packet, HeaderName::To);
            let target_aor = sip_core_utils::extract_aor(&to);
            
            info!("➡️ Inbound INVITE: {} -> {}", from, to);

            // 1. Dahili Abone Kontrolü
            let lookup_result = {
                let mut clients = self.clients.lock().await;
                let req = Request::new(LookupContactRequest {
                    sip_uri: target_aor.clone(),
                });
                clients.registrar.lookup_contact(req).await
            };

            match lookup_result {
                Ok(response) => {
                    let contacts = response.into_inner().contact_uris;
                    if !contacts.is_empty() {
                        let contact_uri = &contacts[0];
                        
                        // Loop Koruması: Hedef adres kaynak adresle aynı mı?
                        if let Some(target_addr) = self.extract_target_addr(contact_uri) {
                             if target_addr == src_addr {
                                 warn!("⚠️ Loop Detected: Hedef ({}) ile Kaynak ({}) aynı. Çağrı reddediliyor.", target_addr, src_addr);
                                 return Some((self.create_response(packet, 482, "Loop Detected"), None));
                             }
                             
                             info!("✅ Dahili Abone (NAT Çözümlü): {} -> {}", target_aor, target_addr);
                             self.add_via_header(packet);
                             return Some((packet.clone(), Some(target_addr)));
                        }
                    }
                },
                Err(e) => {
                    warn!("Registrar Lookup hatası (B2BUA'ya fallback): {}", e);
                }
            }

            // 2. Varsayılan Rota (AI / B2BUA)
            info!("🤖 AI Routing: {} -> B2BUA", target_aor);
            if let Ok(mut addrs) = lookup_host(&self.config.b2bua_sip_addr).await {
                if let Some(target) = addrs.next() {
                    self.add_via_header(packet);
                    return Some((packet.clone(), Some(target)));
                }
            }

            error!("CRITICAL: B2BUA adresi '{}' çözümlenemedi.", self.config.b2bua_sip_addr);
            return Some((self.create_response(packet, 503, "Service Unavailable"), None));
        }
    }

    async fn handle_passthrough_request(&self, packet: &mut SipPacket, _src_addr: SocketAddr) -> Option<(SipPacket, Option<SocketAddr>)> {
        // Basit routing (ACK, BYE)
        let user_agent = utils::get_header(packet, HeaderName::UserAgent);
        
        if user_agent.contains("Sentiric B2BUA") {
             if let Some(target_addr) = self.extract_target_addr(&packet.uri) {
                 self.add_via_header(packet);
                 return Some((packet.clone(), Some(target_addr)));
             }
        } else {
             if let Ok(mut addrs) = lookup_host(&self.config.b2bua_sip_addr).await {
                 if let Some(target) = addrs.next() {
                     self.add_via_header(packet);
                     return Some((packet.clone(), Some(target)));
                 }
             }
        }
        None
    }

    async fn handle_response(&self, packet: &mut SipPacket) -> Option<(SipPacket, Option<SocketAddr>)> {
        let status = packet.status_code;

        // 1. Kendi Via'mızı çıkar
        if !packet.headers.is_empty() && packet.headers[0].name == HeaderName::Via {
            packet.headers.remove(0);
        } else {
            return None; // Via yoksa rotalayamazsın
        }

        // 2. Bir sonraki Via'ya (Kaynak) dön
        if let Some(client_via) = packet.headers.iter().find(|h| h.name == HeaderName::Via) {
            if let Some(target) = self.parse_via_address(&client_via.value) {
                
                // DÜZELTME: Call-ID loglaması eklenebilir
                debug!("↩️ Routing Response ({}) to: {}", status, target);
                return Some((packet.clone(), Some(target)));
            }
        }

        None
    }

    fn add_via_header(&self, packet: &mut SipPacket) {
        let branch = format!("z9hG4bK-proxy-{}", Uuid::new_v4());
        let via_val = format!("SIP/2.0/UDP {}:{};branch={}", 
            "proxy-service", 
            self.config.sip_port,
            branch
        );
        packet.headers.insert(0, Header::new(HeaderName::Via, via_val));
    }

    fn create_response(&self, req: &SipPacket, code: u16, reason: &str) -> SipPacket {
        let mut resp = SipPacket::new_response(code, reason.to_string());
        // Kritik Headerları kopyala
        for h in &req.headers {
            match h.name {
                HeaderName::Via | HeaderName::From | HeaderName::To | HeaderName::CallId | HeaderName::CSeq => {
                    resp.headers.push(h.clone());
                },
                _ => {}
            }
        }
        resp.headers.push(Header::new(HeaderName::Server, "Sentiric Proxy".to_string()));
        resp.headers.push(Header::new(HeaderName::ContentLength, "0".to_string()));
        resp
    }

    fn extract_username_from_uri(&self, uri: &str) -> Option<String> {
        // "sip:1001@..." veya "<sip:1001@...>" formatından 1001'i al
        let clean = uri.trim_start_matches('<').trim_start_matches("sip:");
        if let Some(at_idx) = clean.find('@') {
            return Some(clean[..at_idx].to_string());
        }
        None
    }

    fn extract_target_addr(&self, uri: &str) -> Option<SocketAddr> {
        let clean = uri.trim_start_matches("sip:").trim_start_matches('<').trim_end_matches('>');
        
        // Host:Port kısmını al (user@ kısmını at)
        let host_port_part = if let Some(at_idx) = clean.find('@') {
            &clean[at_idx+1..]
        } else {
            clean
        };
        
        // Parametreleri at (;transport=udp gibi)
        let host_port = if let Some(semi_idx) = host_port_part.find(';') {
            &host_port_part[..semi_idx]
        } else {
            host_port_part
        };
        
        if !host_port.contains(':') {
             format!("{}:5060", host_port).parse().ok()
        } else {
             host_port.parse().ok()
        }
    }

    fn parse_via_address(&self, via_val: &str) -> Option<SocketAddr> {
        // Via: SIP/2.0/UDP 1.2.3.4:5060;received=1.2.3.4;rport=4567
        let parts: Vec<&str> = via_val.split_whitespace().collect();
        if parts.len() < 2 { return None; }
        
        let protocol_part = parts[1]; 
        let params: Vec<&str> = protocol_part.split(';').collect();
        let mut host_port = params[0].to_string(); // Varsayılan host:port
        
        let mut rport: Option<String> = None;
        let mut received: Option<String> = None;

        for param in &params[1..] {
            if let Some((k, v)) = param.split_once('=') {
                if k == "received" { received = Some(v.to_string()); }
                if k == "rport" { rport = Some(v.to_string()); }
            }
        }

        // Eğer rport ve received varsa, onları kullan (NAT gerçeği)
        if let (Some(r), Some(rec)) = (rport, received) {
            return format!("{}:{}", rec, r).parse().ok();
        }

        // Yoksa header'daki host:port'u parse et
        if !host_port.contains(':') {
             host_port = format!("{}:5060", host_port);
        }
        host_port.parse().ok()
    }
}