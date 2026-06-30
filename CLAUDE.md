# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this
repository. **The Antarchy engine is a Rust rewrite of the original Node.js prototype** (which lived in the
parent directory and has been removed). This `sim/` directory is its own git repository and is the
live game engine.

## Running the server

```bash
cargo run              # debug build
cargo run --release    # optimized build (use for real load)
```

Open `http://localhost:8080` in one or more browser tabs: `/` serves the landing + login/spectator
page (`public/landing.html`); the game client (`public/client.html`) lives at `/play`. Each tab is a
separate player. There is no separate client build step — both pages are compiled **into** the binary.

Set the `PORT` env var to run a second instance without disturbing one already on 8080 (e.g.
`PORT=8090 cargo run`) — handy for testing against a live playtest server.

> **IMPORTANT:** `public/client.html` is embedded at compile time via `include_str!`
> (`src/server.rs`). Editing the client requires a **recompile** (`cargo run`/`cargo build`) to
> take effect — a browser refresh alone will not pick up client changes.

**Health check:** `curl http://localhost:8080/health` — tick; ant/queen/player/connected counts;
uptime; tile-store stats (`tilesPainted`/`chunks`/`uniformChunks`/`denseChunks`/`tileMB`); tick
timing (`tickMsP50`/`tickMsP99`/`tickMsMax`); `seasonSecs`.
**World info:** `curl http://localhost:8080/world-info` (world size, spawn, geo projection).

**Persistence:** state is **restored on startup** (`src/persist.rs`). These files live under the
directory from the `HIVE_DATA_DIR` env var (default = working dir; in production point it at the
host's persistent data dir via the service env file, or it won't survive a redeploy):
- `world.snapshot` — zstd-compressed bincode (bincode 2) of tiles, ants, queens, players (durable fields only),
  `next_player_id`, `tick`, `started_at`. Written atomically (temp file + fsync + rename).
- `world.snapshot.epoch` — the R2 tile-generation epoch, persisted beside the snapshot so a restart
  can't regress the super-tile keyspace.
- `users.json` — accounts + ban list (`Auth::save`/`Auth::load`, `{users, banned}`). Same atomic
  temp + fsync + rename treatment.
- `config.json` — admin-tuned slider values (`config::save_config`/`load_config`), rewritten when a
  tunable changes and re-applied through the clamping setters on boot, so admin tuning survives a
  restart.

The world autosaves every ~60 s (in `sim_loop`) and on shutdown (Ctrl-C / SIGTERM handler in
`main.rs` — covers service restarts/redeploys). On boot, `main` calls `Auth::load` + `persist::load`/`restore`;
a missing/corrupt/wrong-version snapshot → fresh empty world + admin-only auth. The admin account
(`ADMIN` / `admin`) is always recreated if absent. **The only thing that clears the world is the
admin panel's type-"WIPE" button** — the automatic season wipe is disabled by default
(`season_secs = 0`). That admin wipe also deletes all non-admin accounts and force-logs-out connected
players (`wipe_world_and_users`); the season-rollover path still uses the milder `wipe_world`.

## File layout

