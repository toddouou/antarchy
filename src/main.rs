mod api;
mod auth;
mod config;
mod email;
mod fog;
mod handlers;
mod metrics;
mod network;
mod persist;
mod regions;
mod server;
mod session;
mod simulation;
mod sms;
mod snapshot;
mod tile_map;
mod world;

use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

use server::{sim_loop, viewport_loop, run, WorldState, Cmd};
use world::World;
use config::cfg;

#[cfg(windows)]
#[link(name = "winmm")]
extern "system" { fn timeBeginPeriod(uPeriod: u32) -> u32; }

#[tokio::main]
async fn main() {
    #[cfg(windows)]
    unsafe { timeBeginPeriod(1); }

    // Parse + project region data (metros + country geojson) once, before serving — so the
    // first queen placement never pays the geojson parse under the world lock.
    regions::init();

    // Restore persisted state (accounts + world) so a restart / Railway redeploy resumes where it
    // left off. Both files live under HIVE_DATA_DIR (see config). Missing/corrupt → fresh start.
    let mut w = World::new();
    let save_file = cfg().save_file.clone();
    w.auth = auth::Auth::load(&save_file);
    match persist::load(&save_file) {
        Some(snap) => persist::restore(&mut w, snap),
        None       => println!("[persist] no snapshot at {save_file} — fresh start"),
    }
    // ∥A: when the write-ahead log is enabled, replay any journal recorded since the base snapshot.
    if config::wal_enabled() {
        let n = persist::replay_wal(&mut w, &persist::wal_path(&save_file));
        if n > 0 { println!("[persist] WAL replay: {n} chunk deltas applied"); }
    }
    let world: WorldState = Arc::new(RwLock::new(w));

    // Scope the config read-guard so it is provably dropped before the `.await` below
    // (an RwLockReadGuard must not be held across an await point).
    let (port, world_w, world_h, spawn_x, spawn_y, tick) = {
        let c = cfg();
        (c.port, c.world_w, c.world_h, c.spawn_x, c.spawn_y, c.tick_rate)
    };

    let ver = env!("CARGO_PKG_VERSION");
    println!(r"
╔════════════════════════════════════════╗
║  ▲ antarchy.fun v{ver} — Rust engine  ║
║  http://localhost:{port:<5}               ║
║  world: {world_w}×{world_h}  ║
║  spawn: ({spawn_x},{spawn_y})   ║
║  tick rate: {tick} Hz                  ║
╚════════════════════════════════════════╝");

    // Command queue: WS tasks push, sim thread drains (no World lock in WS path)
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Cmd>();

    // Sim loop on a dedicated OS thread (blocking, bypasses tokio scheduler)
    let world_sim = world.clone();
    std::thread::spawn(move || sim_loop(world_sim, cmd_rx));

    // Viewport delivery on its own OS thread — keeps heavy tile/fog serialization off the
    // sim thread so the tick cadence stays steady (smooth client interpolation). Its parallel
    // serialization runs on a DEDICATED rayon pool (Phase 2 of the egress rebuild) so
    // per-connection viewport work can never queue ahead of the 50 Hz tick's move-plan on the
    // shared global pool — the CPU wall that bunches ticks once hundreds of viewers cluster on a
    // hot metro. Thread count defaults to ~half the cores (override with HIVE_VIEWPORT_THREADS).
    let cores = std::thread::available_parallelism().map(|x| x.get()).unwrap_or(4);
    let vp_threads = std::env::var("HIVE_VIEWPORT_THREADS").ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or((cores / 2).max(2))
        .max(1);
    let vp_pool = Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(vp_threads)
            .thread_name(|i| format!("viewport-{i}"))
            .build()
            .expect("build dedicated viewport rayon pool"),
    );
    println!("[viewport] dedicated rayon pool: {vp_threads} threads (of {cores} cores)");
    let world_vp = world.clone();
    std::thread::spawn(move || viewport_loop(world_vp, vp_pool));

    // Phase-6 R2 snapshot writer — only when a sink is configured (SNAPSHOT_CDN + creds, or
    // HIVE_SNAP_DIR). Dormant by default, so the live server pays nothing until R2 is wired up.
    if snapshot::sink_active() {
        let world_snap = world.clone();
        std::thread::spawn(move || server::snapshot_writer_loop(world_snap));
    } else {
        println!("[snapshot] disabled (set SNAPSHOT_CDN + R2 creds, or HIVE_SNAP_DIR, to enable)");
    }

    // Save-on-shutdown: Ctrl-C / SIGTERM (Railway sends SIGTERM on redeploy) flushes the latest
    // state to disk before exit, so a redeploy loses at most the gap since the last autosave.
    let world_shutdown = world.clone();
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        let path = cfg().save_file.clone();
        println!("[persist] shutdown signal — saving snapshot to {path}…");
        {
            let w = world_shutdown.write().await;
            match persist::save(&w, &path) {
                Ok(())  => println!("[persist] snapshot saved"),
                Err(e)  => eprintln!("[persist] shutdown save failed: {e}"),
            }
        }
        std::process::exit(0);
    });

    // HTTP + WebSocket server on tokio runtime
    run(world, cmd_tx).await;
}

/// Resolve when the process receives a shutdown signal: Ctrl-C on any platform, plus SIGTERM on
/// unix (what container platforms like Railway send before SIGKILL on redeploy/scale-down).
async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv()             => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
