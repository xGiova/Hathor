//! DSP proofs for the analyzer, driven through the REAL host path:
//! a fabricated `clap_process` wrapped by `ProcessView::from_sys` (public with
//! documented safety since task-5) straight into `Plugin::process`. There is
//! exactly ONE audio path under test — the one a host hits — and the spectrum
//! oracle below stays independent of it (direct f64 Hann DFT, log-axis spread
//! and cross-tier max re-derived here, the pattern `hathor-dsp-fft`'s tests
//! use: never our FFT, never our mapping tables — two copies of a wrong
//! constant share bugs, an independent sum does not).

use hathor_analyzer::AnalyzerPlugin;
use hathor_clap::sys::audio_buffer::clap_audio_buffer;
use hathor_clap::sys::process::clap_process;
use hathor_clap::{Plugin, ProcessStatus, ProcessView};

const SR: f64 = 48_000.0;
const BINS: usize = 1024; // DEFAULT_BINS, re-stated: the oracle must not import the bank.
const F_TONE: f64 = 984.375; // bin 21 of the 1024 tier at 48 kHz, exactly
const AXIS_LO: f64 = 20.0;
const AXIS_HI: f64 = 24_000.0;

/// Drive one block through the real adapter. `in_ptrs`/`out_ptrs` are rows
/// (1 or 2 channels each); pass the same row into both lists to simulate the
/// in-place pair a host performs.
fn drive(
    p: &mut AnalyzerPlugin,
    in_ptrs: &mut [*mut f32],
    out_ptrs: &mut [*mut f32],
    frames: u32,
) -> ProcessStatus {
    let in_buf = clap_audio_buffer {
        data32: in_ptrs.as_mut_ptr(),
        data64: core::ptr::null_mut(),
        channel_count: in_ptrs.len() as u32,
        latency: 0,
        constant_mask: 0,
    };
    let mut out_buf = clap_audio_buffer {
        data32: out_ptrs.as_mut_ptr(),
        data64: core::ptr::null_mut(),
        channel_count: out_ptrs.len() as u32,
        latency: 0,
        constant_mask: 0,
    };
    let proc = clap_process {
        steady_time: 0,
        frames_count: frames,
        transport: core::ptr::null(),
        audio_inputs: &raw const in_buf,
        audio_outputs: &raw mut out_buf,
        audio_inputs_count: 1,
        audio_outputs_count: 1,
        in_events: core::ptr::null(),
        out_events: core::ptr::null(),
    };
    // SAFETY: this test IS the host here, and it vouches for exactly what the
    // CLAP contract vouches for: the rows back `frames` f32 samples, the
    // arrays match the declared channel counts, everything lives until
    // `drive` returns.
    let mut view = unsafe { ProcessView::from_sys(&proc, SR) };
    p.process(&mut view)
}

fn log_position(f: f64, bins: usize) -> f64 {
    (f / AXIS_LO).ln() / (AXIS_HI / AXIS_LO).ln() * (bins - 1) as f64
}

fn hann(j: usize, n: usize) -> f64 {
    0.5 - 0.5 * (std::f64::consts::TAU * j as f64 / n as f64).cos()
}

/// |X[k]| of the Hann-windowed frame, summed directly in f64 (O(n) per bin).
fn direct_hann_bin(frame: &[f32], n: usize, k: usize) -> f64 {
    let mut re = 0.0f64;
    let mut im = 0.0f64;
    for (j, &x) in frame.iter().enumerate() {
        let angle = -std::f64::consts::TAU * k as f64 * j as f64 / n as f64;
        let (s, c) = angle.sin_cos();
        let y = x as f64 * hann(j, n);
        re += y * c;
        im += y * s;
    }
    re.hypot(im)
}

/// Normalized power predicted for shared log bin `i` by ONE tier, given the
/// stream that ends exactly at the tier's newest full frame.
fn expected_tier_bin(stream: &[f32], tier_n: usize, i: usize) -> f64 {
    let norm = (4.0 / tier_n as f64).powi(2);
    let frame = &stream[stream.len() - tier_n..];
    let mut total = 0.0f64;
    for k in 1..=tier_n / 2 {
        let f = k as f64 * SR / tier_n as f64;
        if !(AXIS_LO..=AXIS_HI).contains(&f) {
            continue;
        }
        let p = log_position(f, BINS).clamp(0.0, (BINS - 1) as f64);
        let i0 = p as usize;
        let frac = p - i0 as f64;
        let (wi0, wi1) = if i0 + 1 < BINS && frac > 0.0 {
            (1.0 - frac, frac)
        } else {
            (1.0, 0.0)
        };
        let w = if i0 == i {
            wi0
        } else if i0 + 1 == i {
            wi1
        } else {
            continue;
        };
        let x = direct_hann_bin(frame, tier_n, k);
        total += x * x * norm * w;
    }
    total
}

