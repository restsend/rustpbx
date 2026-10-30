//! Wire protocol between rustpbx-master and its workers.

pub mod control;
pub mod frame;
pub mod link;

pub use control::{ControlMessage, StreamKind};
pub use frame::{DataWriter, FrameKind, FrameParser, DatagramFrame};
pub use link::{
    AcceptPairer, ControlSender, LinkHandle, PairedOutcome, control_recv_with_fds,
    control_send_with_fds, handle_for_end, peer_pid_of, read_magic, set_runtime_handle,
    socketpair_std, worker_connect, CONTROL_MAGIC, DATA_MAGIC,
};
