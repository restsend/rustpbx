//! In-band audio DTMF detection (Goertzel energy analysis over decoded PCM).
//!
//! When a leg does NOT negotiate out-of-band DTMF (no `telephone-event` in the
//! SDP — old phones, PSTN trunks), app flows (IVR / voicemail) would never see
//! key presses. This module recovers them from the audio itself: a bank of
//! eight Goertzel filters (four low + four high DTMF frequencies) runs over
//! the leg's decoded ingress PCM in 20 ms blocks, and an edge-triggered state
//! machine turns consecutive tone blocks into exactly one event per key press.
//!
//! Detected digits are injected into the SAME `IngressTap` DTMF broadcast the
//! RFC 2833 tap uses (`DtmfEvent { source: Inband }`), so every downstream
//! consumer (app injection / RWI `dtmf` events / bridge dtmf_bus / startup
//! buffering) works unchanged.
//!
//! # Algorithm
//!
//! Ported from <https://github.com/shenjinti/dtmfdecoder> (itself from
//! <https://dtmf.netlify.app> / goertzeljs), with four deliberate changes:
//!
//! 1. **`#` frequency fix** — upstream's table maps `#` to 477 Hz instead of
//!    1477 Hz, so upstream never detects `#`. Fixed; locked by a test that
//!    synthesizes 941 + 1477 Hz and requires `#`.
//! 2. **Two-block tone minimum** (≈40 ms, ITU-T Q.24 minimum tone length) —
//!    a single 20 ms block is too eager: jitter/PLC glitches would flash a
//!    digit through.
//! 3. **Edge triggering with gap re-arm** — after a digit fires, the same key
//!    cannot fire again until ~40 ms of non-tone blocks passed (one physical
//!    press = one event, RFC 4733 semantics), plus the upstream 200 ms
//!    same-key suppression window against in-tone packet-loss tearing.
//! 4. **Tone-run bridging** — isolated non-tone blocks (jitter, PLC) and
//!    weaker blocks with unchanged dominant bins (lossy-codec energy dips,
//!    e.g. Opus 16 kbps) continue the running tone instead of ending it;
//!    only `rearm_gap_blocks` consecutive non-tones close a press.
//!
//! Speech "talk-off" is mitigated beyond the upstream classifier with two
//! neighbor-margin guards calibrated on this repo's voice prompt corpus (see
//! the `talk_off` tests): the strongest low and strongest high bin must
//! dominate their frequency group, because speech smears energy onto
//! neighbors while a DTMF row/column does not. A second-harmonic guard was
//! evaluated and deliberately dropped — lossy codecs (Opus 16 kbps CELT)
//! put 2–5% harmonic power on legitimate tones, overlapping speech's
//! profile, so it cannot separate the two (measured, see the opus loopback
//! tests). The last block's eight normalized energies plus the last
//! rejection reason stay observable via [`InbandDtmfDetector::energies`] /
//! [`InbandDtmfDetector::last_rejection`] for future tuning.

use tokio::sync::broadcast;
use tracing::debug;

use crate::ingress_tap::{DtmfEvent, DtmfSource, PacketDirection};
use crate::leg_id::LegId;
use crate::pcm_analysis_tap::{AnalysisSinkKind, PcmBlockSink};

/// When in-band audio DTMF detection should run on a leg.
///
/// Configured per dialplan media (`[rtp] inband_dtmf` as the global default),
/// resolved per leg in `Leg::sync_negotiated_profile` after every SDP apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InbandDtmfMode {
    /// Start the detector on every leg that negotiated NO `telephone-event`
    /// (the RFC 2833 channel stays authoritative wherever it exists).
    #[default]
    Auto,
    /// Always run the detector alongside any negotiated telephone-event PT
    /// (audio-PT packets only, so the two channels never double-report).
    #[serde(rename = "on")]
    AlwaysOn,
    /// Never run in-band detection.
    #[serde(rename = "off")]
    Off,
}

impl InbandDtmfMode {
    /// Wire/atomic encoding (`AtomicU8` storage on the leg).
    pub fn to_u8(self) -> u8 {
        match self {
            Self::Auto => 0,
            Self::AlwaysOn => 1,
            Self::Off => 2,
        }
    }

    /// Inverse of [`Self::to_u8`]; unknown values fall back to [`Self::Auto`].
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::AlwaysOn,
            2 => Self::Off,
            _ => Self::Auto,
        }
    }
}

/// DTMF low-group frequencies (rows of the keypad).
pub const LOW_FREQUENCIES: [f64; 4] = [697.0, 770.0, 852.0, 941.0];
/// DTMF high-group frequencies (columns of the keypad).
pub const HIGH_FREQUENCIES: [f64; 4] = [1209.0, 1336.0, 1477.0, 1633.0];

/// Keypad matrix: `DIGIT_MATRIX[low_index][high_index]`.
/// NOTE: row 3 column 2 is **1477 Hz → `#`** — the upstream
/// dtmfdecoder reference has a typo there (477) that made `#` undetectable.
const DIGIT_MATRIX: [[char; 4]; 4] = [
    ['1', '2', '3', 'A'],
    ['4', '5', '6', 'B'],
    ['7', '8', '9', 'C'],
    ['*', '0', '#', 'D'],
];

/// Relative-energy threshold for a frequency bin to count as "present"
/// (upstream default; energies are per-sample normalized ratios in 0..1).
const DEFAULT_ENERGY_THRESHOLD: f64 = 0.032;
/// Lossy codecs (Opus at low bitrate) oscillate frame-to-frame: a sustained
/// tone's dominant bins keep their identity while the energy dips. A block
/// whose dominant bins match the running tone and whose energies reach at
/// least this fraction of the threshold CONTINUES that tone instead of
/// ending it (cannot start one).
const CONTINUATION_FACTOR: f64 = 0.4;
/// Consecutive tone blocks required before firing (2 × 20 ms ≈ 40 ms, the
/// ITU-T Q.24 minimum press length). One block is too glitch-sensitive.
const DEFAULT_MIN_TONE_BLOCKS: u32 = 2;
/// Consecutive non-tone blocks required to re-arm the trigger (2 × 20 ms ≈
/// 40 ms gap) — one physical press yields exactly one event.
const DEFAULT_REARM_GAP_BLOCKS: u32 = 2;
/// Same-key suppression window (upstream `PressInterval`): a repeated
/// classification of the SAME digit within this window after its trigger is
/// dropped (in-tone packet-loss tearing protection).
const DEFAULT_PRESS_INTERVAL_MS: u64 = 200;

