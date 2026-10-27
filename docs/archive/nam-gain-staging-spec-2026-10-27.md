# Archived external artifact: "Real-Time Gain Staging & Calibration Engine" + "Component Tolerance Simulation" (spec §6–7)

> **Provenance.** Pasted verbatim into the project chat by the author on 2026-10-27 from an
> external document whose sections 1–5 were *not* provided and whose authorship is unknown to
> us. This file exists so that citations of "the pasted spec" in
> [hathor-calibration-variance.md](../modules/hathor-calibration-variance.md) and
> [hathor-nam.md](../modules/hathor-nam.md) resolve to something in-repo instead of to a
> conversation — the same failure class we recorded for `Herrie` and `steveatkinson`: a plan
> anchored on an unarchived source. **Status: external, non-normative.** Hathor's contract
> stands on its own merits and its own citations; where this artifact is internally
> inconsistent (±0.005-linear struct vs ±0.3 dB prose) or unverified (NN amplitude claims),
> our docs say so rather than inheriting the claim. Math notation kept as received.

---

## 6. Real-Time Gain Staging & Calibration Engine

Neural networks inside NAM are highly sensitive to the absolute amplitude of the incoming signal. Unlike traditional linear digital systems where $0\text{ dBFS}$ is the only hard ceiling, NAM models exhibit an analog-style saturation curve. If the input signal is too hot, the model will over-saturate and introduce harsh artifacts.

### A. The -18 dBFS Calibration Standard
To ensure predictable analog behavior, the DAW must implement an automatic or user-adjustable calibration stage before hitting the NAM inference node.
*   **Target Alignment:** Align the digital signal so that $0\text{ dBu}$ (the nominal operating level of the emulated analog hardware) corresponds to $-18\text{ dBFS}$ (or a configurable $-20\text{ dBFS}$ to $-14\text{ dBFS}$ window) inside the floating-point pipeline.
*   **DSP Pre-Gain Stage:** Every channel strip must feature a hard-coded, zero-latency gain coefficient stage ($G_{cal}$) applied directly to the incoming audio buffer before the inference loop:
    $$X_{calibrated}[n] = X_{input}[n] \times G_{cal}$$

### B. Level Tracking and Safety Limiting
*   **RMS/LUFS Monitoring:** The mixer architecture must calculate running RMS values over a small window (e.g., $300\text{ ms}$) to guide the user visually toward the analog "sweet spot".
*   **Post-NAM Auto-MakeUp Gain:** Because driving a NAM model harder can compress the dynamic range and alter perceived volume, an optional decoupled look-ahead or instantaneous auto-gain module should stabilize the output peak levels.

---

## 7. Component Tolerance Simulation (Channel Variance Algorithm)

To break the "digital phase alignment" where identical plug-ins sound sterile when multi-tracked, the DAW must introduce deterministic, per-channel hardware component drift.

### A. Seeded Deterministic Variation
Using a global pseudo-random number generator (PRNG) or a unique hash based on the `Channel_ID`, compute a static drift vector $\mathbf{\Delta}_c$ during track creation. This ensures that a specific channel always sounds identical to itself across project reloads, mimicking fixed physical component values.

### B. Implementation Blueprint in Rust
```rust
struct AnalogChannelVariance {
    channel_id: u32,
    eq_frequency_drift: f32, // e.g., +/- 0.02 (2% drift)
    nam_bias_offset: f32,    // e.g., +/- 0.005 input scaling tweak
}

impl AnalogChannelVariance {
    fn new(channel_id: u32) -> Self {
        // Use a lightweight, deterministic hasher to derive offsets from the ID
        let hash = low_overhead_hash(channel_id);

        Self {
            channel_id,
            // Map hash values to tiny physical component tolerance brackets (e.g., +/- 1% to 5%)
            eq_frequency_drift: map_to_range(hash, -0.02, 0.02),
            nam_bias_offset: map_to_range(hash ^ 0x55555555, -0.005, 0.005),
        }
    }
}
```

### C. Target Vectors for Variance
1.  **NAM Input Scale Perturbation:** Apply `nam_bias_offset` to the calibrated pre-gain stage of each channel. A variance of just $\pm 0.3\text{ dB}$ changes how hard that specific track hits the "virtual transformer," introducing organic variations in harmonic distortion across the mix.
2.  **Filter Coefficient Shifting:** Shift the cutoff frequencies of high-pass (HPF) and low-pass (LPF) filters by the `eq_frequency_drift` percentage. This tiny phase alteration between channels replicates the behavior of analog capacitors, creating a wide, pseudo-three-dimensional stereo field when tracks are panned.
