//! Objective measurement of a mix and of every track in it (#166).
//!
//! Classic signal processing, no learned models: the numbers a human, the
//! band player or an agent in a closed loop (change → render → measure →
//! keep or revert) reads to decide what to do next. This module only
//! measures; what counts as "better" is deliberately not its business.
//!
//! # What is measured, and where
//!
//! The **mix** is the output of [`MixChain`] — exactly the bus that render
//! pass 1 measures, before normalisation, mastering, limiter and fades.
//! A **track** is measured at one of two points ([`LevelPoint`]): by
//! default *post-fader*, i.e. its stereo contribution to that bus (EQ,
//! gain, pan, polarity applied — what lands in the mix; the contributions
//! of all audible tracks sum to the mix, which a test pins), or *pre-fader*:
//! the EQ'd mono channel before gain and pan, like the pre-fader meters of
//! #115. Tracks that are muted, soloed away or out of the mix contribute
//! nothing post-fader, so their post-fader figures are silence.
//!
//! | Quantity | Basis |
//! |---|---|
//! | LUFS-I, LRA, true peak, momentary/short-term | ITU-R BS.1770-5, EBU R 128 / Tech 3341–3342, via the `ebur128` crate (the same one the render reports with) |
//! | PLR, crest factor, short-term percentiles 10/50/95 | derived |
//! | LTAS in third-octave bands, 25 Hz–20 kHz | IEC 61260-1 base-10 mid-band frequencies, STFT power |
//! | masking: masker-to-signal ratio per band, masked share of time | see below |
//! | stereo: correlation, width (side/mid), overall and per band | mix only |
//! | activity: active share, first/last active time | gate on momentary loudness |
//!
//! All levels in dB are **AES17 dBFS**: a full-scale sine reads 0 dB. For a
//! stereo signal the band power is the mean of the two channels, so a
//! centre-panned track reads 3 dB below its mono source — the pan law, not
//! an error.
//!
//! # Masking model
//!
//! Follows the cross-adaptive MPEG masking metric of Ma, Reiss et al.
//! ("Automatic Minimisation of Masking in Multitrack Audio using
//! Subgroups", Ronan, Ma, Mc Namara, Gunes, Reiss, arXiv:1803.09960 v1/v2;
//! the arXiv entry was withdrawn in 2021, v1/v2 remain retrievable). Track
//! *n* is masked by the **sum of all other tracks**, i.e. the rest of the
//! mix (their eq. 9: T̃ₙ = H(Σ_{i≠n} sᵢ), H = the psychoacoustic threshold
//! computation). Per band and frame:
//!
//! ```text
//! MSRₙ(b) = 10·log10( T̃ₙ(b) / Eₙ(b) )                     (their eq. 10)
//! masked  ⇔  T̃ₙ(b) > Eₙ(b)
//! Mₙ      = Σ_{b masked} MSRₙ(b) / T_max,  T_max = 20 dB   (their eq. 11)
//! ```
//!
//! H is a simplified MPEG-1 psychoacoustic model 2, in the spirit of the
//! "simplified masking measure" of Hafezi & Reiss (JAES 63(5), 2015, which
//! Ma et al. cite as their ref. [33]) — simple enough to run per track and
//! frame on a 200-track take:
//!
//! 1. **Bands** are the third-octave bands of the LTAS rather than MPEG's
//!    scale-factor partitions, so masking and spectrum share one axis.
//! 2. **Spreading** across bands with the Schroeder spreading function used
//!    by model 2, on the Bark scale of Ma et al. eq. 14
//!    (`z = 13·atan(0.00076 f) + 3.5·atan((f/7500)²)`):
//!    `B(dz) = 15.81 + 7.5(dz+0.474) − 17.5·√(1+(dz+0.474)²)` dB with
//!    `dz = z(maskee) − z(masker)`, zero below −60 dB (their eq. 13), and the
//!    spread energy renormalised by the spreading sum like model 2's.
//! 3. **Offset** between excitation and threshold interpolated between
//!    noise-masking-tone (6 dB) and tone-masking-noise (29 dB), the two values
//!    Ma et al. quote from the standard. The tonality index comes from the
//!    masker's spectral flatness in the band (Johnston 1988:
//!    α = min(SFM_dB / −60, 1)) instead of model 2's unpredictability
//!    measure, which needs phase history per bin and track.
//! 4. **Left out:** the threshold in quiet (the files carry no SPL
//!    calibration), the two-frame temporal maximum, pre-echo control.
//! 5. **Which bands count.** A band only counts when it is audible in the
//!    track *on its own*: above the track's own masking threshold (the same
//!    H applied to the track itself), above −80 dBFS, in a frame whose total
//!    is above −60 dBFS. Without the first condition the window's spectral
//!    leakage of a loud partial — 50–70 dB down in the neighbouring bands —
//!    counts as content and is, unsurprisingly, "masked"; a pure tone then
//!    reads as mostly masked while its actual band is not. Whether someone
//!    else hides a component the track itself already hides is not a
//!    question a mix engineer can act on.
//!
//! Frames are ~46 ms (2048 samples at 44.1/48 kHz, the power of two nearest
//! to 46 ms at any rate), Hann window, 50 % overlap. Below ~100 Hz a
//! third-octave band is narrower than two FFT bins; bins are shared out by
//! their overlap with the band, which keeps the energy right but blurs the
//! lowest bands into each other.
//!
//! # Cost
//!
//! **Linear in the channel count**, measured with `channel_scaling`: the
//! FFT is linear, so the rest-of-mix spectrum of a track is the mix spectrum
//! minus its own — `R = X_mix − g·S` — one FFT per track and frame, never
//! one per pair. A pairwise matrix exists only on request, for at most
//! [`MAX_PAIR_TRACKS`] named tracks. One read of the file serves any number
//! of mixes ([`measure`] takes a slice), which is what `compare` uses.

use std::f64::consts::PI;
use std::sync::Arc;

use realfft::num_complex::Complex;
use realfft::{RealFftPlanner, RealToComplex};
use serde::{Deserialize, Serialize};

use crate::chain::{ChainConfig, EqChain, MixChain};
use crate::dsp::linear_to_db;
use crate::error::{EngineError, Result};
use crate::mix::{resolve_channels, TrackParams};
use crate::render::{normalisation_gain, MixLevel, RenderSettings, BLOCK_FRAMES};
use crate::wav::InputHandle;

pub const MEASURE_SCHEMA: &str = "mixstack.measure/1";
pub const COMPARE_SCHEMA: &str = "mixstack.compare/1";

/// Most tracks a pairwise masking matrix may name — it is quadratic.
pub const MAX_PAIR_TRACKS: usize = 12;
/// T_max of Ma et al. eq. 11.
pub const MASKING_T_MAX_DB: f64 = 20.0;
/// A track is active while its momentary loudness is within this many LU
/// of its own integrated loudness (and above BS.1770's −70 LUFS gate):
/// spill from other instruments, typically 20 dB and more down, then does
/// not count as playing.
pub const ACTIVITY_GATE_LU: f64 = 20.0;