/// Exact Blackman window coefficients (upstream `exactBlackmanWindow`).
const BLACKMAN_A0: f64 = 0.426591;
const BLACKMAN_A1: f64 = 0.496561;
const BLACKMAN_A2: f64 = 0.076849;

/// Neighbor-margin guards (talk-off rejection), calibrated on the repo's
/// voice-prompt corpus (`talk_off` tests): a true DTMF column must dominate
/// its group; speech smears energy onto neighbors.
/// Reject when e(2nd-best-high) / e(best-high) exceeds this.
const NEIGHBOR_HIGH_MARGIN_MAX: f64 = 0.10;
/// Low-group neighbor margin. Considerably looser than the high-group one:
/// real phones emit strong adjacent-low leakage (measured 0.29 on the
/// reference fixture) and lossy codecs smear the row tone onto its 70–80 Hz
/// distant neighbor (measured up to 0.8 for 852→770 under Opus 16 kbps).
/// The high group carries the talk-off separation (speech formants crowd
/// 1209–1633 Hz far more than 697–941 Hz).
const NEIGHBOR_LOW_MARGIN_MAX: f64 = 0.8;

/// Single-frequency Goertzel recursion with in-block incremental
/// normalization, bit-exact with the upstream `goertzel.go`.
///
/// Energy is refreshed on EVERY sample as `power / totalPower / filterLength`
/// so the value after the last sample is the block's normalized energy.
#[derive(Debug)]
pub(crate) struct Goertzel {
    coefficient: f64,
    f1: f64,
    f2: f64,
    total_power: f64,
    /// Normalized energy of the block so far (updated per sample).
    energy: f64,
    /// Raw power (no `/totalPower` division) for harmonic-ratio math.
    power: f64,
    filter_length: usize,
}

impl Goertzel {
    pub(crate) fn new(frequency: f64, sample_rate: f64) -> Self {
        let omega = 2.0 * std::f64::consts::PI * frequency / sample_rate;
        Self {
            coefficient: 2.0 * omega.cos(),
            f1: 0.0,
            f2: 0.0,
            total_power: 0.0,
            energy: 0.0,
            power: 0.0,
            filter_length: 0,
        }
    }

    #[inline]
    pub(crate) fn process_sample(&mut self, sample: f64) {
        let f1 = self.f1;
        let sine = sample + self.coefficient * f1 - self.f2;
        self.f2 = f1;
        self.f1 = sine;
        self.filter_length += 1;
        self.power = (self.f2 * self.f2) + (self.f1 * self.f1)
            - (self.coefficient * self.f1 * self.f2);
        self.total_power += sample * sample;
        let total = if self.total_power == 0.0 { 1.0 } else { self.total_power };
        self.energy = self.power / total / self.filter_length as f64;
    }

    /// Normalized energy of the block so far (0..1 relative fraction).
    pub(crate) fn energy(&self) -> f64 {
        self.energy
    }

    pub(crate) fn refresh(&mut self) {
        self.f1 = 0.0;
        self.f2 = 0.0;
        self.total_power = 0.0;
        self.energy = 0.0;
        self.power = 0.0;
        self.filter_length = 0;
    }
}

/// In-band DTMF detector: eight Goertzel bins + edge-triggered state machine.
///
/// Feed exactly one analysis block per [`Self::feed`] call (any block length
/// works — the window/normalization adapt to `samples.len()`; the tap uses
/// 20 ms). Returns the digit once per physical key press.
pub struct InbandDtmfDetector {
    sample_rate: u32,
    energy_threshold: f64,
    min_tone_blocks: u32,
    rearm_gap_blocks: u32,
    /// Same-key suppression window in samples (`sample_rate` × 200 ms).
    press_interval_samples: u64,
    low: [Goertzel; 4],
    high: [Goertzel; 4],
    /// Last block's normalized energies, `LOW_FREQUENCIES` then
    /// `HIGH_FREQUENCIES` order (diagnostics/tuning via [`Self::energies`]).
    energies: [f64; 8],
    /// Why the last fed block failed classification (`"no_low"`,
    /// `"no_high"`, `"neighbor_low"`, `"neighbor_high"`, `"harmonic"`); set
    /// once the block had at least one threshold-passing bin per group and
    /// a guard rejected it. `None` after a classified digit, an empty
    /// spectrum, or a reset. Diagnostics for threshold tuning.
    last_rejection: Option<&'static str>,
    /// Dominant (low, high) bin indices of the last classified block —
    /// recorded before the Goertzel bank resets, so `feed` can update the
    /// tone run's identity.
    last_bins: Option<(usize, usize)>,
    // ── edge-trigger state machine ───────────────────────────────────────
    armed: bool,
    /// Consecutive blocks of the running tone (full-strength classifications
    /// plus same-digit weak continuations; interleaved non-tones bridge until
    /// `rearm_gap_blocks` in a row end the run).
    tone_blocks: u32,
    gap_blocks: u32,
    /// The digit whose tone run is currently accumulating.
    run_digit: Option<char>,
    /// The (low_bin, high_bin) pair of the running tone — a weak block only
    /// continues the run when its dominant bins are unchanged.
    run_bins: Option<(usize, usize)>,
    last_digit: Option<char>,
    last_trigger_sample: u64,
    elapsed_samples: u64,
}

impl InbandDtmfDetector {
    /// Detector for PCM at `sample_rate` with the default thresholds
    /// (0.032 energy / 2-block tone / 2-block re-arm / 200 ms press interval).
    pub fn new(sample_rate: u32) -> Self {
        let rate = f64::from(sample_rate);
        Self {
            sample_rate,
            energy_threshold: DEFAULT_ENERGY_THRESHOLD,
            min_tone_blocks: DEFAULT_MIN_TONE_BLOCKS,
            rearm_gap_blocks: DEFAULT_REARM_GAP_BLOCKS,
            press_interval_samples: (rate * f64::from(DEFAULT_PRESS_INTERVAL_MS as u32) / 1000.0) as u64,
            low: std::array::from_fn(|i| Goertzel::new(LOW_FREQUENCIES[i], rate)),
            high: std::array::from_fn(|i| Goertzel::new(HIGH_FREQUENCIES[i], rate)),
            energies: [0.0; 8],
            last_rejection: None,
            last_bins: None,
            armed: true,
            tone_blocks: 0,
            gap_blocks: 0,
            run_digit: None,
            run_bins: None,
            last_digit: None,
            last_trigger_sample: 0,
            elapsed_samples: 0,
        }
    }

