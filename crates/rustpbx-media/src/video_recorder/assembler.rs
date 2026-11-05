//! H264 RTP → MP4 frame assembly.
//!
//! Feeds the NAL units emitted by rustrtc's [`H264Depacketizer`] (one
//! `MediaSample::Video` per NAL — STAP-A splits, FU-A reassembles — with the
//! marker bit only on the frame's last sample) and groups them into MP4
//! access units in AVCC form (4-byte big-endian length prefix + NAL bytes).
//!
//! SPS/PPS are captured separately (they become the `avcC` box; MP4 frames
//! must not inline them), and each finished frame reports whether it carried
//! an IDR so the recorder can align the file start on a keyframe.

use bytes::Bytes;
use rustrtc::media::MediaSample;

/// One finished access unit, ready for the muxer.
#[derive(Debug, Clone)]
pub struct VideoFrame {
    /// AVCC-encoded sample bytes (length-prefixed NAL units).
    pub data: Bytes,
    /// RTP timestamp of the access unit (90 kHz for H264).
    pub rtp_timestamp: u32,
    /// Whether the access unit contains an IDR NAL (type 5).
    pub is_keyframe: bool,
}

/// Assembles depacketized NAL units into AVCC access units.
#[derive(Debug, Default)]
pub struct H264FrameAssembler {
    /// AVCC bytes of the access unit under construction.
    pending: Vec<u8>,
    /// RTP timestamp of the access unit under construction.
    current_ts: Option<u32>,
    /// Whether the AU under construction contains an IDR NAL.
    pending_keyframe: bool,
    /// Latest SPS NAL (header byte included; avcC wants it raw).
    sps: Option<Vec<u8>>,
    /// Latest PPS NAL (header byte included).
    pps: Option<Vec<u8>>,
    /// NAL bytes dropped because a timestamp switch aborted a partial frame
    /// (packet loss on the last packets of the previous frame).
    bytes_dropped: u64,
}

fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map(|byte| byte & 0x1F).unwrap_or(0)
}

