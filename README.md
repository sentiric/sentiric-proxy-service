# 🛡️ Sentiric Proxy Service

[![Status](https://img.shields.io/badge/status-vision-lightgrey.svg)]()
[![Language](https://img.shields.io/badge/language-Rust-orange.svg)]()
[![Protocol](https://img.shields.io/badge/protocol-gRPC_&_UDP-green.svg)]()

**Sentiric Proxy Service**, SIP trafiği için kritik bir yönlendirme ve güvenlik noktasıdır. Bu servis, gelen SIP isteklerini (özellikle `INVITE` ve `REGISTER`) alır, Sinyalleşme katmanına (`sip-signaling` / `registrar-service`) iletir ve yanıtları dış dünyaya yönlendirir.

Bu servis, gelen ve giden SIP mesajlarını inceleyerek yük dengeleme (load balancing) ve topoloji gizleme (topology hiding) gibi işlemleri gerçekleştirir.

## 🎯 Temel Sorumluluklar

1.  **SIP Proxyleme:** Gelen SIP mesajlarını (UDP/TCP), hedef URI'sine göre doğru iç servise yönlendirir.
2.  **Yük Dengeleme (SIP):** Birden fazla `registrar` veya `b2bua` servisi çalıştığında, trafiği sağlıklı olan servislere dağıtır.
3.  **Topology Gizleme:** İç IP adreslerini dış dünyaya sızdırmaz.
4.  **Hata Geri Dönüşü:** İç servislerden biri başarısız olduğunda, standart SIP hata kodlarıyla (örn: `503 Service Unavailable`) dış dünyaya yanıt verir.

## 🛠️ Teknoloji Yığını

*   **Dil:** Rust (Yüksek performanslı ağ I/O için)
*   **Ağ:** Tokio UDP Listener
*   **Servisler Arası İletişim:** gRPC (Tonic)

## 🔌 API Etkileşimleri

*   **Gelen (Sunucu):**
    *   SIP Sağlayıcıları / İstemcileri (UDP/TCP): Ham SIP trafiği.
*   **Giden (İstemci):**
    *   `sentiric-registrar-service` (gRPC): Kayıt (REGISTER) trafiğini işlemek için.
    *   `sentiric-b2bua-service` (gRPC): Çağrı (INVITE) trafiğini başlatmak için.

---
## 🏛️ Anayasal Konum

Bu servis, [Sentiric Anayasası'nın](https://github.com/sentiric/sentiric-governance) **Core Logic Layer**'ında yer alan yeni SIP Protokol Yönetimi bileşenidir.