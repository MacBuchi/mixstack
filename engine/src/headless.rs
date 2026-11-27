//! Headless entry points: probe a take, apply a template, render — the
//! engine side of the `mixstack` CLI and its MCP server (#163).
//!
//! Everything a front end without a UI needs lives here, so the CLI stays a
//! translation of arguments and JSON (like `rust/` is for the app) and the
//! MCP server reuses the same functions instead of a second copy:
//!
//! - which tracks a take has ([`open_take`], via
//!   [`ixml::tracks_or_channels`] — the same list the app starts from),
//! - how seconds become trim frames ([`apply_render_options`]),
//! - which reference profile a template's mastering settings imply
//!   ([`reference_profile`]),
//! - measurement and comparison of mixes ([`measure_template`],
//!   [`compare_templates`], over [`crate::measure`]),
//! - and the render itself, which is [`render_io`] — the app's export — with
//!   nothing in front of it but the safety checks of a tool that writes
//!   files on someone's disk ([`render_session_to`]).
//!
//! Rule 6 of AGENTS.md holds here as everywhere: no gain or normalisation
//! formula of its own. A render from here and an export from the app are
//! the same function on the same session, and `engine/tests` and
//! `cli/tests` prove it byte for byte.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::EngineError;
use crate::ixml::{self, TrackInfo};
use crate::mastering::{merge_profiles, ReferenceProfile};
use crate::measure::{
    compare, measure, Comparison, MeasureJob, MeasureOptions, Measurement, COMPARE_SCHEMA,
    MEASURE_SCHEMA,
};
use crate::reference::analyze_reference;
use crate::render::{render_io, LoudnessMode, OutputFormat, RenderReport, RenderSettings};
use crate::session::Session;
use crate::sink::{validate_format, OutputHandle};
use crate::template::{apply_template, AppliedTemplate, MappingReport, Template, UnknownTracks};
use crate::wav::{InputHandle, SampleFormat, WavSpec};

pub const PROBE_SCHEMA: &str = "mixstack.probe/1";
pub const RENDER_SCHEMA: &str = "mixstack.render/1";

/// What went wrong, sorted by who has to act — the CLI turns each kind into
/// its own exit code, the MCP server into a tool error the model can read.
#[derive(Debug, thiserror::Error)]
pub enum HeadlessError {
    /// The request itself is inconsistent (a range outside the take, an
    /// MP3 bitrate the encoder does not have). Fix the arguments.
    #[error("{0}")]
    Usage(String),
    /// A file that has to be read — the take, the mix, a mastering
    /// reference — could not be.
    #[error("cannot read {path}: {source}")]
    Input { path: String, source: EngineError },
    /// Not one track of the take matched the template: it belongs to a
    /// different setup, and rendering it would render only defaults.
    #[error("the mix matches no track of {0}")]
    NoMatch(String),
    /// The target cannot be written (exists, is the source, no permission).
    #[error("cannot write {path}: {reason}")]
    Output { path: String, reason: String },
    /// Anything else the engine reported during the render.
    #[error(transparent)]
    Engine(#[from] EngineError),
}

pub type Result<T> = std::result::Result<T, HeadlessError>;

fn input_err(path: &Path, source: EngineError) -> HeadlessError {
    HeadlessError::Input {
        path: path.display().to_string(),
        source,
    }
}

fn output_err(path: &Path, reason: impl Into<String>) -> HeadlessError {
    HeadlessError::Output {
        path: path.display().to_string(),
        reason: reason.into(),
    }
}

/// An opened (header-parsed) recording: everything a template is applied
/// against, without a byte of audio read.
#[derive(Debug, Clone)]
pub struct Take {
    pub path: PathBuf,
    pub spec: WavSpec,
    pub num_frames: u64,
    pub has_ixml: bool,
    pub tracks: Vec<TrackInfo>,
}

impl Take {
    pub fn duration_seconds(&self) -> f64 {
        self.num_frames as f64 / self.spec.sample_rate as f64
    }

