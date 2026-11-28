# `mixstack` — command-line reference

`mixstack` is the Mixstack engine without the app: it reads a multichannel
recording, applies a mix and writes a stereo file, measures mixes, and serves
all of it to AI agents over MCP. It is the **same engine** as the app — a
`mixstack render` is byte-identical to the app's export of the same mix, and
that is a tested guarantee (`cli_render_is_bit_identical_to_the_app_export`).

Input is any interleaved RIFF/RF64/BW64 WAV at any channel count; track names
come from the file's iXML. **The recording is never written**, not even with
`--force`.

This page is for people using the tool. The formats it reads and writes have
their own contracts: [TEMPLATE.md](TEMPLATE.md) (`mixstack.mix/1`) and
[MEASURE.md](MEASURE.md) (`mixstack.measure/1`, `mixstack.compare/1`); every
document also comes as a JSON Schema from [`mixstack schema`](#mixstack-schema).

- [Install](#install)
- [Concepts](#concepts): mixes, track mapping, output, progress, exit codes
- Commands: [`probe`](#mixstack-probe) · [`render`](#mixstack-render) ·
  [`measure`](#mixstack-measure) · [`compare`](#mixstack-compare) ·
  [`mcp`](#mixstack-mcp) · [`schema`](#mixstack-schema) ·
  [`completions`](#mixstack-completions) · [`man`](#mixstack-man)

## Install

**From a release** (macOS, Apple Silicon): download
`mixstack-cli-<version>-macos-arm64.zip` from the
[releases](https://github.com/MacBuchi/mixstack/releases), unpack it and put
`mixstack` on your `PATH`. The binary is not signed; on first start macOS
blocks it — allow it under *System Settings → Privacy & Security*, or run
`xattr -d com.apple.quarantine mixstack`. The zip carries the licence notices
and — from the release after 0.21.0 on — this reference, the man pages
(`man/`) and shell completions (`completions/`). With 0.21.0, generate them
yourself as shown below.

**From source** (any platform with a Rust toolchain):

```sh
cargo install --locked --path cli          # from a checkout of this repository
cargo install --locked --path cli --no-default-features   # without the MCP server
```

This installs into `~/.cargo/bin`. Then, optionally:

```sh
mixstack man --out-dir ~/.local/share/man/man1           # man mixstack-render
mixstack completions zsh > ~/.zfunc/_mixstack             # see `completions`
```

`mixstack --version` prints the release and every contract the binary
speaks; a script can check for the one it needs:

```text
mixstack 0.21.0 mixstack.mix/1 mixstack.probe/1 mixstack.render/1 mixstack.measure/1 mixstack.compare/1
```

The release number is the app's: CLI and app built from one commit report
the same version.

## Concepts

### Mixes: templates and sessions

Wherever a command takes a mix (`--mix`, `--mix-a`, `--mix-b`), it accepts
either

- a **template** (`mixstack.mix/1`, [TEMPLATE.md](TEMPLATE.md)): faders, pan,
  polarity, mute/solo/in-mix, EQ and the master settings (loudness target,
  limiter, mastering reference) — nothing tied to one recording, so one
  template serves many takes; or
- a **session** the app stored (`*.durecmix.json`), converted on the way in.

A minimal template:

```json
{ "schema": "mixstack.mix/1", "name": "Band",
  "tracks": [ {"name": "Vocals"}, {"name": "Kick", "gain_db": -3},
              {"name": "Keys L", "pan": -1}, {"name": "Keys R", "pan": 1} ],
  "settings": { "loudness": {"LufsIntegrated": -16.0} } }
```

Missing fields take their defaults (unity, centre, in the mix, EQ off;
−1 dBFS peak normalisation with the −1 dBTP limiter). `mixstack schema mix`
prints the complete format. The app writes templates, and so does the MCP
tool `save_template`.

### Track mapping

A template is applied to a recording's tracks in three rounds — **exact
name**, then **normalised name** (case and whitespace only), then
**position** — and every template track is used at most once. There is no
fuzzy matching: a wrong match would move the wrong fader without a word.
Every render and measurement reports the result:

```json
"mapping": {
  "matched": [ {"name": "Kick", "index": 2, "by": "name", "template_name": "Kick"} ],
  "unknown": ["Gtr", "Phones L"],
  "missing": []
}
```

`unknown` are tracks of the recording the template does not know. With
`--unknown-tracks exclude` (the default) they stay out of the mix — a new
channel is as likely a monitor feed as a source; `include` mixes them in at
unity. `missing` are template tracks the recording does not have. A partial
match is not an error; a mix that matches **no** track is (exit code 4).

Names are free text from the recording interface's configuration. The engine
reads nothing into them — no "this sounds like a vocal" logic anywhere.

### Output, progress and errors

- **stdout** carries the result: readable text, or with `--json` exactly one
  JSON document (`mixstack.probe/1`, `mixstack.render/1`,
  `mixstack.measure/1`, `mixstack.compare/1`). If the command fails, `--json`
  still prints one document, `mixstack.error/1`:

  ```json
  { "schema": "mixstack.error/1", "code": 5, "kind": "output",
    "message": "cannot write mix.mp3: it already exists (pass --force to overwrite)",
    "engine": "0.21.0" }
  ```

  `kind` is `render`, `usage`, `input`, `no_match` or `output`. A value that is
  −∞ in the engine (the loudness of silence) is `null` in JSON.
- **stderr** carries progress, as JSON lines of the overall progress 0…1 and
  the pass it belongs to (1 = analysis, 2 = render), at most one per percent:

  ```text
  {"progress":0.42,"pass":1}
  ```

  plus a human-readable line when a command fails. `--quiet` silences the
  progress.
- **Files** are written to a hidden `.<name>.partial` beside the target and
  renamed when complete, so no reader ever sees a truncated file under the
  final name.

### Exit codes

| Code | Meaning |
|---|---|
| `0` | Success. |
| `1` | The render or measurement failed inside the engine. |
| `2` | Usage: unknown option, bad value, range outside the recording, unsupported bitrate. |
| `3` | An input is unreadable: the recording, a mix or a mastering reference. |
| `4` | The mix matches no track of the recording. |
| `5` | The output cannot be written: it exists (without `--force`), it is the source, or its folder does not exist. |

### Time and loudness

- `--start`/`--end` are seconds from the start of the recording (rounded to
  the nearest sample); either may be omitted. An end past the recording is
  clamped to its end; a negative time, a start at or past the end of the
  recording, or an end not after the start is a usage error (exit code 2).
- `--loudness` overrides the template's target for one render:
  `lufs:-16` (integrated loudness, BS.1770), `peak:-1` (peak normalisation,
  dBFS) or `none`. The −1 dBTP true-peak limiter runs unless the template
  switches it off. If the template names a mastering reference, the render is
  matched to it instead and the loudness setting does not apply — exactly as
  in the app.

## `mixstack probe`

```text
mixstack probe <INPUT> [--json]
```

Reads only the header and iXML of a recording — instant even for a
multi-gigabyte file.

| Option | |
|---|---|
| `<INPUT>` | The recording. |
| `--json` | Print `mixstack.probe/1` instead of text. |

```text
$ mixstack probe take.wav
take.wav: 8 ch, 44100 Hz, 24-bit int, 5.000 s
    1  Vocals
    2  Kick
    3  Keys L
  …
```

The JSON carries `channels`, `sample_rate`, `bits_per_sample`,
`sample_format` (`int`/`float`), `frames`, `duration_seconds`, `has_ixml` and
`tracks[]` (`index` 1-based, `name`). Without iXML the tracks are
`Channel 1`, `Channel 2`, …

## `mixstack render`

```text
mixstack render <INPUT> --mix <MIX> --out <OUT> [options]
```

Renders the recording with a mix to a stereo file, through the app's export
chain: EQ, pan, normalisation or loudness target, true-peak limiter,
mastering, dither on 16-bit.

| Option | |
|---|---|
| `<INPUT>` | The recording. Never written. |
| `--mix <MIX>` | Template or session. |
| `--out <OUT>` | Target file. Its folder must exist. |
| `--format <F>` | `wav16`, `wav24`, `wav32f`, `flac16`, `flac24` or `mp3`. Default: from the extension of `--out` — `.wav` and `.flac` are 24-bit, `.mp3` is MP3. |
| `--start <S>`, `--end <S>` | Trim, in seconds. |
| `--fade-in-ms <MS>`, `--fade-out-ms <MS>` | Fades at the trim points. Default 0. |
| `--loudness <L>` | `lufs:<n>`, `peak:<n>` or `none`; overrides the template. |
| `--mp3-bitrate <K>` | 128, 192, 256 or 320 kbps (default 320). MP3 is **always constant bitrate**: browsers derive an MP3's length and seek positions from its bitrate, and only CBR gets that right. |
| `--unknown-tracks <U>` | `exclude` (default) or `include`, see [Track mapping](#track-mapping). |
| `--force` | Replace `--out` if it exists. |
| `--json` | Print `mixstack.render/1`. |
| `--quiet` | No progress on stderr. |

```sh
mixstack render take.wav --mix band.mixstack.json --out song.mp3 \
    --start 2.8 --end 120 --fade-out-ms 500 --loudness lufs:-16 --mp3-bitrate 128
```

`mixstack.render/1` reports the `range`, `duration_seconds`, the `mapping`
and the loudness `report` of the file as written — `integrated_lufs`,
`true_peak_dbtp`, `lra_lu`, `gain_applied_db`, `source_integrated_lufs` and,
with a mastering reference, `mastering_applied`/`mastering_gain_db`.

A whole 34-track song (UFX33, 205 s) renders in about 5 s on an Apple
Silicon Mac.

## `mixstack measure`

```text
mixstack measure <INPUT> [--mix <MIX>] [options]
```

Objective numbers for a mix and each of its tracks: loudness (LUFS-I, LRA,
true peak, PLR, percentiles), the long-term spectrum in third-octave bands,
masking of each track by the rest of the mix, stereo correlation and width,
activity. What every number means and how it is computed:
[MEASURE.md](MEASURE.md).

| Option | |
|---|---|
| `<INPUT>` | The recording. |
| `--mix <MIX>` | Template or session. Without it every track is measured at unity. |
| `--start <S>`, `--end <S>` | Measure an excerpt. Fades are never applied. |
| `--tracks <T>` | `mix` (default: the audible tracks), `all` (muted and out-of-mix ones too), or a comma-separated list of names. |
| `--track <NAME>` | One track by exact name; repeatable. For names containing a comma. Overrides `--tracks`. |
| `--masking <M>` | `vs-rest` (default: each track against the rest of the mix), `off`, or `pairs:<name,name,…>` to add a pairwise matrix for up to 12 tracks. |
| `--pre-fader` | Track levels before fader and pan (EQ applied) instead of their contribution to the mix. |
| `--unknown-tracks <U>` | `exclude` (default) or `include`. |
| `--json` | Print `mixstack.measure/1`. |
| `--quiet` | No progress on stderr. |

```text
$ mixstack measure take.wav --mix band.mixstack.json
take.wav [0.00–5.00 s]: mix -10.3 LUFS-I, LRA 0.0, -3.1 dBTP, PLR 7.2, corr 0.89, width 0.24
    1 Vocals                    -15.7 LUFS, active 100 %, masked 0 %
    2 Kick                      -16.1 LUFS, active 100 %, masked 0 %
  …
```

The mix is measured as the render's first pass hears it — before
normalisation — and the JSON includes the gain the render would apply. Cost
grows linearly with the track count: about 9× realtime at 34 tracks, 1.5× at
200.

## `mixstack compare`

```text
mixstack compare <INPUT> --mix-a <A> --mix-b <B> [options]
```

Measures two mixes of the same recording over the same range and reports the
differences **b − a**, reading the file only once. The JSON has the deltas
plus both full measurements and both mapping reports.

| Option | |
|---|---|
| `<INPUT>` | The recording. |
| `--mix-a <A>` | The first mix — "before". |
| `--mix-b <B>` | The second mix — "after". |
| `--start`, `--end`, `--tracks`, `--track`, `--masking`, `--pre-fader`, `--unknown-tracks`, `--json`, `--quiet` | As for [`measure`](#mixstack-measure). |

```text
$ mixstack compare take.wav --mix-a band.json --mix-b bass-down.json --start 1 --end 4
b − a: mix -1.09 LU, LRA +0.00, TP -1.23 dB, width +0.029
    1 Vocals                   +0.00 LU, masked +0 pp
  …
    6 Bass                     -6.00 LU, masked +0 pp
```

(`bass-down.json` is `band.json` with the bass fader 6 dB lower; `pp` are
percentage points of the time a track is masked.)

## `mixstack mcp`

```text
mixstack mcp --allow <DIR> [--allow <DIR>…] --out-dir <DIR> [--limits <FILE>]
```

Serves the engine to an AI agent (Claude Code, Claude Desktop, any MCP
client) over stdio: the agent loads a mix, measures it, changes faders and
EQ, compares, reverts and renders. Speaks MCP 2026-07-28 (stateless) and the
older 2025-11-25 handshake.

| Option | |
|---|---|
| `--allow <DIR>` | A folder the server may read recordings, templates and mastering references from, recursively. Repeatable; at least one. |
| `--out-dir <DIR>` | The only folder the server writes to — renders and saved templates. It is readable too, so a saved template can be loaded again. |
| `--limits <FILE>` | Optional JSON with tighter edit limits than the app's control ranges, for example `{"max_eq_gain_db": 6, "max_gain_step_db": 3}`. Keys: `gain_db` (`[min, max]`), `max_gain_step_db`, `max_eq_gain_db`, `max_eq_step_db`, `max_pan_step`. An unknown key is an error, so a typo cannot silently lift a limit. |

Register it with Claude Code:

```sh
mkdir -p ~/mixstack-out
claude mcp add mixstack -- mixstack mcp --allow ~/Recordings --out-dir ~/mixstack-out
```

Then ask in plain language, e.g. *"Load take.wav with band.mixstack.json,
measure 60–90 s, lower the bass by 2 dB, compare, and render it as MP3
128."* To try the server by hand: `npx @modelcontextprotocol/inspector
mixstack mcp --allow … --out-dir …`.

**Tools:**

| Tool | What it does |
|---|---|
| `probe_recording` | Header-only look at a recording. |
| `load_mix` | Apply a template, a session or a fresh mix to a recording; returns a `mix_id`, revision 0 and the mapping report. |
| `update_mix` | Apply a batch of operations (`set_gain`, `set_pan`, `set_eq`, `set_hpf`, `set_in_mix`, `set_mute`, `set_solo`, `set_polarity`) — all or nothing — and return the new revision. Ranges are the app's controls: fader −60…+6 dB, pan ±1, EQ ±15 dB. |
| `revert_mix` | Return to an earlier revision; it is appended, so the history keeps every step. |
| `measure` | Measure a revision (default: the newest), optionally an excerpt. |
| `compare_mixes` | Measure two revisions on the same excerpt and return the differences. |
| `render` | Render a revision into `--out-dir`; returns the loudness report and a `file://` link, never the audio itself. |
| `save_template` | Save a revision as a `mixstack.mix/1` template into `--out-dir`. |

**Safety.** Paths are checked after resolving `..` and symlinks: nothing
outside `--allow`/`--out-dir` can be read, nothing outside `--out-dir` can be
written, output names must be plain file names, an existing file is replaced
only with `overwrite: true`, and the server never writes through a symlink.
The recording is never modified. Mixes and their revisions live in the
server process; `save_template` is how a mix outlives it. stdout is the
protocol channel; diagnostics go to stderr.

## `mixstack schema`

```text
mixstack schema <mix|probe|render|measure|compare|error>
```

Prints the JSON Schema (draft 2020-12) of a document, generated from the
engine's own types — the same schemas the MCP server announces as
`outputSchema`. Use it to validate what the CLI prints, or to generate types
for a consumer:

```sh
mixstack schema render > mixstack.render-1.schema.json
```

`mix` is the input format (`mixstack.mix/1`); `error` is
`mixstack.error/1`. Each schema's `title` is its contract name.

## `mixstack completions`

```text
mixstack completions <bash|elvish|fish|powershell|zsh>
```

Prints a completion script for the shell:

```sh
mixstack completions zsh  > ~/.zfunc/_mixstack      # with fpath+=(~/.zfunc) before compinit
mixstack completions bash > ~/.local/share/bash-completion/completions/mixstack
mixstack completions fish > ~/.config/fish/completions/mixstack.fish
```

## `mixstack man`

```text
mixstack man --out-dir <DIR>
```

| Option | |
|---|---|
| `--out-dir <DIR>` | Target folder, created if missing. Existing pages are replaced. |

Writes `mixstack.1` and one page per command (`mixstack-render.1`, …), the
same text as `--help`. Prints the paths it wrote.

```sh
mixstack man --out-dir ~/.local/share/man/man1 && man mixstack-render
```

## Versioning

Every JSON document names its contract in `schema` (`mixstack.render/1`).
Within a major version fields are only ever added; a reader should ignore
fields it does not know. A breaking change gets a new major
(`mixstack.render/2`), never an edit in place — and `--version` lists which
majors a binary speaks.
