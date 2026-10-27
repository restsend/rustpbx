use arc_swap::ArcSwap;
use ipnet::IpNet;
use prefix_trie::joint::JointPrefixSet;
use rsipstack::sip::prelude::HeadersExt;
use rsipstack::sip::{HostWithPort, SipMessage, ToTypedHeader};
use rsipstack::transaction::endpoint::MessageInspector;
use rsipstack::transport::SipAddr;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use tracing::debug;

use super::data::ProxyDataContext;
use super::routing::TrunkConfig;

/// Rewrites private-IP Contacts on received messages to the observed source
/// address.
///
/// The global `proxy.nat_fix` is the default; trunks whose own `nat_fix`
/// differs from it form an exception list keyed by source network. With the
/// global switch on the list is a blacklist (messages from those trunks are
/// left alone), with it off a whitelist (only those trunks are fixed). The
/// list is replaced on every trunk reload via [`NatInspector::update_trunks`].
#[derive(Clone)]
pub struct NatInspector {
    enabled: bool,
    exceptions: Arc<ArcSwap<JointPrefixSet<IpNet>>>,
}

impl Default for NatInspector {
    fn default() -> Self {
        Self::new(true)
    }
}

impl NatInspector {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            exceptions: Arc::new(ArcSwap::from_pointee(JointPrefixSet::new())),
        }
    }

    /// Rebuild the exception list from the current trunks: every enabled
    /// trunk whose `nat_fix` is set and differs from the global default
    /// contributes its `inbound_hosts`, `dest` and `backup_dest` (IP/CIDR
    /// entries only; hostnames are not resolved).
    pub fn update_trunks(&self, trunks: &HashMap<String, TrunkConfig>) {
        let mut exceptions = JointPrefixSet::new();
        for trunk in trunks.values() {
            if trunk.nat_fix.is_none_or(|enabled| enabled == self.enabled)
                || trunk.disabled == Some(true)
            {
                continue;
            }
            let networks = trunk
                .inbound_hosts
                .iter()
                .map(String::as_str)
                .chain(std::iter::once(trunk.dest.as_str()))
                .chain(trunk.backup_dest.as_deref())
                .filter_map(ProxyDataContext::dest_to_ipnet);
            for network in networks {
                exceptions.insert(network);
            }
        }
        self.exceptions.store(Arc::new(exceptions));
    }

    fn enabled_for(&self, from: &SipAddr) -> bool {
        let rsipstack::sip::Host::IpAddr(ip) = &from.addr.host else {
            return self.enabled;
        };
        let listed = self.exceptions.load().get_lpm(&IpNet::from(*ip)).is_some();
        self.enabled != listed
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
        if let Some(new_value) = rewrite_contact_value(header_value, from_addr) {
            *header_value = new_value;
        }
    }
}

/// Core rewrite logic operating on an inbound INVITE request.
///
/// `source_port` is the actual transport source port, used as a fallback when
/// the Via has no `rport`.
fn fix_nated_contact_in_request(
    req: &mut rsipstack::sip::Request,
    source_port: Option<u16>,
) -> bool {
    // REGISTER Contact identifies the binding across reconnects; the registrar
    // stores the reachable transport flow separately as its destination.
    if req.method != rsipstack::sip::Method::Invite {
        return false;
    }

    let Ok(via) = req.via_header() else {
        return false;
    };
    let Ok(top) = via.first_value() else {
        return false;
    };
    let Ok(typed) = top.typed() else {
        return false;
    };

    // NAT signal: the transport layer only sets `received` (alongside `rport`)
    // when the real source address differs from the Via sent-by.
    let Some(Ok(received_ip)) = typed.received() else {
        return false;
    };

    let target = HostWithPort {
        host: rsipstack::sip::Host::IpAddr(received_ip),
        port: typed
            .rport()
            .and_then(|r| r)
            .or(source_port)
            .map(Into::into),
    };

    let mut changed = false;
    for header in req.headers.iter_mut() {
        if let rsipstack::sip::Header::Contact(contact) = header {
            let value = contact.value().to_string();
            if let Some(new_value) = rewrite_contact_value(&value, &target) {
                *contact = new_value.into();
                changed = true;
            }
        }
    }
    if changed {
        debug!(
            source = %target,
            method = %req.method,
            "Fixed NATed Contact on inbound INVITE/REGISTER"
        );
    }
    changed
}