    /// The take's file name without extension — what a template's
    /// `source.take` records.
    pub fn stem(&self) -> String {
        self.path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

/// Parse a take's headers and track list. Header-only: fast even for a
/// multi-GB RF64 file on slow media.
pub fn open_take(path: &Path) -> Result<Take> {
    let handle = InputHandle::Path(path.to_string_lossy().into_owned());
    let reader = handle.open().map_err(|e| input_err(path, e))?;
    let spec = reader.spec();
    Ok(Take {
        path: path.to_path_buf(),
        spec,
        num_frames: reader.num_frames(),
        has_ixml: reader.ixml().is_some(),
        tracks: ixml::tracks_or_channels(reader.ixml(), spec.channels),
    })
}

/// `mixstack.probe/1`.
#[derive(Debug, Clone, Serialize)]
pub struct ProbeOutput {
    pub schema: &'static str,
    pub path: String,
    pub channels: u16,
    pub sample_rate: u32,
    pub bits_per_sample: u16,
    /// `"int"` or `"float"`.
    pub sample_format: &'static str,
    pub frames: u64,
    pub duration_seconds: f64,
    /// False when the file carries no iXML; `tracks` are then generic
    /// `Channel n` entries.
    pub has_ixml: bool,
    pub tracks: Vec<TrackInfo>,
    pub engine: String,
}

pub fn probe_output(take: &Take, engine_version: &str) -> ProbeOutput {
    ProbeOutput {
        schema: PROBE_SCHEMA,
        path: take.path.display().to_string(),
        channels: take.spec.channels,
        sample_rate: take.spec.sample_rate,
        bits_per_sample: take.spec.bits_per_sample,
        sample_format: match take.spec.sample_format {
            SampleFormat::Int => "int",
            SampleFormat::Float => "float",
        },
        frames: take.num_frames,
        duration_seconds: take.duration_seconds(),
        has_ixml: take.has_ixml,
        tracks: take.tracks.clone(),
        engine: engine_version.to_string(),
    }
}

/// Load a template or a stored session from disk.
pub fn load_mix(path: &Path) -> Result<Template> {
    Template::load(path).map_err(|e| input_err(path, e))
}

/// Everything about one render that is not part of the mix.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderOptions {
    /// Trim-in in seconds from the start of the take.
    pub start_seconds: Option<f64>,
    /// Trim-out in seconds from the start of the take; past the end means
    /// the end.
    pub end_seconds: Option<f64>,
    pub fade_in_ms: f64,
    pub fade_out_ms: f64,
    /// Overrides the template's loudness setting when set.
    pub loudness: Option<LoudnessMode>,
    pub format: OutputFormat,
    pub mp3_bitrate_kbps: u32,
    pub unknown: UnknownTracks,
}

impl Default for RenderOptions {
    fn default() -> Self {
        let r = RenderSettings::default();
        Self {
            start_seconds: None,
            end_seconds: None,
            fade_in_ms: 0.0,
            fade_out_ms: 0.0,
            loudness: None,
            format: r.format,
            mp3_bitrate_kbps: r.mp3_bitrate_kbps,
            unknown: UnknownTracks::Exclude,
        }
    }
}

/// A position in seconds as a frame index: rounded to the nearest frame.
pub fn seconds_to_frame(seconds: f64, sample_rate: u32) -> u64 {
    (seconds * sample_rate as f64).round().max(0.0) as u64
}

/// Turn `opts` into trim frames, fades, format and loudness on `settings`.
///
/// The range is validated against the take: a start at or past the end, or
/// an end before the start, is a usage error rather than an empty file.
pub fn apply_render_options(
    settings: &mut RenderSettings,
    take: &Take,
    opts: &RenderOptions,
) -> Result<()> {
    let sr = take.spec.sample_rate;
    for (what, v) in [("start", opts.start_seconds), ("end", opts.end_seconds)] {
        if let Some(v) = v {
            if !v.is_finite() || v < 0.0 {
                return Err(HeadlessError::Usage(format!(
                    "{what} must be a non-negative number of seconds, got {v}"
                )));
            }
        }
    }
    for (what, v) in [("fade-in", opts.fade_in_ms), ("fade-out", opts.fade_out_ms)] {
        if !v.is_finite() || v < 0.0 {
            return Err(HeadlessError::Usage(format!(
                "{what} must be a non-negative number of milliseconds, got {v}"
            )));
        }
    }
    let start = opts.start_seconds.map_or(0, |s| seconds_to_frame(s, sr));
    let end = opts
        .end_seconds
        .map(|s| seconds_to_frame(s, sr).min(take.num_frames));
    if start >= take.num_frames {
        return Err(HeadlessError::Usage(format!(
            "start {:.3} s is at or past the end of the take ({:.3} s)",
            opts.start_seconds.unwrap_or(0.0),
            take.duration_seconds()
        )));
    }
    if let Some(e) = end {
        if e <= start {
            return Err(HeadlessError::Usage(format!(
                "end ({:.3} s) must lie after start ({:.3} s)",
                opts.end_seconds.unwrap_or(0.0),
                opts.start_seconds.unwrap_or(0.0)
            )));
        }
    }
    settings.trim_start_frame = start;
    settings.trim_end_frame = end.filter(|&e| e < take.num_frames);
    settings.fade_in_ms = opts.fade_in_ms;
    settings.fade_out_ms = opts.fade_out_ms;
    settings.format = opts.format;
    settings.mp3_bitrate_kbps = opts.mp3_bitrate_kbps;
    if let Some(l) = opts.loudness {
        settings.loudness = l;
    }
    validate_format(settings.format, sr, settings.mp3_bitrate_kbps)
        .map_err(|e| HeadlessError::Usage(e.to_string()))
}

/// Apply `template` to `take` and set the per-render options: the session a
/// render runs on. Refuses a template that matches no track at all.
pub fn prepare_session(
    template: &Template,
    take: &Take,
    opts: &RenderOptions,
) -> Result<AppliedTemplate> {
    let mut applied = apply_template(template, &take.tracks, opts.unknown);
    if applied.report.matched_nothing() {
        return Err(HeadlessError::NoMatch(take.path.display().to_string()));
    }
    apply_render_options(&mut applied.session.settings, take, opts)?;
    Ok(applied)
}

/// The reference profile the session's mastering settings ask for, or
/// `None` when mastering is off or has no reference. Several references
/// are merged the way the app merges them ([`merge_profiles`]).
///
/// Each reference is decoded and analysed — a few seconds per song. The app
/// caches profiles on its side; a headless caller that renders the same mix
/// often can hold on to the returned profile itself.
pub fn reference_profile(settings: &RenderSettings) -> Result<Option<ReferenceProfile>> {
    let Some(m) = settings.mastering.as_ref().filter(|m| m.enabled) else {
        return Ok(None);
    };
    let refs = m.normalized_references();
    if refs.is_empty() {
        return Ok(None);
    }
    let mut profiles = Vec::with_capacity(refs.len());
    for r in &refs {
        let path = Path::new(&r.path);
        profiles.push(
            analyze_reference(&InputHandle::Path(r.path.clone()), |_| {})
                .map_err(|e| input_err(path, e))?,
        );
    }
    Ok(Some(merge_profiles(&profiles)?))
}

/// The trimmed range a render covers, in seconds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct RangeSeconds {
    pub start: f64,
    pub end: f64,
}

pub fn range_seconds(settings: &RenderSettings, take: &Take) -> RangeSeconds {
    let sr = take.spec.sample_rate as f64;
    let end = settings.trim_end_frame.unwrap_or(take.num_frames);
    RangeSeconds {
        start: settings.trim_start_frame as f64 / sr,
        end: end as f64 / sr,
    }
}

/// `mixstack.render/1`.
#[derive(Debug, Clone, Serialize)]
pub struct RenderOutput {
    pub schema: &'static str,
    pub input: String,
    pub output: String,
    pub format: OutputFormat,
    /// Set for MP3 only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mp3_bitrate_kbps: Option<u32>,
    pub range: RangeSeconds,
    pub duration_seconds: f64,
    pub report: RenderReport,
    pub mapping: MappingReport,
    pub engine: String,
}

/// Check that `out` may be written: not the source, and not an existing file
/// unless `overwrite` is set.
pub fn check_output(take: &Take, out: &Path, overwrite: bool) -> Result<()> {
    let same = match (
        std::fs::canonicalize(&take.path),
        std::fs::canonicalize(out),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if same {
        return Err(output_err(
            out,
            "it is the source recording, which is never modified",
        ));
    }
    if out.exists() && !overwrite {
        return Err(output_err(
            out,
            "it already exists (pass --force to overwrite)",
        ));
    }
    if out.is_dir() {
        return Err(output_err(out, "it is a directory"));
    }
    Ok(())
}

/// Where a render is written before it is complete: next to the target, so
/// the final rename stays on one filesystem.
fn partial_path(out: &Path) -> PathBuf {
    let name = out
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    out.with_file_name(format!(".{name}.partial"))
}

/// Render `session` of `take` to `out` — the app's export ([`render_io`]),
/// written to a hidden `.partial` file first and renamed when complete, so
/// a failed or interrupted render never leaves a truncated file under the
/// name a consumer is waiting for.
///
/// `progress` gets 0..=1 across both passes (pass 2 starts at 0.5).
#[allow(clippy::too_many_arguments)] // one argument per thing the caller decides
pub fn render_session_to(
    take: &Take,
    session: &Session,
    reference: Option<&ReferenceProfile>,
    mapping: &MappingReport,
    out: &Path,
    overwrite: bool,
    engine_version: &str,
    progress: impl FnMut(f32),
) -> Result<RenderOutput> {
    check_output(take, out, overwrite)?;
    let partial = partial_path(out);
    // Find out about permissions now, not after a full analysis pass.
    std::fs::File::create(&partial).map_err(|e| output_err(out, e.to_string()))?;
    let result = render_io(
        &InputHandle::Path(take.path.to_string_lossy().into_owned()),
        &session.tracks,
        &session.settings,
        reference,
        &OutputHandle::Path(partial.to_string_lossy().into_owned()),
        progress,
    );
    let report = match result {
        Ok(r) => r,
        Err(e) => {
            let _ = std::fs::remove_file(&partial);
            return Err(e.into());
        }
    };
    std::fs::rename(&partial, out).map_err(|e| {
        let _ = std::fs::remove_file(&partial);
        output_err(out, e.to_string())
    })?;
    let s = &session.settings;
    Ok(RenderOutput {
        schema: RENDER_SCHEMA,
        input: take.path.display().to_string(),
        output: out.display().to_string(),
        format: s.format,
        mp3_bitrate_kbps: (s.format == OutputFormat::Mp3).then_some(s.mp3_bitrate_kbps),
        range: range_seconds(s, take),
        duration_seconds: report.duration_seconds,
        report,
        mapping: mapping.clone(),
        engine: engine_version.to_string(),
    })
}

/// The whole headless render in one call: template → session → reference
/// profile → [`render_session_to`].
pub fn render_template(
    take: &Take,
    template: &Template,
    opts: &RenderOptions,
    out: &Path,
    overwrite: bool,
    engine_version: &str,
    progress: impl FnMut(f32),
) -> Result<RenderOutput> {
    let applied = prepare_session(template, take, opts)?;
    // Checked before the references are analysed, so a refused target costs
    // nothing.
    check_output(take, out, overwrite)?;
    let reference = reference_profile(&applied.session.settings)?;
    render_session_to(
        take,
        &applied.session,
        reference.as_ref(),
        &applied.report,
        out,
        overwrite,
        engine_version,
        progress,
    )
}

// ── measurement (#166) ──────────────────────────────────────────────────────

fn measure_err(e: EngineError) -> HeadlessError {
    match e {
        EngineError::InvalidRequest(m) => HeadlessError::Usage(m),
        other => HeadlessError::Engine(other),
    }
}

/// The mix a measurement without `--mix` describes: a fresh session, every
/// track in at unity — exactly what the app shows when a take is opened for
/// the first time.
pub fn fresh_template(take: &Take) -> Template {
    Template::from_session(&Session::from_track_info(&take.tracks))
}

/// `mixstack.measure/1` as the CLI and the MCP server print it.
#[derive(Debug, Clone, Serialize)]
pub struct MeasureOutput {
    pub input: String,
    #[serde(flatten)]
    pub measurement: Measurement,
    pub mapping: MappingReport,
    pub engine: String,
}

/// `mixstack.compare/1` as the CLI and the MCP server print it.
#[derive(Debug, Clone, Serialize)]
pub struct CompareOutput {
    pub input: String,
    #[serde(flatten)]
    pub comparison: Comparison,
    pub mapping_a: MappingReport,
    pub mapping_b: MappingReport,
    pub engine: String,
}

/// Measure applied sessions of one take in a single read. Only the range of
/// `opts` matters (start/end); every session gets the same.
pub fn measure_sessions(
    take: &Take,
    sessions: &[(&Session, &[Vec<String>])],
    opts: &RenderOptions,
    mopts: &MeasureOptions,
    progress: impl FnMut(f32),
) -> Result<Vec<Measurement>> {
    let mut settings = Vec::with_capacity(sessions.len());
    for (s, _) in sessions {
        let mut st = s.settings.clone();
        let range_only = RenderOptions {
            start_seconds: opts.start_seconds,
            end_seconds: opts.end_seconds,
            ..RenderOptions::default()
        };
        apply_render_options(&mut st, take, &range_only)?;
        // Keep the mix's own loudness setting for the predicted gain.
        st.loudness = s.settings.loudness;
        settings.push(st);
    }
    let jobs: Vec<MeasureJob> = sessions
        .iter()
        .zip(&settings)
        .map(|((s, tags), st)| MeasureJob {
            tracks: &s.tracks,
            tags,
            settings: st,
        })
        .collect();
    measure(
        &InputHandle::Path(take.path.to_string_lossy().into_owned()),
        &jobs,
        mopts,
        progress,
    )
    .map_err(|e| match e {
        EngineError::Io(_) | EngineError::NotWav => input_err(&take.path, e),
        other => measure_err(other),
    })
}

fn applied_for(template: &Template, take: &Take, opts: &RenderOptions) -> Result<AppliedTemplate> {
    let applied = apply_template(template, &take.tracks, opts.unknown);
    if applied.report.matched_nothing() {
        return Err(HeadlessError::NoMatch(take.path.display().to_string()));
    }
    Ok(applied)
}

/// Measure `template` applied to `take` over the range in `opts`.
pub fn measure_template(
    take: &Take,
    template: &Template,
    opts: &RenderOptions,
    mopts: &MeasureOptions,
    engine_version: &str,
    progress: impl FnMut(f32),
) -> Result<MeasureOutput> {
    let applied = applied_for(template, take, opts)?;
    let mut m = measure_sessions(
        take,
        &[(&applied.session, &applied.tags)],
        opts,
        mopts,
        progress,
    )?;
    let measurement = m.remove(0);
    debug_assert_eq!(measurement.schema, MEASURE_SCHEMA);
    Ok(MeasureOutput {
        input: take.path.display().to_string(),
        measurement,
        mapping: applied.report,
        engine: engine_version.to_string(),
    })
}

/// Measure two templates on the same take and range — one read of the file
/// for both — and report `b − a`.
pub fn compare_templates(
    take: &Take,
    a: &Template,
    b: &Template,
    opts: &RenderOptions,
    mopts: &MeasureOptions,
    engine_version: &str,
    progress: impl FnMut(f32),
) -> Result<CompareOutput> {
    let aa = applied_for(a, take, opts)?;
    let bb = applied_for(b, take, opts)?;
    let mut m = measure_sessions(
        take,
        &[(&aa.session, &aa.tags), (&bb.session, &bb.tags)],
        opts,
        mopts,
        progress,
    )?
    .into_iter();
    let (ma, mb) = (m.next().unwrap(), m.next().unwrap());
    let comparison = compare(ma, mb).map_err(measure_err)?;
    debug_assert_eq!(comparison.schema, COMPARE_SCHEMA);
    Ok(CompareOutput {
        input: take.path.display().to_string(),
        comparison,
        mapping_a: aa.report,
        mapping_b: bb.report,
        engine: engine_version.to_string(),
    })
}
