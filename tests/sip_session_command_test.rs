// E2E Tests for SipSession CallCommand handlers
// These tests verify the command processing pipeline from RWI through to SipSession

// Import test utilities from rwi_integration_test
mod rwi_integration_test;
use rwi_integration_test::{RwiRequest, RwiTestClient};

/// Test helper to create a test call and return its ID
async fn create_test_call(client: &mut RwiTestClient) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let call_id = format!("test-call-{}", uuid::Uuid::new_v4());
    
    // Originate a call
    let response = client.originate(&call_id, "sip:test@example.com", Some("Test Caller")).await?;
    
    // Extract call_id from response
    if let Some(data) = response.data {
        if let Some(id) = data.get("call_id").and_then(|v| v.as_str()) {
            return Ok(id.to_string());
        }
    }
    
    // If we can't get the call_id from response, use the one we generated
    Ok(call_id)
}

/// Test Reject command through RWI interface
#[tokio::test]
async fn test_e2e_reject_command() {
    let result = RwiTestClient::connect().await;
    
    match result {
        Ok(mut client) => {
            // Subscribe to default context
            let _ = client.subscribe(vec!["default"]).await;
            
            // Create a test call
            let call_result = create_test_call(&mut client).await;
            let call_id = match call_result {
                Ok(id) => id,
                Err(_) => {
                    println!("Could not create test call, skipping");
                    let _ = client.close().await;
                    return;
                }
            };
            
            // Reject the call
            let reject_result = client.reject_call(&call_id, Some("User busy")).await;
            assert!(reject_result.is_ok(), "Reject should succeed");
            
            let response = reject_result.unwrap();
            assert_eq!(response.response, "Success", "Reject should return Success");
            
            let _ = client.close().await;
        }
        Err(e) => {
            println!("RWI not available: {}. Skipping test.", e);
        }
    }
}

/// Test Ring command through RWI interface
#[tokio::test]
async fn test_e2e_ring_command() {
    let result = RwiTestClient::connect().await;
    
    match result {
        Ok(mut client) => {
            let _ = client.subscribe(vec!["default"]).await;
            
            let call_result = create_test_call(&mut client).await;
            let call_id = match call_result {
                Ok(id) => id,
                Err(_) => {
                    println!("Could not create test call, skipping");
                    let _ = client.close().await;
                    return;
                }
            };
            
            // Send ring command
            let ring_result = client.ring_call(&call_id).await;
            assert!(ring_result.is_ok(), "Ring should succeed");
            
            let response = ring_result.unwrap();
            assert_eq!(response.response, "Success", "Ring should return Success");
            
            // Cleanup
            let _ = client.hangup_call(&call_id, None).await;
            let _ = client.close().await;
        }
        Err(e) => {
            println!("RWI not available: {}. Skipping test.", e);
        }
    }
}

/// Test Transfer command through RWI interface
#[tokio::test]
async fn test_e2e_transfer_command() {
    let result = RwiTestClient::connect().await;
    
    match result {
        Ok(mut client) => {
            let _ = client.subscribe(vec!["default"]).await;
            
            // Create source call
            let source_call_result = create_test_call(&mut client).await;
            let source_call_id = match source_call_result {
                Ok(id) => id,
                Err(_) => {
                    println!("Could not create test call, skipping");
                    let _ = client.close().await;
                    return;
                }
            };
            
            // Transfer the call
            let transfer_result = client.transfer_call(&source_call_id, "sip:target@example.com").await;
            assert!(transfer_result.is_ok(), "Transfer should succeed");
            
            let response = transfer_result.unwrap();
            assert_eq!(response.response, "Success", "Transfer should return Success");
            
            let _ = client.close().await;
        }
        Err(e) => {
            println!("RWI not available: {}. Skipping test.", e);
        }
    }
}

/// Test Hold/Unhold commands through RWI interface
#[tokio::test]
async fn test_e2e_hold_unhold_commands() {
    let result = RwiTestClient::connect().await;
    
    match result {
        Ok(mut client) => {
            let _ = client.subscribe(vec!["default"]).await;
            
            let call_result = create_test_call(&mut client).await;
            let call_id = match call_result {
                Ok(id) => id,
                Err(_) => {
                    println!("Could not create test call, skipping");
                    let _ = client.close().await;
                    return;
                }
            };
            
            // Test Hold
            let hold_request = RwiRequest::new("call.hold")
                .with_params(serde_json::json!({ "call_id": call_id }));
            let hold_result = client.send_request(hold_request).await;
            assert!(hold_result.is_ok(), "Hold should succeed");
            
            // Test Unhold
            let unhold_request = RwiRequest::new("call.unhold")
                .with_params(serde_json::json!({ "call_id": call_id }));
            let unhold_result = client.send_request(unhold_request).await;
            assert!(unhold_result.is_ok(), "Unhold should succeed");
            
            // Cleanup
            let _ = client.hangup_call(&call_id, None).await;
            let _ = client.close().await;
        }
        Err(e) => {
            println!("RWI not available: {}. Skipping test.", e);
        }
    }
}

