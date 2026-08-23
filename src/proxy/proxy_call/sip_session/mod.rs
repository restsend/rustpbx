//! SIP B2BUA session implementation, split across focused submodules.

mod prelude;

mod util;
mod builtin_app_factory;
mod session;
mod peer_audio;

mod conference;
mod live_transcription;
mod supervisor;
mod transfer;

pub(crate) use util::{pct_decode_query, route_outbound_leg};

#[cfg(test)]
pub(crate) use transfer::ReturnTargetSpec;

pub use session::{SessionSnapshot, SipSession, SipSessionHandle};
pub use util::{into_callee_err, CalleeError};
