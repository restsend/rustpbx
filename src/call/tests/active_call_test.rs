use super::super::sip::Invitation;
use crate::{
    app::AppStateInner,
    call::{ActiveCall, ActiveCallType, CallOption, Command},
    callrecord::CallRecordHangupReason,
};
use bytes::Bytes;
use rsipstack::{EndpointBuilder, dialog::dialog_layer::DialogLayer};
use std::{
    collections::{HashMap, VecDeque},
    fs,
    sync::Arc,
    time::Duration,
};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use voice_engine::{
    event::{EventReceiver, SessionEvent},
    media::{engine::StreamEngine, track::TrackConfig},
};

type AppState = Arc<AppStateInner>;

fn create_test_app_state_with(mutator: impl FnOnce(&mut crate::config::Config)) -> AppState {
    let base_dir =
        std::env::temp_dir().join(format!("rustpbx-active-call-test-{}", Uuid::new_v4()));
    let recorder_dir = base_dir.join("recorder");
    let mediacache_dir = base_dir.join("mediacache");
    fs::create_dir_all(&recorder_dir).unwrap();
    fs::create_dir_all(&mediacache_dir).unwrap();

    let mut config = crate::config::Config::default();
    let mut recording_policy = crate::config::RecordingPolicy::default();
    recording_policy.path = Some(recorder_dir.to_string_lossy().to_string());
    config.recording = Some(recording_policy);
    config.ensure_recording_defaults();
    config.media_cache_path = mediacache_dir.to_string_lossy().to_string();
    mutator(&mut config);

    let mut stream_engine = StreamEngine::new();
    stream_engine.with_processor_hook(Box::new(|_, _, _, _, _| Box::pin(async { Ok(Vec::new()) })));

    let db = futures::executor::block_on(sea_orm::Database::connect("sqlite::memory:")).unwrap();

    Arc::new(AppStateInner {
        core: Arc::new(crate::app::CoreContext {
            config: Arc::new(config),
            db,
            token: CancellationToken::new(),
            stream_engine: Arc::new(stream_engine),
            callrecord_sender: None,
            callrecord_stats: None,
        }),
        useragent: None,
        sip_server: std::sync::OnceLock::new(),
        active_calls: Arc::new(std::sync::Mutex::new(HashMap::new())),
        total_calls: std::sync::atomic::AtomicU64::new(0),
        total_failed_calls: std::sync::atomic::AtomicU64::new(0),
        uptime: chrono::Utc::now(),
        config_loaded_at: chrono::Utc::now(),
        config_path: None,
        reload_requested: std::sync::atomic::AtomicBool::new(false),
        #[cfg(feature = "console")]
        console: None,
        addon_registry: Arc::new(crate::addons::registry::AddonRegistry::new()),
    })
}

fn create_test_app_state() -> AppState {
    create_test_app_state_with(|_| {})
}

fn create_test_invitation() -> Invitation {
    let endpoint = EndpointBuilder::new().build();
    let dialog_layer = Arc::new(DialogLayer::new(endpoint.inner.clone()));
    Invitation::new(dialog_layer)
}

async fn wait_for_event<F>(
    receiver: &mut EventReceiver,
    buffer: &mut VecDeque<SessionEvent>,
    predicate: F,
) -> Option<SessionEvent>
where
    F: Fn(&SessionEvent) -> bool + Send,
{
    if let Some(pos) = buffer.iter().position(|event| predicate(event)) {
        return buffer.remove(pos);
    }

    let deadline = Duration::from_secs(2);

    timeout(deadline, async {
        loop {
            match receiver.recv().await {
                Ok(event) => {
                    if predicate(&event) {
                        return Some(event);
                    }
                    buffer.push_back(event);
                }
                Err(_) => return None,
            }
        }
    })
    .await
    .ok()
    .flatten()
}

#[tokio::test]
async fn websocket_hangup_should_finish_even_with_dump_enabled() {
    let app_state = create_test_app_state();
    let invitation = create_test_invitation();
    let session_id = format!("session-{}", Uuid::new_v4());
    let cancel_token = CancellationToken::new();
    let (audio_tx, audio_rx) = tokio::sync::mpsc::unbounded_channel();

    let active_call = Arc::new(ActiveCall::new(
        ActiveCallType::WebSocket,
        cancel_token.clone(),
        session_id.clone(),
        invitation,
        app_state.clone(),
        TrackConfig::default(),
        Some(audio_rx),
        true,
        None,
        None,
    ));

    let receiver = active_call.new_receiver();
    let mut event_receiver = active_call.event_sender.subscribe();
    let serve_handle = {
        let active_call = active_call.clone();
        tokio::spawn(async move {
            active_call.serve(receiver).await.unwrap();
        })
    };

    let mut call_option = CallOption::default();
    call_option.caller = Some("sip:alice@rustpbx.com".to_string());
    call_option.callee = Some("sip:bob@rustpbx.com".to_string());

    active_call
        .enqueue_command(Command::Invite {
            option: call_option,
        })
        .await
        .unwrap();

    let mut buffered_events = VecDeque::new();

    wait_for_event(&mut event_receiver, &mut buffered_events, |event| {
        matches!(event, SessionEvent::Answer { .. })
    })
    .await
    .expect("expected answer event");

    wait_for_event(&mut event_receiver, &mut buffered_events, |event| {
        matches!(event, SessionEvent::TrackStart { .. })
    })
    .await
    .expect("expected track start event");

    audio_tx.send(Bytes::from_static(&[0u8; 160])).unwrap();
    drop(audio_tx);

    wait_for_event(&mut event_receiver, &mut buffered_events, |event| {
        matches!(event, SessionEvent::TrackEnd { .. })
    })
    .await
    .expect("expected track end event");

    active_call
        .enqueue_command(Command::Hangup {
            reason: None,
            initiator: Some("caller".to_string()),
        })
        .await
        .unwrap();

    let join_result = timeout(Duration::from_millis(500), serve_handle).await;
    assert!(
        join_result.is_ok(),
        "serve task should finish after hangup even when dump events are enabled"
    );

    let call_state = active_call.call_state.read().unwrap();
    assert!(matches!(
        call_state.hangup_reason,
        Some(crate::callrecord::CallRecordHangupReason::ByCaller)
    ));
}