/// Rewrite every `<...>` URI in a Contact header value whose host is a private
/// IP, returning the new header value when at least one URI changed.
fn rewrite_contact_value(value: &str, target: &HostWithPort) -> Option<String> {
    let mut out = String::with_capacity(value.len());
    let mut changed = false;
    let mut rest = value;
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..=lt]);
        let after = &rest[lt + 1..];
        let uri_len = after.find('>').unwrap_or(after.len());
        let uri = &after[..uri_len];
        match rewrite_nated_uri(uri, target) {
            Some(new_uri) => {
                out.push_str(&new_uri);
                changed = true;
            }
            None => out.push_str(uri),
        }
        out.push('>');
        rest = &after[uri_len + 1..];
    }
    out.push_str(rest);
    changed.then_some(out)
}

/// Rewrite a single URI (the content of a `<...>` Contact value) when its host
/// is a private IP. Returns `None` when no rewrite applies.
fn rewrite_nated_uri(uri: &str, target: &HostWithPort) -> Option<String> {
    let scheme_end = uri.find(':')? + 1;
    let scheme = &uri[..scheme_end];
    if scheme != "sip:" && scheme != "sips:" {
        return None;
    }
    let rest = &uri[scheme_end..];
    let (user_part, authority) = match rest.find('@') {
        Some(idx) => (&rest[..=idx], &rest[idx + 1..]),
        None => ("", rest),
    };
    let (authority, suffix) = match authority.find([';', '?']) {
        Some(idx) => (&authority[..idx], &authority[idx..]),
        None => (authority, ""),
    };

    let (host, orig_port) = if let Some(after_bracket) = authority.strip_prefix('[') {
        let close = after_bracket.find(']')?;
        let host = &after_bracket[..close];
        let port = after_bracket[close + 1..].strip_prefix(':');
        (host, port.map(String::from))
    } else {
        match authority.find(':') {
            Some(idx) => (&authority[..idx], Some(authority[idx + 1..].to_string())),
            None => (authority, None),
        }
    };

    let Ok(host_ip) = host.parse::<IpAddr>() else {
        return None;
    };
    if !NatInspector::is_private_ip(&host_ip) {
        return None;
    }

    let target_ip = match &target.host {
        rsipstack::sip::Host::IpAddr(ip) => *ip,
        rsipstack::sip::Host::Domain(d) => d.0.parse().ok()?,
    };
    // When the received address is itself private *and* differs from the Contact
    // host, both sides are on private networks — rewriting buys nothing.
    // However when received == contact host (port-only NAT), fixing the port is
    // still beneficial so we fall through.
    if NatInspector::is_private_ip(&target_ip) && target_ip != host_ip {
        return None;
    }

    let new_port = target
        .port
        .map(|p| p.to_string())
        .or_else(|| orig_port.clone());

    if target_ip == host_ip {
        // Host already reachable; only the port needs fixing.
        if new_port == orig_port {
            return None;
        }
        let new_authority = match new_port {
            Some(ref p) => format!("{}:{}", host, p),
            None => host.to_string(),
        };
        return Some(format!(
            "{}{}{}{}",
            scheme, user_part, new_authority, suffix
        ));
    }

    let new_host = match &target.host {
        rsipstack::sip::Host::IpAddr(ip @ IpAddr::V6(_)) => format!("[{}]", ip),
        rsipstack::sip::Host::IpAddr(ip) => ip.to_string(),
        rsipstack::sip::Host::Domain(d) => d.0.clone(),
    };
    let new_authority = match new_port {
        Some(ref p) => format!("{}:{}", new_host, p),
        None => new_host,
    };
    Some(format!(
        "{}{}{}{}",
        scheme, user_part, new_authority, suffix
    ))
}

