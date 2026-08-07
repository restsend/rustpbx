//! [`MediaRecorder`] backends — pluggable recording / capture implementations
//! that own their own threading, files, and codec policy.
//!
//! - [`FileRecorder`] — WAV file via the existing [`Recorder`], drained from a
//!   background `spawn_blocking` task so the RTP hot path never blocks.
//! - [`SipflowRecorder`] — forwards raw RTP bytes to a sink (sipflow / pcap).
//! - [`TeeRecorder`] — fans out to several backends at once.
//!
//! Wire direction → recorder leg: ingress (received) = `Leg::A`,
//! egress (sent) = `Leg::B`, giving a single leg's bidirectional capture.

use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use rustrtc::media::frame::{AudioFrame, MediaSample};
use rustrtc::rtp::RtpPacket;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

use crate::ingress_tap::{DtmfEvent, MediaRecorder, PacketDirection};
use crate::negotiate::NegotiatedLegProfile;
use crate::recorder::{Leg, Recorder};

/// A WAV-file recorder backend. Owns a background task that drains encoded
/// packets and writes them via [`Recorder`] (which decodes/resamples/encodes
/// internally). The hot path only `try_send`s into the channel — never blocks.
pub struct FileRecorder {
    cmd_tx: mpsc::Sender<FileRecCmd>,
    /// Handle to the dedicated WAV-writer OS thread. Joined on Drop so the WAV
    /// header is always finalized (rewritten with the true data size) before
    /// the recorder is released — preventing truncated/corrupt recordings when
    /// the process exits mid-recording.
    thread_handle: Option<std::thread::JoinHandle<()>>,
}

enum FileRecCmd {
    Sample(Leg, MediaSample, Option<audio_codec::CodecType>),
    Dtmf(Leg, char),
    Pause(bool),
    Finalize(oneshot::Sender<PathBuf>),
}

impl FileRecorder {
    /// Start recording to `path`. `profiles` carries the negotiated audio
    /// codec per leg (A = ingress, B = egress); the recorder uses them to
    /// decode incoming payloads.
    ///
    /// The WAV writer runs on a dedicated OS thread (Recorder uses sync File
    /// IO); the async runtime is never blocked. Samples reach it via a tokio
    /// mpsc channel (`try_send` on the hot path).
    pub async fn start(
        path: impl Into<String>,
        profiles: [(Leg, NegotiatedLegProfile); 2],
    ) -> anyhow::Result<Arc<Self>> {
        Self::start_with_channels(path, profiles, 2, false).await
    }

    /// Start recording to `path` with an explicit output layout.
    ///
    /// `channels == 1 && mono_caller_only` writes a mono WAV containing only
    /// the caller's ingress (leg A) at full amplitude — used by voicemail,
    /// where the egress leg is silence.
    pub async fn start_with_channels(
        path: impl Into<String>,
        profiles: [(Leg, NegotiatedLegProfile); 2],
        channels: u16,
        mono_caller_only: bool,
    ) -> anyhow::Result<Arc<Self>> {
        let path = path.into();
        let path_for_rec = path.clone();
        // Resolve the WAV output codec from the first leg's audio codec.
        let out_codec = profiles
            .iter()
            .find_map(|(_, p)| p.audio.as_ref().map(|c| c.codec))
            .unwrap_or(audio_codec::CodecType::PCMU);
        let (cmd_tx, cmd_rx) = mpsc::channel::<FileRecCmd>(1024);

        // Build the Recorder on a dedicated thread (File::create + WAV header
        // are sync IO — kept off the async runtime entirely).
        let (ready_tx, ready_rx) = oneshot::channel();
        let profiles_move = profiles;
        let thread_handle = std::thread::spawn(move || {
            let mut recorder = match Recorder::new_with_channels(
                &path_for_rec,
                out_codec,
                channels,
                mono_caller_only,
            ) {
                Ok(r) => r,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            for (leg, profile) in profiles_move {
                recorder.set_leg_profile(leg, profile);
            }
            let _ = ready_tx.send(Ok(()));
            run_file_recorder(&mut recorder, cmd_rx);
        });
        match ready_rx.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(anyhow::anyhow!("recorder init thread died")),
        }

        debug!(path = %path, "FileRecorder started");
        Ok(Arc::new(Self {
            cmd_tx,
            thread_handle: Some(thread_handle),
        }))
    }
}

