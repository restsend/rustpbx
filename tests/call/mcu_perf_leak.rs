//! MCU performance / resource-leak tests.
//!
//! Validates that the conference system is bounded and leak-free:
//! - DashMap entries are fully cleaned up after conference lifecycle
//! - Cancellation propagates within a bounded time
//! - Many participants / rapid churn do not panic or accumulate state
//! - Channels exert backpressure (bounded, no OOM)
//! - P2P ↔ MCU transitions complete within latency bounds

use std::sync::Arc;
use std::time::Duration;

use rustpbx::call::domain::LegId;
use rustpbx::call::runtime::{
    ConferenceId, ConferenceManager, ConferenceServer, ConferenceStrategy, LegMediaBridger,
    MediaPathContext, MediaPathDecision, MediaPathStrategy, SessionId,
};
use rustpbx::media::conference_mixer::AudioFrame;

fn new_manager() -> Arc<ConferenceManager> {
    Arc::new(ConferenceManager::new())
}

#[tokio::test]
async fn test_many_participants_single_conference() {
    let manager = new_manager();
    manager.create_conference("load-conf".into(), None).await.unwrap();

    const N: usize = 32;
    let mut txs = Vec::with_capacity(N);
    let mut rxs = Vec::with_capacity(N);

    for i in 0..N {
        let leg = LegId::new(format!("p{i}"));
        let ch = manager
            .add_participant(&"load-conf".into(), leg.clone())
            .await
            .unwrap();
        txs.push(ch.input_tx);
        let rx = manager.take_participant_output_rx(&leg).await.unwrap();
        rxs.push(rx);
    }

    // All speak simultaneously.
    for (i, tx) in txs.iter().enumerate() {
        let samples = vec![((i % 16) as i16 + 1) * 500; 160];
        tx.send(AudioFrame::new(samples, 8000)).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(80)).await;

    // Every participant should receive non-silent mixed audio of frame size 160.
    for (i, rx) in rxs.iter_mut().enumerate() {
        let frame = rx.try_recv().unwrap_or_else(|_| {
            panic!("participant {i} should receive mixed audio under load")
        });
        assert!(
            frame.samples.iter().any(|&s| s != 0),
            "participant {i} received silence"
        );
        assert_eq!(frame.samples.len(), 160);
    }

    manager.destroy_conference(&"load-conf".into()).await.unwrap();
    assert_eq!(manager.dashmap_sizes(), (0, 0, 0, 0, 0));
}

#[tokio::test]
async fn test_many_conferences_no_cross_contamination() {
    let manager = new_manager();
    const C: usize = 20;
    for c in 0..C {
        let cid: ConferenceId = format!("iso-conf-{c}").into();
        manager.create_conference(cid.clone(), None).await.unwrap();
        for p in 0..3 {
            manager
                .add_participant(&cid, LegId::new(format!("c{c}-p{p}")))
                .await
                .unwrap();
        }
    }

    // Send audio in conf-0 only; conf-1 must not hear it.
    let ch0 = manager
        .get_participant_channels(&LegId::new("c0-p0"))
        .await
        .unwrap();
    ch0.input_tx
        .send(AudioFrame::new(vec![1000i16; 160], 8000))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(60)).await;

    let mut rx1 = manager
        .take_participant_output_rx(&LegId::new("c1-p1"))
        .await
        .unwrap();
    assert!(
        rx1.try_recv().is_err(),
        "cross-conference audio leakage detected"
    );

    for c in 0..C {
        manager
            .destroy_conference(&format!("iso-conf-{c}").into())
            .await
            .unwrap();
    }
    assert_eq!(manager.dashmap_sizes(), (0, 0, 0, 0, 0));
}

#[tokio::test]
async fn test_rapid_join_leave_no_panics() {
    let manager = new_manager();
    manager.create_conference("churn-conf".into(), None).await.unwrap();

    // Keep one persistent participant so the conference never auto-destroys
    // mid-churn (auto-destroy only fires when the last participant leaves).
    manager
        .add_participant(&"churn-conf".into(), LegId::new("keeper"))
        .await
        .unwrap();

    for cycle in 0..50 {
        let leg = LegId::new(format!("churner-{cycle}"));
        let ch = manager
            .add_participant(&"churn-conf".into(), leg.clone())
            .await
            .unwrap();
        ch.input_tx
            .send(AudioFrame::new(vec![1000i16; 160], 8000))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(2)).await;
        manager
            .remove_participant(&"churn-conf".into(), &leg)
            .await
            .unwrap();
    }

    let (_c, leg_map, _mixers, ch, out) = manager.dashmap_sizes();
    // The keeper participant remains registered.
    assert_eq!(leg_map, 1, "keeper should still be in the conference");
    assert_eq!(ch, 1, "keeper channel should remain");
    assert_eq!(out, 1, "keeper output should remain");
    manager.destroy_conference(&"churn-conf".into()).await.unwrap();
    assert_eq!(manager.dashmap_sizes(), (0, 0, 0, 0, 0));
}

#[tokio::test]
async fn test_no_dashmap_leak_after_lifecycle() {
    let manager = new_manager();
    for cycle in 0..10 {
        let cid: ConferenceId = format!("leak-conf-{cycle}").into();
        manager.create_conference(cid.clone(), Some(10)).await.unwrap();
        let leg_a = LegId::new(format!("la-{cycle}"));
        let leg_b = LegId::new(format!("lb-{cycle}"));
        manager.add_participant(&cid, leg_a.clone()).await.unwrap();
        manager.add_participant(&cid, leg_b.clone()).await.unwrap();

        let ch_a = manager.get_participant_channels(&leg_a).await.unwrap();
        ch_a.input_tx
            .send(AudioFrame::new(vec![1000i16; 160], 8000))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;

        manager.destroy_conference(&cid).await.unwrap();
    }
    assert_eq!(
        manager.dashmap_sizes(),
        (0, 0, 0, 0, 0),
        "all internal maps must be empty after conference lifecycle"
    );
}

