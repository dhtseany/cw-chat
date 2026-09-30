use std::f64::consts::TAU;

/// Search range and grid for the automatic tuner.
pub const MIN_HZ: f64 = 300.0;
pub const MAX_HZ: f64 = 1200.0;
const STEP_HZ: f64 = 10.0;
/// A tone counts as keyed CW when its recent peak is this far above the band's
/// noise floor (13 dB, in power) ...
const PEAK_OVER_FLOOR: f32 = 20.0;
/// ... and it recently dropped back near the floor (a gap between elements), which
/// rules out steady carriers.
const GAP_OVER_FLOOR: f32 = 4.0;
/// Ignore tones quieter than -60 dBFS (amplitude 1e-3, in power).
const MIN_POWER: f32 = 1e-6;
/// Frames a candidate must hold (within 20 Hz) before it is reported (about 30 ms).
const STABLE_FRAMES: u32 = 3;

/// Finds the strongest keyed tone in 300–1200 Hz: a bank of Hann-windowed DFT bins
/// every 10 Hz over a 21 ms window, evaluated every 10.7 ms. Much coarser in time
/// than the decoder's detector, but it sees the whole band at once.
pub struct Tuner {
    freqs: Vec<f64>,
    cos: Vec<Vec<f32>>,
    sin: Vec<Vec<f32>>,
    norm: f32,
    history: Vec<f32>,
    oldest: usize,
    filled: usize,
    since_hop: usize,
    hop: usize,
    /// Per bin: recent peak power, and the last half second of powers (for gaps).
    peak: Vec<f32>,
    recent: Vec<Vec<f32>>,
    recent_at: usize,
    peak_decay: f32,
    floor: f32,
    /// Bins judged keyed in the latest frame, and the strongest keyed peak.
    keyed: Vec<bool>,
    best_peak: f32,
    last: Option<f64>,
    stable: u32,
}

impl Tuner {
    pub fn new(sample_rate: u32) -> Self {
        let n = (sample_rate as usize / 1000 * 1024) / 48; // 1024 at 48 kHz
        let hop = n / 2;
        let hann: Vec<f64> = (0..n)
            .map(|k| 0.5 - 0.5 * (TAU * k as f64 / n as f64).cos())
            .collect();
        let freqs: Vec<f64> = (0..)
            .map(|i| MIN_HZ + i as f64 * STEP_HZ)
            .take_while(|&f| f <= MAX_HZ)
            .collect();
        let table = |f: f64, trig: fn(f64) -> f64| -> Vec<f32> {
            (0..n)
                .map(|k| (hann[k] * trig(TAU * f * k as f64 / sample_rate as f64)) as f32)
                .collect()
        };
        let frames_per_half_second = (sample_rate as usize / 2 / hop).max(1);
        let frame_seconds = hop as f64 / sample_rate as f64;
        Self {
            cos: freqs.iter().map(|&f| table(f, f64::cos)).collect(),
            sin: freqs.iter().map(|&f| table(f, f64::sin)).collect(),
            norm: 2.0 / hann.iter().sum::<f64>() as f32,
            history: vec![0.0; n],
            oldest: 0,
            filled: 0,
            since_hop: 0,
            hop,
            peak: vec![0.0; freqs.len()],
            recent: vec![vec![0.0; frames_per_half_second]; freqs.len()],
            recent_at: 0,
            // Peak halves in 0.3 s, so the tone keying right now wins over one that
            // stopped a moment ago.
            peak_decay: 0.5f32.powf((frame_seconds / 0.3) as f32),
            floor: 0.0,
            keyed: vec![false; freqs.len()],
            best_peak: 0.0,
            last: None,
            stable: 0,
            freqs,
        }
    }

    /// Push one sample; returns a frequency when a keyed tone has been steady for a
    /// few frames.
    pub fn push(&mut self, sample: f32) -> Option<f64> {
        let n = self.history.len();
        self.history[self.oldest] = sample;
        self.oldest = (self.oldest + 1) % n;
        self.filled = (self.filled + 1).min(n);
        self.since_hop += 1;
        if self.since_hop < self.hop || self.filled < n {
            return None;
        }
        self.since_hop = 0;
        self.frame()
    }

