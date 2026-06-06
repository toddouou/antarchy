# Antarchy.fun (hive-sim) — Security

Production hardening of the live real-time multiplayer engine, mapped to **OWASP Top 10:2025**.
This document covers the **P0-critical tier** delivered on branch `security-p0`. Controls below are
implemented and tested; the **Manual follow-ups** and **Residual risk** sections list what is
deliberately left to operations or a later (P1/P2) pass.

> Engineering plan + per-section status: [`docs/security/SECURITY_P0_PLAN.md`](docs/security/SECURITY_P0_PLAN.md).

## Threat model

The engine is built against these actors, not a generic checklist:

1. **Hostile client** — the attacker controls the browser, JS, and every WS frame. The official
   client is untrusted I/O; nothing client-sent is authoritative.
2. **Map-hack / fog stripper** — reads any entity the server transmits regardless of UI.
3. **Speed / automation cheat** — forges timing, replays/forges commands, scripts superhuman rates.
4. **Economy / progression exploiter** — manipulates XP/HP/damage/territory/purchase math.
5. **Denial-of-wallet** — drives R2 Class-A ops + egress to inflate the bill.
6. **App-layer DoS** — connection floods, slowloris, oversized/compression-bomb payloads, slow consumers.
7. **Classic web attacker** — injection, XSS, CSRF, auth/session bypass, account takeover.
8. **Supply chain** — malicious or vulnerable crate / build step (addressed in P1).

**Golden rules:** the server is the sole source of truth; the client receives only what the player is
authorized to know; validate at every boundary; fail closed; never trust client time or
client-computed results; never log secrets/tokens/passwords.

## Controls implemented (P0)

### A01 — Broken Access Control / map-hack (§1 AoI, §2 anti-replay)
- **AoI view-span clamp.** `config::clamp_view_span` clamps the client-reported viewport
  (center-preserving, `i64` math to survive an adversarial full-`i32` rect) to `HIVE_MAX_VIEW_SPAN`
  on each axis, applied both in `view-set` (on store, `handlers.rs`) and `snapshot_view` (defensively,
  `network.rs`). This bounds the LOD step → bounds the fog-reveal radius in world units, closing the
  **zoom info-leak** (distant enemy queens could be un-fogged by zooming out). Zoomed-out territory
  still renders from the free R2 super-tiles.
- **Per-frame queen cap.** `cap_queens_to` keeps the viewer's own/admin (revealed) queens
  unconditionally, then the nearest-to-center, up to `HIVE_MAX_QUEENS_PER_FRAME`.
- **Anti-replay.** Mutating commands (`place-queen`, `place-ant`, `shop-buy`) require a strictly
  increasing per-connection `seq` (`World.last_seq`, reset on (re)connect, cleared on disconnect);
  duplicates / out-of-order are dropped and logged `[anticheat]`. The client stamps `seq` in `send()`.
- Every `admin-*` handler gates on `is_admin` first (pre-existing, audited). `admin-give-xp` rejects
  non-finite XP.

### A06 / A01 — Server-authoritative sim
Damage, XP, HP, territory, and ownership are computed server-side from authoritative state
(`simulation.rs`); no client-supplied result is trusted. The client never sends timestamps that the
sim reads (`current_ms()` stamps everything).

### A02 / A10 — WebSocket & DoS hardening (§3)
- **Frame/message size cap** (`HIVE_WS_MAX_MSG`, 64 KiB) via `WebSocketUpgrade::max_message_size` /
  `max_frame_size` — oversized frames close the socket. (axum does not negotiate inbound
  permessage-deflate, so there is no decompression-bomb path today; enabling it later must add a
  ratio/size cap.)
- **Origin allowlist** on the WS upgrade (`config::allowed_origins`, `HIVE_ALLOWED_ORIGINS`) → 403 on
  a disallowed browser Origin. Absent Origin (non-browser / same-origin) is allowed; cross-site WS
  hijack requires a browser, which always sends Origin.
- **Bounded send queues (backpressure).** `world::BoundedTx<T>` wraps a bounded `mpsc::Sender` and
  `try_send`s (drop-on-full), so a slow/malicious consumer can't grow server memory. Depth
  `HIVE_WS_SEND_QUEUE` (1024). Viewport frames already ride a latest-wins `watch` slot.
- **Liveness.** Keepalive `Ping` every `HIVE_WS_IDLE_SECS/2`; the socket closes if no inbound frame
  (incl. the pong) arrives within `HIVE_WS_IDLE_SECS` (slowloris / dead-peer reclaim).
- **Rate limit.** 120 msg/s per connection (pre-existing), enforced before any World access.