#[tokio::test]
async fn test_destroy_within_bounded_time() {
    let manager = new_manager();
    let cid: ConferenceId = "cancel-conf".into();
    manager
        .create_conference_ex(cid.clone(), None, None, Some(60))
        .await
        .unwrap();
    let leg = LegId::new("cancel-leg");
    let ch = manager.add_participant(&cid, leg.clone()).await.unwrap();
    ch.input_tx
        .send(AudioFrame::new(vec![1000i16; 160], 8000))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let start = std::time::Instant::now();
    let _ = manager.destroy_conference(&cid).await;
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(2000),
        "destroy_conference exceeded 2s: {elapsed:?}"
    );
    assert!(manager.get_conference(&cid).await.is_none());
    assert_eq!(manager.dashmap_sizes(), (0, 0, 0, 0, 0));
}

#[tokio::test]
async fn test_channel_backpressure_no_oom() {
    let manager = new_manager();
    manager.create_conference("bp-conf".into(), None).await.unwrap();

    let fast = LegId::new("fast-talker");
    let slow = LegId::new("slow-listener");
    let fast_ch = manager
        .add_participant(&"bp-conf".into(), fast.clone())
        .await
        .unwrap();
    let _slow_ch = manager
        .add_participant(&"bp-conf".into(), slow.clone())
        .await
        .unwrap();

    // Take the slow listener's output so it is NEVER drained (bounded output
    // channel must drop rather than grow).
    let _slow_output = manager
        .take_participant_output_rx(&slow)
        .await
        .unwrap();

    // Flood the mixer's INPUT channel with try_send. The channel is bounded at
    // capacity 100 — flooding faster than the 20ms mix tick MUST eventually
    // reject, proving the audio path cannot grow memory without bound.
    let mut saw_full = false;
    for i in 0..500 {
        let samples = vec![(i as i16) % 1000; 160];
        match fast_ch.input_tx.try_send(AudioFrame::new(samples, 8000)) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                saw_full = true;
                break;
            }
            Err(_) => break,
        }
    }
    assert!(
        saw_full,
        "bounded input channel (capacity 100) must reject when flooded"
    );

    manager.destroy_conference(&"bp-conf".into()).await.unwrap();
}

// ── Strategy / transition latency ─────────────────────────────────────────

struct MockSession {
    server: Arc<ConferenceServer>,
    bridged: std::sync::Arc<std::sync::Mutex<Vec<LegId>>>,
}

#[async_trait::async_trait]
impl LegMediaBridger for MockSession {
    async fn bridge_into(&mut self, conf_id: &str, leg_id: &LegId) -> anyhow::Result<()> {
        self.server
            .add_participant(&ConferenceId::from(conf_id), leg_id.clone())
            .await?;
        self.bridged.lock().unwrap().push(leg_id.clone());
        Ok(())
    }
    async fn unbridge(&mut self, conf_id: &str, leg_id: &LegId) -> anyhow::Result<()> {
        let _ = self.server.leave_conference(conf_id, leg_id).await;
        self.bridged.lock().unwrap().retain(|l| l != leg_id);
        Ok(())
    }
}

fn strategy_ctx(sid: &str, legs: Vec<LegId>) -> MediaPathContext {
    MediaPathContext {
        session_id: SessionId::from(sid),
        active_legs: legs,
        cancel_token: tokio_util::sync::CancellationToken::new(),
    }
}

#[tokio::test]
async fn test_p2p_to_mcu_transition_latency() {
    let manager = new_manager();
    let server = Arc::new(ConferenceServer::new(manager.clone()));
    let strategy = ConferenceStrategy::new(server.clone());
    let mut session = MockSession {
        server: server.clone(),
        bridged: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
    };

    let c2 = strategy_ctx("latency-s", vec![LegId::new("a"), LegId::new("b")]);
    assert_eq!(
        strategy.decide(&c2.active_legs).unwrap(),
        MediaPathDecision::Direct(vec![LegId::new("a"), LegId::new("b")])
    );

    let c3 = strategy_ctx(
        "latency-s",
        vec![LegId::new("a"), LegId::new("b"), LegId::new("c")],
    );
    let start = std::time::Instant::now();
    strategy.apply_multi_party(&c3, &mut session).await.unwrap();
    let apply_elapsed = start.elapsed();
    assert!(
        apply_elapsed < Duration::from_millis(100),
        "apply_multi_party took {apply_elapsed:?}"
    );

    // New participant's audio reaches an existing participant quickly.
    let ch_c = manager
        .get_participant_channels(&LegId::new("c"))
        .await
        .unwrap();
    ch_c.input_tx
        .send(AudioFrame::new(vec![1000i16; 160], 8000))
        .await
        .unwrap();
    let start_audio = std::time::Instant::now();
    let mut rx_a = manager
        .take_participant_output_rx(&LegId::new("a"))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_millis(500), rx_a.recv())
        .await
        .expect("audio transition timeout")
        .expect("receiver closed");
    let audio_elapsed = start_audio.elapsed();
    assert!(
        audio_elapsed < Duration::from_millis(80),
        "first mixed audio took {audio_elapsed:?}"
    );

    // Teardown back to 2 legs is also fast.
    let c2b = strategy_ctx("latency-s", vec![LegId::new("a"), LegId::new("b")]);
    let start = std::time::Instant::now();
    strategy.leave_multi_party(&c2b, &mut session).await.unwrap();
    let teardown = start.elapsed();
    assert!(
        teardown < Duration::from_millis(100),
        "leave_multi_party took {teardown:?}"
    );
}