impl MessageInspector for NatInspector {
    fn before_send(&self, msg: SipMessage, _dest: Option<&SipAddr>) -> SipMessage {
        msg
    }
    fn after_received(&self, msg: SipMessage, from: Option<&SipAddr>) -> SipMessage {
        let mut msg = msg;
        let Some(from) = from else { return msg };
        if !self.enabled_for(from) {
            return msg;
        }

        // WebSocket clients keep their registered Contact across reconnects;
        // the locator stores the current reachable flow separately. Rewriting
        // a dialog Contact to this flow's peer address breaks that identity and
        // strands in-dialog requests at the old address after a reconnect.
        if matches!(
            from.r#type,
            Some(rsipstack::sip::Transport::Ws | rsipstack::sip::Transport::Wss)
        ) {
            return msg;
        }

        // ── Request side: fix Caller Contact for INVITE ────────────────
        if let SipMessage::Request(ref mut req) = msg {
            let source_port = from.addr.port.map(|p| p.0);
            fix_nated_contact_in_request(req, source_port);
            return msg;
        }

        // ── Response side: fix Callee Contact in 1xx/2xx ────────────────
        if let SipMessage::Response(ref mut resp) = msg {
            let kind = resp.status_code.kind();
            // A REGISTER response's Contacts are our own bindings echoed
            // back by the registrar, not the peer's address; rewriting them
            // breaks matching our binding to read its granted expiration.
            let is_register = resp
                .cseq_header()
                .ok()
                .and_then(|cseq| cseq.method().ok())
                == Some(rsipstack::sip::Method::Register);
            let is_target_forming = !is_register
                && matches!(
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
    use crate::proxy::routing::TrunkConfig;
    use std::collections::HashMap;
    use std::sync::Arc;
    use rsipstack::sip::SipMessage;
    use rsipstack::sip::{HeadersExt, ToTypedHeader};
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

        let rewritten = NatInspector::default().after_received(msg, Some(&from));
        let text = rewritten.to_string();
        let contact_line = text
            .lines()
            .find(|line| line.starts_with("Contact:"))
            .expect("Contact header should exist");

        assert_eq!(
            contact_line, "Contact: <sip:41111112222@198.51.100.24:15060>",
            "rewritten Contact header should not duplicate the header name"
        );
    }

    #[test]
    fn test_nat_fix_leaves_register_response_contacts_untouched() {
        // A REGISTER 200 OK echoes our own (private) binding; rewriting it to
        // the registrar's address would stop us finding our granted expires.
        let raw = concat!(
            "SIP/2.0 200 OK\r\n",
            "Via: SIP/2.0/UDP 10.0.0.5:15060;rport=40000;received=198.51.100.9;branch=z9hG4bK-reg\r\n",
            "From: <sip:trunkuser@203.0.113.52>;tag=a\r\n",
            "To: <sip:trunkuser@203.0.113.52>;tag=b\r\n",
            "Call-ID: reg-nat-test\r\n",
            "CSeq: 2 REGISTER\r\n",
            "Contact: <sip:trunkuser@10.0.0.5:15060>;expires=600\r\n",
            "Content-Length: 0\r\n",
            "\r\n"
        );
        let msg = SipMessage::try_from(raw).unwrap();
        let from: SipAddr = rsipstack::sip::HostWithPort::try_from("203.0.113.52:5060")
            .unwrap()
            .into();

        let text = NatInspector::default()
            .after_received(msg, Some(&from))
            .to_string();
        assert!(
            text.contains("Contact: <sip:trunkuser@10.0.0.5:15060>;expires=600"),
            "{text}"
        );
    }

    fn carrier_trunks(nat_fix: Option<bool>) -> HashMap<String, TrunkConfig> {
        HashMap::from([(
            "carrier".to_string(),
            TrunkConfig {
                dest: "sip:203.0.113.52:5060".to_string(),
                inbound_hosts: vec!["198.51.100.0/24".to_string()],
                nat_fix,
                ..Default::default()
            },
        )])
    }

    fn inspector_with_trunk(global: bool, trunk_nat_fix: Option<bool>) -> NatInspector {
        let inspector = NatInspector::new(global);
        inspector.update_trunks(&carrier_trunks(trunk_nat_fix));
        inspector
    }

    fn fixed_invite_contact(inspector: &NatInspector, source: &str) -> String {
        let msg = SipMessage::Request(nated_invite());
        let from: SipAddr = rsipstack::sip::HostWithPort::try_from(source)
            .unwrap()
            .into();
        let SipMessage::Request(req) = inspector.after_received(msg, Some(&from)) else {
            panic!("expected request")
        };
        contact_line(&req)
    }

    #[test]
    fn test_trunk_nat_fix_overrides_disabled_global() {
        let inspector = inspector_with_trunk(false, Some(true));
        // From the trunk's inbound_hosts: the trunk enables the fix.
        assert_eq!(
            fixed_invite_contact(&inspector, "198.51.100.24:15060"),
            "Contact: <sip:41111112222@198.51.100.24:15060>"
        );
        // Any other source keeps the global (disabled) behaviour.
        assert_eq!(
            fixed_invite_contact(&inspector, "192.0.2.7:15060"),
            "Contact: <sip:41111112222@10.10.10.10:15060>"
        );
    }

    #[test]
    fn test_trunk_nat_fix_overrides_enabled_global() {
        let inspector = inspector_with_trunk(true, Some(false));
        assert_eq!(
            fixed_invite_contact(&inspector, "198.51.100.24:15060"),
            "Contact: <sip:41111112222@10.10.10.10:15060>"
        );
        assert_eq!(
            fixed_invite_contact(&inspector, "192.0.2.7:15060"),
            "Contact: <sip:41111112222@198.51.100.24:15060>"
        );
    }

    #[test]
    fn test_trunk_nat_fix_override_matches_trunk_dest_responses() {
        // A 200 OK from the trunk's dest (an outbound call) follows the trunk.
        let inspector = inspector_with_trunk(true, Some(false));
        let raw = concat!(
            "SIP/2.0 200 OK\r\n",
            "Via: SIP/2.0/UDP 198.51.100.24:15060;branch=z9hG4bK-out\r\n",
            "Call-ID: out@test\r\n",
            "From: <sip:alice@198.51.100.24>;tag=a\r\n",
            "To: <sip:bob@203.0.113.52>;tag=b\r\n",
            "CSeq: 1 INVITE\r\n",
            "Contact: <sip:bob@10.20.30.40:5060>\r\n",
            "Content-Length: 0\r\n",
            "\r\n"
        );
        let from: SipAddr = rsipstack::sip::HostWithPort::try_from("203.0.113.52:5060")
            .unwrap()
            .into();
        let msg = SipMessage::try_from(raw).unwrap();
        let text = inspector.after_received(msg, Some(&from)).to_string();
        assert!(
            text.contains("Contact: <sip:bob@10.20.30.40:5060>"),
            "{text}"
        );
    }

    #[test]
    fn test_trunk_without_nat_fix_follows_global() {
        let inspector = inspector_with_trunk(false, None);
        assert_eq!(
            fixed_invite_contact(&inspector, "198.51.100.24:15060"),
            "Contact: <sip:41111112222@10.10.10.10:15060>"
        );
    }

    #[tokio::test]
    async fn test_trunk_reload_updates_nat_exceptions() {
        use crate::config::ProxyConfig;
        use crate::proxy::data::ProxyDataContext;

        let generated_dir = tempfile::tempdir().unwrap();
        let mut config = ProxyConfig::default();
        config.nat_fix = false;
        config.acl_rules = None;
        config.generated_dir = generated_dir.path().to_string_lossy().to_string();
        config.trunks = carrier_trunks(Some(true));
        let dc = ProxyDataContext::new(Arc::new(config.clone()), None)
            .await
            .unwrap();

        let inspector = dc.nat_inspector().clone();
        assert_eq!(
            fixed_invite_contact(&inspector, "198.51.100.24:15060"),
            "Contact: <sip:41111112222@198.51.100.24:15060>"
        );

        config.trunks = carrier_trunks(None);
        dc.reload_trunks(false, Some(Arc::new(config)))
            .await
            .unwrap();
        assert_eq!(
            fixed_invite_contact(&inspector, "198.51.100.24:15060"),
            "Contact: <sip:41111112222@10.10.10.10:15060>"
        );
    }

    fn contact_line(req: &rsipstack::sip::Request) -> String {
        let text = rsipstack::sip::SipMessage::Request(req.clone()).to_string();
        text.lines()
            .find(|line| line.starts_with("Contact:"))
            .expect("Contact header should exist")
            .to_string()
    }

    fn nated_invite() -> rsipstack::sip::Request {
        let raw = concat!(
            "INVITE sip:79900123456@203.0.113.52 SIP/2.0\r\n",
            "Via: SIP/2.0/UDP 10.10.10.10:15060;rport=15060;received=198.51.100.24;branch=z9hG4bKdDbDaK1ixkQ7\r\n",
            "Call-ID: test@voltecall\r\n",
            "From: <sip:41111112222@10.10.10.10>;tag=aTNjBN8v\r\n",
            "To: <sip:79900123456@203.0.113.52>\r\n",
            "CSeq: 7 INVITE\r\n",
            "Contact: <sip:41111112222@10.10.10.10:15060>\r\n",
            "Content-Length: 0\r\n",
            "\r\n"
        );
        let rsipstack::sip::SipMessage::Request(req) =
            rsipstack::sip::SipMessage::try_from(raw).unwrap()
        else {
            panic!("expected request")
        };
        req
    }

    #[test]
    fn websocket_contacts_preserve_registration_identity() {
        use rsipstack::sip::{Response, StatusCode, Transport, Version};

        for transport in [
            Transport::Ws,
            Transport::Wss,
            Transport::Udp,
            Transport::Tcp,
        ] {
            let from = SipAddr {
                r#type: Some(transport),
                addr: "198.51.100.24:15060".try_into().unwrap(),
            };
            let mut request = nated_invite();
            // The actual transport determines whether a Contact is rewritten;
            // a peer-supplied transport parameter must not select this policy.
            *request.contact_header_mut().unwrap() =
                "<sip:41111112222@10.10.10.10:15060;transport=wss>".into();
            let original = request.contact_header().unwrap().value().to_owned();
            let response = Response {
                version: Version::V2,
                status_code: StatusCode::OK,
                headers: request.headers.clone(),
                body: vec![],
                wire_reason: None,
            };
            for message in [SipMessage::Request(request), SipMessage::Response(response)] {
                let inspected = NatInspector::new(true).after_received(message, Some(&from));
                let contact = match &inspected {
                    SipMessage::Request(request) => request.contact_header().unwrap(),
                    SipMessage::Response(response) => response.contact_header().unwrap(),
                };
                let expected = if matches!(transport, Transport::Ws | Transport::Wss) {
                    original.as_str()
                } else {
                    "<sip:41111112222@198.51.100.24:15060;transport=wss>"
                };
                assert_eq!(
                    contact.value(),
                    expected,
                    "transport={transport}, message={inspected}"
                );
            }
        }
    }

    #[test]
    fn test_fix_nated_contact_invite_rewrites_private_contact() {
        let mut req = nated_invite();
        assert!(super::fix_nated_contact_in_request(&mut req, None));
        assert_eq!(
            contact_line(&req),
            "Contact: <sip:41111112222@198.51.100.24:15060>"
        );
    }

    #[test]
    fn test_fix_nated_contact_register_preserves_binding_identity() {
        let raw = concat!(
            "REGISTER sip:rustpbx.com SIP/2.0\r\n",
            "Via: SIP/2.0/UDP 192.168.1.50:5060;rport=60780;received=203.0.113.9;branch=z9hG4bKreg1\r\n",
            "Call-ID: reg-test@rustpbx.com\r\n",
            "From: <sip:1001@rustpbx.com>;tag=from-tag\r\n",
            "To: <sip:1001@rustpbx.com>\r\n",
            "CSeq: 1 REGISTER\r\n",
            "Contact: <sip:1001@192.168.1.50:5060;transport=udp>;expires=3600\r\n",
            "Content-Length: 0\r\n",
            "\r\n"
        );
        let rsipstack::sip::SipMessage::Request(mut req) =
            rsipstack::sip::SipMessage::try_from(raw).unwrap()
        else {
            panic!("expected request")
        };
        assert!(!super::fix_nated_contact_in_request(&mut req, None));
        assert_eq!(
            contact_line(&req),
            "Contact: <sip:1001@192.168.1.50:5060;transport=udp>;expires=3600"
        );
    }

    #[test]
    fn test_fix_nated_contact_no_received_param_leaves_unchanged() {
        let mut req = nated_invite();
        // remove received/rport so no NAT signal
        let mut via = req.via_header().unwrap().clone();
        via.update_first_value(|v| {
            let mut typed = v.typed()?;
            typed.params.retain(|p| {
                !matches!(
                    p,
                    rsipstack::sip::Param::Received(_) | rsipstack::sip::Param::Rport(_)
                )
            });
            Ok(typed.into())
        })
        .unwrap();
        *req.via_header_mut().unwrap() = via;

        assert!(!super::fix_nated_contact_in_request(&mut req, None));
        assert_eq!(
            contact_line(&req),
            "Contact: <sip:41111112222@10.10.10.10:15060>"
        );
    }

    #[test]
    fn test_fix_nated_contact_port_only_nat_rewrites_port() {
        let mut req = nated_invite();
        // source host equals contact host but the port changed (port-only NAT)
        let mut via = req.via_header().unwrap().clone();
        via.update_first_value(|v| {
            let mut typed = v.typed()?;
            typed.params.retain(|p| {
                !matches!(
                    p,
                    rsipstack::sip::Param::Rport(_) | rsipstack::sip::Param::Received(_)
                )
            });
            typed.params.push(rsipstack::sip::Param::Received(
                rsipstack::sip::param::Received::new("10.10.10.10"),
            ));
            typed.params.push(rsipstack::sip::Param::Rport(Some(16060)));
            Ok(typed.into())
        })
        .unwrap();
        *req.via_header_mut().unwrap() = via;

        assert!(super::fix_nated_contact_in_request(&mut req, None));
        assert_eq!(
            contact_line(&req),
            "Contact: <sip:41111112222@10.10.10.10:16060>"
        );
    }

    #[test]
    fn test_fix_nated_contact_public_contact_host_leaves_unchanged() {
        let raw = concat!(
            "INVITE sip:79900123456@203.0.113.52 SIP/2.0\r\n",
            "Via: SIP/2.0/UDP 198.51.100.24:15060;rport=15060;received=198.51.100.24;branch=z9hG4bKpub\r\n",
            "Call-ID: pub@voltecall\r\n",
            "From: <sip:41111112222@198.51.100.24>;tag=aTNjBN8v\r\n",
            "To: <sip:79900123456@203.0.113.52>\r\n",
            "CSeq: 7 INVITE\r\n",
            "Contact: <sip:41111112222@198.51.100.24:15060>\r\n",
            "Content-Length: 0\r\n",
            "\r\n"
        );
        let rsipstack::sip::SipMessage::Request(mut req) =
            rsipstack::sip::SipMessage::try_from(raw).unwrap()
        else {
            panic!("expected request")
        };
        assert!(!super::fix_nated_contact_in_request(&mut req, None));
        assert_eq!(
            contact_line(&req),
            "Contact: <sip:41111112222@198.51.100.24:15060>"
        );
    }

    #[test]
    fn test_fix_nated_contact_private_received_leaves_unchanged() {
        // received is itself private: same LAN / private hop, nothing to fix
        let raw = concat!(
            "INVITE sip:79900123456@203.0.113.52 SIP/2.0\r\n",
            "Via: SIP/2.0/UDP 10.10.10.10:15060;rport=15060;received=10.10.0.1;branch=z9hG4bKpriv\r\n",
            "Call-ID: priv@voltecall\r\n",
            "From: <sip:41111112222@10.10.10.10>;tag=aTNjBN8v\r\n",
            "To: <sip:79900123456@203.0.113.52>\r\n",
            "CSeq: 7 INVITE\r\n",
            "Contact: <sip:41111112222@10.10.10.10:15060>\r\n",
            "Content-Length: 0\r\n",
            "\r\n"
        );
        let rsipstack::sip::SipMessage::Request(mut req) =
            rsipstack::sip::SipMessage::try_from(raw).unwrap()
        else {
            panic!("expected request")
        };
        assert!(!super::fix_nated_contact_in_request(&mut req, None));
        assert_eq!(
            contact_line(&req),
            "Contact: <sip:41111112222@10.10.10.10:15060>"
        );
    }

    #[test]
    fn test_fix_nated_contact_non_invite_register_leaves_unchanged() {
        let mut req = nated_invite();
        req.method = rsipstack::sip::Method::Bye;
        assert!(!super::fix_nated_contact_in_request(&mut req, None));
        assert_eq!(
            contact_line(&req),
            "Contact: <sip:41111112222@10.10.10.10:15060>"
        );
    }
}