/// Test Bridge command through RWI interface
#[tokio::test]
async fn test_e2e_bridge_command() {
    let result = RwiTestClient::connect().await;
    
    match result {
        Ok(mut client) => {
            let _ = client.subscribe(vec!["default"]).await;
            
            // Create two test calls
            let leg_a_result = create_test_call(&mut client).await;
            let leg_a = match leg_a_result {
                Ok(id) => id,
                Err(_) => {
                    println!("Could not create test call, skipping");
                    let _ = client.close().await;
                    return;
                }
            };
            
            let leg_b_result = create_test_call(&mut client).await;
            let leg_b = match leg_b_result {
                Ok(id) => id,
                Err(_) => {
                    println!("Could not create test call, skipping");
                    let _ = client.close().await;
                    return;
                }
            };
            
            // Bridge the calls
            let bridge_result = client.bridge(&leg_a, &leg_b).await;
            assert!(bridge_result.is_ok(), "Bridge should succeed");
            
            let response = bridge_result.unwrap();
            assert_eq!(response.response, "Success", "Bridge should return Success");
            
            // Cleanup
            let _ = client.hangup_call(&leg_a, None).await;
            let _ = client.hangup_call(&leg_b, None).await;
            let _ = client.close().await;
        }
        Err(e) => {
            println!("RWI not available: {}. Skipping test.", e);
        }
    }
}

/// Test MediaPlay command through RWI interface
#[tokio::test]
async fn test_e2e_media_play_command() {
    let result = RwiTestClient::connect().await;
    
    match result {
        Ok(mut client) => {
            let _ = client.subscribe(vec!["default"]).await;
            
            let call_result = create_test_call(&mut client).await;
            let call_id = match call_result {
                Ok(id) => id,
                Err(_) => {
                    println!("Could not create test call, skipping");
                    let _ = client.close().await;
                    return;
                }
            };
            
            // Play media
            let play_result = client.media_play(&call_id, "file", "/tmp/test.wav").await;
            assert!(play_result.is_ok(), "Media play should succeed");
            
            let response = play_result.unwrap();
            assert_eq!(response.response, "Success", "Media play should return Success");
            
            // Cleanup
            let _ = client.hangup_call(&call_id, None).await;
            let _ = client.close().await;
        }
        Err(e) => {
            println!("RWI not available: {}. Skipping test.", e);
        }
    }
}

/// Test Recording commands through RWI interface
#[tokio::test]
async fn test_e2e_recording_commands() {
    let result = RwiTestClient::connect().await;
    
    match result {
        Ok(mut client) => {
            let _ = client.subscribe(vec!["default"]).await;
            
            let call_result = create_test_call(&mut client).await;
            let call_id = match call_result {
                Ok(id) => id,
                Err(_) => {
                    println!("Could not create test call, skipping");
                    let _ = client.close().await;
                    return;
                }
            };
            
            // Start recording
            let record_start_request = RwiRequest::new("record.start")
                .with_params(serde_json::json!({
                    "call_id": call_id,
                    "storage": {
                        "type": "file",
                        "path": "/tmp/test-recording.wav"
                    }
                }));
            let record_result = client.send_request(record_start_request).await;
            assert!(record_result.is_ok(), "Record start should succeed");
            
            // Stop recording
            let record_stop_request = RwiRequest::new("record.stop")
                .with_params(serde_json::json!({ "call_id": call_id }));
            let stop_result = client.send_request(record_stop_request).await;
            assert!(stop_result.is_ok(), "Record stop should succeed");
            
            // Cleanup
            let _ = client.hangup_call(&call_id, None).await;
            let _ = client.close().await;
        }
        Err(e) => {
            println!("RWI not available: {}. Skipping test.", e);
        }
    }
}

