// sentiric-proxy-service/src/sip/utils.rs

use sentiric_sip_core::{SipPacket, HeaderName};

pub fn extract_aor(uri: &str) -> String {
    // sip:user@domain:port -> user@domain
    let start = uri.find("sip:").map(|i| i + 4).unwrap_or(0);
    let end = uri.find(';').unwrap_or(uri.len());
    let clean = &uri[start..end];
    
    // Port varsa temizle
    if let Some(colon) = clean.rfind(':') {
        // Eğer @ işaretinden sonra ise porttur (IPv6 hariç basit kontrol)
        if let Some(at) = clean.find('@') {
            if colon > at {
                return clean[..colon].to_string();
            }
        }
    }
    clean.to_string()
}

pub fn get_header(packet: &SipPacket, name: HeaderName) -> String {
    packet.get_header_value(name).cloned().unwrap_or_default()
}