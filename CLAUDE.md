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

Set the `PORT` env var to run a second instance without disturbing one already on 8080 (e.g.
`PORT=8090 cargo run`) — handy for testing against a live playtest server.

> **IMPORTANT:** `public/client.html` is embedded at compile time via `include_str!`
> (`src/server.rs`). Editing the client requires a **recompile** (`cargo run`/`cargo build`) to
> take effect — a browser refresh alone will not pick up client changes.

**Health check:** `curl http://localhost:8080/health` — tick; ant/queen/player/connected counts;
uptime; tile-store stats (`tilesPainted`/`chunks`/`uniformChunks`/`denseChunks`/`tileMB`); tick
timing (`tickMsP50`/`tickMsP99`/`tickMsMax`); `seasonSecs`.
**World info:** `curl http://localhost:8080/world-info` (world size, spawn, geo projection).

**Fresh start:** the world is never loaded from disk — every run starts empty. Accounts also reset
each restart (`Auth::load` exists but is unused); the admin account (`ADMIN` / `admin`) is
recreated at startup. `users.json` is written by `Auth::save` but is not read back.

## File layout

```
Cargo.toml             — crate manifest + dependencies (tokio, axum, rayon, serde, sha2, base64,
                         hex, rustc-hash, smallvec, once_cell, parking_lot, rand, futures-util,
                         tokio-tungstenite)
src/
  main.rs              — entry: parses regions, builds World, spawns sim_loop AND viewport_loop on
                         dedicated OS threads, runs the axum HTTP/WS server on the tokio runtime
  config.rs            — Config struct + global cfg()/cfg_write() (OnceLock<RwLock>), ADMIN_CLAMP,
                         apply_admin_param, reset_to_defaults, queen_size_for_level,
                         total_xp_for_level, level_for_xp, calc_score, current_ms; shop prices +
                         tunables (CREDIT_CAP, PRICE_*, BRUTE_DMG_MULT, DEFENDER_RANGE, …)
  world.rs             — World aggregate + Ant/Queen/Player/PlayerView/XpGrant/QueenHit/
                         MetroHolder/AwaySnapshot structs; send_to/broadcast/broadcast_near;
                         get_queen_map; cell_key; paint_queen_body/clear_queen_body/
                         too_close_to_queen; Queen::set_level; tick-ms ring
  tile_map.rs          — TileMap: sparse 256×256 chunks, each Uniform(idx) or Dense{ u16 cells };
                         cells are u16 palette indices (palette/id_to_idx, recycled on zero) so RAM
                         scales with painted perimeter; per-player counts kept exact;
                         get/set/clear/clear_owner; stats/compact_pass/tally_owners_in_circle
  regions.rs           — metro + country tagging: point-in-polygon over data/countries.geojson,
                         metros from data/regions.json, projected with the client's Mercator math;
                         region_for / country_and_continent (Discovery + leaderboard regions)
  simulation.rs        — tick_world (11 phases) + turn_ccw/turn_cw, award_xp/flush_xp, kill_queen,
                         wipe_world, spawn_npc, resolve_queen_collisions (spatially bucketed),
                         recompute_holders, sample_visited
  fog.rs               — compute_fog_field_slice: two-pass Chebyshev distance transform over a
                         padded ownership slice (runs lock-free, off the sim thread)
  network.rs           — snapshot_view (under lock) + finish_view (lock-free: base64 tiles + fog +
                         ants + queens + palette), build_player_info, build_leaderboard,
                         build_server_stats, build_region_holders (MAX_DIM = 800; LOD pyramid)
  handlers.rs          — handle_message: all WebSocket message dispatch (incl. shop-buy);
                         validate_worker_placement; create_or_reconnect_player; welcome-back
  auth.rs              — Auth (users + banned), hash_pw (SHA-256 + "hive-salt"),
                         save/load users.json, admin account
  server.rs            — axum routes (root_handler / health / world-info), WS connection +
                         rate limit, Cmd queue, sim_loop, viewport_loop (separate OS thread)
public/
  client.html          — single-file canvas client; embedded into the binary via include_str!
data/
  regions.json         — editable metro list (embedded via include_str! → rebuild to apply)
  countries.geojson    — Natural Earth admin-0 countries (embedded; point-in-polygon source)
```

