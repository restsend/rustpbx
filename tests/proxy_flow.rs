mod common;

#[path = "proxy_flow/test_basic_call.rs"]
mod test_basic_call;

#[path = "proxy_flow/test_transcoding_e2e.rs"]
mod test_transcoding_e2e;

#[path = "proxy_flow/test_webrtc_interop_e2e.rs"]
mod test_webrtc_interop_e2e;

#[path = "proxy_flow/test_ringback_e2e.rs"]
mod test_ringback_e2e;

#[path = "proxy_flow/test_recording_e2e.rs"]
mod test_recording_e2e;

#[path = "proxy_flow/test_video_e2e.rs"]
mod test_video_e2e;

#[path = "proxy_flow/test_hold_e2e.rs"]
mod test_hold_e2e;

#[path = "proxy_flow/test_outbound_e2e.rs"]
mod test_outbound_e2e;

#[path = "proxy_flow/test_media_commands_e2e.rs"]
mod test_media_commands_e2e;

#[path = "proxy_flow/test_dtmf_e2e.rs"]
mod test_dtmf_e2e;

#[path = "proxy_flow/test_presence_e2e.rs"]
mod test_presence_e2e;

#[path = "proxy_flow/test_reinvite_e2e.rs"]
mod test_reinvite_e2e;

#[path = "proxy_flow/test_outbound_cancel_e2e.rs"]
mod test_outbound_cancel_e2e;
