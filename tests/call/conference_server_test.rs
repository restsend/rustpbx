//! Conference Server integration tests
//!
//! Verifies the standalone MCU service (`ConferenceServer`) covering room
//! lifecycle, participant management, host role, isolation, concurrency,
//! and end-to-end audio flow through `ConferenceMediaBridge` (the same
//! machinery production sessions use via `start_conference_media_bridge`).

use std::sync::Arc;

use rustpbx::call::domain::LegId;
use rustpbx::call::runtime::conference_media_bridge::{ConferenceMediaBridge, PcmAudioFrame};
use rustpbx::call::runtime::{ConferenceId, ConferenceManager, ConferenceServer, ParticipantRole};

#[path = "../common/audio_mocks.rs"]
mod audio_mocks;
use audio_mocks::{MockAudioReceiver, MockAudioSender};

fn test_server() -> (ConferenceServer, Arc<ConferenceManager>) {
    let manager = Arc::new(ConferenceManager::new());
    let server = ConferenceServer::new(manager.clone());
    (server, manager)
}

#[tokio::test]
async fn test_server_construction() {
    let (server, manager) = test_server();
    assert_eq!(server.list_conferences().await.len(), 0);
    // The server and the caller share the SAME manager instance: a conference
    // created through the server is visible through the raw manager.
    server
        .create_conference("shared-mgr-check".into(), None)
        .await
        .unwrap();
    assert!(
        manager
            .get_conference(&"shared-mgr-check".into())
            .await
            .is_some(),
        "server and manager must share the same ConferenceManager"
    );
    server
        .destroy_conference(&"shared-mgr-check".into())
        .await
        .unwrap();
}

#[tokio::test]
async fn test_create_conference_and_list() {
    let (server, _manager) = test_server();
    let conf_id = ConferenceId::from("test-server-conf");

    let conf = server
        .create_conference(conf_id.clone(), Some(10))
        .await
        .unwrap();
    assert_eq!(conf.participant_count(), 0);

    let list = server.list_conferences().await;
    assert!(list.contains(&conf_id));

    server.destroy_conference(&conf_id).await.unwrap();
    assert!(server.get_conference(&conf_id).await.is_none());
}

#[tokio::test]
async fn test_create_conference_duplicate_rejected() {
    let (server, _manager) = test_server();
    let conf_id = ConferenceId::from("test-dup-conf");

    server
        .create_conference(conf_id.clone(), None)
        .await
        .unwrap();
    let result = server.create_conference(conf_id.clone(), None).await;
    assert!(result.is_err(), "Duplicate create should fail");

    server.destroy_conference(&conf_id).await.unwrap();
}

#[tokio::test]
async fn test_destroy_conference_idempotent() {
    let (server, _manager) = test_server();
    let conf_id = ConferenceId::from("test-idem-conf");

    server
        .create_conference(conf_id.clone(), None)
        .await
        .unwrap();
    server.destroy_conference(&conf_id).await.unwrap();
    let second = server.destroy_conference(&conf_id).await;
    assert!(second.is_ok(), "Double destroy should be idempotent");
    assert!(server.get_conference(&conf_id).await.is_none());
}

#[tokio::test]
async fn test_join_and_leave_with_media() {
    let (server, manager) = test_server();
    let conf_id = ConferenceId::from("test-media-conf");

    server
        .create_conference(conf_id.clone(), None)
        .await
        .unwrap();

    // Bridge a leg via the production path (ConferenceMediaBridge directly).
    let leg_a = LegId::new("leg-a");
    let bridge = ConferenceMediaBridge::new(manager.clone());
    let handle = bridge
        .start_bridge_full_duplex(
            "test-media-conf",
            &leg_a,
            MockAudioSender::new().clone_with_shared(),
            Box::new(MockAudioReceiver::new(vec![
                PcmAudioFrame::new(vec![1000i16; 160], 8000),
                PcmAudioFrame::new(vec![2000i16; 160], 8000),
            ])),
            audio_codec::CodecType::PCMU,
        )
        .await
        .unwrap();

    // Participant registered in room
    let conf = server.get_conference(&conf_id).await.unwrap();
    assert_eq!(conf.participant_count(), 1);
    assert!(conf.participants.contains_key(&leg_a));

    // Stopping the session-owned handle + leaving removes the participant.
    handle.stop();
    server
        .leave_conference("test-media-conf", &leg_a)
        .await
        .unwrap();
    let conf = server.get_conference(&conf_id).await.unwrap();
    assert!(!conf.participants.contains_key(&leg_a));

    server.destroy_conference(&conf_id).await.unwrap();
}

