//! The `mixstack` binary end to end, on the gen_fixture take.
//!
//! The load-bearing test is `cli_render_is_bit_identical_to_the_app_export`:
//! the CLI exists so the band player's uploads sound like the app, and only
//! byte equality with `render_to_file` — the function the app's export runs
//! — proves that.

#[path = "../../engine/examples/gen_fixture.rs"]
mod gen_fixture;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use durecmix_engine::ixml::TrackInfo;
use durecmix_engine::render::{render_to_file, LoudnessMode, OutputFormat};
use durecmix_engine::session::Session;
use durecmix_engine::template::Template;

const BIN: &str = env!("CARGO_BIN_EXE_mixstack");

struct Fixture {
    dir: tempfile::TempDir,
    wav: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("take.wav");
        std::fs::write(&wav, gen_fixture::fixture_bytes()).unwrap();
        Fixture { dir, wav }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn tracks(&self) -> Vec<TrackInfo> {
        gen_fixture::TRACKS
            .iter()
            .enumerate()
            .map(|(i, (name, _, _))| TrackInfo {
                index: i as u32 + 1,
                name: name.to_string(),
            })
            .collect()
    }

    /// A session as the app would store it: EQ, a fader moved, the monitor
    /// buses out of the mix, LUFS normalisation.
    fn bent_session(&self) -> Session {
        let mut s = Session::from_track_info(&self.tracks());
        for t in &mut s.tracks {
            if t.name.starts_with("Phones") || t.name.starts_with("Out") {
                t.in_mix = false;
            }
        }
        s.tracks[0].gain_db = -4.5;
        s.tracks[0].eq.mid.enabled = true;
        s.tracks[0].eq.mid.gain_db = 3.0;
        s.tracks[1].eq.hpf_enabled = true;
        s.tracks[1].eq.hpf_freq = 90.0;
        s.tracks[5].polarity_invert = true;
        s.settings.loudness = LoudnessMode::LufsIntegrated(-16.0);
        s
    }

    fn write_json(&self, name: &str, json: &str) -> PathBuf {
        let p = self.path(name);
        std::fs::write(&p, json).unwrap();
        p
    }
}

fn run(args: &[&str]) -> Output {
    Command::new(BIN).args(args).output().unwrap()
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

fn stdout_json(o: &Output) -> serde_json::Value {
    serde_json::from_slice(&o.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not one JSON document ({e}):\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        )
    })
}

#[test]
fn version_names_the_release_and_every_schema() {
    let o = run(&["--version"]);
    assert!(o.status.success());
    let line = String::from_utf8(o.stdout).unwrap();
    assert!(line.starts_with("mixstack "), "{line}");
    for schema in ["mixstack.mix/1", "mixstack.probe/1", "mixstack.render/1"] {
        assert!(line.contains(schema), "{line}");
    }
}

#[test]
fn probe_reports_the_takes_tracks() {
    let f = Fixture::new();
    let o = run(&["probe", s(&f.wav), "--json"]);
    assert!(o.status.success());
    let v = stdout_json(&o);
    assert_eq!(v["schema"], "mixstack.probe/1");
    assert_eq!(v["channels"], 8);
    assert_eq!(v["sample_rate"], 44_100);
    assert_eq!(v["bits_per_sample"], 24);
    assert_eq!(v["frames"], 44_100 * 5);
    assert_eq!(v["has_ixml"], true);
    assert_eq!(v["tracks"][2]["name"], "Keys L");
    assert_eq!(v["tracks"][2]["index"], 3);
}

