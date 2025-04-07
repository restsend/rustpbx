mod webrtc_test;

use crate::handler::call::CallHandlerState;
use crate::handler::sip::make_sip_call;
use crate::handler::sip::{MediaType, SipCallRequest};
use crate::useragent::UserAgentBuilder;
use axum::{
    extract::{Extension, State},
    response::Response,
    Json,
};
use std::sync::Arc;

#[tokio::test]
async fn test_sip_call() {
    // Create the UserAgent
    let ua_builder = UserAgentBuilder::new();
    let ua = Arc::new(ua_builder.build().await.unwrap());

    // Create the state
    let state = CallHandlerState::new();

    // Create a SIP call request
    let call_request = SipCallRequest {
        callee: "sip:test@example.com".to_string(),
        username: Some("testuser".to_string()),
        password: Some("testpass".to_string()),
        media_type: MediaType::Rtp,
    };

    // Directly call our handler function with the correct arguments
    let response = make_sip_call(State(state), Extension(ua.clone()), Json(call_request)).await;

    // Convert response to bytes and parse the JSON to verify we got a call_id
    let body_bytes = response_to_bytes(response).await;
    let body_str = String::from_utf8(body_bytes).unwrap();
    let json: serde_json::Value = serde_json::from_str(&body_str).unwrap();

    // Verify we got a call_id and status
    assert!(json.get("call_id").is_some());
    assert_eq!(json.get("status").unwrap(), "initiated");

    // Wait a bit to let the background task complete
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
}

// Helper function to convert axum response to bytes
async fn response_to_bytes(response: Response) -> Vec<u8> {
    let body = response.into_body();
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    bytes.to_vec()
}
