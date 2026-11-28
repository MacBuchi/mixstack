//! `mixstack` — the app's engine without the app.
//!
//! This crate only translates: command-line arguments into engine types,
//! engine results into JSON. Every decision — which tracks a take has, how a
//! template maps, how seconds become frames, the render itself — is made in
//! `durecmix_engine::headless`, the same way `rust/` only translates for the
//! app. Reference for users: docs/CLI.md (kept in step with this file by
//! `the_reference_documents_every_command_and_option` below).

#[cfg(feature = "mcp")]
mod mcp;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use durecmix_engine::headless::{self, HeadlessError, RenderOptions, PROBE_SCHEMA, RENDER_SCHEMA};
use durecmix_engine::measure::{
    LevelPoint, MaskingMode, MeasureOptions, TrackSelection, COMPARE_SCHEMA, MEASURE_SCHEMA,
};
use durecmix_engine::render::{LoudnessMode, OutputFormat};
use durecmix_engine::template::{UnknownTracks, TEMPLATE_SCHEMA};
use schemars::JsonSchema;

/// The release this binary belongs to (pubspec.yaml, via build.rs).
pub(crate) const VERSION: &str = env!("MIXSTACK_VERSION");
const ERROR_SCHEMA: &str = "mixstack.error/1";

/// The exit-code table, as a macro so `concat!` can put it into --help.
macro_rules! exit_codes {
    () => {
        "\
Exit codes:
  0  ok
  1  render failed
  2  usage (bad arguments, out-of-range values)
  3  input unreadable (recording, mix or mastering reference)
  4  the mix matches no track of the recording
  5  output not writable (exists without --force, is the source, no folder)"
    };
}

macro_rules! top_examples {
    () => {
        "\
Examples:
  mixstack probe take.wav
  mixstack render take.wav --mix band.mixstack.json --out mix.mp3 --mp3-bitrate 128
  mixstack measure take.wav --mix band.mixstack.json --start 60 --end 90
  mixstack completions zsh > ~/.zfunc/_mixstack"
    };
}

#[derive(Parser)]
#[command(
    name = "mixstack",
    about = "Mix multichannel recordings down to stereo with the Mixstack engine",
    long_about = "Mix multichannel recordings down to stereo with the Mixstack engine.\n\n\
        The same engine as the Mixstack app: a render is byte-identical to the \
        app's export of the same mix. Reads interleaved RIFF/RF64/BW64 WAV at \
        any channel count; the source is never written.",
    after_help = concat!(
        top_examples!(),
        "\n\nExit codes: 0 ok · 1 render failed · 2 usage · 3 input unreadable\n            4 no track matched · 5 output not writable (--help for details)"
    ),
    after_long_help = concat!(
        top_examples!(),
        "\n\n",
        exit_codes!(),
        "\n\nReference: docs/CLI.md in https://github.com/MacBuchi/mixstack"
    ),
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
    #[command(after_help = "\
Examples:
  mixstack probe take.wav
  mixstack probe take.wav --json | jq '.tracks[].name'

