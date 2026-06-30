# Antarchy

The live engine for **Antarchy** — a real-time, planet-scale multiplayer Langton's-ant territory war.
Players place a queen on a world map and deploy Langton's-ant workers that paint territory under the
classic turn-and-flip rule, fighting for tiles across a world the size of Earth. Rust server + an
embedded single-file canvas client. This is the engine that replaced the original Node.js prototype.

## Quick start

```bash
cargo run            # debug
cargo run --release  # optimized (recommended for real play)
# open http://localhost:8080  — admin account: ADMIN / admin
```

Each browser tab is a separate player. Set `PORT=8090` to run a second instance alongside one
already on 8080. The world is never persisted — every run starts empty and accounts reset.

```bash
curl localhost:8080/health      # tick, counts, tile-store stats, tick p50/p99
curl localhost:8080/world-info  # world size, spawn, geo projection
```

> The browser client (`public/client.html`) is compiled **into** the binary via `include_str!`.
> After editing it you must **recompile** (`cargo run`) — refreshing the browser is not enough.

## What's a queen? an ant?

- A **queen** is a player's home base / objective: an HP-bearing footprint that grows with level and
  forfeits all of its territory when it dies.
- An **ant** is a Langton's ant — a moving cursor that paints territory by the turn-and-flip rule
  (unclaimed → paint + turn left; own tile → erase + turn right; enemy tile → overwrite, go
  straight). A **brute** ant is a 2×2, slow, heavy-damage siege unit.

## How it works

Three threads share one `World` behind `Arc<RwLock<World>>`:

1. **tokio** serves HTTP/WebSocket; connection tasks never lock the world — they push `Cmd`s onto a
   channel and drain an outbox back to the socket.
2. The **sim thread** (`sim_loop`) ticks at 50 Hz: drain commands → `tick_world` → record timing.
   Pure tick, no serialization, so the cadence stays steady.
3. The **viewport thread** (`viewport_loop`) snapshots each client's view under a short read lock,
   then serializes fog + base64 tiles + ants/queens **lock-free in parallel** (rayon) and pushes frames.

Storage is a sparse `TileMap` of 256×256 chunks; solid chunks collapse to a few bytes and cells are
`u16` palette indices, so RAM tracks painted *perimeter*, not area. The world is **1,500,000 ×
750,000 tiles** at ≈26.72 m/tile (≈ Earth's circumference), overlaid on OpenStreetMap via Mercator.

## Source map

| File | Role |
|---|---|
| `src/simulation.rs` | `tick_world` (11 phases): the Langton sim, clashes, damage, queen collisions |
| `src/tile_map.rs` | the sparse `u16` palette-indexed chunk store (Uniform/Dense) |
| `src/world.rs` | `World` aggregate + `Ant`/`Queen`/`Player` structs + helpers |
| `src/network.rs` | `snapshot_view` / `finish_view` (viewport frames), leaderboard, `me` |
| `src/server.rs` | axum routes, WS connection + rate limit, `sim_loop`, `viewport_loop` |
| `src/handlers.rs` | all WebSocket message dispatch (place-queen/ant, shop, admin) |
| `src/regions.rs` | metro/country tagging (point-in-polygon over `data/`) |
| `src/fog.rs` · `src/config.rs` · `src/auth.rs` | fog transform · tunables · accounts |

## Benchmarks

```bash
CARGO_TARGET_DIR=target-dev cargo test --release bench_ -- --ignored --nocapture
```

Targets 1,000 queens / 100,000 ants; the tick holds ~45–50% of the 20 ms @ 50 Hz budget.

## Deeper docs

- **[`CLAUDE.md`](./CLAUDE.md)** — the working reference: full architecture, the WebSocket protocol,
  key invariants, and **Performance & scaling** (target counts, benchmarks, known limits).
- **[`docs/ARCHITECTURE.md`](./docs/ARCHITECTURE.md)** — a plain-language explainer of the stack and
  the genuinely clever parts (how a pixel placement flows end-to-end, the tile store, the LOD pyramid).
- The longer-term product vision (visual overhaul, persistence, planet-scale sharding) lives in the
  parent project's `ROADMAP.md`.