fn tone_frame(base: usize) -> [f32; 64] {
    core::array::from_fn(|i| {
        (std::f64::consts::TAU * F_TONE * (base + i) as f64 / SR).sin() as f32 * 0.5
    })
}

/// Deterministic noise (the house Lcg: multiply-add-1013904223, 24-bit
/// extraction — same generator as every Hathor numeric test).
struct Lcg(u32);

impl Lcg {
    fn next_f32(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        ((self.0 >> 8) as f32 / 8_388_608.0) - 1.0
    }
}

/// Bypassed smoothing so every read is a deterministic function of the frames.
fn activated_bypass() -> AnalyzerPlugin {
    let mut p = AnalyzerPlugin::instantiate();
    for id in 1..5 {
        p.set_param_value(id, 0.0);
    }
    for id in 5..9 {
        p.set_param_value(id, 0.0);
    }
    assert!(p.activate(SR, 64, 64));
    p
}

#[test]
fn a_known_sine_lights_the_bin_the_oracle_predicts() {
    let mut p = activated_bypass();
    let mut stream: Vec<f32> = Vec::with_capacity(24 * 64);
    for b in 0..24 {
        let mut in0 = tone_frame(b * 64);
        let mut in1 = in0; // identical tone both ears; 0.5*(x+x) is x bit-exact
        let keep = in0;
        let (mut o0, mut o1) = ([0.0f32; 64], [0.0f32; 64]);
        let mut in_ptrs = [in0.as_mut_ptr(), in1.as_mut_ptr()];
        let mut out_ptrs = [o0.as_mut_ptr(), o1.as_mut_ptr()];
        assert!(matches!(
            drive(&mut p, &mut in_ptrs, &mut out_ptrs, 64),
            ProcessStatus::Continue
        ));
        assert_eq!(o0, keep, "pass-through bit-exact even here");
        assert_eq!(o1, keep);
        stream.extend_from_slice(&keep);
    }
    p.render_db();
    let db = p.db();
    assert_eq!(db.len(), BINS);

    // Only the 256 and 1024 tiers have triggered (4096 needs 4096 frames);
    // the cross-tier max of their oracle predictions is the expectation.
    let i0 = log_position(F_TONE, BINS) as usize;
    let mut best_i = i0;
    let mut best_pred = f64::MIN;
    for i in i0.saturating_sub(2)..=(i0 + 2).min(BINS - 1) {
        let pred = expected_tier_bin(&stream, 1024, i).max(expected_tier_bin(&stream, 256, i));
        if pred > best_pred {
            best_pred = pred;
            best_i = i;
        }
    }
    let measured_dom = db
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.total_cmp(b))
        .map(|(i, _)| i);
    assert_eq!(
        measured_dom,
        Some(best_i),
        "the dominant log bin is the predicted one"
    );
    let pred_db = 10.0 * best_pred.log10();
    let meas_db = db[best_i] as f64;
    assert!(
        (meas_db - pred_db).abs() < 0.3,
        "bin {best_i}: measured {meas_db:.3} dB vs oracle {pred_db:.3} dB"
    );
    // The bank's full-scale convention: a 0.5-amplitude sine reads near -6 dBFS.
    assert!(
        meas_db < -5.0 && meas_db > -7.5,
        "level convention sane: {meas_db} dB"
    );
}

#[test]
fn silence_sits_exactly_at_the_floor_param() {
    let mut p = AnalyzerPlugin::instantiate();
    p.set_param_value(0, -40.0); // Floor
    assert!(p.activate(SR, 64, 64));
    for _ in 0..24 {
        let (mut in0, mut in1) = ([0.0f32; 64], [0.0f32; 64]);
        let (mut o0, mut o1) = ([-3.0f32; 64], [-3.0f32; 64]);
        let mut in_ptrs = [in0.as_mut_ptr(), in1.as_mut_ptr()];
        let mut out_ptrs = [o0.as_mut_ptr(), o1.as_mut_ptr()];
        assert!(matches!(
            drive(&mut p, &mut in_ptrs, &mut out_ptrs, 64),
            ProcessStatus::Continue
        ));
    }
    p.render_db();
    assert!(
        p.db().iter().all(|&d| d == -40.0),
        "every bin at the -40 dB floor, exactly (the param reached the bank)"
    );
}

