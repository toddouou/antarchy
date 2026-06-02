# HIVE Rebuild Plan — Efficiency · Scale · UX

> **v2 (2026-06-01).** Supersedes the egress-only v1. Pressure-tested by a 15-agent adversarial review
> (8/9 load-bearing concerns confirmed against code); every change below is code- or math-grounded.
> Written so a future Claude Code agent with **zero memory of this session** can execute any single
> phase standalone. Engine = the Rust crate in `sim/` (axum + tokio + tokio-tungstenite + rayon,
> hosted on Railway). Client = `public/client.html` (embedded via `include_str!` → **recompile** to apply).

---

## 0. The mandate, the invariant, the honest read

### The mandate (three prongs — all required)
1. **Efficiency** — kill the ~$10,000/mo egress bill → **≤ $50/mo total**.
2. **Scale** — design for **~300+ concurrent connections, 1,000 queens, 100k ants**, a month-long season.
3. **UX** — *do not ship a worse game.* Playtesters **love** HIVE today (the CP1–CP4 overhaul: killfeed,
   welcome-back, workers-lifespan panel, leaderboard, regions, death screen, discovery). Protect every
   one, and harvest the upside this rebuild unlocks.

### This is your roadmap's H3 arriving early — not a detour
ROADMAP H3 already names the endgame: **"Cloudflare R2 + Workers — zero egress cost"** and **"CBOR over
WebSocket — 3–5× smaller than JSON."** The bill is forcing you to build H3's data architecture now, on a
codebase already ~90% prepared for it (the `hive-optimization` P1–P5 work is done: u16 palette TileMap,
100k-ant tick @10.4 ms, LOD pyramid, season ops). The same R2 snapshot pipeline that kills the bill is
the **virality engine** the roadmap has wanted since H1 (shareable PNGs, `?spectate=` links, world
minimap, daily time-lapse). This rebuild pulls the roadmap's destination forward, for free, under cost
pressure.

### The governing invariant — WIDENED to a per-connection RESOURCE budget
v1's invariant covered only billed bytes. That lets the plan pass its own gate while the game lags at
scale. The real invariant:

> **Per active connection, three resources — billed egress, server CPU-ms per viewport cycle, and
> retained memory — must each be independent of (a) total accumulated canvas size and (b) the count of
> players who have ever painted, and bounded per connection. Egress may scale only with live, in-view
> change for currently active viewers, and even that is hard-capped per connection.**

Every phase states which curve term it decouples (**world-size / player-count / frequency**) **and**
which resource (**egress / CPU / memory**). The acceptance gate is **three flat lines** (bytes/s,
`finish_view`-ms, MB) vs **both** painted-cells **and** connection-count — plus a separate
**gameplay-feel** gate (§9).

### Corrected targets (Railway egress = **$0.10/GB**, verified 2026-04 — [pricing](https://railway.com/pricing))
| Metric | Now (6 players) | Target (~300 players) |
|---|---|---|
| Total monthly egress cost | ~$10,000, climbing | **≤ $50 total** (≈ 500 GB/mo) |
| Egress / **active viewer**, sustained | day-1 ~**0.96 MB/s** → month-end ~**6.43 MB/s** | **≤ ~0.65 KB/s** (≈1.7 GB/viewer/mo; <2 GB ceiling = 0.77 KB/s) |
| Per-viewer reduction needed | — | **~1,500× off the day-1 floor; ~9,900× off month-end-dense** |
| Egress vs. canvas size | grows | **flat** |
| Egress vs. player count | multiplicative | **linear, hard-capped per connection** |
| **Server CPU / viewport cycle** | O(connections × viewport) on the *sim's* rayon pool | **isolated pool; tick p99 flat 6→300 conns** |
| **Persistence stall** | ~multi-sec gzip-JSON under the write lock every 60s | **off-lock; no tick stall** |
| Bulk / snapshot / far-zoom | live from billed origin (no CDN) | **Cloudflare R2 ($0 egress)** |
| Live updates | uncompressed JSON, 50 Hz ant frames, full-fog/frame | **binary deltas, compressed, coalesced, bounded** |

> **Math correction (v1 was self-contradictory).** v1 claimed a "12–20×" season ramp; its own figures
> give **6.7×** (6.43 / 0.96 MB/s). State it correctly: the **monthly billed ramp is ~6.7×** (the
> canvas-coupled portion, attacked by R2 + palette/fog). The "10–20×" figure is a *different* thing —
> the **per-keyframe entropy ramp** (empty viewport ~1–3 KB deflated → dense ~30–60 KB), attacked by
> compression + palette. **The binding constraint on $50/mo is the day-1 ~0.96 MB/s/viewer FLOOR, not
> the ramp** — and no single phase kills it. It falls to compression + per-conn cap + fog-delta +
> leaderboard diffs **together**; R2 then removes the ramp on top.

### Honest read (for the team)
Both **≤ $50/mo AND a better game** are achievable — but **not by v1 as written**. Ship v1 unchanged and
you cut the bill ~20–50× (a real, >$8,500/mo win) but land at ~$200–1,000/mo at 300 players, with a
*laggier* game at scale than at 6, because v1 never carried the arithmetic to the finish line and ignored
the CPU/persistence walls. This v2 closes the math (budget ledger), folds in the two scale walls, adds a
gameplay-feel gate, and harvests the UX upside. **Milestone A (below) kills most of the bill in days,
behind feature flags, with zero new infrastructure** — that's the morale win to ship first.

---

## 1. Progress tracker  *(executing agents: update Status + measured numbers after each phase)*

| Phase | Name | Decouples | Resource | Status | Measured after |
|---|---|---|---|---|---|
| 0 | Diagnose + build the **budget ledger** | measurement | all | **DONE** (2026-06-02) | counters at send sites + `/egress-stats` + CPU/mem on `/health` + dirty-chunk counter; runtime-verified |
| 1 | Measurement harness (3 axes + feel gate) | measurement | all | **DONE** (conn-sweep + worst-case) | in-process WS harness `bench_egress_conn_sweep`; canvas-growth axis still TODO |
| 2 | **Rayon pool isolation** (CPU guarantee, FIRST) | — | CPU | **DONE** (2026-06-02) | tick p99 **flat ~0.65–0.73 ms** across 6→300 conns (gate PASS) |
| 3 | Compress firehose + fog-delta + frame-kind namespace + `me`-split + ant-rate cut | frequency/encoding | egress | **DONE (3A+3B)** (2026-06-02) | **4.2× per-viewer @ k=6** (send 317→76 KB/s; 3A=317→125, 3B ant-rate 50→15 Hz=125→76); tick p99 still flat 0.24–0.47 ms; admin worst-case ⇒ understates fog-delta; ant_hz/ant_view_cap admin-tunable |
| 4 | Activity-aware per-conn cap + coalesce + channel taxonomy | frequency / per-conn | egress | NOT_STARTED | — |
| 5 | Palette + **leaderboard/stats send-on-change** | player-count | egress/CPU | NOT_STARTED | — |
| **★ A** | **MILESTONE A — ship 0–5 to :8080 behind flags** | — | — | NOT_STARTED | **target ~$10k→$200–1,400/mo** |
| 6 | **R2 snapshot tiles** (H3 endgame) | **world-size** | egress | NOT_STARTED | — |
| 7 | Connection hygiene + reconnect ordering | frequency | egress | NOT_STARTED | — |
| ∥A | Off-lock persistence (WAL + bincode) — PARALLEL | — | CPU | NOT_STARTED | — |
| ∥B | Base-map sovereignty (licensed tiles via R2/Worker) — PARALLEL | — | launch-gate | NOT_STARTED | — |
| UX | UX-upside track (PNG / spectate / minimap / time-lapse) | — | growth | NOT_STARTED | — |

**Single highest-leverage move:** build Phase 0's **end-to-end KB/s-per-viewer budget ledger** with
rayon-pool isolation baked into the widened invariant, and make **$50/mo provable at a fixed operating
point before building R2** — because the real residual is the *live edge* (fog-on-deltas + leaderboard
fan-out + ant frames) and the *CPU wall*, none of which R2 touches.

---

## 1b. SESSION RECAP — 2026-06-02  ⬅ **RESUME at Phase 4 (zero-memory next chat)**

