//! AMD — Answering Machine / AI-assistant detection on the callee leg.
//!
//! After an outbound leg is answered, the remote party's decoded ingress PCM
//! (fed by the leg's shared [`crate::pcm_analysis_tap::PcmAnalysisTap`]) is
//! classified in real time:
//!
//! - **Human** — a short opener ("喂?") then silence while the caller talks.
//!   Detected early so further analysis is a cheap no-op.
//! - **AnsweringMachine** — a long unbroken announcement (voicemail greeting)
//!   and/or the classic sustained ~1 kHz beep after the greeting.
//! - **AiAssistant** — TTS-like speech: sentence runs with regular pauses,
//!   no beep. DSP heuristics only (best-effort); ambiguous cases stay
//!   `Unknown` or fall back to `AnsweringMachine` with an `extra.subclass`
//!   hint rather than guessing.
//! - **Silence** / **Unknown** — window elapsed without usable evidence.
//!
//! The analysis is strictly passive: it never touches the media path, only
//! records a verdict to CDR and emits an RWI `call_amd_detected` event.
//!
//! # Engines
//!
//! [`AmdDetector`] is the pluggable engine trait. v1 ships the zero-dependency
//! [`HeuristicAmdDetector`] (RMS speech tracking + Goertzel beep filter built
//! on the in-band DTMF crate's [`crate::inband_dtmf::Goertzel`]). ONNX
//! (in-process) and HTTP-API engines implement the same trait later; unknown
//! engine names in [`AmdConfig`] fall back to heuristic with a warning so a
//! config typo degrades instead of breaking the call.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::warn;

use crate::inband_dtmf::Goertzel;
use crate::pcm_analysis_tap::{AnalysisSinkKind, PcmBlockSink};

// ── configuration ────────────────────────────────────────────────────────

/// AMD configuration. Every tunable is `Option` so the four config sources
/// (global `[proxy] amd` → trunk → route hints → `app_params`) overlay
/// field-by-field via [`AmdConfig::overlay`]; only explicitly-set fields of a
/// higher-priority source override lower ones. Engine-private knobs travel in
/// `params` (an opaque bag the selected engine parses at creation time —
/// unknown keys warn, they never fail the call).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AmdConfig {
    /// Master switch; absent means off (AMD is opt-in per trunk/route/call).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Engine name: `"heuristic"` (default) today; `"onnx"` / `"http"` later.
    /// Unknown names fall back to heuristic with a warning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    /// Detection window after answer (seconds). Default 20.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// Continuous-announcement length that flags a machine (ms). Default 6000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_greeting_ms: Option<u64>,
    /// Detect the voicemail beep. Default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detect_beep: Option<bool>,
    /// Engine-private parameters, parsed and validated by the engine.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub params: HashMap<String, serde_json::Value>,
}

impl AmdConfig {
    /// Overlay `higher` (a closer-to-the-call source) onto `self`: only
    /// explicitly-set fields and `params` keys of `higher` take effect.
    pub fn overlay(&mut self, higher: AmdConfig) {
        if higher.enabled.is_some() {
            self.enabled = higher.enabled;
        }
        if higher.engine.is_some() {
            self.engine = higher.engine;
        }
        if higher.timeout_secs.is_some() {
            self.timeout_secs = higher.timeout_secs;
        }
        if higher.max_greeting_ms.is_some() {
            self.max_greeting_ms = higher.max_greeting_ms;
        }
        if higher.detect_beep.is_some() {
            self.detect_beep = higher.detect_beep;
        }
        for (key, value) in higher.params {
            self.params.insert(key, value);
        }
    }
}

// ── verdict types ────────────────────────────────────────────────────────

/// Classification outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AmdResult {
    /// Live human answered.
    Human,
    /// Voicemail / answering machine.
    AnsweringMachine,
    /// TTS / AI assistant audio (best-effort under heuristic DSP).
    AiAssistant,
    /// Never heard the remote party within the window.
    Silence,
    /// Speech seen but the evidence was inconclusive.
    Unknown,
}

