use crate::callrecord::CallRecordHangupReason;
use crate::proxy::proxy_call::state::SessionHangupMessage;
use rsipstack::dialog::DialogId;
use rsipstack::sip::StatusCode;
use std::collections::HashSet;
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
        }
    }
}

impl Default for CallMeta {
    fn default() -> Self {
        Self::new()
    }
}
