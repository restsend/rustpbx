use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use rustpbx::call::app::ivr::config::EntryAction;
use rustpbx::call::app::ivr::config::IvrProviderConfig;
use rustpbx::call::app::ivr::{
    ActionProvider, ProviderContext, ProviderEvent, RetryConfig, StepProvider,
};

#[test]
fn step_provider_uses_configured_timeout_and_retry_delay() {
    let config = IvrProviderConfig {
        url: "http://127.0.0.1:28080/ivr/step".into(),
        headers: HashMap::new(),
        max_retries: 3,
        retry_delay_ms: 250,
        timeout_secs: 10,
        fallback_action: None,
    };

    let retry = RetryConfig::from(&config);

    assert_eq!(retry.max_retries, 3);
    assert_eq!(retry.timeout_ms, 10_000);
    assert_eq!(retry.retry_delay_ms, 250);
    assert!(retry.fallback_action.is_none());

    let with_fallback = IvrProviderConfig {
        fallback_action: Some(rustpbx::call::app::ivr::config::ActionNode::new(
            EntryAction::Hangup {
                prompt: Some("sounds/error.wav".into()),
                prompt_text: None,
                prompt_voice: None,
            },
        )),
        ..config
    };
    let retry = RetryConfig::from(&with_fallback);
    assert!(retry.fallback_action.is_some());
}

async fn record_attempt(
    State(request_times): State<Arc<Mutex<Vec<Instant>>>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let attempt = {
        let mut request_times = request_times.lock().unwrap();
        request_times.push(Instant::now());
        request_times.len()
    };

    if attempt == 1 {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "retry"})),
        )
    } else {
        (StatusCode::OK, Json(serde_json::json!({"type": "hangup"})))
    }
}

#[tokio::test]
async fn step_provider_waits_for_configured_retry_delay() {
    let request_times = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/ivr/step", post(record_attempt))
        .with_state(Arc::clone(&request_times));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let provider = StepProvider::new(format!("http://{addr}/ivr/step"), reqwest::Client::new())
        .with_retry(RetryConfig {
            max_retries: 2,
            timeout_ms: 1_000,
            retry_delay_ms: 250,
            fallback_action: None,
        });
    let context = ProviderContext {
        session_id: "retry-delay-test".into(),
        app_execution_id: 1,
        caller: "1001".into(),
        callee: "2000".into(),
        direction: "inbound".into(),
        tenant_id: None,
        ivr_id: None,
        variables: HashMap::new(),
        sip_headers: None,
        event: Some(ProviderEvent::SessionStart),
        route_name: None,
        custom_data: None,
        step_start_time: None,
        step_end_time: None,
        step_duration_ms: None,
        step_index: None,
        transferred_from: None,
    };

    let action = tokio::time::timeout(Duration::from_secs(2), provider.next_action(context))
        .await
        .expect("provider retry loop timed out")
        .expect("second provider attempt should succeed");
    server.abort();

    assert!(matches!(action.action, EntryAction::Hangup { .. }));
    let request_times = request_times.lock().unwrap();
    assert_eq!(request_times.len(), 2);
    let observed_delay = request_times[1].duration_since(request_times[0]);
    assert!(
        observed_delay >= Duration::from_millis(200),
        "configured 250ms retry delay was not honored; observed {observed_delay:?}"
    );
}