impl std::fmt::Display for AmdResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Human => "human",
            Self::AnsweringMachine => "answering_machine",
            Self::AiAssistant => "ai_assistant",
            Self::Silence => "silence",
            Self::Unknown => "unknown",
        };
        f.write_str(s)
    }
}

/// One AMD decision. Produced once per armed leg attempt (early verdict or
/// window timeout / hangup flush) and fanned out to CDR + RWI event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AmdVerdict {
    pub result: AmdResult,
    /// 0.0..1.0 engine confidence.
    pub confidence: f32,
    /// ms after answer when the decision was reached.
    pub decision_time_ms: u64,
    /// Detected greeting/opener duration (first speech run).
    pub speech_duration_ms: u64,
    /// Sustained ~1 kHz beep seen (voicemail hallmark).
    pub beep_detected: bool,
    /// Engine identifier (e.g. `"heuristic"`).
    pub engine: String,
    /// Engine-private evidence (run list, subclass hints, …).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, serde_json::Value>,
}

// ── engine trait ─────────────────────────────────────────────────────────

/// Pluggable AMD engine. Fed decoded ingress PCM of the remote party in
/// 20 ms blocks; returns the verdict once confident (early exit) and keeps
/// returning `None` afterwards.
pub trait AmdDetector: Send {
    /// Feed one block of remote-party audio. `Some` = decided.
    fn feed_block(&mut self, samples: &[i16], sample_rate: u32) -> Option<AmdVerdict>;
    /// The caller (our side) started speaking — strong Human evidence.
    fn notify_local_speech(&mut self) {}
    /// Call end / window expiry: flush with whatever evidence accumulated.
    fn finalize(&mut self) -> Option<AmdVerdict>;
}

/// Engine dispatch by [`AmdConfig::engine`]. Unknown names fall back to the
/// heuristic engine (warn) so a config typo degrades instead of failing.
pub fn create_amd_detector(config: &AmdConfig) -> anyhow::Result<Box<dyn AmdDetector>> {
    let engine = config.engine.as_deref().unwrap_or("heuristic");
    if engine != "heuristic" {
        // Future engines implement the same trait (onnx in-process, http api);
        // they plug in here and in AmdConfig::engine docs.
        warn!("amd: unknown engine {engine:?}, falling back to heuristic");
    }
    Ok(Box::new(HeuristicAmdDetector::new(
        HeuristicParams::resolve(config),
    )))
}

// ── heuristic engine ─────────────────────────────────────────────────────

/// Fully-resolved heuristic parameters (no Options left).
#[derive(Debug, Clone)]
pub(crate) struct HeuristicParams {
    timeout_secs: u64,
    max_greeting_ms: u64,
    detect_beep: bool,
    /// Speech RMS threshold (i16 units). Default 400.
    rms_threshold: f64,
    /// Sustained speech needed to open a run (ms). Default 40.
    min_speech_ms: u64,
    /// Silence that closes a speech run (ms). Default 800.
    pause_ms: u64,
    /// First-run length still counted as a human opener (ms). Default 2500.
    human_max_first_run_ms: u64,
    /// Post-opener silence that confirms Human (ms). Default 2000.
    human_confirm_silence_ms: u64,
    /// Silence after a machine-candidate greeting that confirms "no beep"
    /// voicemail (ms). Default 4000.
    post_greeting_wait_ms: u64,
    /// Voicemail beep frequency (Hz). Default 1000.
    beep_freq_hz: f64,
    /// Sustained beep length (ms). Default 300.
    beep_min_ms: u64,
    /// Beep relative-energy threshold (0..1). Default 0.03.
    beep_energy: f64,
}

