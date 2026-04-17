use crate::proxy::proxy_call::session_timer::TIMER_TAG;
use rsipstack::sip::{Header, SipMessage};
use rsipstack::transaction::endpoint::MessageInspector;
use rsipstack::transport::SipAddr;
use std::sync::{Arc, OnceLock};

pub struct CapabilityHeadersInspector {
    allow_methods: Arc<OnceLock<Vec<rsipstack::sip::Method>>>,
    session_timer_enabled: bool,
}

impl CapabilityHeadersInspector {
    pub fn new(
        allow_methods: Arc<OnceLock<Vec<rsipstack::sip::Method>>>,
        session_timer_enabled: bool,
    ) -> Self {
        Self {
            allow_methods,
            session_timer_enabled,
        }
    }

    fn should_include_register(msg: &SipMessage) -> bool {
        let SipMessage::Response(response) = msg else {
            return false;
        };

        response.headers.iter().any(|header| {
            matches!(
                header,
                Header::CSeq(cseq)
                    if cseq
                        .method()
                        .map(|method| method == rsipstack::sip::Method::Register)
                        .unwrap_or(false)
            )
        })
    }

    fn append_allow_header(
        &self,
        include_register: bool,
        headers: &mut rsipstack::sip::Headers,
    ) {
        let Some(allow_methods) = self.allow_methods.get() else {
            return;
        };
        let rendered = allow_methods
            .iter()
            .filter(|method| include_register || **method != rsipstack::sip::Method::Register)
            .map(|method| method.to_string())
            .collect::<Vec<String>>()
            .join(",");
        if rendered.is_empty() {
            return;
        }

        headers.push(Header::Allow(rendered.into()));
    }

    fn append_supported_timer_header(&self, headers: &mut rsipstack::sip::Headers) {
        if !self.session_timer_enabled {
            return;
        }

        headers.push(Header::Supported(
            rsipstack::sip::headers::Supported::from(TIMER_TAG),
        ));
    }

    fn apply(&self, msg: &mut SipMessage) {
        let include_register = Self::should_include_register(msg);
        let headers = match msg {
            SipMessage::Request(request) => &mut request.headers,
            SipMessage::Response(response) => &mut response.headers,
        };

        self.append_allow_header(include_register, headers);
        self.append_supported_timer_header(headers);
    }
}

impl MessageInspector for CapabilityHeadersInspector {
    fn before_send(&self, msg: SipMessage, _dest: Option<&SipAddr>) -> SipMessage {
        let mut msg = msg;
        self.apply(&mut msg);
        msg
    }

    fn after_received(&self, msg: SipMessage, _from: &SipAddr) -> SipMessage {
        msg
    }
}

#[cfg(test)]
mod tests {
    use super::CapabilityHeadersInspector;
    use rsipstack::sip::SipMessage;
    use rsipstack::transaction::endpoint::MessageInspector;
    use std::sync::{Arc, OnceLock};

    fn parse_request(raw: &str) -> SipMessage {
        SipMessage::try_from(raw).expect("request should parse")
    }

    fn parse_response(raw: &str) -> SipMessage {
        SipMessage::try_from(raw).expect("response should parse")
    }

    fn allow_methods(
        methods: Vec<rsipstack::sip::Method>,
    ) -> Arc<OnceLock<Vec<rsipstack::sip::Method>>> {
        let store = Arc::new(OnceLock::new());
        store
            .set(methods)
            .expect("allow methods should only be initialized once");
        store
    }

    #[test]
    fn adds_allow_and_supported_timer() {
        let inspector = CapabilityHeadersInspector::new(
            allow_methods(vec![
                rsipstack::sip::Method::Invite,
                rsipstack::sip::Method::Ack,
                rsipstack::sip::Method::Register,
            ]),
            true,
        );
        let msg = parse_request(
            "OPTIONS sip:alice@example.com SIP/2.0\r\nContent-Length: 0\r\n\r\n",
        );

        let rewritten = inspector.before_send(msg, None).to_string();

        assert!(rewritten.contains("Allow: INVITE,ACK\r\n"));
        assert!(rewritten.contains("Supported: timer\r\n"));
    }

    #[test]
    fn excludes_register_on_non_register_messages() {
        let inspector = CapabilityHeadersInspector::new(
            allow_methods(vec![
                rsipstack::sip::Method::Invite,
                rsipstack::sip::Method::Register,
            ]),
            true,
        );
        let msg = parse_request(
            "OPTIONS sip:alice@example.com SIP/2.0\r\nContent-Length: 0\r\n\r\n",
        );

        let rewritten = inspector.before_send(msg, None).to_string();

        assert!(rewritten.contains("Allow: INVITE\r\n"));
        assert!(!rewritten.contains("REGISTER"));
    }

    #[test]
    fn includes_register_on_register_responses() {
        let inspector = CapabilityHeadersInspector::new(
            allow_methods(vec![
                rsipstack::sip::Method::Invite,
                rsipstack::sip::Method::Register,
            ]),
            true,
        );
        let msg = parse_response(
            concat!(
                "SIP/2.0 200 OK\r\n",
                "CSeq: 1 REGISTER\r\n",
                "Content-Length: 0\r\n\r\n"
            ),
        );

        let rewritten = inspector.before_send(msg, None).to_string();

        assert!(rewritten.contains("Allow: INVITE,REGISTER\r\n"));
    }

    #[test]
    fn excludes_register_on_non_register_responses() {
        let inspector = CapabilityHeadersInspector::new(
            allow_methods(vec![
                rsipstack::sip::Method::Invite,
                rsipstack::sip::Method::Register,
            ]),
            true,
        );
        let msg = parse_response(
            concat!(
                "SIP/2.0 200 OK\r\n",
                "CSeq: 1 INVITE\r\n",
                "Content-Length: 0\r\n\r\n"
            ),
        );

        let rewritten = inspector.before_send(msg, None).to_string();

        assert!(rewritten.contains("Allow: INVITE\r\n"));
        assert!(!rewritten.contains("REGISTER"));
    }

    #[test]
    fn appends_supported_even_if_present() {
        let inspector = CapabilityHeadersInspector::new(allow_methods(Vec::new()), true);
        let msg = parse_request(
            concat!(
                "OPTIONS sip:alice@example.com SIP/2.0\r\n",
                "Supported: replaces, 100rel\r\n",
                "Content-Length: 0\r\n\r\n"
            ),
        );

        let rewritten = inspector.before_send(msg, None).to_string();

        assert_eq!(rewritten.matches("Supported:").count(), 2);
        assert!(rewritten.contains("Supported: replaces, 100rel\r\n"));
        assert!(rewritten.contains("Supported: timer\r\n"));
    }

    #[test]
    fn skips_supported_when_session_timer_disabled() {
        let inspector = CapabilityHeadersInspector::new(allow_methods(Vec::new()), false);
        let msg = parse_request(
            "OPTIONS sip:alice@example.com SIP/2.0\r\nContent-Length: 0\r\n\r\n",
        );

        let rewritten = inspector.before_send(msg, None).to_string();

        assert!(!rewritten.contains("Supported:"));
    }
}
