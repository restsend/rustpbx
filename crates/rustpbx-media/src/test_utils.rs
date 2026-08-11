//! Shared test doubles used across the crate's test modules.

use std::sync::atomic::{AtomicUsize, Ordering};

use rustrtc::rtp::RtpPacket;

use crate::ingress_tap::{DtmfEvent, MediaRecorder, PacketDirection};

/// A recorder that counts the packets written per direction and the DTMF
/// events received, used to verify recording hooks fire for the right leg /
/// direction without touching the filesystem.
pub struct CountingRecorder {
    /// `write_sample` calls for [`PacketDirection::Ingress`].
    pub ingress: AtomicUsize,
    /// `write_sample` calls for [`PacketDirection::Egress`].
    pub egress: AtomicUsize,
    /// `write_dtmf` calls.
    pub dtmfs: AtomicUsize,
}

impl CountingRecorder {
    pub fn new() -> Self {
        Self {
            ingress: AtomicUsize::new(0),
            egress: AtomicUsize::new(0),
            dtmfs: AtomicUsize::new(0),
        }
    }

    /// Total `write_sample` calls across both directions.
    pub fn samples(&self) -> usize {
        self.ingress.load(Ordering::Relaxed) + self.egress.load(Ordering::Relaxed)
    }
}

impl Default for CountingRecorder {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaRecorder for CountingRecorder {
    fn write_sample(&self, direction: PacketDirection, _: &RtpPacket) {
        match direction {
            PacketDirection::Ingress => {
                self.ingress.fetch_add(1, Ordering::Relaxed);
            }
            PacketDirection::Egress => {
                self.egress.fetch_add(1, Ordering::Relaxed);
            }
        };
    }

    fn write_dtmf(&self, _: DtmfEvent) {
        self.dtmfs.fetch_add(1, Ordering::Relaxed);
    }

    fn set_paused(&self, _: bool) {}

    fn finalize(&self) {}
}
