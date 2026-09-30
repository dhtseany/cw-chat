use super::{
    detector::{Bin, ToneDetector},
    tuner::{self, Tuner},
};
use crate::morse::table;
use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RxEvent {
    /// A new over begins; sent just before its first character.
    Over(OverInfo),
    /// Corrected details for the current over, after auto tuning re-centred on it.
    OverUpdate(OverInfo),
    /// A decoded character; `*` marks an unknown pattern.
    Char(char),
    WordGap,
    /// The over ended: long silence, or the next character came from another station.
    Idle,
}

/// Who is sending, from the first character of an over.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverInfo {
    /// 0 for the first station heard, 1 for the next, and so on.
    pub station: usize,
    /// Audio pitch of the station in Hz, measured from the detector phase.
    pub pitch_hz: f64,
    /// Tone level in dBFS while keyed.
    pub level_db: f64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Status {
    /// Latest tone amplitude, noise floor, and recent peak (full-scale sine = 1.0).
    pub level: f32,
    pub noise: f32,
    pub peak: f32,
    pub keyed: bool,
    pub wpm: f64,
    /// Frequency the decoder is listening at, and whether it follows the signal.
    pub tone: f64,
    pub auto: bool,
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
/// Hops (400 ms) held to estimate the noise floor from their quietest quarter. Long
/// enough to include element gaps even when audio starts on a dash.
const WARMUP_HOPS: usize = 80;
/// Marks shorter than this fraction of a dot are noise spikes.
const GLITCH: f64 = 0.35;
/// Stations closer than this in pitch AND level are treated as the same sender.
const PITCH_TOLERANCE_HZ: f64 = 15.0;
const LEVEL_TOLERANCE_DB: f64 = 10.0;
/// Keyed hops needed in an over before a change of station can split it (100 ms).
const MIN_OVER_HOPS: f64 = 20.0;
/// Auto tuning: jump to a new tone only when it is this far from the current one;
/// closer than that, per-character centering handles it.
const RETUNE_HZ: f64 = 25.0;
/// Audio kept for replay after a jump, so the first letter at the new tone is kept.
const REPLAY_SECONDS: f64 = 0.25;

/// Pitch and level of the keyed hops in a character, an over, or a station.
#[derive(Debug, Clone, Copy, Default)]
struct Signature {
    pitch_sum: f64,
    pitch_weight: f64,
    db_sum: f64,
    hops: f64,
}

impl Signature {
    fn add(&mut self, level: f32, pitch: Option<f64>) {
        self.db_sum += 20.0 * (level.max(1e-9) as f64).log10();
        self.hops += 1.0;
        if let Some(pitch) = pitch {
            self.pitch_sum += pitch * level as f64;
            self.pitch_weight += level as f64;
        }
    }

    fn merge(&mut self, other: &Signature) {
        self.pitch_sum += other.pitch_sum;
        self.pitch_weight += other.pitch_weight;
        self.db_sum += other.db_sum;
        self.hops += other.hops;
    }

    /// Level-weighted mean pitch in Hz.
    fn pitch(&self) -> Option<f64> {
        (self.pitch_weight > 0.0).then(|| self.pitch_sum / self.pitch_weight)
    }

    fn db(&self) -> Option<f64> {
        (self.hops > 0.0).then(|| self.db_sum / self.hops)
    }

    /// Normalized distance; 1.0 is the edge of "same station". Pitch only counts
    /// when both sides have a measurement.
    fn distance(&self, other: &Signature) -> f64 {
        let pitch = match (self.pitch(), other.pitch()) {
            (Some(a), Some(b)) => ((a - b) / PITCH_TOLERANCE_HZ).powi(2),
            _ => 0.0,
        };
        let level = match (self.db(), other.db()) {
            (Some(a), Some(b)) => ((a - b) / LEVEL_TOLERANCE_DB).powi(2),
            _ => 0.0,
        };
        (pitch + level).sqrt()
    }

    /// Blend a finished over into a station's remembered signature, as averages.
    fn blend(&mut self, over: &Signature) {
        let mut next = Signature::default();
        for (source, weight) in [(*self, 0.5), (*over, 0.5)] {
            if let Some(pitch) = source.pitch() {
                next.pitch_sum += pitch * weight;
                next.pitch_weight += weight;
            }
            if let Some(db) = source.db() {
                next.db_sum += db * weight;
                next.hops += weight;
            }
        }
        *self = next;
    }
}

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
    peak_decay_fast: f32,
    /// Bins from the first 400 ms, held until the noise floor can be estimated.
    warmup: Option<Vec<Bin>>,
    /// Previous bin, when it was part of a steady keyed tone (for pitch).
    previous: Option<Bin>,
    char_signature: Signature,
    over_signature: Signature,
    over_open: bool,
    /// The current character began after at least a word gap of silence.
    char_after_gap: bool,
    stations: Vec<Signature>,
    station: Option<usize>,
    /// The current station was first heard in this over (so it can be replaced).
    station_is_new: bool,
    /// Re-identify the sender on the next character (auto tuning moved mid-over).
    rematch: bool,
    /// Automatic tuning: the band-wide tuner and recent audio for replay.
    tuner: Option<Tuner>,
    replay: VecDeque<f32>,
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
            // After a word gap the other station may answer, possibly much weaker,
            // so the peak lets go quickly (half in 0.1 s).
            peak_decay_fast: 0.5f32.powf((hop / 0.1) as f32),
            warmup: Some(Vec::with_capacity(WARMUP_HOPS)),
            previous: None,
            char_signature: Signature::default(),
            over_signature: Signature::default(),
            over_open: false,
            char_after_gap: false,
            stations: Vec::new(),
            station: None,
            station_is_new: false,
            rematch: false,
            tuner: None,
            replay: VecDeque::new(),
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
        self.previous = None;
    }

    /// Follow the strongest keyed tone in 300–1200 Hz instead of a fixed frequency.
    pub fn set_auto(&mut self, auto: bool) {
        if auto == self.tuner.is_some() {
            return;
        }
        self.tuner = auto.then(|| Tuner::new(self.sample_rate));
        self.replay.clear();
    }

    pub fn with_auto(mut self, auto: bool) -> Self {
        self.set_auto(auto);
        self
    }

    pub fn status(&self) -> Status {
        Status {
            level: self.level,
            noise: self.noise,
            peak: self.peak,
            keyed: self.keyed,
            wpm: 1.2 / self.unit(),
            tone: self.detector.frequency(),
            auto: self.tuner.is_some(),
        }
    }

    /// Estimated dot length, combining both clusters.
    fn unit(&self) -> f64 {
        (self.dot + self.dash / 3.0) / 2.0
    }

    pub fn process(&mut self, samples: &[f32], out: &mut Vec<RxEvent>) {
        let replay_len = (self.sample_rate as f64 * REPLAY_SECONDS) as usize;
        for &sample in samples {
            if let Some(tuner) = &mut self.tuner {
                if self.replay.len() == replay_len {
                    self.replay.pop_front();
                }
                self.replay.push_back(sample);
                // A retune replays the recent audio, this sample included.
                if let Some(tone) = tuner.push(sample)
                    && self.retune(tone, out)
                {
                    continue;
                }
            }
            if let Some(bin) = self.detector.push(sample) {
                self.hop(bin, out);
            }
        }
    }

    /// Jump to a tone the tuner found, if it is far from the current one. Only
    /// between characters, and mid-over only when nothing is keying at the current
    /// tone (we are hearing the sender off-centre), so a louder station cannot
    /// steal the lock from one that is still sending.
    fn retune(&mut self, tone: f64, out: &mut Vec<RxEvent>) -> bool {
        let current = self.detector.frequency();
        let silence = if self.keyed {
            0.0
        } else {
            self.run as f64 * self.hop
        };
        let paused = !self.over_open || silence > 5.0 * self.unit();
        let current_active = self
            .tuner
            .as_ref()
            .is_some_and(|tuner| tuner.keyed_near(current));
        if self.warmup.is_some()
            || (tone - current).abs() < RETUNE_HZ
            || self.keyed
            || self.candidate > 0
            || !self.pattern.is_empty()
            || (!paused && current_active)
        {
            return false;
        }
        self.detector = ToneDetector::new(self.sample_rate, tone);
        self.previous = None;
        self.char_signature = Signature::default();
        if !paused {
            // Same sender, now centred: restart the level reference so the jump in
            // level is not taken for a different station, and re-identify them.
            self.over_signature = Signature::default();
            self.rematch = true;
        }
        // Replay recent audio at the new tone, so a first letter that was too far off
        // to hear is kept. Only over silence, rewinding the run it was counted in.
        if silence < REPLAY_SECONDS {
            return false;
        }
        let hop_samples = (self.hop * self.sample_rate as f64).round() as usize;
        self.run = self
            .run
            .saturating_sub((self.replay.len() / hop_samples.max(1)) as u32);
        let replay: Vec<f32> = self.replay.iter().copied().collect();
        for sample in replay {
            if let Some(bin) = self.detector.push(sample) {
                self.hop(bin, out);
            }
        }
        true
    }

    /// Emit any pending character, as though the sender had gone quiet.
    pub fn finish(&mut self, out: &mut Vec<RxEvent>) {
        if let Some(levels) = self.warmup.take() {
            // Shorter than the warm-up, so there is no noise reference: decode
            // against the absolute minimum level.
            self.noise = 0.0;
            self.peak = 0.0;
            levels.into_iter().for_each(|bin| self.step(bin, out));
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
        self.char_signature = Signature::default();
        self.previous = None;
        if self.idle_pending {
            self.idle_pending = false;
            self.word_pending = false;
            self.end_over();
            out.push(RxEvent::Idle);
        }
    }

    fn hop(&mut self, bin: Bin, out: &mut Vec<RxEvent>) {
        self.level = bin.level;
        let Some(warmup) = &mut self.warmup else {
            return self.step(bin, out);
        };
        warmup.push(bin);
        if warmup.len() < WARMUP_HOPS {
            return;
        }
        // Estimate the floor from the quietest quarter, so tone already present at the
        // start does not inflate it; for Rayleigh-distributed noise the mean is 1.65
        // times the lower quartile. Then replay the held levels so nothing is lost.
        let bins = self.warmup.take().unwrap();
        let mut sorted: Vec<f32> = bins.iter().map(|bin| bin.level).collect();
        sorted.sort_by(f32::total_cmp);
        self.noise = sorted[WARMUP_HOPS / 4] * 1.65;
        self.peak = self.noise;
        for bin in bins {
            self.step(bin, out);
        }
    }

    fn step(&mut self, bin: Bin, out: &mut Vec<RxEvent>) {
        let level = bin.level;
        let paused = !self.keyed && self.run as f64 * self.hop > 5.0 * self.unit();
        let decay = if paused {
            self.peak_decay_fast
        } else {
            self.peak_decay
        };
        self.peak = level.max(self.peak * decay).max(self.noise);
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
                    if self.pattern.is_empty() {
                        self.char_after_gap = seconds > 5.0 * self.unit();
                    }
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
        // Pitch and level come from steady tone: keyed, confirmed, and well above
        // the threshold, so the detector's ramps at each edge are left out.
        let steady = self.keyed && self.candidate == 0 && level > self.noise + 0.5 * span;
        if steady {
            let pitch = self
                .previous
                .map(|previous| self.detector.frequency() + self.detector.offset_hz(previous, bin));
            self.char_signature.add(level, pitch);
            self.previous = Some(bin);
        } else {
            self.previous = None;
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
            self.end_over();
            out.push(RxEvent::Idle);
        }
    }

    fn flush_char(&mut self, out: &mut Vec<RxEvent>) {
        if self.pattern.is_empty() {
            return;
        }
        let signature = std::mem::take(&mut self.char_signature);
        // Auto tuning keeps the detector centred on the sender, a character at a time.
        if self.tuner.is_some()
            && let Some(pitch) = signature.pitch()
        {
            let error = pitch - self.detector.frequency();
            if (3.0..100.0).contains(&error.abs()) {
                let tone =
                    (self.detector.frequency() + 0.7 * error).clamp(tuner::MIN_HZ, tuner::MAX_HZ);
                self.set_frequency(tone);
            }
        }
        if self.over_open && self.rematch && signature.hops >= 3.0 {
            // A station created from off-centre readings is replaced, not kept.
            self.rematch = false;
            if self.station_is_new && self.station == Some(self.stations.len() - 1) {
                self.stations.pop();
            }
            out.push(RxEvent::OverUpdate(self.identify(&signature)));
        }
        // After a pause, a character that does not match the over so far is the other
        // station answering: end this over and start a new one.
        if self.over_open
            && self.char_after_gap
            && self.over_signature.hops >= MIN_OVER_HOPS
            && signature.hops >= 3.0
            && signature.distance(&self.over_signature) > 1.0
        {
            self.end_over();
            self.word_pending = false;
            out.push(RxEvent::Idle);
        }
        if !self.over_open {
            out.push(RxEvent::Over(self.start_over(&signature)));
        }
        self.over_signature.merge(&signature);
        out.push(RxEvent::Char(table::reverse(&self.pattern).unwrap_or('*')));
        self.pattern.clear();
        self.word_pending = true;
        self.idle_pending = true;
    }

    fn start_over(&mut self, first: &Signature) -> OverInfo {
        self.over_open = true;
        self.rematch = false;
        self.over_signature = Signature::default();
        self.identify(first)
    }

    /// Match a character to a known station, or add a new one.
    fn identify(&mut self, first: &Signature) -> OverInfo {
        let nearest = self
            .stations
            .iter()
            .enumerate()
            .map(|(index, station)| (index, station.distance(first)))
            .filter(|&(_, distance)| distance <= 1.0)
            .min_by(|a, b| a.1.total_cmp(&b.1));
        self.station_is_new = nearest.is_none();
        let station = match nearest {
            Some((index, _)) => index,
            None => {
                self.stations.push(*first);
                self.stations.len() - 1
            }
        };
        self.station = Some(station);
        OverInfo {
            station,
            pitch_hz: first.pitch().unwrap_or(self.detector.frequency()),
            level_db: first.db().unwrap_or(-120.0),
        }
    }

    /// Close the over and fold its signature into its station's.
    fn end_over(&mut self) {
        if !self.over_open {
            return;
        }
        self.over_open = false;
        if let Some(station) = self.station {
            self.stations[station].blend(&self.over_signature);
        }
    }
}

