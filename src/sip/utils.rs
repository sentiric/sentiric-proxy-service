// sentiric-proxy-service/src/sip/utils.rs

use sentiric_sip_core::{HeaderName, SipPacket};

pub fn get_header(packet: &SipPacket, name: HeaderName) -> String {
    packet.get_header_value(name).cloned().unwrap_or_default()
}
