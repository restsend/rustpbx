//! Audio injection into an active call.
//!
//! Simply plays a file through the bridge's output so the target leg(s)
//! hear it (FileTrack → RTP).  The bridge's normal recording path
//! captures the conversation as usual — no special recorder handling
//! is needed.

use anyhow::Result;
use tracing::info;

/// Inject an audio file into a session so the specified leg(s) hear it.
///
/// Internally this sends [`MediaCommand::InjectAudio`] to the engine,
/// which uses the bridge's `replace_output_with_file` path — the same
/// mechanism as `Play` — for zero-copy file→RTP playback.
pub fn inject_audio(
    engine: &crate::media::engine::MediaEngine,
    session_id: &str,
    path: &str,
    target: crate::media::engine::command::InjectTarget,
    mute_peer: bool,
) -> Result<()> {
    info!(session_id, path, ?target, "InjectAudio: playing file");
    engine.send(crate::media::engine::MediaCommand::InjectAudio {
        session_id: session_id.to_string(),
        source: crate::media::engine::command::PlaySource::File {
            path: path.to_string(),
        },
        target,
        mute_peer,
    })
}
