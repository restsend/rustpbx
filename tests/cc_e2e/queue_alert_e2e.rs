//! E2E: skill-group queue threshold alert → RWI `queue_alert` event →
//! gateway → `[rwi_webhook]` HTTP POST.
//!
//! Drives the REAL alert checker (`run_queue_alert_check`) over real sqlite
//! queue-status rows, with a real RWI webhook handler capturing POSTs. Also
//! anchors the cooldown contract over the webhook channel: a second check
//! inside the window must NOT produce another POST.

use std::sync::Arc;
use std::time::Duration;

use crate::common::webhook_capture::WebhookCapture;
use rustpbx::addons::cc::config::{AlertingConfig, CcConfig};
use rustpbx::addons::cc::{CcAddonState, alerting::run_queue_alert_check, stats_writer};
use rustpbx::config::LocatorWebhookConfig;
use rustpbx::rwi::{
    RwiGateway, RwiGatewayRef,
    webhook::{WEBHOOK_CHANNEL_SIZE, start_rwi_webhook_handler},
};
use sea_orm_migration::MigratorTrait;
use std::sync::Mutex;

async fn alert_db() -> sea_orm::DatabaseConnection {
    let db = sea_orm::Database::connect("sqlite::memory:").await.unwrap();
    rustpbx::addons::cc::migration::Migrator::up(&db, None)
        .await
        .unwrap();
    db
}

fn webhook_config(url: String) -> LocatorWebhookConfig {
    LocatorWebhookConfig {
        url,
        events: vec![],
        headers: None,
        timeout_ms: Some(5000),
        retries: None,
        track_queue_latency: None,
    }
}

fn wait_for_alert(received: &Mutex<Vec<serde_json::Value>>) -> Option<serde_json::Value> {
    let got = received.lock().unwrap();
    got.iter()
        .find(|v| v["event_type"].as_str() == Some("queue_alert"))
        .cloned()
}

#[tokio::test]
async fn queue_alert_reaches_rwi_webhook() {
    let _ = tracing_subscriber::fmt::try_init();
    let capture = WebhookCapture::start().await;

    let webhook_tx =
        start_rwi_webhook_handler(webhook_config(capture.url.clone()), WEBHOOK_CHANNEL_SIZE);

    let gateway: RwiGatewayRef = Arc::new(parking_lot::RwLock::new({
        let mut gw = RwiGateway::new();
        gw.set_webhook_tx(webhook_tx);
        gw
    }));

    let db = alert_db().await;

    // Queue status built by the event-driven path: 2 waiting.
    stats_writer::adjust_calls_waiting(&db, "sg-e2e", 1).await;
    stats_writer::adjust_calls_waiting(&db, "sg-e2e", 1).await;

    let mut cfg = CcConfig::default();
    cfg.alerting = AlertingConfig {
        enabled: true,
        cooldown_secs: 60,
        max_waiting: 1,
        ..AlertingConfig::default()
    };
    let cc = CcAddonState::with_db(db.clone())
        .with_cc_config(Arc::new(cfg))
        .with_gateway(gateway.clone());
    let _ = gateway; // keep alive alongside cc

    // First pass: must fire waiting_overflow (+ agents_exhausted — no
    // agents registered) and POST the RWI event to the webhook.
    let fired = run_queue_alert_check(&cc, &db).await;
    assert!(
        fired.iter().any(|r| r.alert_type == "waiting_overflow"),
        "waiting_overflow must fire, got {:?}",
        fired
            .iter()
            .map(|r| r.alert_type.clone())
            .collect::<Vec<_>>()
    );

    // The webhook envelope: {rwi, event_id, timestamp, event_type, event}.
    let payload = {
        let mut found = None;
        for _ in 0..50 {
            if let Some(v) = wait_for_alert(&capture.received) {
                found = Some(v);
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        found.expect("queue_alert must reach the RWI webhook")
    };
    assert_eq!(payload["event"]["queue_id"], "sg-e2e");
    assert_eq!(payload["event"]["alert_type"], "waiting_overflow");
    assert_eq!(payload["event"]["current"], 2.0);
    assert_eq!(payload["event"]["threshold"], 1.0);
    assert_eq!(payload["event"]["severity"], "warning");

    // Cooldown: a second pass inside the window must NOT POST again.
    let second = run_queue_alert_check(&cc, &db).await;
    assert!(second.is_empty(), "cooldown must suppress, got {second:?}");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let overflow_posts: Vec<_> = {
        let got = capture.received.lock().unwrap();
        got.iter()
            .filter(|v| {
                v["event_type"].as_str() == Some("queue_alert")
                    && v["event"]["alert_type"].as_str() == Some("waiting_overflow")
            })
            .cloned()
            .collect()
    };
    assert_eq!(
        overflow_posts.len(),
        1,
        "cooldown must prevent duplicate webhook POSTs per alert type, got {overflow_posts:?}"
    );
}
