# 🛡️ Sentiric Proxy Service - Mantık ve Akış Mimarisi

**Stratejik Rol:** SIP trafiğini alarak uygun iç servise yönlendiren ve yük dengeleme yapan L4/L7 proxy.

---

## 1. Yönlendirme Akışı: REGISTER İsteği

```mermaid
sequenceDiagram
    participant Softphone
    participant Proxy as Proxy Service
    participant Registrar as Registrar Service

    Softphone->>Proxy: REGISTER (Ham UDP Paketi)
    Note over Proxy: Paketi ayrıştırır ve hedef SIP URI'yi (AOR) kontrol eder.
    Proxy->>Registrar: Register(sip_message, src_ip) (gRPC)
    Registrar-->>Proxy: RegisterResponse (200 OK / 401 Unauthorized)
    Proxy-->>Softphone: 200 OK / 401 Unauthorized (Ham UDP Paketi)
```

## 2. Yönlendirme Akışı: INVITE (Çağrı Kurulumu)

```mermaid
sequenceDiagram
    participant Softphone
    participant Proxy as Proxy Service
    participant B2BUA as B2BUA Service

    Softphone->>Proxy: INVITE (Ham UDP Paketi)
    Note over Proxy: Paketi ayrıştırır, NAT/Topoloji temizliği yapar.
    Proxy->>B2BUA: InitiateCall(sip_message, ...) (gRPC)
    B2BUA-->>Proxy: InitiateCallResponse (180 Ringing / 200 OK / Hata)
    Proxy-->>Softphone: 200 OK (Ham UDP Paketi)
```