```
Cargo.toml             — crate manifest + dependencies (tokio, axum, serde, serde_json, rayon,
                         rustc-hash, base64, sha2, hex, argon2 + subtle (password hashing),
                         once_cell, parking_lot, rand, futures-util, tokio-tungstenite, flate2,
                         bincode, zstd, png, rust-s3 (R2/S3 upload), reqwest (Resend email))
src/
  main.rs              — entry: parses regions, builds World, spawns sim_loop AND viewport_loop on
                         dedicated OS threads, runs the axum HTTP/WS server on the tokio runtime
  config.rs            — Config struct + global cfg()/cfg_write() (OnceLock<RwLock>), ADMIN_CLAMP,
                         apply_admin_param, reset_to_defaults, queen_size_for_level,
                         max_hp_for_level, total_xp_for_level, level_for_xp, calc_score,
                         current_ms; shop prices +
                         tunables (PRICE_*, BRUTE_DMG_MULT, DEFENDER_RANGE, …)
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
                         region_for / country_and_continent (Passport + leaderboard regions)
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
  auth.rs              — Auth (users + banned); Argon2id hashing (hash_pw_argon2/verify_pw/
                         needs_rehash) w/ transparent rehash-on-login + constant-time compare; legacy
                         SHA-256+"hive-salt" still verified; save/load users.json; admin account
  session.rs           — opaque HttpOnly cookie session tokens: issue/verify/revoke (replaced the old
                         localStorage bearer token)
  api.rs               — HTTP /api/* account endpoints (register, verify-email/phone, login, logout,
                         forgot/reset-password, roster); CSRF Origin guard + per-IP throttle; sets the
                         session cookie; calls email::/sms:: for codes
  email.rs             — transactional email via Resend (verify/reset codes); dormant until
                         HIVE_RESEND_API_KEY set — logs the code to the console as a dev fallback
  sms.rs               — SMS verify-code sender; dormant stub (logs the code) until a provider is wired
  persist.rs           — world snapshot save/load/restore: zstd bincode of tiles/ants/queens/players/
                         counters → world.snapshot (atomic temp+fsync+rename); pairs with users.json
  metrics.rs           — egress + R2 op counters (Class-A/B/delete) behind /egress-stats; denial-of-
                         wallet spike alerts
  snapshot.rs          — R2 super-tile pipeline: rasterize dirty chunks → PNG, coalesce into super
                         keys, upload to R2/S3 (zero-egress bulk map); gated on SNAPSHOT_CDN + R2 creds
  server.rs            — axum routes: `/` (landing + guest-spectator WS), `/play` (game client +
                         authed game WS), `/reset`, `/health`, `/world-info`, `/egress-stats`,
                         `/api/*`; WS connection + rate limit, Cmd queue, sim_loop (incl. ~60s autosave
                         + R2 snapshot writer), viewport_loop (OS thread)
public/
  client.html          — single-file canvas game client; embedded into the binary via include_str!
  landing.html         — public landing + login/spectator page; embedded via include_str!
data/
  regions.json         — editable metro list (embedded via include_str! → rebuild to apply)
  countries.geojson    — Natural Earth admin-0 countries (embedded; point-in-polygon source)
```

**Dependency chain:** `config` → {`auth`, `session`, `tile_map`, `regions`, `metrics`} → `world` →
{`fog`, `network`, `simulation`, `handlers`, `persist`, `snapshot`, `email`, `sms`, `api`} →
`server` → `main`.

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

**Account auth is HTTP, not WS:** the `/api/*` routes (register, verify-email/phone, login, logout,
forgot/reset-password) set an HttpOnly `__Host-` session cookie; the WS connection then authenticates
from that cookie via `enter` (authed player), `session`, or `spectate` (guest, read-only). Legacy
`register`/`login` WS messages remain for back-compat.

**WebSocket message types** (client → server): `enter` / `session` / `spectate` (cookie auth),
`register`, `login`, `set-color`, `view-set`, `get-forbidden-zones`, `place-queen`, `place-ant`,
`shop-buy` (`relocate` / `defender` / `brute` / `shield`), `admin` (slider param),
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

## Auth, sessions, R2 snapshots & hosting

- **Accounts/sessions** (`auth.rs` / `session.rs` / `api.rs`): Argon2id hashing with transparent
  rehash of legacy SHA-256 on login; HttpOnly `__Host-` cookie sessions (token never in JS); CSRF
  Origin guard + per-IP throttle on `/api/*`; enumeration-safe registration. **WS hardening:** inbound
  size cap (`HIVE_WS_MAX_MSG`), Origin allowlist on upgrade, idle close, bounded per-conn send queues,
  anti-replay `seq`. Full OWASP-mapped detail in `docs/security/SECURITY_P0_PLAN.md` +
  `CHANGELOG-security.md` (P0 done; P1/P2 pending).
- **Zero-egress bulk map** (`snapshot.rs` + `metrics.rs`): the snapshot writer rasterizes dirty chunks
  to PNG super-tiles and uploads them to Cloudflare R2 (gated on `SNAPSHOT_CDN` + `R2_*` creds), so the
  bulk of the map is served from R2 at $0 egress; `/egress-stats` exposes egress + R2 op counters with
  denial-of-wallet alerts. Plans: `docs/egress/EGRESS_PLAN.md`, `docs/egress/R2_CLASSA_PLAN.md`.
- **Hosting:** runs on a dedicated VPS (migrated off Railway). Config is `HIVE_*` / `R2_*` env vars
  from the service env file; `HIVE_DATA_DIR` must point at the live world directory.

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
  `max_hp` follows an **exponential** curve (`max_hp_for_level`) anchored at `hp_base` (L1) and
  `hp_max` (the level cap) — defaults 10 HP @ L1 → 100,000 HP @ L100. Level-ups grant the added HP
  headroom (the `max_hp` delta), not a flat amount.
- **Ant damage scales with level**: each bite deals `ant_damage × attacker-queen-level`; **brutes**
  carry a `BRUTE_DMG_MULT` (3×) on top. `ant_damage` (default 1.0) is the global admin scalar.
- **Brute movement** is a 2×2-footprint Langton variant: footprint all-friendly → 90° CW,
  all-white → 90° CCW, otherwise (mixed/enemy) → straight; the footprint is always repainted
  friendly. Brutes act on even ticks only.
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

**Former known limits — both addressed:**
- **Palette fan-out**: tile frames carry a per-frame **local** palette bounded by the owners visible
  in that viewport (`tileIds`/`tileIdsAdd`), and the global owner→colour map is built **once per
  tile cycle** behind an `Arc` shared by every client's `finish_view` job (skipped entirely on
  ants-only cycles) — no O(players²) work at high connected counts.
- **Leaderboard payload**: capped to the top 100 by Grand Score (`LEADERBOARD_TOP_N`) and broadcast
  only when a signature of the standings changes (plus a ~5 s forced heartbeat).

## Client ↔ geography

The client maps grid ↔ lat/lon via a Mercator projection centered at
`(capitol_lat, capitol_lon)` (default `(0, 0)`). The base map is **OpenStreetMap raster tiles
drawn client-side directly from the live camera** (`view.x/view.y/view.zoom`) in `drawOsmTiles` —
it is independent of the server viewport, so it tracks panning/zooming in real time. The
server-sent viewport (`vw`) drives only the territory/ant/queen overlay.

---

## Engineering Org Constitution

> The operating contract for the autonomous engineering org that lives in this repo. Stable + dense
> on purpose (it caches every call) — edit rarely. Specialist charters live in `.claude/agents/*.md`;
> durable domain knowledge in `.claude/memory/*.md`; the human-facing guide is `ORG.md`.

### Mission & quality bar
Make Antarchy the best version of itself — a planet-scale, real-time multiplayer game that stays
smooth and cheap at launch scale (~1,000 queens / 100,000 ants / many viewers). The bar is a
**billion-dollar product**: correctness, durability, security, and craft first. "It compiles" / "it
shipped" is not "done" (see Definition of done).

### Best, not easiest
Always take the best engineering decision available for our actual purpose — never the easiest,
simplest, or first-thing-that-compiles. When the easy path and the right path diverge, take the
right path and say why. Reuse what exists before adding code. Root-cause over symptom-patch (the
owner explicitly prefers structural fixes). Never trade correctness, gameplay, or data integrity for
cheapness silently — surface the tradeoff and let the human decide.

### Token economy (non-negotiable — quality AND frugality)
- **Model tiering.** Search / read / extract / summarize / format → **haiku**. Implementation &
  analysis → **sonnet**. Orchestrator reasoning, decomposition, hard architecture/cost calls →
  **opus** (effort scaled to difficulty). Never put an expensive model on a cheap task. Hard run
  ceiling: pin subagents via `CLAUDE_CODE_SUBAGENT_MODEL`.
- **Delegate the heavy reading.** Exploration goes to read-only subagents that return *distilled*
  briefs, not raw file dumps — keep their stack traces and dead ends out of the main thread. ~3–5 in
  parallel; wider only for genuinely independent searches.
- **Scope-bound everything.** Hand each agent only what it needs, never the whole transcript. Pass
  one step's output to the next, not the accumulated history.
- **Targeted reads.** Grep/Glob to the exact lines before reading whole files; don't re-read what's
  already summarized. `client.html` is ~6k lines — never read it whole; locate first.
- **Cache the stable stuff.** This file + charters + tool defs are reused every call — don't churn
  them mid-task.
- **Context hygiene.** `/clear` between unrelated tasks; `/compact` before long runs bloat. Two
  failures on one approach → stop, re-plan with what you learned, don't push a polluted thread.

### Org chart & escalation
The **Orchestrator** (opus) is the only agent the human talks to. Heads (charters in
`.claude/agents/`): **Strategy** (opus, rare) · **Backend/Engine** + auth/api/stripe impl (sonnet) ·
**Client/Rendering+UX** (sonnet) · **Infrastructure & Storage** incl. R2/persist/tiles (sonnet) ·
**Security** advisory gate, no-write (sonnet) · **QA** (sonnet) · **Reviewer/Verifier**
fresh-context, no-write (sonnet→opus for high-risk). Ephemeral search agents → haiku.
**Escalation:** a head decides within its charter → the Orchestrator decides cross-cutting /
architecture → the human decides direction, scope, and irreversible calls. The author never grades
their own work (Reviewer + QA are independent). Auth / payment / WS / input changes are **gated** by
Security before ship.

### Core loop (every request)
1. **Intake & reframe** raw direction into a precise problem + success criteria; surface hidden
   assumptions; correct the human with evidence when the framing is wrong; recommend the best
   approach vs. its runner-up; get the decision. 2. **Decompose & delegate** to the owning head on
   the cheapest reliable model; bound context; require distilled output. 3. **Verify independently**
   — Reviewer checks the diff in fresh context against the plan; QA produces *evidence* (command +
   real output + measured number), not assertions; loop until it meets the bar. 4. **Report up**
   tight: what changed, what it cost (tokens / metered ops when relevant), the decision needed next.

### Coding standards (as the repo actually is)
Rust 2021, single `hive-sim` binary; match surrounding style. Error handling is **deliberate, not
`anyhow`** — `Option` / early-return / `unwrap_or(default)` and silent-drop on send failure; don't
add an error framework without cause. Logging is `println!("[module] …")` / `eprintln!` for fatal —
no logging crate. `snake_case` fns, `PascalCase` types, `SCREAMING_SNAKE` consts; default rustfmt (no
config); high comment density that explains **why**. Keep `cargo build`, `cargo clippy`, and the
~180 inline tests green. **Tile mutations only via `tiles.set`; level only via `Queen::set_level`;**
respect the Key Invariants above — they are load-bearing.

### World-rendering cost doctrine (the standing hard problem)
**Measure before optimizing** — `/egress-stats` (R2 Class-A + dirty-chunk counters); `/health`
(`viewport_cycle_ms` / `finish_view_ms` / tick rings / `tileMB`). What actually costs money / limits
scale:
1. **R2 Class-A `PutObject`** (snapshot writer → super-tiles) is the **only real $ meter** — $4.50/M,
   ~$200/yr ceiling. NOT storage, NOT Class-B, NOT host egress.
2. **`finish_view` serialization CPU** at high connection counts is the live **capacity** wall on the
   flat-rate VPS (not a bill). The per-cycle `Arc` palette + leaderboard send-on-change already
   mitigate the old O(players²) fan-out — **re-measure before assuming it's still the wall**
   (`EGRESS_PLAN` P4/P5 doc lags the code).
3. **Host egress is flat-rate** (VPS, post-Railway) — bytes are a *capacity* concern, not an invoice.
   **Class-B reads are ~free** (browsers fetch R2 directly at $0 egress). Don't optimize these as cost.
Full state + levers: `.claude/memory/cost-doctrine.md`.

### Definition of done
Agreed success criteria met · independently verified with **evidence** (not self-assertion) · tests
pass · no secrets in the diff · nothing outside declared scope changed · the owning domain charter
(`.claude/memory/*`) updated with durable learnings · cost impact on metered paths known and
acceptable. Security & Auth are gates, not afterthoughts. Irreversible / ambiguous → escalate to the
human, briefly.

### /compact instructions
On summarize, **preserve**: decisions + their rationale, the current task's success criteria, the
modified-file list, cost/measurement findings (numbers + counter names), and open escalations.
**Drop**: dead ends, verbose search output, raw file/log dumps, and superseded intermediate states.