impl HeuristicParams {
    /// Resolve from [`AmdConfig`]: common fields from the top level, engine
    /// tuning from the `params` bag. Unknown keys and bad values warn and use
    /// defaults (analysis degradation, never a call failure).
    fn resolve(config: &AmdConfig) -> Self {
        let mut p = Self {
            timeout_secs: config.timeout_secs.unwrap_or(20),
            max_greeting_ms: config.max_greeting_ms.unwrap_or(6000),
            detect_beep: config.detect_beep.unwrap_or(true),
            rms_threshold: 400.0,
            min_speech_ms: 40,
            pause_ms: 800,
            human_max_first_run_ms: 2500,
            human_confirm_silence_ms: 2000,
            post_greeting_wait_ms: 4000,
            beep_freq_hz: 1000.0,
            beep_min_ms: 300,
            beep_energy: 0.03,
        };
        for (key, value) in &config.params {
            match key.as_str() {
                "rms_threshold" => p.rms_threshold = value.as_f64().unwrap_or(p.rms_threshold),
                "min_speech_ms" => p.min_speech_ms = bag_u64(value).unwrap_or(p.min_speech_ms),
                "pause_ms" => p.pause_ms = bag_u64(value).unwrap_or(p.pause_ms),
                "human_max_first_run_ms" => {
                    p.human_max_first_run_ms = bag_u64(value).unwrap_or(p.human_max_first_run_ms);
                }
                "human_confirm_silence_ms" => {
                    p.human_confirm_silence_ms =
                        bag_u64(value).unwrap_or(p.human_confirm_silence_ms);
                }
                "post_greeting_wait_ms" => {
                    p.post_greeting_wait_ms = bag_u64(value).unwrap_or(p.post_greeting_wait_ms);
                }
                "beep_freq_hz" => p.beep_freq_hz = value.as_f64().unwrap_or(p.beep_freq_hz),
                "beep_min_ms" => p.beep_min_ms = bag_u64(value).unwrap_or(p.beep_min_ms),
                "beep_energy" => p.beep_energy = value.as_f64().unwrap_or(p.beep_energy),
                other => {
                    warn!("amd/heuristic: ignoring unknown params key {other:?}");
                }
            }
        }
        p
    }
}

fn bag_u64(value: &serde_json::Value) -> Option<u64> {
    value.as_f64().map(|v| v as u64)
}

/// Heuristic AMD state machine (RMS speech tracking + Goertzel beep filter).
///
/// All thresholds operate on 20 ms blocks (the analysis tap's fixed block),
/// so every duration converts to a block count at comparison time.
pub(crate) struct HeuristicAmdDetector {
    p: HeuristicParams,
    engine: &'static str,
    // per-block timing
    blocks: u64,
    done: Option<AmdVerdict>,
    // speech-run tracking
    speech_run_blocks: u32,
    silence_blocks: u32,
    total_speech_blocks: u64,
    runs: Vec<u64>,
    // classification state
    machine_candidate: bool,
    candidate_at_ms: Option<u64>,
    local_speech_seen: bool,
    // beep detection
    beep: Option<Goertzel>,
    beep_rate: u32,
    beep_blocks: u32,
    beep_at_ms: Option<u64>,
    pre_speech_beep: bool,
}

impl HeuristicAmdDetector {
    pub(crate) fn new(p: HeuristicParams) -> Self {
        Self {
            p,
            engine: "heuristic",
            blocks: 0,
            done: None,
            speech_run_blocks: 0,
            silence_blocks: 0,
            total_speech_blocks: 0,
            runs: Vec::new(),
            machine_candidate: false,
            candidate_at_ms: None,
            local_speech_seen: false,
            beep: None,
            beep_rate: 0,
            beep_blocks: 0,
            beep_at_ms: None,
            pre_speech_beep: false,
        }
    }

    fn elapsed_ms(&self) -> u64 {
        self.blocks * 20
    }

