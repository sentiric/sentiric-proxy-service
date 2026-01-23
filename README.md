# 🛡️ Sentiric Proxy Service

[![Status](https://img.shields.io/badge/status-active-success.svg)]()
[![Language](https://img.shields.io/badge/language-Rust-orange.svg)]()
[![Protocol](https://img.shields.io/badge/protocol-gRPC_&_UDP-green.svg)]()

**Sentiric Proxy Service**, SIP trafiği için kritik bir yönlendirme ve güvenlik noktasıdır. Gelen SIP isteklerini (özellikle `INVITE` ve `REGISTER`) alır, analiz eder ve **Dahili Abone** veya **Yapay Zeka (AI)** ayrımını yaparak doğru hedefe yönlendirir.

Bu servis, sistemin "Santral Memuru" gibi çalışır.

## 🎯 Temel Sorumluluklar

1.  **Akıllı Yönlendirme (Smart Routing):**
    *   **Dahili Çağrılar:** Eğer aranan numara sistemde kayıtlı bir SIP abonesi ise (`registrar-service` sorgusu ile), çağrıyı doğrudan o aboneye bağlar (P2P/Internal).
    *   **AI Çağrıları:** Eğer aranan numara bir abone değilse (veya dış hat ise), çağrıyı işlenmesi için `b2bua-service`'e (AI Orkestratörü) yönlendirir.
2.  **SIP Proxyleme:** Gelen SIP mesajlarını (UDP/TCP) bozmadan iletir.
3.  **Topology Gizleme:** İç IP adreslerini dış dünyaya sızdırmaz (`Via` ve `Record-Route` manipülasyonu).
4.  **Yük Dengeleme:** Birden fazla `registrar` veya `b2bua` servisi çalıştığında trafiği dağıtır.

## 🛠️ Teknoloji Yığını

*   **Dil:** Rust (Yüksek performanslı ağ I/O için)
*   **Ağ:** Tokio UDP Listener
*   **Servisler Arası İletişim:** gRPC (Tonic)

## 🔌 API Etkileşimleri

*   **Gelen (Sunucu):**
    *   SIP İstemcileri (Softphone/SBC): Ham SIP trafiği.
*   **Giden (İstemci):**
    *   `sentiric-registrar-service` (gRPC): Kayıt (`REGISTER`) trafiği ve Abone Sorgulama (`Lookup`) için.
    *   `sentiric-b2bua-service` (gRPC/SIP): AI tabanlı çağrıları başlatmak için.

---
## 🏛️ Anayasal Konum

Bu servis, [Sentiric Anayasası'nın](https://github.com/sentiric/sentiric-governance) **Core Logic Layer**'ında yer alan yeni SIP Protokol Yönetimi bileşenidir.


---
