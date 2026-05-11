use crate::call::{DialStrategy, Dialplan, DialplanFlow, Location};
use crate::config::EmergencyConfig;
use crate::proxy::call::{DialplanInspector, RouteError};
use async_trait::async_trait;
use rsipstack::sip::prelude::HeadersExt;
use rsipstack::sip::Request;
use tracing::info;

pub struct EmergencyInspector {
    config: Option<EmergencyConfig>,
}

impl EmergencyInspector {
    pub fn new(config: Option<EmergencyConfig>) -> Self {
        Self { config }
    }
}

impl Default for EmergencyInspector {
    fn default() -> Self {
        Self::new(None)
    }
}

#[async_trait]
impl DialplanInspector for EmergencyInspector {
    async fn inspect_dialplan(
        &self,
        mut dialplan: Dialplan,
        _cookie: &crate::call::TransactionCookie,
        original: &Request,
    ) -> Result<Dialplan, RouteError> {
        let cfg = match self.config.as_ref() {
            Some(c) => c,
            None => return Ok(dialplan),
        };

        if !cfg.enabled {
            return Ok(dialplan);
        }

        if original.method() != &rsipstack::sip::Method::Invite {
            return Ok(dialplan);
        }

        let callee = match original.to_header() {
            Ok(to) => match to.uri() {
                Ok(uri) => uri.user().unwrap_or_default().to_string(),
                Err(_) => return Ok(dialplan),
            },
            Err(_) => return Ok(dialplan),
        };

        if cfg.numbers.iter().any(|n| callee.contains(n.as_str())) {
            info!(
                "Emergency call detected: callee={}, routing to trunk {}",
                callee, cfg.emergency_trunk
            );
            if let Ok(trunk_uri) = rsipstack::sip::Uri::try_from(cfg.emergency_trunk.as_str()) {
                dialplan.flow = DialplanFlow::Targets(DialStrategy::Sequential(vec![Location {
                    aor: trunk_uri,
                    ..Default::default()
                }]));
            }
        }

        Ok(dialplan)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::call::DialplanInspector;

    fn emg_request(callee: &str) -> Request {
        let hp = rsipstack::sip::HostWithPort {
            host: "127.0.0.1".parse().unwrap(),
            port: Some(5060.into()),
        };

        let uri = rsipstack::sip::Uri {
            scheme: Some(rsipstack::sip::Scheme::Sip),
            auth: None,
            host_with_port: hp.clone(),
            params: vec![],
            headers: vec![],
        };

        let callee_uri = rsipstack::sip::Uri {
            scheme: Some(rsipstack::sip::Scheme::Sip),
            auth: Some(rsipstack::sip::Auth {
                user: callee.to_string(),
                password: None,
            }),
            host_with_port: hp.clone(),
            params: vec![],
            headers: vec![],
        };

        let from = rsipstack::sip::typed::From {
            display_name: None,
            uri: uri.clone(),
            params: vec![rsipstack::sip::Param::Tag(rsipstack::sip::param::Tag::new("t1"))],
        };

        let to = rsipstack::sip::typed::To {
            display_name: None,
            uri: callee_uri,
            params: vec![],
        };

        Request {
            method: rsipstack::sip::Method::Invite,
            uri: uri.clone(),
            version: rsipstack::sip::Version::V2,
            headers: vec![
                from.into(),
                to.into(),
                rsipstack::sip::headers::Via::new(
                    "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-b",
                )
                .into(),
                rsipstack::sip::headers::CallId::new("c1").into(),
                rsipstack::sip::headers::typed::CSeq {
                    seq: 1,
                    method: rsipstack::sip::Method::Invite,
                }
                .into(),
                rsipstack::sip::typed::Contact {
                    display_name: None,
                    uri,
                    params: vec![],
                }
                .into(),
            ]
            .into(),
            body: vec![],
        }
    }

    fn empty_targets(flow: &DialplanFlow) -> bool {
        matches!(
            flow,
            DialplanFlow::Targets(s)
                if matches!(s, DialStrategy::Sequential(v) if v.is_empty())
        )
    }

    fn make_cfg(enabled: bool, numbers: &[&str], trunk: &str) -> Option<EmergencyConfig> {
        Some(EmergencyConfig {
            enabled,
            numbers: numbers.iter().map(|s| s.to_string()).collect(),
            emergency_trunk: trunk.to_string(),
        })
    }

    #[tokio::test]
    async fn test_emergency_matches_911() {
        let inspector = EmergencyInspector::new(make_cfg(true, &["911", "999"], "sip:emg@pbx.com"));
        let req = emg_request("911");
        let dp = Dialplan::new("s".into(), req.clone(), crate::call::DialDirection::Inbound);
        let r = inspector.inspect_dialplan(dp, &Default::default(), &req).await.unwrap();
        match r.flow {
            DialplanFlow::Targets(s) => {
                let t = match s {
                    DialStrategy::Sequential(t) => t,
                    _ => vec![],
                };
                assert_eq!(t.len(), 1);
                assert_eq!(t[0].aor.to_string(), "sip:emg@pbx.com");
            }
            _ => panic!("expected Targets"),
        }
    }

    #[tokio::test]
    async fn test_emergency_no_match() {
        let inspector = EmergencyInspector::new(make_cfg(true, &["911"], "sip:emg@pbx.com"));
        let req = emg_request("14085551234");
        let dp = Dialplan::new("s".into(), req.clone(), crate::call::DialDirection::Inbound);
        let r = inspector.inspect_dialplan(dp, &Default::default(), &req).await.unwrap();
        assert!(empty_targets(&r.flow));
    }

    #[tokio::test]
    async fn test_emergency_disabled() {
        let inspector = EmergencyInspector::new(make_cfg(false, &["911"], "sip:t@c.com"));
        let req = emg_request("911");
        let dp = Dialplan::new("s".into(), req.clone(), crate::call::DialDirection::Inbound);
        let r = inspector.inspect_dialplan(dp, &Default::default(), &req).await.unwrap();
        assert!(empty_targets(&r.flow));
    }

    #[tokio::test]
    async fn test_emergency_no_config() {
        let inspector = EmergencyInspector::new(None);
        let req = emg_request("911");
        let dp = Dialplan::new("s".into(), req.clone(), crate::call::DialDirection::Inbound);
        let r = inspector.inspect_dialplan(dp, &Default::default(), &req).await.unwrap();
        assert!(empty_targets(&r.flow));
    }

    #[tokio::test]
    async fn test_emergency_not_invite() {
        let inspector = EmergencyInspector::new(make_cfg(true, &["911"], "sip:t@c.com"));
        let mut req = emg_request("911");
        req.method = rsipstack::sip::Method::Register;
        let dp = Dialplan::new("s".into(), req.clone(), crate::call::DialDirection::Inbound);
        let r = inspector.inspect_dialplan(dp, &Default::default(), &req).await.unwrap();
        assert!(empty_targets(&r.flow));
    }
}
