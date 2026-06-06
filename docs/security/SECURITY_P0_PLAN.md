# Antarchy.fun — Security & Anti-Cheat Hardening — P0 Tier (RESUMABLE CHECKPOINT)

> **STATUS: IN PROGRESS on branch `security-p0` (off `beta-v1`). §4a+§1+§2+§3+§4b+§5 DONE.**
> **Resume at:** §0 egress/R2 telemetry, then deliverables. See "Execution order" / "Next action".
>
> Done so far:
> - baseline commit (beta-v2 state) · `bbe539f`
> - **§4a Argon2id** · `0fedcb3` — `argon2`+`subtle` deps; `hash_pw_argon2`/`verify_pw`/`needs_rehash`
>   + `HIVE_ARGON2_*` knobs; transparent rehash-on-login in `api.rs`/`handlers.rs`; admin seed uses
>   argon2; 3 new auth tests.
> - **§1 AoI zoom-leak** · `6b76460` — `config::clamp_view_span` + `max_view_span`
>   (HIVE_MAX_VIEW_SPAN=4000) + `max_queens_per_frame` (256); clamp in `view-set` (`handlers.rs`) +
>   defensively in `snapshot_view`; `cap_queens_to` (reveal-first, nearest-to-centre) in `network.rs`;
>   5 new tests.
> - **§2 anti-replay** — `World.last_seq` map (transient; reset on (re)connect in
>   `create_or_reconnect_player`, cleared on disconnect in `server.rs`); `check_seq` guard on
>   `place-queen`/`place-ant`/`shop-buy`; client emits monotonic `seq` in `send()`; `admin-give-xp`
>   rejects non-finite XP; 1 new test.
> - **§3 WebSocket hardening** — (3a) `WebSocketUpgrade::max_message_size`/`max_frame_size`
>   (`HIVE_WS_MAX_MSG`=64KiB), `Origin` allowlist (`config::allowed_origins`, `HIVE_ALLOWED_ORIGINS`)
>   → 403 on mismatch via `upgrade_guarded`, keepalive `Ping` + idle close (`HIVE_WS_IDLE_SECS`=60);
>   (3b) bounded backpressure: `world::BoundedTx<T>` newtype over a bounded `mpsc::Sender`
>   (`HIVE_WS_SEND_QUEUE`=1024), drop-on-full — `Player.tx`/`ctl_tx` + the per-conn channels switched
>   over with every `tx.send(...)` call site unchanged. **Runtime-verified on :8090**: evil Origin→403,
>   allowed/no-Origin→101, pages→200, admin/admin login OK under Argon2id.
> - **§4b cookie sessions + CSRF + throttle + generic errors** — `__Host-`/`antarchy_session`
>   HttpOnly+SameSite=Lax cookie (`config::secure_cookies`/`session_cookie_name`); login/verify set it,
>   token no longer in the JSON body; WS auths from the cookie via the new `enter` message
>   (`session_uid_from_cookie` at upgrade); `/api/logout` revokes + clears; CSRF `Origin` guard on all
>   `/api/*` (shared `server::origin_allowed`); per-account login throttle (`World.login_attempts`) +
>   `verify-email/phone` rate-limited; enumeration-safe email-taken (`RegExisting` → notify existing
>   owner); username/color charset (`config::valid_hex_color`); client (`landing.html`+`client.html`)
>   drops all localStorage token/password storage. **Runtime-verified on :8090**: CSRF evil→403/good→200,
>   login Set-Cookie (no token in body), logout 200+clear+revoke, register→verify→cookie, enumeration-safe
>   re-register, color sanitized, per-IP 429.
> - Running total: builds on `target-dev`; **44 tests pass**. (Pre-existing clippy style lints in the
>   baseline remain — a `-D warnings` cleanup is P2 §12, out of scope for P0.)
> Mirror of the approved plan at `~/.claude/plans/antarchy-fun-security-eager-comet.md`, kept in-repo
> so any session can pick up without re-deriving context.

## Decisions locked (from the user)
1. **Sessions → `__Host-` HttpOnly cookies.** Drop the localStorage bearer token AND the legacy
   plaintext `hive-u`/`hive-p` localStorage + `{t:"login"}` client path. WS upgrade authenticates from
   the cookie.