    /// The detector's PCM sample rate.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Normalized energies of the LAST fed block, `LOW_FREQUENCIES` then
    /// `HIGH_FREQUENCIES` order. Diagnostics: when a digit misfires or is
    /// missed, these eight values show how much margin the classification
    /// (threshold + neighbor/harmonic guards) had.
    pub fn energies(&self) -> &[f64; 8] {
        &self.energies
    }

    /// Why the last fed block failed classification (see field docs).
    pub fn last_rejection(&self) -> Option<&'static str> {
        self.last_rejection
    }

    /// Classify one block: apply the Exact Blackman window, run the Goertzel
    /// bank, and map the strongest low + strongest high bin (after guards)
    /// onto the keypad matrix. A block whose dominant bins match the running
    /// tone and whose energies reach [`CONTINUATION_FACTOR`] × threshold
    /// continues that run (lossy codecs dip frame-to-frame without changing
    /// the tone's identity). Returns `None` when no digit is present.
    ///
    /// Note: no second-harmonic guard. Measured on the corpus, Opus at
    /// 16 kbps puts 2–5% harmonic power on legitimate tones (CELT spectral
    /// shaping), overlapping speech's harmonic profile — the guard cannot
    /// separate them. The neighbor-margin guards plus the two-block minimum
    /// carry the talk-off load (see `talk_off` tests).
    fn classify_block(&mut self, samples: &[f64], windowed: &[f64]) -> Option<char> {
        let n = samples.len();
        for i in 0..n {
            let s = windowed[i];
            for g in &mut self.low {
                g.process_sample(s);
            }
            for g in &mut self.high {
                g.process_sample(s);
            }
        }
        for i in 0..4 {
            self.energies[i] = self.low[i].energy;
            self.energies[4 + i] = self.high[i].energy;
        }

        // Dominant bin per group (argmax; when any bin passes the threshold
        // this is exactly upstream's strongest-passing-bin selection).
        let low_idx = (0..4)
            .max_by(|a, b| self.low[*a].energy.total_cmp(&self.low[*b].energy))
            .expect("4 low bins");
        let high_idx = (0..4)
            .max_by(|a, b| self.high[*a].energy.total_cmp(&self.high[*b].energy))
            .expect("4 high bins");
        let (low_energy, high_energy) = (self.low[low_idx].energy, self.high[high_idx].energy);
        // Full-strength classification, or a continuation of the running
        // tone (weaker, same identity). Both go through the talk-off guards.
        let full = low_energy >= self.energy_threshold && high_energy >= self.energy_threshold;
        let continuation = !full
            && self.run_bins == Some((low_idx, high_idx))
            && low_energy >= self.energy_threshold * CONTINUATION_FACTOR
            && high_energy >= self.energy_threshold * CONTINUATION_FACTOR;
        if !full && !continuation {
            let low_present = low_energy >= self.energy_threshold * CONTINUATION_FACTOR;
            let high_present = high_energy >= self.energy_threshold * CONTINUATION_FACTOR;
            self.last_rejection = match (low_present, high_present) {
                (true, false) => Some("no_high"),
                (false, true) => Some("no_low"),
                _ => None, // empty spectrum — nothing near the keypad frequencies
            };
            return None;
        }

        // Talk-off guards (see module docs + `talk_off` tests for calibration).
        let mut low_sorted = [0.0f64; 4];
        let mut high_sorted = [0.0f64; 4];
        for i in 0..4 {
            low_sorted[i] = self.low[i].energy;
            high_sorted[i] = self.high[i].energy;
        }
        low_sorted.sort_by(|a, b| b.total_cmp(a));
        high_sorted.sort_by(|a, b| b.total_cmp(a));
        let neighbor_low = low_sorted[1] / low_sorted[0].max(1e-300);
        let neighbor_high = high_sorted[1] / high_sorted[0].max(1e-300);
        if neighbor_low > NEIGHBOR_LOW_MARGIN_MAX {
            self.last_rejection = Some("neighbor_low");
            return None;
        }
        if neighbor_high > NEIGHBOR_HIGH_MARGIN_MAX {
            self.last_rejection = Some("neighbor_high");
            return None;
        }

        self.last_rejection = None;
        self.last_bins = Some((low_idx, high_idx));
        Some(DIGIT_MATRIX[low_idx][high_idx])
    }

    /// Feed one analysis block of PCM samples. Returns the detected digit
    /// only once per physical key press (edge-triggered; see module docs).
    pub fn feed(&mut self, samples: &[i16]) -> Option<char> {
        let n = samples.len();
        if n == 0 {
            return None;
        }

        // Normalize to f64 and precompute the Exact Blackman window
        // (window length = this block's length, matching upstream Decode).
        let nf = n as f64;
        let mut windowed = Vec::with_capacity(n);
        let mut raw = Vec::with_capacity(n);
        for (i, &s) in samples.iter().enumerate() {
            let normalized = f64::from(s) / 32768.0;
            let two_pi_i = 2.0 * std::f64::consts::PI * i as f64;
            let window = normalized
                * (BLACKMAN_A0
                    - BLACKMAN_A1 * (two_pi_i / nf).cos()
                    + BLACKMAN_A2 * (two_pi_i * 2.0 / nf).cos());
            windowed.push(window);
            raw.push(normalized);
        }

        let digit = self.classify_block(&raw, &windowed);
        for g in &mut self.low {
            g.refresh();
        }
        for g in &mut self.high {
            g.refresh();
        }

        self.elapsed_samples += n as u64;

        self.state_machine(digit)
    }

    /// Edge-triggered state machine turning per-block classifications into
    /// one event per physical key press.
    fn state_machine(&mut self, digit: Option<char>) -> Option<char> {
        match digit {
            Some(d) => {
                self.gap_blocks = 0;
                // The tone run accumulates per digit: a different digit (or
                // a bridged run that comes back as another key) restarts
                // the count — bridging must never fuse two distinct keys.
                if self.run_digit == Some(d) {
                    self.tone_blocks += 1;
                } else {
                    self.tone_blocks = 1;
                    self.run_digit = Some(d);
                }
                if let Some(bins) = self.last_bins {
                    self.run_bins = Some(bins);
                }
                let same_key_recent = self.last_digit == Some(d)
                    && self.elapsed_samples - self.last_trigger_sample
                        < self.press_interval_samples;
                if self.armed
                    && self.tone_blocks >= self.min_tone_blocks
                    && !same_key_recent
                {
                    self.armed = false;
                    self.last_digit = Some(d);
                    self.last_trigger_sample = self.elapsed_samples;
                    return Some(d);
                }
                None
            }
            None => {
                self.gap_blocks += 1;
                // The run survives isolated non-tone blocks (jitter, PLC,
                // codec energy dips) until `rearm_gap_blocks` in a row —
                // that is the "tone ended" boundary.
                if self.gap_blocks >= self.rearm_gap_blocks {
                    self.tone_blocks = 0;
                    self.run_digit = None;
                    self.run_bins = None;
                    self.armed = true;
                }
                None
            }
        }
    }
}

