use rsipstack::sip::SipMessage;
use rsipstack::{transaction::endpoint::MessageInspector, transport::SipAddr};
use std::net::IpAddr;
use tracing::debug;

pub struct NatInspector;

impl Default for NatInspector {
    fn default() -> Self {
        Self::new()
    }
}

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

    fn fix_contact_header(
        &self,
        header_value: &mut String,
        from_addr: &rsipstack::sip::HostWithPort,
    ) {
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

            if let Ok(ip) = host_only.parse::<IpAddr>()
                && Self::is_private_ip(&ip) {
                    let from_host = from_addr.host.to_string();
                    if let Ok(from_ip) = from_host.parse::<IpAddr>()
                        && (from_ip == ip || Self::is_private_ip(&from_ip)) {
                            return;
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
        } else if let Some(host_part) = uri_str.strip_prefix("sip:") {
            let host_only = if let Some(col_idx) = host_part.find(':') {
                &host_part[..col_idx]
            } else {
                host_part
            };

            if let Ok(ip) = host_only.parse::<IpAddr>()
                && Self::is_private_ip(&ip) {
                    let from_host = from_addr.host.to_string();
                    // If the ip is the same as the source ip, we don't need to fix it
                    if let Ok(from_ip) = from_host.parse::<IpAddr>()
                        && from_ip == ip {
                            return;
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
                rsipstack::sip::StatusCodeKind::Provisional
                    | rsipstack::sip::StatusCodeKind::Successful
            );

            if is_target_forming {
                for header in resp.headers.iter_mut() {
                    if let rsipstack::sip::Header::Contact(contact) = header {
                        let mut val = contact.value().to_string();
                        let old_val = val.clone();
                        self.fix_contact_header(&mut val, &from.addr);
                        if val != old_val {
                            *contact = val.into();
                        }
                    }
                }
            }
        }
        msg
    }
}

#[cfg(test)]
mod tests {
    use super::NatInspector;
    use rsipstack::sip::SipMessage;
    use rsipstack::transaction::endpoint::MessageInspector;
    use rsipstack::transport::SipAddr;

    #[test]
    fn test_nat_fix_rewritten_contact_should_not_duplicate_header_name() {
        let raw = concat!(
            "SIP/2.0 200 OK\r\n",
            "Via: SIP/2.0/UDP 198.51.100.24:15060;rport=15060;received=198.51.100.24;branch=z9hG4bKdDbDaK1ixkQ7\r\n",
            "Call-ID: lFG6BkmOTiJ7fbAS5as6S2@voltecall\r\n",
            "From: <sip:alice@198.51.100.23>;tag=aTNjBN8v\r\n",
            "To: <sip:79900123456@203.0.113.52>;tag=df598941-c590-4772-9a26-7c9633759dd6\r\n",
            "CSeq: 7 INVITE\r\n",
            "Allow: PRACK, INVITE, ACK, BYE, CANCEL, UPDATE, INFO, SUBSCRIBE, NOTIFY, REFER, MESSAGE, OPTIONS\r\n",
            "Contact: <sip:41111112222@10.10.10.10:15060>\r\n",
            "Supported: replaces, 100rel, timer, norefersub\r\n",
            "Content-Type: application/sdp\r\n",
            "Content-Length:   311\r\n",
            "\r\n",
            "v=0\r\n",
            "o=- 3985392156 3985392157 IN IP4 10.10.10.10\r\n",
            "s=volte\r\n",
            "b=AS:84\r\n",
            "t=0 0\r\n",
            "a=X-nat:0\r\n",
            "m=audio 4000 RTP/AVP 8 101\r\n",
            "c=IN IP4 10.10.10.10\r\n",
            "b=TIAS:64000\r\n",
            "a=rtcp:4001 IN IP4 10.10.10.10\r\n",
            "a=sendrecv\r\n",
            "a=rtpmap:8 PCMA/8000\r\n",
            "a=ssrc:1173482294 cname:0ea64c6460b897ba\r\n",
            "a=rtpmap:101 telephone-event/8000\r\n",
            "a=fmtp:101 0-16\r\n"
        );
        let msg = SipMessage::try_from(raw).unwrap();
        let from: SipAddr = rsipstack::sip::HostWithPort::try_from("198.51.100.24:15060")
            .unwrap()
            .into();

        let rewritten = NatInspector::new().after_received(msg, &from);
        let text = rewritten.to_string();
        let contact_line = text
            .lines()
            .find(|line| line.starts_with("Contact:"))
            .expect("Contact header should exist");

        assert_eq!(
            contact_line,
            "Contact: <sip:41111112222@198.51.100.24:15060>",
            "rewritten Contact header should not duplicate the header name"
        );
    }
}