    /// Decide with whatever evidence accumulated (window expiry or hangup).
    fn classify(&self) -> AmdVerdict {
        let elapsed = self.elapsed_ms();
        let first_run = self.runs.first().copied().unwrap_or(0);
        let total_speech_ms = self.total_speech_blocks * 20;
        let mut extra = BTreeMap::new();
        extra.insert("runs_ms".to_string(), serde_json::json!(self.runs));
        extra.insert("total_speech_ms".to_string(), serde_json::json!(total_speech_ms));
        if let Some(at) = self.beep_at_ms {
            extra.insert("beep_at_ms".to_string(), serde_json::json!(at));
        }
        if self.local_speech_seen {
            extra.insert("local_speech".to_string(), serde_json::json!(true));
        }
        let (result, confidence) = if total_speech_ms == 0 {
            (AmdResult::Silence, 0.9)
        } else if self.beep_at_ms.is_some() {
            (AmdResult::AnsweringMachine, 0.85)
        } else if self.machine_candidate {
            (AmdResult::AnsweringMachine, 0.6)
        } else if self.runs.len() >= 2 {
            (AmdResult::AiAssistant, 0.6)
        } else if self.runs.len() == 1 && first_run < self.p.human_max_first_run_ms {
            (AmdResult::Human, 0.6)
        } else {
            (AmdResult::Unknown, 0.5)
        };
        if result == AmdResult::AnsweringMachine && self.beep_at_ms.is_none() {
            extra.insert("subclass".to_string(), serde_json::json!("no_beep"));
        }
        if self.pre_speech_beep {
            extra.insert("pre_speech_tone".to_string(), serde_json::json!(true));
        }
        AmdVerdict {
            result,
            confidence,
            decision_time_ms: elapsed,
            speech_duration_ms: first_run,
            beep_detected: self.beep_at_ms.is_some(),
            engine: self.engine.to_string(),
            extra,
        }
    }

    fn finish(&mut self, verdict: AmdVerdict) -> Option<AmdVerdict> {
        if self.done.is_none() {
            self.done = Some(verdict.clone());
            return Some(verdict);
        }
        None
    }

    fn block_rms(samples: &[i16]) -> f64 {
        if samples.is_empty() {
            return 0.0;
        }
        let sum: f64 = samples.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
        (sum / samples.len() as f64).sqrt()
    }

    /// Run the beep filter over one block; returns the block's 1 kHz relative
    /// energy (0..1).
    fn beep_energy_of(&mut self, samples: &[i16], sample_rate: u32) -> f64 {
        if self.beep_rate != sample_rate || self.beep.is_none() {
            self.beep = Some(Goertzel::new(self.p.beep_freq_hz, f64::from(sample_rate)));
            self.beep_rate = sample_rate;
        }
        let Some(beep) = self.beep.as_mut() else { return 0.0 };
        for &s in samples {
            beep.process_sample(f64::from(s) / 32768.0);
        }
        let energy = beep.energy();
        beep.refresh();
        energy
    }
}

