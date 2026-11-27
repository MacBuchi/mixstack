//! `mixstack mcp` end to end: an rmcp client talks to the real binary over
//! stdio, on the gen_fixture take — the whole loop of #167 (load → measure →
//! update → compare → revert → render → save) plus the guards around it.
#![cfg(feature = "mcp")]

#[path = "../../engine/examples/gen_fixture.rs"]
mod gen_fixture;

use std::path::{Path, PathBuf};

use durecmix_engine::ixml::TrackInfo;
use durecmix_engine::session::Session;
use durecmix_engine::template::Template;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_mixstack");

struct Env {
    _tmp: tempfile::TempDir,
    takes: PathBuf,
    out: PathBuf,
    private: PathBuf,
    wav: PathBuf,
    template: PathBuf,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let takes = tmp.path().join("takes");
    let out = tmp.path().join("out");
    let private = tmp.path().join("private");
    for d in [&takes, &out, &private] {
        std::fs::create_dir(d).unwrap();
    }
    let wav = takes.join("take.wav");
    std::fs::write(&wav, gen_fixture::fixture_bytes()).unwrap();
    std::fs::write(private.join("other.wav"), gen_fixture::fixture_bytes()).unwrap();
    let infos: Vec<TrackInfo> = gen_fixture::TRACKS
        .iter()
        .enumerate()
        .map(|(i, (n, _, _))| TrackInfo {
            index: i as u32 + 1,
            name: n.to_string(),
        })
        .collect();
    let mut s = Session::from_track_info(&infos);
    for t in &mut s.tracks {
        t.in_mix = !(t.name.starts_with("Phones") || t.name.starts_with("Out"));
    }
    let mut t = Template::from_session_with_tags(&s, &[vec!["gesang".into()]]);
    t.name = "Band".into();
    let template = takes.join("band.mixstack.json");
    std::fs::write(&template, t.to_json().unwrap()).unwrap();
    Env {
        takes: std::fs::canonicalize(&takes).unwrap(),
        out: std::fs::canonicalize(&out).unwrap(),
        private: std::fs::canonicalize(&private).unwrap(),
        wav: std::fs::canonicalize(&wav).unwrap(),
        template: std::fs::canonicalize(&template).unwrap(),
        _tmp: tmp,
    }
}

async fn connect(e: &Env) -> RunningService<RoleClient, ()> {
    let (takes, out) = (e.takes.clone(), e.out.clone());
    let transport = TokioChildProcess::new(tokio::process::Command::new(BIN).configure(|cmd| {
        cmd.arg("mcp")
            .arg("--allow")
            .arg(&takes)
            .arg("--out-dir")
            .arg(&out);
    }))
    .unwrap();
    ().serve(transport).await.unwrap()
}

async fn call(
    c: &RunningService<RoleClient, ()>,
    tool: &'static str,
    args: Value,
) -> CallToolResult {
    let Value::Object(map) = args else {
        panic!("args must be an object")
    };
    c.call_tool(CallToolRequestParams::new(tool).with_arguments(map))
        .await
        .unwrap_or_else(|e| panic!("{tool}: protocol error {e}"))
}

/// The structured result of a successful call.
async fn ok(c: &RunningService<RoleClient, ()>, tool: &'static str, args: Value) -> Value {
    let r = call(c, tool, args).await;
    assert_ne!(r.is_error, Some(true), "{tool} failed: {:?}", r.content);
    r.structured_content
        .unwrap_or_else(|| panic!("{tool}: no structured content"))
}

/// The message of a tool error — must be a tool error, not a protocol one.
async fn tool_error(c: &RunningService<RoleClient, ()>, tool: &'static str, args: Value) -> String {
    let r = call(c, tool, args).await;
    assert_eq!(r.is_error, Some(true), "{tool} should have failed");
    serde_json::to_string(&r.content).unwrap()
}

