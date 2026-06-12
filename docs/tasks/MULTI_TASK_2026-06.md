# Antarchy Multi-Task Work Order — June 2026

Tracking file for the 5-group update batch (Groups A–E). Resumable across sessions.
Plan-first and approval-gated: **each group gets its own plan, approved before any code.**
One group = one commit batch.

## Operating contract

- Server-authoritative for anything that affects player state (ants, daily claims, nectar/gem
  balances, queen zones). Never trust the client; assume a hostile client.
- Daily claim / ad-gate / nectar accrual must be **idempotent** — replay or double-call cannot
  double-grant.
- **Server clock is truth** for every timer (daily reset, ant expiry). Client countdowns are
  display only.
- Queen level stays the sole authoritative stat.
- Keep diffs minimal and reversible; narrow fixes over rewrites unless the rewrite is the root cause.
- Stack note: everything (landing + client + /api) is served by the Rust binary `hive-sim`;
  both HTML pages are `include_str!`'d → **client edits require a recompile**. (The work order's
  mention of a Node/Express landing is stale — the Node prototype was retired.)

## Decisions (answered by Todd)

- **D1 — Daily reset:** fixed wall-clock **00:00 UTC**. Unclaimed window = forfeited
  (no carry-over, no stacking).
- **D2 — Fog island anchor:** "ANCHOR ZOOM TO QUEEN REGION UNTIL THEY RECONNECT" — zoom-out cap
  anchors to the territory island containing the queen until the islands physically reconnect
  (merge); other islands reachable by panning only, never by auto-fit.
- **D3 — Economy:** "NECTAR REPLACES THE CREDIT SYSTEM, IT BUYS POWERUPS. ADD GEMS TOO, THE START
  OF OUR COSMETICS LINE. FIND 2 NEW SPOTS FOR NECTAR AND GEMS ON THE UI. BUILD GEMS TOO, AND BUILD
  A NEW SHOP TAB. POWERUPS & COSMETICS." Shop tabs = POWERUPS / COSMETICS, **cosmetics is the
  default tab** (even before functional); cosmetics start = tile/queen color & effects. Icons:
  nectar = orange drop, gems = green gem. Passive nectar ~1/day per 100k metro tiles is a tunable.
- **A1 scope:** light default on **/play only**; landing stays dark-first. Saved prefs respected.
- **A2 zone growth:** overlap enforced **at placement/relocation time only** with current radii;
  later level-growth overlap is allowed (no retroactive eviction).

## Cross-cutting layout constraint

The /play bottom ad banner (C2), the rewarded-ad window (C3), and the events-log popup under the
center queen (E3) must not collide — settle z-index/anchor strategy in the Group C and E plans.

## Status

| # | Task | Status |
|---|------|--------|
| 0 | This tracking file | ✅ done |
| A1 | /play defaults to light mode | ✅ code done 2026-06-12 — browser visual pass pending |
| A2 | No ant placement in enemy queen zone; zones never intersect (server-gated + client UX) | ✅ code done 2026-06-12 — two-tab playtest pending |
| A3 | Zoom cap anchors to queen's island on split territory (zoomBounds) | ✅ code done 2026-06-12 — relocate playtest pending |
| B1 | Stop daily-ant compounding (exactly 1× portion per window) | ⏸ awaiting plan approval |
| B2 | Claim button + server-idempotent claim per 00:00-UTC window | ⏸ awaiting plan approval |
| C1 | Fix landing-page ads | ⏸ awaiting plan approval |
| C2 | /play bottom AdSense banner | ⏸ awaiting plan approval |
| C3 | Rewarded ~30s ad gate before daily claim (server grant only) | ⏸ awaiting plan approval |
| C4 | Shop rework: gems + nectar UI spots/icons, POWERUPS/COSMETICS tabs (cosmetics default) | ⏸ awaiting plan approval |
| D1-LP | Livecam focus offset to ~75% x / 50% y (off hero text) | ⏸ awaiting plan approval |
| D2-LP | Livecam shows changing tiles, not just ants | ⏸ awaiting plan approval |
| D3-LP | Privacy Policy + ToS draft pages + footer links | ⏸ awaiting plan approval |
| E1 | Credits → Nectar rework + passive metro-region accrual (server-side) | ⏸ awaiting plan approval |
| E2 | Help/tutorial page (numbers pulled from live config) | ⏸ awaiting plan approval |
| E3 | Persistent ~20-entry events log popup under center queen (feature-flagged) | ⏸ awaiting plan approval |

Sequence: A → B → C (needs B) → D (independent) → E.

## Group A plan (approved 2026-06-12)

Full plan: `~/.claude/plans/antarchy-multi-task-modular-sky.md`. Summary:
- **A1:** `public/client.html:2` `data-theme="dark"` → `"light"` (inline pref script unchanged).
- **A2:** `validate_worker_placement` gains `too_close_to_queen` enemy-zone rejection (covers
  place-ant + brute); new `World::queen_zone_overlaps(x,y,my_r,exclude)` sum-of-radii check at
  place-queen (`cfg.bubble_r`) + relocate (current `q.bubble_r`); `get-forbidden-zones` adds
  `placeR`; client inflates zones by `placeR` for queen placement, fetches/renders zones + blocks
  clicks during ant/brute placement.
- **A3:** TileMap gains `#[serde(skip)] owner_chunks` (owner → painted chunk keys, expand-only,
  rebuilt in `rebuild_bounds`) + `queen_island_bounds` (chunk-grid BFS from queen's chunk);
  `build_player_info` emits `zoomBounds`; client zoom-out cap uses zoomBounds, pan leash keeps
  full ownerBounds.
