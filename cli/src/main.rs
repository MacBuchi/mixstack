//! `mixstack` — the app's engine without the app.
//!
//! This crate only translates: command-line arguments into engine types,
//! engine results into JSON. Every decision — which tracks a take has, how a
//! template maps, how seconds become frames, the render itself — is made in
//! `durecmix_engine::headless`, the same way `rust/` only translates for the
//! app. Contract (schemas, exit codes): AGENTS.md, section "CLI".

#[cfg(feature = "mcp")]
mod mcp;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use durecmix_engine::headless::{self, HeadlessError, RenderOptions, PROBE_SCHEMA, RENDER_SCHEMA};
use durecmix_engine::measure::{
    LevelPoint, MaskingMode, MeasureOptions, TrackSelection, COMPARE_SCHEMA, MEASURE_SCHEMA,
};
use durecmix_engine::render::{LoudnessMode, OutputFormat};
use durecmix_engine::template::{UnknownTracks, TEMPLATE_SCHEMA};

/// The release this binary belongs to (pubspec.yaml, via build.rs).
pub(crate) const VERSION: &str = env!("MIXSTACK_VERSION");
const ERROR_SCHEMA: &str = "mixstack.error/1";

#[derive(Parser)]
#[command(
    name = "mixstack",
    about = "Mix multichannel recordings down to stereo with the Mixstack engine",
    disable_version_flag = true
)]
struct Cli {
    /// Print the release and the schema versions this binary speaks.
    #[arg(short = 'V', long = "version")]
    version: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Header-only look at a recording: format, length, tracks.
    Probe {
        input: PathBuf,
        /// Print `mixstack.probe/1` JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Render a recording with a template or session to a stereo file.
    Render(RenderArgs),
    /// Measure a mix and its tracks: loudness, spectrum, masking, stereo.
    Measure(MeasureArgs),
    /// Measure two mixes of one recording and report the differences (b − a).
    Compare(CompareArgs),
    /// Serve the engine to an agent over MCP (stdio).
    #[cfg(feature = "mcp")]
    Mcp(McpArgs),
}

#[cfg(feature = "mcp")]
#[derive(clap::Args)]
struct McpArgs {
    /// A folder the server may read recordings, templates and mastering
    /// references from (and below). Repeatable; at least one.
    #[arg(long = "allow", required = true)]
    allow: Vec<PathBuf>,
    /// The only folder the server writes to (renders, templates).
    #[arg(long)]
    out_dir: PathBuf,
    /// Optional JSON with tighter edit limits than the app's ranges, e.g.
    /// {"max_eq_gain_db": 6, "max_gain_step_db": 3}.
    #[arg(long)]
    limits: Option<PathBuf>,
}

/// What to measure and how — shared by `measure` and `compare`.
#[derive(clap::Args)]
struct MeasureFlags {
    /// Trim-in, seconds from the start of the recording.
    #[arg(long)]
    start: Option<f64>,
    /// Trim-out, seconds from the start of the recording.
    #[arg(long)]
    end: Option<f64>,
    /// `mix` (audible tracks), `all`, or a comma-separated list of names.
    #[arg(long, default_value = "mix")]
    tracks: String,
    /// A track to measure, by exact name; repeatable. For names that contain
    /// a comma, which `--tracks` cannot express. Overrides `--tracks`.
    #[arg(long = "track")]
    track: Vec<String>,
    /// `off`, `vs-rest` (each track against the rest of the mix) or
    /// `pairs:<name,name,…>` (additionally a matrix, at most 12 tracks).
    #[arg(long, default_value = "vs-rest", value_parser = parse_masking)]
    masking: MaskingMode,
    /// Track levels before gain and pan (EQ applied) instead of their
    /// contribution to the mix.
    #[arg(long)]
    pre_fader: bool,
    #[arg(long, value_enum, default_value_t = UnknownArg::Exclude)]
    unknown_tracks: UnknownArg,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    /// No progress lines on stderr.
    #[arg(long)]
    quiet: bool,
}

impl MeasureFlags {
    fn options(&self) -> (RenderOptions, MeasureOptions) {
        let tracks = if !self.track.is_empty() {
            TrackSelection::Names(self.track.clone())
        } else {
            match self.tracks.as_str() {
                "mix" => TrackSelection::Mix,
                "all" => TrackSelection::All,
                list => TrackSelection::Names(split_names(list)),
            }
        };
        (
            RenderOptions {
                start_seconds: self.start,
                end_seconds: self.end,
                unknown: self.unknown_tracks.into(),
                ..RenderOptions::default()
            },
            MeasureOptions {
                tracks,
                masking: self.masking.clone(),
                point: if self.pre_fader {
                    LevelPoint::PreFader
                } else {
                    LevelPoint::PostFader
                },
            },
        )
    }
}

#[derive(clap::Args)]
struct MeasureArgs {
    input: PathBuf,
    /// Template or session; without it, every track at unity (a fresh mix).
    #[arg(long)]
    mix: Option<PathBuf>,
    #[command(flatten)]
    flags: MeasureFlags,
}

#[derive(clap::Args)]
struct CompareArgs {
    input: PathBuf,
    #[arg(long)]
    mix_a: PathBuf,
    #[arg(long)]
    mix_b: PathBuf,
    #[command(flatten)]
    flags: MeasureFlags,
}

fn split_names(list: &str) -> Vec<String> {
    list.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

fn parse_masking(s: &str) -> Result<MaskingMode, String> {
    match s {
        "off" => Ok(MaskingMode::Off),
        "vs-rest" => Ok(MaskingMode::VsRest),
        _ => match s.strip_prefix("pairs:") {
            Some(list) if !split_names(list).is_empty() => {
                Ok(MaskingMode::Pairs(split_names(list)))
            }
            _ => Err(format!(
                "expected off, vs-rest or pairs:<name,name,…>, got {s:?}"
            )),
        },
    }
}

#[derive(clap::Args)]
struct RenderArgs {
    input: PathBuf,
    /// A `mixstack.mix/1` template or a stored session (`*.durecmix.json`).
    #[arg(long)]
    mix: PathBuf,
    /// Target file; the extension picks the format unless --format is given.
    #[arg(long)]
    out: PathBuf,
    /// Output format (default: from the --out extension; .wav = 24-bit).
    #[arg(long, value_enum)]
    format: Option<FormatArg>,
    /// Trim-in, seconds from the start of the recording.
    #[arg(long)]
    start: Option<f64>,
    /// Trim-out, seconds from the start of the recording.
    #[arg(long)]
    end: Option<f64>,
    #[arg(long, default_value_t = 0.0)]
    fade_in_ms: f64,
    #[arg(long, default_value_t = 0.0)]
    fade_out_ms: f64,
    /// `lufs:<target>`, `peak:<dBFS>` or `none`; overrides the template.
    #[arg(long, value_parser = parse_loudness)]
    loudness: Option<LoudnessMode>,
    /// Constant MP3 bitrate in kbps.
    #[arg(long, default_value_t = 320, value_parser = parse_bitrate)]
    mp3_bitrate: u32,
    /// What happens to tracks of the recording the template does not know.
    #[arg(long, value_enum, default_value_t = UnknownArg::Exclude)]
    unknown_tracks: UnknownArg,
    /// Overwrite --out if it exists.
    #[arg(long)]
    force: bool,
    /// Print `mixstack.render/1` JSON instead of text.
    #[arg(long)]
    json: bool,
    /// No progress lines on stderr.
    #[arg(long)]
    quiet: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum FormatArg {
    Wav16,
    Wav24,
    Wav32f,
    Flac16,
    Flac24,
    Mp3,
}

impl From<FormatArg> for OutputFormat {
    fn from(f: FormatArg) -> Self {
        match f {
            FormatArg::Wav16 => OutputFormat::Wav16,
            FormatArg::Wav24 => OutputFormat::Wav24,
            FormatArg::Wav32f => OutputFormat::Wav32Float,
            FormatArg::Flac16 => OutputFormat::Flac16,
            FormatArg::Flac24 => OutputFormat::Flac24,
            FormatArg::Mp3 => OutputFormat::Mp3,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum UnknownArg {
    Exclude,
    Include,
}

impl From<UnknownArg> for UnknownTracks {
    fn from(u: UnknownArg) -> Self {
        match u {
            UnknownArg::Exclude => UnknownTracks::Exclude,
            UnknownArg::Include => UnknownTracks::Include,
        }
    }
}

fn parse_loudness(s: &str) -> Result<LoudnessMode, String> {
    if s == "none" {
        return Ok(LoudnessMode::None);
    }
    let (kind, value) = s
        .split_once(':')
        .ok_or_else(|| format!("expected lufs:<n>, peak:<n> or none, got {s:?}"))?;
    let v: f64 = value
        .parse()
        .map_err(|_| format!("{value:?} is not a number"))?;
    match kind {
        "lufs" => Ok(LoudnessMode::LufsIntegrated(v)),
        "peak" => Ok(LoudnessMode::PeakDbfs(v)),
        _ => Err(format!("unknown loudness mode {kind:?} (lufs, peak, none)")),
    }
}

/// The bitrates the band player's contract names; the engine itself
/// encodes a few more (`MP3_CBR_KBPS`).
fn parse_bitrate(s: &str) -> Result<u32, String> {
    match s.parse::<u32>() {
        Ok(v @ (128 | 192 | 256 | 320)) => Ok(v),
        _ => Err(format!("{s:?} is not one of 128, 192, 256, 320")),
    }
}

/// Exit codes, part of the contract: 0 ok, 1 render failed, 2 usage,
/// 3 input unreadable, 4 the mix matches no track, 5 output not writable.
fn exit_code(e: &HeadlessError) -> u8 {
    match e {
        HeadlessError::Engine(_) => 1,
        HeadlessError::Usage(_) => 2,
        HeadlessError::Input { .. } => 3,
        HeadlessError::NoMatch(_) => 4,
        HeadlessError::Output { .. } => 5,
    }
}

fn error_kind(e: &HeadlessError) -> &'static str {
    match e {
        HeadlessError::Engine(_) => "render",
        HeadlessError::Usage(_) => "usage",
        HeadlessError::Input { .. } => "input",
        HeadlessError::NoMatch(_) => "no_match",
        HeadlessError::Output { .. } => "output",
    }
}

fn print_json(value: &impl serde::Serialize) {
    let mut out = std::io::stdout().lock();
    // Non-finite floats (the loudness of silence) become null rather than
    // aborting the document.
    let _ = serde_json::to_writer_pretty(&mut out, value);
    let _ = writeln!(out);
}

/// Progress as JSON lines on stderr, one per percent: overall 0..1 plus the
/// pass it belongs to (1 = analysis, 2 = render).
fn progress_printer(quiet: bool) -> impl FnMut(f32) {
    let mut last = -1i32;
    move |p: f32| {
        if quiet {
            return;
        }
        let pct = (p * 100.0).floor() as i32;
        if pct == last {
            return;
        }
        last = pct;
        let pass = if p < 0.5 { 1 } else { 2 };
        eprintln!("{{\"progress\":{:.2},\"pass\":{pass}}}", pct as f32 / 100.0);
    }
}

fn probe(input: &Path, json: bool) -> Result<(), HeadlessError> {
    let take = headless::open_take(input)?;
    let out = headless::probe_output(&take, VERSION);
    if json {
        print_json(&out);
    } else {
        println!(
            "{}: {} ch, {} Hz, {}-bit {}, {:.3} s{}",
            out.path,
            out.channels,
            out.sample_rate,
            out.bits_per_sample,
            out.sample_format,
            out.duration_seconds,
            if out.has_ixml { "" } else { " (no iXML)" }
        );
        for t in &out.tracks {
            println!("  {:>3}  {}", t.index, t.name);
        }
    }
    Ok(())
}

fn render(a: &RenderArgs) -> Result<(), HeadlessError> {
    let format = match a.format {
        Some(f) => f.into(),
        None => headless::format_for_path(&a.out)?,
    };
    let take = headless::open_take(&a.input)?;
    let template = headless::load_mix(&a.mix)?;
    let opts = RenderOptions {
        start_seconds: a.start,
        end_seconds: a.end,
        fade_in_ms: a.fade_in_ms,
        fade_out_ms: a.fade_out_ms,
        loudness: a.loudness,
        format,
        mp3_bitrate_kbps: a.mp3_bitrate,
        unknown: a.unknown_tracks.into(),
    };
    let out = headless::render_template(
        &take,
        &template,
        &opts,
        &a.out,
        a.force,
        VERSION,
        progress_printer(a.quiet),
    )?;
    if a.json {
        print_json(&out);
    } else {
        let r = &out.report;
        println!(
            "{} → {}: {:.3} s, {:.1} LUFS-I, {:.1} dBTP, LRA {:.1} LU, gain {:+.1} dB",
            out.input,
            out.output,
            out.duration_seconds,
            r.integrated_lufs,
            r.true_peak_dbtp,
            r.lra_lu,
            r.gain_applied_db
        );
        let m = &out.mapping;
        println!(
            "  mapping: {} matched, {} unknown{}, {} missing{}",
            m.matched.len(),
            m.unknown.len(),
            if m.unknown.is_empty() {
                String::new()
            } else {
                format!(" ({})", m.unknown.join(", "))
            },
            m.missing.len(),
            if m.missing.is_empty() {
                String::new()
            } else {
                format!(" ({})", m.missing.join(", "))
            },
        );
    }
    Ok(())
}

fn fmt_db(v: f64) -> String {
    if v.is_finite() {
        format!("{v:.1}")
    } else {
        "−∞".into()
    }
}

fn measure_cmd(a: &MeasureArgs) -> Result<(), HeadlessError> {
    let take = headless::open_take(&a.input)?;
    let template = match &a.mix {
        Some(p) => headless::load_mix(p)?,
        None => headless::fresh_template(&take),
    };
    let (opts, mopts) = a.flags.options();
    let out = headless::measure_template(
        &take,
        &template,
        &opts,
        &mopts,
        VERSION,
        progress_printer(a.flags.quiet),
    )?;
    if a.flags.json {
        print_json(&out);
        return Ok(());
    }
    let m = &out.measurement;
    let l = &m.mix.level;
    println!(
        "{} [{:.2}–{:.2} s]: mix {} LUFS-I, LRA {:.1}, {} dBTP, PLR {}, corr {:.2}, width {:.2}",
        out.input,
        m.range.start,
        m.range.end,
        fmt_db(l.lufs_i),
        l.lra,
        fmt_db(l.true_peak_dbtp),
        fmt_db(l.plr_db),
        m.mix.correlation,
        m.mix.width
    );
    for t in &m.tracks {
        let masked = t
            .masking
            .as_ref()
            .map(|k| format!(", masked {:.0} %", k.masked_fraction * 100.0))
            .unwrap_or_default();
        println!(
            "  {:>3} {:<24} {:>6} LUFS, active {:>3.0} %{}",
            t.index,
            t.name,
            fmt_db(t.level.lufs_i),
            t.level.active_fraction * 100.0,
            masked
        );
    }
    Ok(())
}

fn compare_cmd(a: &CompareArgs) -> Result<(), HeadlessError> {
    let take = headless::open_take(&a.input)?;
    let ta = headless::load_mix(&a.mix_a)?;
    let tb = headless::load_mix(&a.mix_b)?;
    let (opts, mopts) = a.flags.options();
    let out = headless::compare_templates(
        &take,
        &ta,
        &tb,
        &opts,
        &mopts,
        VERSION,
        progress_printer(a.flags.quiet),
    )?;
    if a.flags.json {
        print_json(&out);
        return Ok(());
    }
    let d = &out.comparison.delta;
    println!(
        "b − a: mix {:+.2} LU, LRA {:+.2}, TP {:+.2} dB, width {:+.3}",
        d.mix.level.lufs_i, d.mix.level.lra, d.mix.level.true_peak_dbtp, d.mix.width
    );
    for t in &d.tracks {
        let masked = t
            .masking
            .as_ref()
            .map(|k| format!(", masked {:+.0} pp", k.masked_fraction * 100.0))
            .unwrap_or_default();
        println!(
            "  {:>3} {:<24} {:+.2} LU{}",
            t.index, t.name, t.level.lufs_i, masked
        );
    }
    Ok(())
}

fn version_line() -> String {
    format!(
        "mixstack {VERSION} {TEMPLATE_SCHEMA} {PROBE_SCHEMA} {RENDER_SCHEMA} {MEASURE_SCHEMA} {COMPARE_SCHEMA}"
    )
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if cli.version {
        println!("{}", version_line());
        return ExitCode::SUCCESS;
    }
    let (result, json) = match &cli.cmd {
        Some(Cmd::Probe { input, json }) => (probe(input, *json), *json),
        Some(Cmd::Render(a)) => (render(a), a.json),
        Some(Cmd::Measure(a)) => (measure_cmd(a), a.flags.json),
        Some(Cmd::Compare(a)) => (compare_cmd(a), a.flags.json),
        #[cfg(feature = "mcp")]
        Some(Cmd::Mcp(a)) => {
            // stdout is the protocol channel here: nothing else may print.
            return match mcp::run(&a.allow, &a.out_dir, a.limits.as_deref()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("mixstack mcp: {e}");
                    ExitCode::from(2)
                }
            };
        }
        None => {
            eprintln!(
                "{}\n\nUsage: mixstack <probe|render|measure|compare> … (see --help)",
                version_line()
            );
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let code = exit_code(&e);
            eprintln!("mixstack: {e}");
            if json {
                print_json(&serde_json::json!({
                    "schema": ERROR_SCHEMA,
                    "code": code,
                    "kind": error_kind(&e),
                    "message": e.to_string(),
                    "engine": VERSION,
                }));
            }
            ExitCode::from(code)
        }
    }
}