/// Decode a complete recording into text, with word gaps as spaces and overs as newlines.
pub fn decode_all(samples: &[f32], sample_rate: u32, frequency: f64, wpm_hint: f64) -> String {
    decode_all_with(samples, sample_rate, frequency, wpm_hint, false)
}

/// As `decode_all`, optionally following the signal's tone from `frequency`.
pub fn decode_all_with(
    samples: &[f32],
    sample_rate: u32,
    frequency: f64,
    wpm_hint: f64,
    auto: bool,
) -> String {
    let mut decoder = Decoder::new(sample_rate, frequency, wpm_hint).with_auto(auto);
    let mut events = Vec::new();
    decoder.process(samples, &mut events);
    decoder.finish(&mut events);
    let mut text = String::new();
    for event in events {
        match event {
            RxEvent::Over(_) | RxEvent::OverUpdate(_) => {}
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
        // Starting on a dash leaves no gap in the first 100 ms.
        let audio = render("CQ DE W8ABC K", 20.0, 700.0);
        assert_eq!(decode_all(&audio, RATE, 700.0, 20.0), "CQ DE W8ABC K");
        let audio = render("TTT MMM", 13.0, 700.0);
        assert_eq!(decode_all(&audio, RATE, 700.0, 20.0), "TTT MMM");
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

    fn over(text: &str, tone: f64, gain: f64) -> Vec<f32> {
        let events = timing::schedule(&encode(text).unwrap());
        oscillator::render(&events, 20.0, tone, gain).unwrap()
    }

    /// Three overs separated by 0.8 s changeovers: longer than a word gap, much
    /// shorter than the 2.5 s silence that ends an over on its own.
    fn qso(b_tone: f64, b_gain: f64) -> (String, Vec<OverInfo>) {
        let changeover = vec![0.0; RATE as usize * 8 / 10];
        let mut audio = vec![0.0; RATE as usize / 2];
        audio.extend(over("CQ CQ DE A K", 700.0, 0.2));
        audio.extend(&changeover);
        audio.extend(over("A DE B GM K", b_tone, b_gain));
        audio.extend(&changeover);
        audio.extend(over("B DE A TU", 700.0, 0.2));
        audio.extend(vec![0.0; RATE as usize / 2]);
        let mut decoder = Decoder::new(RATE, 700.0, 20.0);
        let mut events = Vec::new();
        decoder.process(&audio, &mut events);
        decoder.finish(&mut events);
        (decode_all(&audio, RATE, 700.0, 20.0), final_overs(&events))
    }

    /// Each over's details, with any later correction applied.
    fn final_overs(events: &[RxEvent]) -> Vec<OverInfo> {
        let mut overs = Vec::new();
        for event in events {
            match event {
                RxEvent::Over(info) => overs.push(*info),
                RxEvent::OverUpdate(info) => *overs.last_mut().unwrap() = *info,
                _ => {}
            }
        }
        overs
    }

    #[test]
    fn splits_overs_by_pitch_and_recognizes_stations() {
        let (text, overs) = qso(740.0, 0.2);
        assert_eq!(text, "CQ CQ DE A K\nA DE B GM K\nB DE A TU");
        let stations: Vec<_> = overs.iter().map(|o| o.station).collect();
        assert_eq!(stations, [0, 1, 0]);
        assert!((overs[0].pitch_hz - 700.0).abs() < 3.0, "{overs:?}");
        assert!((overs[1].pitch_hz - 740.0).abs() < 3.0, "{overs:?}");
    }

    #[test]
    fn splits_overs_by_level() {
        // Same pitch; B is 16 dB weaker.
        let (text, overs) = qso(700.0, 0.2 / 6.3);
        assert_eq!(text, "CQ CQ DE A K\nA DE B GM K\nB DE A TU");
        let stations: Vec<_> = overs.iter().map(|o| o.station).collect();
        assert_eq!(stations, [0, 1, 0]);
        assert!(
            (overs[0].level_db - overs[1].level_db - 16.0).abs() < 2.0,
            "{overs:?}"
        );
    }

    #[test]
    fn same_station_after_pauses_stays_one_over() {
        let (text, overs) = qso(700.0, 0.2);
        assert_eq!(
            text,
            "CQ CQ DE A KA DE B GM KB DE A TU"
                .replace("KA", "K A")
                .replace("KB", "K B")
        );
        assert_eq!(overs.len(), 1);
    }

    #[test]
    fn auto_tuning_finds_the_signal_from_a_wrong_start() {
        let text = "CQ CQ DE W8ABC K";
        for (start, actual) in [(700.0, 500.0), (500.0, 1000.0), (900.0, 350.0)] {
            let audio = padded(render(text, 20.0, actual));
            assert_eq!(
                decode_all(&audio, RATE, start, 20.0),
                "",
                "fixed {start} Hz"
            );
            let decoded = decode_all_with(&audio, RATE, start, 20.0, true);
            assert_eq!(decoded, text, "auto from {start} Hz to {actual} Hz");
        }
    }

    #[test]
    fn auto_tuning_centres_on_a_drifted_signal() {
        let mut decoder = Decoder::new(RATE, 500.0, 20.0).with_auto(true);
        decoder.process(&padded(render("PARIS PARIS", 20.0, 540.0)), &mut Vec::new());
        let tone = decoder.status().tone;
        assert!((tone - 540.0).abs() < 4.0, "{tone}");
    }

    #[test]
    fn auto_tuning_follows_each_station() {
        let changeover = vec![0.0; RATE as usize * 8 / 10];
        let mut audio = vec![0.0; RATE as usize / 2];
        audio.extend(over("CQ CQ DE A K", 550.0, 0.2));
        audio.extend(&changeover);
        audio.extend(over("A DE B GM K", 850.0, 0.2));
        audio.extend(&changeover);
        audio.extend(over("B DE A TU", 550.0, 0.2));
        audio.extend(vec![0.0; RATE as usize / 2]);
        let mut decoder = Decoder::new(RATE, 700.0, 20.0).with_auto(true);
        let mut events = Vec::new();
        decoder.process(&audio, &mut events);
        decoder.finish(&mut events);
        let overs = final_overs(&events);
        assert_eq!(
            decode_all_with(&audio, RATE, 700.0, 20.0, true),
            "CQ CQ DE A K\nA DE B GM K\nB DE A TU"
        );
        let stations: Vec<_> = overs.iter().map(|o| o.station).collect();
        assert_eq!(stations, [0, 1, 0], "{overs:?}");
        assert!((overs[1].pitch_hz - 850.0).abs() < 5.0, "{overs:?}");
    }

    #[test]
    fn auto_tuning_in_noise() {
        let mut noise = Noise(99);
        let quiet: Vec<f32> = (0..RATE * 5)
            .map(|_| (0.3 * noise.gaussian()) as f32)
            .collect();
        assert_eq!(decode_all_with(&quiet, RATE, 700.0, 20.0, true), "");
        let audio: Vec<f32> = padded(render("PARIS PARIS DE N0CALL", 20.0, 620.0))
            .into_iter()
            .map(|s| s + (0.2 * noise.gaussian()) as f32)
            .collect();
        assert_eq!(
            decode_all_with(&audio, RATE, 800.0, 20.0, true),
            "PARIS PARIS DE N0CALL"
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
