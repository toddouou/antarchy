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
| B1 | Stop daily-ant compounding (exactly 1× portion per window) | ✅ code done 2026-06-12 — auto-refill loop deleted; claim is the only grant path |
| B2 | Claim button + server-idempotent claim per 00:00-UTC window | ✅ code done 2026-06-12 — WS E2E verified (grant once, replays rejected, persisted) |
| C1 | Fix landing-page ads | ✅ code done 2026-06-13 — best-practice cleanup (responsive `auto`, one push/slot, CLS-safe) |
| C2 | /play bottom AdSense banner | ✅ code done 2026-06-13 — `#play-adbar` in canvas cell, click-through while placing; real slot id TODO |
| C3 | Rewarded ~30s ad gate before daily claim (server grant only) | ✂ DROPPED 2026-06-13 — user: no ad gating the daily claim; `claim-daily` unchanged |
| C4 | Shop rework: gems + nectar UI spots/icons, POWERUPS/COSMETICS tabs (cosmetics default) | ✅ code done 2026-06-13 — header nectar+gems chips, CREDITS→NECTAR relabel, COSMETICS(default)/POWERUPS tabs |
| D1-LP | Livecam focus offset to ~75% x / 50% y (off hero text) | ✅ code done 2026-06-13 — render/drawSnap/setViewport anchored at `(W*focusFX(),H*0.5)`, responsive (centre on phones) |
| D2-LP | Livecam shows changing tiles, not just ants | ✅ code done 2026-06-13 — bounded `trail` painted from the ant feed over the R2 snapshot (zero egress), reset per shot |
| D3-LP | Privacy Policy + ToS draft pages + footer links | ✅ code done 2026-06-13 — `public/privacy.html`+`public/tos.html` at `/privacy` `/tos`, footer links, contact support@antarchy.fun, governing-law bracketed |
| E1 | Credits → Nectar rework + passive metro-region accrual (server-side) | ✅ code done 2026-06-13 — full `credits`→`nectar` rename (snapshot-safe) + daily UTC accrual (`nectar_per_100k_day`, leaders-only, unit-tested) |
| E2 | Help/tutorial page (numbers pulled from live config) | ✅ code done 2026-06-13 — in-game HELP window from live `me.cfg`/`GATES`/`SHOP_ITEMS` |
| E3 | ~20-entry timestamped events log | ✅ code done 2026-06-13 — EVENTS button → window (reuses `openWindow`); ring buffer fed from `event` msgs (clarified: button+window, not an under-queen popup) |

Sequence: A → B → C (needs B) → D (independent) → E.

## Group C plan (approved + implemented 2026-06-13)

Full plan: `~/.claude/plans/continue-the-development-we-cuddly-planet.md`. Scoped down at clarification:
**C3 DROPPED** (no ad on the daily claim — `claim-daily` left exactly as Group B shipped); **C1** is a
best-practice cleanup (not a render bug). Build clean, 61 tests pass, clippy clean, WS E2E confirms
`me.gems` on the wire. Verified via `:8090` hermetic smoke + curl/grep of the served pages.

- **C1** (`public/landing.html`): the two ad units dropped the flaky fixed `width:100%;height:90px`
  + `horizontal` combo for `data-ad-format="auto"` responsive; `.adslot { min-height:90px }` reserves
  space (CLS-safe); init now does **one `push({})` per `ins.adsbygoogle`** (was an eager double-push).
  `/ads.txt` was already served (`server.rs` `ads_txt_handler`) — untouched.
- **C2** (`public/client.html`): AdSense loader added to `<head>` (CSP already whitelists it,
  `server.rs:1104`). `#play-adbar` is `position:absolute` **inside `.canvas-area`** (never overlaps the
  sidebar/footer), z-index 40 (below killfeed 60 / mobile-nav 70 / windows 500 / modals 880-910; the
  future **E3 popup takes a higher centered lane** — no collision with this bottom strip). Mobile: rides
  above `#mobile-nav` (`bottom: calc(60px+safe-area)`). `updateAdbarVis()` (called from `updateHUD`)
  shows it only in-game and pushes the slot once on first visibility; `#map.placing ~ #play-adbar ins`
  is click-through so it never eats a placement click. **TODO: real `/play` ad-slot id (placeholder
  `0000000000`).**
- **C4** (`public/client.html` + `src/auth.rs` + `src/network.rs`): **gems** = new `UserRecord` field
  (`#[serde(default)] pub gems: u64`, users.json, wipe-proof like `peak_level`/`last_claim_day` — NOT
  in the bincode Player snapshot); emitted in `me` (network.rs, sourced from the auth record). Client:
  two header chips (NECTAR orange-drop = `me.credits`, GEMS green-gem = `me.gems`); all player-facing
  CREDITS→**NECTAR** (stat grid, death screen + prose, queen inspect, shop top, brute-deploy label,
  "Not enough nectar" err); shop window now has **COSMETICS (default) / POWERUPS** tabs (`_shopTab`,
  `shopTab()`) — powerups = unchanged `SHOP_ITEMS` priced in NECTAR (spends `credits`), cosmetics =
  disabled `COSMETIC_ITEMS` placeholders priced in GEMS. **Backend credits→nectar rename + accrual +
  gem earning + functional cosmetics stay in Group E.** Admin-panel "CR" labels left as-is (internal).

