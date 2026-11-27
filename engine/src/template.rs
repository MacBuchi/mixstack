//! Mix templates — `mixstack.mix/1`, a mix that outlives one recording.
//!
//! A [`Session`] belongs to one take: it carries trim, fades and the output
//! format along with the mix, and it lives in the app container under a key
//! derived from the take's path. A template is the part of it that carries
//! over to the *next* rehearsal: per-track faders, pan, EQ and the master
//! settings. App, CLI and MCP server read and write the same format, and
//! other tools (the band player) consume it, so it is a public contract —
//! `docs/TEMPLATE.md` describes it for them.
//!
//! **Nothing here derives anything from a track name or a tag.** Names are
//! free text from the recording interface's configuration (see
//! [`Session::from_track_info`]); the only use of a name is to find the same
//! track again in the next take. `tags` are roles set by a human or a
//! configuration ("vocals", "monitor") and are carried through untouched.
//!
//! Applying a template to a take maps its tracks in three rounds — exact
//! name, then a normalised name (case and whitespace only), then the
//! interleave index — and returns a [`MappingReport`] that says which round
//! found each track, which tracks of the take the template did not know and
//! which template tracks the take did not have. There is deliberately no
//! fuzzy matching: a wrong match moves a fader on the wrong instrument, and
//! nothing in the report would make that visible.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{EngineError, Result};
use crate::ixml::{default_pan_for_name, TrackInfo};
use crate::mix::{TrackEq, TrackParams};
use crate::render::{LoudnessMode, MasteringSettings, RenderSettings};
use crate::session::{Session, SESSION_VERSION};

/// Schema identifier written into every template. The major version after
/// the slash changes only for incompatible changes; readers refuse a major
/// they do not know rather than guess.
pub const TEMPLATE_SCHEMA: &str = "mixstack.mix/1";

const SCHEMA_PREFIX: &str = "mixstack.mix/";
const SCHEMA_MAJOR: &str = "1";

/// Where a template came from — informational only, never used for mapping.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TemplateSource {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub take: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
}

/// One track of a template: the fields of [`TrackParams`] under the same
/// serde names, plus free `tags`. Every field has a default, so a template
/// written by an older or a third-party tool that leaves fields out still
/// loads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TemplateTrack {
    pub name: String,
    /// 1-based interleave index in the take the template was made from.
    /// Only the last mapping round uses it; `0` means "unknown".
    pub index: u32,
    pub gain_db: f64,
    pub pan: f64,
    pub polarity_invert: bool,
    pub muted: bool,
    pub solo: bool,
    pub in_mix: bool,
    pub eq: TrackEq,
    /// Free roles ("vocals", "monitor") — data for whoever reads the
    /// template, never interpreted by the engine.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

impl Default for TemplateTrack {
    fn default() -> Self {
        Self {
            name: String::new(),
            index: 0,
            gain_db: 0.0,
            pan: 0.0,
            polarity_invert: false,
            muted: false,
            solo: false,
            in_mix: true,
            eq: TrackEq::default(),
            tags: Vec::new(),
        }
    }
}

impl TemplateTrack {
    fn from_params(t: &TrackParams, tags: Vec<String>) -> Self {
        Self {
            name: t.name.clone(),
            index: t.index,
            gain_db: t.gain_db,
            pan: t.pan,
            polarity_invert: t.polarity_invert,
            muted: t.muted,
            solo: t.solo,
            in_mix: t.in_mix,
            eq: t.eq,
            tags,
        }
    }

    /// The parameters for a track of the take this template track was
    /// matched to: the take's index and name, the template's mix.
    fn to_params(&self, info: &TrackInfo) -> TrackParams {
        TrackParams {
            index: info.index,
            name: info.name.clone(),
            gain_db: self.gain_db,
            pan: self.pan,
            polarity_invert: self.polarity_invert,
            muted: self.muted,
            solo: self.solo,
            in_mix: self.in_mix,
            eq: self.eq,
        }
    }
}

/// The master part of [`RenderSettings`] that belongs to a mix rather than
/// to one export. Trim, fades and the output format are per render and are
/// not stored here; dither only means something for a 16-bit target, which
/// is also the render's choice.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TemplateSettings {
    pub loudness: LoudnessMode,
    pub limiter_enabled: bool,
    pub ceiling_dbtp: f64,
    pub mastering: Option<MasteringSettings>,
}

impl Default for TemplateSettings {
    fn default() -> Self {
        let r = RenderSettings::default();
        Self {
            loudness: r.loudness,
            limiter_enabled: r.limiter_enabled,
            ceiling_dbtp: r.ceiling_dbtp,
            mastering: r.mastering,
        }
    }
}

impl TemplateSettings {
    fn from_render(s: &RenderSettings) -> Self {
        Self {
            loudness: s.loudness,
            limiter_enabled: s.limiter_enabled,
            ceiling_dbtp: s.ceiling_dbtp,
            mastering: s.mastering.clone(),
        }
    }

