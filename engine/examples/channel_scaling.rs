//! What does a take with *this* many channels cost to play?
//!
//! Usage: cargo run -p durecmix-engine --release --example channel_scaling [channels] [muted_share]
//!        cargo run -p durecmix-engine --release --example channel_scaling 200 0.75
//!
//! **34 is this user's recordings, not a limit.** The channel count comes from
//! the attached interface — a UCX II records up to 40, a UFX II/III more, and a
//! multisample recording reaches a few hundred. Anything that scales worse than
//! linearly with the channel count only shows up out there, so this measures it
//! rather than assuming.
//!
//! Two numbers, because they fail differently: mixing+metering has to stay
//! **above realtime** or live playback cannot keep up at all, while a parameter
//! change has to stay cheap enough to happen on every pointer move of a fader.
//!
//! A third number covers the measurement module (#166): every audible track
//! measured with masking against the rest of the mix, which is what the
//! band player's closed loop runs. It is offline work, so it only has to be
//! linear — the ×-realtime figure per track should stay flat as the channel
//! count grows.

use durecmix_engine::chain::MasterParams;
use durecmix_engine::measure::{MeasureJob, MeasureOptions, MeasureStream, TrackSelection};
use durecmix_engine::mix::TrackParams;
use durecmix_engine::preview::PreviewStage;

/// What the measurement reads per block — the render's block size.
const BLOCK: usize = durecmix_engine::render::BLOCK_FRAMES;

fn main() {
    let mut args = std::env::args().skip(1);
    let channels: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(200);
    let muted_share: f64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(0.75);
    let sr = 48_000u32;
    let frames = 1024usize; // what the player pulls per callback
    let seconds = 10.0;

    let mut tracks: Vec<TrackParams> = (1..=channels)
        .map(|i| TrackParams::new(i as u32, format!("T{i}"), 0.0))
        .collect();
    // Muted tracks are the interesting case: they are metered but not mixed,
    // so they exercise the path that has no channel strip behind it.
    let keep_every = if muted_share >= 1.0 {
        usize::MAX
    } else {
        (1.0 / (1.0 - muted_share)).round().max(1.0) as usize
    };
    let mut muted = 0;
    for (i, t) in tracks.iter_mut().enumerate() {
        if keep_every == usize::MAX || i % keep_every != 0 {
            t.muted = true;
            muted += 1;
        }
    }

    let master = MasterParams {
        limiter_enabled: true,
        ceiling_dbtp: -1.0,
    };
    let mut stage = PreviewStage::new(channels, sr, &tracks, master, None);
    let block: Vec<f64> = (0..frames * channels)
        .map(|i| ((i % 977) as f64 / 977.0 - 0.5) * 0.5)
        .collect();
    let rounds = (sr as f64 * seconds) as usize / frames;

    let t0 = std::time::Instant::now();
    for _ in 0..rounds {
        stage.process(&block);
    }
    let mixing = t0.elapsed().as_secs_f64();

    let t1 = std::time::Instant::now();
    for _ in 0..rounds {
        stage.set_params(&tracks, master, None);
    }
    let params = t1.elapsed().as_secs_f64();

    println!(
        "{channels} channels, {muted} muted, {seconds:.0} s of audio in {frames}-frame blocks"
    );
    println!(
        "  mix + meters:  {mixing:.3} s  →  {:.1}× realtime",
        seconds / mixing
    );
    println!(
        "  {rounds} parameter changes: {params:.3} s  →  {:.3} ms each",
        params * 1000.0 / rounds as f64
    );
    if seconds / mixing < 2.0 {
        println!(
            "  ⚠️  below 2× realtime — live playback has no headroom left here; \
             export still works, it is simply not realtime-bound"
        );
    }

    // Measurement: the audible tracks against the rest of the mix, plus a
    // second pass with every track measured (muted ones read as silence and
    // skip their spectra, so this mainly shows the per-track floor).
    let settings = durecmix_engine::render::RenderSettings::default();
    let measure_seconds = 5.0;
    let mrounds = (sr as f64 * measure_seconds) as usize / BLOCK;
    let big: Vec<f64> = (0..BLOCK * channels)
        .map(|i| ((i.wrapping_mul(2_654_435_761) % 1_000_003) as f64 / 1_000_003.0 - 0.5) * 0.5)
        .collect();
    for (label, selection) in [
        ("audible", TrackSelection::Mix),
        ("all", TrackSelection::All),
    ] {
        let jobs = [MeasureJob {
            tracks: &tracks,
            tags: &[],
            settings: &settings,
        }];
        let opts = MeasureOptions {
            tracks: selection,
            ..MeasureOptions::default()
        };
        let mut stream = MeasureStream::new(&jobs, channels, sr, 0, &opts).unwrap();
        let t2 = std::time::Instant::now();
        for _ in 0..mrounds {
            stream.push_frames(&big);
        }
        let m = stream.finish();
        let took = t2.elapsed().as_secs_f64();
        let audio = (mrounds * BLOCK) as f64 / sr as f64;
        println!(
            "  measure ({label}, {} tracks, masking vs rest): {took:.3} s for {audio:.1} s  →  {:.1}× realtime",
            m[0].tracks.len(),
            audio / took
        );
    }
}