impl Drop for FileRecorder {
    fn drop(&mut self) {
        // Drop cmd_tx first so the writer thread's blocking_recv() returns None
        // and it exits promptly; then join to guarantee the WAV header is
        // finalized before the file is released.
        self.cmd_tx = mpsc::channel(1).0;
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }
}

/// Drive a [`Recorder`] from the command channel until Finalize/close.
fn run_file_recorder(recorder: &mut Recorder, mut cmd_rx: mpsc::Receiver<FileRecCmd>) {
    while let Some(cmd) = cmd_rx.blocking_recv() {
        match cmd {
            FileRecCmd::Sample(leg, sample, hint) => {
                let dtmf_pt = recorder_dtmf_pt(recorder, leg);
                let dtmf_clk = recorder_dtmf_clock_rate(recorder, leg);
                if let Err(e) = recorder.write_sample(leg, &sample, dtmf_pt, dtmf_clk, hint) {
                    warn!("FileRecorder write_sample error: {e}");
                }
            }
            FileRecCmd::Dtmf(leg, digit) => {
                // 100ms audible tone, synthesized into the WAV by the recorder.
                if let Err(e) = recorder.write_dtmf(leg, digit, 100) {
                    warn!("FileRecorder write_dtmf error: {e}");
                }
            }
            FileRecCmd::Pause(p) => {
                // The hot path already skips write_sample when paused; this is
                // informational (no-op on the WAV writer itself).
                let _ = p;
            }
            FileRecCmd::Finalize(reply) => {
                if let Err(e) = recorder.finalize() {
                    warn!("FileRecorder finalize error: {e}");
                }
                let path = PathBuf::from(recorder.path.clone());
                let _ = reply.send(path);
                break;
            }
        }
    }
}

fn recorder_dtmf_pt(_recorder: &Recorder, _leg: Leg) -> Option<u8> {
    // The Recorder already knows its DTMF PT from the leg profile; pass None
    // and let it resolve internally (it also has a shape-based fallback).
    None
}

fn recorder_dtmf_clock_rate(_recorder: &Recorder, _leg: Leg) -> Option<u32> {
    None
}

impl MediaRecorder for FileRecorder {
    fn write_sample(&self, direction: PacketDirection, packet: &RtpPacket) {
        let leg = direction_to_leg(direction);
        let frame = AudioFrame {
            rtp_timestamp: packet.header.timestamp,
            clock_rate: 0, // Recorder resolves from the leg profile / payload PT
            data: packet.payload.clone(),
            sequence_number: Some(packet.header.sequence_number),
            payload_type: Some(packet.header.payload_type),
            marker: packet.header.marker,
            header_extension: None,
            source_addr: None,
            raw_packet: Some(packet.clone()),
        };
        let _ = self
            .cmd_tx
            .try_send(FileRecCmd::Sample(leg, MediaSample::Audio(frame), None));
    }

    fn write_dtmf(&self, event: DtmfEvent) {
        let leg = direction_to_leg(event.direction);
        let _ = self.cmd_tx.try_send(FileRecCmd::Dtmf(leg, event.digit));
    }

    fn set_paused(&self, paused: bool) {
        let _ = self.cmd_tx.try_send(FileRecCmd::Pause(paused));
    }

    fn finalize(&self) {
        let (tx, _rx) = oneshot::channel();
        // Best-effort: if the channel is closed the task already finalized.
        let _ = self.cmd_tx.try_send(FileRecCmd::Finalize(tx));
    }
}

/// Map a packet direction to the recorder leg tag (ingress → A, egress → B).
fn direction_to_leg(direction: PacketDirection) -> Leg {
    match direction {
        PacketDirection::Ingress => Leg::A,
        PacketDirection::Egress => Leg::B,
    }
}

/// Forwards every sample (marshaled to raw RTP bytes) to a sink channel — the
/// sipflow / pcap export path. Non-blocking `try_send`.
pub struct SipflowRecorder {
    tx: mpsc::Sender<SipflowItem>,
}