2. **Spectator roster (`build_queen_roster`, exact queen x,y) left as-is** — no real third-party
   spectators in practice (only the site's own agent). Goes in residual-risk, not the fix list.
3. **P0-critical only (§0–§5), then PAUSE for review** before P1/P2.
4. Constraints unchanged: no new infra, no paid services; keep `admin/admin` working; don't change
   gameplay balance; wire-protocol changes additive + version-flagged.

## Context

`hive-sim` is the live planet-scale multiplayer Langton's-ant game behind antarchy.fun. The
login/landing layer was just rebuilt (untracked `src/api.rs`, `src/session.rs`, `src/email.rs`,
`src/sms.rs`, `public/landing.html`) and is unaudited. Hardening mapped to OWASP Top 10:2025.

### Audit findings (firsthand, with line refs)
- **Weak password hashing** — `auth::hash_pw` (`src/auth.rs:60`) = SHA-256 + constant `"hive-salt"`,
  no KDF/per-user salt; compared with `!=` (not constant-time) at `src/api.rs:172`,
  `src/handlers.rs:72`.
- **Session token in localStorage** — minted fine in `src/session.rs` (256-bit), but kept as a
  JS-readable bearer token; legacy client also stores **plaintext** `hive-u`/`hive-p`
  (`public/client.html` ~3865) and sends `{t:"login",...}`. XSS-exfiltratable.
- **AoI zoom leak (known exploit)** — `view-set` stored unclamped (`src/handlers.rs:176-183`);
  `snapshot_view` (`src/network.rs:300`) derives LOD `step` from that span and the fog
  distance-transform runs on the downsampled grid, so **fog-reveal radius scales with zoom-out** →
  distant enemy queens un-fogged + shipped. No server cap on span or queens/frame.
- **WS layer** — no `Origin` check, no frame/message-size cap (`src/server.rs:156`), **unbounded**
  per-conn `prio_tx`/`ctl_tx` mpsc (no backpressure), no ping/pong liveness, no anti-replay.
- **Stored XSS** — usernames/colors → `innerHTML` unescaped in leaderboard
  (`public/client.html` `lbSimpleRow`/`lbFullRow` ~2154-2197), region sub-header (~2212), workers
  (~3843), admin list (~3962). Only killfeed escapes (`kfEsc`, line 2478). Legacy WS register
  (`src/handlers.rs:27-53`) does **no** charset/color validation; REST path validates handle charset
  but **not** color (`src/api.rs:88,98`).
- **Enumeration** — `do_register` leaks "email already exists" (`src/api.rs:96`);
  `verify-email`/`verify-phone` POSTs **not** rate-limited (`src/server.rs:891-892`) — 6-digit code
  brute-forceable (only the 6-attempt/`reg_id` cap at `src/api.rs:124` mitigates).
- **No per-IP egress / R2 op telemetry** — `EgressMeter` + `metrics` (per-kind bytes) exist; no
  Class-A/B op counters, no per-IP / threshold-breach alerting (denial-of-wallet, §0).
- **Already solid** — every `admin-*` handler checks `is_admin` first; damage/XP/HP/territory
  server-computed (`src/simulation.rs`); no `.unwrap()` on network input; secrets env-only (no
  hardcoded keys). Absent: `.env`, `deny.toml`, `Caddyfile`, CI.

---

## Work items (P0, per-section commits on `security-p0`)

Build + clippy + tests after each section, against the **isolated** target (`target-dev`) so the
live :8080 and test :8090 servers are untouched.

### §4a — Argon2id foundation (do FIRST; other auth work builds on it) — OWASP A07
Files: `Cargo.toml`, `src/auth.rs`, `src/api.rs`, `src/handlers.rs`, `src/config.rs`.
- Pinned deps: `argon2 = "0.5"`, `password-hash = "0.5"`, `subtle = "2"` (pure-Rust, no system libs).
- `auth::hash_pw_argon2(pw) -> String` (Argon2id PHC string). Params via `config.rs` env:
  `HIVE_ARGON2_MEM_KIB` (default **19456**), `HIVE_ARGON2_TIME` (**2**), `HIVE_ARGON2_LANES` (**1**)
  — OWASP minimum; tunable up to m=64MiB/t=3. 16-byte salt, 32-byte output.
- `auth::verify_pw(stored, pw) -> bool`: `$argon2…` → argon2 verify (constant-time); else legacy
  SHA-256 → `subtle::ConstantTimeEq` compare vs `hash_pw(pw)`.
- `auth::needs_rehash(stored) -> bool` (legacy or weaker-param argon2).
- **Transparent migration**: swap the three `password_hash != hash_pw(...)` sites (`api.rs:172`,
  `handlers.rs:72`, and reset `api.rs:203` now writes argon2) to `verify_pw`; on successful login where
  `needs_rehash`, rehash + `world.auth.save()` (both paths hold `&mut World`). New registrations write
  argon2. `admin_record()` (`auth.rs:75`) seeds with `hash_pw_argon2(ADMIN_PASSWORD)` — `admin/admin`
  still logs in.

### §1 — AoI culling: close the zoom leak (headline) — OWASP A01
Files: `src/handlers.rs`, `src/network.rs`, `src/config.rs`.
- `config::max_view_span()` (env `HIVE_MAX_VIEW_SPAN`, default **4000** tiles),
  `config::max_queens_per_frame()` (default **256**).
- **Clamp `view-set` server-side** (`handlers.rs:176`): center-preserving clamp so `(x1-x0)`/`(y1-y0)`
  ≤ span, then clamp to world bounds, before storing `PlayerView`. Bounds LOD `step` → bounds
  fog-reveal world radius → distant queens can't be un-fogged by zoom-out.
- **Defense-in-depth in `snapshot_view`** (`network.rs:300`): re-clamp the stored view and cap the
  queens vector to `max_queens_per_frame` (nearest-to-center first). Existing fog gate on non-`reveal`
  queens (`finish_view` `network.rs:587-595`) stays.
- Zoomed-out territory still renders from free R2 super-tiles; only live ants/queens are AoI-bounded
  (no map-view UX regression). Leaderboard/region/`me` confirmed position-free; roster left as-is.

### §2 — Anti-replay + validation — OWASP A01/A06
Files: `src/world.rs` (Player), `src/handlers.rs`, `public/client.html`.
- `Player.last_seq: u64` (transient). Client emits monotonic `seq` on mutating msgs (`place-ant`,
  `place-queen`, `shop-buy`, `relocate`). In `handle_message`: `seq <= last_seq` → reject + log
  `[anticheat] replay/dup`; else advance. Absent seq (legacy) → process, counted.
- `admin-give-xp`: reject non-finite (`xp.is_finite()`) before use (`handlers.rs:503`). Keep all
  existing bounds checks + `validate_worker_placement`. No client time trusted anywhere (keep).

### §3 — WebSocket hardening — OWASP A02/A10
Files: `src/server.rs`, `src/config.rs`.
- Size caps: `WebSocketUpgrade::max_message_size`/`max_frame_size` (env `HIVE_WS_MAX_MSG`, default
  **64 KiB**). (axum doesn't negotiate inbound permessage-deflate → no bomb path today; document.)
- **Origin allowlist** on upgrade: read `Origin` in `root_handler`/`play_handler`, check
  `config::allowed_origins()` (env `HIVE_ALLOWED_ORIGINS`; default public base + `localhost:PORT`).
  Mismatch → 403.
- **Bounded send queues**: `prio_tx`/`ctl_tx_conn` → bounded `mpsc::channel(cap)` (env
  `HIVE_WS_SEND_QUEUE`, default **1024**); sim-thread `try_send`; `Full` on priority → disconnect.
  Viewport stays latest-wins `watch`. Update `world.rs` `send_to`/`broadcast` + `Player.tx` type.
- **Liveness**: write-task `interval` Ping; close if no inbound within `HIVE_WS_IDLE_SECS` (**60**).

### §4b — Cookie sessions + CSRF + throttle + generic errors — OWASP A07
Files: `src/api.rs`, `src/server.rs`, `src/session.rs`, `src/config.rs`, `src/world.rs`,
`public/landing.html`, `public/client.html`.
- `config::secure_cookies()` (env `HIVE_SECURE_COOKIES`, default off for http://localhost dev).
  `set_session_cookie(token)` → prod `Set-Cookie: __Host-antarchy_session=…; HttpOnly; Secure;
  SameSite=Lax; Path=/`; dev name `antarchy_session`, no `Secure`. `/api/login` + verify-finalize set
  it and **stop returning the token in JSON**. Add `/api/logout` (revoke + clear cookie). Session
  gets idle+absolute expiry.
- **WS auth from cookie**: `play_handler`/`root_handler` read cookie → `sessions.validate` → `uid`,
  pass into `handle_ws_connection`; auto-issue the existing `session-login` Cmd on connect. Keep
  `{t:"session",token}` one release as transitional fallback.
- **CSRF**: Origin/Referer allowlist check on all POST `/api/*` (+ covers cookie-authed `/api/logout`).
- **Throttle per-account**: transient `World.login_attempts: HashMap<String,(u64,u32)>` keyed by
  `ident`, backoff, checked in `do_login`. Also add `app.rate.check` to `verify-email`/`verify-phone`.
- **Generic errors**: `do_register` email-exists → return the normal success shape + email the existing
  address a "someone tried to sign up" notice (no enumeration). Handle-taken stays (handles public).
- **Username/color charset** (kills stored-XSS source): username `[A-Za-z0-9_-]{3,20}`; color `#rrggbb`
  or ∈ `HUES`. Apply in legacy WS `register` (`handlers.rs:27`) + harden REST color (`api.rs:98`).
- **Client**: stop writing `antarchy-session`/`hive-u`/`hive-p`; rely on cookie; logout → `/api/logout`;
  remove plaintext-password WS fallback (server `login` handler stays for test/bench/admin).

### §5 — Injection & XSS — OWASP A05
Files: `public/client.html`, §4 server validators, `src/server.rs`/Caddy.
- Shared `esc()` (rename `kfEsc`, client.html:2478) on every user field rendered via `innerHTML`
  (leaderboard names, region holder, workers, admin list). Validate colors as hex before `style=`.
- **CSP + headers** emitted from axum (dev parity; Caddy enforces in prod): strict `CSP` scoped to
  self + R2 snapshot base + OSM tile hosts + WS origin; `X-Content-Type-Options`,
  `X-Frame-Options: DENY`, `Referrer-Policy`. (Inline-script client needs `'unsafe-inline'` for now →
  residual; nonce/extraction is P1/P2.)
- No SQL (no DB). R2 keys are numeric — add a validator when forming keys (belt-and-suspenders).

### §0 — Egress & R2 op-budget telemetry — OWASP A09 (P0 telemetry slice)
Files: `src/metrics.rs`, `src/snapshot.rs`, `src/server.rs`.
- Global Class-A/Class-B op counters in the R2 sink (`snapshot.rs` put/delete/delete_prefix/list);
  document the hot-path-free invariant (writer is interval-driven; `roster` is `max-age=5` cached).
- Per-IP/per-connection egress aggregation + periodic threshold-breach log (env
  `HIVE_EGRESS_ALERT_KBPS`, `HIVE_CLASSA_ALERT_PER_MIN`); surface on `/egress-stats`. (Outbound
  permessage-deflate + CDN cache-config are P1 config/Caddy — documented, not built here.)

### Deliverables at end of P0
- `sim/SECURITY.md` (threat model + controls + how to tune every new env knob + manual follow-ups).
- `CHANGELOG-security.md` grouped by OWASP 2025 category.
- Residual-risk list (spectator roster x,y accepted; CSP `'unsafe-inline'`; legacy WS login retained
  for tests; P1/P2 pending).

---

## New env knobs introduced (document in SECURITY.md)
`HIVE_ARGON2_MEM_KIB`(19456) `HIVE_ARGON2_TIME`(2) `HIVE_ARGON2_LANES`(1) ·
`HIVE_MAX_VIEW_SPAN`(4000) `HIVE_MAX_QUEENS_PER_FRAME`(256) ·
`HIVE_WS_MAX_MSG`(65536) `HIVE_WS_SEND_QUEUE`(1024) `HIVE_WS_IDLE_SECS`(60) ·
`HIVE_ALLOWED_ORIGINS`(public base + localhost) `HIVE_SECURE_COOKIES`(off dev / **1 in prod**) ·
`HIVE_EGRESS_ALERT_KBPS` `HIVE_CLASSA_ALERT_PER_MIN`.

## Manual follow-ups (human, outside code — list in SECURITY.md)
Rotate the R2 key → bucket-scoped token + revoke old; Cloudflare orange-cloud + Authenticated Origin
Pulls; UFW → Cloudflare IP ranges only; SSH key-only + fail2ban + unattended-upgrades; systemd
sandboxing for `hive-sim`; set `HIVE_SECURE_COOKIES=1` in prod; replace `admin/admin` before launch.

---

## Verification
Isolated build/lint/test (never touch :8080):
```
cd sim
CARGO_TARGET_DIR=target-dev cargo build
CARGO_TARGET_DIR=target-dev cargo clippy -- -D warnings
CARGO_TARGET_DIR=target-dev cargo test
```
New tests: AoI (zero queens beyond span cap, ≤ max/frame, fog reveal flat vs step); anti-replay
(dup/out-of-order seq rejected); validation rejects (wrong-owner/bounds/cooldown/inventory);
auth (argon2 params; legacy login → transparent rehash; constant-time; `/api/logout` revokes; generic
login error; per-account+per-IP throttle trips; verify-email throttled); WS (oversized frame closed;
bad Origin 403; malformed JSON no panic; send-queue Full disconnects); XSS (markup username/color
rejected; client `esc()` on names).

Smoke (port 8090, never kill 8080):
```
CARGO_TARGET_DIR=target-dev PORT=8090 HIVE_SECURE_COOKIES=0 cargo run
```
Register+login on landing → cookie set, no token/password in localStorage → `/play` enters game;
`admin/admin` still logs in; gameplay unchanged; zoom fully out → territory renders but NO enemy
queens beyond the radius cap; `/egress-stats` shows new counters; synthetic spike fires the threshold
log.

---

## Execution order (commit boundaries)
1. ~~`security-p0` branch off `beta-v1`.~~ ✅
2. ~~§4a Argon2id foundation → build/clippy/test → commit.~~ ✅
3. ~~§1 AoI fix → tests → commit.~~ ✅
4. ~~§2 anti-replay → tests → commit.~~ ✅
5. ~~§3 WS hardening → tests → smoke → commit.~~ ✅
6. ~~§4b cookies/CSRF/throttle/generic-errors + client → tests → smoke → commit.~~ ✅
7. ~~§5 XSS escaping + CSP headers → commit.~~ ✅
8. §0 egress/R2 telemetry → commit.  ← **NEXT**
9. SECURITY.md + CHANGELOG-security.md + residual-risk → commit. **PAUSE for review.**

## Next concrete action (resume here)
Start **§4b cookie sessions + CSRF + throttle + generic errors** (`api.rs`, `server.rs`,
`session.rs`, `config.rs`, `world.rs`, `landing.html`, `client.html`):
1. `config::secure_cookies()` (`HIVE_SECURE_COOKIES`, default off in dev) + `set_session_cookie()`
   helper → `__Host-antarchy_session` (HttpOnly, Secure, SameSite=Lax, Path=/) in prod, plain
   `antarchy_session` (no Secure) in dev. `/api/login` + verify-finalize set it; stop returning the
   token in the JSON body. Add `/api/logout` (revoke + clear cookie).
2. WS auth from cookie: `play_handler`/`root_handler` read the cookie → `sessions.validate` → uid,
   pass into `handle_ws_connection`, auto-issue the `session-login` Cmd on connect; keep
   `{t:"session",token}` one release as fallback.
3. CSRF: Origin/Referer allowlist check on all POST `/api/*` (reuse `origin_allowed`).
4. Per-account login throttle: transient `World.login_attempts` keyed by ident (backoff) in
   `do_login`; also `app.rate.check` on `verify-email`/`verify-phone`.
5. Generic errors: `do_register` email-exists → uniform success + notify the existing address (no
   enumeration); keep handle-taken (public).
6. Username/color charset validators (also kills the stored-XSS source) — legacy WS `register`
   (`handlers.rs`) + REST color (`api.rs`).
7. Client: stop writing `antarchy-session`/`hive-u`/`hive-p`; rely on the cookie; logout →
   `/api/logout`; drop the plaintext-password WS login fallback. Smoke-test register→login→/play on
   :8090 with `HIVE_SECURE_COOKIES=0`.
