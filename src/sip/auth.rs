// Dosya: sentiric-sip-proxy-service/src/sip/auth.rs

use std::collections::HashMap;

pub struct DigestAuth {
    pub username: String,
    pub realm: String,
    pub nonce: String,
    pub uri: String,
    pub response: String,
}

impl DigestAuth {
    pub fn parse(auth_header: &str) -> Option<Self> {
        let header_clean = auth_header.trim();
        if !header_clean.to_lowercase().starts_with("digest ") {
            return None;
        }

        let mut map = HashMap::new();
        let parts = header_clean[7..].split(',');
        for part in parts {
            if let Some((k, v)) = part.split_once('=') {
                let key = k.trim();
                let val = v.trim().trim_matches('"');
                map.insert(key, val.to_string());
            }
        }

        Some(Self {
            username: map.get("username")?.clone(),
            realm: map.get("realm")?.clone(),
            nonce: map.get("nonce")?.clone(),
            uri: map.get("uri")?.clone(),
            response: map.get("response")?.clone(),
        })
    }

    pub fn verify(&self, ha1: &str, method: &str) -> bool {
        // HA1 veritabanından hash olarak gelir: MD5(username:realm:password)
        // HA2 = MD5(method:digestURI)
        let ha2_str = format!("{}:{}", method, self.uri);
        let ha2 = format!("{:x}", md5::compute(ha2_str.as_bytes()));

        // Response = MD5(HA1:nonce:HA2)
        let response_str = format!("{}:{}:{}", ha1, self.nonce, ha2);
        let expected_response = format!("{:x}", md5::compute(response_str.as_bytes()));

        self.response == expected_response
    }
}
