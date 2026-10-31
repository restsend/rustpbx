//! SIP B2BUA session implementation, split across focused submodules.

pub mod prelude;

pub(crate) mod inbox;
pub(crate) mod session_bridge;
pub(crate) mod session_hold;
pub(crate) mod session_media;
pub(crate) mod session_app;
pub(crate) mod session_recording;
mod builtin_app_factory;
mod realtime_bridge;
pub(crate) mod session;
pub(crate) mod util;

mod conference;
mod live_transcription;
mod refer;
mod supervisor;
mod transfer;

pub(crate) use util::{pct_decode_query, route_outbound_leg};

#[cfg(test)]
pub(crate) use transfer::{BridgeTraceContext, ReturnTargetSpec, TransferDisposition};

pub use session::{DialogSide, SessionSnapshot, SipSession, SipSessionHandle};
pub use session::CMD_CHANNEL_CAPACITY;
pub use session::RUSTPBX_COMMAND_CT;
pub use util::{CalleeError, into_callee_err};
