use crate::proxy::tests::common::outbound_failure_with;

async fn outbound_failure(
    from: &str,
    auth_user: Option<&str>,
    contact: &str,
    status: u16,
) -> Vec<crate::rwi::EventCacheEntry> {
    outbound_failure_with(from, auth_user, contact, status, true, |_| {}, async {}).await
}

#[tokio::test]
async fn outbound_failure_reports_agent_hangup_from_real_setup() {
    let mut cases = Vec::new();
    for status in [404, 486, 408, 487] {
        cases.push((
            status,
            outbound_failure("agent-a", Some("agent-a"), "agent-a", status).await,
        ));
    }
    for (status, events) in cases {
        let hangups: Vec<_> = events
            .iter()
            .filter(|e| e.event.event_type == "call_hangup")
            .collect();
        assert_eq!(hangups.len(), 2, "status={status}: {hangups:?}");
        assert_eq!(hangups[0].event.payload["scope"], "leg");
        assert_eq!(hangups[0].event.payload["leg_id"], "caller");
        assert_eq!(hangups[0].event.payload["leg_role"], "agent");
        assert_eq!(hangups[0].event.payload["agent_id"], "agent-a");
        assert_eq!(hangups[0].event.payload["duration_secs"], 0);
        assert_eq!(hangups[1].event.payload["scope"], "call");
    }
}

#[tokio::test]
async fn outbound_failure_created_uses_the_resolved_originating_agent() {
    for (from, auth, contact) in [
        ("hotline", Some("agent-a"), "agent-a"),
        ("agent-b", Some("agent-a"), "agent-b"),
        ("hotline", None, "agent-a"),
    ] {
        let events = outbound_failure(from, auth, contact, 486).await;
        let created = events
            .iter()
            .find(|e| e.event.event_type == "call_created")
            .unwrap();
        assert_eq!(
            created.event.payload["leg_role"], "agent",
            "from={from}, auth={auth:?}"
        );
        assert_eq!(created.event.payload["agent_id"], "agent-a");
        let hangups: Vec<_> = events
            .iter()
            .filter(|e| e.event.event_type == "call_hangup")
            .collect();
        assert_eq!(hangups.len(), 2);
        assert_eq!(hangups[0].event.payload["leg_role"], "agent");
        assert_eq!(hangups[0].event.payload["agent_id"], "agent-a");
    }
}

#[tokio::test]
async fn outbound_failure_non_agent_stays_session_scoped() {
    let events = outbound_failure("external", None, "external", 486).await;
    let created = events
        .iter()
        .find(|e| e.event.event_type == "call_created")
        .unwrap();
    assert_eq!(created.event.payload["leg_role"], "caller");
    let hangups: Vec<_> = events
        .iter()
        .filter(|e| e.event.event_type == "call_hangup")
        .collect();
    assert_eq!(hangups.len(), 1);
    assert_eq!(hangups[0].event.payload["scope"], "call");
    assert!(hangups[0].event.payload.get("leg_role").is_none());
}
