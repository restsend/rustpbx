use rsip::SipMessage;
use rsipstack::{transaction::endpoint::MessageInspector, transport::SipAddr};
use std::net::IpAddr;
use tracing::debug;

pub struct NatInspector;

impl NatInspector {
    pub fn new() -> Self {
        Self
    }

    fn is_private_ip(ip: &IpAddr) -> bool {
        match ip {
            IpAddr::V4(v4) => {
                v4.is_private() || v4.is_loopback() || v4.is_link_local() || v4.is_unspecified()
            }
            IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified(),
        }
    }

    fn fix_contact_header(&self, header_value: &mut String, from_addr: &rsip::HostWithPort) {
        // Simple regex-less replacement of host:port in Contact header if it contains a private IP
        // Contact header looks like: "Display Name" <sip:user@host:port;params> or <sip:host:port> or sip:host:port

        // Let's try to find the URI part
        let uri_start = if let Some(idx) = header_value.find('<') {
            idx + 1
        } else if let Some(idx) = header_value.find("sip:") {
            idx
        } else {
            return;
        };

        let uri_end = if let Some(idx) = header_value[uri_start..].find('>') {
            uri_start + idx
        } else if let Some(idx) = header_value[uri_start..].find(';') {
            uri_start + idx
        } else {
            header_value.len()
        };

        let uri_str = &header_value[uri_start..uri_end];

        // Parse URI
        // Example: sip:user@172.17.0.2:13050
        if let Some(at_idx) = uri_str.find('@') {
            let host_part = &uri_str[at_idx + 1..];
            let host_only = if let Some(col_idx) = host_part.find(':') {
                &host_part[..col_idx]
            } else {
                host_part
            };

            if let Ok(ip) = host_only.parse::<IpAddr>() {
                if Self::is_private_ip(&ip) {
                    let from_host = from_addr.host.to_string();
                    if let Ok(from_ip) = from_host.parse::<IpAddr>() {
                        if from_ip == ip || Self::is_private_ip(&from_ip) {
                            return;
                        }
                    }

                    let from_port = from_addr.port.as_ref().map(|p| p.to_string());

                    let mut new_host_part = from_host;
                    if let Some(port) = from_port {
                        new_host_part.push(':');
                        new_host_part.push_str(&port);
                    } else if let Some(col_idx) = host_part.find(':') {
                        // Keep original port if from_addr doesn't have one (though it should)
                        new_host_part.push_str(&host_part[col_idx..]);
                    }

                    let mut new_header = header_value[..uri_start + at_idx + 1].to_string();
                    new_header.push_str(&new_host_part);
                    new_header.push_str(&header_value[uri_end..]);

                    debug!(old = %header_value, new = %new_header, "Fixed NAT Contact header");
                    *header_value = new_header;
                }
            }
        } else if uri_str.starts_with("sip:") {
            let host_part = &uri_str[4..];
            let host_only = if let Some(col_idx) = host_part.find(':') {
                &host_part[..col_idx]
            } else {
                host_part
            };

            if let Ok(ip) = host_only.parse::<IpAddr>() {
                if Self::is_private_ip(&ip) {
                    let from_host = from_addr.host.to_string();
                    // If the ip is the same as the source ip, we don't need to fix it
                    if let Ok(from_ip) = from_host.parse::<IpAddr>() {
                        if from_ip == ip {
                            return;
                        }
                    }

                    let from_port = from_addr.port.as_ref().map(|p| p.to_string());

                    let mut new_host_part = from_host;
                    if let Some(port) = from_port {
                        new_host_part.push(':');
                        new_host_part.push_str(&port);
                    } else if let Some(col_idx) = host_part.find(':') {
                        new_host_part.push_str(&host_part[col_idx..]);
                    }

                    let mut new_header = header_value[..uri_start + 4].to_string();
                    new_header.push_str(&new_host_part);
                    new_header.push_str(&header_value[uri_end..]);

                    debug!(old = %header_value, new = %new_header, "Fixed NAT Contact header");
                    *header_value = new_header;
                }
            }
        }
    }
}

impl MessageInspector for NatInspector {
    fn before_send(&self, msg: SipMessage, _dest: Option<&SipAddr>) -> SipMessage {
        msg
    }

    fn after_received(&self, msg: SipMessage, from: &SipAddr) -> SipMessage {
        let mut msg = msg;
        if let SipMessage::Response(ref mut resp) = msg {
            // Check if it's a 2xx for INVITE or a 1xx/2xx response that might be used for target learning
            let kind = resp.status_code.kind();
            let is_target_forming = matches!(
                kind,
                rsip::StatusCodeKind::Provisional | rsip::StatusCodeKind::Successful
            );

            if is_target_forming {
                for header in resp.headers.iter_mut() {
                    match header {
                        rsip::Header::Contact(contact) => {
                            let mut val = contact.to_string();
                            let old_val = val.clone();
                            self.fix_contact_header(&mut val, &from.addr);
                            if val != old_val {
                                *contact = val.into();
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        msg
    }
}
