//! `mixstack mcp` — the engine as an MCP server over stdio (#167).
//!
//! Eight tools shaped around one loop: load → measure → update → compare →
//! keep or revert → render / save. Every decision is the engine's
//! (`durecmix_engine::{headless, edit, measure}`); this module translates
//! tool arguments into engine calls, guards paths ([`paths`]) and keeps the
//! revisions of each loaded mix in memory.
//!
//! Protocol notes (MCP 2026-07-28, `rmcp` pinned to 3.12.0):
//! - stdio only: it runs next to the recordings, nothing leaves the machine.
//! - State lives in the server process under a `mix_id`, not in a protocol
//!   session (the 2026-07-28 revision is stateless) — one stdio process per
//!   client, and `save_template` is the way to keep a mix beyond it.
//! - Errors a model can fix (a path outside the allowed folders, a gain out
//!   of range, an unknown track) are **tool errors** (`isError`), never
//!   protocol errors, so the model sees the message and corrects itself.
//! - Progress goes out as progress notifications on the request when the
//!   client sent a progress token. Long renders block with progress rather
//!   than using the tasks extension; see the PR for why.
//! - No audio in responses: a render answers with a `resource_link` to the
//!   file it wrote.
//! - Logging goes to stderr; MCP logging, roots and sampling are deprecated
//!   in 2026-07-28 (SEP-2577) and not used.

