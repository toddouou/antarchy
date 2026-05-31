mod auth;
mod config;
mod fog;
mod handlers;
mod network;
mod regions;
mod server;
mod simulation;
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

    let world: WorldState = Arc::new(RwLock::new(World::new()));

    let c = cfg();
    let port    = c.port;
    let world_w = c.world_w;
    let world_h = c.world_h;
    let spawn_x = c.spawn_x;
    let spawn_y = c.spawn_y;
    let tick    = c.tick_rate;
    drop(c);

    println!(r"
╔════════════════════════════════════════╗
║  ▲ HIVE-SIM v0.2 — Rust engine        ║
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
    // sim thread so the tick cadence stays steady (smooth client interpolation).
    let world_vp = world.clone();
    std::thread::spawn(move || viewport_loop(world_vp));

    // HTTP + WebSocket server on tokio runtime
    run(world, cmd_tx).await;
}