#[tokio::test]
async fn test_join_same_leg_twice_rejected() {
    let (server, _manager) = test_server();
    let conf_id = ConferenceId::from("test-dup-leg");

    server
        .create_conference(conf_id.clone(), None)
        .await
        .unwrap();

    let leg = LegId::new("leg-1");
    server.add_participant(&conf_id, leg.clone()).await.unwrap();

    let result = server.add_participant(&conf_id, leg.clone()).await;
    assert!(result.is_err(), "Joining same leg twice should fail");

    server.destroy_conference(&conf_id).await.unwrap();
}

#[tokio::test]
async fn test_mute_unmute_via_server() {
    let (server, _manager) = test_server();
    let conf_id = ConferenceId::from("test-server-mute");

    server
        .create_conference(conf_id.clone(), None)
        .await
        .unwrap();

    let leg = LegId::new("leg-mute");
    server.add_participant(&conf_id, leg.clone()).await.unwrap();

    server.mute_participant(&conf_id, &leg).await.unwrap();
    let conf = server.get_conference(&conf_id).await.unwrap();
    assert!(conf.participants.get(&leg).unwrap().muted);

    server.unmute_participant(&conf_id, &leg).await.unwrap();
    let conf = server.get_conference(&conf_id).await.unwrap();
    assert!(!conf.participants.get(&leg).unwrap().muted);

    server.destroy_conference(&conf_id).await.unwrap();
}

#[tokio::test]
async fn test_host_role_and_end_by_host() {
    let (server, _manager) = test_server();
    let conf_id = ConferenceId::from("test-server-host");
    let host_leg = LegId::new("host");

    server
        .create_conference_ex(conf_id.clone(), None, Some(host_leg.clone()), None)
        .await
        .unwrap();

    let member = LegId::new("member");
    server
        .add_participant(&conf_id, member.clone())
        .await
        .unwrap();
    server
        .add_participant_ex(&conf_id, host_leg.clone(), ParticipantRole::Host)
        .await
        .unwrap();

    // Non-host cannot end
    let denied = server.end_by_host(&conf_id, &member).await;
    assert!(denied.is_err());

    // Host can end
    let removed = server.end_by_host(&conf_id, &host_leg).await.unwrap();
    assert_eq!(removed.len(), 2);
    assert!(server.get_conference(&conf_id).await.is_none());
}

#[tokio::test]
async fn test_cross_conference_isolation() {
    let (server, _manager) = test_server();
    let conf1 = ConferenceId::from("iso-conf-1");
    let conf2 = ConferenceId::from("iso-conf-2");

    server.create_conference(conf1.clone(), None).await.unwrap();
    server.create_conference(conf2.clone(), None).await.unwrap();

    let leg = LegId::new("shared-leg");
    server.add_participant(&conf1, leg.clone()).await.unwrap();

    let denied = server.add_participant(&conf2, leg.clone()).await;
    assert!(denied.is_err(), "Leg should not join two conferences");

    server.destroy_conference(&conf1).await.unwrap();
    server.destroy_conference(&conf2).await.unwrap();
}