/// Frames per internal sub-block, see [`MeasureStream::push_frames`].
pub const SUB_BLOCK_FRAMES: usize = 4096;

const NMT_DB: f64 = 6.0;
const TMN_DB: f64 = 29.0;
const BAND_FLOOR_DB: f64 = -80.0;
const FRAME_FLOOR_DB: f64 = -60.0;
const MSR_CLAMP_DB: f64 = 60.0;
const ABS_GATE_LUFS: f64 = -70.0;

/// IEC 61260-1 nominal third-octave mid-band frequencies, 25 Hz–20 kHz.
pub const THIRD_OCTAVE_NOMINAL_HZ: [f64; 30] = [
    25.0, 31.5, 40.0, 50.0, 63.0, 80.0, 100.0, 125.0, 160.0, 200.0, 250.0, 315.0, 400.0, 500.0,
    630.0, 800.0, 1000.0, 1250.0, 1600.0, 2000.0, 2500.0, 3150.0, 4000.0, 5000.0, 6300.0, 8000.0,
    10000.0, 12500.0, 16000.0, 20000.0,
];

/// Which tracks get per-track figures.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum TrackSelection {
    /// The audible ones (in the mix, not muted, not soloed away).
    #[default]
    Mix,
    All,
    /// Exactly these names; an unknown name is an error, not a skip.
    Names(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "kebab-case")]