### A07 — Authentication & sessions (§4a, §4b)
- **Argon2id** password hashing (`auth::hash_pw_argon2`, PHC string, 16-byte salt, 32-byte output),
  cost from `HIVE_ARGON2_*` (OWASP minimum m=19 MiB/t=2/p=1 by default; raise on a RAM-rich host).
  `verify_pw` is constant-time for both Argon2id (crate) and the legacy SHA-256 fallback (`subtle`).
  Legacy hashes are **transparently re-hashed** with Argon2id on the next successful login. `admin/admin`
  keeps working (admin seed uses Argon2id).
- **Cookie sessions.** Login + email-verify set an **HttpOnly, SameSite=Lax** session cookie
  (`__Host-antarchy_session` + `Secure` when `HIVE_SECURE_COOKIES=1` in prod; plain `antarchy_session`
  in http dev). The token is **no longer returned in the JSON body** and **never stored in client JS**.
  The game socket authenticates from the cookie validated at upgrade (the credential-free `enter`
  message). `POST /api/logout` revokes the server-side session (instant) and clears the cookie.
- **Opaque tokens** (256-bit, server-side, revocable), idle+absolute expiry (`HIVE_SESSION_TTL_HOURS`).
- **CSRF.** Origin allowlist guard (`server::origin_allowed`) on every state-changing `POST /api/*` → 403.
- **Throttling.** Per-IP limiter (`HIVE_AUTH_RATE_PER_MIN`, 20) on register/login/forgot/verify, plus a
  **per-account** login throttle (10 / 60 s / identity, soft, cleared on success — no hard lockout).
- **Generic errors / no enumeration.** Uniform "Invalid credentials"; **email-taken registration is
  enumeration-safe** (responds identically to a fresh signup and emails the existing owner a heads-up
  instead of a code). Handle-taken is still reported (handles are public).
- **Input caps.** Username charset `[A-Za-z0-9_-]{3,20}`; color validated to `#rrggbb`
  (`config::valid_hex_color`) on both register paths.

### A05 — Injection & XSS (§4b source, §5 defense-in-depth)
- No SQL anywhere (no database). R2 object keys are numeric (`epoch/cx/cy`).
- The **source** of stored XSS is closed server-side (username charset + hex-color validation above).
- The client (`client.html`) adds a canonical `esc()` (HTML-escape, incl. `"`/`'`) and `safeColor()`
  (only a validated `#hex`/`var(--x)` reaches a `style=` attribute), applied to every user-controlled
  field rendered via `innerHTML` (leaderboard, region switcher/holders, discovery, inspect panels,
  admin player list).
- **Security headers** on every response (`security_headers` middleware): `Content-Security-Policy`
  (scoped to self + Google Fonts + AdSense + OSM/R2 tiles + ws/wss), `X-Content-Type-Options: nosniff`,
  `X-Frame-Options: DENY`, `Referrer-Policy: strict-origin-when-cross-origin`, HSTS.

### A09 — Egress & R2 op-budget (denial-of-wallet) (§0)
- R2 **Class-A / Class-B / delete** op counters (`metrics::r2_ops`), incremented in the snapshot sink
  (PutObject + each ListObjects page = Class A; DeleteObject free but tracked). Surfaced on
  `/egress-stats` (`r2ClassA/r2ClassB/r2Deletes/r2ClassAPerMin`).
- **Alerts:** the writer logs `[egress-alert]` when Class-A exceeds `HIVE_CLASSA_ALERT_PER_MIN`
  (600/min default) over a cycle; the viewport loop logs per-connection breaches over
  `HIVE_EGRESS_ALERT_KBPS` (off by default). R2 writes/lists happen **only** on the interval-driven
  writer thread — never per tick/request (hot-path-free invariant, confirmed).

## Configuration reference (new env knobs)

Defaults are safe for local http dev. **Bold** knobs should be set/changed in production.

| Env var | Default | Purpose / tuning |
|---|---|---|
| `HIVE_ARGON2_MEM_KIB` | 19456 | Argon2id memory (KiB). Raise (e.g. 65536) on a RAM-rich host. |
| `HIVE_ARGON2_TIME` | 2 | Argon2id iterations. |
| `HIVE_ARGON2_LANES` | 1 | Argon2id parallelism. |
| `HIVE_MAX_VIEW_SPAN` | 4000 | Max live-viewport span (world tiles). Lower = tighter AoI. |
| `HIVE_MAX_QUEENS_PER_FRAME` | 256 | Max live queens per viewport frame. |
| `HIVE_WS_MAX_MSG` | 65536 | Max inbound WS message/frame bytes. |
| `HIVE_WS_SEND_QUEUE` | 1024 | Per-connection outbound queue depth (backpressure). |
| `HIVE_WS_IDLE_SECS` | 60 | Idle/slowloris close + ping interval = half this. |
| **`HIVE_ALLOWED_ORIGINS`** | base + localhost | Comma-separated WS/CSRF Origin allowlist. Set to your prod origin(s). |
| **`HIVE_SECURE_COOKIES`** | off | **Set `1` in prod** → `__Host-` + `Secure` cookies (HTTPS only). |
| **`HIVE_PUBLIC_BASE_URL`** | – | e.g. `https://antarchy.fun`; feeds allowlist + email links. |
| `HIVE_SESSION_TTL_HOURS` | 720 | Session lifetime. |
| `HIVE_AUTH_RATE_PER_MIN` | 20 | Per-IP `/api/*` rate limit. |
| `HIVE_CLASSA_ALERT_PER_MIN` | 600 | R2 Class-A spike alert threshold (0 = off). |
| `HIVE_EGRESS_ALERT_KBPS` | 0 (off) | Per-connection egress alert (KB/s). |

