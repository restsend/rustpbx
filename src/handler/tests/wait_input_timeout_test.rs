#[cfg(test)]
mod wait_input_timeout_tests {
    use crate::{
        app::{AppState, AppStateBuilder},
        call::{ActiveCall, ActiveCallState, ActiveCallType, CallOption},
        callrecord::CallRecordManagerBuilder,
        config::{Config, UseragentConfig},
        event::{SessionEvent, create_event_sender},
        media::stream::MediaStreamBuilder,
        synthesis::{SynthesisOption, SynthesisType},
    };
    use anyhow::Result;
    use chrono::Utc;
    use std::{
        sync::{Arc, RwLock},
        time::Duration,
    };
    use tokio::time::{sleep, timeout};
    use tokio_util::sync::CancellationToken;

    /// Helper function to create a basic app state for testing with unique ports
    async fn create_test_app_state_with_port(port_offset: u16) -> Result<AppState> {
        let mut config = Config::default();

        // Use unique ports to avoid conflicts between tests
        let ua_port = 25000 + port_offset;
        config.ua = Some(UseragentConfig {
            addr: "127.0.0.1".to_string(),
            udp_port: ua_port,
            rtp_start_port: Some((12000 + (port_offset as u32 * 1000)) as u16),
            rtp_end_port: Some((13000 + (port_offset as u32 * 1000)) as u16),
            useragent: Some("rustpbx-test".to_string()),
            ..Default::default()
        });

        let mut callrecord = CallRecordManagerBuilder::new()
            .with_config(config.callrecord.clone().unwrap_or_default())
            .build();

        let callrecord_sender = callrecord.sender.clone();
        tokio::spawn(async move {
            callrecord.serve().await;
        });

        AppStateBuilder::new()
            .config(config)
            .with_callrecord_sender(callrecord_sender)
            .build()
            .await
    }

    /// Helper function to create a test ActiveCall
    async fn create_test_active_call(
        session_id: String,
        app_state: AppState,
    ) -> Result<Arc<ActiveCall>> {
        let cancel_token = CancellationToken::new();
        let event_sender = create_event_sender();

        let call_state = Arc::new(RwLock::new(ActiveCallState {
            start_time: Utc::now(),
            last_status_code: 200,
            ..Default::default()
        }));

        let mut option = CallOption::default();
        option.tts = Some(SynthesisOption {
            provider: Some(SynthesisType::TencentCloud),
            ..Default::default()
        });

        let media_stream = MediaStreamBuilder::new(event_sender.clone())
            .with_id(session_id.clone())
            .with_cancel_token(cancel_token.clone())
            .build();

        Ok(Arc::new(ActiveCall::new(
            call_state,
            ActiveCallType::WebSocket,
            cancel_token,
            event_sender,
            session_id,
            media_stream,
            app_state,
        )))
    }