**Dependency chain:** `config` → {`auth`, `tile_map`, `regions`} → `world` → {`fog`, `network`,
`simulation`, `handlers`} → `server` → `main`.

## Architecture

**One process, three execution contexts sharing a single `World` behind `Arc<RwLock<World>>`:**

1. **Tokio async runtime** (`server::run`) — the axum HTTP/WebSocket server. WebSocket tasks
   **never lock the World**: each connection parses only the message *type* and pushes a `Cmd`
   (`Auth` / `Message` / `Disconnect`) onto an unbounded mpsc channel. Each connection has two
   outbound channels — a priority mpsc (events/confirmations) and a latest-wins `watch` slot
   (viewport frames) — merged by a `biased` `select!`. The root route `/` serves the client on a
   normal GET and upgrades to WebSocket when requested.

2. **Sim thread** (`server::sim_loop`, dedicated blocking OS thread) — a fixed-interval scheduler at
   `cfg.tick_rate` Hz (default 50). Each iteration: take the write lock; drain **all** pending
   `Cmd`s via `handle_message` (the only place messages mutate the World); `tick_world(&mut w)`
   unless paused; daily-refill + season-rollover; record the tick duration. **Pure tick — no
   serialization here**, so the cadence stays steady.

3. **Viewport thread** (`server::viewport_loop`, its own OS thread) — two phases per cycle:
   **(A)** under a *short* read lock, snapshot each client's viewport data + clone its channel
   senders (`snapshot_view`); **(B)** with *no lock held*, run the fog transform + base64 + JSON
   **in parallel via rayon** (`finish_view`) and push frames. Moving the heavy serialization off the
   sim thread is what keeps the tick cadence rock-steady → smooth client interpolation. Cadence is
   paced on the sim's tick counter: ant frames whenever the tick advances, tile frames ≤ ~10 Hz,
   `me` ~1 Hz, leaderboard + server-stats ~1 Hz / every 20 ticks.

**Tick loop** (`tick_world`, 11 phases): plans Langton ant moves **in parallel** (rayon), resolves
ant-ant collisions and 5×5-majority clashes, paints tiles (all via `tiles.set`), applies batched
queen damage/heals, resolves queen-queen collisions (**spatially bucketed**, `resolve_queen_collisions`),
ages out expired ants, and flushes the XP queue.

**No database.** Tiles live in `TileMap` (sparse 256×256 chunks); players, queens, and ant counts
live in `FxHashMap`s keyed by numeric player id.

**WebSocket message types** (client → server): `register`, `login`, `set-color`, `view-set`,
`get-forbidden-zones`, `place-queen`, `place-ant`, `shop-buy` (`highway` / `relocate` / `defender` /
`brute` / `shield`; `alliance` is a no-charge "coming soon" stub), `admin` (slider param),
`admin-action` (`add-ants` / `heal-queen` / `level-up` / `level-down` / `spawn-npc` / `wipe-world`),
`admin-target` (`reset-hp` / `delete-queen` / `move-queen` / `ban-player`), `admin-pause`,
`admin-kick`, `admin-set-level`, `admin-give-xp`, `admin-set-ants`, `admin-give-credits`,
`admin-set-credits`, `admin-broadcast`, `admin-spawn-at`, `admin-place-ant` (place an ant owned by
`targetId` at `x,y` — admin override, no bubble/territory/ant-count checks), `admin-player-list`
(includes NPC players), `admin-cfg-reset`.

