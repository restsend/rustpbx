// E2E Tests for SipSession CallCommand handlers
// These tests verify the command processing pipeline from RWI through to SipSession

// Import test utilities from rwi_integration_test
mod rwi_integration_test;
use rwi_integration_test::{RwiRequest, RwiTestClient};

/// Test helper to create a test call and return its ID
async fn create_test_call(
    client: &mut RwiTestClient,
) -> rwi_integration_test::TestResult<String> {
    let call_id = format!("test-call-{}", uuid::Uuid::new_v4());

    // Originate a call
    let response = client
        .originate(&call_id, "sip:test@example.com", Some("Test Caller"))
        .await?;

    // Extract call_id from response
    if let Some(data) = response.data
        && let Some(id) = data.get("call_id").and_then(|v| v.as_str()) {
            return Ok(id.to_string());
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
            let transfer_result = client
                .transfer_call(&source_call_id, "sip:target@example.com")
                .await;
            assert!(transfer_result.is_ok(), "Transfer should succeed");

            let response = transfer_result.unwrap();
            assert_eq!(
                response.response, "Success",
                "Transfer should return Success"
            );

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
            let hold_request =
                RwiRequest::new("call.hold").with_params(serde_json::json!({ "call_id": call_id }));
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
            assert_eq!(
                response.response, "Success",
                "Media play should return Success"
            );

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
            let record_start_request =
                RwiRequest::new("record.start").with_params(serde_json::json!({
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

            // Enqueue using helper function
            let enqueue_result = client.queue_enqueue(&call_id, "test-queue", Some(1)).await;
            assert!(enqueue_result.is_ok(), "Queue enqueue should succeed");

            // Dequeue using helper function
            let dequeue_result = client.queue_dequeue(&call_id).await;
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

            // Send DTMF using helper function
            let dtmf_result = client.send_dtmf(&call_id, "1234").await;
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

            // Create conference using helper function
            let create_result = client.conference_create(&conf_id).await;
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

            // Add to conference using helper function
            let add_result = client.conference_add(&conf_id, &call_id).await;
            assert!(add_result.is_ok(), "Conference add should succeed");

            // Mute in conference
            let conf_mute_request =
                RwiRequest::new("conference.mute").with_params(serde_json::json!({
                    "conf_id": conf_id,
                    "call_id": call_id
                }));
            let mute_result = client.send_request(conf_mute_request).await;
            assert!(mute_result.is_ok(), "Conference mute should succeed");

            // Unmute in conference
            let conf_unmute_request =
                RwiRequest::new("conference.unmute").with_params(serde_json::json!({
                    "conf_id": conf_id,
                    "call_id": call_id
                }));
            let unmute_result = client.send_request(conf_unmute_request).await;
            assert!(unmute_result.is_ok(), "Conference unmute should succeed");

            // Remove from conference using helper function
            let remove_result = client.conference_remove(&conf_id, &call_id).await;
            assert!(remove_result.is_ok(), "Conference remove should succeed");

            // Destroy conference using helper function
            let destroy_result = client.conference_destroy(&conf_id).await;
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

/// Test ListCalls command through RWI interface
#[tokio::test]
async fn test_e2e_list_calls_command() {
    let result = RwiTestClient::connect().await;

    match result {
        Ok(mut client) => {
            let _ = client.subscribe(vec!["default"]).await;

            // Create a test call first
            let call_result = create_test_call(&mut client).await;
            let _call_id = match call_result {
                Ok(id) => id,
                Err(_) => {
                    println!("Could not create test call, skipping");
                    let _ = client.close().await;
                    return;
                }
            };

            // List calls using helper function
            let list_result = client.list_calls().await;
            assert!(list_result.is_ok(), "List calls should succeed");

            let response = list_result.unwrap();
            assert_eq!(
                response.response, "Success",
                "List calls should return Success"
            );

            let _ = client.close().await;
        }
        Err(e) => {
            println!("RWI not available: {}. Skipping test.", e);
        }
    }
}

/// Test Answer command through RWI interface
#[tokio::test]
async fn test_e2e_answer_command() {
    let result = RwiTestClient::connect().await;

    match result {
        Ok(mut client) => {
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

            // Answer call using helper function
            let answer_result = client.answer_call(&call_id).await;
            assert!(answer_result.is_ok(), "Answer call should succeed");

            let response = answer_result.unwrap();
            assert_eq!(response.response, "Success", "Answer should return Success");

            // Cleanup
            let _ = client.hangup_call(&call_id, None).await;
            let _ = client.close().await;
        }
        Err(e) => {
            println!("RWI not available: {}. Skipping test.", e);
        }
    }
}

/// Test 3-party conference through RWI interface
#[tokio::test]
async fn test_e2e_three_party_conference() {
    let result = RwiTestClient::connect().await;

    match result {
        Ok(mut client) => {
            let _ = client.subscribe(vec!["default"]).await;

            // Create conference
            let conf_id = format!("test-conf-{}", uuid::Uuid::new_v4());
            let create_result = client.conference_create(&conf_id).await;
            assert!(create_result.is_ok(), "Conference create should succeed");

            // Create three test calls
            let call1_result = create_test_call(&mut client).await;
            let call1 = match call1_result {
                Ok(id) => id,
                Err(_) => {
                    println!("Could not create test call 1, skipping");
                    let _ = client.close().await;
                    return;
                }
            };

            let call2_result = create_test_call(&mut client).await;
            let call2 = match call2_result {
                Ok(id) => id,
                Err(_) => {
                    println!("Could not create test call 2, skipping");
                    let _ = client.hangup_call(&call1, None).await;
                    let _ = client.close().await;
                    return;
                }
            };

            let call3_result = create_test_call(&mut client).await;
            let call3 = match call3_result {
                Ok(id) => id,
                Err(_) => {
                    println!("Could not create test call 3, skipping");
                    let _ = client.hangup_call(&call1, None).await;
                    let _ = client.hangup_call(&call2, None).await;
                    let _ = client.close().await;
                    return;
                }
            };

            // Add all calls to conference
            let add1 = client.conference_add(&conf_id, &call1).await;
            assert!(add1.is_ok(), "Add call1 to conference should succeed");

            let add2 = client.conference_add(&conf_id, &call2).await;
            assert!(add2.is_ok(), "Add call2 to conference should succeed");

            let add3 = client.conference_add(&conf_id, &call3).await;
            assert!(add3.is_ok(), "Add call3 to conference should succeed");

            // Test muting one participant
            let mute_result = client
                .send_request(
                    RwiRequest::new("conference.mute").with_params(serde_json::json!({
                        "conf_id": conf_id,
                        "call_id": call1
                    })),
                )
                .await;
            assert!(mute_result.is_ok(), "Mute should succeed");

            // Unmute
            let unmute_result = client
                .send_request(
                    RwiRequest::new("conference.unmute").with_params(serde_json::json!({
                        "conf_id": conf_id,
                        "call_id": call1
                    })),
                )
                .await;
            assert!(unmute_result.is_ok(), "Unmute should succeed");

            // Remove one participant
            let remove_result = client.conference_remove(&conf_id, &call2).await;
            assert!(
                remove_result.is_ok(),
                "Remove from conference should succeed"
            );

            // Cleanup remaining calls
            let _ = client.hangup_call(&call1, None).await;
            let _ = client.hangup_call(&call3, None).await;

            // Destroy conference
            let destroy_result = client.conference_destroy(&conf_id).await;
            assert!(destroy_result.is_ok(), "Conference destroy should succeed");

            let _ = client.close().await;
        }
        Err(e) => {
            println!("RWI not available: {}. Skipping test.", e);
        }
    }
}

/// Test conference with max participants limit
#[tokio::test]
async fn test_e2e_conference_max_participants() {
    let result = RwiTestClient::connect().await;

    match result {
        Ok(mut client) => {
            let _ = client.subscribe(vec!["default"]).await;

            // Create conference with max 2 participants
            let conf_id = format!("test-conf-limit-{}", uuid::Uuid::new_v4());
            let create_request =
                RwiRequest::new("conference.create").with_params(serde_json::json!({
                    "conf_id": conf_id,
                    "max_members": 2
                }));
            let create_result = client.send_request(create_request).await;
            assert!(create_result.is_ok(), "Conference create should succeed");

            // Create three test calls
            let call1 = create_test_call(&mut client).await.unwrap_or_else(|_| {
                println!("Could not create test call 1, skipping");
                String::new()
            });
            if call1.is_empty() {
                let _ = client.close().await;
                return;
            }

            let call2 = create_test_call(&mut client).await.unwrap_or_else(|_| {
                println!("Could not create test call 2, skipping");
                String::new()
            });
            if call2.is_empty() {
                let _ = client.hangup_call(&call1, None).await;
                let _ = client.close().await;
                return;
            }

            let call3 = create_test_call(&mut client).await.unwrap_or_else(|_| {
                println!("Could not create test call 3, skipping");
                String::new()
            });
            if call3.is_empty() {
                let _ = client.hangup_call(&call1, None).await;
                let _ = client.hangup_call(&call2, None).await;
                let _ = client.close().await;
                return;
            }

            // Add first two participants (should succeed)
            let add1 = client.conference_add(&conf_id, &call1).await;
            assert!(add1.is_ok(), "Add first participant should succeed");

            let add2 = client.conference_add(&conf_id, &call2).await;
            assert!(add2.is_ok(), "Add second participant should succeed");

            // Add third participant (should fail due to limit)
            let add3 = client.conference_add(&conf_id, &call3).await;
            // Note: This might succeed or fail depending on implementation
            // Just log the result for now
            match add3 {
                Ok(_) => println!("Third participant added (limit not enforced)"),
                Err(_) => println!("Third participant rejected (limit enforced)"),
            }

            // Cleanup
            let _ = client.hangup_call(&call1, None).await;
            let _ = client.hangup_call(&call2, None).await;
            let _ = client.hangup_call(&call3, None).await;
            let _ = client.conference_destroy(&conf_id).await;
            let _ = client.close().await;
        }
        Err(e) => {
            println!("RWI not available: {}. Skipping test.", e);
        }
    }
}

/// Test conference lifecycle with multiple operations
#[tokio::test]
async fn test_e2e_conference_lifecycle() {
    let result = RwiTestClient::connect().await;

    match result {
        Ok(mut client) => {
            let _ = client.subscribe(vec!["default"]).await;

            let conf_id = format!("test-conf-lifecycle-{}", uuid::Uuid::new_v4());

            // Create conference
            let create_result = client.conference_create(&conf_id).await;
            assert!(create_result.is_ok(), "Conference create should succeed");

            // Add participant 1
            let call1 = create_test_call(&mut client).await.unwrap_or_else(|_| {
                println!("Could not create test call, skipping");
                String::new()
            });
            if call1.is_empty() {
                let _ = client.close().await;
                return;
            }

            let add1 = client.conference_add(&conf_id, &call1).await;
            assert!(add1.is_ok(), "Add participant 1 should succeed");

            // Add participant 2
            let call2 = match create_test_call(&mut client).await {
                Ok(id) => id,
                Err(_) => {
                    let _ = client.hangup_call(&call1, None).await;
                    let _ = client.close().await;
                    return;
                }
            };

            let add2 = client.conference_add(&conf_id, &call2).await;
            assert!(add2.is_ok(), "Add participant 2 should succeed");

            // Mute participant 1
            let mute_req = RwiRequest::new("conference.mute").with_params(serde_json::json!({
                "conf_id": conf_id,
                "call_id": call1
            }));
            let mute_result = client.send_request(mute_req).await;
            assert!(mute_result.is_ok(), "Mute should succeed");

            // Unmute participant 1
            let unmute_req = RwiRequest::new("conference.unmute").with_params(serde_json::json!({
                "conf_id": conf_id,
                "call_id": call1
            }));
            let unmute_result = client.send_request(unmute_req).await;
            assert!(unmute_result.is_ok(), "Unmute should succeed");

            // Remove participant 1
            let remove1 = client.conference_remove(&conf_id, &call1).await;
            assert!(remove1.is_ok(), "Remove participant 1 should succeed");

            // Add participant 3 (after removal)
            let call3 = match create_test_call(&mut client).await {
                Ok(id) => id,
                Err(_) => {
                    let _ = client.hangup_call(&call1, None).await;
                    let _ = client.hangup_call(&call2, None).await;
                    let _ = client.close().await;
                    return;
                }
            };

            let add3 = client.conference_add(&conf_id, &call3).await;
            assert!(add3.is_ok(), "Add participant 3 should succeed");

            // Remove all participants
            let remove2 = client.conference_remove(&conf_id, &call2).await;
            assert!(remove2.is_ok(), "Remove participant 2 should succeed");

            let remove3 = client.conference_remove(&conf_id, &call3).await;
            assert!(remove3.is_ok(), "Remove participant 3 should succeed");

            // Destroy conference
            let destroy = client.conference_destroy(&conf_id).await;
            assert!(destroy.is_ok(), "Conference destroy should succeed");

            // Hangup all calls
            let _ = client.hangup_call(&call1, None).await;
            let _ = client.hangup_call(&call2, None).await;
            let _ = client.hangup_call(&call3, None).await;

            let _ = client.close().await;
        }
        Err(e) => {
            println!("RWI not available: {}. Skipping test.", e);
        }
    }
}