    #[tokio::test]
    async fn test_wait_input_timeout_triggers_silence_event() -> Result<()> {
        let app_state = create_test_app_state_with_port(1).await?;
        let session_id = "test_session_wait_input_timeout".to_string();
        let timeout_ms = 200u32; // Shorter timeout for faster tests

        let active_call = create_test_active_call(session_id.clone(), app_state).await?;

        let mut event_receiver = active_call.media_stream.subscribe();

        // Set wait_input_timeout
        *active_call.wait_input_timeout.lock().await = Some(timeout_ms);

        let serve_handle = {
            let active_call_clone = active_call.clone();
            tokio::spawn(async move {
                let _ = active_call_clone.serve().await;
            })
        };

        // Give the serve task a moment to start processing
        sleep(Duration::from_millis(50)).await;

        // Simulate TrackEnd event to trigger wait_input_timeout
        active_call
            .media_stream
            .get_event_sender()
            .send(SessionEvent::TrackEnd {
                track_id: active_call.track_config.server_side_track_id.clone(),
                timestamp: crate::get_timestamp(),
                duration: 1000,
                ssrc: 0,
            })?;

        // Wait for the Silence event with more generous timeout
        let silence_event = timeout(Duration::from_millis(800), async {
            while let Ok(event) = event_receiver.recv().await {
                if let SessionEvent::Silence { track_id, .. } = event {
                    return Some(track_id);
                }
            }
            None
        })
        .await;

        active_call.cancel_token.cancel();
        let _ = timeout(Duration::from_millis(100), serve_handle).await;

        assert!(
            silence_event.is_ok(),
            "Should receive silence event within timeout"
        );
        let silence_track_id = silence_event.unwrap();
        assert!(
            silence_track_id.is_some(),
            "Should have received Silence event"
        );
        assert_eq!(
            silence_track_id.unwrap(),
            active_call.track_config.server_side_track_id,
            "Silence event should have correct track_id"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_wait_input_timeout_reset_on_speaking() -> Result<()> {
        let app_state = create_test_app_state_with_port(2).await?;
        let session_id = "test_session_timeout_reset_speaking".to_string();
        let timeout_ms = 300u32;

        let active_call = create_test_active_call(session_id.clone(), app_state).await?;

        let mut event_receiver = active_call.media_stream.subscribe();
        *active_call.wait_input_timeout.lock().await = Some(timeout_ms);

        let event_sender = active_call.media_stream.get_event_sender();

        // Simulate TrackEnd to start timeout
        event_sender.send(SessionEvent::TrackEnd {
            track_id: active_call.track_config.server_side_track_id.clone(),
            timestamp: crate::get_timestamp(),
            duration: 1000,
            ssrc: 0,
        })?;

        let serve_handle = {
            let active_call_clone = active_call.clone();
            tokio::spawn(async move {
                let _ = active_call_clone.serve().await;
            })
        };

        // Wait a bit, then send Speaking event to reset timeout
        sleep(Duration::from_millis(100)).await;

        event_sender.send(SessionEvent::Speaking {
            track_id: session_id.clone(),
            timestamp: crate::get_timestamp(),
            start_time: crate::get_timestamp(),
        })?;

        // Check that no Silence event is received in a reasonable time
        let no_silence_event = timeout(Duration::from_millis(400), async {
            while let Ok(event) = event_receiver.recv().await {
                if let SessionEvent::Silence { .. } = event {
                    return true;
                }
            }
            false
        })
        .await;

        active_call.cancel_token.cancel();
        let _ = timeout(Duration::from_millis(100), serve_handle).await;

        assert!(
            no_silence_event.is_err() || !no_silence_event.unwrap(),
            "Should not receive Silence event after Speaking resets timeout"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_wait_input_timeout_reset_on_dtmf() -> Result<()> {
        let app_state = create_test_app_state_with_port(3).await?;
        let session_id = "test_session_timeout_reset_dtmf".to_string();
        let timeout_ms = 300u32;

        let active_call = create_test_active_call(session_id.clone(), app_state).await?;

        let mut event_receiver = active_call.media_stream.subscribe();
        *active_call.wait_input_timeout.lock().await = Some(timeout_ms);

        let event_sender = active_call.media_stream.get_event_sender();

        // Simulate TrackEnd to start timeout
        event_sender.send(SessionEvent::TrackEnd {
            track_id: active_call.track_config.server_side_track_id.clone(),
            timestamp: crate::get_timestamp(),
            duration: 1000,
            ssrc: 0,
        })?;

        let serve_handle = {
            let active_call_clone = active_call.clone();
            tokio::spawn(async move {
                let _ = active_call_clone.serve().await;
            })
        };

        // Wait a bit, then send DTMF event to reset timeout
        sleep(Duration::from_millis(100)).await;

        event_sender.send(SessionEvent::Dtmf {
            track_id: session_id.clone(),
            timestamp: crate::get_timestamp(),
            digit: "1".to_string(),
        })?;

        // Check that no Silence event is received
        let no_silence_event = timeout(Duration::from_millis(400), async {
            while let Ok(event) = event_receiver.recv().await {
                if let SessionEvent::Silence { .. } = event {
                    return true;
                }
            }
            false
        })
        .await;

        active_call.cancel_token.cancel();
        let _ = timeout(Duration::from_millis(100), serve_handle).await;

        assert!(
            no_silence_event.is_err() || !no_silence_event.unwrap(),
            "Should not receive Silence event after DTMF resets timeout"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_wait_input_timeout_disabled_when_zero() -> Result<()> {
        let app_state = create_test_app_state_with_port(4).await?;
        let session_id = "test_session_timeout_disabled".to_string();

        let active_call = create_test_active_call(session_id.clone(), app_state).await?;

        let mut event_receiver = active_call.media_stream.subscribe();

        // Set wait_input_timeout to 0 (disabled)
        *active_call.wait_input_timeout.lock().await = Some(0);

        let event_sender = active_call.media_stream.get_event_sender();

        // Simulate TrackEnd
        event_sender.send(SessionEvent::TrackEnd {
            track_id: active_call.track_config.server_side_track_id.clone(),
            timestamp: crate::get_timestamp(),
            duration: 1000,
            ssrc: 0,
        })?;

        let serve_handle = {
            let active_call_clone = active_call.clone();
            tokio::spawn(async move {
                let _ = active_call_clone.serve().await;
            })
        };

        // Wait and check that no Silence event is generated when timeout is 0
        let no_silence_event = timeout(Duration::from_millis(500), async {
            while let Ok(event) = event_receiver.recv().await {
                if let SessionEvent::Silence { .. } = event {
                    return true; // Unexpected silence event
                }
            }
            false // No silence event - expected behavior
        })
        .await;

        active_call.cancel_token.cancel();
        let _ = timeout(Duration::from_millis(100), serve_handle).await;

        assert!(
            no_silence_event.is_err() || !no_silence_event.unwrap(),
            "Should not receive Silence event when timeout is 0 (disabled)"
        );

        Ok(())
    }
}