#[test]
fn cli_render_is_bit_identical_to_the_app_export() {
    let f = Fixture::new();
    let session = f.bent_session();
    let mix = f.write_json("take.durecmix.json", &session.to_json().unwrap());
    let out_cli = f.path("cli.wav");
    let o = run(&[
        "render",
        s(&f.wav),
        "--mix",
        s(&mix),
        "--out",
        s(&out_cli),
        "--start",
        "0.5",
        "--end",
        "4.25",
        "--fade-in-ms",
        "50",
        "--fade-out-ms",
        "500",
        "--json",
        "--quiet",
    ]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    // What the app's export does with the same session and the same range.
    let mut app = session.clone();
    app.settings.trim_start_frame = 22_050;
    app.settings.trim_end_frame = Some(187_425);
    app.settings.fade_in_ms = 50.0;
    app.settings.fade_out_ms = 500.0;
    app.settings.format = OutputFormat::Wav24;
    let out_app = f.path("app.wav");
    let report = render_to_file(&f.wav, &app.tracks, &app.settings, &out_app, |_| {}).unwrap();

    assert_eq!(
        std::fs::read(&out_cli).unwrap(),
        std::fs::read(&out_app).unwrap(),
        "CLI and app export differ"
    );
    let v = stdout_json(&o);
    assert_eq!(v["schema"], "mixstack.render/1");
    // serde_json parses floats to within an ulp, not exactly.
    assert!(
        (v["report"]["integrated_lufs"].as_f64().unwrap() - report.integrated_lufs).abs() < 1e-9
    );
    assert_eq!(v["range"]["start"], 0.5);
    assert_eq!(v["range"]["end"], 4.25);
    assert_eq!(v["mapping"]["matched"].as_array().unwrap().len(), 8);
}

#[test]
fn a_template_renders_like_the_session_it_was_made_from() {
    let f = Fixture::new();
    let session = f.bent_session();
    let from_session = f.write_json("s.durecmix.json", &session.to_json().unwrap());
    let mut t = Template::from_session(&session);
    t.name = "Band".into();
    let from_template = f.write_json("band.mixstack.json", &t.to_json().unwrap());
    let a = f.path("a.flac");
    let b = f.path("b.flac");
    for (mix, out) in [(&from_session, &a), (&from_template, &b)] {
        let o = run(&[
            "render",
            s(&f.wav),
            "--mix",
            s(mix),
            "--out",
            s(out),
            "--quiet",
        ]);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }
    assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
}

#[test]
fn stdout_carries_one_document_and_progress_goes_to_stderr() {
    let f = Fixture::new();
    let mix = f.write_json(
        "m.json",
        &Template::from_session(&f.bent_session()).to_json().unwrap(),
    );
    let out = f.path("o.wav");
    let o = run(&[
        "render",
        s(&f.wav),
        "--mix",
        s(&mix),
        "--out",
        s(&out),
        "--json",
    ]);
    assert!(o.status.success());
    stdout_json(&o);
    let stderr = String::from_utf8(o.stderr).unwrap();
    let lines: Vec<serde_json::Value> = stderr
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|_| panic!("not JSON: {l}")))
        .collect();
    assert!(lines.len() > 2);
    assert_eq!(lines.first().unwrap()["pass"], 1);
    assert_eq!(lines.last().unwrap()["pass"], 2);
    assert_eq!(lines.last().unwrap()["progress"], 1.0);
}