impl AmdDetector for HeuristicAmdDetector {
    fn feed_block(&mut self, samples: &[i16], sample_rate: u32) -> Option<AmdVerdict> {
        if self.done.is_some() {
            return None;
        }
        self.blocks += 1;
        let elapsed = self.elapsed_ms();

        // Window expiry → flush with accumulated evidence.
        if elapsed >= self.p.timeout_secs.saturating_mul(1000) {
            return self.finalize();
        }

        let speech = Self::block_rms(samples) >= self.p.rms_threshold;

        // Beep filter: a voicemail beep is a LOUD steady tone (its RMS also
        // reads as "speech"), so the discriminator is sustained 1 kHz energy
        // for ≥300 ms — speech formants wander and cannot hold that.
        if self.p.detect_beep {
            let energy = self.beep_energy_of(samples, sample_rate);
            if energy >= self.p.beep_energy {
                self.beep_blocks += 1;
                if u64::from(self.beep_blocks) * 20 >= self.p.beep_min_ms
                    && self.beep_at_ms.is_none()
                {
                    self.beep_at_ms = Some(elapsed);
                }
            } else {
                self.beep_blocks = 0;
                if self.total_speech_blocks == 0 && energy > 0.0 {
                    self.pre_speech_beep = true;
                }
            }
        }

        // Speech-run tracking + verdict checks.
        let min_speech_blocks = (self.p.min_speech_ms / 20).max(1) as u32;
        let pause_blocks = (self.p.pause_ms / 20).max(1) as u32;

        if speech {
            self.silence_blocks = 0;
            self.speech_run_blocks += 1;
            self.total_speech_blocks += 1;
            let _ = min_speech_blocks; // run opening uses it below

            // New speech after a closed long greeting → the remote party keeps
            // talking in sentences (TTS/AI pattern). Beep evidence outranks it.
            if self.machine_candidate
                && self.beep_at_ms.is_none()
                && self
                    .candidate_at_ms
                    .is_some_and(|at| elapsed > at + self.p.pause_ms)
            {
                let verdict = AmdVerdict {
                    result: AmdResult::AiAssistant,
                    confidence: 0.7,
                    decision_time_ms: elapsed,
                    speech_duration_ms: self.runs.first().copied().unwrap_or(0),
                    beep_detected: false,
                    engine: self.engine.to_string(),
                    extra: BTreeMap::from([
                        ("runs_ms".to_string(), serde_json::json!(self.runs)),
                        ("subclass".to_string(), serde_json::json!("sentence_runs")),
                    ]),
                };
                return self.finish(verdict);
            }
        } else {
            self.silence_blocks += 1;
            if self.speech_run_blocks > 0 && self.silence_blocks >= pause_blocks {
                // Speech run closed.
                let run_ms = u64::from(self.speech_run_blocks) * 20;
                let is_first = self.runs.is_empty();
                self.runs.push(run_ms);
                self.speech_run_blocks = 0;

                if is_first && run_ms >= self.p.max_greeting_ms {
                    self.machine_candidate = true;
                    self.candidate_at_ms = Some(elapsed);
                }
            }

            // Human confirm: short opener(s), no machine signs, then sustained
            // silence (the caller is expected to talk now).
            if self.speech_run_blocks == 0
                && !self.runs.is_empty()
                && !self.machine_candidate
                && self.beep_at_ms.is_none()
                && self.runs.len() <= 3
                && self.runs.iter().all(|r| *r < self.p.human_max_first_run_ms)
                && u64::from(self.silence_blocks) * 20 >= self.p.human_confirm_silence_ms
            {
                let confidence = if self.local_speech_seen { 0.95 } else { 0.85 };
                let verdict = AmdVerdict {
                    result: AmdResult::Human,
                    confidence,
                    decision_time_ms: elapsed,
                    speech_duration_ms: self.runs.first().copied().unwrap_or(0),
                    beep_detected: false,
                    engine: self.engine.to_string(),
                    extra: BTreeMap::from([
                        ("runs_ms".to_string(), serde_json::json!(self.runs)),
                        (
                            "total_speech_ms".to_string(),
                            serde_json::json!(self.total_speech_blocks * 20),
                        ),
                        (
                            "local_speech".to_string(),
                            serde_json::json!(self.local_speech_seen),
                        ),
                    ]),
                };
                return self.finish(verdict);
            }

            // No-beep voicemail confirm: long greeting, then long silence
            // without beep → machine (subclass greeting_no_beep).
            if self.machine_candidate
                && self.beep_at_ms.is_none()
                && self.candidate_at_ms
                    .is_some_and(|at| elapsed - at >= self.p.post_greeting_wait_ms)
            {
                let verdict = AmdVerdict {
                    result: AmdResult::AnsweringMachine,
                    confidence: 0.6,
                    decision_time_ms: elapsed,
                    speech_duration_ms: self.runs.first().copied().unwrap_or(0),
                    beep_detected: false,
                    engine: self.engine.to_string(),
                    extra: BTreeMap::from([
                        ("runs_ms".to_string(), serde_json::json!(self.runs)),
                        (
                            "subclass".to_string(),
                            serde_json::json!("greeting_no_beep"),
                        ),
                    ]),
                };
                return self.finish(verdict);
            }
        }

        // Beep after speech → voicemail (highest-confidence machine sign).
        if let Some(beep_at) = self.beep_at_ms {
            if self.total_speech_blocks > 0 && elapsed >= beep_at {
                let verdict = AmdVerdict {
                    result: AmdResult::AnsweringMachine,
                    confidence: 0.9,
                    decision_time_ms: elapsed,
                    speech_duration_ms: self.runs.first().copied().unwrap_or(0),
                    beep_detected: true,
                    engine: self.engine.to_string(),
                    extra: BTreeMap::from([
                        ("runs_ms".to_string(), serde_json::json!(self.runs)),
                        ("beep_at_ms".to_string(), serde_json::json!(beep_at)),
                    ]),
                };
                return self.finish(verdict);
            }
        }

        None
    }

