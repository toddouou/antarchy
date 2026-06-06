# Security Changelog — P0 hardening (branch `security-p0`)

Grouped by **OWASP Top 10:2025** category. Each entry maps to a commit. Baseline = `bbe539f`
(beta-v2 auth/landing state, pre-hardening).

## A01 — Broken Access Control
- **AoI view-span clamp + per-frame queen cap** (`6b76460`) — fixes the zoom info-leak: the live
  viewport is clamped server-side to `HIVE_MAX_VIEW_SPAN`, bounding the fog-reveal radius so distant
  enemy queens can no longer be revealed by zooming out; queens per frame capped to
  `HIVE_MAX_QUEENS_PER_FRAME` (own/admin always kept).
- **Anti-replay command sequencing** (`7c617b4`) — monotonic per-connection `seq` on mutating
  commands; duplicates/out-of-order dropped + logged.

## A02 — Security Misconfiguration / DoS
- **WebSocket hardening** (`b85c57c`) — inbound frame/message size cap (`HIVE_WS_MAX_MSG`), Origin
  allowlist on upgrade (403 on cross-site), bounded per-connection send queues (`BoundedTx`,
  `HIVE_WS_SEND_QUEUE`), keepalive ping + idle/slowloris close (`HIVE_WS_IDLE_SECS`).
- **Security headers** (`90f4375`) — CSP, `X-Content-Type-Options`, `X-Frame-Options: DENY`,
  `Referrer-Policy`, HSTS on every response.

## A05 — Injection / XSS
- **Server-side input validation** (`7c649f9`) — username charset `[A-Za-z0-9_-]`, color → `#rrggbb`
  on both register paths (closes the stored-XSS source).
- **Client output-encoding** (`90f4375`) — `esc()` / `safeColor()` applied to all user-controlled
  fields rendered via `innerHTML` (leaderboard, regions, discovery, inspect, admin list).

## A06 — Vulnerable & Outdated Components / server-authoritative cheat resistance
- Confirmed all state transitions (damage/XP/HP/territory) are server-computed; `admin-give-xp`
  rejects non-finite input (`7c617b4`). (Crate audit/pinning is P1 — A03.)

## A07 — Identification & Authentication Failures
- **Argon2id password hashing with transparent rehash** (`0fedcb3`) — replaces SHA-256 + constant
  salt; constant-time verify for both formats; legacy hashes upgraded on next login.
- **Cookie sessions + CSRF + throttle + no enumeration** (`7c649f9`) — HttpOnly `__Host-` session
  cookie (token out of JS), WS auth from cookie via `enter`, `/api/logout` revocation, CSRF Origin
  guard on `/api/*`, per-account login throttle, enumeration-safe email-taken, generic errors.

## A09 — Security Logging & Monitoring Failures (denial-of-wallet)
- **R2 op-budget telemetry + alerts** (`fdabdfd`) — Class-A/Class-B/delete counters on `/egress-stats`,
  Class-A spike alert in the snapshot writer (`HIVE_CLASSA_ALERT_PER_MIN`), opt-in per-connection
  egress alert (`HIVE_EGRESS_ALERT_KBPS`).

## A10 — Server-Side Request Forgery / exceptional conditions
- No SSRF surface (server makes only fixed outbound calls: Resend, R2). Robust WS error handling (no
  `unwrap()` on network input; bounded sends; idle close). Full `Result`-based paths; broader
  `#![forbid(unsafe_code)]` / clippy-deny / `overflow-checks` / panic-at-boundary work is P2.

## A03 / A04 / A08 — not in P0
Supply-chain (`cargo audit`/`cargo deny`, lockfile pinning), secrets handling (`secrecy`/`zeroize`,
gitignore `sessions.json`), and design-level edge/VPS hardening are scheduled for P1. See
`docs/security/SECURITY_P0_PLAN.md` → "After P0".

---
_Note: commits `0fedcb3`, `6b76460`, `7c617b4` and the baseline carry a stray `@ ` subject prefix
from a shell here-string quirk (cosmetic; not amended to avoid rewriting history). `b85c57c` onward
are clean._
