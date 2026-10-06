//! Optional statistics from PCM that already exists before egress encoding.
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct VolumeStatsConfig {
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct VolumeSummary {
    pub measurement: &'static str,
    pub direction: &'static str,
    pub window_ms: u64,
    pub observed_ms: u64,
    pub window_start_micros: u64,
    pub window_end_micros: u64,
    pub sample_count: u64,
    pub clipped_count: u64,
    pub bucket_count: usize,
    pub rms: f64,
    pub peak: u32,
    pub attenuation_dbov: u8,
    pub min_attenuation_dbov: u8,
    pub max_attenuation_dbov: u8,
}

#[derive(Default)]
struct Energy {
    samples: u64,
    squares: u64,
    clipped: u64,
    peak: u32,
}

pub(crate) struct Bucket {
    start_micros: u64,
    end_micros: u64,
    at: Instant,
    start: Instant,
    energy: Energy,
}

#[derive(Default)]
pub(crate) struct VolumeState {
    buckets: Option<VecDeque<Bucket>>,
    // The last observed window survives live-window expiry and source replacement.
    pub(crate) last: Option<VolumeSummary>,
}

pub(crate) type VolumeSink = Arc<Mutex<VolumeState>>;

pub(crate) struct VolumeSampler {
    interval: Duration,
    start: Instant,
    start_micros: u64,
    energy: Energy,
    sink: VolumeSink,
    window: Duration,
    log_interval: Duration,
    last_emit: Instant,
    last_observed: Instant,
    last_observed_micros: u64,
    identity: Option<(String, String)>,
}

impl VolumeSampler {
    pub(crate) fn new(config: crate::quality_stats::QualityStatsConfig, sink: VolumeSink,
                     identity: Option<(String, String)>) -> Self {
        *sink.lock() = VolumeState { buckets: Some(VecDeque::new()), last: None };
        let now = Instant::now();
        Self {
            interval: Duration::from_millis(config.sample_interval_ms),
            window: Duration::from_millis(config.window_ms),
            log_interval: Duration::from_millis(config.log_interval_ms),
            last_emit: now,
            last_observed: now,
            last_observed_micros: unix_micros(),
            identity,
            start: now,
            start_micros: unix_micros(),
            energy: Energy::default(),
            sink,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.finish();
        self.energy = Energy::default();
        self.start = Instant::now();
        self.start_micros = unix_micros();
        if let Some(buckets) = self.sink.lock().buckets.as_mut() {
            buckets.clear();
        }
    }

    pub(crate) fn finish(&mut self) {
        // Teardown latency must not extend the PCM observation's timestamps.
        let has_samples = self.energy.samples > 0;
        if has_samples {
            if let Some(buckets) = self.sink.lock().buckets.as_mut() {
                if buckets.len() == 241 { buckets.pop_front(); }
                buckets.push_back(Bucket {
                    start_micros: self.start_micros,
                    end_micros: self.last_observed_micros,
                    at: self.last_observed,
                    start: self.start,
                    energy: std::mem::take(&mut self.energy),
                });
            }
        }
        if let Some(value) = self.archive(self.last_observed) {
            // Only newly consumed PCM earns a terminal log, never a repeated finish.
            if has_samples { self.emit(&value); }
        }
    }

    fn emit(&self, value: &VolumeSummary) {
        if let Some((session_id, leg_id)) = &self.identity {
            match serde_json::to_string(value) {
                Ok(volume) => tracing::info!(session_id = %session_id, leg_id = %leg_id,
                    egress_mode = "pcm_pacing", volume = %volume, "PCM egress volume window"),
                Err(error) => tracing::warn!(session_id = %session_id, leg_id = %leg_id,
                    %error, "Failed to serialize PCM egress volume window"),
            }
        }
    }

    fn archive(&self, at: Instant) -> Option<VolumeSummary> {
        let cutoff = at.checked_sub(self.window).unwrap_or(at);
        let mut state = self.sink.lock();
        let value = summarize(state.buckets.as_ref()?.iter()
            .filter(|bucket| bucket.start >= cutoff && bucket.at <= at), self.window);
        if value.is_some() { state.last = value.clone(); }
        value
    }

    pub(crate) fn observe(&mut self, pcm: &[i16]) {
        let now = Instant::now();
        if now.duration_since(self.start) >= self.interval {
            let end_micros = unix_micros();
            // A stalled/parked task cannot claim continuous PCM observation.
            if now.duration_since(self.start) <= self.interval * 2 && self.energy.samples > 0 {
                if let Some(buckets) = self.sink.lock().buckets.as_mut() {
                    if buckets.len() == 241 {
                        buckets.pop_front();
                    }
                    buckets.push_back(Bucket {
                        start_micros: self.start_micros,
                        end_micros,
                        at: now,
                        start: self.start,
                        energy: std::mem::take(&mut self.energy),
                    });
                }
            }
            self.energy = Energy::default();
            self.start = now;
            self.start_micros = end_micros;
            if now.duration_since(self.last_emit) >= self.log_interval {
                self.last_emit = now;
                if let Some(value) = self.archive(now) {
                    self.emit(&value);
                }
            }
        }
        self.last_observed = now;
        self.last_observed_micros = unix_micros();
        for sample in pcm {
            let value = *sample as i64;
            self.energy.squares += (value * value) as u64;
            self.energy.samples += 1;
            self.energy.peak = self.energy.peak.max(value.unsigned_abs() as u32);
            if *sample == i16::MIN || *sample == i16::MAX {
                self.energy.clipped += 1;
            }
        }
    }
}

fn unix_micros() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_micros() as u64)
        .unwrap_or_default()
}