    /// Overwrite the mix-level fields of `s`, leaving the per-render ones
    /// (trim, fades, format, dither) as the caller set them.
    pub fn apply_to(&self, s: &mut RenderSettings) {
        s.loudness = self.loudness;
        s.limiter_enabled = self.limiter_enabled;
        s.ceiling_dbtp = self.ceiling_dbtp;
        s.mastering = self.mastering.clone();
    }
}

/// A `mixstack.mix/1` document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Template {
    pub schema: String,
    #[serde(default)]
    pub name: String,
    /// RFC 3339 timestamp, set by whoever writes the file (the engine has
    /// no clock of its own to consult).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<TemplateSource>,
    #[serde(default)]
    pub tracks: Vec<TemplateTrack>,
    #[serde(default)]
    pub settings: TemplateSettings,
}

/// What happens to a track of the take that no template track maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UnknownTracks {
    /// Out of the mix (`in_mix: false`). The default for templates: a new
    /// channel in the take is as likely a monitor feed as a source, and a
    /// mix that is too quiet in one place is easier to notice and fix than
    /// one with an unexpected in-ear bus — click track included — on top.
    #[default]
    Exclude,
    /// In the mix at unity, centred (or hard left/right for an `L`/`R`
    /// pair, like a fresh session).
    Include,
}

/// Which mapping round matched a track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MatchedBy {
    Name,
    /// Same name after [`normalize_name`] — reported separately because it
    /// is the only round that treats two different strings as one track.
    Normalized,
    Index,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MatchedTrack {
    /// The track's name in the take.
    pub name: String,
    /// The track's interleave index in the take.
    pub index: u32,
    pub by: MatchedBy,
    /// The name the template knew it under — differs from `name` for a
    /// normalised or an index match, which is exactly the case a reader of
    /// the report wants to check.
    pub template_name: String,
}

/// The answer to "what did applying this template actually do", returned
/// rather than logged so a caller (the band player, an agent) can act on it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct MappingReport {
    /// Tracks of the take that got the template's parameters, in take order.
    pub matched: Vec<MatchedTrack>,
    /// Tracks of the take the template had no entry for, in take order.
    pub unknown: Vec<String>,
    /// Template tracks the take does not have, in template order.
    pub missing: Vec<String>,
}

impl MappingReport {
    /// True when not a single template track found a home — the template
    /// belongs to a different setup and rendering it would mean rendering
    /// only defaults.
    pub fn matched_nothing(&self) -> bool {
        self.matched.is_empty()
    }
}

/// The result of [`apply_template`].
#[derive(Debug, Clone, PartialEq)]
pub struct AppliedTemplate {
    /// A session for the take, tracks in take order, ready to render. Its
    /// per-render settings are [`RenderSettings::default`]'s; the caller
    /// sets trim, fades and format.
    pub session: Session,
    pub report: MappingReport,
    /// Tags of each track, parallel to `session.tracks` (empty for unknown
    /// tracks). [`Session`] has no place for them, but a later
    /// [`Template::from_session_with_tags`] must not lose them.
    pub tags: Vec<Vec<String>>,
}

/// Case-folded, whitespace-collapsed form of a track name. The only
/// normalisation the mapping does: `"Bass  Out"` and `"bass out"` are the
/// same track, `"Bass_Out"` and `"Bass Out"` are not.
pub fn normalize_name(name: &str) -> String {
    name.split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

impl Template {
    /// A template from a session: its tracks and its mix-level settings,
    /// without trim, fades or format. No tags — a session carries none.
    pub fn from_session(session: &Session) -> Template {
        Self::from_session_with_tags(session, &[])
    }

    /// Like [`Template::from_session`], with `tags` parallel to
    /// `session.tracks` (shorter is fine; missing entries mean no tags).
    pub fn from_session_with_tags(session: &Session, tags: &[Vec<String>]) -> Template {
        Template {
            schema: TEMPLATE_SCHEMA.to_string(),
            name: String::new(),
            created: None,
            source: None,
            tracks: session
                .tracks
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    TemplateTrack::from_params(t, tags.get(i).cloned().unwrap_or_default())
                })
                .collect(),
            settings: TemplateSettings::from_render(&session.settings),
        }
    }

    /// Parse a template. Refuses a document without a `schema` or with a
    /// major version this engine does not know; unknown fields are ignored,
    /// missing ones take their defaults.
    pub fn from_json(data: &str) -> Result<Template> {
        let t: Template = serde_json::from_str(data)?;
        check_schema(&t.schema)?;
        Ok(t)
    }

    /// Parse either a template or a stored [`Session`] (`.durecmix.json`):
    /// a session is turned into a template on the way in, so both go
    /// through the same mapping and produce the same report.
    pub fn from_template_or_session_json(data: &str) -> Result<Template> {
        let value: serde_json::Value = serde_json::from_str(data)?;
        if value.get("schema").is_some() {
            let t: Template = serde_json::from_value(value)?;
            check_schema(&t.schema)?;
            Ok(t)
        } else if value.get("version").is_some() && value.get("tracks").is_some() {
            let s: Session = serde_json::from_value(value)?;
            Ok(Template::from_session(&s))
        } else {
            Err(EngineError::Schema(
                "neither a mixstack.mix template (no \"schema\") nor a session (no \"version\")"
                    .into(),
            ))
        }
    }

    pub fn load(path: &Path) -> Result<Template> {
        Template::from_template_or_session_json(&std::fs::read_to_string(path)?)
    }

    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, self.to_json()?)?;
        Ok(())
    }
}