/// Test Queue operations through RWI interface
#[tokio::test]
async fn test_e2e_queue_operations() {
    let result = RwiTestClient::connect().await;
    
    match result {
        Ok(mut client) => {
            let _ = client.subscribe(vec!["default"]).await;
            
            let call_result = create_test_call(&mut client).await;
            let call_id = match call_result {
                Ok(id) => id,
                Err(_) => {
                    println!("Could not create test call, skipping");
                    let _ = client.close().await;
                    return;
                }
            };
            
            // Enqueue
            let enqueue_request = RwiRequest::new("queue.enqueue")
                .with_params(serde_json::json!({
                    "call_id": call_id,
                    "queue_id": "test-queue",
                    "priority": 1
                }));
            let enqueue_result = client.send_request(enqueue_request).await;
            assert!(enqueue_result.is_ok(), "Queue enqueue should succeed");
            
            // Dequeue
            let dequeue_request = RwiRequest::new("queue.dequeue")
                .with_params(serde_json::json!({ "call_id": call_id }));
            let dequeue_result = client.send_request(dequeue_request).await;
            assert!(dequeue_result.is_ok(), "Queue dequeue should succeed");
            
            // Cleanup
            let _ = client.hangup_call(&call_id, None).await;
            let _ = client.close().await;
        }
        Err(e) => {
            println!("RWI not available: {}. Skipping test.", e);
        }
    }
}

/// Test DTMF sending through RWI interface
#[tokio::test]
async fn test_e2e_dtmf_command() {
    let result = RwiTestClient::connect().await;
    
    match result {
        Ok(mut client) => {
            let _ = client.subscribe(vec!["default"]).await;
            
            let call_result = create_test_call(&mut client).await;
            let call_id = match call_result {
                Ok(id) => id,
                Err(_) => {
                    println!("Could not create test call, skipping");
                    let _ = client.close().await;
                    return;
                }
            };
            
            // Send DTMF
            let dtmf_request = RwiRequest::new("call.send_dtmf")
                .with_params(serde_json::json!({
                    "call_id": call_id,
                    "digits": "1234"
                }));
            let dtmf_result = client.send_request(dtmf_request).await;
            assert!(dtmf_result.is_ok(), "DTMF send should succeed");
            
            // Cleanup
            let _ = client.hangup_call(&call_id, None).await;
            let _ = client.close().await;
        }
        Err(e) => {
            println!("RWI not available: {}. Skipping test.", e);
        }
    }
}

/// Test Conference operations through RWI interface
#[tokio::test]
async fn test_e2e_conference_operations() {
    let result = RwiTestClient::connect().await;
    
    match result {
        Ok(mut client) => {
            let _ = client.subscribe(vec!["default"]).await;
            
            let conf_id = format!("test-conf-{}", uuid::Uuid::new_v4());
            
            // Create conference
            let conf_create_request = RwiRequest::new("conference.create")
                .with_params(serde_json::json!({
                    "conf_id": conf_id,
                    "options": {
                        "max_participants": 10,
                        "record": false
                    }
                }));
            let create_result = client.send_request(conf_create_request).await;
            assert!(create_result.is_ok(), "Conference create should succeed");
            
            let call_result = create_test_call(&mut client).await;
            let call_id = match call_result {
                Ok(id) => id,
                Err(_) => {
                    println!("Could not create test call, skipping");
                    let _ = client.close().await;
                    return;
                }
            };
            
            // Add to conference
            let conf_add_request = RwiRequest::new("conference.add")
                .with_params(serde_json::json!({
                    "conf_id": conf_id,
                    "call_id": call_id
                }));
            let add_result = client.send_request(conf_add_request).await;
            assert!(add_result.is_ok(), "Conference add should succeed");
            
            // Mute in conference
            let conf_mute_request = RwiRequest::new("conference.mute")
                .with_params(serde_json::json!({
                    "conf_id": conf_id,
                    "call_id": call_id
                }));
            let mute_result = client.send_request(conf_mute_request).await;
            assert!(mute_result.is_ok(), "Conference mute should succeed");
            
            // Unmute in conference
            let conf_unmute_request = RwiRequest::new("conference.unmute")
                .with_params(serde_json::json!({
                    "conf_id": conf_id,
                    "call_id": call_id
                }));
            let unmute_result = client.send_request(conf_unmute_request).await;
            assert!(unmute_result.is_ok(), "Conference unmute should succeed");
            
            // Remove from conference
            let conf_remove_request = RwiRequest::new("conference.remove")
                .with_params(serde_json::json!({
                    "conf_id": conf_id,
                    "call_id": call_id
                }));
            let remove_result = client.send_request(conf_remove_request).await;
            assert!(remove_result.is_ok(), "Conference remove should succeed");
            
            // Destroy conference
            let conf_destroy_request = RwiRequest::new("conference.destroy")
                .with_params(serde_json::json!({ "conf_id": conf_id }));
            let destroy_result = client.send_request(conf_destroy_request).await;
            assert!(destroy_result.is_ok(), "Conference destroy should succeed");
            
            // Cleanup
            let _ = client.hangup_call(&call_id, None).await;
            let _ = client.close().await;
        }
        Err(e) => {
            println!("RWI not available: {}. Skipping test.", e);
        }
    }
}