impl H264FrameAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn sps(&self) -> Option<&[u8]> {
        self.sps.as_deref()
    }

    pub fn pps(&self) -> Option<&[u8]> {
        self.pps.as_deref()
    }

    pub fn bytes_dropped(&self) -> u64 {
        self.bytes_dropped
    }

    /// Whether SPS and PPS have both been captured (the muxer needs both for
    /// the `avcC` decoder configuration).
    pub fn has_decoder_config(&self) -> bool {
        self.sps.is_some() && self.pps.is_some()
    }

    /// Feed one depacketized NAL. `marker` is the RTP marker bit carried on
    /// the frame's last packet. Returns a finished access unit when this NAL
    /// closes one (timestamp switch or marker).
    pub fn push_nal(
        &mut self,
        nal: &[u8],
        rtp_timestamp: u32,
        marker: bool,
    ) -> Option<VideoFrame> {
        if nal.is_empty() {
            return None;
        }
        match nal_type(nal) {
            // SPS/PPS are captured for avcC and never enter the sample data.
            7 => {
                self.sps = Some(nal.to_vec());
                return None;
            }
            8 => {
                self.pps = Some(nal.to_vec());
                return None;
            }
            // Filler/AUD/SEI are legal in-band NALs; pass them through so the
            // bitstream stays decodable, keyframe detection stays on type 5.
            _ => {}
        }

        let mut finished = None;
        // A new RTP timestamp starts a new access unit; an interrupted
        // partial frame (lost tail packets) is dropped rather than glued
        // onto the next frame — decoders would reject it anyway.
        if self.current_ts.is_some_and(|ts| ts != rtp_timestamp) {
            if !self.pending.is_empty() {
                self.bytes_dropped += self.pending.len() as u64;
            }
            self.pending.clear();
            self.pending_keyframe = false;
            self.current_ts = Some(rtp_timestamp);
        } else if self.current_ts.is_none() {
            // First NAL of the stream: open the access unit.
            self.current_ts = Some(rtp_timestamp);
        }

        // AVCC: 4-byte big-endian NAL length + NAL bytes.
        let len = nal.len() as u32;
        self.pending.extend_from_slice(&len.to_be_bytes());
        self.pending.extend_from_slice(nal);
        if nal_type(nal) == 5 {
            self.pending_keyframe = true;
        }

        if marker {
            if !self.pending.is_empty() {
                finished = Some(VideoFrame {
                    data: Bytes::from(std::mem::take(&mut self.pending)),
                    rtp_timestamp,
                    is_keyframe: self.pending_keyframe,
                });
            }
            self.pending_keyframe = false;
            self.current_ts = None;
        }
        finished
    }

    /// Feed a `MediaSample::Video` from the rustrtc depacketizer. Non-video
    /// samples are ignored.
    pub fn push_media_sample(&mut self, sample: &MediaSample) -> Option<VideoFrame> {
        let MediaSample::Video(frame) = sample else {
            return None;
        };
        self.push_nal(
            &frame.data,
            frame.rtp_timestamp,
            frame.is_last_packet,
        )
    }

    /// Flush a partially assembled access unit (recording stopped mid-frame).
    /// Returns the AU when it contains at least one NAL; such a frame lacks
    /// the marker bit, which is legal input for an MP4 muxer as long as the
    /// next sample starts at a new timestamp.
    pub fn flush(&mut self) -> Option<VideoFrame> {
        if self.pending.is_empty() {
            self.current_ts = None;
            self.pending_keyframe = false;
            return None;
        }
        let frame = VideoFrame {
            data: Bytes::from(std::mem::take(&mut self.pending)),
            rtp_timestamp: self.current_ts.unwrap_or_default(),
            is_keyframe: self.pending_keyframe,
        };
        self.pending_keyframe = false;
        self.current_ts = None;
        Some(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nal(nal_type: u8, body: &[u8]) -> Vec<u8> {
        // nal_ref_idc=3, forbidden=0: header = (3 << 5) | type. Masked OR so
        // PPS (8) and AUD (9) stay distinct.
        let mut bytes = vec![0x60 | (nal_type & 0x1F)];
        bytes.extend_from_slice(body);
        bytes
    }

    #[test]
    fn groups_nals_by_timestamp_and_marker() {
        let mut asm = H264FrameAssembler::new();
        assert!(asm.push_nal(&nal(1, b"a"), 3000, false).is_none());
        assert!(asm.push_nal(&nal(1, b"b"), 3000, true).is_some_and(|frame| {
            // Two NALs, each 4-byte length + 2 bytes (header + body):
            // (4+2) * 2 = 12 bytes.
            assert_eq!(frame.data.len(), 12);
            assert_eq!(frame.rtp_timestamp, 3000);
            assert!(!frame.is_keyframe);
            true
        }));
    }

    #[test]
    fn timestamp_switch_flushes_previous_partial_frame() {
        let mut asm = H264FrameAssembler::new();
        assert!(asm.push_nal(&nal(1, b"a"), 3000, false).is_none());
        // Next frame starts without the marker on the previous one: the
        // partial AU is dropped (bytes_dropped) instead of glued.
        assert!(asm.push_nal(&nal(1, b"b"), 6000, true).is_some_and(|frame| {
            assert_eq!(frame.rtp_timestamp, 6000);
            assert_eq!(frame.data.len(), 6);
            true
        }));
        assert_eq!(asm.bytes_dropped(), 6);
    }

    #[test]
    fn sps_pps_captured_not_inlined() {
        let mut asm = H264FrameAssembler::new();
        let sps = nal(7, &[0x64, 0x00, 0x1f]);
        let pps = nal(8, &[0xce]);
        assert!(asm.push_nal(&sps, 3000, false).is_none());
        assert!(asm.push_nal(&pps, 3000, false).is_none());
        assert!(asm.push_nal(&nal(5, b"idr"), 3000, true).is_some_and(|frame| {
            // Only the IDR NAL lands in the sample data.
            assert_eq!(frame.data.len(), 4 + 4);
            assert!(frame.is_keyframe);
            true
        }));
        assert!(asm.has_decoder_config());
        assert_eq!(asm.sps(), Some(sps.as_slice()));
        assert_eq!(asm.pps(), Some(pps.as_slice()));
    }

    #[test]
    fn keyframe_flag_follows_idr_nal() {
        let mut asm = H264FrameAssembler::new();
        assert!(asm.push_nal(&nal(5, b"idr"), 9000, true).is_some_and(|f| f.is_keyframe));
        assert!(asm.push_nal(&nal(1, b"p"), 12000, true).is_some_and(|f| !f.is_keyframe));
    }

    #[test]
    fn flush_emits_partial_frame_once() {
        let mut asm = H264FrameAssembler::new();
        assert!(asm.push_nal(&nal(1, b"x"), 15000, false).is_none());
        let frame = asm.flush().expect("partial AU flushed");
        assert_eq!(frame.rtp_timestamp, 15000);
        assert!(asm.flush().is_none());
    }

    #[test]
    fn avcc_length_prefix_is_big_endian() {
        let mut asm = H264FrameAssembler::new();
        let body = [0u8; 300];
        // NAL = header + body = 301 bytes.
        assert!(asm.push_nal(&nal(1, &body), 18000, true).is_some_and(|frame| {
            assert_eq!(&frame.data[..4], &301u32.to_be_bytes());
            true
        }));
    }
}