/// Pop the first template position from `queue` that no earlier round claimed.
fn take_from(queue: Option<&mut Vec<usize>>, claimed: &mut [bool]) -> Option<usize> {
    let queue = queue?;
    while let Some(pos) = queue.pop() {
        if !claimed[pos] {
            claimed[pos] = true;
            return Some(pos);
        }
    }
    None
}

fn check_schema(schema: &str) -> Result<()> {
    match schema.strip_prefix(SCHEMA_PREFIX) {
        Some(SCHEMA_MAJOR) => Ok(()),
        Some(other) => Err(EngineError::Schema(format!(
            "template schema version {other} is not supported (this engine reads {TEMPLATE_SCHEMA})"
        ))),
        None => Err(EngineError::Schema(format!(
            "\"{schema}\" is not a mixstack.mix schema"
        ))),
    }
}

/// Map `template` onto the tracks of a take.
///
/// Three rounds, each only over what the previous ones left unclaimed: exact
/// name, then [`normalize_name`], then interleave index. A template track is
/// used at most once, so a renamed track found by index cannot also pull
/// in a second track of the take. Duplicate names are paired up in order.
///
/// Every round is a hash lookup per track, so the whole mapping is linear in
/// the channel count.
pub fn apply_template(
    template: &Template,
    take: &[TrackInfo],
    unknown: UnknownTracks,
) -> AppliedTemplate {
    let n = template.tracks.len();
    let mut claimed = vec![false; n];
    // Template position for each take track, once found.
    let mut found: Vec<Option<(usize, MatchedBy)>> = vec![None; take.len()];

    // Name → queue of template positions, consumed front to back.
    let mut by_name: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut by_norm: HashMap<String, Vec<usize>> = HashMap::new();
    let mut by_index: HashMap<u32, Vec<usize>> = HashMap::new();
    for (pos, t) in template.tracks.iter().enumerate().rev() {
        by_name.entry(t.name.as_str()).or_default().push(pos);
        by_norm
            .entry(normalize_name(&t.name))
            .or_default()
            .push(pos);
        if t.index != 0 {
            by_index.entry(t.index).or_default().push(pos);
        }
    }

    for (i, info) in take.iter().enumerate() {
        if let Some(pos) = take_from(by_name.get_mut(info.name.as_str()), &mut claimed) {
            found[i] = Some((pos, MatchedBy::Name));
        }
    }
    for (i, info) in take.iter().enumerate() {
        if found[i].is_none() {
            if let Some(pos) = take_from(by_norm.get_mut(&normalize_name(&info.name)), &mut claimed)
            {
                found[i] = Some((pos, MatchedBy::Normalized));
            }
        }
    }
    for (i, info) in take.iter().enumerate() {
        if found[i].is_none() {
            if let Some(pos) = take_from(by_index.get_mut(&info.index), &mut claimed) {
                found[i] = Some((pos, MatchedBy::Index));
            }
        }
    }

    let mut report = MappingReport::default();
    let mut tracks = Vec::with_capacity(take.len());
    let mut tags = Vec::with_capacity(take.len());
    for (info, hit) in take.iter().zip(&found) {
        match *hit {
            Some((pos, by)) => {
                let t = &template.tracks[pos];
                tracks.push(t.to_params(info));
                tags.push(t.tags.clone());
                report.matched.push(MatchedTrack {
                    name: info.name.clone(),
                    index: info.index,
                    by,
                    template_name: t.name.clone(),
                });
            }
            None => {
                let mut p = TrackParams::new(
                    info.index,
                    info.name.clone(),
                    default_pan_for_name(&info.name),
                );
                p.in_mix = unknown == UnknownTracks::Include;
                tracks.push(p);
                tags.push(Vec::new());
                report.unknown.push(info.name.clone());
            }
        }
    }
    report.missing = template
        .tracks
        .iter()
        .zip(&claimed)
        .filter(|(_, c)| !**c)
        .map(|(t, _)| t.name.clone())
        .collect();

    let mut settings = RenderSettings::default();
    template.settings.apply_to(&mut settings);
    AppliedTemplate {
        session: Session {
            version: SESSION_VERSION,
            tracks,
            settings,
        },
        report,
        tags,
    }
}
