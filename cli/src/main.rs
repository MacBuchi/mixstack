//! `mixstack` — the app's engine without the app.
//!
//! This crate only translates: command-line arguments into engine types,
//! engine results into JSON. Every decision — which tracks a take has, how a
//! template maps, how seconds become frames, the render itself — is made in
//! `durecmix_engine::headless`, the same way `rust/` only translates for the
//! app. Contract (schemas, exit codes): AGENTS.md, section "CLI".

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use durecmix_engine::headless::{self, HeadlessError, RenderOptions, PROBE_SCHEMA, RENDER_SCHEMA};
use durecmix_engine::render::{LoudnessMode, OutputFormat};
use durecmix_engine::template::{UnknownTracks, TEMPLATE_SCHEMA};

/// The release this binary belongs to (pubspec.yaml, via build.rs).
const VERSION: &str = env!("MIXSTACK_VERSION");
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

/// Format from the target's extension, the app's defaults (24-bit).
fn format_for(out: &Path) -> Result<OutputFormat, HeadlessError> {
    let ext = out
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "wav" => Ok(OutputFormat::Wav24),
        "flac" => Ok(OutputFormat::Flac24),
        "mp3" => Ok(OutputFormat::Mp3),
        _ => Err(HeadlessError::Usage(format!(
            "cannot tell the format from {:?} — use .wav, .flac or .mp3, or pass --format",
            out.display()
        ))),
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
        None => format_for(&a.out)?,
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

fn version_line() -> String {
    format!("mixstack {VERSION} {TEMPLATE_SCHEMA} {PROBE_SCHEMA} {RENDER_SCHEMA}")
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
        None => {
            eprintln!(
                "{}\n\nUsage: mixstack <probe|render> … (see --help)",
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
