# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this
repository. **HIVE-SIM is a Rust rewrite of the original Node.js prototype** (which lived in the
parent directory and has been removed). This `sim/` directory is its own git repository and is the
live game engine.

## Running the server

```bash
cargo run              # debug build
cargo run --release    # optimized build (use for real load)
```

Open `http://localhost:8080` in one or more browser tabs. Each tab is a separate player. There is
no separate client build step — `public/client.html` is compiled **into** the binary.

> **IMPORTANT:** `public/client.html` is embedded at compile time via `include_str!`
> (`src/server.rs`). Editing the client requires a **recompile** (`cargo run`/`cargo build`) to
> take effect — a browser refresh alone will not pick up client changes.

**Health check:** `curl http://localhost:8080/health` (tick, ant/queen/player counts, uptime).
**World info:** `curl http://localhost:8080/world-info` (world size, spawn, geo projection).

**Fresh start:** the world is never loaded from disk — every run starts empty. Accounts also reset
each restart (`Auth::load` exists but is unused); the admin account (`ADMIN` / `admin`) is
recreated at startup. `users.json` is written by `Auth::save` but is not read back.

## File layout

```
Cargo.toml             — crate manifest + dependencies (tokio, axum, rayon, serde, sha2,
                         base64, rustc-hash, futures-util)
src/
  main.rs              — entry: builds World, spawns sim_loop on a dedicated OS thread,
                         runs the axum HTTP/WS server on the tokio runtime
  config.rs            — Config struct + global cfg()/cfg_write() (OnceLock<RwLock>), ADMIN_CLAMP,
                         apply_admin_param, reset_to_defaults, queen_size_for_level,
                         total_xp_for_level, level_for_xp, calc_score, current_ms
  world.rs             — World aggregate + Ant/Queen/Player/PlayerView/XpGrant/QueenHit structs;
                         send_to/broadcast/broadcast_near; get_queen_map; cell_key
  tile_map.rs          — TileMap: sparse 256×256 chunked tile store (FxHashMap of chunks),
                         per-player tile counts kept in sync; get/set/clear
  simulation.rs        — tick_world + turn_ccw/turn_cw, award_xp/flush_xp, kill_queen,
                         wipe_world, spawn_npc
  fog.rs               — compute_fog_field: two-pass Chebyshev distance transform (admins get none)
  network.rs           — build_view_update (base64 tiles + fog + ants + queens + palette),
                         build_player_info, build_leaderboard, get_palette (MAX_DIM = 800)
  handlers.rs          — handle_message: all WebSocket message dispatch
  auth.rs              — Auth (users + banned), hash_pw (SHA-256 + "hive-salt"),
                         save/load users.json, admin account
  server.rs            — axum routes (root_handler / health / world-info), WS connection +
                         rate limit, Cmd queue, sim_loop, send_viewports
public/
  client.html          — single-file canvas client; embedded into the binary via include_str!
```

**Dependency chain:** `config` → {`auth`, `tile_map`} → `world` → {`fog`, `network`, `simulation`,
`handlers`} → `server` → `main`.

## Architecture

**One process, two execution contexts sharing a single `World` behind `Arc<RwLock<World>>`:**

1. **Tokio async runtime** (`server::run`) — the axum HTTP/WebSocket server. WebSocket tasks
   **never lock the World**: each connection parses only the message *type* and pushes a `Cmd`
   (`Auth` / `Message` / `Disconnect`) onto an unbounded mpsc channel. A per-connection write task
   drains an outbox channel to the socket. The root route `/` serves the client on a normal GET and
   upgrades to WebSocket when requested.

2. **Dedicated blocking OS thread** (`server::sim_loop`) — a fixed-interval scheduler running at
   `cfg.tick_rate` Hz (default 50). Each iteration:
   1. Take the exclusive write lock.
   2. Drain **all** pending `Cmd`s and dispatch them via `handle_message` (this is the only place
      messages mutate the World).
   3. `tick_world(&mut w)` unless paused.
   4. Daily ant refill check.
   5. Release the write lock, take a read lock, and run `send_viewports`.

**Tick loop** (`tick_world`): computes Langton ant moves and paints tiles (all via `tiles.set`),
resolves clashes (5×5 majority converts the minority), applies queen damage/heals, and flushes the
XP queue.

**Viewport delivery** (`send_viewports`): runs every 2nd tick (≈25 Hz). For each connected,
non-NPC player it builds — **in parallel via rayon** — a viewport-clipped tile snapshot (base64
little-endian `u16`), a server-side fog distance field, the visible ants/queens, and the palette.
The leaderboard is broadcast every 20 ticks.

**No database.** Tiles live in `TileMap` (sparse 256×256 chunks); players, queens, and ant counts
live in `FxHashMap`s keyed by numeric player id.

**WebSocket message types** (client → server): `register`, `login`, `set-color`, `view-set`,
`get-forbidden-zones`, `place-queen`, `place-ant`, `admin` (slider param), `admin-action`
(`add-ants` / `heal-queen` / `level-up` / `level-down`), `admin-target` (`reset-hp` /
`delete-queen` / `move-queen` / `ban-player`), `admin-pause`, `admin-kick`, `admin-set-level`,
`admin-give-xp`, `admin-set-ants`, `admin-broadcast`, `admin-spawn-n`, `admin-player-list`,
`admin-cfg-reset`.

**Config** (`config.rs`): all tunable simulation parameters live in the global `Config`, accessed
via `cfg()` / `cfg_write()`. Admin-panel sliders mutate it through `apply_admin_param`, clamped by
`ADMIN_CLAMP`. The world is `world_w` × `world_h` = **1,500,000 × 750,000** tiles at
`tile_meters` ≈ 26.72 (≈ Earth's circumference); spawn is the grid center.

## Key invariants

- **Tile encoding**: `tiles.get(x, y)` returns `0` (unclaimed) or the owner's numeric player id.
- **Tile counts**: all tile mutations must go through `tiles.set(x, y, new_owner)` — it keeps
  `TileMap.counts` (player id → exact count) in sync and drops empty chunks. Never mutate chunk
  cells directly.
- **Queen map**: cached in `World.queen_map`. Set `world.queen_map_dirty = true` whenever a queen is
  placed, killed, moved, or resized; `get_queen_map()` rebuilds only when dirty.
- **XP is queued**: `award_xp(...)` pushes to `world.xp_queue`; `flush_xp(world)` applies it within
  the tick. Call `flush_xp` explicitly after any out-of-tick award.
- **Queen size grows with level** (`queen_size_for_level`). Queens re-derive size from level.
- **Fog**: admins receive an all-zero fog field (no fog) from `compute_fog_field`.
- **NPC queens**: have `p.npc = true`; excluded from daily refills and normal leaderboard behavior.
- **Rate limiting**: 120 messages/second per WebSocket connection, enforced in the WS task (on the
  connection, before any World access) so unauthenticated connections are covered.
- **Client embedding**: `public/client.html` is `include_str!`'d at compile time — **recompile**
  after editing it.

## Client ↔ geography

The client maps grid ↔ lat/lon via a Mercator projection centered at
`(capitol_lat, capitol_lon)` (default `(0, 0)`). The base map is **OpenStreetMap raster tiles
drawn client-side directly from the live camera** (`view.x/view.y/view.zoom`) in `drawOsmTiles` —
it is independent of the server viewport, so it tracks panning/zooming in real time. The
server-sent viewport (`vw`) drives only the territory/ant/queen overlay.