pub enum MaskingMode {
    Off,
    /// Every measured track against the rest of the mix. Linear.
    #[default]
    VsRest,
    /// Additionally a pairwise matrix among these named tracks (≤ 12).
    Pairs(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum LevelPoint {
    #[default]
    PostFader,
    PreFader,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct MeasureOptions {
    pub tracks: TrackSelection,
    pub masking: MaskingMode,
    pub point: LevelPoint,
}

/// One mix to measure: its tracks, their tags (parallel, may be shorter)
/// and its settings, whose trim decides the range.
#[derive(Debug, Clone, Copy)]
pub struct MeasureJob<'a> {
    pub tracks: &'a [TrackParams],
    pub tags: &'a [Vec<String>],
    pub settings: &'a RenderSettings,
}

// ── results ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Percentiles {
    #[cfg_attr(feature = "schema", schemars(with = "Option<f64>"))]
    pub p10: f64,
    #[cfg_attr(feature = "schema", schemars(with = "Option<f64>"))]
    pub p50: f64,
    #[cfg_attr(feature = "schema", schemars(with = "Option<f64>"))]
    pub p95: f64,
}

/// What is measured on any signal, mix or track. Non-finite values (the
/// loudness of silence) serialise as `null`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LevelMetrics {
    #[cfg_attr(feature = "schema", schemars(with = "Option<f64>"))]
    pub lufs_i: f64,
    pub lra: f64,
    #[cfg_attr(feature = "schema", schemars(with = "Option<f64>"))]
    pub true_peak_dbtp: f64,
    #[cfg_attr(feature = "schema", schemars(with = "Option<f64>"))]
    pub sample_peak_dbfs: f64,
    /// Peak-to-loudness ratio: true peak − LUFS-I.
    #[cfg_attr(feature = "schema", schemars(with = "Option<f64>"))]
    pub plr_db: f64,
    /// Sample peak − RMS, both in dBFS.
    #[cfg_attr(feature = "schema", schemars(with = "Option<f64>"))]
    pub crest_db: f64,
    /// Percentiles of short-term (3 s) loudness above −70 LUFS.
    pub short_term_lufs: Percentiles,
    /// Long-term average spectrum per band, AES17 dBFS.
    #[cfg_attr(feature = "schema", schemars(with = "Vec<Option<f64>>"))]
    pub ltas_db: Vec<f64>,
    pub active_fraction: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_active_s: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_active_s: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MixMetrics {
    #[serde(flatten)]
    pub level: LevelMetrics,
    /// Phase correlation over the range, −1..+1.
    #[cfg_attr(feature = "schema", schemars(with = "Option<f64>"))]
    pub correlation: f64,
    /// RMS(side) / RMS(mid): 0 for mono, 1 for two uncorrelated channels.
    #[cfg_attr(feature = "schema", schemars(with = "Option<f64>"))]
    pub width: f64,
    #[cfg_attr(feature = "schema", schemars(with = "Vec<Option<f64>>"))]
    pub correlation_bands: Vec<f64>,
    #[cfg_attr(feature = "schema", schemars(with = "Vec<Option<f64>>"))]
    pub width_bands: Vec<f64>,
    /// The gain the render's normalisation would apply to this mix (without
    /// fades, which the measurement leaves out), from the one
    /// [`normalisation_gain`] the render uses. `None` with mastering on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub normalisation_gain_db: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MaskingMetrics {
    /// Mean masker-to-signal ratio per band over the frames where the track
    /// has signal in it; `null` where it never does. > 0 dB = masked.
    pub msr_db: Vec<Option<f64>>,
    /// Share of those frames in which the band is masked.
    pub masked_time_fraction: Vec<Option<f64>>,
    /// Share of all counted (frame, band) cells that are masked.
    #[cfg_attr(feature = "schema", schemars(with = "Option<f64>"))]
    pub masked_fraction: f64,
    /// Ma et al.'s Mₙ (eq. 11), averaged over the counted frames.
    #[cfg_attr(feature = "schema", schemars(with = "Option<f64>"))]
    pub m_index: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TrackMetrics {
    pub index: u32,
    pub name: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tags: Vec<String>,
    /// Audible in this mix (in the mix, not muted, not soloed away).
    pub in_mix: bool,
    #[serde(flatten)]
    pub level: LevelMetrics,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub masking: Option<MaskingMetrics>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PairMasking {
    pub maskee: String,
    pub masker: String,
    #[cfg_attr(feature = "schema", schemars(with = "Option<f64>"))]
    pub masked_fraction: f64,
    #[cfg_attr(feature = "schema", schemars(with = "Option<f64>"))]
    pub m_index: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Range {
    pub start: f64,
    pub end: f64,
}

/// `mixstack.measure/1`, minus the fields only a front end knows (input
/// path, engine version), which the caller adds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Measurement {
    pub schema: String,
    pub range: Range,
    pub sample_rate: u32,
    pub frame_seconds: f64,
    pub bands_hz: Vec<f64>,
    pub level_point: LevelPoint,
    pub mix: MixMetrics,
    pub tracks: Vec<TrackMetrics>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub pairs: Vec<PairMasking>,
}

// ── bands ───────────────────────────────────────────────────────────────────

fn bark(f: f64) -> f64 {
    13.0 * (0.00076 * f).atan() + 3.5 * ((f / 7500.0).powi(2)).atan()
}

/// Schroeder spreading function in dB, `dz = z(maskee) − z(masker)`.
pub fn spreading_db(dz: f64) -> f64 {
    let t = dz + 0.474;
    15.81 + 7.5 * t - 17.5 * (1.0 + t * t).sqrt()
}

struct Bands {
    nominal: Vec<f64>,
    /// Per band: (bin, weight) by overlap of the bin with the band.
    weights: Vec<Vec<(usize, f64)>>,
    /// Per band: bins whose centre lies in it (for spectral flatness).
    centre_bins: Vec<(usize, usize)>,
    /// Row-normalised spreading matrix, `spread[maskee][masker]`.
    spread: Vec<Vec<f64>>,
}

impl Bands {
    fn new(sample_rate: u32, n_fft: usize) -> Bands {
        let nyq = sample_rate as f64 / 2.0;
        let df = sample_rate as f64 / n_fft as f64;
        let bins = n_fft / 2 + 1;
        let mut nominal = Vec::new();
        let mut edges = Vec::new();
        for (i, &nom) in THIRD_OCTAVE_NOMINAL_HZ.iter().enumerate() {
            // Exact base-10 mid-band frequency: 1000 · 10^(n/10), n = −16..13.
            let fm = 1000.0 * 10f64.powf((i as f64 - 16.0) / 10.0);
            let lo = fm * 10f64.powf(-0.05);
            let hi = (fm * 10f64.powf(0.05)).min(nyq);
            if lo >= nyq {
                break;
            }
            nominal.push(nom);
            edges.push((lo, hi, fm));
        }
        let weights = edges
            .iter()
            .map(|&(lo, hi, _)| {
                (0..bins)
                    .filter_map(|k| {
                        let (a, b) = ((k as f64 - 0.5) * df, (k as f64 + 0.5) * df);
                        let w = (b.min(hi) - a.max(lo)).max(0.0) / df;
                        (w > 0.0).then_some((k, w))
                    })
                    .collect()
            })
            .collect();
        let centre_bins = edges
            .iter()
            .map(|&(lo, hi, _)| {
                let a = (lo / df).ceil() as usize;
                let b = ((hi / df).floor() as usize).min(bins - 1);
                (a, b.max(a))
            })
            .collect();
        let z: Vec<f64> = edges.iter().map(|&(_, _, fm)| bark(fm)).collect();
        let spread = z
            .iter()
            .map(|&zi| {
                let row: Vec<f64> = z
                    .iter()
                    .map(|&zj| {
                        let db = spreading_db(zi - zj);
                        if db <= -60.0 {
                            0.0
                        } else {
                            10f64.powf(db / 10.0)
                        }
                    })
                    .collect();
                let sum: f64 = row.iter().sum();
                row.into_iter().map(|v| v / sum).collect()
            })
            .collect();
        Bands {
            nominal,
            weights,
            centre_bins,
            spread,
        }
    }

    fn len(&self) -> usize {
        self.nominal.len()
    }
}

/// Power of two nearest (on a log scale) to 46 ms.
fn frame_len(sample_rate: u32) -> usize {
    let target = 0.046 * sample_rate as f64;
    1usize << (target.log2().round() as u32)
}

// ── per-signal level meter ──────────────────────────────────────────────────

struct LevelMeter {
    ebu: ebur128::EbuR128,
    channels: usize,
    hop: usize,
    pending: usize,
    momentary: Vec<f64>,
    short_term: Vec<f64>,
    /// K-weighted energy of the last 30 100 ms blocks, newest last.
    blocks: std::collections::VecDeque<f64>,
    sum_sq: f64,
    samples: u64,
    peak: f64,
    ltas: Vec<f64>,
    frames: u64,
}

impl LevelMeter {
    fn new(channels: usize, sample_rate: u32, bands: usize) -> Result<LevelMeter> {
        Ok(LevelMeter {
            ebu: ebur128::EbuR128::new(
                channels as u32,
                sample_rate,
                ebur128::Mode::I | ebur128::Mode::LRA | ebur128::Mode::TRUE_PEAK | ebur128::Mode::S,
            )
            .map_err(|e| EngineError::Encode(format!("ebur128: {e}")))?,
            channels,
            hop: (sample_rate / 10) as usize,
            pending: 0,
            momentary: Vec::new(),
            short_term: Vec::new(),
            blocks: std::collections::VecDeque::with_capacity(30),
            sum_sq: 0.0,
            samples: 0,
            peak: 0.0,
            ltas: vec![0.0; bands],
            frames: 0,
        })
    }

    /// Interleaved samples, `channels` per frame. Loudness is polled every
    /// 100 ms of input, the BS.1770 block step.
    fn push(&mut self, interleaved: &[f64]) {
        for &s in interleaved {
            self.sum_sq += s * s;
            self.peak = self.peak.max(s.abs());
        }
        self.samples += interleaved.len() as u64;
        let mut rest = interleaved;
        while !rest.is_empty() {
            let want = (self.hop - self.pending) * self.channels;
            let take = want.min(rest.len());
            let _ = self.ebu.add_frames_f64(&rest[..take]);
            self.pending += take / self.channels;
            rest = &rest[take..];
            if self.pending == self.hop {
                self.pending = 0;
                // Momentary (400 ms) and short-term (3 s) loudness are the
                // mean energy of the last 4 and 30 100 ms blocks — exactly,
                // since the blocks are equally long. Asking ebur128 for the
                // windows themselves re-sums 400 ms and 3 s of samples every
                // 100 ms, 34× the audio; it was 60 % of the measurement.
                let l = self.ebu.loudness_window(100).unwrap_or(f64::NEG_INFINITY);
                let e = if l.is_finite() {
                    10f64.powf((l + 0.691) / 10.0)
                } else {
                    0.0
                };
                if self.blocks.len() == 30 {
                    self.blocks.pop_front();
                }
                self.blocks.push_back(e);
                let mean_last = |n: usize| {
                    // Before n blocks exist the window is zero-padded, as in
                    // ebur128's own buffer.
                    self.blocks.iter().rev().take(n).sum::<f64>() / n as f64
                };
                let to_lufs = |e: f64| {
                    if e > 0.0 {
                        -0.691 + 10.0 * e.log10()
                    } else {
                        f64::NEG_INFINITY
                    }
                };
                self.momentary.push(to_lufs(mean_last(4)));
                self.short_term.push(to_lufs(mean_last(30)));
            }
        }
    }

    fn add_spectrum(&mut self, band_power: &[f64]) {
        for (a, p) in self.ltas.iter_mut().zip(band_power) {
            *a += p;
        }
        self.frames += 1;
    }

    fn finish(&self) -> LevelMetrics {
        let lufs_i = self.ebu.loudness_global().unwrap_or(f64::NEG_INFINITY);
        let tp = (0..self.channels as u32)
            .filter_map(|c| self.ebu.true_peak(c).ok())
            .fold(0.0f64, f64::max);
        let rms = if self.samples > 0 {
            (self.sum_sq / self.samples as f64).sqrt()
        } else {
            0.0
        };
        let tp_db = linear_to_db(tp);
        let peak_db = linear_to_db(self.peak);
        let mut st: Vec<f64> = self
            .short_term
            .iter()
            .copied()
            .filter(|v| v.is_finite() && *v > ABS_GATE_LUFS)
            .collect();
        st.sort_by(|a, b| a.total_cmp(b));
        let pct = |p: f64| {
            if st.is_empty() {
                f64::NEG_INFINITY
            } else {
                st[((st.len() - 1) as f64 * p).round() as usize]
            }
        };
        let gate = (lufs_i - ACTIVITY_GATE_LU).max(ABS_GATE_LUFS);
        let active: Vec<usize> = self
            .momentary
            .iter()
            .enumerate()
            .filter(|(_, m)| m.is_finite() && **m > gate)
            .map(|(i, _)| i)
            .collect();
        // Momentary value i covers the 400 ms ending at (i+1) · 100 ms and
        // passes the gate once roughly one of its four 100 ms blocks carries
        // signal: an onset lies in the newest block of the first active
        // window, an end in the oldest block of the last. ±100 ms.
        // Still active in the very last window means: active to the end.
        let last = self.momentary.len().saturating_sub(1);
        let start_of = |i: usize| ((i as f64 + 1.0) * 0.1 - 0.1).max(0.0);
        let end_of = |i: usize| {
            let end = (i as f64 + 1.0) * 0.1;
            if i == last {
                end
            } else {
                (end - 0.3).max(0.0)
            }
        };
        let frames = self.frames.max(1) as f64;
        LevelMetrics {
            lufs_i,
            lra: self.ebu.loudness_range().unwrap_or(0.0),
            true_peak_dbtp: tp_db,
            sample_peak_dbfs: peak_db,
            plr_db: tp_db - lufs_i,
            crest_db: peak_db - linear_to_db(rms),
            short_term_lufs: Percentiles {
                p10: pct(0.10),
                p50: pct(0.50),
                p95: pct(0.95),
            },
            ltas_db: self.ltas.iter().map(|p| aes_db(p / frames)).collect(),
            active_fraction: if self.momentary.is_empty() {
                0.0
            } else {
                active.len() as f64 / self.momentary.len() as f64
            },
            first_active_s: active.first().map(|&i| start_of(i)),
            last_active_s: active.last().map(|&i| end_of(i)),
        }
    }
}

/// Mean-square band power → AES17 dBFS (full-scale sine = 0 dB).
fn aes_db(p: f64) -> f64 {
    10.0 * (2.0 * p).log10()
}

// ── masking accumulators ────────────────────────────────────────────────────

#[derive(Clone)]
struct MaskAccum {
    msr_sum: Vec<f64>,
    counted: Vec<u64>,
    masked: Vec<u64>,
    m_sum: f64,
    m_frames: u64,
}

impl MaskAccum {
    fn new(bands: usize) -> MaskAccum {
        MaskAccum {
            msr_sum: vec![0.0; bands],
            counted: vec![0; bands],
            masked: vec![0; bands],
            m_sum: 0.0,
            m_frames: 0,
        }
    }

    /// One frame: the maskee's band powers against a threshold; `own` is
    /// the maskee's own threshold (bands below it are inaudible solo and do
    /// not count).
    fn add(&mut self, signal: &[f64], threshold: &[f64], own: &[f64]) {
        let total: f64 = signal.iter().sum();
        if aes_db(total) < FRAME_FLOOR_DB {
            return;
        }
        let mut m = 0.0;
        for b in 0..signal.len() {
            if aes_db(signal[b]) < BAND_FLOOR_DB || signal[b] <= own[b] {
                continue;
            }
            let msr =
                (10.0 * (threshold[b] / signal[b]).log10()).clamp(-MSR_CLAMP_DB, MSR_CLAMP_DB);
            self.msr_sum[b] += msr;
            self.counted[b] += 1;
            if threshold[b] > signal[b] {
                self.masked[b] += 1;
                m += msr / MASKING_T_MAX_DB;
            }
        }
        self.m_sum += m;
        self.m_frames += 1;
    }

    fn masked_fraction(&self) -> f64 {
        let c: u64 = self.counted.iter().sum();
        let m: u64 = self.masked.iter().sum();
        if c == 0 {
            0.0
        } else {
            m as f64 / c as f64
        }
    }

    fn m_index(&self) -> f64 {
        if self.m_frames == 0 {
            0.0
        } else {
            self.m_sum / self.m_frames as f64
        }
    }

    fn finish(&self) -> MaskingMetrics {
        MaskingMetrics {
            msr_db: (0..self.counted.len())
                .map(|b| (self.counted[b] > 0).then(|| self.msr_sum[b] / self.counted[b] as f64))
                .collect(),
            masked_time_fraction: (0..self.counted.len())
                .map(|b| {
                    (self.counted[b] > 0).then(|| self.masked[b] as f64 / self.counted[b] as f64)
                })
                .collect(),
            masked_fraction: self.masked_fraction(),
            m_index: self.m_index(),
        }
    }
}

/// Spectral flatness of `power[a..=b]` → tonality α (Johnston 1988).
fn tonality(power: impl Iterator<Item = f64>) -> f64 {
    let (mut n, mut log_sum, mut sum) = (0usize, 0.0, 0.0);
    for p in power {
        let p = p.max(1e-30);
        log_sum += p.ln();
        sum += p;
        n += 1;
    }
    if n < 2 || sum <= 0.0 {
        return 0.5;
    }
    let geo = (log_sum / n as f64).exp();
    let sfm_db = 10.0 * (geo / (sum / n as f64)).log10();
    (sfm_db / -60.0).clamp(0.0, 1.0)
}

/// Masking threshold per band from a masker's band powers and tonality.
fn threshold(bands: &Bands, masker: &[f64], alpha: &[f64], out: &mut [f64]) {
    for (b, t) in out.iter_mut().enumerate() {
        let e: f64 = bands.spread[b].iter().zip(masker).map(|(s, p)| s * p).sum();
        let offset = alpha[b] * TMN_DB + (1.0 - alpha[b]) * NMT_DB;
        *t = e * 10f64.powf(-offset / 10.0);
    }
}

// ── the measurer for one mix ────────────────────────────────────────────────

struct TrackState {
    pos: usize,
    channel: usize,
    left: f64,
    right: f64,
    audible: bool,
    eq: EqChain,
    meter: LevelMeter,
    mask: Option<MaskAccum>,
    /// Slot in the pair matrix, if named there.
    pair_slot: Option<usize>,
    /// Samples of the EQ'd channel since the oldest frame still pending.
    tail: Vec<f64>,
}

struct Measurer {
    sample_rate: u32,
    channels: usize,
    chain: MixChain,
    mix_meter: LevelMeter,
    tracks: Vec<TrackState>,
    point: LevelPoint,
    stereo: Vec<f64>,
    mix_tail: [Vec<f64>; 2],
    // Mix stereo accumulators.
    sum_ll: f64,
    sum_rr: f64,
    sum_lr: f64,
    band_ll: Vec<f64>,
    band_rr: Vec<f64>,
    band_lr: Vec<f64>,
    band_mid: Vec<f64>,
    band_side: Vec<f64>,
    pairs: Vec<usize>,
    pair_accum: Vec<MaskAccum>,
}

/// Shared frame geometry and scratch for all measurers of one pass.
struct Framing {
    n: usize,
    hop: usize,
    window: Vec<f64>,
    /// 1 / (N · Σw²): mean-square scale for a full two-sided spectrum.
    norm: f64,
    fft: Arc<dyn RealToComplex<f64>>,
    bands: Bands,
}

impl Framing {
    fn new(sample_rate: u32) -> Framing {
        let n = frame_len(sample_rate);
        let window: Vec<f64> = (0..n)
            .map(|i| 0.5 - 0.5 * (2.0 * PI * i as f64 / n as f64).cos())
            .collect();
        let wsum: f64 = window.iter().map(|w| w * w).sum();
        Framing {
            n,
            hop: n / 2,
            norm: 1.0 / (n as f64 * wsum),
            fft: RealFftPlanner::<f64>::new().plan_fft_forward(n),
            bands: Bands::new(sample_rate, n),
            window,
        }
    }

    /// One-sided bin weight: DC and Nyquist once, the rest twice.
    fn bin_gain(&self, k: usize) -> f64 {
        if k == 0 || k == self.n / 2 {
            self.norm
        } else {
            2.0 * self.norm
        }
    }

    fn band_power(&self, bins_power: &[f64], out: &mut [f64]) {
        for (b, o) in out.iter_mut().enumerate() {
            *o = self.bands.weights[b]
                .iter()
                .map(|&(k, w)| w * bins_power[k])
                .sum();
        }
    }
}

struct Scratch {
    frame: Vec<f64>,
    spec: Vec<Complex<f64>>,
    mix_l: Vec<Vec<Complex<f64>>>,
    mix_r: Vec<Vec<Complex<f64>>>,
    pow: Vec<f64>,
    pow_rest: Vec<f64>,
    eq_out: Vec<f64>,
    level_buf: Vec<f64>,
}

impl Measurer {
    fn new(
        job: &MeasureJob,
        channels: usize,
        sample_rate: u32,
        opts: &MeasureOptions,
        nb: usize,
    ) -> Result<Measurer> {
        let tracks = job.tracks;
        let audible = resolve_channels(tracks, channels);
        let mut coeff = vec![None; tracks.len()];
        for c in &audible {
            coeff[c.track_pos] = Some((c.left, c.right));
        }
        let measured: Vec<usize> = match &opts.tracks {
            TrackSelection::Mix => (0..tracks.len()).filter(|&p| coeff[p].is_some()).collect(),
            TrackSelection::All => (0..tracks.len()).collect(),
            TrackSelection::Names(names) => names
                .iter()
                .map(|n| {
                    tracks
                        .iter()
                        .position(|t| &t.name == n)
                        .ok_or_else(|| EngineError::InvalidRequest(format!("no track named {n:?}")))
                })
                .collect::<Result<_>>()?,
        };
        let pair_names = match &opts.masking {
            MaskingMode::Pairs(names) => names.clone(),
            _ => Vec::new(),
        };
        if pair_names.len() > MAX_PAIR_TRACKS {
            return Err(EngineError::InvalidRequest(format!(
                "a pairwise masking matrix takes at most {MAX_PAIR_TRACKS} tracks, got {}",
                pair_names.len()
            )));
        }
        let mut pairs = Vec::new();
        for n in &pair_names {
            let p = tracks
                .iter()
                .position(|t| &t.name == n)
                .ok_or_else(|| EngineError::InvalidRequest(format!("no track named {n:?}")))?;
            if coeff[p].is_none() {
                return Err(EngineError::InvalidRequest(format!(
                    "{n:?} is not audible in this mix, so it cannot mask or be masked"
                )));
            }
            pairs.push(p);
        }
        let mut order = measured;
        for &p in &pairs {
            if !order.contains(&p) {
                order.push(p);
            }
        }
        let sr = sample_rate as f64;
        let mut states = Vec::with_capacity(order.len());
        for pos in order {
            let t = &tracks[pos];
            let (left, right) = coeff[pos].unwrap_or((0.0, 0.0));
            let is_audible = coeff[pos].is_some();
            let meter_channels = if opts.point == LevelPoint::PreFader {
                1
            } else {
                2
            };
            states.push(TrackState {
                pos,
                channel: (t.index as usize)
                    .saturating_sub(1)
                    .min(channels.saturating_sub(1)),
                left,
                right,
                audible: is_audible,
                eq: EqChain::new(&t.eq, sr),
                meter: LevelMeter::new(meter_channels, sample_rate, nb)?,
                mask: (opts.masking != MaskingMode::Off && is_audible).then(|| MaskAccum::new(nb)),
                pair_slot: pairs.iter().position(|&p| p == pos),
                tail: Vec::new(),
            });
        }
        let np = pairs.len();
        Ok(Measurer {
            sample_rate,
            channels,
            chain: MixChain::new(tracks, channels, &ChainConfig { sample_rate }),
            mix_meter: LevelMeter::new(2, sample_rate, nb)?,
            tracks: states,
            point: opts.point,
            stereo: Vec::new(),
            mix_tail: [Vec::new(), Vec::new()],
            sum_ll: 0.0,
            sum_rr: 0.0,
            sum_lr: 0.0,
            band_ll: vec![0.0; nb],
            band_rr: vec![0.0; nb],
            band_lr: vec![0.0; nb],
            band_mid: vec![0.0; nb],
            band_side: vec![0.0; nb],
            pairs,
            pair_accum: vec![MaskAccum::new(nb); np * np],
        })
    }

    /// Frames that complete inside a stream whose tail starts at
    /// `tail_len` samples before a block of `block_len` samples.
    fn frame_starts(framing: &Framing, tail_len: usize, block_len: usize) -> usize {
        let total = tail_len + block_len;
        if total < framing.n {
            0
        } else {
            (total - framing.n) / framing.hop + 1
        }
    }

    fn push(&mut self, input: &[f64], fr: &Framing, sc: &mut Scratch) {
        let n_ch = self.channels;
        let frames_in = input.len() / n_ch;
        self.chain.process(input, &mut self.stereo);
        self.mix_meter.push(&self.stereo);
        for f in self.stereo.as_chunks::<2>().0 {
            self.sum_ll += f[0] * f[0];
            self.sum_rr += f[1] * f[1];
            self.sum_lr += f[0] * f[1];
        }

        // Mix spectra for every frame completing in this block. All signals
        // share one sample clock, so the frame count is the same for each.
        let tail_len = self.mix_tail[0].len();
        let nf = Self::frame_starts(fr, tail_len, frames_in);
        let nbins = fr.n / 2 + 1;
        let nb = fr.bands.len();
        for v in [&mut sc.mix_l, &mut sc.mix_r] {
            v.resize_with(nf, || vec![Complex::new(0.0, 0.0); nbins]);
        }
        let mut band_l = vec![0.0; nb];
        let mut band_r = vec![0.0; nb];
        let mut band_m = vec![0.0; nb];
        let mut band_s = vec![0.0; nb];
        let mut mix_band = vec![0.0; nb];
        for i in 0..nf {
            for ch in 0..2 {
                let tail = &self.mix_tail[ch];
                let start = i * fr.hop;
                for (j, x) in sc.frame.iter_mut().enumerate() {
                    let p = start + j;
                    let s = if p < tail_len {
                        tail[p]
                    } else {
                        self.stereo[2 * (p - tail_len) + ch]
                    };
                    *x = s * fr.window[j];
                }
                let dst = if ch == 0 {
                    &mut sc.mix_l[i]
                } else {
                    &mut sc.mix_r[i]
                };
                let _ = fr.fft.process(&mut sc.frame, dst);
            }
            for k in 0..nbins {
                sc.pow[k] = sc.mix_l[i][k].norm_sqr() * fr.bin_gain(k);
            }
            fr.band_power(&sc.pow, &mut band_l);
            for k in 0..nbins {
                sc.pow[k] = sc.mix_r[i][k].norm_sqr() * fr.bin_gain(k);
            }
            fr.band_power(&sc.pow, &mut band_r);
            for k in 0..nbins {
                sc.pow[k] = (sc.mix_l[i][k] * sc.mix_r[i][k].conj()).re * fr.bin_gain(k);
            }
            fr.band_power(&sc.pow, &mut mix_band);
            for b in 0..nb {
                self.band_lr[b] += mix_band[b];
            }
            for k in 0..nbins {
                sc.pow[k] = ((sc.mix_l[i][k] + sc.mix_r[i][k]) * 0.5).norm_sqr() * fr.bin_gain(k);
            }
            fr.band_power(&sc.pow, &mut band_m);
            for k in 0..nbins {
                sc.pow[k] = ((sc.mix_l[i][k] - sc.mix_r[i][k]) * 0.5).norm_sqr() * fr.bin_gain(k);
            }
            fr.band_power(&sc.pow, &mut band_s);
            for b in 0..nb {
                self.band_ll[b] += band_l[b];
                self.band_rr[b] += band_r[b];
                self.band_mid[b] += band_m[b];
                self.band_side[b] += band_s[b];
                mix_band[b] = 0.5 * (band_l[b] + band_r[b]);
            }
            self.mix_meter.add_spectrum(&mix_band);
        }
        let keep_from = nf * fr.hop;
        for ch in 0..2 {
            let mut joined: Vec<f64> = Vec::with_capacity(fr.n);
            let total = tail_len + frames_in;
            for p in keep_from.min(total)..total {
                joined.push(if p < tail_len {
                    self.mix_tail[ch][p]
                } else {
                    self.stereo[2 * (p - tail_len) + ch]
                });
            }
            self.mix_tail[ch] = joined;
        }

        // Pair matrix inputs for this block: per frame, per pair slot.
        let np = self.pairs.len();
        let mut pair_power = vec![vec![0.0; nb]; nf * np];
        let mut pair_alpha = vec![vec![0.5; nb]; nf * np];

        let mut track_band = vec![0.0; nb];
        let mut rest_band = vec![0.0; nb];
        let mut alpha = vec![0.0; nb];
        let mut own_alpha = vec![0.0; nb];
        let mut thr = vec![0.0; nb];
        let mut own_thr = vec![0.0; nb];
        // Own thresholds of the pair tracks, per frame and slot.
        let mut pair_own = vec![vec![0.0; nb]; nf * self.pairs.len()];
        for t in &mut self.tracks {
            let post = self.point == LevelPoint::PostFader;
            if post && !t.audible {
                // Contributes nothing: silence for its meter, no spectra.
                sc.level_buf.clear();
                sc.level_buf.resize(frames_in * 2, 0.0);
                t.meter.push(&sc.level_buf);
                let zero = vec![0.0; nb];
                for _ in 0..nf {
                    t.meter.add_spectrum(&zero);
                }
                continue;
            }
            sc.eq_out.clear();
            sc.eq_out.extend(
                input
                    .chunks_exact(n_ch)
                    .map(|fr| t.eq.process(fr[t.channel])),
            );
            sc.level_buf.clear();
            if post {
                sc.level_buf
                    .extend(sc.eq_out.iter().flat_map(|&s| [s * t.left, s * t.right]));
            } else {
                sc.level_buf.extend_from_slice(&sc.eq_out);
            }
            t.meter.push(&sc.level_buf);
            let gain_pow = if post {
                0.5 * (t.left * t.left + t.right * t.right)
            } else {
                1.0
            };
            let ttail = t.tail.len();
            debug_assert_eq!(ttail, tail_len);
            for i in 0..nf {
                let start = i * fr.hop;
                for (j, x) in sc.frame.iter_mut().enumerate() {
                    let p = start + j;
                    let s = if p < ttail {
                        t.tail[p]
                    } else {
                        sc.eq_out[p - ttail]
                    };
                    *x = s * fr.window[j];
                }
                let _ = fr.fft.process(&mut sc.frame, &mut sc.spec);
                for k in 0..nbins {
                    sc.pow[k] = sc.spec[k].norm_sqr() * fr.bin_gain(k);
                }
                fr.band_power(&sc.pow, &mut track_band);
                // Level/LTAS at the chosen point; masking always post-fader.
                let level: Vec<f64> = track_band.iter().map(|p| p * gain_pow).collect();
                t.meter.add_spectrum(&level);
                let contrib: Vec<f64> = track_band
                    .iter()
                    .map(|p| p * 0.5 * (t.left * t.left + t.right * t.right))
                    .collect();
                if t.mask.is_some() || t.pair_slot.is_some() {
                    for (b, a) in own_alpha.iter_mut().enumerate() {
                        let (lo, hi) = fr.bands.centre_bins[b];
                        *a = tonality(sc.pow[lo..=hi].iter().copied());
                    }
                    threshold(&fr.bands, &contrib, &own_alpha, &mut own_thr);
                }
                if let Some(acc) = &mut t.mask {
                    // Rest of the mix = mix − own contribution, bin by bin.
                    for k in 0..nbins {
                        let rl = sc.mix_l[i][k] - sc.spec[k] * t.left;
                        let rr = sc.mix_r[i][k] - sc.spec[k] * t.right;
                        sc.pow_rest[k] = 0.5 * (rl.norm_sqr() + rr.norm_sqr()) * fr.bin_gain(k);
                    }
                    fr.band_power(&sc.pow_rest, &mut rest_band);
                    for (b, a) in alpha.iter_mut().enumerate() {
                        let (lo, hi) = fr.bands.centre_bins[b];
                        *a = tonality(sc.pow_rest[lo..=hi].iter().copied());
                    }
                    threshold(&fr.bands, &rest_band, &alpha, &mut thr);
                    acc.add(&contrib, &thr, &own_thr);
                }
                if let Some(slot) = t.pair_slot {
                    pair_power[i * np + slot].copy_from_slice(&contrib);
                    pair_alpha[i * np + slot].copy_from_slice(&own_alpha);
                    pair_own[i * np + slot].copy_from_slice(&own_thr);
                }
            }
            let total = ttail + frames_in;
            let mut joined = Vec::with_capacity(fr.n);
            for p in keep_from.min(total)..total {
                joined.push(if p < ttail {
                    t.tail[p]
                } else {
                    sc.eq_out[p - ttail]
                });
            }
            t.tail = joined;
        }

        for i in 0..nf {
            for m in 0..np {
                threshold(
                    &fr.bands,
                    &pair_power[i * np + m],
                    &pair_alpha[i * np + m],
                    &mut thr,
                );
                for n in 0..np {
                    if n != m {
                        self.pair_accum[n * np + m].add(
                            &pair_power[i * np + n],
                            &thr,
                            &pair_own[i * np + n],
                        );
                    }
                }
            }
        }
    }

    fn finish(self, job: &MeasureJob, fr: &Framing, range: Range) -> Measurement {
        let nb = fr.bands.len();
        let corr = |lr: f64, ll: f64, rr: f64| {
            if ll > 0.0 && rr > 0.0 {
                lr / (ll * rr).sqrt()
            } else {
                0.0
            }
        };
        // Rounding can leave a hair of negative side energy for identical
        // channels; that is zero, not NaN.
        let width = |side: f64, mid: f64| {
            if mid > 0.0 {
                (side.max(0.0) / mid).sqrt()
            } else {
                0.0
            }
        };
        let level = self.mix_meter.finish();
        let settings = job.settings;
        let mastering = settings.mastering.as_ref().is_some_and(|m| m.enabled);
        let norm = (!mastering).then(|| {
            linear_to_db(normalisation_gain(
                settings,
                MixLevel {
                    peak: self.mix_meter.peak,
                    integrated_lufs: level.lufs_i,
                },
            ))
        });
        // Overall mid/side from the bus sums: M = (L+R)/2, S = (L−R)/2.
        let mid = 0.25 * (self.sum_ll + self.sum_rr + 2.0 * self.sum_lr);
        let side = 0.25 * (self.sum_ll + self.sum_rr - 2.0 * self.sum_lr);
        let mix = MixMetrics {
            level,
            correlation: corr(self.sum_lr, self.sum_ll, self.sum_rr),
            width: width(side, mid),
            correlation_bands: (0..nb)
                .map(|b| corr(self.band_lr[b], self.band_ll[b], self.band_rr[b]))
                .collect(),
            width_bands: (0..nb)
                .map(|b| width(self.band_side[b], self.band_mid[b]))
                .collect(),
            normalisation_gain_db: norm,
        };
        let names: Vec<String> = self
            .pairs
            .iter()
            .map(|&p| job.tracks[p].name.clone())
            .collect();
        let np = self.pairs.len();
        let mut pairs = Vec::new();
        for n in 0..np {
            for m in 0..np {
                if n != m {
                    let a = &self.pair_accum[n * np + m];
                    pairs.push(PairMasking {
                        maskee: names[n].clone(),
                        masker: names[m].clone(),
                        masked_fraction: a.masked_fraction(),
                        m_index: a.m_index(),
                    });
                }
            }
        }
        let tracks = self
            .tracks
            .iter()
            .map(|t| {
                let p = &job.tracks[t.pos];
                TrackMetrics {
                    index: p.index,
                    name: p.name.clone(),
                    tags: job.tags.get(t.pos).cloned().unwrap_or_default(),
                    in_mix: t.audible,
                    level: t.meter.finish(),
                    masking: t.mask.as_ref().map(|m| m.finish()),
                }
            })
            .collect();
        Measurement {
            schema: MEASURE_SCHEMA.to_string(),
            range,
            sample_rate: self.sample_rate,
            frame_seconds: fr.n as f64 / self.sample_rate as f64,
            bands_hz: fr.bands.nominal.clone(),
            level_point: self.point,
            mix,
            tracks,
            pairs,
        }
    }
}

/// The measurement pass driven from outside: push interleaved source
/// frames, then [`finish`](Self::finish). [`measure`] drives it from a file;
/// `channel_scaling` drives it from synthetic blocks, and a browser could
/// drive it from `Blob` slices the way [`crate::render::MixScan`] is.
pub struct MeasureStream<'a> {
    jobs: Vec<MeasureJob<'a>>,
    measurers: Vec<Measurer>,
    framing: Framing,
    scratch: Scratch,
    sample_rate: u32,
    start_frame: u64,
    done: u64,
}

impl<'a> MeasureStream<'a> {
    /// `start_frame` only labels the range in the result; the caller pushes
    /// the frames from there on.
    pub fn new(
        jobs: &[MeasureJob<'a>],
        channels: usize,
        sample_rate: u32,
        start_frame: u64,
        opts: &MeasureOptions,
    ) -> Result<MeasureStream<'a>> {
        if jobs.is_empty() {
            return Err(EngineError::InvalidRequest("nothing to measure".into()));
        }
        let framing = Framing::new(sample_rate);
        let nb = framing.bands.len();
        let measurers = jobs
            .iter()
            .map(|j| Measurer::new(j, channels, sample_rate, opts, nb))
            .collect::<Result<Vec<_>>>()?;
        let nbins = framing.n / 2 + 1;
        Ok(MeasureStream {
            jobs: jobs.to_vec(),
            measurers,
            scratch: Scratch {
                frame: vec![0.0; framing.n],
                spec: vec![Complex::new(0.0, 0.0); nbins],
                mix_l: Vec::new(),
                mix_r: Vec::new(),
                pow: vec![0.0; nbins],
                pow_rest: vec![0.0; nbins],
                eq_out: Vec::new(),
                level_buf: Vec::new(),
            },
            framing,
            sample_rate,
            start_frame,
            done: 0,
        })
    }

    /// Interleaved source frames, `channels` samples each.
    ///
    /// Worked through in sub-blocks of [`SUB_BLOCK_FRAMES`]: every measured
    /// track reads its own channel out of the interleaved block, and over a
    /// whole 65 536-frame block that strided walk is a cache miss per sample
    /// per track — at 34 channels 17 MB per track and block. A sub-block
    /// stays in cache for all tracks (the AGENTS.md rule about metering
    /// silent channels, for the same reason).
    pub fn push_frames(&mut self, input: &[f64]) {
        let channels = self.measurers[0].channels;
        for sub in input.chunks(SUB_BLOCK_FRAMES * channels) {
            for m in &mut self.measurers {
                m.push(sub, &self.framing, &mut self.scratch);
            }
            self.done += (sub.len() / channels) as u64;
        }
    }

    pub fn frames_done(&self) -> u64 {
        self.done
    }

    /// One [`Measurement`] per job, in job order.
    pub fn finish(self) -> Vec<Measurement> {
        let sr = self.sample_rate as f64;
        let range = Range {
            start: self.start_frame as f64 / sr,
            end: (self.start_frame + self.done) as f64 / sr,
        };
        let framing = self.framing;
        self.measurers
            .into_iter()
            .zip(&self.jobs)
            .map(|(m, j)| m.finish(j, &framing, range))
            .collect()
    }
}

/// Measure one or more mixes of the same take in a single read of the
/// trimmed range (taken from the first job's settings; every job must use
/// the same one). `progress` gets 0..=1.
pub fn measure(
    input: &InputHandle,
    jobs: &[MeasureJob],
    opts: &MeasureOptions,
    mut progress: impl FnMut(f32),
) -> Result<Vec<Measurement>> {
    let first = jobs
        .first()
        .ok_or_else(|| EngineError::InvalidRequest("nothing to measure".into()))?;
    for j in jobs {
        if (j.settings.trim_start_frame, j.settings.trim_end_frame)
            != (
                first.settings.trim_start_frame,
                first.settings.trim_end_frame,
            )
        {
            return Err(EngineError::InvalidRequest(
                "mixes measured together must share one range".into(),
            ));
        }
    }
    let mut reader = input.open()?;
    let spec = reader.spec();
    let num_frames = reader.num_frames();
    let start = first.settings.trim_start_frame.min(num_frames);
    let end = first
        .settings
        .trim_end_frame
        .unwrap_or(num_frames)
        .clamp(start, num_frames);
    let range_frames = end - start;
    let mut stream =
        MeasureStream::new(jobs, spec.channels as usize, spec.sample_rate, start, opts)?;
    reader.seek_to_frame(start)?;
    let mut buf = Vec::new();
    let scale = range_frames.max(1) as f64;
    while stream.frames_done() < range_frames {
        let want = BLOCK_FRAMES.min((range_frames - stream.frames_done()) as usize);
        if reader.read_frames(&mut buf, want)? == 0 {
            break;
        }
        stream.push_frames(&buf);
        progress((stream.frames_done() as f64 / scale) as f32);
    }
    progress(1.0);
    Ok(stream.finish())
}

// ── compare ─────────────────────────────────────────────────────────────────

/// `b − a` for every scalar and band value; `null` where either side is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LevelDelta {
    #[cfg_attr(feature = "schema", schemars(with = "Option<f64>"))]
    pub lufs_i: f64,
    #[cfg_attr(feature = "schema", schemars(with = "Option<f64>"))]
    pub lra: f64,
    #[cfg_attr(feature = "schema", schemars(with = "Option<f64>"))]
    pub true_peak_dbtp: f64,
    #[cfg_attr(feature = "schema", schemars(with = "Option<f64>"))]
    pub plr_db: f64,
    #[cfg_attr(feature = "schema", schemars(with = "Option<f64>"))]
    pub crest_db: f64,
    #[cfg_attr(feature = "schema", schemars(with = "Option<f64>"))]
    pub short_term_p50: f64,
    #[cfg_attr(feature = "schema", schemars(with = "Vec<Option<f64>>"))]
    pub ltas_db: Vec<f64>,
    pub active_fraction: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MaskingDelta {
    pub msr_db: Vec<Option<f64>>,
    pub masked_time_fraction: Vec<Option<f64>>,
    #[cfg_attr(feature = "schema", schemars(with = "Option<f64>"))]
    pub masked_fraction: f64,
    #[cfg_attr(feature = "schema", schemars(with = "Option<f64>"))]
    pub m_index: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TrackDelta {
    pub index: u32,
    pub name: String,
    #[serde(flatten)]
    pub level: LevelDelta,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub masking: Option<MaskingDelta>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MixDelta {
    #[serde(flatten)]
    pub level: LevelDelta,
    #[cfg_attr(feature = "schema", schemars(with = "Option<f64>"))]
    pub correlation: f64,
    #[cfg_attr(feature = "schema", schemars(with = "Option<f64>"))]
    pub width: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Comparison {
    pub schema: String,
    pub range: Range,
    pub bands_hz: Vec<f64>,
    pub delta: ComparisonDelta,
    pub a: Measurement,
    pub b: Measurement,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ComparisonDelta {
    pub mix: MixDelta,
    /// Tracks measured in both, matched by interleave index.
    pub tracks: Vec<TrackDelta>,
}

/// `b − a`, treating two equal infinities (silence in both) as no change.
fn d(a: f64, b: f64) -> f64 {
    if a == b {
        0.0
    } else {
        b - a
    }
}

fn level_delta(a: &LevelMetrics, b: &LevelMetrics) -> LevelDelta {
    LevelDelta {
        lufs_i: d(a.lufs_i, b.lufs_i),
        lra: d(a.lra, b.lra),
        true_peak_dbtp: d(a.true_peak_dbtp, b.true_peak_dbtp),
        plr_db: d(a.plr_db, b.plr_db),
        crest_db: d(a.crest_db, b.crest_db),
        short_term_p50: d(a.short_term_lufs.p50, b.short_term_lufs.p50),
        ltas_db: a
            .ltas_db
            .iter()
            .zip(&b.ltas_db)
            .map(|(x, y)| d(*x, *y))
            .collect(),
        active_fraction: b.active_fraction - a.active_fraction,
    }
}

fn opt_delta(a: &[Option<f64>], b: &[Option<f64>]) -> Vec<Option<f64>> {
    a.iter()
        .zip(b)
        .map(|(x, y)| match (x, y) {
            (Some(x), Some(y)) => Some(y - x),
            _ => None,
        })
        .collect()
}

/// Differences of two measurements of the same take and range.
pub fn compare(a: Measurement, b: Measurement) -> Result<Comparison> {
    if a.range != b.range || a.bands_hz != b.bands_hz {
        return Err(EngineError::InvalidRequest(
            "only measurements of the same range can be compared".into(),
        ));
    }
    let tracks = a
        .tracks
        .iter()
        .filter_map(|ta| {
            let tb = b.tracks.iter().find(|t| t.index == ta.index)?;
            Some(TrackDelta {
                index: ta.index,
                name: tb.name.clone(),
                level: level_delta(&ta.level, &tb.level),
                masking: match (&ta.masking, &tb.masking) {
                    (Some(x), Some(y)) => Some(MaskingDelta {
                        msr_db: opt_delta(&x.msr_db, &y.msr_db),
                        masked_time_fraction: opt_delta(
                            &x.masked_time_fraction,
                            &y.masked_time_fraction,
                        ),
                        masked_fraction: y.masked_fraction - x.masked_fraction,
                        m_index: y.m_index - x.m_index,
                    }),
                    _ => None,
                },
            })
        })
        .collect();
    Ok(Comparison {
        schema: COMPARE_SCHEMA.to_string(),
        range: a.range,
        bands_hz: a.bands_hz.clone(),
        delta: ComparisonDelta {
            mix: MixDelta {
                level: level_delta(&a.mix.level, &b.mix.level),
                correlation: b.mix.correlation - a.mix.correlation,
                width: b.mix.width - a.mix.width,
            },
            tracks,
        },
        a,
        b,
    })
}