## Group E plan (approved + implemented 2026-06-13) — A–E WORK ORDER COMPLETE

Full plan: `~/.claude/plans/continue-the-development-we-cuddly-planet.md`. Build clean, 62 tests pass
(incl. new `metro_nectar_accrues_once_per_utc_day`), clippy clean; `:8090` WS E2E confirms `me` carries
`nectar`/`unlimitedNectar` and **no** `credits`. Browser pass pending (Help numbers, Events window).

- **E1a — full rename** `credits`→`nectar`: `Player.nectar`/`unlimited_nectar` (world.rs), the
  `PlayerSnapshot` durable struct + both `From` impls + round-trip test (persist.rs — **bincode is
  positional so existing snapshots still load**), `charge`/shop/admin (handlers.rs; WS message types
  renamed `admin-give-nectar`/`admin-set-nectar`/`unlimited-nectar`), kill/starter grants +
  death-screen `"nectar"` key (simulation.rs), `me` keys `"nectar"`/`"unlimitedNectar"` (network.rs),
  and the client (`me.nectar`, `me.unlimitedNectar`, admin calls/labels). DOM ids `s-credits`/
  `ds-credits` + the internal `creditsRow` var kept (selectors, not user-facing).
- **E1b — passive accrual**: `config.nectar_per_100k_day` (default 1.0; clamp/slider/`me.cfg`),
  `UserRecord.last_accrual_day` (users.json, wipe-proof), `simulation::accrue_metro_nectar` (sum
  `metro_holders` tiles per leader → `round(tiles/100k × rate)` nectar, idempotent per UTC day, fires
  a `+N NECTAR FROM YOUR REGIONS` event). sim_loop runs it once per UTC-day change (loop-local gate +
  per-account guard; waits for `metro_holders` to populate so a fresh boot doesn't burn the day).
- **E2 — Help window**: header HELP `hbtn` (+ `.mobile-only` drawer button); `openHelpWindow`/
  `renderHelpWindow` build a field manual from live `me.cfg` + `GATES`/`SHOP_ITEMS`.
- **E3 — Events window**: header EVENTS `hbtn` (+ drawer button); `eventLog` ring buffer (cap 30)
  fed from the `event` handler, rendered newest-first with `[HH:MM:SS]` stamps via `openWindow`.
  (User clarified E3 = button + window, not a popup under the queen; no dev flag.)

## Group D plan (approved + implemented 2026-06-13)

Full plan: `~/.claude/plans/continue-the-development-we-cuddly-planet.md`. Build clean, 61 tests pass,
clippy clean; `:8090` smoke confirms `/privacy` + `/tos` = 200 with all sections + footer links + the
new livecam markers. **Browser visual pass still pending** (D1/D2 need a populated world to film — the
livecam shows nothing on a fresh `:8090`).

- **D1** (`public/landing.html`): the spectator projection mapped world→screen at `(W/2,H/2)`; now it
  anchors at `(W*focusFX(), H*FOCUS_FY)` where `focusFX()=innerWidth<760?0.5:0.75` and `FOCUS_FY=0.5`
  (action right of the hero text on desktop, centred on phones). Applied in `render` (queens+ants),
  `drawSnap` (cull rect + draw), `setViewport` (AoI bounds), and the new `drawTrail`. Director unchanged.
- **D2** (`public/landing.html`): bounded `trail` Map (`"x_y"→{x,y,owner}`, `TRAIL_CAP=20000`,
  oldest-evicted) recorded from each ant in `applyAnts`, drawn in full territory colour by `drawTrail`
  (after `drawSnap`, before queens/ants) so tiles visibly grow in real time. **Zero new egress** — uses
  only the capped ant feed guests already get (guests still never receive tile frames). Reset via
  `trail.clear()` in `applyShot` so paint doesn't carry across a cut.
- **D3**: new `public/privacy.html` + `public/tos.html` (self-contained, dark-default theme via
  `antarchy-theme`, AdSense-ready draft), served at `/privacy` + `/tos` (`server.rs`: `PRIVACY_HTML`/
  `TOS_HTML` `include_str!` + `privacy_page`/`tos_page` handlers + routes near `/reset`). Footer
  (`landing.html`) gains Privacy/Terms links. Contact = `support@antarchy.fun`; governing law +
  responsible entity left as bracketed `[…]` placeholders for the user to fill before launch.

## Group B plan (approved + implemented 2026-06-12)

- Root cause: rolling-24h auto-grant in sim_loop (`ants_avail += daily`) — deleted. The ONLY
  grant path is the `claim-daily` WS handler (`claim_daily` helper in handlers.rs), idempotent
  via `UserRecord.last_claim_day` (UTC day number, users.json `#[serde(default)]` — NOT the
  bincode snapshot). Windows = `config::utc_day` / `next_utc_midnight_ms` (00:00 UTC).
- `me` sends `nextResetMs` + `claimReady` (replaced `nextRefillMs`); workers-panel DAILY cell
  renders a pulsing CLAIM button when claimable, else the countdown; `daily-claimed` msg applies
  the authoritative count. Registration marks day-1 claimed (starter = day-1 portion); existing
  accounts get one catch-up claim. `Player.next_refill` kept as a legacy field (snapshot layout).
- C3 hook: the rewarded-ad gate becomes a precondition inside the same `claim-daily` handler.

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
