//! Ordered dialplan inspector entries for the routing stack runtime.

use crate::proxy::call::DialplanInspector;
use std::sync::Arc;

use super::stack::{EvalMode, RoutingPhase};

/// Metadata + inspector instance registered on the SIP server.
pub struct OrderedDialplanInspector {
    pub id: String,
    pub phase: RoutingPhase,
    pub priority: i32,
    pub eval_mode: EvalMode,
    pub enabled: bool,
    pub inspector: std::sync::Arc<dyn DialplanInspector>,
}

impl OrderedDialplanInspector {
    pub fn new(
        id: impl Into<String>,
        phase: RoutingPhase,
        priority: i32,
        eval_mode: EvalMode,
        inspector: Box<dyn DialplanInspector>,
    ) -> Self {
        Self {
            id: id.into(),
            phase,
            priority,
            eval_mode,
            enabled: true,
            inspector: std::sync::Arc::from(inspector),
        }
    }
}

/// Sort inspectors by phase then priority (higher first within phase).
pub fn sort_inspector_entries(entries: &mut [Arc<OrderedDialplanInspector>]) {
    entries.sort_by(|a, b| {
        super::stack::phase_order(a.phase)
            .cmp(&super::stack::phase_order(b.phase))
            .then_with(|| b.priority.cmp(&a.priority))
            .then_with(|| a.id.cmp(&b.id))
    });
}
