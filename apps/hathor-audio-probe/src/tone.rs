//! The signal the probe feeds the chain: a table sine, or silence.
//!
//! ## Why a table and not `sin`
//!
//! The realtime law this project defends (CONVENTION §2, and the filters crate says
//! it out loud in its own module doc) is *bounded cycles*: no transcendentals in the
//! callback, because `sin` per frame is ~20 ns (**expected**) and it multiplies by
//! frames × channels while making the block cost data-dependent. A 4096-entry table
//! is `setup()` allocation; the callback is one load and one masked add per frame.
//! The price is frequency quantisation — the tone you asked for is rounded to the
//! nearest table step — and the probe **prints the frequency it actually plays**
//! rather than the one it was asked for. That rounding is `actual_hz`, and it is the
//! difference between a measurement and a story.
//!
//! ## Why the tone at all
//!
//! Silence cannot xrun: an empty buffer is trivially fast. The M0 criterion is the
//! *chain* running on real data with the guard watching, so the pump has to have
//! something to filter. `--tone silence` exists for the human who wants to hear
//! nothing but still get the numbers.

use crate::cli::Tone;

/// Table length. Power of two so the wrap is a mask, and large enough that the
/// quantisation at 48 kHz is ~11.7 Hz (**measured** by `tone_step_and_actual_hz`).
pub const TABLE_LEN: usize = 4_096;

/// Output amplitude: −14 dBFS. Loud enough to hear that the device is alive, quiet
/// enough that a filter resonance peak cannot clip the DAC on a wrong parameter.
pub const AMPLITUDE: f32 = 0.2;

/// The frequency step one table entry represents at a given rate: `sr / TABLE_LEN`.
/// 48 kHz → 11.71875 Hz, the same window resolution the fft tests use for exact bins.
pub fn table_bin_hz(rate: u32) -> f32 {
    rate as f32 / TABLE_LEN as f32
}

/// Integer table step for a requested frequency, and the frequency that step really
/// plays. `step` is clamped to ≥ 1: a 0 Hz tone would be a frozen sample, and a probe
/// that reports "playing 440 Hz" while emitting DC would be worse than silence.
pub fn tone_step_and_actual_hz(hz: f32, rate: u32) -> (usize, f32) {
    let bin = table_bin_hz(rate);
    let raw = (hz / bin).round();
    let step = if raw < 1.0 { 1 } else { raw as usize };
    (step, step as f32 * bin)
}

/// The generator state. `fill` is the only method the callback may call.
#[derive(Debug, Clone)]
pub struct ToneGen {
    table: Vec<f32>,
    step: usize,
    index: usize,
    silent: bool,
    asked_hz: f32,
    actual_hz: f32,
}

impl ToneGen {
    /// Builds the table (LEGITIMATE ALLOCATION: setup thread).
    pub fn new(tone: Tone, rate: u32) -> Self {
        let (silent, hz) = match tone {
            Tone::Silence => (true, 0.0),
            Tone::Sine(hz) => (false, hz),
        };
        let (step, actual) = tone_step_and_actual_hz(hz, rate);
        // LEGITIMATE ALLOCATION: setup thread. Built in f64 and narrowed once: a
        // table that drifted would be a measurement error you cannot see.
        let table: Vec<f32> = (0..TABLE_LEN)
            .map(|i| {
                let a = core::f64::consts::TAU * i as f64 / TABLE_LEN as f64;
                (a.sin() * f64::from(AMPLITUDE)) as f32
            })
            .collect();
        Self {
            table,
            step,
            index: 0,
            silent,
            asked_hz: hz,
            actual_hz: actual,
        }
    }

    /// True when the probe emits digital silence.
    pub fn is_silent(&self) -> bool {
        self.silent
    }

    /// The frequency requested on the command line.
    pub fn asked_hz(&self) -> f32 {
        self.asked_hz
    }

    /// The frequency actually generated after table quantisation.
    pub fn actual_hz(&self) -> f32 {
        self.actual_hz
    }

    /// The integer table step (printed with `actual_hz` so the number is checkable).
    pub fn step(&self) -> usize {
        self.step
    }

    /// Writes `dst.len()` frames of the tone. Allocation-free, transcendental-free:
    /// one masked add and one load per frame.
    pub fn fill(&mut self, dst: &mut [f32]) {
        if self.silent {
            dst.iter_mut().for_each(|s| *s = 0.0);
            return;
        }
        let mask = TABLE_LEN - 1;
        let step = self.step;
        let mut i = self.index;
        for s in dst.iter_mut() {
            *s = self.table[i & mask];
            i += step;
        }
        self.index = i & mask;
    }