    fn notify_local_speech(&mut self) {
        self.local_speech_seen = true;
    }

    fn finalize(&mut self) -> Option<AmdVerdict> {
        if self.done.is_some() {
            return None;
        }
        let verdict = self.classify();
        self.finish(verdict)
    }
}

// ── tap sink wiring ──────────────────────────────────────────────────────

/// Detector slot shared between the leg (arm/disarm/rebuild) and the tap sink.
/// Holding the detector in an `Arc<Mutex<..>>` means a codec change (re-INVITE
/// rebuild) can recreate the transport while the analysis state survives.
pub type SharedDetector = Arc<parking_lot::Mutex<Option<Box<dyn AmdDetector>>>>;

/// Everything the leg needs to arm AMD on the shared analysis tap.
pub struct AmdArmament {
    pub detector: Box<dyn AmdDetector>,
    /// Set by the session when the local side starts speaking (v1 wiring is
    /// optional; the heuristic treats it as extra Human evidence).
    pub local_speech: Arc<AtomicBool>,
    /// Verdict channel into the session (small; at most one verdict per arm).
    pub outcome_tx: mpsc::Sender<AmdVerdict>,
}

/// AMD sink for the shared analysis tap: feeds the shared detector and
/// forwards the verdict to the session channel.
struct AmdSink {
    slot: SharedDetector,
    local_speech: Arc<AtomicBool>,
    outcome_tx: mpsc::Sender<AmdVerdict>,
}

impl PcmBlockSink for AmdSink {
    fn feed_block(&mut self, samples: &[i16], sample_rate: u32) {
        let mut guard = self.slot.lock();
        let Some(detector) = guard.as_mut() else {
            return;
        };
        if self.local_speech.swap(false, Ordering::AcqRel) {
            detector.notify_local_speech();
        }
        if let Some(verdict) = detector.feed_block(samples, sample_rate) {
            let _ = self.outcome_tx.try_send(verdict);
        }
    }
}

/// Build the registry entry for the shared analysis tap: `(kind, sink)`.
pub fn make_sink(
    slot: SharedDetector,
    local_speech: Arc<AtomicBool>,
    outcome_tx: mpsc::Sender<AmdVerdict>,
) -> (AnalysisSinkKind, Box<dyn PcmBlockSink>) {
    (
        AnalysisSinkKind::Amd,
        Box::new(AmdSink {
            slot,
            local_speech,
            outcome_tx,
        }),
    )
}

// ── tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 8000;
    const BLOCK: usize = (RATE as usize * 20) / 1000; // 160 samples = 20 ms

    fn params(timeout_secs: u64) -> HeuristicParams {
        HeuristicParams {
            timeout_secs,
            max_greeting_ms: 6000,
            detect_beep: true,
            rms_threshold: 400.0,
            min_speech_ms: 40,
            pause_ms: 800,
            human_max_first_run_ms: 2500,
            human_confirm_silence_ms: 2000,
            post_greeting_wait_ms: 4000,
            beep_freq_hz: 1000.0,
            beep_min_ms: 300,
            beep_energy: 0.03,
        }
    }

    fn ms_blocks(ms: u64) -> usize {
        (ms as usize * RATE as usize) / 1000
    }

    /// Noise burst with a hard envelope (RMS ≈ 4600, well over threshold).
    fn noise(ms: u64, seed: u64) -> Vec<i16> {
        let n = ms_blocks(ms);
        let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (0..n)
            .map(|_| {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((x >> 33) as i64 % 8000) as i16
            })
            .collect()
    }

    fn silence(ms: u64) -> Vec<i16> {
        vec![0i16; ms_blocks(ms)]
    }

    /// Sustained sine tone at `freq` Hz, amplitude 8000 (a loud beep).
    fn tone(ms: u64, freq: f64) -> Vec<i16> {
        let n = ms_blocks(ms);
        (0..n)
            .map(|i| {
                let t = i as f64 / RATE as f64;
                ((2.0 * std::f64::consts::PI * freq * t).sin() * 8000.0) as i16
            })
            .collect()
    }

    /// Feed whole 20 ms blocks until a verdict or the audio runs out.
    fn feed(detector: &mut dyn AmdDetector, samples: &[i16]) -> Option<AmdVerdict> {
        for chunk in samples.chunks(BLOCK) {
            if chunk.len() < BLOCK {
                break;
            }
            if let Some(v) = detector.feed_block(chunk, RATE) {
                return Some(v);
            }
        }
        None
    }

    #[test]
    fn human_short_opener_then_silence() {
        let mut d = HeuristicAmdDetector::new(params(20));
        assert!(feed(&mut d, &noise(1000, 1)).is_none(), "no verdict mid-run");
        let v = feed(&mut d, &silence(2600)).expect("human verdict expected");
        assert_eq!(v.result, AmdResult::Human);
        assert!(v.confidence >= 0.85);
        assert!(!v.beep_detected);
        assert_eq!(v.speech_duration_ms, 1000);
    }

    #[test]
    fn human_two_short_runs() {
        let mut d = HeuristicAmdDetector::new(params(20));
        assert!(feed(&mut d, &noise(800, 2)).is_none());
        assert!(feed(&mut d, &silence(900)).is_none());
        assert!(feed(&mut d, &noise(700, 3)).is_none());
        let v = feed(&mut d, &silence(2400)).expect("human verdict expected");
        assert_eq!(v.result, AmdResult::Human);
        assert_eq!(v.extra.get("runs_ms"), Some(&serde_json::json!([800, 700])));
    }

    #[test]
    fn voicemail_greeting_then_beep() {
        let mut d = HeuristicAmdDetector::new(params(20));
        assert!(
            feed(&mut d, &noise(6500, 4)).is_none(),
            "greeting alone must not decide"
        );
        assert!(feed(&mut d, &silence(400)).is_none());
        let v = feed(&mut d, &tone(400, 1000.0)).expect("machine verdict at beep");
        assert_eq!(v.result, AmdResult::AnsweringMachine);
        assert!(v.beep_detected);
        assert!(v.confidence >= 0.9);
    }

    #[test]
    fn beep_right_after_greeting_pause_outranks_ai_pattern() {
        // Greeting closes with a ≥800 ms pause (candidate set), then the beep:
        // the machine-with-beep verdict must win over the sentence pattern.
        let mut d = HeuristicAmdDetector::new(params(20));
        assert!(feed(&mut d, &noise(6500, 12)).is_none());
        assert!(feed(&mut d, &silence(900)).is_none());
        let v = feed(&mut d, &tone(400, 1000.0)).expect("machine verdict at beep");
        assert_eq!(v.result, AmdResult::AnsweringMachine);
        assert!(v.beep_detected);
    }

    #[test]
    fn voicemail_greeting_no_beep() {
        // Greeting 6.5 s, run closes at ~800 ms silence; the no-beep wait
        // (4 s from candidate) needs ≈4.8 s of silence total.
        let mut d = HeuristicAmdDetector::new(params(20));
        assert!(feed(&mut d, &noise(6500, 5)).is_none());
        let v = feed(&mut d, &silence(5000)).expect("no-beep machine verdict expected");
        assert_eq!(v.result, AmdResult::AnsweringMachine);
        assert!(!v.beep_detected);
        assert_eq!(
            v.extra.get("subclass"),
            Some(&serde_json::json!("greeting_no_beep"))
        );
    }

    #[test]
    fn ai_assistant_sentence_runs() {
        let mut d = HeuristicAmdDetector::new(params(9));
        assert!(feed(&mut d, &noise(3500, 6)).is_none());
        assert!(feed(&mut d, &silence(900)).is_none());
        assert!(
            feed(&mut d, &noise(3500, 7)).is_none(),
            "multi-run needs timeout/flush"
        );
        // Window expiry flushes with the multi-run pattern.
        let v = feed(&mut d, &silence(9500)).expect("timeout flush expected");
        assert_eq!(v.result, AmdResult::AiAssistant);
        assert!(v.confidence < 0.8, "best-effort classification");
    }

    #[test]
    fn silence_verdict() {
        let mut d = HeuristicAmdDetector::new(params(3));
        let v = feed(&mut d, &silence(4000)).expect("silence verdict expected");
        assert_eq!(v.result, AmdResult::Silence);
        assert_eq!(v.speech_duration_ms, 0);
    }

    #[test]
    fn finalize_flushes_pending_state() {
        let mut d = HeuristicAmdDetector::new(params(60));
        feed(&mut d, &noise(1000, 8));
        // Not yet confirmed (silence < 2 s) — hangup flush must decide.
        feed(&mut d, &silence(1000));
        let v = d.finalize().expect("finalize must flush");
        assert_eq!(v.result, AmdResult::Human);
        // Post-verdict feeds and flushes are no-ops.
        assert!(d.feed_block(&noise(1000, 9)[..BLOCK], RATE).is_none());
        assert!(d.finalize().is_none());
    }

    #[test]
    fn local_speech_boosts_human_confidence() {
        let mut d = HeuristicAmdDetector::new(params(20));
        feed(&mut d, &noise(1000, 10));
        d.notify_local_speech();
        let v = feed(&mut d, &silence(2600)).expect("human verdict expected");
        assert_eq!(v.result, AmdResult::Human);
        assert!((v.confidence - 0.95).abs() < 1e-6);
    }

    #[test]
    fn ambiguous_middle_run_is_unknown() {
        // 4 s first run: too long for human, too short for the machine rule.
        let mut d = HeuristicAmdDetector::new(params(6));
        feed(&mut d, &noise(4000, 11));
        let v = feed(&mut d, &silence(3000)).expect("timeout flush expected");
        assert_eq!(v.result, AmdResult::Unknown);
    }

    #[test]
    fn config_overlay_field_and_params_granularity() {
        let mut base = AmdConfig {
            enabled: Some(true),
            engine: Some("heuristic".into()),
            timeout_secs: Some(20),
            max_greeting_ms: None,
            detect_beep: Some(true),
            params: HashMap::from([
                ("rms_threshold".to_string(), serde_json::json!(500)),
                ("pause_ms".to_string(), serde_json::json!(900)),
            ]),
        };
        base.overlay(AmdConfig {
            enabled: None,
            engine: None,
            timeout_secs: Some(30),
            max_greeting_ms: Some(7000),
            detect_beep: None,
            params: HashMap::from([("rms_threshold".to_string(), serde_json::json!(650))]),
        });
        assert_eq!(base.enabled, Some(true));
        assert_eq!(base.timeout_secs, Some(30));
        assert_eq!(base.max_greeting_ms, Some(7000));
        assert_eq!(
            base.params.get("rms_threshold"),
            Some(&serde_json::json!(650))
        );
        assert_eq!(base.params.get("pause_ms"), Some(&serde_json::json!(900)));
    }

    #[test]
    fn config_serde_roundtrip() {
        let json = r#"{"enabled": true, "engine": "heuristic", "timeout_secs": 15,
            "params": {"rms_threshold": 350}}"#;
        let config: AmdConfig = serde_json::from_str(json).expect("parse");
        assert_eq!(config.enabled, Some(true));
        assert_eq!(config.timeout_secs, Some(15));
        assert_eq!(
            config.params.get("rms_threshold"),
            Some(&serde_json::json!(350))
        );
    }

    #[test]
    fn unknown_engine_falls_back_to_heuristic() {
        let config = AmdConfig {
            enabled: Some(true),
            engine: Some("onnx".into()),
            ..Default::default()
        };
        let mut detector = create_amd_detector(&config).expect("fallback must not fail");
        // Heuristic behaviour behind the fallback: 21 s of silence → Silence.
        let v = feed(detector.as_mut(), &silence(21_000)).expect("flush expected");
        assert_eq!(v.result, AmdResult::Silence);
        assert_eq!(v.engine, "heuristic");
    }
}