> **UPDATE — Phase 3B SHIPPED (2026-06-02, same session as 3A; Phase 3 now COMPLETE).** The ant-rate
> cut + smooth-crawl work landed (compiles clean, **25 unit tests pass** incl. new
> `ant_cap_subsamples_stably_by_id`). Three parts: **(1)** ant-frame cadence is now tunable
> `cfg.ant_hz` (admin slider **ANT RATE**, clamp 5–60) and **defaults to 15 Hz** (was the full ~50 Hz);
> the `viewport_loop` gate sends one ants-only frame every `tick_rate/ant_hz` ticks (`last_ant_tick`;
> tile frames carry ants too so they reset the ant clock). **(2)** Client **grid-aware (Manhattan)
> interpolation** replaces the diagonal chord-lerp (`client.html` ant render): ants walk along grid
> lines (crawl, no corner-cut) at low rate, and it's **identical to the old lerp at 50 Hz** so raising
> the knob is a perfect escape hatch. **(3)** **Visible-ant cap** `cfg.ant_view_cap` (default 4000,
> 0=off): above it, `snapshot_view` subsamples by a **stable id-stride** (frame-stable, no flicker) —
> bounds dense-battle egress, no-op in normal play. Harness: **per-viewer send 125→76 KB/s @ k=6
> (4.2× total vs the original 317 baseline)**, tick p99 still flat (0.24–0.47 ms), cap dormant at 160
> ants as designed. **Owner choice: shipped aggressive (15 Hz).** Owner browser check (do post-deploy):
> place ~10 ants → they crawl cell-by-cell not slide; watch a turn → L-shape not diagonal; drag the
> ANT RATE knob 5↔60 → smooth across the range; rollback = set it to 50 (or `HIVE_BIN_CTL=0` for all of
> Phase 3). **Files (3B):** `config.rs` (ant_hz/ant_view_cap), `server.rs` (cadence gate),
> `network.rs` (`cap_ants_by_id` + snapshot_view), `public/client.html` (Manhattan interp + ANT RATE
> slider/CFG_SLIDER_MAP/bindSlider/presets). **⬅ NEXT = Phase 4** (activity-aware per-conn cap +
> channel taxonomy), then **Phase 5** (palette + leaderboard/stats send-on-change), then **Milestone A**.

> **UPDATE — Phase 3A SHIPPED (2026-06-02, later session).** The zero-feel-risk byte-win subset of
> Phase 3 is done: **binary frame-kind namespace + a second ordered binary control channel**
> (`me`/leaderboard/stats/region-holders compressed, kinds 16–19), **fog-on-keyframes-only** (deltas
> tagged `nofog`), **packed+deflated ant frames** (kind 3, *still 50 Hz*), and the **`me`-split**
> (static cfg/geo/world/spawn ride `logged-in` once). Compiles clean on `target-dev`; all 24 unit
> tests pass (incl. new `packed_ants_round_trip`, `bin_delta_omits_fog`); conn-sweep harness:
> **2.5× per-viewer egress @ k=6** (send 317.1→125.5 KB/s, recv 314.8→123.6), **tick p99 flat
> 0.18–0.30 ms** (gate held), `finish_view` p99 *down* (13.5→9.0 @ k=6, 322→205 @ k=300). NOTE the
> harness viewers are **admins (un-fogged)**, so the fog-delta win is *understated* — real fogged
> players save the full ~25 KB/s on top, so the day-1 fogged number is below 125 KB/s.
>
> **Design deviation (intentional, lower-risk):** rather than convert the priority channel
> `mpsc<String>`→`mpsc<Message>` (~100 send sites), a **parallel binary control channel** was added
> (`Player.ctl_tx: UnboundedSender<Vec<u8>>`) beside the untouched text `tx`. Gated by a per-conn
> capability **`Player.bin`** = client-advertised `{"bin":1}` at login AND the master flag
> **`config::bin_ctl_enabled()`** (env `HIVE_BIN_CTL`, default on). This per-conn flag gates **all
> three** upgrades (ant kind 3, fog-on-keyframes, binary control), so an old tab across a redeploy
> (never advertises `bin`) transparently keeps the legacy text/JSON protocol — no version-skew break.
> Rollback: `HIVE_BIN_CTL=0` (or any client omitting `bin`) → full legacy path.
>
> **Files touched (3A):** `config.rs` (`bin_ctl_enabled`), `world.rs` (`Player.ctl_tx`/`bin`,
> `broadcast_ctl`), `server.rs` (Cmd::Auth + ctl channel + 3rd write-task select arm + viewport_loop
> routing + harness `bin:1`), `handlers.rs` (`apply_bin_cap`, `build_player_info(..,true)`),
> `network.rs` (`ctl_frame`+`CTL_*`, kind-3 packed ants, fog-on-delta, `build_player_info(full)`),
> `simulation.rs` (holders via `broadcast_ctl`), `persist.rs` (Player fields), `public/client.html`
> (binary router on byte0, `handleCtlBin`, kind-3 unpack, fog retained on keyframe / reused on delta,
> `bin:1` on all 3 auth sends).
>
> **⬅ NEXT = Phase 3B** (the feel-risky remainder, gated on owner in-browser A/B — see the bottom of
> the §1b original "NEXT" block, now retitled "3B remaining"): ant-Hz slider (default 50 = no cut) +
> path-aware dead-reckoning interpolation (current chord-lerp `client.html` corner-cuts turning ants
> at low cadence) + zoom-tied visible-ant budget with hysteresis. **Owner browser checklist for 3A**
> (do before 3B): with a fresh client (advertises `bin:1`) — (a) a **kill event still toasts/killfeeds**
> (text path), (b) **leaderboard / region tabs / header stats update live** (now binary control),
> (c) **WORKERS lifespan bars tick + army/credits/refill/HP/XP track** (after `me`-split),
> (d) fog reveals/conceals correctly while panning a frontier, (e) ants render as before (50 Hz).

**Shipped the PRIOR session (P0/P1/P2)** (all on the `sim` git repo; compiles clean on `target-dev`; **P0 runtime-verified**, harness runs green). Everything is **additive / internal — zero wire or UX change yet**, so it is safe to deploy as-is.

- **P0 — DONE.** New leaf module **`src/metrics.rs`** (global, lock-free): per-kind billed-byte counters recorded at the *actual* `ws_tx.send` sites (`server.rs` Text@~127 / Binary@~136, classified by `quick_msg_type` / frame byte-0), `viewport_cycle_ms` + `finish_view_ms` rings + a `PREVGRID_BYTES` gauge (recorded in `viewport_loop`), and a cheap **dirty-chunk-touch counter** in `tile_map.set` (serde-skip fields). New **`/egress-stats`** route; CPU/mem fields added to **`/health`**.
  - *Deviation (intentional):* the timing rings live in `metrics`, **not** in `World` as the plan text suggested — `viewport_loop` holds no write lock in Phase B, so a `World` ring would force lock contention. The global module is the correct home.
  - *Dirty-chunk counter* is a conservative chunk-touch **transition** count (over-estimate of distinct dirty chunks); exact `FxHashSet` tracking is deferred to P6 as planned.
- **P2 — DONE.** Dedicated viewport `rayon::ThreadPool` built in `main.rs` (env `HIVE_VIEWPORT_THREADS`, default ~½ cores); both viewport `par_iter` sites wrapped in `pool.install` (`server.rs::viewport_loop`). Sim tick stays on the global pool.
- **P1 — DONE.** Harness as **`#[cfg(test)] mod egress_bench` in `src/server.rs`** — **not** `tests/egress_harness.rs`: this is a `[[bin]]` crate with **no lib target**, so integration tests can't reach internals. This follows the existing `bench_*` convention (`simulation.rs`). It boots the full stack on an **ephemeral 127.0.0.1 port**, seeds admin viewer accounts **in-memory** (no `users.json` writes; admins get an un-fogged worst-case viewport), spawns an NPC swarm, and sweeps K∈{6,25,100,300}. Run:
  `CARGO_TARGET_DIR=target-dev cargo test --release bench_egress_conn_sweep -- --ignored --nocapture`
- **P0 doc fixes (step 7):** verified **already applied in v2** — no stale "12–20×" ramp claims remain (every instance is the v1-quote, the changelog, or ant-cadence Hz).

**Measured — DEBUG run (release numbers being produced by a monitor agent):**

| conns | recvKB/s/vw | sendKB/s/vw | tickP99 ms | vpCycP99 ms (debug) | prevGrMB |
|---|---|---|---|---|---|
| 6 | 42.2 | 44.0 | 0.63 | 303 | 5.5 |
| 25 | 18.7 | 18.9 | 0.73 | 996 | 23 |
| 100 | 0* | 0* | 0.72 | 996 | 23 |
| 300 | 0* | 0* | 0.65 | 1037 | 92 |

