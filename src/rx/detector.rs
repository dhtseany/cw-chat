use std::f64::consts::TAU;

/// Hann-windowed single-bin DFT (a Goertzel-style tone detector) evaluated every hop.
/// The window is 10 ms (about 100 Hz wide) and hops are 5 ms.
pub struct ToneDetector {
    cos: Vec<f32>,
    sin: Vec<f32>,
    norm: f32,
    history: Vec<f32>,
    oldest: usize,
    filled: usize,
    since_hop: usize,
    hop: usize,
}

impl ToneDetector {
    pub fn new(sample_rate: u32, frequency: f64) -> Self {
        let window = (sample_rate / 100) as usize;
        let mut detector = Self {
            cos: Vec::new(),
            sin: Vec::new(),
            norm: 1.0,
            history: vec![0.0; window],
            oldest: 0,
            filled: 0,
            since_hop: 0,
            hop: window / 2,
        };
        detector.set_frequency(sample_rate, frequency);
        detector
    }

    pub fn set_frequency(&mut self, sample_rate: u32, frequency: f64) {
        let n = self.history.len();
        let hann: Vec<f64> = (0..n)
            .map(|k| 0.5 - 0.5 * (TAU * k as f64 / n as f64).cos())
            .collect();
        let phase = |k: usize| TAU * frequency * k as f64 / sample_rate as f64;
        self.cos = (0..n).map(|k| (hann[k] * phase(k).cos()) as f32).collect();
        self.sin = (0..n).map(|k| (hann[k] * phase(k).sin()) as f32).collect();
        self.norm = 2.0 / hann.iter().sum::<f64>() as f32;
    }

    pub fn hop_seconds(&self, sample_rate: u32) -> f64 {
        self.hop as f64 / sample_rate as f64
    }

    /// Push one sample; every hop returns the tone amplitude (a full-scale sine reads 1.0).
    pub fn push(&mut self, sample: f32) -> Option<f32> {
        let n = self.history.len();
        self.history[self.oldest] = sample;
        self.oldest = (self.oldest + 1) % n;
        self.filled = (self.filled + 1).min(n);
        self.since_hop += 1;
        if self.since_hop < self.hop || self.filled < n {
            return None;
        }
        self.since_hop = 0;
        let (mut re, mut im) = (0.0f32, 0.0f32);
        for k in 0..n {
            let x = self.history[(self.oldest + k) % n];
            re += x * self.cos[k];
            im += x * self.sin[k];
        }
        Some((re * re + im * im).sqrt() * self.norm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn measures_amplitude_and_rejects_off_frequency() {
        let tone = |f: f64| -> Vec<f32> {
            (0..4800)
                .map(|i| (0.5 * (TAU * f * i as f64 / 48_000.0).sin()) as f32)
                .collect()
        };
        let last = |samples: Vec<f32>| {
            let mut detector = ToneDetector::new(48_000, 700.0);
            samples
                .into_iter()
                .filter_map(|s| detector.push(s))
                .last()
                .unwrap()
        };
        assert!((last(tone(700.0)) - 0.5).abs() < 0.01);
        assert!(last(tone(1000.0)) < 0.01);
    }
}