#[test]
fn downmix_is_the_average_not_a_channel_pick() {
    // L = +s, R = -s cancel: if the plugin (wrongly) fed only L, the bank
    // would light the tone; correct 0.5*(L+R) math keeps it silent.
    let mut p = activated_bypass();
    for b in 0..24 {
        let mut in0 = tone_frame(b * 64);
        let mut in1: [f32; 64] = core::array::from_fn(|i| -in0[i]);
        let (mut o0, mut o1) = ([0.0f32; 64], [0.0f32; 64]);
        let mut in_ptrs = [in0.as_mut_ptr(), in1.as_mut_ptr()];
        let mut out_ptrs = [o0.as_mut_ptr(), o1.as_mut_ptr()];
        assert!(matches!(
            drive(&mut p, &mut in_ptrs, &mut out_ptrs, 64),
            ProcessStatus::Continue
        ));
    }
    assert!(
        p.levels().iter().all(|&x| x == 0.0),
        "perfectly cancelled input leaves every linear level at zero"
    );
}

#[test]
fn pass_through_keeps_distinct_channels_bit_exact() {
    let mut p = activated_bypass();
    let mut rng = Lcg(0x5eed);
    let bytes = |a: &[f32; 64]| -> Vec<u8> {
        let mut v = Vec::with_capacity(256);
        for x in a {
            v.extend_from_slice(&x.to_ne_bytes());
        }
        v
    };
    for _ in 0..40 {
        let mut in0 = [0f32; 64];
        let mut in1 = [0f32; 64];
        for i in 0..64 {
            in0[i] = rng.next_f32();
            in1[i] = rng.next_f32();
        }
        let keep0 = bytes(&in0);
        let keep1 = bytes(&in1);
        let (mut o0, mut o1) = ([9.0f32; 64], [-9.0f32; 64]);
        let mut in_ptrs = [in0.as_mut_ptr(), in1.as_mut_ptr()];
        let mut out_ptrs = [o0.as_mut_ptr(), o1.as_mut_ptr()];
        assert!(matches!(
            drive(&mut p, &mut in_ptrs, &mut out_ptrs, 64),
            ProcessStatus::Continue
        ));
        // Exact bit equality over raw bytes: an identity, not a comparison.
        assert_eq!(bytes(&o0), keep0, "channel 0 identity over random blocks");
        assert_eq!(bytes(&o1), keep1, "channel 1 identity over random blocks");
    }
    // And the bank actually heard something (else the above is vacuous):
    assert!(p.levels().iter().any(|&x| x > 0.0), "noise drove the bank");
}

#[test]
fn in_place_mode_skips_the_copy_but_still_analyzes() {
    let mut p = activated_bypass();
    for b in 0..24 {
        let mut in0 = tone_frame(b * 64);
        let mut in1 = in0;
        let keep = in0;
        // The host hands the SAME rows to in and out (the advertised pair):
        // the adapter must detect the alias and refuse to double-write.
        let mut in_ptrs = [in0.as_mut_ptr(), in1.as_mut_ptr()];
        let mut out_ptrs = [in0.as_mut_ptr(), in1.as_mut_ptr()];
        assert!(matches!(
            drive(&mut p, &mut in_ptrs, &mut out_ptrs, 64),
            ProcessStatus::Continue
        ));
        assert_eq!(
            in0, keep,
            "in-place rows untouched: pass-through holds trivially"
        );
    }
    assert!(
        p.levels().iter().any(|&x| x > 0.0),
        "bank fed while in-place"
    );
}

#[test]
fn refuses_when_not_active_oversized_or_broken() {
    let mut p = AnalyzerPlugin::instantiate();
    let mut in0 = [0.5f32; 64];
    let mut o0 = [0.0f32; 64];
    let mut in_ptrs = [in0.as_mut_ptr()];
    let mut out_ptrs = [o0.as_mut_ptr()];
    assert!(
        matches!(
            drive(&mut p, &mut in_ptrs, &mut out_ptrs, 64),
            ProcessStatus::Error
        ),
        "no activate yet"
    );
    assert!(p.activate(SR, 64, 64));
    // 65 frames > the 64-frame scratch: Error, outputs untouched.
    let mut big = [0.5f32; 65];
    let mut bo = [7.0f32; 65];
    let mut big_in = [big.as_mut_ptr()];
    let mut big_out = [bo.as_mut_ptr()];
    assert!(matches!(
        drive(&mut p, &mut big_in, &mut big_out, 65),
        ProcessStatus::Error
    ));
    assert!(
        bo.iter().all(|&x| x == 7.0),
        "a refused block writes nothing"
    );
    // A null output row (a host lying about its buffers): Error, no UB.
    let mut null_out = [core::ptr::null_mut()];
    assert!(matches!(
        drive(&mut p, &mut in_ptrs, &mut null_out, 64),
        ProcessStatus::Error
    ));
    // Deactivated: Error again.
    p.deactivate();
    assert!(matches!(
        drive(&mut p, &mut in_ptrs, &mut out_ptrs, 64),
        ProcessStatus::Error
    ));
    assert!(
        p.levels().is_empty() && p.db().is_empty(),
        "views empty when no bank"
    );
}
