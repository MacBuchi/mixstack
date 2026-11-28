# Measurement — `mixstack.measure/1`, `mixstack.compare/1`

Objective numbers for a mix and every track in it, for a human, the band
player or an agent in a closed loop (change → render → measure → keep or
revert). Classic signal processing only; the module never decides what is
"better". Implementation: `engine/src/measure.rs` (the model is documented
there in full, with sources). Tests: `measure_tests` in
`engine/tests/engine_tests.rs`, CLI tests in `cli/tests/cli_tests.rs`.

```
mixstack measure <in.wav> [--mix <template|session>] [--start s] [--end s]
                 [--tracks mix|all|<name,…>] [--track <name>]…
                 [--masking off|vs-rest|pairs:<name,…>] [--pre-fader] [--json]
mixstack compare <in.wav> --mix-a <a.json> --mix-b <b.json> [same flags]
```

Without `--mix`, every track is measured at unity (a fresh session).
`--track` (repeatable) exists for names that contain a comma.

## What is measured

| Field | Meaning |
|---|---|
| `lufs_i`, `lra`, `true_peak_dbtp` | ITU-R BS.1770-5 / EBU R 128 (`ebur128` crate, as in the render report) |
| `sample_peak_dbfs`, `crest_db` | sample peak; peak − RMS |
| `plr_db` | true peak − LUFS-I |
| `short_term_lufs.{p10,p50,p95}` | percentiles of 3 s loudness above −70 LUFS |
| `ltas_db[]` | long-term spectrum per third-octave band (`bands_hz`, IEC 61260-1, 25 Hz–20 kHz, bands above Nyquist dropped), **AES17 dBFS**: a full-scale sine reads 0 dB; stereo = mean of both channels |
| `active_fraction`, `first_active_s`, `last_active_s` | momentary loudness within 20 LU of the signal's own LUFS-I (and above −70 LUFS); times ±100 ms |
| mix: `correlation`, `width`, `correlation_bands[]`, `width_bands[]` | phase correlation; RMS(side)/RMS(mid) |
| mix: `normalisation_gain_db` | the gain the render's normalisation would apply (same function), absent with mastering |
| track: `masking.msr_db[]`, `masking.masked_time_fraction[]` | per band: mean masker-to-signal ratio (> 0 dB = masked) and share of frames masked, over the frames in which the band is audible in the track on its own; `null` where it never is |
| track: `masking.masked_fraction`, `masking.m_index` | share of counted (frame, band) cells masked; Mₙ of Ma et al. eq. 11 (Σ MSR/20 dB over masked bands), frame average |
| `pairs[]` | with `--masking pairs:…`: `{maskee, masker, masked_fraction, m_index}` for every ordered pair, at most 12 tracks |

**The mix** is the bus render pass 1 measures — before normalisation,
mastering, limiter and fades. **A track** is measured post-fader by default
(its stereo contribution to that bus: EQ, gain, pan, polarity — what lands in
the mix), or with `--pre-fader` as its EQ'd mono channel before gain and pan.
Masking is always post-fader. Tracks that are muted, soloed away or out of the
mix read as silence post-fader and have no masking.

**Masking model:** track *n* against the rest of the mix (the sum of all other
tracks), MSR = 10·log10(T̃ₙ/Eₙ) per band and ~46 ms frame, after the
cross-adaptive MPEG masking metric of Ma, Reiss et al. (Ronan et al.,
arXiv:1803.09960 v1/v2; the entry is withdrawn, v1/v2 remain retrievable),
with a simplified MPEG model-2 threshold in the spirit of Hafezi & Reiss
(JAES 2015): third-octave bands, Schroeder spreading on the Bark scale, an
offset between 6 dB (noise masker) and 29 dB (tonal masker) from the masker's
spectral flatness. Details and what is left out: the module doc of
`engine/src/measure.rs`.

Non-finite values (the loudness of silence) are `null` in JSON.

## compare

Both mixes are measured in **one read** of the file. `delta` holds `b − a` for
the mix (`lufs_i`, `lra`, `true_peak_dbtp`, `plr_db`, `crest_db`,
`short_term_p50`, `ltas_db[]`, `active_fraction`, `correlation`, `width`) and
for each track measured in both (matched by interleave index), including the
masking values. `a` and `b` carry the two full measurements; `mapping_a`,
`mapping_b` the two mapping reports.

## Cost

Linear in the channel count — the rest-of-mix spectrum of a track is the mix
spectrum minus its own (the FFT is linear), so it is one FFT per track and
frame. Measured with `cargo run -p durecmix-engine --release --example
channel_scaling <n> 0.0` on the user's Mac (2026-11-27, release): 34 tracks
9.3× realtime, 200 tracks 1.5× realtime. On the real UFX33 take (34 ch,
44.1 kHz): a 30 s excerpt in 3.5 s, the whole 205 s song in 21.9 s.
