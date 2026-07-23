use crate::callrecord::CallRecordHangupReason;
use crate::proxy::proxy_call::state::SessionHangupMessage;
use rsipstack::dialog::DialogId;
use rsipstack::sip::StatusCode;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

pub struct CallMeta {
    pub connected_callee: Option<String>,
    pub connected_callee_dialog_id: Option<DialogId>,
    pub callee_call_ids: HashSet<String>,
    pub ring_time: Option<Instant>,
    pub answer_time: Option<Instant>,
    pub hangup_reason: Option<CallRecordHangupReason>,
    pub hangup_messages: Vec<SessionHangupMessage>,
    pub last_error: Option<(StatusCode, Option<String>)>,
    pub routed_caller: Option<String>,
    pub routed_callee: Option<String>,
    pub routed_contact: Option<String>,
    pub routed_destination: Option<String>,
    pub queue_name: Option<String>,
    /// When set and the connected B‑leg terminates, the session returns the
    /// caller to this IVR instead of hanging up.
    /// Set by `handle_blind_transfer` for `TransferTarget::Sip` and
    /// `handle_queue_transfer` (on successful agent connection) when the
    /// user configures `return_to_ivr` on the action.
    /// Consumed once by `handle_callee_state` so the return is one-shot.
    pub transfer_return_to_ivr: Option<String>,
    /// Extra return-context params extracted from the `return_*` query string
    /// of the transfer target (e.g. `return_menu`, `return_step_id`).
    /// Forwarded as `ivr_params` when restarting the IVR.
    pub transfer_return_params: HashMap<String, String>,
}

impl CallMeta {
    pub fn new() -> Self {
        Self {
            connected_callee: None,
            connected_callee_dialog_id: None,
            callee_call_ids: HashSet::new(),
            ring_time: None,
            answer_time: None,
            hangup_reason: None,
            hangup_messages: Vec::new(),
            last_error: None,
            routed_caller: None,
            routed_callee: None,
            routed_contact: None,
            routed_destination: None,
            queue_name: None,
            transfer_return_to_ivr: None,
            transfer_return_params: HashMap::new(),
        }
    }
}

impl Default for CallMeta {
    fn default() -> Self {
        Self::new()
    }
}