#[tokio::test]
async fn websocket_invite_emits_answer_and_track_events() {
    let app_state = create_test_app_state();
    let invitation = create_test_invitation();
    let session_id = format!("session-{}", Uuid::new_v4());
    let cancel_token = CancellationToken::new();
    let (audio_tx, audio_rx) = tokio::sync::mpsc::unbounded_channel();

    let active_call = Arc::new(ActiveCall::new(
        ActiveCallType::WebSocket,
        cancel_token.clone(),
        session_id.clone(),
        invitation,
        app_state.clone(),
        TrackConfig::default(),
        Some(audio_rx),
        false,
        None,
        None,
    ));

    let receiver = active_call.new_receiver();
    let mut event_receiver = active_call.event_sender.subscribe();
    let serve_handle = {
        let active_call = active_call.clone();
        tokio::spawn(async move { active_call.serve(receiver).await.unwrap() })
    };

    let mut call_option = CallOption::default();
    call_option.caller = Some("sip:alice@rustpbx.com".to_string());
    call_option.callee = Some("sip:bob@rustpbx.com".to_string());

    active_call
        .enqueue_command(Command::Invite {
            option: call_option,
        })
        .await
        .unwrap();

    let mut buffered_events = VecDeque::new();

    let answer_event = wait_for_event(&mut event_receiver, &mut buffered_events, |event| {
        matches!(event, SessionEvent::Answer { .. })
    })
    .await
    .expect("expected answer event");

    if let SessionEvent::Answer { track_id, .. } = answer_event {
        assert_eq!(track_id, session_id);
    }

    let track_start = wait_for_event(&mut event_receiver, &mut buffered_events, |event| {
        matches!(event, SessionEvent::TrackStart { .. })
    })
    .await
    .expect("expected track start event");
    if let SessionEvent::TrackStart { track_id, .. } = track_start {
        assert_eq!(track_id, session_id);
    }

    audio_tx.send(Bytes::from_static(&[0u8; 160])).unwrap();
    drop(audio_tx);

    let track_end = wait_for_event(&mut event_receiver, &mut buffered_events, |event| {
        matches!(event, SessionEvent::TrackEnd { .. })
    })
    .await
    .expect("expected track end event");
    if let SessionEvent::TrackEnd { track_id, .. } = track_end {
        assert_eq!(track_id, session_id);
    }

    active_call
        .enqueue_command(Command::Hangup {
            reason: None,
            initiator: Some("caller".to_string()),
        })
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_secs(2), serve_handle)
        .await
        .expect("active call serve task did not finish")
        .expect("active call serve task panicked");

    let call_state = active_call.call_state.read().unwrap();
    assert!(matches!(
        call_state.hangup_reason,
        Some(CallRecordHangupReason::ByCaller)
    ));
}

#[tokio::test]
async fn setup_answer_track_returns_rtp_answer_for_standard_offer() {
    let app_state = create_test_app_state();
    let invitation = create_test_invitation();
    let cancel_token = CancellationToken::new();
    let session_id = format!("session-{}", Uuid::new_v4());

    let active_call = ActiveCall::new(
        ActiveCallType::Sip,
        cancel_token,
        session_id,
        invitation,
        app_state,
        TrackConfig::default(),
        None,
        false,
        None,
        None,
    );

    let offer = "v=0\r\n\
o=- 0 0 IN IP4 192.168.1.10\r\n\
s=RustPBX Test\r\n\
c=IN IP4 192.168.1.10\r\n\
t=0 0\r\n\
m=audio 49170 RTP/AVP 0 8\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:8 PCMA/8000\r\n"
        .to_string();

    let mut call_option = CallOption::default();
    call_option.offer = Some(offer.clone());

    {
        let mut state = active_call.call_state.write().unwrap();
        state.option = Some(call_option.clone());
    }

    let ssrc = active_call.call_state.read().unwrap().ssrc;

    let (answer, track) = active_call
        .setup_answer_track(ssrc, &call_option, offer)
        .await
        .expect("expected rtp answer");

    assert!(answer.contains("m=audio"));
    assert!(answer.contains("sendrecv"));

    track.stop().await.unwrap();
}
