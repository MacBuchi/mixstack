//! Editing a mix by operations, with revisions — the engine side of the MCP
//! server's `update_mix` / `revert_mix` (#167).
//!
//! An agent changes a mix in small, named steps (`set_gain`, `set_eq`, …)
//! and keeps or reverts each one after measuring. The operations and their
//! **technical** ranges live here, not in the server, because they are the
//! same ranges the app's controls have: what an agent may set is what a
//! person at the console could set, no more. Tighter, policy-style limits
//! ("at most ±6 dB of EQ per step") are a caller's business and come in as
//! [`EditLimits`].
//!
//! A batch of operations is atomic: every operation is validated against
//! the state the previous ones produced, and if one fails the mix is left
//! exactly as it was. Revisions are whole snapshots — a mix is a few hundred
//! numbers, and a snapshot makes revert trivially exact.

use serde::{Deserialize, Serialize};

use crate::error::{EngineError, Result};
use crate::mix::{HpfSlope, TrackParams};
use crate::session::Session;

/// The ranges of the app's controls (`lib/ui/track_strip.dart`).
pub mod ranges {
    pub const GAIN_DB: (f64, f64) = (-60.0, 6.0);
    pub const PAN: (f64, f64) = (-1.0, 1.0);
    pub const EQ_GAIN_DB: (f64, f64) = (-15.0, 15.0);
    pub const HPF_HZ: (f64, f64) = (20.0, 500.0);
    pub const LOW_HZ: (f64, f64) = (40.0, 500.0);
    pub const MID_HZ: (f64, f64) = (200.0, 8000.0);
    pub const HIGH_HZ: (f64, f64) = (1500.0, 16000.0);
    /// The app keeps Q fixed per band; this is the range the biquads are
    /// well-behaved over, for callers that set it.
    pub const Q: (f64, f64) = (0.1, 10.0);
}

/// A track, by exact name or by 1-based interleave index.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum TrackRef {
    Index(u32),
    Name(String),
}

impl std::fmt::Display for TrackRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TrackRef::Index(i) => write!(f, "#{i}"),
            TrackRef::Name(n) => write!(f, "{n:?}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum EqBandName {
    Low,
    Mid,
    High,
}

/// One change to one track.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum MixOp {
    /// Fader, dB (−60 … +6; −60 is silence).
    SetGain {
        track: TrackRef,
        gain_db: f64,
    },
    /// Pan, −1 (left) … +1 (right), constant-power.
    SetPan {
        track: TrackRef,
        pan: f64,
    },
    /// One EQ band; fields left out keep their value. Setting `gain_db`
    /// enables the band unless `enabled` says otherwise.
    SetEq {
        track: TrackRef,
        band: EqBandName,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        enabled: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gain_db: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        freq: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        q: Option<f64>,
    },
    /// High-pass filter; fields left out keep their value. Setting `freq`
    /// enables it unless `enabled` says otherwise.
    SetHpf {
        track: TrackRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        enabled: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        freq: Option<f64>,
        /// 12 or 24 dB/octave.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        slope_db_per_octave: Option<u32>,
    },
    SetInMix {
        track: TrackRef,
        in_mix: bool,
    },
    SetMute {
        track: TrackRef,
        muted: bool,
    },
    SetSolo {
        track: TrackRef,
        solo: bool,
    },
    SetPolarity {
        track: TrackRef,
        invert: bool,
    },
}

impl MixOp {
    pub fn track(&self) -> &TrackRef {
        match self {
            MixOp::SetGain { track, .. }
            | MixOp::SetPan { track, .. }
            | MixOp::SetEq { track, .. }
            | MixOp::SetHpf { track, .. }
            | MixOp::SetInMix { track, .. }
            | MixOp::SetMute { track, .. }
            | MixOp::SetSolo { track, .. }
            | MixOp::SetPolarity { track, .. } => track,
        }
    }
}

/// Optional caller policy on top of the technical ranges — e.g. the KI-Mix
/// rule "at most ±6 dB of EQ per step". Every field is optional; an absent
/// one imposes nothing.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(default, deny_unknown_fields)]
pub struct EditLimits {
    /// Fader range, dB, narrower than −60 … +6.
    pub gain_db: Option<(f64, f64)>,
    /// Largest fader change of one operation, dB.
    pub max_gain_step_db: Option<f64>,
    /// Largest |EQ gain| a band may be set to, dB.
    pub max_eq_gain_db: Option<f64>,
    /// Largest EQ gain change of one operation, dB.
    pub max_eq_step_db: Option<f64>,
    /// Largest pan change of one operation.
    pub max_pan_step: Option<f64>,
}

