# EMAIL + SEAMLESS LOGIN PLAN

**Goal:** make account signup, email verification, password reset, and login work
**end-to-end for real public users, on a free tier**, with no recurring cost and no
recompile required for routine operation.

**Status legend:** ☐ todo · ◐ in progress · ☑ done
**Owner:** Todd · **Branch:** `beta-v3` · **Last updated:** 2026-06-07

> **2026-06-07 (beta-v3) — code polish landed (compiles clean):** resend-code endpoint
> (`POST /api/resend-code` → `api::resend` → `email::send_code`); plaintext `text:` part added to
> all Resend sends (`src/email.rs`); verify-step UI now auto-submits on 6 digits / paste, strips
> non-digits, and has a "Resend code" link with a 30 s cooldown (`public/landing.html`). **Still
> needs Phase A config/DNS + a rebuild/redeploy to go live** (client is `include_str!`'d).

> This is a multi-session checkpoint doc (same convention as
> [`docs/egress/EGRESS_PLAN.md`](../egress/EGRESS_PLAN.md) and
> [`docs/security/SECURITY_P0_PLAN.md`](../security/SECURITY_P0_PLAN.md)). Update the status
> markers as each item lands so the next session can resume instantly.

---

## 0. TL;DR

The **code is already complete** — register → email a 6-digit code → verify → session
cookie → logged in, plus forgot/reset-password. Nothing is broken in Rust. Two things are
unconfigured in production:

1. **Origin allowlist is empty** → the CSRF/WS Origin guard rejects every browser request
   with `"bad origin"` (this is the login failure you saw). **Fix = env vars, no rebuild.**
2. **Email provider is dormant** → with no `HIVE_RESEND_API_KEY`/`HIVE_EMAIL_FROM`, the
   verification code is only printed to the server log, so real users never receive it.
   **Fix = free Resend account + domain DNS + env vars, no rebuild.**

Everything in **Phase A** is config/DNS only and gets signup working for free. **Phase B** is
optional code polish (resend-code button, stale-pending cleanup). **Phase C** is deliverability
hardening so codes land in the inbox, not spam.

---

## 1. Current state (verified against the code)

| Piece | Where | State |
|---|---|---|
| Register (pending account + code) | `src/api.rs:318` `register` → `:99` `do_register` | ☑ built |
| Verify email → finalize + session | `src/api.rs:341` `verify_email` → `:352` `finish_verify` | ☑ built |
| Login → session cookie | `src/api.rs:366` `login` | ☑ built |
| Forgot / reset password | `src/api.rs:393` / `:405` | ☑ built |
| Email send (Resend + dev log fallback) | `src/email.rs:50` `send` | ☑ built, **dormant** |
| Origin / CSRF guard | `src/server.rs:114` `origin_allowed`, `src/api.rs:442` `reject_csrf` | ☑ built, **misconfigured** |
| Cookie sessions (`__Host-` in prod) | `src/session.rs`, `src/config.rs:447` `session_cookie_name` | ☑ built |
| Client signup/verify/login/reset UI | `public/landing.html` ~`:370`–`:428` | ☑ built |
| Phone/SMS verification | `src/sms.rs`, `src/config.rs:397` `sms_enabled` | stub, **off by default** (good — email-only) |

**Key facts that shape this plan:**

- **Phone is NOT required.** `phone_required = sms_enabled() && !phone.is_empty()`
  (`src/api.rs:108`); `sms_enabled()` defaults **off**, so accounts finalize on **email
  verification alone**. We deliberately keep SMS off (no free SMS tier worth wiring).
- **Verification is mandatory.** `do_register` creates a `PendingReg` (`src/world.rs:~255`),
  not a real account; the account only exists after `verify-email` succeeds. So if the email
  never arrives, the user is stuck → email delivery is on the critical path.
- **Codes expire in 15 min** (`PendingReg.created_ms` + check in `finish_verify`) and there is
  a small failed-attempt cap (brute-force guard). There is **no resend-code endpoint** today
  (Phase B gap).
- **All `/api/*` + the WS upgrade share `origin_allowed`** (`src/server.rs:114`). One env var
  fixes both the "bad origin" login error and the `wss://antarchy.fun/` spectator failure.
- **Env is read once at startup** via `OnceLock` (e.g. `src/config.rs:583` `allowed_origins`,
  `:383` `resend_api_key`). Changing any `HIVE_*` value requires a **service restart**, not a
  rebuild.

---

## 2. The free stack

**Provider: [Resend](https://resend.com)** — already integrated in `src/email.rs` (POSTs to
`https://api.resend.com/emails`). No code change needed to start sending.

- **Free tier:** ~**3,000 emails/month**, ~**100 emails/day**, **1 verified domain**.
  (Confirm current numbers at <https://resend.com/pricing> before launch.)
- **Cost:** $0 within those limits. A 6-digit code is one email; password reset is one email.
  3,000/mo comfortably covers a beta (≈100 new signups/day even if each retries a couple times).
- **Why Resend over alternatives:** it's already wired; the free tier needs no credit card; DKIM
  setup is a 3-record copy/paste. (SendGrid/Mailgun free tiers are comparable but would need new
  code; SES is cheap but not free and has a sandbox approval step. Stick with Resend.)

**Domain:** sending must come from `antarchy.fun` (e.g. `noreply@antarchy.fun`). Resend's shared
`onboarding@resend.dev` sender only delivers to *your own* account email — useless for public
signups — so domain verification (Phase A3) is required, not optional.

---

## 3. Phase A — Config + DNS (no code) → signup works, free

> Outcome: real users can sign up, receive a code, verify, and log in. ~15 min of work, most
> of it waiting for DNS + Resend's domain check.

### A1 — Fix the Origin guard (unblocks login + spectator WS) ☐
Append to `/etc/antarchy.env` on the VPS:
```
HIVE_ALLOWED_ORIGINS=https://antarchy.fun,https://www.antarchy.fun
HIVE_PUBLIC_BASE_URL=https://antarchy.fun
HIVE_SECURE_COOKIES=1
```
- `HIVE_ALLOWED_ORIGINS` — exact `scheme://host`, no trailing slash. Trim `www` if you don't
  serve it; add `localhost` back only if you test the prod binary locally.
- `HIVE_PUBLIC_BASE_URL` — also used to build the reset-password link in emails
  (`src/email.rs:23`), so it must be the real HTTPS origin.
- `HIVE_SECURE_COOKIES=1` — enables the hardened `__Host-` Secure cookie (you're on HTTPS).

### A2 — Resend account + API key ☐
1. Sign up at <https://resend.com> (free, no card).
2. **API Keys → Create** → scope "Sending access". Copy the `re_...` key (shown once).

### A3 — Verify the `antarchy.fun` domain (DNS) ☐
In Resend: **Domains → Add Domain → `antarchy.fun`**. It generates records to add at your DNS
host (the registrar / Cloudflare zone for antarchy.fun). They look like this (use the **exact**
values Resend shows — tokens are per-account):

| Type | Name/Host | Value | Purpose |
|---|---|---|---|
| TXT | `send` (→ `send.antarchy.fun`) | `v=spf1 include:amazonses.com ~all` | SPF |
| MX  | `send` | `feedback-smtp.<region>.amazonses.com` (priority 10) | bounce handling |
| TXT | `resend._domainkey` | `p=MIGfMA0...` (long DKIM key) | DKIM signing |
| TXT | `_dmarc` | `v=DMARC1; p=none;` | DMARC (start permissive) |

Then click **Verify** in Resend (DNS can take minutes–hours to propagate). Domain must show
**Verified** before real sends work.

### A4 — Wire the email env vars ☐
Append to `/etc/antarchy.env`:
```
HIVE_RESEND_API_KEY=re_xxxxxxxxxxxxxxxx
HIVE_EMAIL_FROM=Antarchy <noreply@antarchy.fun>
```
> `noreply@antarchy.fun` does not need a real mailbox — it just has to be on the verified
> domain. (Consider a real `support@` reply-to in Phase C.)
> **Secret hygiene:** the API key is a live credential. If it ever lands in a chat, log, or
> screenshot, rotate it in Resend.

### A5 — Restart + verify ☐
```
sudo systemctl restart antarchy
# guard passes (NOT "bad origin"):
curl -s -i -X POST https://antarchy.fun/api/login \
  -H 'Origin: https://antarchy.fun' -H 'Content-Type: application/json' \
  -d '{"ident":"nobody","password":"wrong"}' | head -n 20
```
Then sign up with a **real inbox** end-to-end (see §6).

---

## 4. Phase B — Code polish for a seamless flow (optional, needs rebuild+redeploy)

These close real UX gaps. Each is a Rust change → recompile (`cargo build --release`) +
redeploy. Tackle in priority order.

### B1 — "Resend code" endpoint + button ☑ DONE (2026-06-07)
Built: `AuthOp::ResendCode` + sim-side `resend()` (new code, restarts the 15-min window, **keeps the
attempt count** so it can't reset the brute-force guard); `POST /api/resend-code` handler (same
`origin_allowed` + per-IP `rate.check` guards as register); landing `vemail` step has a "Resend code"
link with a 30 s client cooldown. Server-side abuse is bounded by the per-IP limiter + the cooldown.
~~Today a lost/expired code is a dead end (code expires in 15 min, no re-send). Add:~~
- `AuthOp::ResendCode { reg_id }` in `src/api.rs` enum (~`:50`); handle in the sim-side match
  (regenerate the email code on the existing `PendingReg`, reset `created_ms`, keep attempts).
- Route `POST /api/resend-code` (`src/server.rs` router) → `resend_code` handler mirroring
  `verify_email`'s guards (`origin_allowed` + `app.rate.check`), then `email::send_code`.
- Landing UI: a "Resend code" link on the `vemail` step (`public/landing.html` ~`:393`) with a
  ~30 s cooldown on the button. **Recompile** (client is `include_str!`'d).
- Rate-limit it hard (it's a cost/abuse amplifier like register) via the existing per-IP
  `app.rate` + the per-account attempt cap.

### B2 — Stale pending-registration cleanup ☐
Ensure expired `PendingReg`s are swept (don't accumulate in `world.pending_regs`, and don't
block a genuine re-signup of the same email). Add a sweep in the sim loop or lazily on register.
Verify whether re-registering an email that has an *unexpired* pending reg replaces it cleanly.

### B3 — Verify-step UX niceties ◐ (mostly done 2026-06-07)
- ☑ Auto-submit when 6 digits are entered / support paste of the whole code (non-digits stripped)
  (`public/landing.html` `ve-code` `input` handler).
- ☑ "Resend code" link + spam hint now both visible on the verify step (the static spam hint + B1's
  resend link cover "didn't get it?").
- ☐ Friendlier copy for the 429 throttle response ("Too many tries — wait a minute").

### B4 — Welcome email (optional) ☐
On `Verified`, optionally send a one-time welcome/onboarding email. Pure upside, counts against
the free quota — skip if near the ceiling.

---

## 5. Phase C — Deliverability hardening (so codes hit the inbox)

- **Plaintext part:** ☑ DONE (2026-06-07) — `email::send` now takes a `text` body and includes it in
  the Resend payload (`{from,to,subject,html,text}`); `send_code`/`send_reset`/`send_register_exists_notice`
  each build a plaintext version. The dev-log fallback collapses it to one greppable `email:DEV` line.
- **DMARC ramp:** start `p=none` (Phase A3), then tighten to `p=quarantine` once SPF+DKIM are
  confirmed aligned in Resend's dashboard. ☐
- **From name + reply-to:** use a recognizable `From` ("Antarchy") and a monitored `reply-to`
  (e.g. `support@antarchy.fun`) so replies aren't black-holed. ☐
- **Subject/links:** keep the verify subject plain; avoid spammy phrasing; the reset link must
  be `https://antarchy.fun/...` (driven by `HIVE_PUBLIC_BASE_URL`). ☐
- **Warm-up:** volume is tiny, but if early mail lands in spam, send a few test codes to
  Gmail/Outlook/Yahoo and mark "not spam" to seed reputation. ☐

---

## 6. Test plan

### Dev / staging (no Resend needed — log fallback)
Run with no `HIVE_RESEND_API_KEY`; the code prints to the console (`src/email.rs:52`):
```
journalctl -u antarchy -f | grep --line-buffered 'email:DEV'
# register on the page → read the printed code → enter it → should land in-game
```
This proves register → verify → session works independent of email delivery.

### Production end-to-end (after Phase A)
1. Register with a **real inbox you control** → confirm the email arrives (check spam first
   time).
2. Enter the code → expect `{ok:true, done:true}` + redirect into `/play` (session cookie set).
3. Reload `antarchy.fun` → should show **"Enter game ▸"** (cookie persists = seamless return).
4. Log out (`/api/logout`) → log back in with email+password → success.
5. **Forgot password** → reset link arrives → set new password → log in with it.
6. **Legacy user** (pre-existing account) → log in → confirm it works (the original bug) and,
   if it was a legacy SHA-256 hash, that it transparently rehashes to Argon2id on login.
7. Negative: wrong code → "Incorrect code"; expired (>15 min) code → rejected; re-register an
   existing email → enumeration-safe "check your email" (no "already exists" leak).

### Smoke (curl)
```
# origin guard rejects a foreign origin:
curl -s -X POST https://antarchy.fun/api/login -H 'Origin: https://evil.test' \
  -H 'Content-Type: application/json' -d '{}' | grep -q 'bad origin' && echo "guard OK"
```

---

## 7. Free-tier budget & ceiling behavior

- **Per signup:** 1 verify email (+ at most a couple resends) and, rarely, 1 reset email.
- **Headroom:** 100/day ≈ 100 fresh signups/day; 3,000/mo ≈ steady beta traffic. Watch the
  Resend dashboard's usage meter.
- **At the ceiling:** Resend starts failing sends. `email::send` logs the error
  (`src/email.rs:69`) but **does not surface it to the user** — they'd just never get a code.
  Mitigations if you approach the limit:
  - Add daily-volume telemetry / an alert (mirror the R2 denial-of-wallet pattern in
    `src/metrics.rs`).
  - Tighten the per-IP register throttle (`HIVE_AUTH_RATE_PER_MIN`, `src/config.rs:461`).
  - Only then consider Resend's paid tier (~$20/mo for 50k) — still cheap, but out of scope
    for "free".

---

## 8. Rollout checklist (do in this order)

1. ☐ **A1** origin/cookie env vars → `restart` → verify "bad origin" is gone (fixes login NOW).
2. ☐ **A2** Resend account + API key.
3. ☐ **A3** add DNS records → domain shows **Verified** in Resend.
4. ☐ **A4** `HIVE_RESEND_API_KEY` + `HIVE_EMAIL_FROM` → `restart`.
5. ☐ **A5 / §6** real end-to-end signup from an external inbox (check spam).
6. ☑ **C1** plaintext email part — **code landed 2026-06-07** (still needs the rebuild+redeploy in step 4/A4).
7. ☑ **B1** resend-code endpoint+button — **code landed 2026-06-07** (rebuild+redeploy to ship).
8. ◐ **B2/B3** — B3 verify-UX polish mostly done (2026-06-07); B2 pending-reg cleanup already covered by `api::gc`; 429-copy still ☐.
9. ☐ **C2–C5** DMARC tighten, reply-to, deliverability monitoring.

**Phases A + C1 = "fully working, free email + seamless login."** B is quality-of-life.

---

## 9. Code map (where each piece lives)

```
src/email.rs           send_code / send_reset / send_register_exists_notice / send (dev-log fallback :50)
src/api.rs             register :318 · verify_email :341 · login :366 · forgot :393 · reset :405
                       do_register :99 (phone_required :108) · finish_verify :352 · reject_csrf :442
src/config.rs          resend_api_key :383 · email_from :389 · sms_enabled :397 · public_base_url :453
                       allowed_origins :583 · secure_cookies :~440 · session_cookie_name :447
                       auth_rate_per_min :461 (HIVE_AUTH_RATE_PER_MIN)
src/server.rs          origin_allowed :114 · upgrade_guarded :96 · /api/* + WS routes
src/session.rs         mint / validate / revoke cookie sessions
src/world.rs           PendingReg :~255 (created_ms, attempts, email_ok, phone_required)
public/landing.html    login :370 · register :380 · verify-email :393 · forgot :412 · reset :418
                       (compiled into the binary via include_str! → recompile to apply UI edits)
```

### Env vars touched by this plan (all `HIVE_*`, read once at startup → restart to apply)
```
HIVE_ALLOWED_ORIGINS    https://antarchy.fun,https://www.antarchy.fun   # CSRF/WS origin allowlist
HIVE_PUBLIC_BASE_URL    https://antarchy.fun                            # email links + origin default
HIVE_SECURE_COOKIES     1                                               # __Host- Secure cookie (HTTPS)
HIVE_RESEND_API_KEY     re_...                                          # Resend send key (secret)
HIVE_EMAIL_FROM         Antarchy <noreply@antarchy.fun>                 # verified-domain sender
# (left OFF on purpose) HIVE_SMS_ENABLED                                # keep email-only signup
HIVE_AUTH_RATE_PER_MIN  20                                              # per-IP /api/* throttle (default)
```