// ── leg tap sink ─────────────────────────────────────────────────────────

/// Bridges the leg's shared [`PcmAnalysisTap`] blocks into the in-band DTMF
/// detector and publishes digits on the leg's DTMF broadcast channel.
/// Detector state restarts whenever the tap is rebuilt (codec change) — same
/// behaviour as the previous dedicated tap.
struct InbandDtmfSink {
    detector: InbandDtmfDetector,
    dtmf_tx: broadcast::Sender<DtmfEvent>,
    leg: LegId,
}

impl PcmBlockSink for InbandDtmfSink {
    fn feed_block(&mut self, samples: &[i16], _sample_rate: u32) {
        if let Some(digit) = self.detector.feed(samples) {
            debug!(
                leg = %self.leg, digit = %digit, source = "inband",
                "in-band DTMF digit detected",
            );
            let _ = self.dtmf_tx.send(DtmfEvent {
                direction: PacketDirection::Ingress,
                digit,
                source: DtmfSource::Inband,
            });
        }
    }
}

/// Build the shared-tap registry entry for in-band DTMF: `(kind, sink)`.
/// `dtmf_tx` is the leg's `IngressTap` DTMF broadcast sender (digits join the
/// same stream as RFC 2833 events, tagged `Inband`).
pub(crate) fn make_sink(
    sample_rate: u32,
    dtmf_tx: broadcast::Sender<DtmfEvent>,
    leg: LegId,
) -> (AnalysisSinkKind, Box<dyn PcmBlockSink>) {
    (
        AnalysisSinkKind::InbandDtmf,
        Box::new(InbandDtmfSink {
            detector: InbandDtmfDetector::new(sample_rate),
            dtmf_tx,
            leg,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_codec::{create_decoder, create_encoder, CodecType, Decoder, opus::OpusEncoder};
    use rustpbx_record_common::dtmf::DtmfGenerator;

    const TESTDATA_8K: &str = "testdata/123456654321_8k_s16le.raw";
    const TESTDATA_MIX_16K: &str = "testdata/mix_1_16k_s16le.raw";

    // ── helpers ─────────────────────────────────────────────────────────

    /// Feed `samples` in fixed-size blocks, returning concatenated digits.
    fn feed_blocks(detector: &mut InbandDtmfDetector, samples: &[i16], block: usize) -> String {
        let mut out = String::new();
        for chunk in samples.chunks(block) {
            if chunk.len() < block {
                break;
            }
            if let Some(d) = detector.feed(chunk) {
                out.push(d);
            }
        }
        out
    }

    /// Read a s16le raw PCM file as i16 samples.
    fn read_raw_pcm(path: &str) -> Vec<i16> {
        let data = std::fs::read(path)
            .unwrap_or_else(|e| panic!("missing test asset {path} (run from the crate root): {e}"));
        data.chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect()
    }

    /// Minimal RIFF/WAVE reader for the voice talk-off corpus. Supports the
    /// formats the repo's prompts/fixtures actually use: PCM16 (tag 1),
    /// G.711 A-law (tag 6) and μ-law (tag 7). First channel only.
    fn read_wav_mono(path: &str) -> (u32, Vec<i16>) {
        let data = std::fs::read(path)
            .unwrap_or_else(|e| panic!("missing test asset {path}: {e}"));
        assert_eq!(&data[0..4], b"RIFF", "RIFF signature in {path}");
        assert_eq!(&data[8..12], b"WAVE", "WAVE signature in {path}");
        let mut pos = 12;
        let mut fmt: Option<(u16, u16, u32)> = None;
        let mut payload: Option<&[u8]> = None;
        while pos + 8 <= data.len() {
            let id = &data[pos..pos + 4];
            let size =
                u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
                    as usize;
            let end = (pos + 8 + size).min(data.len());
            let body = &data[pos + 8..end];
            match id {
                b"fmt " => {
                    fmt = Some((
                        u16::from_le_bytes([body[0], body[1]]),
                        u16::from_le_bytes([body[2], body[3]]),
                        u32::from_le_bytes([body[4], body[5], body[6], body[7]]),
                    ));
                }
                b"data" => payload = Some(body),
                _ => {}
            }
            pos += 8 + size + (size & 1);
        }
        let (tag, channels, rate) = fmt.unwrap_or_else(|| panic!("no fmt chunk in {path}"));
        let payload = payload.unwrap_or_else(|| panic!("no data chunk in {path}"));
        let mut mono = Vec::new();
        match tag {
            1 => {
                for frame in payload.chunks_exact(2 * channels as usize) {
                    mono.push(i16::from_le_bytes([frame[0], frame[1]]));
                }
            }
            6 => {
                for frame in payload.chunks_exact(channels as usize) {
                    mono.push(alaw_to_linear(frame[0]));
                }
            }
            7 => {
                for frame in payload.chunks_exact(channels as usize) {
                    mono.push(mulaw_to_linear(frame[0]));
                }
            }
            other => panic!("unsupported WAV format tag {other} in {path}"),
        }
        (rate, mono)
    }

    fn alaw_to_linear(byte: u8) -> i16 {
        let a = byte ^ 0x55;
        let sign = a & 0x80 != 0;
        let exponent = (a & 0x70) >> 4;
        let mantissa = a & 0x0F;
        let magnitude: i32 = if exponent == 0 {
            ((mantissa as i32) << 4) + 8
        } else {
            (((mantissa as i32) << 4) + 0x108) << (exponent - 1)
        };
        let value = if sign { -magnitude } else { magnitude };
        value.clamp(i16::MIN as i32, i16::MAX as i32) as i16
    }

    fn mulaw_to_linear(byte: u8) -> i16 {
        const BIAS: i32 = 0x84;
        let u = !byte;
        let mut magnitude = (((u & 0x0F) as i32) << 3) + BIAS;
        magnitude <<= ((u & 0x70) >> 4) as i32;
        if u & 0x80 != 0 {
            BIAS - magnitude
        } else {
            magnitude - BIAS
        }
        .clamp(i16::MIN as i32, i16::MAX as i32) as i16
    }

    /// Synthesize a pure dual tone (amplitude scaled to ±0.4 full scale).
    fn synth_tone(low: f64, high: f64, rate: u32, ms: usize) -> Vec<i16> {
        let n = rate as usize * ms / 1000;
        (0..n)
            .map(|i| {
                let t = i as f64 / rate as f64;
                let s = (2.0 * std::f64::consts::PI * low * t).sin()
                    + (2.0 * std::f64::consts::PI * high * t).sin();
                (s / 2.0 * 0.4 * 32767.0) as i16
            })
            .collect()
    }

    fn silence(rate: u32, ms: usize) -> Vec<i16> {
        vec![0i16; rate as usize * ms / 1000]
    }

    /// All 16 DTMF key frequencies, mirroring `DtmfGenerator`.
    fn digit_freqs(digit: char) -> (f64, f64) {
        match digit {
            '1' => (697.0, 1209.0),
            '2' => (697.0, 1336.0),
            '3' => (697.0, 1477.0),
            '4' => (770.0, 1209.0),
            '5' => (770.0, 1336.0),
            '6' => (770.0, 1477.0),
            '7' => (852.0, 1209.0),
            '8' => (852.0, 1336.0),
            '9' => (852.0, 1477.0),
            '*' => (941.0, 1209.0),
            '0' => (941.0, 1336.0),
            '#' => (941.0, 1477.0),
            'A' => (697.0, 1633.0),
            'B' => (770.0, 1633.0),
            'C' => (852.0, 1633.0),
            'D' => (941.0, 1633.0),
            _ => unreachable!("not a DTMF digit: {digit}"),
        }
    }

    const ALL_DIGITS: &str = "123456789*0#ABCD";

    /// Encode `pcm` through `codec` in 20 ms frames and decode it back,
    /// returning the received PCM (what a remote detector would see).
    fn codec_roundtrip(codec: CodecType, pcm: &[i16]) -> Vec<i16> {
        let mut encoder = create_encoder(codec);
        let mut decoder = create_decoder(codec);
        let frame = codec.samplerate() as usize * 20 / 1000;
        let mut out = Vec::with_capacity(pcm.len());
        for chunk in pcm.chunks(frame) {
            let encoded = encoder.encode(chunk);
            out.extend_from_slice(&decoder.decode(&encoded));
        }
        out
    }

    /// Generate `digits` as DTMF tones, run them through a codec roundtrip,
    /// then detect from the decoded stream. `tone_ms`/`gap_ms` shape the key
    /// press timing; 100/100 also survives non-20ms analysis blocks.
    fn codec_loopback_detect(codec: CodecType, digits: &str, tone_ms: usize, gap_ms: usize) -> String {
        let rate = codec.samplerate();
        let generator = DtmfGenerator::new(rate);
        let mut pcm = Vec::new();
        for d in digits.chars() {
            pcm.extend_from_slice(&generator.generate(d, tone_ms as u32));
            pcm.extend_from_slice(&silence(rate, gap_ms));
        }
        let decoded = codec_roundtrip(codec, &pcm);
        let block = rate as usize * 20 / 1000;
        feed_blocks(&mut InbandDtmfDetector::new(rate), &decoded, block)
    }

    // ── 1. algorithm alignment with the upstream Go implementation ─────

    /// Upstream `TestGoertzel`: two literal samples at 44100 Hz, exact energy
    /// expectations. The final digits can differ between libm `cos`
    /// implementations (Go's portable math vs the platform libm), hence the
    /// 1e-12 tolerance — the values themselves come from the upstream test.
    #[test]
    fn goertzel_matches_upstream_reference() {
        let mut g = Goertzel::new(697.0, 44100.0);
        g.process_sample(42.0);
        g.process_sample(84.0);
        let expected = [
            (697.0, 0.8980292970055115),
            (770.0, 0.8975953139667142),
            (852.0, 0.8970565383230518),
            (941.0, 0.8964104403348228),
        ];
        for (freq, want) in expected {
            let mut g = Goertzel::new(freq, 44100.0);
            g.process_sample(42.0);
            g.process_sample(84.0);
            assert!(
                (g.energy - want).abs() < 1e-12,
                "energies[{freq}] = {} != {want}",
                g.energy
            );
        }
        // Deterministic repeats: refresh resets state bit-exactly.
        g.refresh();
        g.process_sample(42.0);
        g.process_sample(84.0);
        assert!((g.energy - 0.8980292970055115).abs() < 1e-12);
    }

    // ── 2. upstream testdata fixtures ───────────────────────────────────

    /// Upstream `TestDecodeDTMF 8k`: the reference recording must decode to
    /// the full sequence (chunked exactly like the Go test: 320-byte reads).
    #[test]
    fn testdata_8k_sequence() {
        let samples = read_raw_pcm(TESTDATA_8K);
        let mut detector = InbandDtmfDetector::new(8000);
        let decoded = feed_blocks(&mut detector, &samples, 160);
        assert_eq!(decoded, "123456654321");
    }

    /// Upstream `TestDecodeDTMF mix 16k`: a noisy mix must yield exactly one
    /// digit ("1") — no talk-off beyond it.
    #[test]
    fn testdata_16k_mix_single_digit() {
        let samples = read_raw_pcm(TESTDATA_MIX_16K);
        let mut detector = InbandDtmfDetector::new(16000);
        let decoded = feed_blocks(&mut detector, &samples, 320);
        assert_eq!(decoded, "1");
    }

    // ── 3. upstream table bug regression ────────────────────────────────

    /// Upstream's frequency table maps `#` to 477 Hz (typo for 1477), so the
    /// reference implementation can never detect `#`. This port must.
    #[test]
    fn hash_digit_uses_1477hz_fixing_upstream_table_typo() {
        for rate in [8000u32, 16000, 48000] {
            let block = rate as usize * 20 / 1000;
            let pcm = synth_tone(941.0, 1477.0, rate, 100);
            let mut detector = InbandDtmfDetector::new(rate);
            assert_eq!(feed_blocks(&mut detector, &pcm, block), "#", "rate {rate}");
        }
    }

    // ── 4. full digit matrix ────────────────────────────────────────────

    #[test]
    fn sixteen_digit_matrix_all_rates_20ms_blocks() {
        for rate in [8000u32, 16000, 48000] {
            let block = rate as usize * 20 / 1000;
            let mut pcm = Vec::new();
            for d in ALL_DIGITS.chars() {
                let (low, high) = digit_freqs(d);
                pcm.extend_from_slice(&synth_tone(low, high, rate, 60));
                pcm.extend_from_slice(&silence(rate, 60));
            }
            let mut detector = InbandDtmfDetector::new(rate);
            assert_eq!(
                feed_blocks(&mut detector, &pcm, block),
                ALL_DIGITS,
                "rate {rate}"
            );
        }
    }

    /// Non-20ms analysis blocks (e.g. a peer packetizing at 30ms ptime plus
    /// jitter backlog): longer tones still classify, in the right order.
    #[test]
    fn sixteen_digit_matrix_30ms_blocks() {
        for rate in [8000u32, 16000] {
            let block = rate as usize * 30 / 1000;
            let mut pcm = Vec::new();
            for d in ALL_DIGITS.chars() {
                let (low, high) = digit_freqs(d);
                pcm.extend_from_slice(&synth_tone(low, high, rate, 120));
                pcm.extend_from_slice(&silence(rate, 120));
            }
            let mut detector = InbandDtmfDetector::new(rate);
            assert_eq!(
                feed_blocks(&mut detector, &pcm, block),
                ALL_DIGITS,
                "rate {rate} with 30ms blocks"
            );
        }
    }

    // ── 5. debounce / edge-trigger semantics ────────────────────────────

    /// A single held key is ONE event, no matter how long the tone lasts.
    #[test]
    fn long_press_emits_exactly_one_event() {
        let rate = 8000;
        let pcm = synth_tone(770.0, 1336.0, rate, 3000);
        let mut detector = InbandDtmfDetector::new(rate);
        assert_eq!(feed_blocks(&mut detector, &pcm, 160), "5");
    }

    /// Same key re-pressed 60ms later is suppressed by the 200 ms same-key
    /// window (indistinguishable from in-tone packet-loss tearing); after a
    /// long enough pause the same key fires again.
    #[test]
    fn same_key_repeat_within_200ms_is_suppressed_then_fires() {
        let rate = 8000;
        let tone = synth_tone(697.0, 1209.0, rate, 60);
        let mut pcm = Vec::new();
        pcm.extend_from_slice(&tone);
        pcm.extend_from_slice(&silence(rate, 60));
        pcm.extend_from_slice(&tone); // second press 120ms after trigger
        let mut detector = InbandDtmfDetector::new(rate);
        assert_eq!(feed_blocks(&mut detector, &pcm, 160), "1", "suppressed repeat");

        let mut pcm = Vec::new();
        pcm.extend_from_slice(&tone);
        pcm.extend_from_slice(&silence(rate, 300));
        pcm.extend_from_slice(&tone); // well past the press interval
        let mut detector = InbandDtmfDetector::new(rate);
        assert_eq!(feed_blocks(&mut detector, &pcm, 160), "11", "fires after interval");
    }

    /// Different keys 60ms apart both fire (no same-key suppression).
    #[test]
    fn different_keys_close_together_both_fire() {
        let rate = 8000;
        let mut pcm = Vec::new();
        pcm.extend_from_slice(&synth_tone(697.0, 1209.0, rate, 40));
        pcm.extend_from_slice(&silence(rate, 60));
        pcm.extend_from_slice(&synth_tone(697.0, 1336.0, rate, 40));
        let mut detector = InbandDtmfDetector::new(rate);
        assert_eq!(feed_blocks(&mut detector, &pcm, 160), "12");
    }

    // ── 6. talk-off rejection ───────────────────────────────────────────

    /// Repo voice prompts (TTS prompts + real recordings) must produce zero
    /// digits. These are the corpus the neighbor/harmonic guards were
    /// calibrated on — upstream's bare classifier false-triggers on 3 of
    /// these files.
    #[test]
    fn voice_corpus_produces_no_digits() {
        let mut paths: Vec<String> = [
            "bridge-demo.wav",
            "goodbye.wav",
            "invalid.wav",
            "menu.wav",
            "timeout.wav",
            "welcome.wav",
        ]
        .iter()
        .map(|name| format!("../../regression/assets/prompts/{name}"))
        .collect();
        paths.push("../../fixtures/hello_book_course_zh_16k.wav".to_string());
        paths.push("../../fixtures/sample.wav".to_string());
        for path in &paths {
            let (rate, samples) = read_wav_mono(path);
            let block = rate as usize * 20 / 1000;
            let mut detector = InbandDtmfDetector::new(rate);
            let decoded = feed_blocks(&mut detector, &samples, block);
            assert_eq!(decoded, "", "talk-off on {path}");
        }
    }

    /// Call-progress tones (dial / ringback / busy) must be rejected — they
    /// are dual-tone like DTMF but out of the keypad matrix.
    #[test]
    fn signaling_tones_are_rejected() {
        for (name, low, high) in [
            ("dial", 350.0, 440.0),
            ("ringback", 440.0, 480.0),
            ("busy", 480.0, 620.0),
        ] {
            let rate = 8000;
            let pcm = synth_tone(low, high, rate, 1000);
            let mut detector = InbandDtmfDetector::new(rate);
            assert_eq!(
                feed_blocks(&mut detector, &pcm, 160),
                "",
                "{name} tone must not classify"
            );
        }
    }

    /// Every pure tone in the voice band must be rejected (a single
    /// frequency can never light both groups).
    #[test]
    fn pure_tone_sweep_is_rejected() {
        let rate = 8000u32;
        for freq in (300..=3400u32).step_by(50) {
            let pcm = synth_tone(freq as f64, freq as f64, rate, 200);
            let mut detector = InbandDtmfDetector::new(rate);
            let decoded = feed_blocks(&mut detector, &pcm, 160);
            assert_eq!(decoded, "", "{freq} Hz pure tone must not classify");
        }
    }

    /// Band-limited white noise must not classify.
    #[test]
    fn white_noise_is_rejected() {
        let mut seed = 0x1234_5678u64;
        let mut next = move || {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((seed >> 33) as i32 % 8000) as i16
        };
        for rate in [8000u32, 16000] {
            let pcm: Vec<i16> = (0..rate as usize * 2).map(|_| next()).collect();
            let block = rate as usize * 20 / 1000;
            let mut detector = InbandDtmfDetector::new(rate);
            assert_eq!(feed_blocks(&mut detector, &pcm, block), "", "rate {rate}");
        }
    }

    // ── 7. codec loopback (the Opus case is the make-or-break one) ─────

    #[test]
    fn g711_loopback_detects_all_digits() {
        for codec in [CodecType::PCMU, CodecType::PCMA] {
            assert_eq!(
                codec_loopback_detect(codec, ALL_DIGITS, 100, 100),
                ALL_DIGITS,
                "{codec:?} loopback"
            );
        }
    }

    /// G.722 (16 kHz WB) roundtrip must stay decodable.
    #[test]
    fn g722_loopback_detects_all_digits() {
        assert_eq!(
            codec_loopback_detect(CodecType::G722, ALL_DIGITS, 100, 100),
            ALL_DIGITS
        );
    }

    /// Low-bitrate Opus (16 kbps mono WB, VBR) is the critical case: the
    /// codec's spectral discard must not destroy the dual tones. All 16
    /// digits must survive a full roundtrip.
    ///
    /// VBR matters: at CBR 16 kbps CELT folds the 710–880 Hz band onto a
    /// single line, mapping B (770+1633) onto C (852+1633) — measured, all
    /// row information for `B` is lost on the wire. Real-world VoIP runs
    /// VBR (libopus default), which keeps the tones decodable.
    #[test]
    fn opus_low_bitrate_loopback_detects_all_digits() {
        let rate = 16000u32;
        let generator = DtmfGenerator::new(rate);
        let mut opus = OpusEncoder::new(rate, 1);
        opus.set_bitrate(16_000);
        opus.set_cbr(false);
        opus.set_complexity(8);
        let mut encoder: Box<dyn audio_codec::Encoder> = Box::new(opus);
        let mut decoder = audio_codec::opus::OpusDecoder::new(16000, 1);
        let frame = rate as usize * 20 / 1000;
        let mut pcm = Vec::new();
        for d in ALL_DIGITS.chars() {
            pcm.extend_from_slice(&generator.generate(d, 100));
            pcm.extend_from_slice(&silence(rate, 100));
        }
        let mut decoded = Vec::with_capacity(pcm.len());
        for chunk in pcm.chunks(frame) {
            let packet = encoder.encode(chunk);
            decoded.extend_from_slice(&decoder.decode(&packet));
        }
        assert_eq!(
            feed_blocks(&mut InbandDtmfDetector::new(rate), &decoded, frame),
            ALL_DIGITS,
            "opus VBR 16kbps loopback must keep all digits decodable"
        );
    }

    /// The detector must also survive the RTCP-grade 24 kbps setting and a
    /// 32 kbps NB-style config at 8 kHz (narrowband phones over opus).
    #[test]
    fn opus_variable_bitrates_detect_digits() {
        for (rate, bitrate) in [(16000u32, 24_000i32), (8000, 16_000)] {
            let generator = DtmfGenerator::new(rate);
            let mut opus = OpusEncoder::new(rate, 1);
            opus.set_bitrate(bitrate);
            let mut encoder: Box<dyn audio_codec::Encoder> = Box::new(opus);
            let mut decoder = audio_codec::opus::OpusDecoder::new(rate, 1);
            let frame = rate as usize * 20 / 1000;
            let mut pcm = Vec::new();
            for d in "508#".chars() {
                pcm.extend_from_slice(&generator.generate(d, 100));
                pcm.extend_from_slice(&silence(rate, 100));
            }
            let mut decoded = Vec::with_capacity(pcm.len());
            for chunk in pcm.chunks(frame) {
                let packet = encoder.encode(chunk);
                decoded.extend_from_slice(&decoder.decode(&packet));
            }
            assert_eq!(
                feed_blocks(&mut InbandDtmfDetector::new(rate), &decoded, frame),
                "508#",
                "opus {bitrate} bps @{rate} loopback"
            );
        }
    }

    // ── 8. mode serde + u8 conversion ───────────────────────────────────

    #[test]
    fn inband_dtmf_mode_serde_and_atomic_encoding() {
        assert_eq!(InbandDtmfMode::default(), InbandDtmfMode::Auto);
        assert_eq!(serde_json::to_value(InbandDtmfMode::Auto).unwrap(), "auto");
        assert_eq!(serde_json::to_value(InbandDtmfMode::AlwaysOn).unwrap(), "on");
        assert_eq!(serde_json::to_value(InbandDtmfMode::Off).unwrap(), "off");
        for (wire, mode) in [
            ("auto", InbandDtmfMode::Auto),
            ("on", InbandDtmfMode::AlwaysOn),
            ("off", InbandDtmfMode::Off),
        ] {
            assert_eq!(serde_json::from_value::<InbandDtmfMode>(serde_json::json!(wire)).unwrap(), mode);
            assert_eq!(mode.to_u8(), mode.to_u8());
            assert_eq!(InbandDtmfMode::from_u8(mode.to_u8()), mode);
        }
        // Unknown values fall back to Auto, never panic.
        assert_eq!(InbandDtmfMode::from_u8(200), InbandDtmfMode::Auto);
    }

    // ── 9. leg policy (per-leg attach/teardown through the Leg) ────────

    const SDP_PCMU_NO_DTMF: &str = "v=0\r\n\
        o=- 1 1 IN IP4 127.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 127.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 4000 RTP/AVP 0\r\n\
        a=rtpmap:0 PCMU/8000\r\n";
    const SDP_PCMU_WITH_DTMF: &str = "v=0\r\n\
        o=- 1 1 IN IP4 127.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 127.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 4000 RTP/AVP 0 101\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n";
    const SDP_PCMA_NO_DTMF: &str = "v=0\r\n\
        o=- 1 1 IN IP4 127.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 127.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 4000 RTP/AVP 8\r\n\
        a=rtpmap:8 PCMA/8000\r\n";

    /// `auto`: a leg with NO negotiated telephone-event starts the detector.
    #[tokio::test]
    async fn policy_auto_starts_without_telephone_event() {
        let leg = crate::leg::LegInner::new("inband-auto", &crate::leg::LegConfig::rtp_pcmu(), None)
            .expect("leg");
        assert!(!leg.inband_dtmf_active(), "inactive before any SDP");
        leg.apply_profile_from_sdp(SDP_PCMU_NO_DTMF).await.unwrap();
        assert!(leg.inband_dtmf_active(), "auto must start without telephone-event");
        // Re-applying the SAME profile must not rebuild the tap.
        let generation = leg.inband_dtmf_generation();
        leg.apply_profile_from_sdp(SDP_PCMU_NO_DTMF).await.unwrap();
        assert_eq!(leg.inband_dtmf_generation(), generation, "same profile must not rebuild");
        leg.stop();
        assert!(!leg.inband_dtmf_active(), "stop() must tear the tap down");
    }

    /// `auto`: once telephone-event IS negotiated (initial answer or
    /// re-INVITE), the in-band detector must stop — RFC 2833 owns the channel.
    #[tokio::test]
    async fn policy_auto_stops_when_telephone_event_negotiated() {
        let leg = crate::leg::LegInner::new("inband-mixed", &crate::leg::LegConfig::rtp_pcmu(), None)
            .expect("leg");
        leg.apply_profile_from_sdp(SDP_PCMU_NO_DTMF).await.unwrap();
        assert!(leg.inband_dtmf_active());
        // A re-INVITE negotiates telephone-event → auto stops the tap.
        leg.apply_profile_from_sdp(SDP_PCMU_WITH_DTMF).await.unwrap();
        assert!(!leg.inband_dtmf_active(), "auto must stop once telephone-event exists");
        leg.stop();
    }

    /// `off`: never starts, even without telephone-event.
    #[tokio::test]
    async fn policy_off_never_starts() {
        let mut cfg = crate::leg::LegConfig::rtp_pcmu();
        cfg.inband_dtmf = InbandDtmfMode::Off;
        let leg = crate::leg::LegInner::new("inband-off", &cfg, None).expect("leg");
        leg.apply_profile_from_sdp(SDP_PCMU_NO_DTMF).await.unwrap();
        assert!(!leg.inband_dtmf_active());
        leg.stop();
    }

    /// `on`: runs even alongside a negotiated telephone-event (the tap only
    /// consumes the audio PT, so the two channels never double-report).
    #[tokio::test]
    async fn policy_always_on_runs_alongside_telephone_event() {
        let mut cfg = crate::leg::LegConfig::rtp_pcmu();
        cfg.inband_dtmf = InbandDtmfMode::AlwaysOn;
        let leg = crate::leg::LegInner::new("inband-on", &cfg, None).expect("leg");
        leg.apply_profile_from_sdp(SDP_PCMU_WITH_DTMF).await.unwrap();
        assert!(leg.inband_dtmf_active());
        leg.stop();
    }

    /// A re-INVITE that changes the audio codec rebuilds the tap (the old
    /// detector's sample rate / decoder is wrong for the new stream).
    #[tokio::test]
    async fn policy_reinvite_codec_change_rebuilds_tap() {        let leg = crate::leg::LegInner::new("inband-reinvite", &crate::leg::LegConfig::rtp_pcmu(), None)
            .expect("leg");
        leg.apply_profile_from_sdp(SDP_PCMU_NO_DTMF).await.unwrap();
        assert!(leg.inband_dtmf_active());
        let generation = leg.inband_dtmf_generation();
        // re-INVITE to PCMA (different PT, same clock): tap must be rebuilt.
        leg.apply_profile_from_sdp(SDP_PCMA_NO_DTMF).await.unwrap();
        assert!(leg.inband_dtmf_active());
        assert_eq!(
            leg.inband_dtmf_generation(),
            generation + 1,
            "codec change must rebuild the tap"
        );
        leg.stop();
        assert!(!leg.inband_dtmf_active());
    }

    // ── 10. shared analysis tap × AMD (crate::amd sinks on the same tap) ──

    /// Arming AMD on a leg whose DTMF sink already runs adds the AMD sink to
    /// the SAME tap in place — the DTMF detector must not be rebuilt.
    #[tokio::test]
    async fn amd_arm_adds_sink_in_place_without_disturbing_dtmf() {
        let leg = crate::leg::LegInner::new("amd-shared", &crate::leg::LegConfig::rtp_pcmu(), None)
            .expect("leg");
        leg.apply_profile_from_sdp(SDP_PCMU_NO_DTMF).await.unwrap();
        assert!(leg.inband_dtmf_active(), "precondition: DTMF sink running");
        let dtmf_generation = leg.inband_dtmf_generation();

        let (outcome_tx, _outcome_rx) = tokio::sync::mpsc::channel(2);
        leg.arm_amd(crate::amd::AmdArmament {
            detector: crate::amd::create_amd_detector(&crate::amd::AmdConfig::default())
                .expect("heuristic detector"),
            local_speech: Default::default(),
            outcome_tx,
        });
        assert!(leg.amd_armed());
        assert!(leg.inband_dtmf_active(), "DTMF sink must survive the AMD arm");
        assert_eq!(
            leg.inband_dtmf_generation(),
            dtmf_generation,
            "AMD arm must add its sink in place, not rebuild the tap"
        );

        // Disarm: the AMD sink goes away, DTMF keeps running untouched.
        leg.disarm_amd();
        assert!(!leg.amd_armed());
        assert!(leg.inband_dtmf_active());
        assert_eq!(
            leg.inband_dtmf_generation(),
            dtmf_generation,
            "AMD disarm must not disturb the DTMF sink"
        );
        leg.stop();
    }

    /// On a leg that negotiated telephone-event (no DTMF sink) the armed AMD
    /// sink keeps the shared tap alive on its own; disarming it tears the tap
    /// down entirely (no sinks → no observer, no decode task).
    #[tokio::test]
    async fn amd_only_leg_runs_and_teardowns_the_tap() {
        let leg = crate::leg::LegInner::new("amd-only", &crate::leg::LegConfig::rtp_pcmu(), None)
            .expect("leg");
        leg.apply_profile_from_sdp(SDP_PCMU_WITH_DTMF).await.unwrap();
        assert!(!leg.inband_dtmf_active(), "precondition: RFC 2833 owns DTMF");

        let (outcome_tx, _outcome_rx) = tokio::sync::mpsc::channel(2);
        leg.arm_amd(crate::amd::AmdArmament {
            detector: crate::amd::create_amd_detector(&crate::amd::AmdConfig::default())
                .expect("heuristic detector"),
            local_speech: Default::default(),
            outcome_tx,
        });
        assert!(leg.amd_armed());
        assert_eq!(
            leg.amd_generation(),
            1,
            "the AMD sink must have been created on its own tap"
        );

        // Re-applying the same profile must be a no-op (stable generation).
        leg.apply_profile_from_sdp(SDP_PCMU_WITH_DTMF).await.unwrap();
        assert_eq!(leg.amd_generation(), 1, "same profile must not rebuild");

        leg.disarm_amd();
        assert!(!leg.amd_armed());
        assert!(!leg.inband_dtmf_active());
        leg.stop();
    }
}

