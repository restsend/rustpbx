use rsipstack::sip::{Header, SipMessage};
use rsipstack::transaction::endpoint::MessageInspector;
use rsipstack::transport::SipAddr;
use std::sync::{Arc, OnceLock};

pub struct CapabilityHeadersInspector {
    allow_methods: Arc<OnceLock<Vec<rsipstack::sip::Method>>>,
}

impl CapabilityHeadersInspector {
    pub fn new(allow_methods: Arc<OnceLock<Vec<rsipstack::sip::Method>>>) -> Self {
        Self { allow_methods }
    }

    fn cseq_method(msg: &SipMessage) -> Option<rsipstack::sip::Method> {
        let SipMessage::Response(response) = msg else {
            return None;
        };

        response.headers.iter().find_map(|header| match header {
            Header::CSeq(cseq) => cseq.method().ok(),
            _ => None,
        })
    }

    fn should_add_allow_header(msg: &SipMessage) -> bool {
        match msg {
            SipMessage::Request(request) => request.method == rsipstack::sip::Method::Invite,
            SipMessage::Response(response) => {
                response.status_code == rsipstack::sip::StatusCode::MethodNotAllowed
                    || (response.status_code.kind()
                        == rsipstack::sip::StatusCodeKind::Successful
                        && Self::cseq_method(msg) == Some(rsipstack::sip::Method::Invite))
            }
        }
    }

    fn should_render_method(method: &rsipstack::sip::Method) -> bool {
        !matches!(
            method,
            rsipstack::sip::Method::Register
                | rsipstack::sip::Method::Subscribe
                | rsipstack::sip::Method::Publish
                | rsipstack::sip::Method::Options
        )
    }

    fn append_allow_header(&self, headers: &mut rsipstack::sip::Headers) {
        let Some(allow_methods) = self.allow_methods.get() else {
            return;
        };

        let mut rendered_methods = Vec::new();
        for method in allow_methods.iter() {
            if !Self::should_render_method(method) || rendered_methods.contains(method) {
                continue;
            }
            rendered_methods.push(method.clone());
        }

        let rendered = rendered_methods
            .iter()
            .map(|method| method.to_string())
            .collect::<Vec<String>>()
            .join(",");
        if rendered.is_empty() {
            return;
        }

        headers.push(Header::Allow(rendered.into()));
    }

    fn apply(&self, msg: &mut SipMessage) {
        if !Self::should_add_allow_header(msg) {
            return;
        }

        let headers = match msg {
            SipMessage::Request(request) => &mut request.headers,
            SipMessage::Response(response) => &mut response.headers,
        };

        self.append_allow_header(headers);
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
    fn adds_allow_to_invite_requests() {
        let inspector = CapabilityHeadersInspector::new(allow_methods(vec![
            rsipstack::sip::Method::Invite,
            rsipstack::sip::Method::Ack,
            rsipstack::sip::Method::Register,
            rsipstack::sip::Method::Invite,
            rsipstack::sip::Method::Options,
        ]));
        let msg = parse_request(
            "INVITE sip:alice@example.com SIP/2.0\r\nContent-Length: 0\r\n\r\n",
        );

        let rewritten = inspector.before_send(msg, None).to_string();

        assert!(rewritten.contains("Allow: INVITE,ACK\r\n"));
    }

    #[test]
    fn adds_allow_to_successful_invite_responses() {
        let inspector = CapabilityHeadersInspector::new(allow_methods(vec![
            rsipstack::sip::Method::Invite,
            rsipstack::sip::Method::Ack,
            rsipstack::sip::Method::Options,
        ]));
        let msg = parse_response(
            concat!(
                "SIP/2.0 200 OK\r\n",
                "CSeq: 1 INVITE\r\n",
                "Content-Length: 0\r\n\r\n"
            ),
        );

        let rewritten = inspector.before_send(msg, None).to_string();

        assert!(rewritten.contains("Allow: INVITE,ACK\r\n"));
    }

    #[test]
    fn adds_allow_to_method_not_allowed_responses() {
        let inspector = CapabilityHeadersInspector::new(allow_methods(vec![
            rsipstack::sip::Method::Invite,
            rsipstack::sip::Method::Ack,
            rsipstack::sip::Method::Register,
        ]));
        let msg = parse_response(
            concat!(
                "SIP/2.0 405 Method Not Allowed\r\n",
                "CSeq: 1 INVITE\r\n",
                "Content-Length: 0\r\n\r\n"
            ),
        );

        let rewritten = inspector.before_send(msg, None).to_string();

        assert!(rewritten.contains("Allow: INVITE,ACK\r\n"));
    }

    #[test]
    fn skips_allow_for_non_invite_requests() {
        let inspector = CapabilityHeadersInspector::new(allow_methods(vec![
            rsipstack::sip::Method::Invite,
            rsipstack::sip::Method::Register,
        ]));
        let msg = parse_request(
            "OPTIONS sip:alice@example.com SIP/2.0\r\nContent-Length: 0\r\n\r\n",
        );

        let rewritten = inspector.before_send(msg, None).to_string();

        assert!(!rewritten.contains("Allow:"));
    }

    #[test]
    fn skips_allow_for_non_invite_success_responses() {
        let inspector = CapabilityHeadersInspector::new(allow_methods(vec![
            rsipstack::sip::Method::Invite,
            rsipstack::sip::Method::Register,
        ]));
        let msg = parse_response(
            concat!(
                "SIP/2.0 200 OK\r\n",
                "CSeq: 1 REGISTER\r\n",
                "Content-Length: 0\r\n\r\n"
            ),
        );

        let rewritten = inspector.before_send(msg, None).to_string();

        assert!(!rewritten.contains("Allow:"));
    }

    #[test]
    fn filters_non_call_methods_from_allow() {
        let inspector = CapabilityHeadersInspector::new(allow_methods(vec![
            rsipstack::sip::Method::Invite,
            rsipstack::sip::Method::Register,
            rsipstack::sip::Method::Subscribe,
            rsipstack::sip::Method::Publish,
            rsipstack::sip::Method::Options,
            rsipstack::sip::Method::Bye,
        ]));
        let msg = parse_request(
            "INVITE sip:alice@example.com SIP/2.0\r\nContent-Length: 0\r\n\r\n",
        );

        let rewritten = inspector.before_send(msg, None).to_string();

        assert!(rewritten.contains("Allow: INVITE,BYE\r\n"));
        assert!(!rewritten.contains("REGISTER"));
        assert!(!rewritten.contains("SUBSCRIBE"));
        assert!(!rewritten.contains("PUBLISH"));
        assert!(!rewritten.contains("OPTIONS"));
    }

}
