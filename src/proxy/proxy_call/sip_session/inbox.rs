//! Unified event inbox for the SIP session main loop.
//!
//! The run loop used to select over one channel per event source — caller
//! dialog states, callee dialog states, commands — each with hand-rolled
//! "channel closed → park forever" semantics. [`SessionInbox`] collapses the
//! mpsc sources into one [`SessionEvent`] stream with centralized
//! closed-channel semantics; the run loop selects on `inbox.recv()` plus the
//! arms that are futures rather than streams (hangup drains, cancellation,
//! shutdown-drain timeout, session-timer DelayQueue, max-duration sleep,
//! heartbeat tick) and the two **media-plane** arms.
//!
//! # Media-plane separation rule
//!
//! Media events (DTMF / AMD / recording / transcription) do NOT enter this
//! inbox. Each has its own pipeline with its own latency, lifecycle, and
//! ownership semantics: DTMF is injected straight into the running app
//! (latency-critical pending buffer); AMD keeps a dedicated arm with a fresh
//! channel per armament (dropping stale verdicts on teardown is the correct
//! semantics — a session-owned shared channel would deliver stale verdicts
//! into the next attempt's attribution); recording completion is read from
//! the media crate-owned `RecordingSession`. New media event sources get
//! their own pipeline + dedicated arm; do not fold them in here for the sake
//! of uniformity.
//!
//! # Extension contract
//!
//! [`SessionEvent`] is a **closed** enum: a new control-plane event source
//! means one new variant plus one producer branch inside `recv` — the run
//! loop's select is untouched. Addons and external drivers do NOT extend
//! this enum; their injection seam is the existing
//! [`CallCommand`](crate::call::domain::CallCommand) channel
//! (`SessionEvent::Command` — RWI / console / hooks already use it).
//!
//! # Closed-channel semantics
//!
//! Each source keeps its historical behavior: a channel that closes disables
//! only that branch (`Some(x) = rx.recv()` pattern); when *every* mpsc source
//! is closed the inbox pends forever, matching the old per-arm parking so the
//! loop stays alive for its timer arms.

use crate::call::domain::CallCommand;
use rsipstack::dialog::dialog::DialogState;
use tokio::sync::mpsc;

/// One event from any unified session input source (control plane).
#[derive(Debug)]
pub(crate) enum SessionEvent {
    /// Caller-leg dialog state (UAS path).
    CallerDialog(DialogState),
    /// Callee-leg dialog state (attach / re-INVITE / REFER targets).
    CalleeDialog(DialogState),
    /// A control command dispatched into the session (RWI / console / hooks /
    /// internal apps — the addon injection seam).
    Command(CallCommand),
}

/// Owner of every mpsc source feeding the session run loop.
pub(crate) struct SessionInbox {
    /// `None` on the UAC path (no inbound caller dialog pump).
    caller_rx: Option<mpsc::UnboundedReceiver<DialogState>>,
    callee_rx: mpsc::UnboundedReceiver<DialogState>,
    cmd_rx: mpsc::Receiver<CallCommand>,
}

impl SessionInbox {
    pub(crate) fn new(
        caller_rx: Option<mpsc::UnboundedReceiver<DialogState>>,
        callee_rx: mpsc::UnboundedReceiver<DialogState>,
        cmd_rx: mpsc::Receiver<CallCommand>,
    ) -> Self {
        Self {
            caller_rx,
            callee_rx,
            cmd_rx,
        }
    }

    /// The callee-state receiver, mutable — command execution and REFER
    /// handling may attach new dialogs whose state flows through this same
    /// pump.
    pub(crate) fn callee_rx(&mut self) -> &mut mpsc::UnboundedReceiver<DialogState> {
        &mut self.callee_rx
    }

    /// Wait for the next event from any source. Fair (non-biased) polling,
    /// matching the previous per-arm select.
    pub(crate) async fn recv(&mut self) -> SessionEvent {
        tokio::select! {
            Some(state) = async {
                match self.caller_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    // UAC path: no caller pump — park this branch forever.
                    None => std::future::pending().await,
                }
            } => SessionEvent::CallerDialog(state),

            Some(state) = self.callee_rx.recv() => SessionEvent::CalleeDialog(state),

            Some(cmd) = self.cmd_rx.recv() => SessionEvent::Command(cmd),

            // Every mpsc source closed (session teardown in progress): park so
            // the loop's timer arms keep running, as the old per-arm helpers did.
            else => std::future::pending().await,
        }
    }
}
