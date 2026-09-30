use super::timing::Event;
pub const SAMPLE_RATE: u32 = 48_000;

/// Silence of `units` dot lengths at `wpm`, e.g. a trailing word gap (7) between messages.
pub fn gap(units: u32, wpm: f64) -> Vec<f32> {
    vec![0.0; (units as f64 * SAMPLE_RATE as f64 * 1.2 / wpm).round() as usize]
}

/// Edge ramp for CW audio: 5 ms, enough to keep the signal free of key clicks.
pub const AUDIO_RAMP_SECONDS: f64 = 0.005;
/// Edge ramp for a keying tone: 1 ms, so an audio-to-key interface's rectifier
/// switches promptly (and equally late on both edges); the radio shapes its own CW.
pub const KEY_RAMP_SECONDS: f64 = 0.001;

/// Render CW audio with the 5 ms edge ramp.
pub fn render(events: &[Event], wpm: f64, frequency: f64, gain: f64) -> Result<Vec<f32>, String> {
    render_with_ramp(events, wpm, frequency, gain, AUDIO_RAMP_SECONDS)
}

/// Render with cumulative rounding so fractional dot lengths do not accumulate drift.
/// A raised-cosine envelope of `ramp_seconds` at each edge suppresses clicks without
/// changing timing.
pub fn render_with_ramp(
    events: &[Event],
    wpm: f64,
    frequency: f64,
    gain: f64,
    ramp_seconds: f64,
) -> Result<Vec<f32>, String> {
    if !wpm.is_finite() || !(1.0..=100.0).contains(&wpm) {
        return Err("WPM must be between 1 and 100".into());
    }
    if !frequency.is_finite() || !(20.0..=20_000.0).contains(&frequency) {
        return Err("Tone must be between 20 and 20000 Hz".into());
    }
    if !gain.is_finite() || !(0.0..=1.0).contains(&gain) {
        return Err("Gain must be between 0 and 1".into());
    }
    let samples_per_unit = SAMPLE_RATE as f64 * 1.2 / wpm;
    let total_units: u64 = events.iter().map(|e| e.units as u64).sum();
    let total = (total_units as f64 * samples_per_unit).round() as usize;
    if total > SAMPLE_RATE as usize * 600 {
        return Err("Message exceeds the 10 minute transmission limit".into());
    }
    let mut samples = Vec::with_capacity(total);
    let mut units = 0u64;
    for event in events {
        units += event.units as u64;
        let end = (units as f64 * samples_per_unit).round() as usize;
        let count = end - samples.len();
        let ramp = ((SAMPLE_RATE as f64 * ramp_seconds) as usize).min(count / 2);
        for i in 0..count {
            let value = if event.tone {
                let edge = i.min(count - 1 - i);
                let envelope = if edge < ramp {
                    0.5 - 0.5 * (std::f64::consts::PI * edge as f64 / ramp as f64).cos()
                } else {
                    1.0
                };
                (std::f64::consts::TAU * frequency * i as f64 / SAMPLE_RATE as f64).sin()
                    * gain
                    * envelope
            } else {
                0.0
            };
            samples.push(value as f32);
        }
    }
    Ok(samples)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{cw::timing::schedule, morse::encoder::encode};
    #[test]
    fn audio_duration_gaps_and_envelope() {
        let audio = render(&schedule(&encode("A").unwrap()), 20.0, 700.0, 0.2).unwrap();
        assert_eq!(audio.len(), 14400);
        assert!(audio[2880..5760].iter().all(|&x| x == 0.0));
        assert_eq!(audio[0], 0.0);
        assert_eq!(*audio.last().unwrap(), 0.0);
        assert!(audio.iter().all(|x| x.is_finite() && x.abs() <= 0.2));
        let crossings = audio[240..2640]
            .windows(2)
            .filter(|w| w[0] <= 0.0 && w[1] > 0.0)
            .count();
        assert!((34..=36).contains(&crossings));
    }
    #[test]
    fn fractional_timing_and_validation() {
        let events = schedule(&encode("PARIS PARIS").unwrap());
        assert_eq!(
            render(&events, 23.0, 700.0, 0.2).unwrap().len(),
            (93.0_f64 * 57600.0 / 23.0).round() as usize
        );
        assert!(render(&events, 0.0, 700.0, 0.2).is_err());
        assert!(render(&events, 20.0, f64::NAN, 0.2).is_err());
    }
    #[test]
    fn keying_tone_has_fast_edges_and_the_same_timing() {
        let events = schedule(&encode("PARIS").unwrap());
        let audio = render(&events, 20.0, 700.0, 0.2).unwrap();
        let key = render_with_ramp(&events, 20.0, 1600.0, 0.8, KEY_RAMP_SECONDS).unwrap();
        assert_eq!(key.len(), audio.len());
        // The first dot (2880 samples) is at full level within 1 ms (48 samples)...
        let peak = |s: &[f32]| s.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(peak(&key[48..96]) > 0.79);
        // ...and silent in exactly the same places as the audio.
        let silent = |s: &[f32]| s.iter().map(|x| *x == 0.0).collect::<Vec<_>>();
        let (a, k) = (silent(&audio), silent(&key));
        let differ = a.iter().zip(&k).filter(|(a, k)| a != k).count();
        // Only inside the ramps, where one envelope is still zero and the other is not.
        assert!(differ < 20 * 2 * 240, "{differ}");
        let crossings = key[48..2832]
            .windows(2)
            .filter(|w| w[0] <= 0.0 && w[1] > 0.0)
            .count();
        assert!((92..=94).contains(&crossings), "{crossings}");
    }
}