fn p(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[tokio::test]
async fn tools_are_listed_with_schemas_and_honest_annotations() {
    let e = env();
    let c = connect(&e).await;
    assert_eq!(
        c.peer_info().unwrap().protocol_version,
        rmcp::model::ProtocolVersion::V_2025_11_25,
        "older clients still get the initialize handshake"
    );
    let tools = c.list_all_tools().await.unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert_eq!(
        names,
        [
            "compare_mixes",
            "load_mix",
            "measure",
            "probe_recording",
            "render",
            "revert_mix",
            "save_template",
            "update_mix"
        ],
        "eight tools, in a stable order"
    );
    for t in &tools {
        assert!(t.output_schema.is_some(), "{} has no outputSchema", t.name);
        let a = t.annotations.as_ref().unwrap();
        assert_eq!(
            a.open_world_hint,
            Some(false),
            "{}: everything is local",
            t.name
        );
        let read_only = matches!(
            t.name.as_ref(),
            "probe_recording" | "load_mix" | "measure" | "compare_mixes"
        );
        assert_eq!(a.read_only_hint, Some(read_only), "{}", t.name);
        let writes_files = matches!(t.name.as_ref(), "render" | "save_template");
        assert_eq!(a.destructive_hint, Some(writes_files), "{}", t.name);
    }
    let info = c.peer_info().unwrap();
    assert!(info.instructions.as_deref().unwrap().contains("load_mix"));
    c.cancel().await.unwrap();
}

#[tokio::test]
async fn the_whole_loop_load_measure_update_compare_revert_render_save() {
    let e = env();
    let c = connect(&e).await;

    let probe = ok(&c, "probe_recording", json!({ "path": p(&e.wav) })).await;
    assert_eq!(probe["schema"], "mixstack.probe/1");
    assert_eq!(probe["channels"], 8);

    let load = ok(
        &c,
        "load_mix",
        json!({ "recording": p(&e.wav), "mix": p(&e.template) }),
    )
    .await;
    let mix_id = load["mix_id"].as_str().unwrap().to_string();
    assert_eq!(load["revision"], 0);
    assert_eq!(load["tracks"].as_array().unwrap().len(), 8);
    assert_eq!(load["tracks"][0]["tags"], json!(["gesang"]));
    assert_eq!(load["mapping"]["matched"].as_array().unwrap().len(), 8);

    let m = ok(
        &c,
        "measure",
        json!({ "mix_id": mix_id, "start": 1.0, "end": 3.0 }),
    )
    .await;
    assert_eq!(m["schema"], "mixstack.measure/1");
    assert_eq!(m["revision"], 0);
    assert_eq!(
        m["tracks"].as_array().unwrap().len(),
        6,
        "monitor buses are out of this mix"
    );
    let bass_before = m["tracks"][5]["lufs_i"].as_f64().unwrap();

    let up = ok(
        &c,
        "update_mix",
        json!({ "mix_id": mix_id, "operations": [
            { "op": "set_gain", "track": "Bass", "gain_db": -6.0 },
            { "op": "set_eq", "track": "Vocals", "band": "mid", "gain_db": 2.0, "freq": 3000.0 }
        ]}),
    )
    .await;
    assert_eq!(up["revision"], 1);
    assert_eq!(up["applied"].as_array().unwrap().len(), 2);

    // Out of range → a tool error the model can read, and no new revision.
    let msg = tool_error(
        &c,
        "update_mix",
        json!({ "mix_id": mix_id, "operations": [{ "op": "set_gain", "track": "Bass", "gain_db": 9.0 }] }),
    )
    .await;
    assert!(msg.contains("outside"), "{msg}");

    let cmp = ok(
        &c,
        "compare_mixes",
        json!({ "mix_id": mix_id, "revision_a": 0, "start": 1.0, "end": 3.0, "tracks": ["Bass", "Vocals"] }),
    )
    .await;
    assert_eq!(cmp["schema"], "mixstack.compare/1");
    assert_eq!(cmp["revision_b"], 1);
    let bass = cmp["delta"]["tracks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "Bass")
        .unwrap();
    assert!(
        (bass["lufs_i"].as_f64().unwrap() + 6.0).abs() < 0.01,
        "{bass}"
    );

    let rv = ok(&c, "revert_mix", json!({ "mix_id": mix_id, "revision": 0 })).await;
    assert_eq!(rv["revision"], 2);
    let m2 = ok(
        &c,
        "measure",
        json!({ "mix_id": mix_id, "start": 1.0, "end": 3.0, "masking": "off" }),
    )
    .await;
    assert_eq!(m2["revision"], 2);
    assert_eq!(m2["tracks"][5]["lufs_i"].as_f64().unwrap(), bass_before);

    let r = call(
        &c,
        "render",
        json!({ "mix_id": mix_id, "file_name": "excerpt.mp3", "start": 0.5, "end": 4.0,
                "loudness": { "LufsIntegrated": -16.0 }, "mp3_bitrate_kbps": 128 }),
    )
    .await;
    assert_ne!(r.is_error, Some(true), "{:?}", r.content);
    let sc = r.structured_content.clone().unwrap();
    assert_eq!(sc["schema"], "mixstack.render/1");
    assert_eq!(sc["mp3_bitrate_kbps"], 128);
    assert!((sc["report"]["integrated_lufs"].as_f64().unwrap() + 16.0).abs() < 0.5);
    let link = r
        .content
        .iter()
        .find_map(|c| c.as_resource_link())
        .expect("render answers with a resource_link, not audio");
    let rendered = e.out.join("excerpt.mp3");
    assert!(rendered.is_file());
    assert_eq!(link.uri, format!("file://{}", p(&rendered)));
    assert_eq!(link.mime_type.as_deref(), Some("audio/mpeg"));

    // An existing file is not replaced unless asked.
    tool_error(
        &c,
        "render",
        json!({ "mix_id": mix_id, "file_name": "excerpt.mp3" }),
    )
    .await;
    ok(
        &c,
        "render",
        json!({ "mix_id": mix_id, "file_name": "excerpt.mp3", "overwrite": true, "end": 1.0 }),
    )
    .await;

    let saved = ok(
        &c,
        "save_template",
        json!({ "mix_id": mix_id, "revision": 1, "file_name": "tuned.mixstack.json", "name": "Tuned" }),
    )
    .await;
    let path = saved["path"].as_str().unwrap();
    let t = Template::load(Path::new(path)).unwrap();
    assert_eq!(t.name, "Tuned");
    assert_eq!(t.tracks[5].gain_db, -6.0);
    assert_eq!(
        t.tracks[0].tags,
        vec!["gesang".to_string()],
        "tags survive the loop"
    );
    assert!(t.created.as_deref().unwrap().ends_with('Z'));

    // The saved template loads again from the output folder.
    let again = ok(
        &c,
        "load_mix",
        json!({ "recording": p(&e.wav), "mix": path }),
    )
    .await;
    assert_eq!(again["tracks"][5]["gain_db"], -6.0);
    c.cancel().await.unwrap();
}

#[tokio::test]
async fn paths_outside_the_allowed_folders_are_tool_errors() {
    let e = env();
    let c = connect(&e).await;
    let outside = p(&e.private.join("other.wav"));
    let msg = tool_error(&c, "probe_recording", json!({ "path": outside })).await;
    assert!(msg.contains("outside the allowed folders"), "{msg}");
    let sneaky = format!("{}/../private/other.wav", p(&e.takes));
    tool_error(&c, "probe_recording", json!({ "path": sneaky })).await;
    tool_error(&c, "load_mix", json!({ "recording": "take.wav" })).await;
    #[cfg(unix)]
    {
        let link = e.takes.join("link.wav");
        std::os::unix::fs::symlink(e.private.join("other.wav"), &link).unwrap();
        tool_error(&c, "probe_recording", json!({ "path": p(&link) })).await;
    }

    let load = ok(&c, "load_mix", json!({ "recording": p(&e.wav) })).await;
    let mix_id = load["mix_id"].as_str().unwrap();
    for bad in [
        "../escape.wav",
        "/tmp/escape.wav",
        "sub/x.wav",
        ".hidden.wav",
    ] {
        tool_error(&c, "render", json!({ "mix_id": mix_id, "file_name": bad })).await;
        tool_error(
            &c,
            "save_template",
            json!({ "mix_id": mix_id, "file_name": bad }),
        )
        .await;
    }
    assert_eq!(
        std::fs::read_dir(&e.out).unwrap().count(),
        0,
        "nothing was written"
    );
    // Unknown mix and unknown revision are tool errors too.
    tool_error(&c, "measure", json!({ "mix_id": "mix-999" })).await;
    tool_error(&c, "measure", json!({ "mix_id": mix_id, "revision": 42 })).await;
    c.cancel().await.unwrap();
}

/// A client that records the progress notifications it receives.
#[derive(Clone, Default)]
struct ProgressClient {
    seen: std::sync::Arc<std::sync::Mutex<Vec<f64>>>,
}

impl rmcp::ClientHandler for ProgressClient {
    async fn on_progress(
        &self,
        params: rmcp::model::ProgressNotificationParam,
        _context: rmcp::service::NotificationContext<RoleClient>,
    ) {
        self.seen.lock().unwrap().push(params.progress);
    }
}

#[tokio::test]
async fn speaks_2026_07_28_and_reports_progress_on_long_calls() {
    use rmcp::model::{ClientRequest, ProtocolVersion, Request};
    let e = env();
    let (takes, out) = (e.takes.clone(), e.out.clone());
    let transport = TokioChildProcess::new(tokio::process::Command::new(BIN).configure(|cmd| {
        cmd.arg("mcp")
            .arg("--allow")
            .arg(&takes)
            .arg("--out-dir")
            .arg(&out);
    }))
    .unwrap();
    let client = ProgressClient::default();
    let seen = client.seen.clone();
    // The stateless 2026-07-28 startup (`server/discover`, no initialize).
    // The other tests connect with rmcp's default, the 2025-11-25
    // initialize handshake, so both paths stay covered.
    let c = rmcp::ClientServiceExt::serve_with_lifecycle(
        client,
        transport,
        rmcp::ClientLifecycleMode::Discover {
            preferred_versions: vec![ProtocolVersion::V_2026_07_28],
        },
    )
    .await
    .unwrap();
    assert_eq!(
        c.peer_info().unwrap().protocol_version,
        ProtocolVersion::V_2026_07_28
    );

    let Value::Object(args) = json!({ "recording": p(&e.wav) }) else {
        unreachable!()
    };
    let load = c
        .call_tool(CallToolRequestParams::new("load_mix").with_arguments(args))
        .await
        .unwrap();
    let mix_id = load.structured_content.unwrap()["mix_id"]
        .as_str()
        .unwrap()
        .to_string();

    let Value::Object(args) = json!({ "mix_id": mix_id, "file_name": "full.wav" }) else {
        unreachable!()
    };
    let handle = c
        .send_cancellable_request(
            ClientRequest::CallToolRequest(Request::new(
                CallToolRequestParams::new("render").with_arguments(args),
            )),
            rmcp::service::PeerRequestOptions::no_options(),
        )
        .await
        .unwrap();
    handle.await_response().await.unwrap();
    // Notifications may trail the response by a moment.
    for _ in 0..50 {
        if seen.lock().unwrap().last() == Some(&1.0) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let seen = seen.lock().unwrap().clone();
    assert!(seen.len() > 3, "{seen:?}");
    assert!(
        seen.windows(2).all(|w| w[0] <= w[1]),
        "progress only grows: {seen:?}"
    );
    assert_eq!(seen.last(), Some(&1.0));
    c.cancel().await.unwrap();
}
