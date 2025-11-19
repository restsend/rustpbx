use super::super::{
    DialDirection, Dialplan, DialplanFlow, DialplanIvrConfig, QueueFallbackAction, QueueHoldConfig,
    QueuePlan,
};
use rsip::{Headers, Method, Request, Version};

fn mock_request() -> Request {
    Request {
        method: Method::Invite,
        uri: "sip:1000@rustpbx.com".try_into().unwrap(),
        version: Version::V2,
        headers: Headers::default(),
        body: Vec::new(),
    }
}

#[test]
fn queue_hold_config_loops_by_default() {
    let cfg = QueueHoldConfig::default();
    assert!(cfg.loop_playback);
    assert!(cfg.audio_file.is_none());

    let cfg = cfg
        .with_audio_file("moh.wav".to_string())
        .with_loop_playback(false);
    assert_eq!(cfg.audio_file.as_deref(), Some("moh.wav"));
    assert!(!cfg.loop_playback);
}

#[test]
fn dialplan_reports_queue_presence() {
    let req = mock_request();
    let plan = Dialplan::new("session-1".to_string(), req, DialDirection::Inbound);
    assert!(!plan.has_queue());

    let queue = QueuePlan {
        accept_immediately: true,
        hold: Some(QueueHoldConfig::default().with_audio_file("moh.wav".to_string())),
        fallback: Some(QueueFallbackAction::Redirect {
            target: "sip:vm@rustpbx.com".try_into().unwrap(),
        }),
        dial_strategy: None,
        ring_timeout: None,
    };

    let plan = plan.with_queue(queue);
    assert!(plan.has_queue());
}

#[test]
fn queue_wraps_terminal_flow() {
    let req = mock_request();
    let queue = QueuePlan {
        accept_immediately: false,
        hold: None,
        fallback: None,
        dial_strategy: None,
        ring_timeout: None,
    };

    let plan = Dialplan::new(
        "session-queue-flow".to_string(),
        req,
        DialDirection::Inbound,
    )
    .with_queue(queue)
    .with_ivr(DialplanIvrConfig::from_plan_id("menu"));

    match &plan.flow {
        DialplanFlow::Queue { next, .. } => match next.as_ref() {
            DialplanFlow::Ivr(config) => {
                assert_eq!(config.plan_id.as_deref(), Some("menu"));
            }
            _ => panic!("queue next flow should be IVR"),
        },
        _ => panic!("dialplan flow should start with queue"),
    }
}