/// One raw RTP packet bound for the sipflow backend.
#[derive(Debug, Clone)]
pub struct SipflowItem {
    pub direction: PacketDirection,
    pub payload_type: u8,
    pub timestamp: u32,
    pub ssrc: u32,
    pub sequence_number: u16,
    /// Wall-clock receive time (epoch micros), used by sipflow for query
    /// range filtering and WAV timeline placement.
    pub received_at_micros: u64,
    /// Full marshaled RTP packet (header + payload) exactly as observed.
    pub raw: Bytes,
}

impl SipflowRecorder {
    pub fn new(tx: mpsc::Sender<SipflowItem>) -> Arc<Self> {
        Arc::new(Self { tx })
    }
}

impl MediaRecorder for SipflowRecorder {
    fn write_sample(&self, direction: PacketDirection, packet: &RtpPacket) {
        // Marshal the full packet (header + payload) so the sipflow export
        // path can decode codec / timing from the RTP header.
        let Ok(raw) = packet.marshal() else {
            return;
        };
        let received_at_micros = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or_default();
        let item = SipflowItem {
            direction,
            payload_type: packet.header.payload_type,
            timestamp: packet.header.timestamp,
            ssrc: packet.header.ssrc,
            sequence_number: packet.header.sequence_number,
            received_at_micros,
            raw: Bytes::from(raw),
        };
        let _ = self.tx.try_send(item);
    }
    fn write_dtmf(&self, _event: DtmfEvent) {}
    fn set_paused(&self, _paused: bool) {}
    fn finalize(&self) {}
}

/// Fan-out recorder: forwards every call to all wrapped backends.
pub struct TeeRecorder {
    backends: Vec<Arc<dyn MediaRecorder>>,
}

impl TeeRecorder {
    pub fn new(backends: Vec<Arc<dyn MediaRecorder>>) -> Arc<Self> {
        Arc::new(Self { backends })
    }
}

impl MediaRecorder for TeeRecorder {
    fn write_sample(&self, direction: PacketDirection, packet: &RtpPacket) {
        for b in &self.backends {
            b.write_sample(direction, packet);
        }
    }
    fn write_dtmf(&self, event: DtmfEvent) {
        for b in &self.backends {
            b.write_dtmf(event);
        }
    }
    fn set_paused(&self, paused: bool) {
        for b in &self.backends {
            b.set_paused(paused);
        }
    }
    fn finalize(&self) {
        for b in &self.backends {
            b.finalize();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustrtc::rtp::{RtpHeader, RtpPacket};

    fn pkt(pt: u8, seq: u16, ts: u32) -> RtpPacket {
        RtpPacket::new(RtpHeader::new(pt, seq, ts, 1234), vec![0xFFu8; 80])
    }

    #[tokio::test]
    async fn sipflow_recorder_forwards_items() {
        let (tx, mut rx) = mpsc::channel::<SipflowItem>(16);
        let rec = SipflowRecorder::new(tx);
        rec.write_sample(PacketDirection::Ingress, &pkt(0, 1, 160));
        rec.write_sample(PacketDirection::Egress, &pkt(0, 2, 320));

        let a = rx.recv().await.unwrap();
        assert_eq!(a.direction, PacketDirection::Ingress);
        assert_eq!(a.payload_type, 0);
        // Full RTP packet (12-byte header + 80-byte payload).
        assert_eq!(a.raw.len(), 92);
        let b = rx.recv().await.unwrap();
        assert_eq!(b.direction, PacketDirection::Egress);
        rec.finalize(); // no-op, must not panic
    }

    #[tokio::test]
    async fn tee_recorder_fans_out() {
        let (tx1, mut rx1) = mpsc::channel::<SipflowItem>(8);
        let (tx2, mut rx2) = mpsc::channel::<SipflowItem>(8);
        let tee = TeeRecorder::new(vec![SipflowRecorder::new(tx1), SipflowRecorder::new(tx2)]);
        tee.write_sample(PacketDirection::Ingress, &pkt(8, 1, 0));
        let a = rx1.recv().await.unwrap();
        let b = rx2.recv().await.unwrap();
        assert_eq!(a.payload_type, 8);
        assert_eq!(b.payload_type, 8);
    }

    #[test]
    fn direction_to_leg_mapping() {
        assert_eq!(direction_to_leg(PacketDirection::Ingress), Leg::A);
        assert_eq!(direction_to_leg(PacketDirection::Egress), Leg::B);
    }
}