fn attenuation(squares: u64, samples: u64) -> u8 {
    if squares == 0 {
        return 127;
    }
    let mean_square = squares as f64 / samples as f64;
    (-10.0 * (mean_square / (32768.0 * 32768.0)).log10())
        .round()
        .clamp(0.0, 126.0) as u8
}

pub(crate) fn summary(sink: &VolumeSink, window: Duration) -> Option<VolumeSummary> {
    let now = Instant::now();
    let cutoff = now.checked_sub(window).unwrap_or(now);
    let mut state = sink.lock();
    let buckets = state.buckets.as_mut()?;
    while buckets.front().is_some_and(|bucket| bucket.at <= cutoff) {
        buckets.pop_front();
    }
    summarize(buckets.iter().filter(|bucket| bucket.start >= cutoff && bucket.at <= now), window)
}

fn summarize<'a>(buckets: impl Iterator<Item = &'a Bucket>, window: Duration) -> Option<VolumeSummary> {
    let mut total = Energy::default();
    let mut min = 127;
    let mut max = 0;
    let mut count = 0;
    let mut observed_ms = 0;
    let mut start_micros = u64::MAX;
    let mut end_micros = 0;
    for bucket in buckets {
        observed_ms += bucket.at.duration_since(bucket.start).as_millis() as u64;
        start_micros = start_micros.min(bucket.start_micros);
        end_micros = end_micros.max(bucket.end_micros);
        total.samples += bucket.energy.samples;
        total.squares += bucket.energy.squares;
        total.clipped += bucket.energy.clipped;
        total.peak = total.peak.max(bucket.energy.peak);
        let level = attenuation(bucket.energy.squares, bucket.energy.samples);
        min = min.min(level);
        max = max.max(level);
        count += 1;
    }
    (total.samples > 0).then(|| VolumeSummary {
        window_ms: window.as_millis() as u64,
        observed_ms,
        window_start_micros: start_micros,
        window_end_micros: end_micros,
        measurement: "pbx_pcm_pre_encode",
        direction: "egress",
        sample_count: total.samples,
        clipped_count: total.clipped,
        bucket_count: count,
        rms: (total.squares as f64 / total.samples as f64).sqrt(),
        peak: total.peak,
        attenuation_dbov: attenuation(total.squares, total.samples),
        min_attenuation_dbov: min,
        max_attenuation_dbov: max,
    })
}
