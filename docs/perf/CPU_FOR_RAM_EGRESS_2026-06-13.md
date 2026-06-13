# Trading idle CPU for RAM + egress (2026-06-13)

**Problem (live VPS):** CPU < 5% at all times, RAM > 70% and climbing, outgoing (egress) traffic
ramping up the longer the server runs, and in-game **territory/tile updates feel very slow**.

## Root-cause diagnosis

The three symptoms share one cause and are **not** a tradeoff against each other.

1. **CPU idle is structural.** At the live **15 Hz** tick with a modest ant/player count, `tick_world`
   is trivial and `finish_view` serialization is cheap relative to the wire. The box is **network-
   bound, not compute-bound**.

2. **Egress ramps because authed `/play` players render their entire visible territory from the live
   WS tile stream, not R2.** The client only draws R2 super-tiles `if (!vw)` (cold-load splash,
   `client.html` `drawSnapshotTiles`); once the first WS frame arrives, the whole viewport is painted
   from WS keyframe/delta frames. R2's $0-egress path (why Cloudflare looks fine) is bypassed for
   everyone actually playing. As the map fills (no season wipe + 24 h ants) each frame encodes more
   owners + more changed cells → per-player egress climbs over time.

3. **Cadence-formula degeneration.** `server.rs` computed `tile_every = (tick_rate / 10).max(1)`.
   Written for the old 50 Hz tick (→ 5 ticks = 10 Hz); at **15 Hz it evaluates to 1** → a full tile
   frame **every tick (15 Hz)**, ~3× more than intended.

4. **Slow tiles are *caused by* the egress bloat.** Viewport frames ride a latest-wins `watch` slot
   that is the **lowest** priority in the write task's `biased select!`. Big frames make each socket
   send `.await` longer than the 66 ms tick, so the slot coalesces and effective territory refresh
   drops to seconds. Worse: a dropped delta makes the client **freeze its territory until the next
   keyframe** (`baseSeq != clientGrid.seq` → `return`, client.html ~L2001). So **shrinking frames
   makes tiles arrive faster**, not slower.

5. **RAM** = persistent painted territory + 24 h ant trails (`lifespan 1_296_000` ÷ 15 Hz = 24 h,
   season wipe off) **plus glibc holding freed pages** rather than returning them to the OS.

## Changes shipped (this commit) — all "spend idle CPU / accept mild staleness, save bytes + RAM"

| # | File | Change | Effect |
|---|------|--------|--------|
| 1 | `network.rs` `deflate_raw` | `Compression::default()` (6) → `best()` (9) | ~15–30% smaller on **every** WS viewport/ant/control frame; parallel per-client, uses spare CPU |
| 2 | `config.rs` `tile_hz()` + `server.rs` | new `HIVE_TILE_HZ` env knob (default **5**, clamp 1–30); cadence `tr / tile_hz` replaces `tr / 10` | tile frames 15 Hz → 5 Hz (~3× egress cut) and **un-coalesces** delivery → snappier tiles |
| 3 | `network.rs` `kf_interval()` | keyframe interval now **wall-clock-stable** `≈1.5×tile_hz` (floor 8) instead of fixed 15 frames | drop-recovery latency stays ~1.5 s regardless of `tile_hz`; fewer keyframes than today |
| 4 | `Cargo.toml` + `main.rs` | **jemalloc** global allocator, `cfg(unix)` only | returns freed pages to OS via decay → flattens RSS; Windows/MSVC dev build keeps system alloc |
| 5 | `simulation.rs` `COMPACT_INTERVAL` | 1500 → **500** ticks | Dense→Uniform sweep ~3× more often (cheap on idle sim thread) → lower steady-state tile RAM |

**Verified:** `cargo check`, `cargo test` (61 pass), `cargo clippy` clean — all on `CARGO_TARGET_DIR=target-dev`.
NOT yet: a Linux build (jemalloc path is `cfg(unix)`, can't compile on the Windows dev box) and a
live browser confirmation that tiles feel faster.

## Deploy (VPS — branch beta-v1, `/home/ubuntu/antarchy`, `antarchy.service`)

1. Pull + **rebuild** (`cargo build --release`) — jemalloc + the cadence change need the binary, not
   just env. First Linux build will compile `tikv-jemalloc-sys` (a few min).
2. Tunables (optional, in `/etc/antarchy.env`): `HIVE_TILE_HZ=5` (lower→less egress; raise to 7–10 if
   you want crisper territory and have headroom). For more aggressive page return:
   `MALLOC_CONF=dirty_decay_ms:1000,muzzy_decay_ms:0`.
3. `systemctl restart antarchy` → confirm with the diagnostics below.

## Diagnostics (run on the VPS; quantify before/after)

- `curl localhost:8090/health` → `tileMB`, `denseChunks`, `ants`, `tickMsP99`. **RAM split:** compare
  `tileMB` to process RSS — the gap is allocator retention (jemalloc should shrink it).
- `curl localhost:8090/egress-stats` → egress by frame kind; tile frames should drop sharply.

## Not done (deliberately out of scope — needs a decision)

- **Structural egress cure:** composite R2 super-tiles for the *far/static* part of an authed view and
  WS-delta only the near frontier (mirror the guest path for logged-in players). Biggest cut; real
  rebuild + live testing.
- **Gameplay RAM levers:** shorten the 24 h ant `lifespan`, lower `army_cap`, or re-enable periodic
  decay/season wipe. Largest RAM win but a balance decision. (Manual remedy today: admin WIPE +
  `systemctl restart` — see `vps-ram-growth` memory.)
- **`ant_hz`** (default 15) could drop to ~10 for a further (smaller) egress cut; ants are interpolated
  client-side so motion stays smooth.