**Config** (`config.rs`): all tunable simulation parameters live in the global `Config`, accessed
via `cfg()` / `cfg_write()`. Admin-panel sliders mutate it through `apply_admin_param`, clamped by
`ADMIN_CLAMP`. The world is `world_w` × `world_h` = **1,500,000 × 750,000** tiles at
`tile_meters` ≈ 26.72 (≈ Earth's circumference); spawn is the grid center.

## Key invariants

- **Tile encoding**: `tiles.get(x, y)` returns `0` (unclaimed) or the owner's numeric player id.
  Internally cells are compact `u16` palette indices (`palette[idx] → id`, recycled when a count
  hits 0); the public API still speaks raw ids, so callers are unchanged. **On the wire**,
  `finish_view` re-indexes each tile frame into a **per-frame local palette** (`tileIds[local] = id`,
  `0` = unclaimed), so the `u16` tile blob is bounded by the owners *visible in that viewport*, never
  the lifetime id count — the client maps `tileIds` back to real ids on receipt. (This replaced the
  old `min(0xFFFF)` clamp, which collided every id > 65,535.)
- **Tile counts**: all tile mutations must go through `tiles.set(x, y, new_owner)` — it keeps
  `TileMap.counts` (player id → exact count) in sync and drops empty chunks. Never mutate chunk
  cells directly.
- **Queen map**: cached in `World.queen_map`. Set `world.queen_map_dirty = true` whenever a queen is
  placed, killed, moved, or resized; `get_queen_map()` rebuilds only when dirty.
- **XP is queued**: `award_xp(...)` pushes to `world.xp_queue`; `flush_xp(world)` applies it within
  the tick. Call `flush_xp` explicitly after any out-of-tick award.
- **Queen size grows with level** (`queen_size_for_level`). Always change level via
  `Queen::set_level` (sets level → size → max_hp together) so the queen-cell map can't desync.
- **Queen bodies**: paint/clear footprints via `World::paint_queen_body` / `clear_queen_body`
  (both clamp to the world). Worker-placement legality lives in `validate_worker_placement`.
- **Fog**: admins receive an all-zero fog field (no fog) from `compute_fog_field_slice`.
- **NPC queens**: have `p.npc = true`; excluded from daily refills and normal leaderboard behavior.
- **Rate limiting**: 120 messages/second per WebSocket connection, enforced in the WS task (on the
  connection, before any World access) so unauthenticated connections are covered.
- **Client embedding**: `public/client.html` is `include_str!`'d at compile time — **recompile**
  after editing it.

## Performance & scaling

Target scale: **~1,000 queens, 100,000 active ants, activity in all four map corners**, a 1-month
season then a full wipe (~100 billion painted cells at peak — oceans are never painted).

**What holds the budget:**
- **Tick** (`tick_world`) runs at ~45–50% of the 20 ms @ 50 Hz budget at 100k ants. The move-plan
  is rayon-parallel; the periodic ant sorts use `par_sort_unstable`; queen-queen collision is
  spatially bucketed (≈O(queens), not O(queens²)). Adding 1,000 queens adds no measurable tick cost.
- **Tile RAM** scales with painted *perimeter*, not area: solid 256×256 chunks collapse to
  `Uniform` (a few bytes); only frontiers pay a `Dense` `u16` array. `compact_pass` (~every 30 s)
  re-collapses chunks that became solid via clash conversions.
- **Viewport delivery** is decoupled onto its own thread; serialization is lock-free + parallel,
  and zoomed-out views use an on-demand LOD pyramid bounded to `MAX_DIM` (800) cells/axis.

**Benchmarks** (`#[ignore]`, release only — run against `target-dev`, not the live server):
```bash
PORT=8090 CARGO_TARGET_DIR=target-dev cargo test --release bench_ -- --ignored --nocapture
```
- `bench_tick_100k_ants` — tick @ 100k ants, 0 queens (~9–12 ms).
- `bench_tick_1k_queens_100k_ants` — tick @ the full target (1k queens + 100k ants; ~9–12 ms).
- `bench_queen_collisions_1k` — isolated Phase-9 cost; spatial bucketing took it ~2,400 µs → ~90 µs.

**Known limits (identified, not yet addressed — need client/protocol work + browser validation):**
- **Palette fan-out**: `finish_view` clones the full palette into every client's tile frame →
  O(players²) work at high connected counts. Fix: send the palette only on change, cache it client-side.
- **Leaderboard payload**: the full live-queen list is broadcast every 20 ticks; should be top-N.

## Client ↔ geography

The client maps grid ↔ lat/lon via a Mercator projection centered at
`(capitol_lat, capitol_lon)` (default `(0, 0)`). The base map is **OpenStreetMap raster tiles
drawn client-side directly from the live camera** (`view.x/view.y/view.zoom`) in `drawOsmTiles` —
it is independent of the server viewport, so it tracks panning/zooming in real time. The
server-sent viewport (`vw`) drives only the territory/ant/queen overlay.
