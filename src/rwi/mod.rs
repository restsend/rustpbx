pub mod auth;
pub mod event;
pub mod gateway;
pub mod handler;
pub mod processor;
pub mod proto;
pub mod session;
pub mod transfer;
pub mod webhook;

pub use auth::*;
pub use event::*;
pub use gateway::{RwiGatewayRef, *};
pub use handler::*;
pub use processor::*;
pub use session::*;

pub use proto::{
    CallMeta, CallMetaStore, EventCallContext, RecordingMetadata, RootCallInfo, RwiEvent,
};

use std::sync::OnceLock;

static GLOBAL_GATEWAY: OnceLock<RwiGatewayRef> = OnceLock::new();

/// Register the process-wide RWI gateway so non-session code paths (cluster
/// registries, background persistence, CC statistics writers, ...) can emit
/// events without threading a gateway handle through every constructor.
/// Idempotent: the first registration wins.
pub fn set_global_gateway(gateway: RwiGatewayRef) {
    let _ = GLOBAL_GATEWAY.set(gateway);
}

/// The process-wide gateway, if one was registered at startup.
pub fn global_gateway() -> Option<RwiGatewayRef> {
    GLOBAL_GATEWAY.get().cloned()
}