    fn frame(&mut self) -> Option<f64> {
        let n = self.history.len();
        let mut powers = Vec::with_capacity(self.freqs.len());
        for bin in 0..self.freqs.len() {
            let (cos, sin) = (&self.cos[bin], &self.sin[bin]);
            let (mut re, mut im) = (0.0f32, 0.0f32);
            for k in 0..n {
                let x = self.history[(self.oldest + k) % n];
                re += x * cos[k];
                im += x * sin[k];
            }
            powers.push((re * re + im * im) * self.norm * self.norm);
        }
        // Band floor: the median bin, smoothed. A few tones cannot move a median.
        let mut sorted = powers.clone();
        sorted.sort_by(f32::total_cmp);
        let median = sorted[sorted.len() / 2];
        self.floor = if self.floor == 0.0 {
            median
        } else {
            self.floor + (median - self.floor) * 0.2
        };
        let slot = self.recent_at;
        self.recent_at = (self.recent_at + 1) % self.recent[0].len();
        let mut best: Option<(usize, f32)> = None;
        for (bin, &power) in powers.iter().enumerate() {
            self.peak[bin] = power.max(self.peak[bin] * self.peak_decay);
            self.recent[bin][slot] = power;
            let gap = self.recent[bin]
                .iter()
                .copied()
                .fold(f32::INFINITY, f32::min);
            let keyed = self.peak[bin] > MIN_POWER
                && self.peak[bin] > self.floor * PEAK_OVER_FLOOR
                && gap < self.floor.max(MIN_POWER / PEAK_OVER_FLOOR) * GAP_OVER_FLOOR;
            self.keyed[bin] = keyed;
            if keyed && best.is_none_or(|(_, p)| self.peak[bin] > p) {
                best = Some((bin, self.peak[bin]));
            }
        }
        self.best_peak = best.map_or(0.0, |(_, peak)| peak);
        let Some((bin, _)) = best else {
            self.stable = 0;
            self.last = None;
            return None;
        };
        let freq = self.refine(bin);
        match self.last {
            Some(last) if (freq - last).abs() <= 20.0 => self.stable += 1,
            _ => self.stable = 1,
        }
        self.last = Some(freq);
        (self.stable >= STABLE_FRAMES).then_some(freq)
    }

    /// Whether a keyed tone was seen within 20 Hz of `freq` in the latest frame, no
    /// more than 20 dB below the strongest one (weaker is window leakage from it).
    pub fn keyed_near(&self, freq: f64) -> bool {
        self.freqs
            .iter()
            .zip(&self.keyed)
            .zip(&self.peak)
            .any(|((&f, &keyed), &peak)| {
                keyed && (f - freq).abs() <= 20.0 && peak >= self.best_peak / 100.0
            })
    }

    /// Parabolic interpolation of the peak between neighbouring bins, in dB.
    fn refine(&self, bin: usize) -> f64 {
        if bin == 0 || bin + 1 >= self.freqs.len() {
            return self.freqs[bin];
        }
        let db = |b: usize| 10.0 * (self.peak[b].max(1e-20) as f64).log10();
        let (a, b, c) = (db(bin - 1), db(bin), db(bin + 1));
        let denominator = a - 2.0 * b + c;
        let shift = if denominator.abs() > 1e-9 {
            (0.5 * (a - c) / denominator).clamp(-0.5, 0.5)
        } else {
            0.0
        };
        self.freqs[bin] + shift * STEP_HZ
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        cw::{oscillator, timing},
        morse::encoder::encode,
    };

    fn last_candidate(samples: &[f32]) -> Option<f64> {
        let mut tuner = Tuner::new(48_000);
        samples.iter().filter_map(|&s| tuner.push(s)).last()
    }

    #[test]
    fn finds_keyed_tone_and_ignores_carriers_and_silence() {
        let cw = oscillator::render(
            &timing::schedule(&encode("PARIS PARIS").unwrap()),
            20.0,
            837.0,
            0.2,
        )
        .unwrap();
        let found = last_candidate(&cw).unwrap();
        assert!((found - 837.0).abs() < 6.0, "{found}");

        // A steady carrier at 600 Hz louder than the CW does not win.
        let mixed: Vec<f32> = cw
            .iter()
            .enumerate()
            .map(|(i, s)| s + (0.5 * (TAU * 600.0 * i as f64 / 48_000.0).sin()) as f32)
            .collect();
        let found = last_candidate(&mixed).unwrap();
        assert!((found - 837.0).abs() < 6.0, "{found}");

        assert_eq!(last_candidate(&vec![0.0; 48_000]), None);
    }
}