#[test]
fn mp3_is_constant_bitrate_at_the_requested_rate() {
    let f = Fixture::new();
    let mix = f.write_json(
        "m.json",
        &Template::from_session(&f.bent_session()).to_json().unwrap(),
    );
    let out = f.path("o.mp3");
    let o = run(&[
        "render",
        s(&f.wav),
        "--mix",
        s(&mix),
        "--out",
        s(&out),
        "--mp3-bitrate",
        "128",
        "--loudness",
        "lufs:-16",
        "--json",
        "--quiet",
    ]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(stdout_json(&o)["mp3_bitrate_kbps"], 128);

    // Walk every MPEG-1 Layer III frame header: one bitrate throughout is
    // what CBR means, and what lets a browser compute duration and seek.
    let bytes = std::fs::read(&out).unwrap();
    const KBPS: [u32; 16] = [
        0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
    ];
    let mut pos = 0;
    let mut frames = 0;
    while pos + 4 <= bytes.len() {
        let h = &bytes[pos..pos + 4];
        assert!(
            h[0] == 0xFF && (h[1] & 0xFE) == 0xFA,
            "lost frame sync at byte {pos}"
        );
        let kbps = KBPS[(h[2] >> 4) as usize];
        assert_eq!(kbps, 128, "frame {frames} at byte {pos} is {kbps} kbps");
        let padding = ((h[2] >> 1) & 1) as usize;
        pos += 144 * kbps as usize * 1000 / 44_100 + padding;
        frames += 1;
    }
    // 4.98 s of audio is ~191 frames of 1152 samples.
    assert!(frames > 180, "{frames} frames");
}

#[test]
fn exit_codes_follow_the_contract() {
    let f = Fixture::new();
    let mix = f.write_json(
        "m.json",
        &Template::from_session(&f.bent_session()).to_json().unwrap(),
    );
    let out = f.path("o.wav");

    // 2: usage — clap's own errors and the engine's range checks alike.
    assert_eq!(run(&["render", s(&f.wav)]).status.code(), Some(2));
    assert_eq!(
        run(&[
            "render",
            s(&f.wav),
            "--mix",
            s(&mix),
            "--out",
            s(&f.path("o.ogg"))
        ])
        .status
        .code(),
        Some(2)
    );
    assert_eq!(
        run(&[
            "render",
            s(&f.wav),
            "--mix",
            s(&mix),
            "--out",
            s(&out),
            "--start",
            "9"
        ])
        .status
        .code(),
        Some(2)
    );
    assert_eq!(
        run(&[
            "render",
            s(&f.wav),
            "--mix",
            s(&mix),
            "--out",
            s(&out),
            "--start",
            "2",
            "--end",
            "1"
        ])
        .status
        .code(),
        Some(2)
    );
    assert_eq!(
        run(&[
            "render",
            s(&f.wav),
            "--mix",
            s(&mix),
            "--out",
            s(&out),
            "--mp3-bitrate",
            "100"
        ])
        .status
        .code(),
        Some(2)
    );

    // 3: input unreadable — the take, or the mix.
    assert_eq!(
        run(&["probe", s(&f.path("nope.wav"))]).status.code(),
        Some(3)
    );
    let garbage = f.write_json("bad.json", "{ not json");
    assert_eq!(
        run(&["render", s(&f.wav), "--mix", s(&garbage), "--out", s(&out)])
            .status
            .code(),
        Some(3)
    );

    // 4: the mix belongs to another setup.
    let foreign = f.write_json(
        "foreign.json",
        r#"{"schema":"mixstack.mix/1","tracks":[{"name":"Tuba"}]}"#,
    );
    let o = run(&[
        "render",
        s(&f.wav),
        "--mix",
        s(&foreign),
        "--out",
        s(&out),
        "--json",
    ]);
    assert_eq!(o.status.code(), Some(4));
    let v = stdout_json(&o);
    assert_eq!(v["schema"], "mixstack.error/1");
    assert_eq!(v["kind"], "no_match");
    assert!(!out.exists(), "nothing may be written on a refused render");

    // 5: output not writable — exists without --force, or is the source.
    std::fs::write(&out, b"keep me").unwrap();
    assert_eq!(
        run(&["render", s(&f.wav), "--mix", s(&mix), "--out", s(&out)])
            .status
            .code(),
        Some(5)
    );
    assert_eq!(std::fs::read(&out).unwrap(), b"keep me");
    let before = std::fs::read(&f.wav).unwrap();
    assert_eq!(
        run(&[
            "render",
            s(&f.wav),
            "--mix",
            s(&mix),
            "--out",
            s(&f.wav),
            "--format",
            "wav24",
            "--force"
        ])
        .status
        .code(),
        Some(5)
    );
    assert_eq!(
        std::fs::read(&f.wav).unwrap(),
        before,
        "the source was touched"
    );
    assert_eq!(
        run(&[
            "render",
            s(&f.wav),
            "--mix",
            s(&mix),
            "--out",
            s(&f.path("no/such/dir/o.wav"))
        ])
        .status
        .code(),
        Some(5)
    );

    // --force overwrites, and leaves no partial file behind.
    let o = run(&[
        "render",
        s(&f.wav),
        "--mix",
        s(&mix),
        "--out",
        s(&out),
        "--force",
        "--quiet",
    ]);
    assert!(o.status.success());
    assert_ne!(std::fs::read(&out).unwrap(), b"keep me");
    let leftovers: Vec<_> = std::fs::read_dir(f.dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".partial"))
        .collect();
    assert!(leftovers.is_empty());
}

#[test]
fn partial_mapping_is_reported_not_refused() {
    let f = Fixture::new();
    let mut t = Template::from_session(&f.bent_session());
    t.tracks.truncate(3);
    t.tracks[0].index = 0;
    t.tracks[0].name = "Vocals".into();
    t.tracks.push(durecmix_engine::template::TemplateTrack {
        name: "Theremin".into(),
        ..Default::default()
    });
    let mix = f.write_json("m.json", &t.to_json().unwrap());
    let out = f.path("o.wav");
    let o = run(&[
        "render",
        s(&f.wav),
        "--mix",
        s(&mix),
        "--out",
        s(&out),
        "--json",
        "--quiet",
    ]);
    assert!(o.status.success());
    let v = stdout_json(&o);
    assert_eq!(v["mapping"]["matched"].as_array().unwrap().len(), 3);
    assert_eq!(v["mapping"]["unknown"].as_array().unwrap().len(), 5);
    assert_eq!(v["mapping"]["missing"][0], "Theremin");
}

