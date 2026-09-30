//! Shared helper: cross-event RWI contract assertions over a captured
//! webhook stream (Rust twin of event_schema.py::check_call_timeline).

use crate::common::webhook_capture::WebhookCapture;

/// One call's webhook envelope stream (body JSON as POSTed to the webhook
/// receiver: `{"call_id", "event": {...}, "event_type", ...}`).
pub struct RwiTimeline {
    call_id: String,
    events: Vec<serde_json::Value>,
}

impl RwiTimeline {
    /// Build from an already-captured envelope list (harness self-tests).
    pub fn from_events(call_id: &str, events: Vec<serde_json::Value>) -> Self {
        Self {
            call_id: call_id.to_string(),
            events,
        }
    }

    /// Collect every captured envelope belonging to `call_id`, in arrival
    /// order. Snapshot (clones) so later deliveries don't shift assertions.
    pub fn from_capture(capture: &WebhookCapture, call_id: &str) -> Self {
        let events = capture
            .received
            .lock()
            .unwrap()
            .iter()
            .filter(|v| v["call_id"].as_str() == Some(call_id))
            .cloned()
            .collect();
        Self {
            call_id: call_id.to_string(),
            events,
        }
    }

    pub fn event_count(&self) -> usize {
        self.events.len()
    }

    fn etype<'a>(&self, ev: &'a serde_json::Value) -> &'a str {
        ev["event_type"].as_str().unwrap_or("")
    }

    /// Event payload — webhook envelopes nest fields under `event`.
    fn payload<'a>(&self, ev: &'a serde_json::Value) -> &'a serde_json::Value {
        if ev["event"].is_object() {
            &ev["event"]
        } else {
            ev
        }
    }

    fn first_index(&self, etype: &str) -> Option<usize> {
        self.events.iter().position(|e| self.etype(e) == etype)
    }

    /// The queue→agent call contract (see the module docs):
    ///
    /// * at least two `call_ringing` (leg-level + session-level) — and EVERY
    ///   one carries `agent_id == expected`; the historical gap was the
    ///   leg-level event losing attribution to the CC session-hook race;
    /// * ringings precede `queue_agent_offered`, which precedes
    ///   `queue_agent_connected`;
    /// * exactly ONE `call_answered`, session-scoped (no `leg_id`), carrying
    ///   `agent_id`;
    /// * `call_hangup` present and carrying `agent_id`;
    /// * attribution stability: any event carrying `agent_id` must carry the
    ///   expected value — it must never change mid-call.
    ///
    /// Panics with the full violation list (and the timeline) on failure.
    pub fn assert_queue_agent_contract(&self, expected_agent: &str) {
        let mut violations: Vec<String> = Vec::new();

        let ringings: Vec<&serde_json::Value> = self
            .events
            .iter()
            .filter(|e| self.etype(e) == "call_ringing")
            .collect();
        if ringings.len() < 2 {
            violations.push(format!(
                "expected >=2 call_ringing (leg-level + session-level), got {}",
                ringings.len()
            ));
        }
        for ev in &ringings {
            let scope = if self.payload(ev)["leg_id"].is_null() {
                "session-level"
            } else {
                "leg-level"
            };
            if self.payload(ev)["agent_id"].as_str() != Some(expected_agent) {
                violations.push(format!(
                    "call_ringing ({scope}) missing/wrong agent_id: {}",
                    self.payload(ev)["agent_id"]
                ));
            }
        }

        // Ordering.
        let idx = |t: &str| self.first_index(t);
        if let (Some(r), Some(o)) = (idx("call_ringing"), idx("queue_agent_offered")) {
            if r > o {
                violations.push("call_ringing arrived AFTER queue_agent_offered".into());
            }
        }
        if let (Some(o), Some(c)) = (idx("queue_agent_offered"), idx("queue_agent_connected")) {
            if o > c {
                violations.push("queue_agent_offered arrived AFTER queue_agent_connected".into());
            }
        }

        // call_answered: exactly one, session-scoped, attributed.
        let answered: Vec<&serde_json::Value> = self
            .events
            .iter()
            .filter(|e| self.etype(e) == "call_answered")
            .collect();
        if answered.len() != 1 {
            violations.push(format!(
                "call_answered must fire exactly once, got {}",
                answered.len()
            ));
        }
        for ev in &answered {
            if !self.payload(ev)["leg_id"].is_null() {
                violations.push(format!(
                    "call_answered must be session-scoped (no leg_id), got leg_id={}",
                    self.payload(ev)["leg_id"]
                ));
            }
            if self.payload(ev)["agent_id"].as_str() != Some(expected_agent) {
                violations.push(format!(
                    "call_answered missing/wrong agent_id: {}",
                    self.payload(ev)["agent_id"]
                ));
            }
        }

        // Hangup attribution.
        let hangups: Vec<&serde_json::Value> = self
            .events
            .iter()
            .filter(|e| self.etype(e) == "call_hangup")
            .collect();
        if hangups.is_empty() {
            violations.push("call_hangup missing from the timeline".into());
        }
        for ev in &hangups {
            if self.payload(ev)["agent_id"].as_str() != Some(expected_agent) {
                violations.push(format!(
                    "call_hangup missing/wrong agent_id: {}",
                    self.payload(ev)["agent_id"]
                ));
            }
        }

        // Stability: any attributed event must agree with the expected agent.
        for ev in &self.events {
            let aid = self.payload(ev)["agent_id"].as_str();
            if let Some(aid) = aid {
                if aid != expected_agent {
                    violations.push(format!(
                        "{} carries agent_id={aid}, expected {expected_agent} (attribution changed mid-call)",
                        self.etype(ev)
                    ));
                }
            }
        }

        if violations.is_empty() {
            return;
        }
        panic!(
            "RWI timeline contract violations for call {}:\n  - {}\n timeline:\n{}",
            self.call_id,
            violations.join("\n  - "),
            self.render()
        );
    }

    fn render(&self) -> String {
        let mut out = String::new();
        for ev in &self.events {
            let p = self.payload(ev);
            out.push_str(&format!(
                "  {} {} leg_id={:?} agent_id={:?} queue_id={:?}\n",
                ev["timestamp"].as_str().unwrap_or(""),
                self.etype(ev),
                p["leg_id"].as_str(),
                p["agent_id"].as_str(),
                p["queue_id"].as_str(),
            ));
        }
        out
    }
}