\* recv≈0 at K≥100 is a **debug-only** artifact: one un-fogged 480k-cell viewport × 100+ viewers takes >4 s/cycle in an unoptimized build, exceeding the measurement window. Release `finish_view` is ~20–50× faster → expect real numbers at all K. **recv≈send** at K=6 (42 vs 44) cross-checks the counters.
**RELEASE run** (monitor agent, 3× consistent; un-fogged worst case, 160-ant swarm, 4 s windows):

| conns | recvKB/s/vw | sendKB/s/vw | tickP99 ms | vpCycP99 ms | finP99 ms | prevGrMB |
|---|---|---|---|---|---|---|
| 6 | 314.8 | 317.1 | 0.27 | 18.2 | 13.5 | 5.5 |
| 25 | 259.6 | 260.1 | 0.33 | 59.8 | 40.9 | 23.0 |
| 100 | 51.8 | 51.9 | 0.31 | 196.1 | 111.9 | 91.8 |
| 300 | 28.1 | 28.1 | 0.33 | 598.9 | 322.0 | 275.4 |

**Reads:**
- **P2 GATE PASS (decisive):** tick p99 flat **0.27–0.33 ms** across 6→300 conns (~1.5% of the 20 ms budget, **zero upward trend**) while `finish_view` p99 climbs **13→322 ms**. The dedicated pool fully isolates the tick from per-viewer serialization. `recv≈send` within <1% at every K → counters sound.
- **`finish_view` is now the dominant scaling cost** (≈O(K), superlinear: 599 ms cycle p99 at 300 viewers). This is the known **O(players²) palette fan-out** — it directly motivates **P5** (palette + leaderboard send-on-change) and the R2 offload. Per-viewer egress *dropping* at high K (315→28 KB/s) is **delivery throttling** (slower cycle ⇒ fewer frames/s), **not** reduced data need.
- **Per-connection memory wall:** ~**0.92 MB/connection** retained `PrevGrid` (275.4 MB / 300) → ~0.9 GB projected at 1,000 conns. Bounded by MAX_DIM² per conn; flagged for the post-P6 changed-cell-only working set.

**Budget ledger (§5):** still to seed — needs a *dense* **release** run (debug throttles the cycle rate, suppressing per-viewer egress). The harness is the instrument; raise NPC `spawns` (currently 40 in `bench_egress_conn_sweep`) for a denser day-1/dense row.

**Owner browser checklist:** **NONE this session.** P0/P1/P2 are instrumentation + internal pool plumbing with no wire/UX change — nothing to visually validate or flag-flip. The first feel-gates arrive with **P3**.

**Flags:** none yet — P3 introduces the per-kind capability flags.

**Phase 3 step map** (✅ = shipped in 3A · ⏳ = remaining in 3B, owner-A/B-gated):
- ✅ **Step 1** kind-byte namespace + framing — shipped as the parallel binary control channel (see UPDATE above).
- ✅ **Step 2a** ant frames packed binary + deflate (kind 3) — shipped *at 50 Hz*.
- ⏳ **Step 2b** ant 12–20 Hz cadence slider + zoom-tied visible-ant budget + hysteresis — **3B**.
- ✅ **Step 3** fog-on-keyframes-only — shipped (`nofog` deltas).
- ✅ **Step 4** compress large periodic text — shipped (leaderboard/stats/holders binary; small events stay raw text).
- ✅ **Step 5** `me`-split — shipped (`build_player_info(full)`).
- ⏳ **3B also needs** path-aware dead-reckoning interpolation in `client.html` (current chord-lerp
  `~:2810` corner-cuts 90°-turning ants once cadence drops below ~50 Hz).

<details><summary>Original Phase-3 concrete-start notes (kept for reference)</summary>

1. **Kind-byte namespace + framing fix:** `0/1/2/3` viewport, **`16+` compressed control** (16=me,17=leaderboard,18=stats,19=region-holders,20=events). Switch the priority channel `mpsc<String>`→`mpsc<Message>` so control rides `Message::Binary`; client router (`client.html:1595`) reads `u8[0]`: `<16`→`handleViewBin`, `≥16`→inflate→`JSON.parse`→`handle()`. Keep the first `logged-in` as uncompressed `Text` so the `{compress}` capability flag is read first. **The client already inflates viewport binary via `DecompressionStream('deflate-raw')` (`client.html:1605`) — so only CONTROL frames are net-new compression.** `metrics::MsgKind`/`kind_for_bin` already reserve 16–20 for this.
2. Ant frames (uncompressed JSON, `network.rs:~415`) → packed binary + deflate; runtime 12–20 Hz cap (slider, not a const); zoom-tied visible-ant budget + hysteresis.
3. Fog-on-keyframes-only (~10 lines) — respect the client's **ephemeral `fogCache`** (reset to 100 on cache reorg).
4. Compress only large periodic text (leaderboard / welcome-back); small events stay raw **sync** JSON.
5. Split `me`: static `cfg/geo/world/spawn` → `logged-in` once; dynamic `me` ≥1 Hz keeps `ants[]`/army/credits/refill/hp/xp/region.
Reuse `deflate_raw` (`network.rs:323`). Verify: harness egress ↓3–6× **and** the owner browser feel-checklist (kill event reaches a binary-control client; 10 ants crawl not slide; WORKERS bars tick).

</details>

**Env/ops:** Windows — use **`127.0.0.1`** not `localhost` (server binds IPv4 `0.0.0.0`; `localhost`→`::1` refuses). All builds/tests on **`CARGO_TARGET_DIR=target-dev`**; the live game runs on **:8080 — never kill it**.

---

## 2. Diagnosis (corrected; pre-filled from the read-only trace, to be confirmed in Phase 0)

### 2.1 Stack & billed-egress paths
- One process, three contexts over one `Arc<RwLock<World>>`: tokio/axum server, sim thread (50 Hz),
  viewport thread. One **WebSocket per tab** carries ~all game bytes via two outbound channels merged by
  a `biased select!` (`server.rs:445-481`): a **priority `mpsc<String>`** (text/JSON events + `me`,
  ordered/never-dropped) and a **latest-wins `watch<Option<Vec<u8>>>`** (binary viewport frames).
- HTTP routes: `/`, `/health`, `/world-info` (`server.rs:509-513`). **No bulk/snapshot/CDN path exists.**
- Base map: browsers fetch OSM rasters directly from `[a-c].tile.openstreetmap.org`
  (`client.html:2396`) — third-party, **zero origin egress** (but a launch blocker — §2.4).

### 2.2 The THREE walls (egress is only the one that *bills*; the others *lag/crash*)
1. **Egress wall (bills).** The firehose is the **uncompressed JSON ant-overlay frame** at tick cadence
   (`network.rs:412-426`) + tile keyframes/deltas (the only `deflate_raw` path, `network.rs:322-327`) +
   uncompressed text broadcasts. The season ramp (~6.7× monthly) is driven by *frequency* (duty-cycle
   gate `server.rs:394-395`), *population* (ant frames), and *density* (keyframe entropy).
2. **CPU / smoothness wall (lags — CONFIRMED, conf 0.82).** `finish_view` runs **per-connection** on the
   **shared global rayon pool** the 50 Hz sim tick uses (`server.rs:461` `into_par_iter`; `main.rs` has
   no dedicated pool; Phase A `snapshot_view` is also per-connection parallel at `server.rs:429`). At 300
   viewers clustered on hot metros this is ~50× today's serialization load contending for the tick's
   cores → **re-triggers the exact tick-bunching wall `hive-optimization` P1 killed, now driven by
   viewers not ants.** v1 modeled only bytes, so it could pass the egress gate and stutter for everyone.
3. **Persistence wall (crashes smoothness every 60s — CONFIRMED, conf 0.83).** `persist::save`
   gzip-JSON-serializes the **entire world UNDER the sim write lock** (`server.rs:319` inside
   `blocking_write`; `persist.rs:131-153`; self-documented at `persist.rs:12-14`). The viewport thread
   parks on its read lock during serialize → **no frames for any client**. Dense `u16` chunks JSON-encode
   as literal int-lists (worst case); at a dense season-end world this is a **multi-second** stall that
   worsens as the canvas fills.

