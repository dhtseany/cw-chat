use super::detector::ToneDetector;
use crate::morse::table;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RxEvent {
    /// A decoded character; `*` marks an unknown pattern.
    Char(char),
    WordGap,
    /// Long silence after text: the end of an over.
    Idle,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Status {
    /// Latest tone amplitude, noise floor, and recent peak (full-scale sine = 1.0).
    pub level: f32,
    pub noise: f32,
    pub peak: f32,
    pub keyed: bool,
    pub wpm: f64,
}

/// Consecutive hops that must disagree before the key state flips (10 ms).
const DEBOUNCE: u32 = 2;
/// The peak must exceed the noise floor by this ratio (9.5 dB) before keying.
const MIN_SNR: f32 = 3.0;
/// Tones quieter than this (-60 dBFS) are ignored.
const MIN_LEVEL: f32 = 1e-3;
const IDLE_SECONDS: f64 = 2.5;
/// A tone longer than this is a carrier or a new noise floor, not an element.
const CARRIER_SECONDS: f64 = 2.0;
const MAX_PATTERN: usize = 10;
const WARMUP_HOPS: usize = 20;
/// Marks shorter than this fraction of a dot are noise spikes.
const GLITCH: f64 = 0.35;

/// Adaptive CW decoder: a tone detector with a tracking threshold, followed by
/// dot/dash clustering that follows the sender's speed.
pub struct Decoder {
    sample_rate: u32,
    detector: ToneDetector,
    hop: f64,
    level: f32,
    noise: f32,
    peak: f32,
    peak_decay: f32,
    /// Levels from the first 100 ms, held until the noise floor can be estimated.
    warmup: Option<Vec<f32>>,
    keyed: bool,
    run: u32,
    candidate: u32,
    space_before: u32,
    dot: f64,
    dash: f64,
    pattern: String,
    word_pending: bool,
    idle_pending: bool,
}

impl Decoder {
    pub fn new(sample_rate: u32, frequency: f64, wpm_hint: f64) -> Self {
        let detector = ToneDetector::new(sample_rate, frequency);
        let hop = detector.hop_seconds(sample_rate);
        let dot = 1.2 / wpm_hint.clamp(5.0, 60.0);
        Self {
            sample_rate,
            detector,
            hop,
            level: 0.0,
            noise: 0.0,
            peak: 0.0,
            // Peak falls to half in about 0.7 s so fading signals keep their threshold.
            peak_decay: 0.5f32.powf((hop / 0.7) as f32),
            warmup: Some(Vec::with_capacity(WARMUP_HOPS)),
            keyed: false,
            run: 0,
            candidate: 0,
            space_before: 0,
            dot,
            dash: 3.0 * dot,
            pattern: String::new(),
            word_pending: false,
            idle_pending: false,
        }
    }

    pub fn set_frequency(&mut self, frequency: f64) {
        self.detector.set_frequency(self.sample_rate, frequency);
    }

    pub fn status(&self) -> Status {
        Status {
            level: self.level,
            noise: self.noise,
            peak: self.peak,
            keyed: self.keyed,
            wpm: 1.2 / self.unit(),
        }
    }

    /// Estimated dot length, combining both clusters.
    fn unit(&self) -> f64 {
        (self.dot + self.dash / 3.0) / 2.0
    }

    pub fn process(&mut self, samples: &[f32], out: &mut Vec<RxEvent>) {
        for &sample in samples {
            if let Some(level) = self.detector.push(sample) {
                self.hop(level, out);
            }
        }
    }

    /// Emit any pending character, as though the sender had gone quiet.
    pub fn finish(&mut self, out: &mut Vec<RxEvent>) {
        if let Some(levels) = self.warmup.take() {
            // Shorter than the warm-up, so there is no noise reference: decode
            // against the absolute minimum level.
            self.noise = 0.0;
            self.peak = 0.0;
            levels.into_iter().for_each(|level| self.step(level, out));
        }
        if self.keyed {
            let seconds = self.run as f64 * self.hop;
            self.mark(seconds);
            self.keyed = false;
        }
        self.flush_char(out);
    }

    /// Drop the partial character and key state (e.g. while transmitting),
    /// keeping the learned speed and noise floor.
    pub fn interrupt(&mut self, out: &mut Vec<RxEvent>) {
        self.pattern.clear();
        self.keyed = false;
        self.run = 0;
        self.candidate = 0;
        if self.idle_pending {
            self.idle_pending = false;
            self.word_pending = false;
            out.push(RxEvent::Idle);
        }
    }

    fn hop(&mut self, level: f32, out: &mut Vec<RxEvent>) {
        self.level = level;
        let Some(warmup) = &mut self.warmup else {
            return self.step(level, out);
        };
        warmup.push(level);
        if warmup.len() < WARMUP_HOPS {
            return;
        }
        // Estimate the floor from the quietest quarter, so tone already present at the
        // start does not inflate it; for Rayleigh-distributed noise the mean is 1.65
        // times the lower quartile. Then replay the held levels so nothing is lost.
        let mut levels = self.warmup.take().unwrap();
        let mut sorted = levels.clone();
        sorted.sort_by(f32::total_cmp);
        self.noise = sorted[WARMUP_HOPS / 4] * 1.65;
        self.peak = self.noise;
        for level in levels.drain(..) {
            self.step(level, out);
        }
    }

    fn step(&mut self, level: f32, out: &mut Vec<RxEvent>) {
        self.peak = level.max(self.peak * self.peak_decay).max(self.noise);
        // A symmetric average tracks the mean noise level. Excluding loud hops would
        // bias it low and let ordinary noise peaks key the decoder, so only the
        // detector's 20 ms ramp after each mark, and hops loud enough to be tone,
        // are left out.
        if !self.keyed && self.candidate == 0 && self.run >= 4 && level < self.noise * MIN_SNR {
            self.noise += (level - self.noise) * 0.05;
        }
        let span = self.peak - self.noise;
        let raw = if self.keyed {
            level > self.noise + 0.4 * span
        } else {
            self.peak > MIN_LEVEL
                && self.peak > self.noise * MIN_SNR
                && level > self.noise + 0.5 * span
        };
        if self.keyed && self.run as f64 * self.hop > CARRIER_SECONDS {
            // Adopt the new level as the noise floor and discard the element.
            self.noise = level;
            self.keyed = false;
            self.run = 0;
            self.candidate = 0;
            self.pattern.clear();
        } else if raw == self.keyed {
            self.run += self.candidate + 1;
            self.candidate = 0;
        } else {
            self.candidate += 1;
            if self.candidate >= DEBOUNCE {
                let seconds = self.run as f64 * self.hop;
                if !self.keyed {
                    self.space_before = self.run;
                    self.run = self.candidate;
                } else if seconds < GLITCH * self.dot {
                    // Too short to be an element: treat it as part of the silence.
                    self.run += self.space_before + self.candidate;
                } else {
                    self.mark(seconds);
                    self.run = self.candidate;
                }
                self.keyed = raw;
                self.candidate = 0;
            }
        }
        if !self.keyed {
            self.space(self.run as f64 * self.hop, out);
        }
    }

    fn mark(&mut self, seconds: f64) {
        const ALPHA: f64 = 0.3;
        if seconds < (self.dot + self.dash) / 2.0 {
            self.pattern.push('.');
            self.dot += (seconds - self.dot) * ALPHA;
            self.dot = self.dot.clamp(0.02, 0.24);
            self.dash = self.dash.clamp(2.0 * self.dot, 4.0 * self.dot);
        } else {
            self.pattern.push('-');
            self.dash += (seconds - self.dash) * ALPHA;
            self.dash = self.dash.clamp(0.06, 0.72);
            self.dot = self.dot.clamp(self.dash / 4.0, self.dash / 2.0);
        }
        if self.pattern.len() > MAX_PATTERN {
            self.pattern.remove(0);
        }
    }

    /// Called every silent hop with the silence so far; gaps are resolved as soon
    /// as they are long enough, without waiting for the next tone.
    fn space(&mut self, seconds: f64, out: &mut Vec<RxEvent>) {
        let unit = self.unit();
        if seconds > 2.0 * unit {
            self.flush_char(out);
        }
        if self.word_pending && seconds > 5.0 * unit {
            self.word_pending = false;
            out.push(RxEvent::WordGap);
        }
        if self.idle_pending && seconds > IDLE_SECONDS.max(20.0 * unit) {
            self.idle_pending = false;
            out.push(RxEvent::Idle);
        }
    }

    fn flush_char(&mut self, out: &mut Vec<RxEvent>) {
        if self.pattern.is_empty() {
            return;
        }
        out.push(RxEvent::Char(table::reverse(&self.pattern).unwrap_or('*')));
        self.pattern.clear();
        self.word_pending = true;
        self.idle_pending = true;
    }
}

/// Decode a complete recording into text, with word gaps as spaces and overs as newlines.
pub fn decode_all(samples: &[f32], sample_rate: u32, frequency: f64, wpm_hint: f64) -> String {
    let mut decoder = Decoder::new(sample_rate, frequency, wpm_hint);
    let mut events = Vec::new();
    decoder.process(samples, &mut events);
    decoder.finish(&mut events);
    let mut text = String::new();
    for event in events {
        match event {
            RxEvent::Char(c) => text.push(c),
            RxEvent::WordGap => text.push(' '),
            RxEvent::Idle => {
                text.truncate(text.trim_end().len());
                text.push('\n');
            }
        }
    }
    text.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        cw::{oscillator, timing},
        morse::encoder::encode,
    };
    use std::f64::consts::TAU;

    const RATE: u32 = oscillator::SAMPLE_RATE;

    /// Deterministic xorshift noise with a roughly Gaussian distribution.
    struct Noise(u64);
    impl Noise {
        fn uniform(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
        fn gaussian(&mut self) -> f64 {
            (0..12).map(|_| self.uniform()).sum::<f64>() - 6.0
        }
    }

    fn render(text: &str, wpm: f64, tone: f64) -> Vec<f32> {
        let events = timing::schedule(&encode(text).unwrap());
        oscillator::render(&events, wpm, tone, 0.2).unwrap()
    }

    fn padded(mut samples: Vec<f32>) -> Vec<f32> {
        let mut out = vec![0.0; RATE as usize / 2];
        out.append(&mut samples);
        out.extend(vec![0.0; RATE as usize / 2]);
        out
    }

    #[test]
    fn decodes_clean_signal() {
        let audio = padded(render("CQ CQ DE W8ABC K", 20.0, 700.0));
        assert_eq!(decode_all(&audio, RATE, 700.0, 20.0), "CQ CQ DE W8ABC K");
    }

    #[test]
    fn decodes_audio_that_starts_on_a_tone() {
        let audio = render("HELLO TEST", 20.0, 700.0);
        assert_eq!(decode_all(&audio, RATE, 700.0, 20.0), "HELLO TEST");
        assert_eq!(
            decode_all(&render("E", 20.0, 700.0), RATE, 700.0, 20.0),
            "E"
        );
    }

    #[test]
    fn adapts_to_other_speeds() {
        let text = "THE QUICK BROWN FOX JUMPS OVER THE LAZY DOG 0123456789";
        for wpm in [8.0, 13.0, 30.0, 40.0] {
            let audio = padded(render(text, wpm, 700.0));
            let decoded = decode_all(&audio, RATE, 700.0, 20.0);
            // The first character may be misjudged before the speed is learned.
            assert!(decoded.ends_with(&text[4..]), "{wpm} WPM: {decoded}");
        }
    }

    #[test]
    fn decodes_through_noise() {
        let mut noise = Noise(0x2545_f491_4f6c_dd1d);
        // -6.5 dB SNR across 24 kHz: noise RMS 0.3 against a 0.2-peak tone.
        let audio: Vec<f32> = padded(render("PARIS PARIS DE N0CALL", 20.0, 700.0))
            .into_iter()
            .map(|s| s + (0.3 * noise.gaussian()) as f32)
            .collect();
        assert_eq!(
            decode_all(&audio, RATE, 700.0, 20.0),
            "PARIS PARIS DE N0CALL"
        );
    }

    #[test]
    fn noise_alone_decodes_nothing() {
        let mut noise = Noise(7);
        let audio: Vec<f32> = (0..RATE * 5)
            .map(|_| (0.3 * noise.gaussian()) as f32)
            .collect();
        assert_eq!(decode_all(&audio, RATE, 700.0, 20.0), "");
    }

    #[test]
    fn decodes_uneven_hand_keying() {
        // Every element, gap, and amplitude varies randomly by up to ±25%.
        let mut noise = Noise(42);
        let mut audio = vec![0.0f32; RATE as usize / 2];
        let unit = 1.2 / 18.0;
        let mut phase = 0.0f64;
        for event in timing::schedule(&encode("HELLO OM UR RST 599 5NN").unwrap()) {
            let jitter = 1.0 + 0.5 * (noise.uniform() - 0.5);
            let count = (event.units as f64 * unit * jitter * RATE as f64) as usize;
            let gain = 0.2 * (0.75 + 0.5 * noise.uniform());
            for _ in 0..count {
                phase += TAU * 650.0 / RATE as f64;
                audio.push(if event.tone {
                    (gain * phase.sin()) as f32
                } else {
                    0.0
                });
            }
        }
        audio.extend(vec![0.0; RATE as usize / 2]);
        assert_eq!(
            decode_all(&audio, RATE, 650.0, 25.0),
            "HELLO OM UR RST 599 5NN"
        );
    }

    #[test]
    fn separates_overs_and_reports_speed() {
        let mut audio = padded(render("TEST DE A", 20.0, 700.0));
        audio.extend(vec![0.0; RATE as usize * 3]);
        audio.extend(render("QSL DE B", 20.0, 700.0));
        audio.extend(vec![0.0; RATE as usize]);
        assert_eq!(decode_all(&audio, RATE, 700.0, 20.0), "TEST DE A\nQSL DE B");

        let mut decoder = Decoder::new(RATE, 700.0, 35.0);
        decoder.process(&render("PARIS PARIS PARIS", 15.0, 700.0), &mut Vec::new());
        assert!(
            (decoder.status().wpm - 15.0).abs() < 1.5,
            "{:?}",
            decoder.status().wpm
        );
    }

    #[test]
    fn unknown_patterns_decode_as_star() {
        // Eight dots is the error prosign, which is not in the table.
        let events: Vec<_> = (0..15)
            .map(|i| timing::Event {
                tone: i % 2 == 0,
                units: 1,
            })
            .collect();
        let audio = padded(oscillator::render(&events, 20.0, 700.0, 0.2).unwrap());
        assert_eq!(decode_all(&audio, RATE, 700.0, 20.0), "*");
    }
}