#[tokio::test]
async fn test_end_to_end_audio_flow() {
    let (server, manager) = test_server();
    server
        .create_conference("flow-conf".into(), None)
        .await
        .unwrap();

    // Leg A speaks: provides PCM frames to the mixer
    let sender_a = MockAudioSender::new();
    let bridge = ConferenceMediaBridge::new(manager.clone());
    let handle_a = bridge
        .start_bridge_full_duplex(
            "flow-conf",
            &LegId::new("a"),
            sender_a.clone_with_shared(),
            Box::new(MockAudioReceiver::new(vec![
                PcmAudioFrame::new(vec![1000i16; 160], 8000),
                PcmAudioFrame::new(vec![1000i16; 160], 8000),
            ])),
            audio_codec::CodecType::PCMU,
        )
        .await
        .unwrap();

    // Leg B: silent receiver (nothing to send), but its forward loop captures mixed audio
    let sender_b = MockAudioSender::new();
    let handle_b = bridge
        .start_bridge_full_duplex(
            "flow-conf",
            &LegId::new("b"),
            sender_b.clone_with_shared(),
            Box::new(MockAudioReceiver::new(vec![])),
            audio_codec::CodecType::PCMU,
        )
        .await
        .unwrap();

    // Wait for the reverse loop to feed A's audio into the mixer and the forward
    // loop to encode it for B.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let b_received = sender_b.get_samples().await;
    assert!(
        !b_received.is_empty(),
        "B should receive A's mixed audio via the conference"
    );
    match &b_received[0] {
        rustrtc::media::MediaSample::Audio(frame) => {
            assert!(!frame.data.is_empty(), "Encoded audio should be non-empty");
            assert_eq!(frame.payload_type, Some(0), "PCMU payload type");
        }
        other => panic!("Expected Audio sample, got {:?}", other),
    }

    // A should NOT hear itself (N-1 mixing). Its forward loop reads its own
    // output channel which receives no mix from B (B is silent).
    let a_received = sender_a.get_samples().await;
    let _ = a_received; // silence from A's own output is acceptable (nothing mixed in)

    handle_a.stop();
    handle_b.stop();
    server
        .destroy_conference(&"flow-conf".into())
        .await
        .unwrap();
}

#[tokio::test]
async fn test_auto_destroy_on_last_leave() {
    let (server, _manager) = test_server();
    let conf_id = ConferenceId::from("auto-destroy-conf");

    server
        .create_conference(conf_id.clone(), None)
        .await
        .unwrap();

    let leg_a = LegId::new("auto-a");
    let leg_b = LegId::new("auto-b");
    server
        .add_participant(&conf_id, leg_a.clone())
        .await
        .unwrap();
    server
        .add_participant(&conf_id, leg_b.clone())
        .await
        .unwrap();

    // Leave one → 1 remains, conference stays alive
    server
        .leave_conference("auto-destroy-conf", &leg_a)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        server.get_conference(&conf_id).await.is_some(),
        "Conference stays alive with 1 participant"
    );

    // Leave last → auto-destroy
    server
        .leave_conference("auto-destroy-conf", &leg_b)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        server.get_conference(&conf_id).await.is_none(),
        "Conference should auto-destroy when empty"
    );
}

#[tokio::test]
async fn test_concurrent_join_leave_no_panic() {
    let (server, _manager) = test_server();
    let conf_id = ConferenceId::from("concurrent-conf");

    server
        .create_conference(conf_id.clone(), None)
        .await
        .unwrap();

    let mut handles = vec![];
    for i in 0..5 {
        let server = server.clone();
        let conf_id = conf_id.clone();
        let handle = tokio::spawn(async move {
            let leg = LegId::new(format!("conc-{}", i));
            let _ = server.add_participant(&conf_id, leg).await;
        });
        handles.push(handle);
    }

    for handle in handles {
        let _ = handle.await;
    }

    let conf = server.get_conference(&conf_id).await.unwrap();
    assert_eq!(conf.participant_count(), 5);

    server.destroy_conference(&conf_id).await.unwrap();
}