### 2.3 Per-source egress budget (per viewer; estimates pending Phase-0 counters; corrected sizes)
| # | Source | File | Compressed? | Size | Freq | Scope | Curve term |
|---|---|---|---|---|---|---|---|
| 1 | **Ant-overlay frame** | `network.rs:412-426` | **NO** | 5–50 KB ∝ visible ants | ≤50 Hz | per-viewer | frequency+population+density |
| 2 | Tile keyframe | `network.rs:363-404` | deflate | 20–60 KB ∝ density+palette | ~10 Hz if dirty | per-viewer | world-size+player-count |
| 3 | Tile delta (+ **full fog every frame**) | `network.rs:476-520`; fog `348-360` | deflate | fog ~**25 KB/s** realistic† | ~10 Hz | per-viewer | world-size(density) |
| 4 | **Leaderboard** | `network.rs:23-51` | **NO** | ~**16 KB** (top-100)‡ | 2.5 Hz | **broadcast O(conns)** | player-count fan-out |
| 5 | `me` (full cfg/geo every frame) | `network.rs:90-188` | **NO** | 3–6 KB | 1 Hz | per-viewer | frequency |
| 6 | server-stats | `network.rs:54-68` | **NO** | ~150 B | 1 Hz | broadcast | frequency |
| 7 | region-holders | `network.rs:73-87` gated `simulation.rs:819` | **NO** | 2–4 KB | **0.1 Hz** | broadcast | frequency |
| 8 | events (kill/xp/damage) | built at call sites; sent `world.rs:275-299` | **NO** | 50–500 B | game-driven | targeted/`broadcast_near` | frequency |

> † **Fog correction (v1 over-stated this).** Full `w×h` fog ships on *every* tile frame incl. deltas
> (structurally real, `network.rs:348-360,514`) — BUT it's a Chebyshev distance field that deflates
> ~**100:1**, so it's ~**25 KB/s realistic at 1:1 zoom**, not "hundreds of KB/s." Worth a ~10-line
> "fog-on-keyframes-only" fix (folded into Phase 3), **not** a headline, and **not** worth 1-bit
> quantization (deflate already wins).
> ‡ **Leaderboard correction.** Measured ~**16 KB** (not 20–40 KB). At 300 conns × 2.5 Hz uncompressed ≈
> **$3,110/mo**; after Phase-3 deflate alone ≈ **$518/mo** — still **10× the entire budget**. Top-N is
> *already* done (`LEADERBOARD_TOP_N=100`); the residual is **fan-out × frequency × connections**, which
> only **send-on-change diffs** (Phase 5) close. This is plausibly a bigger residual than ant frames at 300.

### 2.4 The base-map launch blocker (CONFIRMED, conf 0.86)
At hundreds of concurrent clients panning, OSM rate-limits/IP-bans **without notice** (Railway/NAT shared
egress IPs = the commercial-load pattern they block) → the map blanks for **everyone**. Orthogonal to the
Railway bill (OSM is already $0 origin), so invisible to the egress KPI but **fatal to "complete scale."**
Fix in Parallel-B. **A transparent OSM proxy is WORSE** (funnels the violation through one bannable egress
IP) — the fix is a **licensed/self-hosted** source.

