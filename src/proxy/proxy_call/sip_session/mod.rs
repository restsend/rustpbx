//! SIP B2BUA session implementation, split across focused submodules.

mod prelude;

mod builtin_app_factory;
mod realtime_bridge;
mod session;
mod util;

mod conference;
mod refer;
mod live_transcription;
mod supervisor;
mod transfer;

pub(crate) use util::{pct_decode_query, route_outbound_leg};

#[cfg(test)]
pub(crate) use transfer::{ReturnTargetSpec, TransferDisposition};
#[cfg(test)]
pub(crate) use supervisor::SupervisorMode;

pub use session::{SessionSnapshot, SipSession, SipSessionHandle};
pub use util::{CalleeError, into_callee_err};