/// What one operation did, for the caller's log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AppliedOp {
    pub index: u32,
    pub name: String,
    pub change: String,
}

fn invalid(msg: String) -> EngineError {
    EngineError::InvalidRequest(msg)
}

fn in_range(what: &str, v: f64, (lo, hi): (f64, f64)) -> Result<()> {
    if v.is_finite() && v >= lo && v <= hi {
        Ok(())
    } else {
        Err(invalid(format!("{what} {v} is outside {lo} … {hi}")))
    }
}

fn step(what: &str, from: f64, to: f64, max: Option<f64>) -> Result<()> {
    match max {
        Some(m) if (to - from).abs() > m + 1e-9 => Err(invalid(format!(
            "{what} changes by {:.2}, more than the allowed {m} per step",
            to - from
        ))),
        _ => Ok(()),
    }
}

fn find(tracks: &[TrackParams], r: &TrackRef) -> Result<usize> {
    let hit = match r {
        TrackRef::Index(i) => tracks.iter().position(|t| t.index == *i),
        TrackRef::Name(n) => {
            let mut it = tracks.iter().enumerate().filter(|(_, t)| &t.name == n);
            let first = it.next().map(|(p, _)| p);
            if first.is_some() && it.next().is_some() {
                return Err(invalid(format!(
                    "{r} names more than one track — use its index"
                )));
            }
            first
        }
    };
    hit.ok_or_else(|| invalid(format!("no track {r}")))
}

fn apply_one(t: &mut TrackParams, op: &MixOp, limits: &EditLimits) -> Result<String> {
    Ok(match op {
        MixOp::SetGain { gain_db, .. } => {
            in_range("gain_db", *gain_db, ranges::GAIN_DB)?;
            if let Some(r) = limits.gain_db {
                in_range("gain_db", *gain_db, r)?;
            }
            step("gain_db", t.gain_db, *gain_db, limits.max_gain_step_db)?;
            let c = format!("gain {:+.2} → {:+.2} dB", t.gain_db, gain_db);
            t.gain_db = *gain_db;
            c
        }
        MixOp::SetPan { pan, .. } => {
            in_range("pan", *pan, ranges::PAN)?;
            step("pan", t.pan, *pan, limits.max_pan_step)?;
            let c = format!("pan {:+.2} → {:+.2}", t.pan, pan);
            t.pan = *pan;
            c
        }
        MixOp::SetEq {
            band,
            enabled,
            gain_db,
            freq,
            q,
            ..
        } => {
            let (b, fr) = match band {
                EqBandName::Low => (&mut t.eq.low, ranges::LOW_HZ),
                EqBandName::Mid => (&mut t.eq.mid, ranges::MID_HZ),
                EqBandName::High => (&mut t.eq.high, ranges::HIGH_HZ),
            };
            if let Some(g) = gain_db {
                in_range("eq gain_db", *g, ranges::EQ_GAIN_DB)?;
                if let Some(m) = limits.max_eq_gain_db {
                    in_range("eq gain_db", *g, (-m, m))?;
                }
                step("eq gain_db", b.gain_db, *g, limits.max_eq_step_db)?;
            }
            if let Some(f) = freq {
                in_range("eq freq", *f, fr)?;
            }
            if let Some(q) = q {
                in_range("eq q", *q, ranges::Q)?;
            }
            let before = *b;
            if let Some(g) = gain_db {
                b.gain_db = *g;
            }
            if let Some(f) = freq {
                b.freq = *f;
            }
            if let Some(q) = q {
                b.q = *q;
            }
            b.enabled = enabled.unwrap_or(before.enabled || gain_db.is_some());
            format!(
                "eq {band:?} {} {:.0} Hz {:+.1} dB Q {:.2} → {} {:.0} Hz {:+.1} dB Q {:.2}",
                if before.enabled { "on" } else { "off" },
                before.freq,
                before.gain_db,
                before.q,
                if b.enabled { "on" } else { "off" },
                b.freq,
                b.gain_db,
                b.q
            )
        }
        MixOp::SetHpf {
            enabled,
            freq,
            slope_db_per_octave,
            ..
        } => {
            if let Some(f) = freq {
                in_range("hpf freq", *f, ranges::HPF_HZ)?;
            }
            let slope = match slope_db_per_octave {
                None => None,
                Some(12) => Some(HpfSlope::Db12),
                Some(24) => Some(HpfSlope::Db24),
                Some(o) => return Err(invalid(format!("hpf slope {o} is not 12 or 24"))),
            };
            let was = (t.eq.hpf_enabled, t.eq.hpf_freq);
            if let Some(f) = freq {
                t.eq.hpf_freq = *f;
            }
            if let Some(s) = slope {
                t.eq.hpf_slope = s;
            }
            t.eq.hpf_enabled = enabled.unwrap_or(was.0 || freq.is_some());
            format!(
                "hpf {} {:.0} Hz → {} {:.0} Hz",
                if was.0 { "on" } else { "off" },
                was.1,
                if t.eq.hpf_enabled { "on" } else { "off" },
                t.eq.hpf_freq
            )
        }
        MixOp::SetInMix { in_mix, .. } => {
            t.in_mix = *in_mix;
            format!("in_mix {in_mix}")
        }
        MixOp::SetMute { muted, .. } => {
            t.muted = *muted;
            format!("muted {muted}")
        }
        MixOp::SetSolo { solo, .. } => {
            t.solo = *solo;
            format!("solo {solo}")
        }
        MixOp::SetPolarity { invert, .. } => {
            t.polarity_invert = *invert;
            format!("polarity_invert {invert}")
        }
    })
}

