use super::super::ivr::{IvrPlan, IvrStep, PromptFile, PromptMedia, PromptSource, PromptStep};
use super::super::{DialDirection, Dialplan, DialplanIvrConfig};
use rsip::{Headers, Method, Request, Version};
use std::collections::HashMap;

fn mock_request() -> Request {
    Request {
        method: Method::Invite,
        uri: "sip:1000@rustpbx.com".try_into().unwrap(),
        version: Version::V2,
        headers: Headers::default(),
        body: Vec::new(),
    }
}

fn sample_plan() -> IvrPlan {
    let mut steps = HashMap::new();
    steps.insert(
        "welcome".to_string(),
        IvrStep::Prompt(PromptStep {
            prompts: vec![PromptMedia {
                source: PromptSource::File {
                    file: PromptFile {
                        path: "prompts/welcome.wav".to_string(),
                    },
                },
                locale: None,
                loop_count: None,
            }],
            allow_barge_in: true,
            next: None,
            availability: None,
        }),
    );

    IvrPlan {
        id: "main-menu".to_string(),
        version: 1,
        entry_step: "welcome".to_string(),
        steps,
        calendars: HashMap::new(),
    }
}

#[test]
fn dialplan_with_ivr_is_not_empty() {
    let req = mock_request();
    let plan = Dialplan::new("session-ivr".to_string(), req, DialDirection::Inbound);
    assert!(plan.is_empty());

    let plan = plan.with_ivr(DialplanIvrConfig::from_plan_id("main-menu"));
    assert!(plan.has_ivr());
    assert!(!plan.is_empty());
}

#[test]
fn dialplan_inline_plan_is_stored() {
    let req = mock_request();
    let ivr_plan = sample_plan();
    let plan = Dialplan::new(
        "session-ivr-inline".to_string(),
        req,
        DialDirection::Inbound,
    )
    .with_ivr(DialplanIvrConfig::new().with_inline_plan(ivr_plan.clone()));

    let ivr = plan.ivr_config().expect("ivr config missing");
    assert_eq!(ivr.plan_id.as_deref(), Some("main-menu"));
    let stored = ivr.plan.clone().expect("inline plan missing");
    assert_eq!(stored.id, ivr_plan.id);
    assert_eq!(stored.entry_step, "welcome");
}