Exit codes: 0 ok · 2 usage · 3 input unreadable")]
    Probe {
        /// The recording (WAV, RF64 or BW64). Only its header and iXML are read.
        input: PathBuf,
        /// Print `mixstack.probe/1` JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Render a recording with a template or session to a stereo file.
    #[command(after_help = "\
Examples:
  mixstack render take.wav --mix band.mixstack.json --out mix.wav
  mixstack render take.wav --mix band.mixstack.json --out song.mp3 \\
      --start 2.8 --end 120 --fade-out-ms 500 --loudness lufs:-16 --mp3-bitrate 128

The file appears under its final name only when complete (written as a
hidden .<name>.partial beside it first). Progress: JSON lines on stderr,
{\"progress\":0.42,\"pass\":2}.

Exit codes: 0 ok · 1 render failed · 2 usage · 3 input unreadable
            4 no track matched · 5 output not writable")]
    Render(RenderArgs),
    /// Measure a mix and its tracks: loudness, spectrum, masking, stereo.
    #[command(after_help = "\
Examples:
  mixstack measure take.wav                              # every track at unity
  mixstack measure take.wav --mix band.mixstack.json --start 60 --end 90
  mixstack measure take.wav --mix band.mixstack.json --masking pairs:Kick,Bass --json

Exit codes: 0 ok · 1 measurement failed · 2 usage · 3 input unreadable · 4 no track matched")]
    Measure(MeasureArgs),
    /// Measure two mixes of one recording and report the differences (b − a).
    #[command(after_help = "\
Examples:
  mixstack compare take.wav --mix-a before.mixstack.json --mix-b after.mixstack.json
  mixstack compare take.wav --mix-a a.json --mix-b b.json --start 60 --end 90 --json

The recording is read once for both mixes.
Exit codes: 0 ok · 1 measurement failed · 2 usage · 3 input unreadable · 4 no track matched")]
    Compare(CompareArgs),
    /// Serve the engine to an agent over MCP (stdio).
    #[cfg(feature = "mcp")]
    #[command(after_help = "\
Example (Claude Code):
  claude mcp add mixstack -- mixstack mcp --allow ~/Recordings --out-dir ~/mixstack-out

stdout carries the protocol; logs go to stderr. Reads only below --allow (and
--out-dir), writes only plain file names into --out-dir.")]
    Mcp(McpArgs),
    /// Print the JSON Schema of an input or output document.
    #[command(after_help = "\
Examples:
  mixstack schema render > mixstack.render-1.schema.json
  mixstack schema mix    # the template format, mixstack.mix/1")]
    Schema {
        /// Which document.
        #[arg(value_enum)]
        name: SchemaName,
    },
    /// Print a shell completion script.
    #[command(after_help = "\
Examples:
  mixstack completions zsh  > ~/.zfunc/_mixstack     # with fpath+=~/.zfunc
  mixstack completions bash > ~/.local/share/bash-completion/completions/mixstack
  mixstack completions fish > ~/.config/fish/completions/mixstack.fish")]
    Completions {
        /// The shell to generate for.
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Write man pages (mixstack.1 and one per command) into a folder.
    #[command(after_help = "\
Example:
  mixstack man --out-dir ~/.local/share/man/man1 && man mixstack-render")]
    Man {
        /// Target folder; created if missing. Existing pages are replaced.
        #[arg(long)]
        out_dir: PathBuf,
    },
}

/// The documents `mixstack schema` describes.
#[derive(Clone, Copy, ValueEnum)]
enum SchemaName {
    /// `mixstack.mix/1`, the template (input of render/measure/compare).
    Mix,
    /// `mixstack.probe/1`.
    Probe,
    /// `mixstack.render/1`.
    Render,
    /// `mixstack.measure/1`.
    Measure,
    /// `mixstack.compare/1`.
    Compare,
    /// `mixstack.error/1`, printed on stdout with --json when a command fails.
    Error,
}

/// `mixstack.error/1`: what stdout carries with --json when a command fails.
#[derive(serde::Serialize, JsonSchema)]
struct ErrorOutput {
    /// Always `mixstack.error/1`.
    schema: &'static str,
    /// The process exit code (1–5).
    code: u8,
    /// `render`, `usage`, `input`, `no_match` or `output`.
    kind: &'static str,
    /// Human-readable; the same text as on stderr.
    message: String,
    /// The release of this binary.
    engine: &'static str,
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
    /// What happens to tracks of the recording the mix does not know.
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
    /// The recording (WAV, RF64 or BW64).
    input: PathBuf,
    /// Template or session; without it, every track at unity (a fresh mix).
    #[arg(long)]
    mix: Option<PathBuf>,
    #[command(flatten)]
    flags: MeasureFlags,
}

#[derive(clap::Args)]
struct CompareArgs {
    /// The recording (WAV, RF64 or BW64).
    input: PathBuf,
    /// The first mix (template or session) — the "before".
    #[arg(long)]
    mix_a: PathBuf,
    /// The second mix — the "after"; deltas are b − a.
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
    /// The recording (WAV, RF64 or BW64). Never written.
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
    /// Fade-in length in milliseconds, from the trim-in point.
    #[arg(long, default_value_t = 0.0)]
    fade_in_ms: f64,
    /// Fade-out length in milliseconds, ending at the trim-out point.
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

/// The JSON Schema of one document, titled with its contract name.
fn schema_value(name: SchemaName) -> serde_json::Value {
    use durecmix_engine::headless::{CompareOutput, MeasureOutput, ProbeOutput, RenderOutput};
    use durecmix_engine::template::Template;
    let (id, schema) = match name {
        SchemaName::Mix => (TEMPLATE_SCHEMA, schemars::schema_for!(Template)),
        SchemaName::Probe => (PROBE_SCHEMA, schemars::schema_for!(ProbeOutput)),
        SchemaName::Render => (RENDER_SCHEMA, schemars::schema_for!(RenderOutput)),
        SchemaName::Measure => (MEASURE_SCHEMA, schemars::schema_for!(MeasureOutput)),
        SchemaName::Compare => (COMPARE_SCHEMA, schemars::schema_for!(CompareOutput)),
        SchemaName::Error => (ERROR_SCHEMA, schemars::schema_for!(ErrorOutput)),
    };
    let mut value = serde_json::to_value(&schema).expect("a schema serialises");
    if let Some(obj) = value.as_object_mut() {
        obj.insert("title".into(), id.into());
    }
    value
}

/// mixstack.1 plus mixstack-<command>.1 for every visible command.
fn write_man_pages(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    std::fs::create_dir_all(dir)?;
    let cmd = Cli::command().version(VERSION);
    let mut written = Vec::new();
    let mut page = |c: clap::Command, file: String| -> std::io::Result<()> {
        let mut buf = Vec::new();
        clap_mangen::Man::new(c).render(&mut buf)?;
        let path = dir.join(file);
        std::fs::write(&path, buf)?;
        written.push(path);
        Ok(())
    };
    page(cmd.clone(), "mixstack.1".into())?;
    for sub in cmd.get_subcommands().filter(|c| !c.is_hide_set()) {
        let name = format!("mixstack-{}", sub.get_name());
        page(
            sub.clone()
                .name(name.clone())
                .bin_name(format!("mixstack {}", sub.get_name()))
                .version(VERSION)
                .disable_version_flag(true),
            format!("{name}.1"),
        )?;
    }
    Ok(written)
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
        Some(Cmd::Schema { name }) => {
            print_json(&schema_value(*name));
            return ExitCode::SUCCESS;
        }
        Some(Cmd::Completions { shell }) => {
            clap_complete::generate(
                *shell,
                &mut Cli::command(),
                "mixstack",
                &mut std::io::stdout(),
            );
            return ExitCode::SUCCESS;
        }
        Some(Cmd::Man { out_dir }) => {
            return match write_man_pages(out_dir) {
                Ok(files) => {
                    for f in files {
                        println!("{}", f.display());
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("mixstack: {}: {e}", out_dir.display());
                    ExitCode::from(5)
                }
            };
        }
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
                "{}\n\nUsage: mixstack <command> … (see mixstack --help)",
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
                print_json(&ErrorOutput {
                    schema: ERROR_SCHEMA,
                    code,
                    kind: error_kind(&e),
                    message: e.to_string(),
                    engine: VERSION,
                });
            }
            ExitCode::from(code)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clap_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    /// docs/CLI.md is the human reference. It must name every visible
    /// command under its own `## mixstack <command>` heading, and every
    /// option of that command inside its section — an option added here
    /// and forgotten there fails this test instead of going undocumented.
    #[test]
    fn the_reference_documents_every_command_and_option() {
        let doc = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../docs/CLI.md"))
            .expect("docs/CLI.md exists");
        let cmd = Cli::command();
        let mut missing = Vec::new();
        for sub in cmd.get_subcommands().filter(|c| !c.is_hide_set()) {
            let heading = format!("## `mixstack {}`", sub.get_name());
            let Some(start) = doc.find(&heading) else {
                missing.push(heading);
                continue;
            };
            let rest = &doc[start + heading.len()..];
            let section = &rest[..rest.find("\n## ").unwrap_or(rest.len())];
            for arg in sub.get_arguments() {
                if arg.is_hide_set() {
                    continue;
                }
                if let Some(long) = arg.get_long() {
                    if long == "help" {
                        continue;
                    }
                    if !section.contains(&format!("--{long}")) {
                        missing.push(format!("{} --{long}", sub.get_name()));
                    }
                }
            }
        }
        for schema in [
            TEMPLATE_SCHEMA,
            PROBE_SCHEMA,
            RENDER_SCHEMA,
            MEASURE_SCHEMA,
            COMPARE_SCHEMA,
            ERROR_SCHEMA,
        ] {
            if !doc.contains(schema) {
                missing.push(schema.to_string());
            }
        }
        assert!(missing.is_empty(), "docs/CLI.md lacks: {missing:?}");
    }

    /// The exit codes in --help, docs/CLI.md and `exit_code` must agree.
    #[test]
    fn the_exit_codes_are_documented_where_they_are_defined() {
        let doc = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../docs/CLI.md"))
            .unwrap();
        for code in 0..=5 {
            assert!(
                exit_codes!().contains(&format!("  {code}  ")),
                "the --help exit-code table lacks {code}"
            );
            assert!(
                doc.contains(&format!("| `{code}` |")),
                "docs/CLI.md lacks exit code {code}"
            );
        }
    }

    #[test]
    fn every_schema_is_an_object_schema_titled_with_its_contract() {
        for name in SchemaName::value_variants() {
            let v = schema_value(*name);
            let title = v["title"].as_str().unwrap();
            assert!(
                title.starts_with("mixstack.") && title.ends_with("/1"),
                "{title}"
            );
            assert_eq!(v["type"], "object", "{title}");
            assert!(
                v["properties"].as_object().is_some_and(|p| !p.is_empty()),
                "{title}"
            );
        }
    }
}
