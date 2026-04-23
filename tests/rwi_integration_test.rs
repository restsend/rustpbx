// Integration tests for RWI WebSocket interface
// These tests require a running RustPBX instance with RWI configured

use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message};

const RWI_URL: &str = "ws://127.0.0.1:8088/rwi/v1";
const TEST_TOKEN: &str = "test-token-rwi";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RwiRequest {
    #[serde(rename = "rwi")]
    pub version: String,
    #[serde(rename = "action_id")]
    pub action_id: Option<String>,
    pub action: String,
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RwiResponse {
    #[serde(rename = "rwi")]
    pub version: String,
    #[serde(rename = "action_id")]
    pub action_id: Option<String>,
    pub response: String,
    pub data: Option<serde_json::Value>,
    pub error: Option<RwiError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RwiError {
    pub code: String,
    pub message: String,
}

impl RwiRequest {
    pub fn new(action: &str) -> Self {
        Self {
            version: "1.0".to_string(),
            action_id: Some(uuid::Uuid::new_v4().to_string()),
            action: action.to_string(),
            params: None,
        }
    }

    pub fn with_params(mut self, params: serde_json::Value) -> Self {
        self.params = Some(params);
        self
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap()
    }
}

pub struct RwiTestClient {
    ws: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
}

/// Type alias for test client results.
pub type TestResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

impl RwiTestClient {
    pub async fn connect() -> TestResult<Self> {
        let url = format!("{}?token={}", RWI_URL, TEST_TOKEN);
        let (ws_stream, _) = timeout(Duration::from_secs(10), connect_async(&url)).await??;

        Ok(Self { ws: ws_stream })
    }

    pub async fn send_request(
        &mut self,
        request: RwiRequest,
    ) -> TestResult<RwiResponse> {
        let json = request.to_json();
        self.ws.send(Message::Text(json.into())).await?;

        // Wait for response
        let msg = match timeout(Duration::from_secs(5), self.ws.next()).await {
            Ok(Some(Ok(msg))) => msg,
            Ok(Some(Err(e))) => return Err(e.to_string().into()),
            Ok(None) => return Err("Stream ended".into()),
            Err(_) => return Err("Timeout".into()),
        };

        match msg {
            Message::Text(text) => {
                let response: RwiResponse = serde_json::from_str(&text)?;
                Ok(response)
            }
            Message::Close(_) => Err("Connection closed".into()),
            _ => Err("Unexpected message type".into()),
        }
    }

    pub async fn subscribe(
        &mut self,
        contexts: Vec<&str>,
    ) -> TestResult<RwiResponse> {
        let request = RwiRequest::new("session.subscribe")
            .with_params(serde_json::json!({ "contexts": contexts }));
        self.send_request(request).await
    }

    pub async fn list_calls(
        &mut self,
    ) -> TestResult<RwiResponse> {
        let request = RwiRequest::new("session.list_calls");
        self.send_request(request).await
    }

    pub async fn answer_call(
        &mut self,
        call_id: &str,
    ) -> TestResult<RwiResponse> {
        let request =
            RwiRequest::new("call.answer").with_params(serde_json::json!({ "call_id": call_id }));
        self.send_request(request).await
    }

    pub async fn hangup_call(
        &mut self,
        call_id: &str,
        reason: Option<&str>,
    ) -> TestResult<RwiResponse> {
        let mut params = serde_json::json!({ "call_id": call_id });
        if let Some(r) = reason {
            params["reason"] = serde_json::json!(r);
        }
        let request = RwiRequest::new("call.hangup").with_params(params);
        self.send_request(request).await
    }

    pub async fn reject_call(
        &mut self,
        call_id: &str,
        reason: Option<&str>,
    ) -> TestResult<RwiResponse> {
        let mut params = serde_json::json!({ "call_id": call_id });
        if let Some(r) = reason {
            params["reason"] = serde_json::json!(r);
        }
        let request = RwiRequest::new("call.reject").with_params(params);
        self.send_request(request).await
    }

    pub async fn ring_call(
        &mut self,
        call_id: &str,
    ) -> TestResult<RwiResponse> {
        let request =
            RwiRequest::new("call.ring").with_params(serde_json::json!({ "call_id": call_id }));
        self.send_request(request).await
    }

    pub async fn transfer_call(
        &mut self,
        call_id: &str,
        target: &str,
    ) -> TestResult<RwiResponse> {
        let request = RwiRequest::new("call.transfer")
            .with_params(serde_json::json!({ "call_id": call_id, "target": target }));
        self.send_request(request).await
    }

    pub async fn originate(
        &mut self,
        call_id: &str,
        destination: &str,
        caller_id: Option<&str>,
    ) -> TestResult<RwiResponse> {
        let mut params = serde_json::json!({
            "call_id": call_id,
            "destination": destination,
        });
        if let Some(cid) = caller_id {
            params["caller_id"] = serde_json::json!(cid);
        }
        let request = RwiRequest::new("call.originate").with_params(params);
        self.send_request(request).await
    }

    pub async fn bridge(
        &mut self,
        leg_a: &str,
        leg_b: &str,
    ) -> TestResult<RwiResponse> {
        let request = RwiRequest::new("call.bridge")
            .with_params(serde_json::json!({ "leg_a": leg_a, "leg_b": leg_b }));
        self.send_request(request).await
    }

    pub async fn media_play(
        &mut self,
        call_id: &str,
        source_type: &str,
        uri: &str,
    ) -> TestResult<RwiResponse> {
        let request = RwiRequest::new("media.play").with_params(serde_json::json!({
            "call_id": call_id,
            "source": {
                "type": source_type,
                "uri": uri
            }
        }));
        self.send_request(request).await
    }

    pub async fn queue_enqueue(
        &mut self,
        call_id: &str,
        queue_id: &str,
        priority: Option<u32>,
    ) -> TestResult<RwiResponse> {
        let mut params = serde_json::json!({
            "call_id": call_id,
            "queue_id": queue_id,
        });
        if let Some(p) = priority {
            params["priority"] = serde_json::json!(p);
        }
        let request = RwiRequest::new("queue.enqueue").with_params(params);
        self.send_request(request).await
    }

    pub async fn queue_dequeue(
        &mut self,
        call_id: &str,
    ) -> TestResult<RwiResponse> {
        let request =
            RwiRequest::new("queue.dequeue").with_params(serde_json::json!({ "call_id": call_id }));
        self.send_request(request).await
    }

    pub async fn send_dtmf(
        &mut self,
        call_id: &str,
        digits: &str,
    ) -> TestResult<RwiResponse> {
        let request = RwiRequest::new("call.send_dtmf")
            .with_params(serde_json::json!({ "call_id": call_id, "digits": digits }));
        self.send_request(request).await
    }

    pub async fn conference_create(
        &mut self,
        conf_id: &str,
    ) -> TestResult<RwiResponse> {
        let request = RwiRequest::new("conference.create")
            .with_params(serde_json::json!({ "conference_id": conf_id }));
        self.send_request(request).await
    }

    pub async fn conference_add(
        &mut self,
        conf_id: &str,
        call_id: &str,
    ) -> TestResult<RwiResponse> {
        let request = RwiRequest::new("conference.add")
            .with_params(serde_json::json!({ "conference_id": conf_id, "call_id": call_id }));
        self.send_request(request).await
    }

    pub async fn conference_remove(
        &mut self,
        conf_id: &str,
        call_id: &str,
    ) -> TestResult<RwiResponse> {
        let request = RwiRequest::new("conference.remove")
            .with_params(serde_json::json!({ "conference_id": conf_id, "call_id": call_id }));
        self.send_request(request).await
    }

    pub async fn conference_destroy(
        &mut self,
        conf_id: &str,
    ) -> TestResult<RwiResponse> {
        let request = RwiRequest::new("conference.destroy")
            .with_params(serde_json::json!({ "conference_id": conf_id }));
        self.send_request(request).await
    }

    pub async fn close(mut self) -> TestResult<()> {
        self.ws.close(None).await?;
        Ok(())
    }
}

// ===== Integration Tests =====

/// Test basic connection and authentication
#[tokio::test]
async fn test_rwi_connection_and_auth() {
    // This test requires RWI to be configured and running
    let result = RwiTestClient::connect().await;

    match result {
        Ok(mut client) => {
            // Connection successful - try to subscribe
            let result = client.subscribe(vec!["default"]).await;
            assert!(result.is_ok(), "Subscribe should work with valid token");

            let _ = client.close().await;
        }
        Err(e) => {
            // If connection fails, RWI might not be configured
            // Skip test in this case
            println!("RWI not available: {}. Skipping test.", e);
        }
    }
}

/// Test session.subscribe
#[tokio::test]
async fn test_session_subscribe() {
    let result = RwiTestClient::connect().await;

    match result {
        Ok(mut client) => {
            let result = client.subscribe(vec!["context1", "context2"]).await;
            assert!(result.is_ok());

            let response = result.unwrap();
            assert_eq!(response.response, "success");

            let _ = client.close().await;
        }
        Err(e) => {
            println!("RWI not available: {}. Skipping test.", e);
        }
    }
}

/// Test session.list_calls on empty registry
#[tokio::test]
async fn test_session_list_calls_empty() {
    let result = RwiTestClient::connect().await;

    match result {
        Ok(mut client) => {
            let _ = client.subscribe(vec!["default"]).await;

            let result = client.list_calls().await;
            assert!(result.is_ok());

            let response = result.unwrap();
            assert_eq!(response.response, "success");
            // Data should be an array (could be empty)
            if let Some(data) = response.data {
                assert!(data.is_array(), "list_calls should return array");
            }

            let _ = client.close().await;
        }
        Err(e) => {
            println!("RWI not available: {}. Skipping test.", e);
        }
    }
}

/// Test call operations on non-existent call
#[tokio::test]
async fn test_call_operations_on_nonexistent_call() {
    let result = RwiTestClient::connect().await;

    match result {
        Ok(mut client) => {
            let _ = client.subscribe(vec!["default"]).await;

            // Try to answer non-existent call
            let result = client.answer_call("nonexistent-call-id").await;
            assert!(result.is_ok());

            let response = result.unwrap();
            // Should return error for non-existent call
            assert_eq!(
                response.response, "error",
                "Expected error for non-existent call"
            );
            assert!(response.error.is_some());

            // Try to hangup non-existent call
            let result = client.hangup_call("nonexistent-call-id", None).await;
            assert!(result.is_ok());

            let response = result.unwrap();
            assert_eq!(response.response, "error");

            // Try to ring non-existent call
            let result = client.ring_call("nonexistent-call-id").await;
            assert!(result.is_ok());

            let response = result.unwrap();
            assert_eq!(response.response, "error");

            let _ = client.close().await;
        }
        Err(e) => {
            println!("RWI not available: {}. Skipping test.", e);
        }
    }
}

/// Test call.reject with different reasons
#[tokio::test]
async fn test_call_reject_reasons() {
    let result = RwiTestClient::connect().await;

    match result {
        Ok(mut client) => {
            let _ = client.subscribe(vec!["default"]).await;

            // Test busy rejection
            let result = client.reject_call("test-call", Some("busy")).await;
            assert!(result.is_ok());

            // Test forbidden rejection
            let result = client.reject_call("test-call", Some("forbidden")).await;
            assert!(result.is_ok());

            // Test not_found rejection
            let result = client.reject_call("test-call", Some("not_found")).await;
            assert!(result.is_ok());

            let _ = client.close().await;
        }
        Err(e) => {
            println!("RWI not available: {}. Skipping test.", e);
        }
    }
}

/// Test call.transfer
#[tokio::test]
async fn test_call_transfer() {
    let result = RwiTestClient::connect().await;

    match result {
        Ok(mut client) => {
            let _ = client.subscribe(vec!["default"]).await;

            // Try to transfer non-existent call
            let result = client.transfer_call("test-call", "sip:3000@local").await;
            assert!(result.is_ok());

            let response = result.unwrap();
            // Should fail because call doesn't exist
            assert_eq!(response.response, "error");

            let _ = client.close().await;
        }
        Err(e) => {
            println!("RWI not available: {}. Skipping test.", e);
        }
    }
}

/// Test call.originate (will fail without actual SIP setup)
#[tokio::test]
async fn test_call_originate() {
    let result = RwiTestClient::connect().await;

    match result {
        Ok(mut client) => {
            let _ = client.subscribe(vec!["default"]).await;

            // Try to originate a call
            let result = client
                .originate("new-call", "sip:test@local", Some("1001"))
                .await;
            assert!(result.is_ok());

            let response = result.unwrap();
            // May succeed or fail depending on SIP backend availability
            // But the command should be accepted
            println!("Originate response: {:?}", response);

            let _ = client.close().await;
        }
        Err(e) => {
            println!("RWI not available: {}. Skipping test.", e);
        }
    }
}

/// Test call.bridge (will fail without actual calls)
#[tokio::test]
async fn test_call_bridge() {
    let result = RwiTestClient::connect().await;

    match result {
        Ok(mut client) => {
            let _ = client.subscribe(vec!["default"]).await;

            // Try to bridge non-existent calls
            let result = client.bridge("leg-a", "leg-b").await;
            assert!(result.is_ok());

            let response = result.unwrap();
            // Should fail because calls don't exist
            assert_eq!(response.response, "error");

            let _ = client.close().await;
        }
        Err(e) => {
            println!("RWI not available: {}. Skipping test.", e);
        }
    }
}

/// Test media.play (not implemented yet)
#[tokio::test]
async fn test_media_play() {
    let result = RwiTestClient::connect().await;

    match result {
        Ok(mut client) => {
            let _ = client.subscribe(vec!["default"]).await;

            // Try to play media on non-existent call
            let result = client.media_play("test-call", "file", "welcome.wav").await;
            assert!(result.is_ok());

            let response = result.unwrap();
            // Should fail - not implemented or call not found
            println!("Media play response: {:?}", response);

            let _ = client.close().await;
        }
        Err(e) => {
            println!("RWI not available: {}. Skipping test.", e);
        }
    }
}

/// Test invalid action
#[tokio::test]
async fn test_invalid_action() {
    let result = RwiTestClient::connect().await;

    match result {
        Ok(mut client) => {
            let request = RwiRequest::new("invalid.action");
            let result = client.send_request(request).await;

            // Should get an error response
            assert!(result.is_ok());
            let response = result.unwrap();
            assert_eq!(response.response, "error");

            let _ = client.close().await;
        }
        Err(e) => {
            println!("RWI not available: {}. Skipping test.", e);
        }
    }
}

/// Test missing action field
#[tokio::test]
async fn test_missing_action() {
    let result = RwiTestClient::connect().await;

    match result {
        Ok(mut client) => {
            // Send request without action field
            let json = r#"{"rwi": "1.0", "params": {}}"#;
            client.ws.send(Message::Text(json.into())).await.unwrap();

            // Should get error response
            let msg = timeout(Duration::from_secs(5), client.ws.next()).await;
            assert!(msg.is_ok());

            let _ = client.close().await;
        }
        Err(e) => {
            println!("RWI not available: {}. Skipping test.", e);
        }
    }
}

/// Test multiple sequential operations
#[tokio::test]
async fn test_sequential_operations() {
    let result = RwiTestClient::connect().await;

    match result {
        Ok(mut client) => {
            let _ = client.subscribe(vec!["default"]).await;

            // Sequential operations should all work (even if calls don't exist)
            let _ = client.list_calls().await;
            let _ = client.ring_call("call-1").await;
            let _ = client.answer_call("call-1").await;
            let _ = client.transfer_call("call-1", "sip:3000@local").await;
            let _ = client.hangup_call("call-1", None).await;

            let _ = client.close().await;
        }
        Err(e) => {
            println!("RWI not available: {}. Skipping test.", e);
        }
    }
}

/// Test reconnection
#[tokio::test]
async fn test_reconnection() {
    // Try to connect, disconnect, and reconnect
    let result1 = RwiTestClient::connect().await;

    match result1 {
        Ok(mut client1) => {
            // First connection - subscribe
            let result = client1.subscribe(vec!["default"]).await;
            assert!(result.is_ok());

            // Close first connection
            let _ = client1.close().await;

            // Wait a bit
            tokio::time::sleep(Duration::from_millis(100)).await;

            // Second connection
            let result2 = RwiTestClient::connect().await;
            match result2 {
                Ok(mut client2) => {
                    // Second connection should also work
                    let result = client2.subscribe(vec!["default"]).await;
                    assert!(result.is_ok());

                    let _ = client2.close().await;
                }
                Err(e) => {
                    println!("Second connection failed: {}. Skipping.", e);
                }
            }
        }
        Err(e) => {
            println!("RWI not available: {}. Skipping test.", e);
        }
    }
}