#[test]
fn measure_prints_one_measure_document() {
    let f = Fixture::new();
    let mix = f.write_json(
        "m.json",
        &Template::from_session(&f.bent_session()).to_json().unwrap(),
    );
    let o = run(&[
        "measure",
        s(&f.wav),
        "--mix",
        s(&mix),
        "--start",
        "1",
        "--end",
        "3",
        "--json",
        "--quiet",
    ]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v = stdout_json(&o);
    assert_eq!(v["schema"], "mixstack.measure/1");
    assert_eq!(v["range"]["start"], 1.0);
    assert_eq!(v["range"]["end"], 3.0);
    assert_eq!(v["bands_hz"].as_array().unwrap().len(), 30);
    assert_eq!(v["level_point"], "post_fader");
    // The bent session leaves the two monitor buses out: 6 audible tracks.
    assert_eq!(v["tracks"].as_array().unwrap().len(), 6);
    let vocals = &v["tracks"][0];
    assert_eq!(vocals["name"], "Vocals");
    assert!(vocals["lufs_i"].is_number());
    assert_eq!(vocals["ltas_db"].as_array().unwrap().len(), 30);
    assert_eq!(vocals["masking"]["msr_db"].as_array().unwrap().len(), 30);
    assert!(v["mix"]["correlation"].is_number());
    assert!(v["mix"]["normalisation_gain_db"].is_number());
    assert_eq!(v["mapping"]["matched"].as_array().unwrap().len(), 8);
}

#[test]
fn measure_without_a_mix_measures_every_track_at_unity() {
    let f = Fixture::new();
    let o = run(&[
        "measure",
        s(&f.wav),
        "--json",
        "--quiet",
        "--masking",
        "off",
    ]);
    assert!(o.status.success());
    let v = stdout_json(&o);
    assert_eq!(v["tracks"].as_array().unwrap().len(), 8);
    assert!(v["tracks"][0].get("masking").is_none());
}

#[test]
fn compare_reports_the_change_of_a_fader() {
    let f = Fixture::new();
    let a = f.bent_session();
    let mut b = a.clone();
    b.tracks[5].gain_db -= 6.0; // Bass
    let ma = f.write_json("a.json", &Template::from_session(&a).to_json().unwrap());
    let mb = f.write_json("b.json", &Template::from_session(&b).to_json().unwrap());
    let o = run(&[
        "compare",
        s(&f.wav),
        "--mix-a",
        s(&ma),
        "--mix-b",
        s(&mb),
        "--json",
        "--quiet",
    ]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v = stdout_json(&o);
    assert_eq!(v["schema"], "mixstack.compare/1");
    let bass = v["delta"]["tracks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "Bass")
        .unwrap();
    assert!(
        (bass["lufs_i"].as_f64().unwrap() + 6.0).abs() < 0.01,
        "{bass}"
    );
    let vocals = &v["delta"]["tracks"][0];
    assert_eq!(vocals["lufs_i"], 0.0);
    assert!(v["delta"]["mix"]["lufs_i"].as_f64().unwrap() < 0.0);
    assert!(v["a"]["schema"] == "mixstack.measure/1" && v["b"]["schema"] == "mixstack.measure/1");
}

#[test]
fn measure_usage_errors_exit_2() {
    let f = Fixture::new();
    let o = run(&[
        "measure",
        s(&f.wav),
        "--tracks",
        "Vocals,Nope",
        "--json",
        "--quiet",
    ]);
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(stdout_json(&o)["kind"], "usage");
    let thirteen = (0..13)
        .map(|i| format!("T{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let o = run(&[
        "measure",
        s(&f.wav),
        "--masking",
        &format!("pairs:{thirteen}"),
        "--quiet",
    ]);
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(
        run(&["measure", s(&f.wav), "--masking", "everything"])
            .status
            .code(),
        Some(2)
    );
    // Names with commas go through repeated --track.
    let o = run(&[
        "measure",
        s(&f.wav),
        "--track",
        "Keys L",
        "--track",
        "Keys R",
        "--json",
        "--quiet",
    ]);
    assert!(o.status.success());
    assert_eq!(stdout_json(&o)["tracks"].as_array().unwrap().len(), 2);
}