Pre-existing egress knobs (`EGRESS_CAP_KBPS`, `HIVE_GUEST_EGRESS_KBPS`, `HIVE_GUEST_ANT_CAP`,
`HIVE_MAX_GUESTS`, `HIVE_SNAP_INTERVAL_SECS`, `HIVE_SNAP_TILE_CHUNKS`) remain the primary egress levers.

## Verifying

```bash
cd sim
CARGO_TARGET_DIR=target-dev cargo build
CARGO_TARGET_DIR=target-dev cargo test           # 44 tests incl. AoI clamp, queen cap, anti-replay, Argon2id
CARGO_TARGET_DIR=target-dev PORT=8090 HIVE_SECURE_COOKIES=0 cargo run   # smoke on :8090 (never touch :8080)
```
Smoke checks (all confirmed on :8090): evil WS/POST Origin → 403; login sets an HttpOnly cookie with
**no** token in the body; logout 200 + cookie cleared + token revoked; register→verify→cookie;
enumeration-safe re-register; malicious color sanitized; per-IP 429; max zoom-out leaks no distant
queens; all five security headers present; `/egress-stats` shows `r2*` counters.

## Manual follow-ups (operations — outside code)

- **Rotate the exposed R2 key** → replace with a **bucket-scoped R2 API token** (least privilege),
  revoke the old one, update `/etc/antarchy.env`, restart `antarchy.service`.
- **Cloudflare:** put the origin behind the proxy (orange-cloud); enable **Authenticated Origin Pulls
  (mTLS)**; firewall the VPS (UFW/nftables) to accept 80/443 **only from Cloudflare IP ranges**.
- **VPS:** SSH key-only (`PasswordAuthentication no`, `PermitRootLogin no`), `fail2ban`,
  `unattended-upgrades`, default-deny inbound. Run `hive-sim` as a **non-root** user under **systemd
  sandboxing** (`NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome`, `PrivateTmp`,
  `CapabilityBoundingSet=`, `SystemCallFilter=@system-service`, `MemoryMax`, `TasksMax`, `LimitNOFILE`).
- **Prod env:** set `HIVE_SECURE_COOKIES=1`, `HIVE_ALLOWED_ORIGINS`, `HIVE_PUBLIC_BASE_URL`.
- **Confirm** HSTS/CSP are live at the edge and the CDN cache-hit ratio on tiles/snapshots is high.
- **Replace `admin/admin`** before public launch (left functional intentionally for now).

## Residual risk / known limitations

- **CSP uses `'unsafe-inline'`** for scripts (the client is a single inline-script document) and a
  broad `connect-src https:` (the R2 base is env-dynamic). A nonce/extraction pass to drop
  `'unsafe-inline'` and tighten `connect-src` is a P1/P2 follow-up.
- **Anti-replay is keyed by player id** (the one-connection-per-account model, same as `conn_gen`).
  Two simultaneous tabs on one account is not a supported configuration.
- **Spectator roster** (`build_queen_roster`) exposes live queen `x,y` to guests. Accepted: the public
  homescreen shows a shared demo and there are no real third-party spectators; documented, not changed.
- **Per-IP egress aggregation** is not implemented; per-connection alerting is. Per-IP is a refinement.
- **Legacy paths retained:** the WS `login`/`register` handlers (used by the bench/admin tooling) and
  the transitional `{t:"session",token}` WS path (one release) still exist.
- **P1/P2 not yet done:** supply chain (`cargo audit`/`cargo deny`, pin/commit lock — A03), edge/VPS &
  systemd hardening (A02), Caddy header enforcement/CSP tightening (A02), secrets handling
  (`secrecy`/`zeroize`, gitignore `sessions.json` — A04/A08), structured logging + kill switch + cheat
  signals (A09), `#![forbid(unsafe_code)]` / clippy `unwrap_used`-`panic` denied / `overflow-checks`
  / panic-at-task-boundary (A10).
