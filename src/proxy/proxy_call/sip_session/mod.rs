//! SIP B2BUA session implementation, split across focused submodules.

mod prelude;

mod builtin_app_factory;
mod peer_audio;
mod session;
mod util;

mod conference;
mod live_transcription;
mod supervisor;
mod transfer;

pub(crate) use util::{pct_decode_query, route_outbound_leg};

#[cfg(test)]
pub(crate) use transfer::ReturnTargetSpec;

pub use session::{SessionSnapshot, SipSession, SipSessionHandle};
pub use util::{CalleeError, into_callee_err};
