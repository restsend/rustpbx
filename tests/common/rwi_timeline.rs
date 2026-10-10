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
    /// * **Option-A dequeue contract**: `queue_left` is TERMINAL. Any
    ///   `queue_left{reason:"connected"}` must appear AFTER
    ///   `queue_agent_connected`, and the ringing window (first
    ///   `queue_agent_offered` → `queue_agent_connected`) must carry no
    ///   `queue_left` other than a legitimate mid-ring overflow stage
    ///   switch (`reason:"overflow"`). A `queue_left` fired at
    ///   assignment/ring time is the regression this guards;
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
        // A 486 rejection is dropped once the call connected (the busy signal
        // is ignored after connect), so any emitted rejection precedes the
        // winner's connected event. Rejections legitimately name a DIFFERENT
        // agent than the eventual winner (sequential re-dial), so unlike the
        // ringing checks this is presence/shape only — no attribution
        // equality.
        if let (Some(r), Some(c)) = (idx("queue_agent_rejected"), idx("queue_agent_connected")) {
            if r > c {
                violations.push("queue_agent_rejected arrived AFTER queue_agent_connected".into());
            }
        }

        // ── Option-A dequeue contract: `queue_left` is terminal ────────
        let connected_idx = idx("queue_agent_connected");
        for (i, ev) in self.events.iter().enumerate() {
            if self.etype(ev) != "queue_left" {
                continue;
            }
            let reason = self.payload(ev)["reason"].as_str().unwrap_or("");
            if reason == "connected" {
                match connected_idx {
                    None => violations.push(
                        "queue_left{reason:\"connected\"} present but queue_agent_connected never fired"
                            .into(),
                    ),
                    Some(c) if i < c => violations.push(format!(
                        "queue_left{{reason:\"connected\"}} fired BEFORE queue_agent_connected \
                         (index {i} < {c}) — assign/ring-time dequeue is a contract violation"
                    )),
                    _ => {}
                }
            } else if let (Some(o), Some(c)) = (idx("queue_agent_offered"), connected_idx) {
                // Inside the ringing window only an overflow stage switch may
                // legitimately leave (and re-join) the queue.
                if i > o && i < c && reason != "overflow" {
                    violations.push(format!(
                        "queue_left{{reason:\"{reason}\"}} fired inside the ringing window \
                         (offered → connected) — only overflow stage switches may"
                    ));
                }
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

        // Rejection shape: the event always describes the agent leg.
        for ev in self.events.iter().filter(|e| self.etype(e) == "queue_agent_rejected") {
            let p = self.payload(ev);
            if p["leg_role"].as_str() != Some("agent") {
                violations.push(format!(
                    "queue_agent_rejected must carry leg_role \"agent\", got {:?}",
                    p["leg_role"]
                ));
            }
            if p["agent_id"].as_str().map(str::is_empty) != Some(false) {
                violations.push("queue_agent_rejected missing agent_id".into());
            }
        }

        // ── Join anchor + assignment-round pairing (skill-group flow) ────
        let indices_of = |t: &str| -> Vec<usize> {
            self.events
                .iter()
                .enumerate()
                .filter(|(_, e)| self.etype(e) == t)
                .map(|(i, _)| i)
                .collect()
        };
        let joined_list = indices_of("queue_joined");
        let assigned_list = indices_of("skill_group_agent_assigned");
        let offered_list = indices_of("queue_agent_offered");
        let no_answer_list = indices_of("queue_agent_no_answer");
        let left_list = indices_of("queue_left");

        if joined_list.is_empty() {
            violations.push("queue_joined missing from the timeline".into());
        } else if let Some(&first_assigned) = assigned_list.first() {
            if first_assigned < joined_list[0] {
                violations.push(
                    "skill_group_agent_assigned fired BEFORE queue_joined (assignment must \
                     follow the join anchor)"
                        .into(),
                );
            }
        }

        // Round pairing: with a CC adapter wired, every ringing round is
        // announced by an assignment (same count, each offered preceded by
        // an assigned).
        if !assigned_list.is_empty() {
            if assigned_list.len() != offered_list.len() {
                violations.push(format!(
                    "assignment/offering round mismatch: {} skill_group_agent_assigned vs \
                     {} queue_agent_offered",
                    assigned_list.len(),
                    offered_list.len()
                ));
            }
            for &o in &offered_list {
                if !assigned_list.iter().any(|&a| a < o) {
                    violations.push(format!(
                        "queue_agent_offered at index {o} has no preceding \
                         skill_group_agent_assigned"
                    ));
                }
            }

            // Assignment rounds must count 1..N monotonically (no dupes/gaps).
            let attempts: Vec<u64> = assigned_list
                .iter()
                .filter_map(|&i| self.events[i]["event"]["attempt"].as_u64().or(
                    self.payload(self.events.get(i).unwrap())["attempt"].as_u64(),
                ))
                .collect();
                // (payload() handles both envelope shapes)
            let expected: Vec<u64> = (1..=attempts.len() as u64).collect();
            if attempts != expected {
                violations.push(format!(
                    "skill_group_agent_assigned.attempt must be 1..N monotonic, got {attempts:?}"
                ));
            }
        }

        // Terminal uniqueness: the standard flow leaves exactly once.
        if left_list.len() != 1 {
            violations.push(format!(
                "exactly one queue_left expected for the flow, got {}",
                left_list.len()
            ));
        }

        // Leg linkage: no-answer legs must be offered legs; the connect leg
        // must be the LAST offered leg (the winning round).
        let offered_legs: Vec<String> = offered_list
            .iter()
            .filter_map(|&i| self.payload(self.events.get(i).unwrap())["leg_id"].as_str().map(String::from))
            .collect();
        for &n in &no_answer_list {
            let leg = self.payload(self.events.get(n).unwrap())["leg_id"].as_str().map(String::from);
            if let Some(leg) = leg {
                if !offered_legs.contains(&leg) {
                    violations.push(format!(
                        "queue_agent_no_answer leg {leg} was never offered (leg linkage broken)"
                    ));
                }
            }
        }
        if let Some(c) = connected_idx {
            let connect_leg =
                self.payload(self.events.get(c).unwrap())["leg_id"].as_str().map(String::from);
            let last_offered_leg = offered_legs.last().cloned();
            if let (Some(cl), Some(ol)) = (connect_leg, last_offered_leg) {
                if cl != ol {
                    violations.push(format!(
                        "queue_agent_connected leg {cl} is not the last offered leg {ol}"
                    ));
                }
            }
        }

        // Leg-scoped hangups (f5faac1d): any call_hangup WITH a leg_id must
        // carry the attribution too (hangup_by/agent_id), not just the
        // session-scoped one.
        for ev in self.events.iter().filter(|e| self.etype(e) == "call_hangup") {
            let p = self.payload(ev);
            if !p["leg_id"].is_null() && p["agent_id"].as_str().map(str::is_empty) != Some(false) {
                violations.push(format!(
                    "leg-scoped call_hangup missing agent_id attribution: {p}"
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

    /// queue\* ↔ skill_group\* PARITY: both families must describe the same
    /// queue story for this call. Downstream migrates analytics from `queue_*`
    /// to `skill_group_*` — this is the confidence contract that the two
    /// families never diverge.
    ///
    /// Skipped (no-op) when the timeline carries no `skill_group_*` events
    /// (plain queue deployments without the CC adapter).
    ///
    /// * `skill_group_call_joined` count == `queue_joined` count
    /// * `skill_group_agent_assigned` count == `queue_agent_offered` count
    /// * `skill_group_agent_no_answer` count == `queue_agent_no_answer`
    ///   count, same agents, same rounds (`attempt`)
    /// * `skill_group_agent_connected` count == `queue_agent_connected`
    ///   count, same agents
    /// * `skill_group_call_left{connected}` count == `queue_left{connected}`
    ///   count, each strictly AFTER its `skill_group_agent_connected`
    pub fn assert_skill_group_parity(&self) {
        let has_skill_events = self
            .events
            .iter()
            .any(|e| self.etype(e).starts_with("skill_group_"));
        if !has_skill_events {
            return; // plain queue, no CC adapter — nothing to pair.
        }

        let mut violations: Vec<String> = Vec::new();
        let count = |t: &str| {
            self.events
                .iter()
                .filter(|e| self.etype(e) == t)
                .count()
        };
        let agents_of = |t: &str| -> Vec<String> {
            self.events
                .iter()
                .filter(|e| self.etype(e) == t)
                .filter_map(|e| self.payload(e)["agent_id"].as_str().map(String::from))
                .collect()
        };

        // Join parity.
        let q_joined = count("queue_joined");
        let sg_joined = count("skill_group_call_joined");
        if q_joined != sg_joined {
            violations.push(format!(
                "join parity: {q_joined} queue_joined vs {sg_joined} skill_group_call_joined"
            ));
        }

        // Assignment ↔ offering parity.
        let q_offered = count("queue_agent_offered");
        let sg_assigned = count("skill_group_agent_assigned");
        if q_offered != sg_assigned {
            violations.push(format!(
                "assignment parity: {q_offered} queue_agent_offered vs \
                 {sg_assigned} skill_group_agent_assigned"
            ));
        }

        // No-answer parity: count, agents, rounds.
        let q_na_agents = agents_of("queue_agent_no_answer");
        let sg_na_agents = agents_of("skill_group_agent_no_answer");
        if q_na_agents.len() != sg_na_agents.len() {
            violations.push(format!(
                "no-answer parity: {} queue_agent_no_answer vs {} skill_group_agent_no_answer",
                q_na_agents.len(),
                sg_na_agents.len()
            ));
        } else {
            let attempts_of = |t: &str| -> Vec<u64> {
                self.events
                    .iter()
                    .filter(|e| self.etype(e) == t)
                    .filter_map(|e| self.payload(e)["attempt"].as_u64())
                    .collect()
            };
            if q_na_agents != sg_na_agents {
                violations.push(format!(
                    "no-answer agents diverge: queue={q_na_agents:?} skill_group={sg_na_agents:?}"
                ));
            }
            let q_att = attempts_of("queue_agent_no_answer");
            let sg_att = attempts_of("skill_group_agent_no_answer");
            if q_att != sg_att {
                violations.push(format!(
                    "no-answer rounds diverge: queue attempts={q_att:?} \
                     skill_group attempts={sg_att:?}"
                ));
            }
        }

        // Connect parity: count + agents.
        let q_conn = agents_of("queue_agent_connected");
        let sg_conn = agents_of("skill_group_agent_connected");
        if q_conn.len() != sg_conn.len() {
            violations.push(format!(
                "connect parity: {} queue_agent_connected vs {} skill_group_agent_connected",
                q_conn.len(),
                sg_conn.len()
            ));
        } else if q_conn != sg_conn {
            violations.push(format!(
                "connect agents diverge: queue={q_conn:?} skill_group={sg_conn:?}"
            ));
        }

        // Terminal parity (connected path).
        let q_left_conn = self
            .events
            .iter()
            .filter(|e| self.etype(e) == "queue_left")
            .filter(|e| self.payload(e)["reason"].as_str() == Some("connected"))
            .count();
        let sg_left = self.events.iter().filter(|e| self.etype(e) == "skill_group_call_left");
        let sg_left_conn = sg_left
            .clone()
            .filter(|e| self.payload(e)["reason"].as_str() == Some("connected"))
            .count();
        if q_left_conn != sg_left_conn {
            violations.push(format!(
                "terminal parity: {q_left_conn} queue_left{{connected}} vs \
                 {sg_left_conn} skill_group_call_left{{connected}}"
            ));
        }
        // Each skill_group_call_left{connected} must FOLLOW its agent_connected.
        if let (Some(conn), Some(left)) = (
            self.first_index("skill_group_agent_connected"),
            self.first_index("skill_group_call_left"),
        ) {
            if left < conn {
                violations.push(
                    "skill_group_call_left fired BEFORE skill_group_agent_connected".into(),
                );
            }
        }
        // The connected terminal carries the full group history when the
        // call was skill-group routed.
        for e in sg_left {
            if self.payload(e)["skill_groups"].as_str().is_none()
                && self.payload(e)["skill_groups"].as_array().is_none()
            {
                // absent field is allowed only for non-routed calls; with
                // skill events present it should carry the history.
                violations.push(
                    "skill_group_call_left missing skill_groups history for a skill-group call"
                        .into(),
                );
            }
        }

        if violations.is_empty() {
            return;
        }
        panic!(
            "queue* ↔ skill_group* parity violations for call {}:\n  - {}\n timeline:\n{}",
            self.call_id,
            violations.join("\n  - "),
            self.render()
        );
    }

    /// Closed-set check: every event type in this timeline must be one of
    /// `allowed` — catches unexpected events leaking into the stream AND
    /// subscription drift (an event type missing from the webhook filter
    /// silently starves assertions).
    pub fn assert_only_expected(&self, allowed: &[&str]) {
        let mut unexpected: Vec<&str> = self
            .events
            .iter()
            .filter_map(|e| {
                let t = self.etype(e);
                (!t.is_empty() && !allowed.contains(&t)).then_some(t)
            })
            .collect();
        unexpected.sort_unstable();
        unexpected.dedup();
        if unexpected.is_empty() {
            return;
        }
        panic!(
            "unexpected RWI event types for call {} (not in the allowed set {:?}):\n  - {}\n timeline:\n{}",
            self.call_id,
            allowed,
            unexpected.join("\n  - "),
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