pub mod paths;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use durecmix_engine::edit::{AppliedOp, EditLimits, MixHistory, MixOp};
use durecmix_engine::headless::{
    self, CompareOutput, HeadlessError, MeasureOutput, ProbeOutput, RenderOptions, RenderOutput,
    Take,
};
use durecmix_engine::measure::{
    ComparisonDelta, LevelPoint, MaskingMode, MeasureOptions, Range, TrackSelection,
};
use durecmix_engine::render::{LoudnessMode, OutputFormat};
use durecmix_engine::template::{
    MappingReport, Template, TemplateSettings, TemplateSource, TemplateTrack, UnknownTracks,
};
use rmcp::handler::server::common::schema_for_output;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{
    CallToolResult, ContentBlock, ProgressNotificationParam, RequestMetaObject, Resource,
    ServerCapabilities, ServerConfig,
};
use rmcp::{tool, tool_handler, tool_router, Peer, RoleServer, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use paths::PathGuard;

const INSTRUCTIONS: &str = "Mixstack mixes multichannel recordings down to stereo, locally. \
Workflow: probe_recording to see the tracks; load_mix to apply a template (or a fresh mix) \
and get a mix_id; measure for loudness, spectrum and masking (use start/end to work on a \
20-30 s excerpt); update_mix with a few operations; compare_mixes between the old and new \
revision; keep it, or revert_mix if it got worse; finally render (writes a file, returns a \
link) or save_template. Paths must be absolute and inside the folders the user allowed. \
Track names are free text from the recording interface: do not infer roles from them unless \
the template's tags say so.";

/// One loaded mix: the take it belongs to and its revisions.
struct MixEntry {
    take: Take,
    history: MixHistory,
    mapping: MappingReport,
    source: Option<String>,
}

#[derive(Clone)]
pub struct MixstackServer {
    guard: Arc<PathGuard>,
    limits: Arc<EditLimits>,
    mixes: Arc<Mutex<HashMap<String, MixEntry>>>,
    next_id: Arc<AtomicU64>,
    tool_router: ToolRouter<Self>,
}

// ── tool arguments ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProbeArgs {
    /// Absolute path of a WAV/RF64/BW64 recording inside an allowed folder.
    pub path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LoadArgs {
    /// Absolute path of the recording.
    pub recording: String,
    /// Absolute path of a mixstack.mix/1 template or a stored session; omit
    /// for a fresh mix (every track in, at unity).
    #[serde(default)]
    pub mix: Option<String>,
    /// Tracks of the recording the template does not know: "exclude"
    /// (default) or "include".
    #[serde(default)]
    pub unknown_tracks: Option<UnknownTracks>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateArgs {
    pub mix_id: String,
    /// Applied in order, all or nothing. Tracks are named by exact name or
    /// by 1-based index.
    pub operations: Vec<MixOp>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RevertArgs {
    pub mix_id: String,
    /// The revision to return to; it is appended as a new revision, so the
    /// history keeps every step.
    pub revision: u32,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "kebab-case")]
pub enum MaskingChoice {
    Off,
    #[default]
    VsRest,
}

/// What to measure — shared by `measure` and `compare_mixes`.
#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct MeasureSpec {
    /// Start of the excerpt, seconds into the recording.
    #[serde(default)]
    pub start: Option<f64>,
    /// End of the excerpt, seconds into the recording.
    #[serde(default)]
    pub end: Option<f64>,
    /// Only these tracks (exact names). Default: every audible track.
    #[serde(default)]
    pub tracks: Option<Vec<String>>,
    /// Every track, muted and out-of-mix ones included.
    #[serde(default)]
    pub all_tracks: bool,
    /// "vs-rest" (default): each track against the rest of the mix; "off".
    #[serde(default)]
    pub masking: Option<MaskingChoice>,
    /// Additionally a pairwise masking matrix among these tracks (≤ 12).
    #[serde(default)]
    pub pairs: Option<Vec<String>>,
    /// Track levels before gain and pan (EQ applied).
    #[serde(default)]
    pub pre_fader: bool,
}

impl MeasureSpec {
    fn options(&self) -> (RenderOptions, MeasureOptions) {
        let tracks = match (&self.tracks, self.all_tracks) {
            (Some(names), _) => TrackSelection::Names(names.clone()),
            (None, true) => TrackSelection::All,
            (None, false) => TrackSelection::Mix,
        };
        let masking = match (&self.pairs, self.masking.unwrap_or_default()) {
            (Some(p), _) => MaskingMode::Pairs(p.clone()),
            (None, MaskingChoice::Off) => MaskingMode::Off,
            (None, MaskingChoice::VsRest) => MaskingMode::VsRest,
        };
        (
            RenderOptions {
                start_seconds: self.start,
                end_seconds: self.end,
                ..RenderOptions::default()
            },
            MeasureOptions {
                tracks,
                masking,
                point: if self.pre_fader {
                    LevelPoint::PreFader
                } else {
                    LevelPoint::PostFader
                },
            },
        )
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MeasureArgs {
    pub mix_id: String,
    /// Default: the newest revision.
    #[serde(default)]
    pub revision: Option<u32>,
    #[serde(flatten)]
    pub spec: MeasureSpec,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CompareArgs {
    pub mix_id: String,
    pub revision_a: u32,
    /// Default: the newest revision.
    #[serde(default)]
    pub revision_b: Option<u32>,
    #[serde(flatten)]
    pub spec: MeasureSpec,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RenderArgs {
    pub mix_id: String,
    /// Default: the newest revision.
    #[serde(default)]
    pub revision: Option<u32>,
    /// Plain file name, written into the server's output folder; the
    /// extension (.wav, .flac, .mp3) picks the format unless `format` is set.
    pub file_name: String,
    #[serde(default)]
    pub format: Option<OutputFormat>,
    #[serde(default)]
    pub start: Option<f64>,
    #[serde(default)]
    pub end: Option<f64>,
    #[serde(default)]
    pub fade_in_ms: Option<f64>,
    #[serde(default)]
    pub fade_out_ms: Option<f64>,
    /// Overrides the mix's loudness: {"LufsIntegrated": -16}, {"PeakDbfs":
    /// -1} or "None".
    #[serde(default)]
    pub loudness: Option<LoudnessMode>,
    /// Constant MP3 bitrate (kbps): 128, 192, 256 or 320 (default).
    #[serde(default)]
    pub mp3_bitrate_kbps: Option<u32>,
    /// Replace an existing file of that name in the output folder.
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SaveArgs {
    pub mix_id: String,
    /// Default: the newest revision.
    #[serde(default)]
    pub revision: Option<u32>,
    /// Plain file name in the output folder, e.g. "band.mixstack.json".
    pub file_name: String,
    /// The template's display name. Default: the file name.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub overwrite: bool,
}

// ── tool results ────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, JsonSchema)]
pub struct LoadResult {
    pub mix_id: String,
    pub revision: u32,
    pub recording: String,
    pub duration_seconds: f64,
    /// Every track of the recording with its parameters and tags, in
    /// recording order.
    pub tracks: Vec<TemplateTrack>,
    pub settings: TemplateSettings,
    pub mapping: MappingReport,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct UpdateResult {
    pub mix_id: String,
    pub revision: u32,
    pub applied: Vec<AppliedOp>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RevertResult {
    pub mix_id: String,
    pub revision: u32,
    pub reverted_to: u32,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct MeasureResult {
    pub mix_id: String,
    pub revision: u32,
    #[serde(flatten)]
    pub measurement: MeasureOutput,
}

/// The differences only — both full measurements are one `measure` call
/// away, and an answer that repeats them would be three times the size.
#[derive(Debug, Serialize, JsonSchema)]
pub struct CompareResult {
    pub schema: String,
    pub mix_id: String,
    pub revision_a: u32,
    pub revision_b: u32,
    pub range: Range,
    pub bands_hz: Vec<f64>,
    /// b − a.
    pub delta: ComparisonDelta,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RenderResult {
    pub mix_id: String,
    pub revision: u32,
    pub uri: String,
    #[serde(flatten)]
    pub render: RenderOutput,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SaveResult {
    pub mix_id: String,
    pub revision: u32,
    pub path: String,
    pub uri: String,
    pub tracks: usize,
}

// ── helpers ─────────────────────────────────────────────────────────────────

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn headless_err(e: HeadlessError) -> String {
    e.to_string()
}

fn file_uri(p: &std::path::Path) -> String {
    let mut s = String::from("file://");
    for b in p.to_string_lossy().bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                s.push(b as char)
            }
            _ => s.push_str(&format!("%{b:02X}")),
        }
    }
    s
}

/// RFC 3339 UTC timestamp for a template's `created`, without a date crate.
fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil from days (H. Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Run blocking engine work off the async runtime, forwarding its 0..1
/// progress as MCP progress notifications when the client asked for them.
async fn blocking_with_progress<T: Send + 'static>(
    meta: &RequestMetaObject,
    peer: &Peer<RoleServer>,
    work: impl FnOnce(&mut dyn FnMut(f32)) -> T + Send + 'static,
) -> Result<T, String> {
    let token = meta.get_progress_token();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<f32>();
    let job = tokio::task::spawn_blocking(move || {
        let mut last = -1i32;
        let mut report = move |p: f32| {
            let pct = (p * 100.0) as i32;
            if pct != last {
                last = pct;
                let _ = tx.send(p);
            }
        };
        work(&mut report)
    });
    if let Some(token) = token {
        let peer = peer.clone();
        tokio::spawn(async move {
            while let Some(p) = rx.recv().await {
                let _ = peer
                    .notify_progress(
                        ProgressNotificationParam::new(token.clone(), f64::from(p)).with_total(1.0),
                    )
                    .await;
            }
        });
    }
    job.await.map_err(err)
}

impl MixstackServer {
    pub fn new(guard: PathGuard, limits: EditLimits) -> Self {
        Self {
            guard: Arc::new(guard),
            limits: Arc::new(limits),
            mixes: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
            tool_router: Self::tool_router(),
        }
    }

    /// A snapshot of what a tool needs from a loaded mix.
    fn snapshot(
        &self,
        mix_id: &str,
        revision: Option<u32>,
    ) -> Result<(Take, durecmix_engine::edit::Revision, u32, MappingReport), String> {
        let mixes = self.mixes.lock().map_err(err)?;
        let e = mixes
            .get(mix_id)
            .ok_or_else(|| format!("no mix {mix_id:?} — load_mix first"))?;
        let rev = revision.unwrap_or_else(|| e.history.current());
        let r = e.history.get(rev).map_err(err)?.clone();
        Ok((e.take.clone(), r, rev, e.mapping.clone()))
    }

    fn check_references(
        &self,
        settings: &durecmix_engine::render::RenderSettings,
    ) -> Result<(), String> {
        if let Some(m) = settings.mastering.as_ref().filter(|m| m.enabled) {
            for r in m.normalized_references() {
                self.guard
                    .readable(&r.path)
                    .map_err(|e| format!("mastering reference: {e}"))?;
            }
        }
        Ok(())
    }
}

#[tool_router]
impl MixstackServer {
    #[tool(
        description = "Header-only look at a recording: channels, sample rate, length, track names.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn probe_recording(
        &self,
        Parameters(a): Parameters<ProbeArgs>,
    ) -> Result<Json<ProbeOutput>, String> {
        let path = self.guard.readable(&a.path)?;
        let take = headless::open_take(&path).map_err(headless_err)?;
        Ok(Json(headless::probe_output(&take, crate::VERSION)))
    }

    #[tool(
        description = "Apply a template or session (or a fresh mix) to a recording. Returns a mix_id, revision 0, every track's parameters and a report of how template tracks were matched (by name, normalized name or index; unknown and missing tracks).",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn load_mix(
        &self,
        Parameters(a): Parameters<LoadArgs>,
    ) -> Result<Json<LoadResult>, String> {
        let rec = self.guard.readable(&a.recording)?;
        let take = headless::open_take(&rec).map_err(headless_err)?;
        let (template, source) = match &a.mix {
            Some(p) => {
                let path = self.guard.readable(p)?;
                (
                    headless::load_mix(&path).map_err(headless_err)?,
                    Some(path.display().to_string()),
                )
            }
            None => (headless::fresh_template(&take), None),
        };
        let applied = durecmix_engine::template::apply_template(
            &template,
            &take.tracks,
            a.unknown_tracks.unwrap_or_default(),
        );
        if applied.report.matched_nothing() {
            return Err(HeadlessError::NoMatch(take.path.display().to_string()).to_string());
        }
        let id = format!("mix-{}", self.next_id.fetch_add(1, Ordering::SeqCst));
        let view = Template::from_session_with_tags(&applied.session, &applied.tags);
        let result = LoadResult {
            mix_id: id.clone(),
            revision: 0,
            recording: take.path.display().to_string(),
            duration_seconds: take.duration_seconds(),
            tracks: view.tracks,
            settings: view.settings,
            mapping: applied.report.clone(),
        };
        self.mixes.lock().map_err(err)?.insert(
            id,
            MixEntry {
                history: MixHistory::new(applied.session, applied.tags, "load"),
                mapping: applied.report,
                take,
                source,
            },
        );
        Ok(Json(result))
    }

    #[tool(
        description = "Change a mix with a list of operations (set_gain, set_pan, set_eq, set_hpf, set_in_mix, set_mute, set_solo, set_polarity). All or nothing; ranges are the app's (gain -60..+6 dB, pan -1..1, EQ +-15 dB). Returns the new revision.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn update_mix(
        &self,
        Parameters(a): Parameters<UpdateArgs>,
    ) -> Result<Json<UpdateResult>, String> {
        let mut mixes = self.mixes.lock().map_err(err)?;
        let e = mixes
            .get_mut(&a.mix_id)
            .ok_or_else(|| format!("no mix {:?} — load_mix first", a.mix_id))?;
        let (revision, applied) = e.history.update(&a.operations, &self.limits).map_err(err)?;
        Ok(Json(UpdateResult {
            mix_id: a.mix_id,
            revision,
            applied,
        }))
    }

    #[tool(
        description = "Go back to an earlier revision of a mix. The old state is appended as a new revision, so nothing is lost.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn revert_mix(
        &self,
        Parameters(a): Parameters<RevertArgs>,
    ) -> Result<Json<RevertResult>, String> {
        let mut mixes = self.mixes.lock().map_err(err)?;
        let e = mixes
            .get_mut(&a.mix_id)
            .ok_or_else(|| format!("no mix {:?} — load_mix first", a.mix_id))?;
        let revision = e.history.revert(a.revision).map_err(err)?;
        Ok(Json(RevertResult {
            mix_id: a.mix_id,
            revision,
            reverted_to: a.revision,
        }))
    }

    #[tool(
        description = "Measure a revision of a mix (default: the newest): loudness (BS.1770), third-octave spectrum, masking of each track by the rest of the mix, stereo, activity. Use start/end for an excerpt.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn measure(
        &self,
        Parameters(a): Parameters<MeasureArgs>,
        meta: RequestMetaObject,
        peer: Peer<RoleServer>,
    ) -> Result<Json<MeasureResult>, String> {
        let (take, rev, revision, _) = self.snapshot(&a.mix_id, a.revision)?;
        let (opts, mopts) = a.spec.options();
        let template = Template::from_session_with_tags(&rev.session, &rev.tags);
        let out = blocking_with_progress(&meta, &peer, move |p| {
            headless::measure_template(&take, &template, &opts, &mopts, crate::VERSION, p)
        })
        .await?
        .map_err(headless_err)?;
        Ok(Json(MeasureResult {
            mix_id: a.mix_id,
            revision,
            measurement: out,
        }))
    }

    #[tool(
        description = "Measure two revisions of a mix on the same excerpt (one read of the file) and return the differences b - a: loudness, spectrum, masking per track, stereo.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn compare_mixes(
        &self,
        Parameters(a): Parameters<CompareArgs>,
        meta: RequestMetaObject,
        peer: Peer<RoleServer>,
    ) -> Result<Json<CompareResult>, String> {
        let (take, ra, revision_a, _) = self.snapshot(&a.mix_id, Some(a.revision_a))?;
        let (_, rb, revision_b, _) = self.snapshot(&a.mix_id, a.revision_b)?;
        let (opts, mopts) = a.spec.options();
        let ta = Template::from_session_with_tags(&ra.session, &ra.tags);
        let tb = Template::from_session_with_tags(&rb.session, &rb.tags);
        let out: CompareOutput = blocking_with_progress(&meta, &peer, move |p| {
            headless::compare_templates(&take, &ta, &tb, &opts, &mopts, crate::VERSION, p)
        })
        .await?
        .map_err(headless_err)?;
        let c = out.comparison;
        Ok(Json(CompareResult {
            schema: c.schema,
            mix_id: a.mix_id,
            revision_a,
            revision_b,
            range: c.range,
            bands_hz: c.bands_hz,
            delta: c.delta,
        }))
    }

    #[tool(
        description = "Render a revision of a mix (or an excerpt) to a stereo file in the server's output folder, with the same chain as the app's export (EQ, normalisation, limiter, mastering). Returns a file link and the loudness report. Never overwrites unless overwrite is true.",
        output_schema = schema_for_output::<RenderResult>(),
        annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn render(
        &self,
        Parameters(a): Parameters<RenderArgs>,
        meta: RequestMetaObject,
        peer: Peer<RoleServer>,
    ) -> Result<CallToolResult, String> {
        let (take, rev, revision, mapping) = self.snapshot(&a.mix_id, a.revision)?;
        let out = self.guard.writable(&a.file_name)?;
        let format = match a.format {
            Some(f) => f,
            None => headless::format_for_path(&out).map_err(headless_err)?,
        };
        let opts = RenderOptions {
            start_seconds: a.start,
            end_seconds: a.end,
            fade_in_ms: a.fade_in_ms.unwrap_or(0.0),
            fade_out_ms: a.fade_out_ms.unwrap_or(0.0),
            loudness: a.loudness,
            format,
            mp3_bitrate_kbps: a.mp3_bitrate_kbps.unwrap_or(320),
            unknown: UnknownTracks::Exclude,
        };
        let mut session = rev.session.clone();
        headless::apply_render_options(&mut session.settings, &take, &opts)
            .map_err(headless_err)?;
        self.check_references(&session.settings)?;
        let overwrite = a.overwrite;
        let target = out.clone();
        let render = blocking_with_progress(&meta, &peer, move |p| {
            headless::check_output(&take, &target, overwrite)?;
            let reference = headless::reference_profile(&session.settings)?;
            headless::render_session_to(
                &take,
                &session,
                reference.as_ref(),
                &mapping,
                &target,
                overwrite,
                crate::VERSION,
                p,
            )
        })
        .await?
        .map_err(headless_err)?;
        let uri = file_uri(&out);
        let name = a.file_name.clone();
        let result = RenderResult {
            mix_id: a.mix_id,
            revision,
            uri: uri.clone(),
            render,
        };
        let mut call = CallToolResult::structured(serde_json::to_value(&result).map_err(err)?);
        let mime = match format {
            OutputFormat::Mp3 => "audio/mpeg",
            OutputFormat::Flac16 | OutputFormat::Flac24 => "audio/flac",
            _ => "audio/wav",
        };
        call.content.push(ContentBlock::resource_link(
            Resource::new(uri, name).with_mime_type(mime),
        ));
        Ok(call)
    }

    #[tool(
        description = "Save a revision of a mix as a mixstack.mix/1 template (tracks, EQ, tags, master settings; no trim, fades or format) in the server's output folder.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn save_template(
        &self,
        Parameters(a): Parameters<SaveArgs>,
    ) -> Result<Json<SaveResult>, String> {
        let out = self.guard.writable(&a.file_name)?;
        if out.exists() && !a.overwrite {
            return Err(format!(
                "{} already exists (pass overwrite: true)",
                out.display()
            ));
        }
        let (take, rev, revision, _) = self.snapshot(&a.mix_id, a.revision)?;
        let source = self
            .mixes
            .lock()
            .map_err(err)?
            .get(&a.mix_id)
            .and_then(|e| e.source.clone());
        let mut t = Template::from_session_with_tags(&rev.session, &rev.tags);
        t.name = a.name.unwrap_or_else(|| a.file_name.clone());
        t.created = Some(now_rfc3339());
        t.source = Some(TemplateSource {
            take: Some(take.stem()),
            snapshot: source.map(|s| format!("{s} @ revision {revision}")),
        });
        t.save(&out).map_err(err)?;
        Ok(Json(SaveResult {
            mix_id: a.mix_id,
            revision,
            uri: file_uri(&out),
            path: out.display().to_string(),
            tracks: t.tracks.len(),
        }))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for MixstackServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(rmcp::model::Implementation::new("mixstack", crate::VERSION))
            .with_instructions(INSTRUCTIONS)
    }
}

/// Serve on stdin/stdout until the client goes away.
pub fn run(
    allow: &[PathBuf],
    out_dir: &std::path::Path,
    limits: Option<&std::path::Path>,
) -> Result<(), String> {
    let guard = PathGuard::new(allow, out_dir)?;
    let limits = match limits {
        Some(p) => serde_json::from_str::<EditLimits>(
            &std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?,
        )
        .map_err(|e| format!("{}: {e}", p.display()))?,
        None => EditLimits::default(),
    };
    eprintln!(
        "mixstack mcp {}: reading under {}, writing to {}",
        crate::VERSION,
        allow
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", "),
        guard.out_dir().display()
    );
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(err)?;
    rt.block_on(async move {
        let service = MixstackServer::new(guard, limits)
            .serve(rmcp::transport::stdio())
            .await
            .map_err(err)?;
        service.waiting().await.map_err(err)?;
        Ok(())
    })
}
