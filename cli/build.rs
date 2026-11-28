//! Take the release version from pubspec.yaml — the single place a release
//! bumps it — so `mixstack --version` names the same release as the app
//! built from this commit.

fn main() {
    let pubspec = concat!(env!("CARGO_MANIFEST_DIR"), "/../pubspec.yaml");
    println!("cargo:rerun-if-changed={pubspec}");
    let version = std::fs::read_to_string(pubspec)
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("version:"))
                .map(|v| v.trim().split('+').next().unwrap_or("").to_string())
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "0.0.0-unknown".into());
    println!("cargo:rustc-env=MIXSTACK_VERSION={version}");
}
