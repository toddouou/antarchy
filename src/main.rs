mod alliance;
mod api;
mod auth;
mod bots;
mod config;
mod cosmetics;
mod email;
mod events;
mod fog;
mod handlers;
mod metrics;
mod monuments;
mod network;
mod persist;
mod regions;
mod server;
mod session;
mod simulation;
mod sms;
mod snapshot;
mod stripe;
mod tile_map;
mod world;

use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

// RAM lever (Linux prod): use jemalloc instead of glibc malloc so freed pages are returned to the OS
// promptly (decay-based purging) rather than ratcheting RSS upward over a long-lived season. Unix-only
// — the Windows/MSVC dev build keeps the system allocator (jemalloc doesn't build cleanly there). To
// purge even more aggressively, set `MALLOC_CONF=dirty_decay_ms:1000,muzzy_decay_ms:0` in the env file.
#[cfg(unix)]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

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

    // Restore admin-tuned config (config.json beside the snapshot) BEFORE the world, so any level
    // recompute on restore uses the persisted XP curve. Missing/corrupt → compiled defaults.
    config::load_config();

    // Restore persisted state (accounts + world) so a restart / Railway redeploy resumes where it
    // left off. Both files live under HIVE_DATA_DIR (see config). Missing/corrupt → fresh start.
    let mut w = World::new();
    let save_file = cfg().save_file.clone();
    w.auth = auth::Auth::load(&save_file);
    let restored = match persist::load(&save_file) {
        Some(snap) => { persist::restore(&mut w, snap); true }
        None       => { println!("[persist] no snapshot at {save_file} — fresh start"); false }
    };
    // R2 tile-generation epoch: re-use the PERSISTED generation across restarts so a redeploy keeps
    // serving `snap/{epoch}/…` instead of minting a fresh generation each boot (which orphaned the
    // previous one on R2 forever — an unbounded storage leak; see `persist::save_epoch`). Only when
    // we actually restored a world: a fresh/failed load keeps its own new epoch and must NOT adopt a
    // stale persisted one (and must not let the boot-GC touch R2 — gated on `restored` below).
    if restored {
        match persist::load_epoch(&save_file) {
            Some(e) => { w.epoch = e; println!("[persist] reusing snapshot epoch {e}"); }
            None    => { persist::save_epoch(&w, &save_file);
                         println!("[persist] no epoch sidecar — pinned epoch {}", w.epoch); }
        }
    } else {
        // Fresh start: pin THIS generation so the first restart re-uses it instead of orphaning it.
        // (We do NOT adopt a pre-existing sidecar here — a failed/empty load must not inherit a stale
        // epoch, and must not let the boot-GC run, which is gated on `restored`.)
        persist::save_epoch(&w, &save_file);
    }
    // ∥A: when the write-ahead log is enabled, replay any journal recorded since the base snapshot.
    if config::wal_enabled() {
        let n = persist::replay_wal(&mut w, &persist::wal_path(&save_file));
        if n > 0 { println!("[persist] WAL replay: {n} chunk deltas applied"); }
    }
    // Admin-placed monuments live in their own runtime file (HIVE_DATA_DIR/monuments.json), NOT the
    // bincode snapshot — so they survive restarts AND world wipes. Load after restore so nothing
    // clobbers them; pin the id allocator past the highest existing id so a restart never reuses one.
    w.monuments = monuments::load();
    w.next_monument_id = w.monuments.iter().map(|m| m.id).max().unwrap_or(0) + 1;

    // Seed the 100 famous landmarks (data/landmarks.json), each projected to a MAP TILE via the game's
    // own Mercator so its marker lands on the right real-world spot. Idempotent by name (a restart or an
    // admin save that persisted seeds into monuments.json won't duplicate them); ids come from the
    // monument allocator so they never collide with admin-placed monuments.
    {
        let mut seeded = 0usize;
        for mut lm in monuments::seeded_landmarks() {
            if w.monuments.iter().any(|m| m.name == lm.name) { continue; }
            lm.id = w.next_monument_id;
            w.next_monument_id += 1;
            w.monuments.push(lm);
            seeded += 1;
        }
        println!("[landmark] {seeded} landmarks seeded ({} total monuments)", w.monuments.len());
    }

    // Restore the persistent per-account event feed (events.json) — survives restarts like accounts.
    w.events = events::EventStore::load();

    // Build the runtime player→alliance index from the persisted roster (users.json) now that both
    // accounts and the world are loaded. Drops any membership pointing at an account that no longer
    // exists (e.g. a wipe-orphaned id), so the hot-path index can't reference a ghost.
    w.rebuild_player_alliance();

    // Pin the player-id allocator past EVERY id that already exists, mirroring the monument pin above.
    // Account ids live in users.json; `next_player_id` lives in the world snapshot — two separate
    // files. If the snapshot resets while users.json survives (the bincode/zstd format break wipes the
    // world but keeps accounts; likewise a crash between an account save and the next ~60 s autosave),
    // `next_player_id` can fall BEHIND existing account ids. Then the next allocation — an organic bot
    // OR a new signup — reuses a live account's id, and `players/queens.insert(id, …)` overwrites that
    // human (their queen disappears) while their session resolves onto the collided record (they "log
    // into a bot"). Reconcile here so no freshly minted id can ever collide with a known one.
    {
        let max_account = w.auth.users.values().map(|u| u.id).max().unwrap_or(0);
        let max_player  = w.players.keys().copied().max().unwrap_or(0);
        let floor = max_account.max(max_player).saturating_add(1);
        if w.next_player_id < floor {
            println!("[persist] next_player_id {} behind existing ids — pinned to {floor} (prevents bot/account id collision)", w.next_player_id);
            w.next_player_id = floor;
        }
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
        std::thread::spawn(move || server::snapshot_writer_loop(world_snap, restored));
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
            w.events.save();   // flush the per-account event feed alongside the world snapshot
        }
        // Flush admin-tuned config too, so a graceful restart (systemctl/SIGTERM) keeps the latest
        // slider values even if they changed within the last autosave window.
        config::save_config();
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
