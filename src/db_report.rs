//! Central reporting for database write failures.
//!
//! Every background/async DB write whose failure would otherwise be silent
//! (cluster session registry, queue persistence, presence, locator cleanup,
//! call-record saves, ...) funnels through [`report_db_write_failure`]:
//!
//! 1. one `tracing::error!` line with structured fields (always);
//! 2. a throttled `db_write_failed` RWI event broadcast (when a gateway is
//!    registered) — throttled per `(entity, operation)` so a full DB outage
//!    produces a trickle of events plus a suppressed-count, not a storm;
//! 3. at call sites holding a live session, the caller additionally appends
//!    a `TraceKind::Error` entry to the CDR trace (see
//!    `SipSession::record_trace`) — this module only carries the optional
//!    `call_id` for consumers to correlate.
//!
//! Console/HTTP CRUD handlers are deliberately out of scope: they already
//! surface failures synchronously to the API caller.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::rwi::event::DbWriteFailed;

/// Minimum interval between `db_write_failed` broadcasts per
/// `(entity, operation)`. During a DB outage each key emits at most one
/// event per window; the failures in between are counted and reported as
/// `suppressed` on the next emission (and in every error! log line).
pub const THROTTLE_COOLDOWN: Duration = Duration::from_secs(30);

#[derive(Default)]
struct ThrottleState {
    /// `(entity, operation) -> (last broadcast instant, suppressed count)`.
    last_emitted: HashMap<(&'static str, &'static str), (Instant, u64)>,
}

fn throttle() -> &'static Mutex<ThrottleState> {
    static THROTTLE: OnceLock<Mutex<ThrottleState>> = OnceLock::new();
    THROTTLE.get_or_init(|| Mutex::new(ThrottleState::default()))
}

/// Decide whether an event may broadcast for `(entity, operation)` right now,
/// and how many failures were suppressed since the last broadcast.
fn throttle_decision(
    state: &mut ThrottleState,
    entity: &'static str,
    operation: &'static str,
    cooldown: Duration,
    now: Instant,
) -> (bool, u64) {
    match state.last_emitted.get_mut(&(entity, operation)) {
        Some((last, suppressed)) if now.duration_since(*last) < cooldown => {
            *suppressed += 1;
            (false, *suppressed)
        }
        Some((last, suppressed)) => {
            let n = *suppressed;
            *last = now;
            *suppressed = 0;
            (true, n)
        }
        None => {
            state.last_emitted.insert((entity, operation), (now, 0));
            (true, 0)
        }
    }
}

/// Report a failed DB write: `tracing::error!` + throttled `db_write_failed`
/// RWI broadcast. Returns `true` when the failure was broadcast (not
/// suppressed by the throttle).
pub fn report_db_write_failure(
    entity: &'static str,
    operation: &'static str,
    call_id: Option<&str>,
    error: impl std::fmt::Display,
) -> bool {
    report_db_write_failure_with_detail(entity, operation, call_id, error, None, THROTTLE_COOLDOWN)
}

/// [`report_db_write_failure`] with explicit detail payload and cooldown —
/// the escape hatch used by tests (short cooldown) and call sites that want
/// to attach structured context (e.g. the alias target session id).
pub fn report_db_write_failure_with_detail(
    entity: &'static str,
    operation: &'static str,
    call_id: Option<&str>,
    error: impl std::fmt::Display,
    detail: Option<serde_json::Value>,
    cooldown: Duration,
) -> bool {
    let (emit, suppressed) = {
        let mut st = throttle().lock();
        throttle_decision(&mut st, entity, operation, cooldown, Instant::now())
    };

    tracing::error!(
        entity = %entity,
        operation = %operation,
        call_id = call_id.unwrap_or("-"),
        suppressed = suppressed,
        error = %error,
        detail = ?detail,
        "database write failed"
    );

    if !emit {
        return false;
    }
    let event = DbWriteFailed {
        call_id: call_id.map(ToOwned::to_owned),
        entity: entity.to_string(),
        operation: operation.to_string(),
        error: error.to_string(),
        suppressed: (suppressed > 0).then_some(suppressed),
    };
    if let Some(gw) = crate::rwi::global_gateway() {
        gw.read().broadcast(&event);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique keys per run so tests don't interact through the shared global
    /// throttle table.
    fn key(tag: &str) -> (&'static str, &'static str) {
        // Leak-free: tests are tiny and the strings are 'static literals
        // made unique via match on the tag. Every test gets its own key —
        // they share the process-wide throttle table and run in parallel.
        match tag {
            "a" => ("test_entity_a", "insert"),
            "b" => ("test_entity_b", "insert"),
            "c" => ("test_entity_a", "delete"),
            "d" => ("test_entity_d", "insert"),
            _ => unreachable!(),
        }
    }

    #[test]
    fn first_failure_emits_then_throttles() {
        let (entity, op) = key("a");
        assert!(report_db_write_failure_with_detail(
            entity,
            op,
            Some("call-1"),
            "boom",
            None,
            Duration::from_secs(3600)
        ));
        assert!(!report_db_write_failure_with_detail(
            entity,
            op,
            Some("call-2"),
            "boom again",
            None,
            Duration::from_secs(3600)
        ));
        assert!(!report_db_write_failure_with_detail(
            entity,
            op,
            None,
            "boom 3",
            None,
            Duration::from_secs(3600)
        ));
    }

    #[test]
    fn throttle_is_per_entity_and_operation() {
        let (entity_a, op_i) = key("a");
        let (entity_b, _) = key("b");
        let (_, op_d) = key("c");
        // A different entity or operation has its own window.
        assert!(report_db_write_failure_with_detail(
            entity_a,
            op_d,
            None,
            "e",
            None,
            Duration::from_secs(3600)
        ));
        assert!(report_db_write_failure_with_detail(
            entity_b,
            op_i,
            None,
            "e",
            None,
            Duration::from_secs(3600)
        ));
    }

    #[test]
    fn zero_cooldown_always_emits() {
        // Dedicated key: tests share the global throttle table and run in
        // parallel, so reusing another test's key would flake.
        let (entity, op) = key("d");
        assert!(report_db_write_failure_with_detail(
            entity,
            op,
            None,
            "e",
            None,
            Duration::ZERO
        ));
        assert!(report_db_write_failure_with_detail(
            entity,
            op,
            None,
            "e",
            None,
            Duration::ZERO
        ));
    }

    #[test]
    fn db_write_failed_event_serializes_with_type() {
        let event = DbWriteFailed {
            call_id: Some("call-9".into()),
            entity: "cc_acd_queue".into(),
            operation: "insert".into(),
            error: "Data too long for column 'direction'".into(),
            suppressed: Some(4),
        };
        let flat = crate::rwi::event::RwiEvent::from_spec(&event, None);
        assert_eq!(flat.event_type, "db_write_failed");
        assert_eq!(flat.call_id.as_deref(), Some("call-9"));
        let payload = serde_json::to_value(&flat.payload).unwrap();
        assert_eq!(payload["event_type"], "db_write_failed");
        assert_eq!(payload["entity"], "cc_acd_queue");
        assert_eq!(payload["suppressed"], 4);
        // call-less variant keeps the field absent.
        let event = DbWriteFailed {
            call_id: None,
            entity: "cluster_sessions".into(),
            operation: "sweep".into(),
            error: "locked".into(),
            suppressed: None,
        };
        let flat = crate::rwi::event::RwiEvent::from_spec(&event, None);
        assert_eq!(flat.call_id, None);
        assert!(flat.payload.get("call_id").is_none());
    }
}