/// Apply `ops` in order to a copy of `session`; on any error the original
/// stays untouched and the error names the failing operation.
pub fn apply_ops(
    session: &Session,
    ops: &[MixOp],
    limits: &EditLimits,
) -> Result<(Session, Vec<AppliedOp>)> {
    if ops.is_empty() {
        return Err(invalid("no operations given".into()));
    }
    let mut next = session.clone();
    let mut log = Vec::with_capacity(ops.len());
    for (i, op) in ops.iter().enumerate() {
        let wrap = |e: EngineError| match e {
            EngineError::InvalidRequest(m) => {
                invalid(format!("operation {i} ({}): {m}", op.track()))
            }
            other => other,
        };
        let pos = find(&next.tracks, op.track()).map_err(wrap)?;
        let t = &mut next.tracks[pos];
        let change = apply_one(t, op, limits).map_err(wrap)?;
        log.push(AppliedOp {
            index: t.index,
            name: t.name.clone(),
            change,
        });
    }
    Ok((next, log))
}

/// One state of a mix.
#[derive(Debug, Clone, PartialEq)]
pub struct Revision {
    pub session: Session,
    /// Parallel to `session.tracks`.
    pub tags: Vec<Vec<String>>,
    /// What produced it, for a caller's log.
    pub note: String,
}

/// The linear history of one mix. Reverting appends a copy of the old
/// revision rather than cutting the history, so nothing is ever lost and
/// revision numbers only grow.
#[derive(Debug, Clone, PartialEq)]
pub struct MixHistory {
    revisions: Vec<Revision>,
}

impl MixHistory {
    pub fn new(session: Session, tags: Vec<Vec<String>>, note: impl Into<String>) -> MixHistory {
        MixHistory {
            revisions: vec![Revision {
                session,
                tags,
                note: note.into(),
            }],
        }
    }

    /// The newest revision number (the first is 0).
    pub fn current(&self) -> u32 {
        (self.revisions.len() - 1) as u32
    }

    pub fn get(&self, revision: u32) -> Result<&Revision> {
        self.revisions.get(revision as usize).ok_or_else(|| {
            invalid(format!(
                "no revision {revision} (this mix has 0 … {})",
                self.current()
            ))
        })
    }

    pub fn latest(&self) -> &Revision {
        self.revisions.last().expect("a history is never empty")
    }

    /// Apply `ops` to the newest revision and append the result.
    pub fn update(&mut self, ops: &[MixOp], limits: &EditLimits) -> Result<(u32, Vec<AppliedOp>)> {
        let base = self.latest();
        let (session, log) = apply_ops(&base.session, ops, limits)?;
        let tags = base.tags.clone();
        self.revisions.push(Revision {
            session,
            tags,
            note: format!("{} operation(s)", ops.len()),
        });
        Ok((self.current(), log))
    }

    /// Append a copy of `revision` as the newest one.
    pub fn revert(&mut self, revision: u32) -> Result<u32> {
        let old = self.get(revision)?.clone();
        self.revisions.push(Revision {
            note: format!("revert to {revision}"),
            ..old
        });
        Ok(self.current())
    }
}