    /// Silence without changing the phase (used for the tail of a failed block).
    pub fn fill_silence(dst: &mut [f32]) {
        dst.iter_mut().for_each(|s| *s = 0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_440hz_tone_at_48k_plays_445_3125hz_because_of_the_table() {
        let (step, actual) = tone_step_and_actual_hz(440.0, 48_000);
        assert_eq!(step, 38, "440 / 11.71875 = 37.547 → 38");
        assert!(
            (actual - 445.3125).abs() < 1e-4,
            "38 bins × 11.71875 = 445.3125, got {actual}"
        );
    }

    #[test]
    fn bin_exact_frequencies_round_to_themselves() {
        // The window-resolution property: multiples of sr/TABLE_LEN are exact.
        for k in [1usize, 8, 37, 341] {
            let hz = k as f32 * table_bin_hz(48_000);
            let (step, actual) = tone_step_and_actual_hz(hz, 48_000);
            assert_eq!(step, k, "{hz} Hz must be exactly bin {k}");
            assert!(
                (actual - hz).abs() < 1e-3,
                "{hz} vs {actual}: a bin-exact frequency is not quantised"
            );
        }
    }

    #[test]
    fn sub_bin_frequencies_are_clamped_to_one_step_not_zero() {
        let (step, actual) = tone_step_and_actual_hz(1.0, 48_000);
        assert_eq!(step, 1, "a 0 step would emit DC while claiming to tone");
        assert!(actual > 0.0, "and the printed frequency must match: {actual}");
    }

    #[test]
    fn fill_walks_the_table_and_wraps_without_losing_phase() {
        // An exact bin frequency (341 bins = 4000.3125 Hz at 48k) → step 341.
        let hz = 341.0 * table_bin_hz(48_000);
        let (step, _) = tone_step_and_actual_hz(hz, 48_000);
        assert_eq!(step, 341, "a bin-exact tone has an exact integer step");

        let mut a = [0.0f32; 8];
        let mut b = [0.0f32; 8];
        let mut t = ToneGen::new(Tone::Sine(hz), 48_000);
        t.fill(&mut a);
        t.fill(&mut b);

        // Continuation: the two 8-frame fills must equal one cold 16-frame fill.
        let mut c = [0.0f32; 16];
        let mut cold = ToneGen::new(Tone::Sine(hz), 48_000);
        cold.fill(&mut c);
        assert_eq!(&a[..], &c[..8], "the first 8 frames match a cold start");
        assert_eq!(&b[..], &c[8..], "phase continues across fills, it does not restart");
    }

    #[test]
    fn the_phase_index_stays_inside_the_table_after_many_fills() {
        // 1000 × 7 frames at a step that is not a divisor of the table: the mask must
        // keep every read in range (an out-of-range index here is a panic in release).
        let hz = 445.3125; // step 38
        let mut t = ToneGen::new(Tone::Sine(hz), 48_000);
        let mut buf = [0.0f32; 7];
        for _ in 0..1_000 {
            t.fill(&mut buf);
            assert!(
                t.index < TABLE_LEN,
                "index {} escaped the table",
                t.index
            );
            assert!(buf.iter().all(|s| s.abs() <= AMPLITUDE + 1e-6), "bounded tone");
        }
    }

    #[test]
    fn the_table_starts_at_zero_and_peaks_a_quarter_in() {
        let mut t = ToneGen::new(Tone::Sine(0.0), 48_000);
        let mut one = [0.0f32; 1];
        t.fill(&mut one);
        assert!(one[0].abs() < 1e-9, "sin(0) = 0, got {}", one[0]);
        // Bin-exact 1 bin/step reaches the peak at index TABLE_LEN/4.
        let peak = t.table[TABLE_LEN / 4];
        assert!(
            (peak - AMPLITUDE).abs() < 1e-6,
            "quarter turn must be +amplitude, got {peak}"
        );
        let trough = t.table[3 * TABLE_LEN / 4];
        assert!(
            (trough + AMPLITUDE).abs() < 1e-6,
            "three-quarter turn must be −amplitude, got {trough}"
        );
    }

    #[test]
    fn silence_fills_zeros_and_never_advances_the_table() {
        let mut t = ToneGen::new(Tone::Silence, 48_000);
        assert!(t.is_silent());
        let mut buf = [1.0f32; 64];
        t.fill(&mut buf);
        assert!(buf.iter().all(|s| *s == 0.0), "silence is zeros");
        let mut buf2 = [7.0f32; 8];
        ToneGen::fill_silence(&mut buf2);
        assert!(buf2.iter().all(|s| *s == 0.0), "the failed-block tail is silence");
    }

    #[test]
    fn an_empty_fill_is_a_no_op() {
        let mut t = ToneGen::new(Tone::Sine(440.0), 48_000);
        let mut nothing: [f32; 0] = [];
        t.fill(&mut nothing);
        assert_eq!(t.step(), 38, "the step is a setup decision, not a per-call one");
    }
}