### 2.5 Hypotheses — confirmed vs. killed (all verified against code)
**KILLED:** clients pull the whole canvas on load (no — viewport-scoped, `handlers.rs:50-74`); each ant
holds a connection (no — pure server-side sim, `world.rs` `Ant` has no channel fields); bulk served from
billed origin (no bulk path exists; the "bulk" is the live stream).
**CONFIRMED:** all text + ant frames uncompressed (tokio-tungstenite has **no permessage-deflate** —
[issue](https://github.com/snapview/tungstenite-rs/issues/2) — use app-level `deflate_raw` + client
`DecompressionStream('deflate-raw')`); broadcasts O(connections) (`world.rs:283-289` clones a `String`
per recipient); fixed-1500 ms reconnect with no backoff/jitter (`client.html:1591`); palette is O(players)
per cycle (`network.rs:14-20`); **plus the two non-egress walls (§2.2) and the framing bug (Phase 3).**

### 2.6 Verified externals
- **R2 = $0 egress** on all reads (r2.dev / Worker / S3 API), **$0.015/GB-mo** storage, free tier **10 GB
  + 1M Class-A (writes) + 10M Class-B (reads)/mo** — [pricing](https://developers.cloudflare.com/r2/pricing/).
  ⚠ R2 still **bills writes**: worst-case dirty-chunk churn could be ~$970/mo — Phase 0 must project it.
- **r/place pattern:** compact full-board bitmap snapshot + WS deltas, edge-cached —
  [Fastly/Reddit](https://www.fastly.com/blog/reddit-on-building-scaling-rplace).

---

## 3. Target architecture (three braided tracks)

```
                          ┌──────────────────────────────────────────────┐
                          │  Cloudflare R2 + Worker  (object store, $0 egress) │
                          │  snapshot tiles  /snap/{epoch}/{lod}/{cx}/{cy}.webp  (immutable) │
                          │  licensed base-map tiles (Parallel-B)               │
                          └───────────▲──────────────────┬───────────────────┘
        snapshot writer (off-lock,     │ Class-A write     │ Class-B read ($0 egress, immutable)
        dirty-chunks only, ~60-120s)   │                   ▼  browser fetches R2 DIRECTLY (never via origin)
┌──────────────────────────────┐      │        ┌────────────────────────────────────┐
│  HIVE-SIM origin (Railway)   │      │        │            Browser client            │
│  sim thread 50 Hz ───────────┼──────┘        │  • initial load / reconnect / far-zoom │
│  viewport thread (DEDICATED   │               │      → R2 snapshot tiles ($0)          │
│   rayon pool, isolated from   │  live deltas  │  • live in-view (lod==1) → WS keyframe  │
│   the tick) — Phase 2         │──────────────►│      + binary deltas (AUTHORITATIVE)   │
│  WS = ONLY the bounded live    │  (bounded,    │  • base map → licensed tiles (R2)      │
│  changing edge, compressed,   │   capped/conn)│  • optimistic ant/queen placement      │
│  coalesced, per-conn capped   │               │  • path-aware ant interpolation        │
└──────────────────────────────┘               └────────────────────────────────────┘
   off-lock persistence (WAL + bincode) — Parallel-A
```

**Authoritative-viewport invariant (the UX-saving rule).** The live in-view canvas (`lod==1`) is **always
WS-keyframe-authoritative**. R2 snapshots serve **only** (a) initial-load before the first WS keyframe,
(b) reconnect resync, (c) far-zoom `lod>1` overview, (d) off-screen prefetch. **Live deltas never layer on
an R2 base** — they layer on a WS keyframe (the delta protocol requires exact `baseSeq===clientGrid.seq`,
`client.html:1636`; an R2 base would seam/flash the actively-painted frontier playtesters love). On epoch
flip, **double-buffer** (prefetch new epoch's visible tiles, swap atomically).

How this satisfies the widened invariant: accumulated canvas → R2 (flat vs world size, $0 egress); live
edge → bounded compressed deltas, hard-capped per connection (linear in viewers); CPU → isolated pool +
R2 collapses per-viewer serialization (flat tick p99); persistence → off-lock (no stall).

---

## 4. Phases (execution order). Each is a self-contained work order.

Format per phase: **Status · Decouples (curve term / resource) · Root cause · Expected reduction (+how
confirmed) · Risk · Preconditions · Files · Steps · Verification (egress + gameplay) · Rollback · Done.**

---

### Phase 0 — Diagnose + build the budget ledger
- **Status:** NOT_STARTED · **Decouples:** measurement (all) · **Reduction:** 0 (instrumentation).
- **Root cause:** v1 proved *direction* (each phase decouples a term) but never proved *arrival* ($50/mo).
- **Risk:** minimal (counters + read endpoint). **Preconditions:** none.
- **Files:** `world.rs` (send sites), `server.rs` (write-task send sites + `/egress-stats`), `network.rs`
  (tag builders), `tile_map.rs` (dirty-chunk counter), `persist.rs` (note for ∥A).
- **Steps:**
  1. **Pull Railway billing** (owner can): plot egress vs calendar day, overlay `tilesPainted` from
     `/health`; confirm the canvas-size correlation and the **~6.7× monthly ramp**. Save `docs/egress/curve-*.csv`.
  2. Add `EgressCounters` (`once_cell` `[AtomicU64; N]` by `MsgKind`: Ant, TileKeyframe, TileDelta, Fog,
     Me, Leaderboard, Stats, RegionHolders, Event, HttpHtml). **Count at the actual `ws_tx.send` sites in
     the write task** (`server.rs:125,134`) — where a send is the *billed* event — **not** at enqueue (the
     latest-wins watch slot drops frames a slow client never gets; enqueue over-counts). Add a per-message
     WS-header estimate; note a ~5–10% TLS/TCP floor when reconciling.
  3. Add `viewport_cycle_ms` + `finish_view_ms` rings to `/health` (beside the existing `tick_ms_ring`,
     `world.rs:204`); report per-conn `PrevGrid` memory. **This widens the invariant to CPU + memory.**
  4. Add a **dirty-chunk-per-snapshot-interval counter** (cheap) so Phase 6's R2 Class-A write volume is
     *projected*, not assumed. Gate Phase 6 on staying < ~800k ops/mo.
  5. Expose `/egress-stats` (per-kind bytes/msgs/bytes-per-sec, conn count, dirty-chunks/interval).
  6. **Build the BUDGET LEDGER (the arrival-proof artifact)** — see §5. Fill its day-1-sparse and
     late-season-dense starting cells from billing + counters.
  7. **Doc fixes (zero-risk):** replace every "12–20×" with the derived **~6.7×** monthly ramp; quarantine
     "10–20×" to "per-keyframe entropy ramp"; state the **day-1 floor** is the binding constraint.
- **Verification:** `/egress-stats` reconciles ±10% with Railway over a window; per-kind % replaces §2.3
  estimates; ledger day-1 and dense cells filled; dirty-chunk projection computed.
- **Rollback:** counters additive; remove route + `fetch_add`. **Done:** ( ) billing curve ( ) send-site
  counters ( ) CPU+mem on /health ( ) dirty-chunk counter ( ) ledger seeded ( ) ramp doc fix.

---

### Phase 1 — Measurement harness (3 axes + gameplay-feel gate)
- **Status:** NOT_STARTED · **Decouples:** measurement · **Reduction:** 0.
- **Root cause:** later phases need a repeatable before/after that catches O(players)/O(connections) and
  CPU regressions — not just canvas growth.
- **Risk:** test-only (`PORT=8090 CARGO_TARGET_DIR=target-dev`, never :8080). **Preconditions:** Phase 0.
- **Files:** new `tests/egress_harness.rs` (reuse the `#[ignore]` release-bench convention).
- **Steps:**
  1. K synthetic `tokio-tungstenite` clients (realistic `view-set` + periodic `place-ant`), **tallying
     bytes received per client** (the watch slot drops frames → enqueue ≠ delivered; receive-side is the
     only honest end-to-end number).
  2. M ants via admin `add-ants`/`spawn-npc`.
  3. **Three sweeps:** (a) canvas-growth (1e6→1e9 painted cells), (b) **connection-count (6→300)**,
     recording bytes/s/client **AND** server-side `tick p99` / `viewport_cycle_ms` from `/health`.
  4. **Acceptance = three flat lines** (bytes/s, `finish_view`-ms, MB) vs **both** painted-cells **and**
     connections. A lower-but-sloping line means *reduced, not decoupled* — fails the invariant.
  5. Add the **"densest active war, single viewer"** scenario asserting **both** under-cap **and**
     above-gameplay-floor simultaneously (turns the cost↔feel tension into a pass/fail gate).
- **Verification:** reproduces ±15% the Phase-0 live split at matched K/M/canvas.
- **Rollback:** delete the test. **Done:** ( ) K+M drivers ( ) 3 sweeps ( ) receive-side tally ( ) feel
  scenario ( ) baseline curves recorded.

---

### Phase 2 — Rayon pool isolation (CPU guarantee — sequence FIRST among engineering work)
- **Status:** NOT_STARTED · **Decouples:** CPU · **Reduction (egress):** 0 — but it is the prerequisite
  that lets every later phase add per-connection work without starving the tick.
- **Root cause:** §2.2 wall #2 — `finish_view`/`snapshot_view` per-connection on the shared global pool.
- **Expected effect:** tick p99 stays flat as connections go 6→300 (confirm via Phase-1 axis).
- **Risk:** low (pool plumbing; no protocol/UX change). **Preconditions:** Phase 1 (to measure).
- **Files:** `main.rs` (build pool), `server.rs` (`viewport_loop` `par_iter` sites `429,461`).
- **Steps:**
  1. Build a **dedicated bounded rayon pool** (`ThreadPoolBuilder::num_threads(k).build()`), and run the
     viewport `par_iter`s inside `pool.install(|| …)`. Leave the sim tick on the default pool (or give it
     its own). Now viewer CPU **can never contend** with the tick's move-plan / `par_sort_unstable`.
  2. **Defer** bespoke fog/frame dedup-by-quantized-viewport — fog depends on `player_id` (own tiles are
     seeds, `fog.rs:25`) so it is **not** shareable across owners; the real dedup is making the live edge
     owner-agnostic, which Phase 6 (R2) largely achieves. Build dedup only if the harness later proves a
     residual stutter.
- **Verification:** Phase-1 connection sweep — `tick p99` and `viewport_cycle_ms` stay flat 6→300.
- **Rollback:** drop `pool.install`, revert to the global pool. **Done:** ( ) dedicated pool ( ) tick p99
  flat to 300 conns in harness.

---

### Phase 3 — Compress the firehose + fog-delta + frame-kind namespace + `me`-split
- **Status:** NOT_STARTED · **Decouples:** frequency/encoding · egress · **Reduction:** ~3–6× total (text
  5–8× on its share; ant frames 4–6× from packing+deflate, plus the rate cap). Confirm via harness.
- **Root cause:** §2.5 — ant frames + all text uncompressed; no permessage-deflate → app-level deflate.
- **Risk:** medium (wire format; client decode). Reversible per-kind via capability flag.
- **Preconditions:** Phases 0,1,2.
- **Files:** `network.rs` (ant frame, text builders, `body_delta` fog), `server.rs` (priority channel
  type, ant-frame gate), **`world.rs:275-299` (send sites — v1 omitted these)**, `client.html` (`:1595`
  router, `handleViewBin`, decode).
- **Steps:**
  1. **Unified frame-kind namespace (FATAL bug fix — CONFIRMED conf 0.93).** Today the client routes by
     *transport type* (`client.html:1595`: string→`handle()`, binary→`handleViewBin()`), and
     `handleViewBin` silently drops unknown kinds (`:1645`). Wrapping text as `Message::Binary` would
     **break all control delivery**. Define **one** byte-0 namespace: `0`=ants-only, `1`=keyframe,
     `2`=delta, `3`=packed-ant; **`16+`=compressed control** (16=me, 17=leaderboard, 18=stats,
     19=region-holders, 20=events). Change `:1595` to read `u8[0]`: viewport kinds→`handleViewBin`,
     control kinds→inflate→`JSON.parse`→`handle()`. Change the **priority channel `mpsc<String>` →
     `mpsc<Message>`** so control rides `Message::Binary` while staying **ordered/never-dropped** (do
     **not** move it to the coalescing watch slot). Keep the first `logged-in` as uncompressed `Text` so
     the `{compress:…}` capability flag is read before any binary control frame. Gate everything behind
     that per-kind flag.
  2. **Ant frames → packed binary + deflate**, rate-cap to a **runtime-tunable 12–20 Hz** (a slider, NOT
     a hardcoded `tr/12`). Quantize coords to viewport-relative `u16`; delta-code positions. Pack
     `(dx,dy,owner,kind)`.
  3. **Visible-ant budget tied to ZOOM/served-pixels** (one ant per few px is the perceptual ceiling),
     **not** a flat N=400; uniform **spatial stride** + **hysteresis** on the boundary (no pan flicker).
     The LOD path already drops sub-pixel ants when zoomed out.
  4. **Fog becomes a delta** (folded here — ~10-line change): send full fog only on keyframes
     (`KF_INTERVAL=15`); on deltas omit it, client retains the last field (it already keeps `clientGrid`).
     Real but ~25 KB/s — do this, skip 1-bit quantization.
  5. **Compress ONLY large periodic text** (leaderboard, welcome-back). **Leave small/discrete events**
     (kill, queen-dead, level-up, ant/queen-placed, damage) as **raw JSON on the synchronous path** —
     async `DecompressionStream` adds a microtask hop to latency-sacred acks for ~zero byte savings on
     50–500 B messages.
  6. **Split `me`:** static `cfg/geo/worldW/H/spawn` → once in `logged-in`; keep a **dynamic `me` at
     ≥1 Hz** still carrying `ants[[id,remTicks,kind]]` (WORKERS lifespan bars), `army`, `credits`,
     `nextRefillMs`, queen `hp/level/xp/tiles`, `region`. Ensure reconnect populates geo from `logged-in`,
     not periodic `me`.
- **Verification (egress):** harness bytes/s/client ↓3–6× at fixed K/M/canvas; `/egress-stats` ant+text
  kinds fall proportionally. **Verification (gameplay, on :8090):** (a) a **kill event still arrives at a
  binary-control client**, killfeed instant; (b) place 10 ants — they crawl (not slide); (c) WORKERS
  active-list lifespan bars tick + army/cap/refill track after the `me` trim.
- **Rollback:** per-kind `{compress:false}` in `logged-in` reverts any kind to legacy JSON/text.
- **Done:** ( ) namespace + binary router ( ) priority chan `mpsc<Message>` ( ) ant binary+deflate+slider
  ( ) zoom-aware ant budget ( ) fog-on-keyframes ( ) large-text-only compression ( ) `me` split
  ( ) gameplay acceptance a–c pass.

---

### Phase 4 — Activity-aware per-connection cap + coalesce + channel taxonomy
- **Status:** NOT_STARTED · **Decouples:** frequency / per-conn bound · egress · **Reduction:** ~2–4×
  on top of Phase 3; converts worst-case spikes into a flat ceiling **without throttling the marquee battle**.
- **Root cause:** the invariant's "bounded per connection" clause; protect the engaged player.
- **Risk:** medium (perceived rate under load; activity-aware so it bites only pathology).
- **Preconditions:** Phases 1–3.
- **Files:** `server.rs` (per-conn meter; `viewport_loop` per-pid builder `429-439`; write task).
- **Steps:**
  1. **Meter = shared per-conn `AtomicU64`** sliding window, **incremented by the write task on real
     send**, **READ by `viewport_loop`'s per-pid job builder before it builds/skips a frame** (the meter
     and the cadence-control point are on different threads — v1's "meter on the write task" can't
     down-shift because the frame is already built and the watch slot already coalesced).
  2. **Cap sized GENEROUSLY to PASS a full dense battle viewport** (validated by the harness feel
     scenario), reserved for **pathology** (hidden tab, idle, genuine send-queue backlog). Down-shift
     order touches ant cadence **LAST**: hidden-tab pause ⟶ widen tile interval ⟶ reduce visible-ant N ⟶
     ant cadence (**never below a ~5–8 Hz floor while ants are in view**).
  3. **Channel message TAXONOMY** (an enum/const set consumed by the Phase-3 compressor + this coalescer):
     **NEVER-DROP one-shots** (welcome-back, queen-dead, kill, level-up, ant/queen-placed, force-logout,
     world-wiped, shop-ok); **COALESCEABLE latest-wins** (me, leaderboard, stats, region-holders);
     **NO-COMPRESS small** (events/acks). The coalescer operates per-kind on the coalesceable set **only**.
  4. P4 must **count priority-channel (text) bytes** and put leaderboard/stats **into** the down-shift list.
- **Verification (egress):** a deliberately over-budget client is pinned at the cap; fast clients
  unaffected. **Verification (gameplay):** a client actively fighting a **100-ant, 5-owner** viewport is
  **NOT** down-shifted; disconnect 60s + reconnect during a busy tick → **welcome-back still arrives**;
  die during a busy tick → **death screen still enriches**.
- **Rollback:** `EGRESS_CAP_KBPS` very high (disables cap); coalescing is latest-wins-safe.
- **Done:** ( ) shared-atomic meter read by viewport_loop ( ) activity-aware cap + floor ( ) taxonomy
  ( ) leaderboard/stats in down-shift ( ) feel acceptance pass.

---

### Phase 5 — Palette + leaderboard/stats send-on-change
- **Status:** NOT_STARTED · **Decouples:** player-count · egress + CPU · **Reduction:** palette modest at
  6 players, large at scale; **leaderboard diff is the big one at 300** (closes the ~$518/mo residual).
- **Root cause:** §2.3 — palette O(players)/keyframe; leaderboard/stats O(connections) full-JSON broadcast.
- **Risk:** low–medium. **Preconditions:** Phases 1–3.
- **Files:** `network.rs` (`get_palette`, keyframe/delta palette, leaderboard/stats builders), `world.rs`
  (`broadcast`), `client.html` (palette merge `1610,1628,1638`).
- **Steps:**
  1. **Palette send-on-change:** server-side palette **version** (bump on color change / new owner); send
     full palette only on version change, else a version tag. Client caches by version (it already merges
     `tileIdsAdd` via `_mergePalette`). Keyframes still ship the viewport-bounded **local index map**.
  2. **Leaderboard + server-stats send-on-change:** sequence + **row-level diffs** (only changed ranks).
     Ranks shift on kills/level-ups, not every 400 ms → steady-state diff ≈ empty (orders of magnitude
     beyond compression). **Cut wire to top-20** (ROADMAP H2 spec). **Gate the `O(queens log queens)` sort
     behind the change trigger** so it stops running at 2.5 Hz when nothing changed.
  3. **`broadcast` fan-out:** replace the per-recipient `String::to_string` (`world.rs:283-289`) with a
     single `Arc<[u8]>`/`Arc<str>` fanned to all sends (CPU + RAM win).
- **Verification:** harness at high simulated player/owner count — keyframe size **and** leaderboard
  bytes/s flat vs player count; small-player case unchanged within noise.
- **Rollback:** force palette version mismatch / full-leaderboard each cycle. **Done:** ( ) palette
  version+cache ( ) leaderboard/stats diffs + top-20 + gated sort ( ) Arc fan-out ( ) flat vs players.

---

### ★ MILESTONE A — ship Phases 0–5 to the live :8080 server (behind capability flags)
- **What ships:** counters + harness + rayon isolation + firehose compression + fog-delta + per-conn cap
  + palette/leaderboard diffs. **Zero new infrastructure, no R2 account, no external secret, no new client
  load path** — fully reversible per-kind.
- **Projected result:** ~$10k/mo → roughly **$200–1,400/mo at 6 players** (conservative ~7×, optimistic
  ~50×; the ant-frame fix alone plausibly ~15–20× on its share). A **>$8,500/mo** cut in **days**.
- **Why first:** de-risks the morale problem immediately and gives the harness a real before/after to
  validate Phase 6 against. **Deploy this before starting the R2 lift.**
- **Gate to proceed to Phase 6:** budget ledger (§5) shows the **live-edge residual** at a fixed dense
  operating point, so Phase 6's job is quantified.

---

### Phase 6 — R2 snapshot tiles (the world-size decoupler; ROADMAP H3, early)
- **Status:** NOT_STARTED · **Decouples:** **world-size** · egress · **Reduction:** moves all *accumulated*
  canvas (initial-load, reconnect, far-zoom) to $0-egress R2 → **flattens the ramp + offloads the bulk**.
- **Root cause:** §2.2 wall #1 density/world-size coupling + the season ramp.
- **Risk:** high (external dep, new client load path) — reversible (`SNAPSHOT_CDN=off` → live keyframes).
- **Preconditions:** Phases 0–5 + the dirty-chunk projection < ~800k Class-A/mo.
- **Files:** new `src/snapshot.rs` (rasterizer + R2 upload), `tile_map.rs` (**dirty-chunk tracking + pub
  accessor**), `server.rs` (snapshot-writer task, expose `epoch`), `client.html` (snapshot fetcher +
  compositor), Railway env (R2 creds).
- **Steps (the v1 blocking gaps, now pinned):**
  1. **Keying — game space, native chunk grid (NOT Mercator).** Key `/snap/{epoch}/{lod}/{cx}/{cy}.webp`
     where `(cx,cy)=(x>>8,y>>8)` over the engine's existing 256×256 game-tile chunks. The client maps its
     camera **game-rect** (`drawOsmTiles`' `gx0..gx1/gy0..gy1`, `client.html:~2442`) by `>>8`. **Do not**
     route snapshot keys through `gameToOsm*` (that's Mercator, for the OSM basemap only). (v1 hand-waved
     "like drawOsmTiles" → projection clash that stalls a fresh agent.)
  2. **Dirty-chunk tracking (absent today).** Add `dirty_chunks: FxHashSet<u64>` to `TileMap`, insert
     `chunk_key` in `set()` **both paint and clear branches** and on `clear()/clear_owner()/wipe`; add
     `drain_dirty_chunks()` + a **pub per-chunk read accessor** (`chunks` is private). Seed dirty with all
     chunks on boot / persist-restore. This makes "only changed chunks re-upload" real and bounds Class-A.
  3. **Rasterizer:** write each dirty chunk to a **PNG/WebP RGBA tile** (unpainted/ocean = transparent →
     size ∝ painted perimeter). **PNG/WebP, not packed bitmap**, so the SAME tiles become MapLibre raster
     textures + WebGL2 quads when H1 lands, and double as the shareable-PNG / minimap assets. Lean on the
     Phase-5 send-on-change palette for id→color (don't re-embed a full palette per tile).
  4. **Far-zoom pyramid (net-new):** majority/argmax-owner mip-reduction over the chunk grid up to far
     zoom. (`snapshot_view`'s LOD path and `tally_owners_in_circle` are samplers, **not** pyramid builders.)
  5. **Upload to R2** with immutable long `max-age` + content hash; bump a global **`epoch`** per
     generation (cache-busting). Expose `epoch` in `world-info`/`me`.
  6. **Authoritative-viewport invariant (§3) — non-negotiable.** R2 fills only initial-load / reconnect /
     far-zoom / off-screen; live in-view stays WS-authoritative; deltas never layer on an R2 base; epoch
     flip double-buffers. **Invariant: no snapshot byte transits the Railway origin** (browser fetches R2
     directly, like OSM does today — add a Phase-0 assertion that origin tile bytes → ~0 after Phase 6).
  7. Add `Cache-Control`/ETag to `client.html` (`server.rs:55`) so refreshes don't re-pull the HTML.
- **Verification:** harness **flat-line passes** — billed-origin bytes/s/client no longer grow with
  `tilesPainted`; R2 Class-B reads carry the bulk (free); origin tile bytes ≈0 on initial-load/far-zoom.
  **Gameplay:** pan a fast front across a snapshot seam + trigger an epoch flip → **no seam, no flash**;
  street→continent in one gesture → smooth, no white flash.
- **Rollback:** `SNAPSHOT_CDN=off` → live keyframe path; snapshots are derived/regenerable (no world risk).
- **Done:** ( ) dirty tracking + accessor ( ) game-space keying ( ) WebP rasterizer ( ) far-zoom pyramid
  ( ) epoch + immutable headers ( ) authoritative-viewport invariant ( ) origin tile bytes ≈0 ( ) seam/flash test ( ) flat-line passes.

---

### Phase 7 — Connection hygiene + reconnect ordering
- **Status:** NOT_STARTED · **Decouples:** frequency · egress · **Reduction:** small steady-state, large
  during incidents (prevents reconnect storms re-pulling full keyframes).
- **Risk:** low. **Preconditions:** backoff/idle (steps 1–2) none; snapshot-resync (step 3) needs Phase 6.
- **Files:** `client.html` (`:1591` reconnect, visibilitychange), `server.rs` (`view-pause` gating
  `423-424`), `handlers.rs` (`view-pause`/`view-resume` beside `view-set` `:109`).
- **Steps:**
  1. **Exponential backoff + jitter** (`min(30s, 1.5^n)` ± 0–1s). **Keep the last good `clientGrid`
     through a brief drop** (null only after N failures) → no white flash on transient drops.
  2. **Idle/visibility cull:** `view-pause`/`view-resume` on `visibilitychange`; server stops viewport
     frames for paused/hidden tabs (gate on `p.view`). Hidden tabs → ~0 egress.
  3. **Reconnect path order (flagship returning-player moment):** R2 snapshot paints instantly → first WS
     keyframe takes over the live viewport → **welcome-back** diff overlays. Welcome-back is a NEVER-DROP
     one-shot (taxonomy, Phase 4).
- **Verification:** simulate a server bounce with K clients → reconnects spread (no 1500 ms spike); hidden
  tabs → ~0 viewport egress; reconnect shows instant world + welcome-back.
- **Rollback:** revert client reconnect/visibility; remove `view-pause`. **Done:** ( ) backoff+jitter
  ( ) keep-grid-on-drop ( ) visibility pause ( ) reconnect ordering + welcome-back intact.

---

### Parallel-A — Off-lock persistence (WAL + bincode)  *(no dependency on the egress phases)*
- **Status:** NOT_STARTED · **Resource:** CPU/smoothness · **Root cause:** §2.2 wall #3.
- **Files:** `server.rs:319` (save call site), `persist.rs`.
- **Steps:** (1) Move `save` **off the write lock** — mirror the viewport pattern: short read lock to
  grab/iterate chunks, gzip **unlocked on a dedicated thread**. (2) **JSON → bincode** (`persist.rs:14`
  already flags it; `Box<[u16]>` cells = 2 bytes vs ASCII int-list → 3–8× smaller + faster); bump
  `SNAPSHOT_VERSION`, keep the JSON loader one release. (3) Adopt ROADMAP H2's **WAL** (per-tick
  changed-tile journal + infrequent full snapshot). (4) Add `bench_snapshot_save` to the `#[ignore]` suite.
- **Verification:** `bench_snapshot_save` shows no multi-second window; viewport thread never parks on save
  (no frame gap at autosave). **Rollback:** keep JSON path / revert to under-lock save.
- **Done:** ( ) off-lock save ( ) bincode ( ) WAL ( ) bench shows no stall.

---

### Parallel-B — Base-map sovereignty  *(launch blocker; off the egress KPI)*
- **Status:** NOT_STARTED · **Resource:** launch-gate · **Root cause:** §2.4.
- **Files:** `client.html:2396` (tile URL), R2/Worker infra from Phase 6.
- **Steps:** serve a **licensed/self-hosted** tile source (planetiler → MapLibre vector per H1/H3, or a
  commercial provider — MapTiler/Stadia/Protomaps) fronted by the **same R2 + Worker** Phase 6 stands up,
  with a unique User-Agent/Referer and immutable `max-age`. **Not** a transparent OSM proxy (one bannable
  egress IP — worse). Keep the existing 30s backoff + 512-tile LRU as interim.
- **Verification:** base map serves from your edge; OSM is no longer hit; survives a 300-client pan load.
- **Gate:** **no public launch on raw `tile.openstreetmap.org`.** **Done:** ( ) licensed source on R2/Worker
  ( ) client points at it ( ) launch-gate cleared.

---

### UX-upside track  *(near-free byproducts of Phase 6; harvest after Milestone A + Phase 6)*
See §10. These turn the cost rebuild into a growth lever (the H1/H4 virality the roadmap has wanted).

---

## 5. The budget ledger (the arrival-proof artifact — fill in Phase 0)
Carry **one number, KB/s per active viewer**, through the phases at **two operating points**. Final row
must satisfy the gate. (Values below are illustrative placeholders — replace with Phase-0 measurements.)

| Stage | day-1 sparse (KB/s/viewer) | late-season dense (KB/s/viewer) | mechanism |
|---|---|---|---|
| Baseline (measure) | ~960 | ~6,400 | current |
| harness anchor (admin un-fogged, k=6) baseline | ~315 | _n/a_ | worst-case upper bound |
| harness anchor **after P3A** (admin un-fogged, k=6) | **~125 (2.5×)** | _n/a_ | binary control + packed/deflated ants @50Hz + fog-delta + me-split; **admin ⇒ fog-delta understated, real fogged < 125** |
| harness anchor **after P3B** (admin un-fogged, k=6) | **~76 (4.2× vs baseline)** | _n/a_ | ant rate 50→15 Hz (125→76); + visible-ant cap (dormant ≤4000). Grid-aware interp keeps crawl; real fogged < 76 |
| after P4 (per-conn cap, activity-aware) | _fill_ | _fill_ | spike ceiling |
| after P5 (palette + leaderboard diffs) | _fill_ | _fill_ | O(players)→~0 |
| after P6 (R2 bulk offload) | _fill_ | _fill_ | world-size→R2 |
| **GATE** | **≤ ~0.65 KB/s** | **≤ ~0.65 KB/s** | `value × 300 × 2.592e6 ≤ 500 GB/mo` |

**Measured anchor (P1 harness, RELEASE, 2026-06-02):** **~315 KB/s per viewer** at a single un-fogged
800×600 viewport over a 160-ant swarm (k=6) — i.e. ~485× over the ~0.65 KB/s gate for this *worst-case
upper-bound* operating point (admin/no-fog, modest swarm). Real fogged players sit lower; a *dense*
season-end viewport sits higher. This confirms the plan's thesis — **no single phase closes the gap**:
compression (P3) + fog-delta + per-conn cap (P4) + leaderboard/palette diffs (P5) must stack, then R2
(P6) removes the canvas-coupled ramp. To fill the day-1/dense rows precisely, raise the harness NPC
`spawns` and run RELEASE (debug throttles cycle rate and suppresses per-viewer egress).

The harness flat-line proves *decoupling*; **this ledger proves *arrival***. If the final row doesn't
clear the gate, name the lever that closes it (lower tile-Hz, tighter ant budget, smaller served viewport).

---

## 6. Gameplay-preservation track (the "don't ship a worse game" gate)
Each optimization carries a guarantee + a browser acceptance test on :8090. **A feel FAIL blocks the
phase regardless of egress numbers.**

| Optimization | Risk | Guarantee | Browser acceptance test |
|---|---|---|---|
| Ant cadence 50→12–20 Hz (P3) | Naive chord-lerp corner-cuts 90°-turning ants → swarm slides/snaps | **Path-aware dead-reckoning** interpolation (frame already carries `dx,dy`, `network.rs:416`); cadence a **runtime slider**, A/B before locking | Place 10 ants in a cluster — they **crawl**, not slide |
| No optimistic placement (P3/P4) | Placed ant only appears next frame → felt latency in battles | **Optimistic ghost ant** on place-ant (echo ant id in the ack to reconcile) | Place an ant — visible within ~1 frame regardless of cap |
| Visible-ant cap (P3) | Flat N=400 thins/flickers the swarm | **Zoom/pixel-tied budget** + spatial stride + hysteresis | Pan a 100+-ant, 3+-owner front — reads dense, no flicker |
| Compress priority text (P3) | Fatal: binary control silently dropped; async inflate adds ack latency | Unified kind-byte router; **only large text compressed**; events stay sync raw JSON | **Kill event arrives at a binary-control client**; killfeed instant |
| Trim `me` (P3) | WORKERS panel / HUD go stale | **Static→logged-in once; dynamic `me` keeps `ants[]`/army/credits/refill/hp/xp @≥1 Hz** | WORKERS lifespan bars tick; army/cap/refill track placements |
| Per-conn cap (P4) | Throttles the engaged players in marquee battles | **Activity-aware**, sized to PASS a battle; pathology-only; ant cadence last with a ~5–8 Hz floor | A client fighting a 100-ant/5-owner view is **not** down-shifted |
| Coalesce text (P4) | One-shots (welcome-back/death) coalesced away | **NEVER-DROP taxonomy**; coalescer per-kind on coalesceable set only | Reconnect after 60s → welcome-back; die in a busy tick → death screen |
| R2 under deltas (P6) | Stale base seams/flashes the live frontier | **Authoritative-viewport invariant**; deltas only on WS keyframes; double-buffer epoch | Pan a fast front across a seam + epoch flip → no seam/flash |

---

## 7. Holistic-scale additions (the walls v1 ignored) — summary
1. **CPU/smoothness** → Phase 2 (dedicated rayon pool) + CPU metrics + connection-count harness axis.
2. **Persistence stall** → Parallel-A (off-lock save + bincode + WAL).
3. **Base-map blocker** → Parallel-B (licensed tiles via R2/Worker).
4. **Per-conn memory** → cap/document the `PrevGrid` ceiling (MAX_DIM²); post-P6 store only the changed-cell
   working set for snapshot-covered viewers; add per-conn MB to the harness.
5. **R2 Class-A writes** → Phase-0 dirty-chunk projection; gate Phase 6 on < ~800k ops/mo.

---

## 8. (reserved — merged into §6/§7)

## 9. (reserved — merged into §5)

## 10. UX-upside track — turn the rebuild into a growth lever
All are **near-free byproducts of Phase 6's immutable, geography-keyed R2 tiles** (ranked leverage/effort).
Keep the **neon/CRT identity** in all shared artifacts (it's the brand).

| Feature | Roadmap | Effort | How (reuses Phase 6) |
|---|---|---|---|
| **Shareable territory PNG** | H1 viral hook | Low | Composite snapshot tiles around a queen + stats overlay → one R2 GET + a client share button |
| **`?spectate=queenid` link** | H1 | Low–med | Static R2 page: snapshot centered on the queen + read-only WS live edge (existing viewport frames) |
| **Live world minimap** | H2 | Low | It *is* the lowest-LOD snapshot tile the P6 pyramid produces — render in a corner widget; aids spawn UX |
| **Daily time-lapse bot** | H4 | Med | Retain N snapshot epochs on R2 (cheap immutable storage), stitch server-side, auto-post |
| **Instant-load reconnect** | (CP4) | Low | Sequence R2 snapshot paint → WS keyframe → welcome-back (Phase 7 ordering); pieces already exist |

> **One format decision buys all of this + the renderer:** emit Phase-6 tiles as **PNG/WebP RGBA**, so the
> same tiles are MapLibre raster textures + WebGL2 quads when H1's renderer lands, *and* the share/minimap
> assets, *and* the snapshot base — no rework.

---

## 11. Out of scope (protect shippability — these stay OUT, by design)
- **WebGL2 / MapLibre client rewrite (H1)** — orthogonal; gating egress phases on a non-headless-testable
  renderer rewrite would destroy shippability. **But pick PNG/WebP snapshot format now** so it's forward-
  compatible (snapshot tiles + binary deltas feed Canvas2D today and WebGL textures tomorrow equally).
- **CBOR-the-format** — deflate-JSON is enough now; CBOR is a later micro-opt.
- **Alliance / faction / seasonal-NFT** (alliance is a stub, `config.rs` `PRICE_ALLIANCE`).
- **ScyllaDB / Postgres / Go multi-host sharding** (H3 multi-host — after traction).
- **Ant-tracking / cinema cam** (already de-scoped per the interface spec).

Staying narrow now is what **buys** the roadmap later, cheaply — the snapshot+delta architecture is
forward-compatible with every one of these.

---

## 12. Risks / open questions (resolve before the dependent phase)
1. **Budget ledger arrival (P0/P5/P6):** the chain *as v1-written* lands ~$200–1,000/mo at 300, not $50
   (adversarial post-P6 floor ~330× over). The ledger + fog-delta + leaderboard-diff + tighter ant budget
   must close it; if not, lower tile-Hz or served-viewport. **The day-1 floor is the binding constraint.**
2. **R2 Class-A writes (P6):** project from the Phase-0 dirty-chunk counter; gate < ~800k/mo.
3. **Snapshot format (P6):** PNG/WebP RGBA (recommended) vs packed bitmap — confirm decode cost on the
   Canvas2D client is acceptable; it's the format that also serves H1/UX.
4. **Cadence floors (P3/P4):** ant 12–20 Hz + ~5–8 Hz hard floor — A/B on :8090 with the owner; **do not
   go aggressive without sign-off.**
5. **permessage-deflate:** re-check whether tokio-tungstenite landed it; if so, WS-layer compression may
   beat per-kind app-deflate (as of this session: not available).
6. **Host trade (after P6):** once R2 zeroes bulk egress, the decision is compute-$/core + included
   traffic + CPU headroom — quantify Railway vs Hetzner CPX31/41 (ROADMAP H2) against the 300-conn CPU
   profile. Don't rebuild the host blindly; measure it.
7. **Live server:** owner runs the live game on **:8080 — do not kill it**; all load testing on
   `PORT=8090 CARGO_TARGET_DIR=target-dev`. No browser automation here → owner validates visuals.

---

## 13. What changed from v1 (corrections applied)
- Ramp **12–20× → 6.7×** monthly (entropy ramp quarantined); **day-1 floor named as the binding constraint**.
- Target stated as a **per-viewer KB/s ceiling at a named concurrency**, not a dimensionless 10,000×.
- Added the **budget ledger** (arrival proof) and **widened the invariant** to egress + CPU + memory.
- Added **Phase 2 rayon isolation** and **Parallel-A off-lock persistence** (the two non-egress walls).
- Fixed the **fatal text-compression framing bug** (unified kind-byte namespace + binary router +
  `mpsc<Message>` priority channel + `world.rs` send sites added to the file list).
- **Fog** demoted to a ~10-line keyframe-only cleanup (it deflates ~100:1; ~25 KB/s, not hundreds).
- **Leaderboard/stats send-on-change** added (the real O(connections) residual, ~$518/mo after compression).
- **Phase 6 keying/dirty-tracking/authoritative-viewport** pinned (was unbuildable); **PNG/WebP** chosen.
- **Phase 0 counts at send sites**, not enqueue; added receive-side harness tally + connection-count axis.
- Added the **gameplay-preservation gate**, the **UX-upside track**, **Milestone A**, and **Parallel-B**
  (base-map blocker).

---

## 14. Summary
- **File:** `sim/docs/egress/EGRESS_PLAN.md` (this v2 rebuild plan).
- **Order:** P0 ledger → P1 harness → **P2 rayon isolation** → P3 compress+fog+framing+me → P4 cap+taxonomy
  → P5 palette+leaderboard diffs → **★ Milestone A (ship; ~$10k→$200–1,400/mo, days, no new infra)** →
  P6 R2 snapshot tiles (world-size; H3) → P7 hygiene; **∥A** off-lock persistence, **∥B** base-map, **UX**
  upside — both parallel/after.
- **Highest-leverage move:** the **budget ledger + rayon isolation in a widened per-connection resource
  invariant**, proving $50/mo at a fixed operating point **before** R2.
- **The honest promise:** ship this as the elevated rebuild and you get the **bill killed, the scale
  completed, and the virality engine the roadmap has wanted since H1** — not a cheaper, laggier game.
