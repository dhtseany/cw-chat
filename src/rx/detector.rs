use std::f64::consts::TAU;

/// One detector output: the tone amplitude (a full-scale sine reads 1.0) and the
/// complex bin value, whose phase change between hops gives the exact pitch.
#[derive(Debug, Clone, Copy, Default)]
pub struct Bin {
    pub level: f32,
    pub re: f32,
    pub im: f32,
}

/// Hann-windowed single-bin DFT (a Goertzel-style tone detector) evaluated every hop.
/// The window is 10 ms (about 100 Hz wide) and hops are 5 ms.
pub struct ToneDetector {
    sample_rate: f64,
    frequency: f64,
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
            sample_rate: sample_rate as f64,
            frequency,
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
        self.frequency = frequency;
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

    pub fn frequency(&self) -> f64 {
        self.frequency
    }

    /// Pitch offset from the detector frequency, in Hz, from two consecutive bins of
    /// the same tone. The window slides one hop between them, so the bin rotates by
    /// 2π·f·hop/rate; the part left after removing the detector frequency's own
    /// rotation is the offset. Unambiguous within ±rate/(2·hop), ±100 Hz here, which
    /// covers the detector's passband.
    pub fn offset_hz(&self, previous: Bin, current: Bin) -> f64 {
        // current · conj(previous); the bins are conjugated DFT values, so the
        // rotation runs backwards.
        let re = (current.re * previous.re + current.im * previous.im) as f64;
        let im = (current.im * previous.re - current.re * previous.im) as f64;
        let turn = self.hop as f64 / self.sample_rate;
        let expected = -TAU * self.frequency * turn;
        let residual =
            (im.atan2(re) - expected + std::f64::consts::PI).rem_euclid(TAU) - std::f64::consts::PI;
        -residual / (TAU * turn)
    }

    /// Push one sample; every hop returns the detector output.
    pub fn push(&mut self, sample: f32) -> Option<Bin> {
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
        Some(Bin {
            level: (re * re + im * im).sqrt() * self.norm,
            re: re * self.norm,
            im: im * self.norm,
        })
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
                .level
        };
        assert!((last(tone(700.0)) - 0.5).abs() < 0.01);
        assert!(last(tone(1000.0)) < 0.01);
    }

    #[test]
    fn measures_pitch_offset() {
        let mut detector = ToneDetector::new(48_000, 700.0);
        for f in [640.0, 700.0, 713.0, 760.0] {
            let bins: Vec<Bin> = (0..4800)
                .map(|i| (0.3 * (TAU * f * i as f64 / 48_000.0).sin()) as f32)
                .filter_map(|s| detector.push(s))
                .collect();
            let [.., a, b] = bins[..] else { panic!() };
            let offset = detector.offset_hz(a, b);
            assert!(
                (offset - (f - 700.0)).abs() < 0.5,
                "{f} Hz read {offset:+.2}"
            );
        }
    }
}
