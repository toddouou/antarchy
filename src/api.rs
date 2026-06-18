//! REST auth (`/api/*`) for the beta-v2 public web layer. Login/registration moved out of the game
//! and onto the landing page; these HTTP endpoints back it.
//!
//! **Actor model (mirrors the WS path):** the sim thread owns `World` for writes, so an HTTP handler
//! must NOT take `world.write().await` (it would contend the per-tick `blocking_write`). Instead each
//! handler builds an [`AuthOp`], pushes it as `Cmd::AuthApi`, and `await`s a `oneshot` reply — the
//! mutation runs on the sim thread between ticks (≤ ~1 tick latency), serialized with everything else.
//! All network I/O (email/SMS) happens HERE on the tokio runtime, AFTER the reply — never on the sim
//! thread. The sim only produces the code + recipient in its reply.
//!
//! Durable account state lives in `users.json` (via `auth`); unverified registrations and reset
//! tokens live in transient `World.pending_regs` / `World.reset_tokens` (RAM-only, GC'd in the tick
//! loop). Sessions live in `AppState.sessions` (its own lock).

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::State,
    http::header::{CACHE_CONTROL, CONTENT_TYPE, SET_COOKIE},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use parking_lot::Mutex;
use rand::Rng;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::oneshot;

use crate::auth::{hash_pw_argon2, needs_rehash, verify_pw, UserRecord};
use crate::config::{
    auth_rate_per_min, current_ms, secure_cookies, session_cookie_name, session_ttl_hours,
    sms_enabled, stripe_secret_key, ADMIN_USERNAME, HUES,
};
use crate::server::{origin_allowed, session_uid_from_cookie, AppState, Cmd};
use crate::world::{PendingReg, ResetToken, World};

// ---- Operations + outcomes (cross the sim-thread boundary via Cmd::AuthApi) ---------------------

/// A REST auth operation, applied to `World` on the sim thread by [`apply`].
pub enum AuthOp {
    Register { handle: String, email: String, phone: String, password: String, color: String, hue_idx: i32 },
    VerifyEmail { reg_id: String, code: String },
    VerifyPhone { reg_id: String, code: String },
    Login { ident: String, password: String },
    Forgot { email: String },
    Reset { token: String, password: String },
    ResendCode { reg_id: String },
    /// Credit gems to an account (Stripe `checkout.session.completed` webhook). Runs on the sim
    /// thread so it serializes with the `users.json` save like every other auth mutation.
    GrantGems { uid: u32, gems: u64, session: String },
}

/// The sim thread's reply for an [`AuthOp`]. The HTTP handler turns it into JSON and does any sending.
pub enum AuthOutcome {
    /// Registration accepted; the handler must send the code(s). `email_code`/`phone_code` never reach
    /// the client — only the server console (dev) or the email/SMS provider.
    RegPending { reg_id: String, email: String, email_code: String, phone: String, phone_code: String, phone_required: bool },
    /// A code matched but the account isn't finalized yet (other channel still pending).
    VerifyProgress { email_ok: bool, phone_ok: bool, phone_required: bool },
    /// Registration where the email is already taken. Enumeration-safe: the HTTP handler returns the
    /// SAME shape as `RegPending` and instead emails the EXISTING owner a heads-up — so an attacker
    /// can't tell "taken" from "fresh". No pending registration is created (`reg_id` is a throwaway).
    RegExisting { email: String, reg_id: String, phone_required: bool },
    /// Account created/active — mint a session token for this identity.
    Verified { uid: u32, handle: String },
    /// Login OK — mint a session token.
    LoggedIn { uid: u32, handle: String },
    /// Forgot-password processed. `send` is `Some((email, token))` only when a real account matched;
    /// the handler still returns a uniform 200 to the client (enumeration-safe).
    ForgotResult { send: Option<(String, String)> },
    ResetOk,
    /// A resend-code request matched a live pending registration; the handler re-sends the email code.
    ResendResult { email: String, email_code: String },
    /// Gems credited (or no-op if the uid didn't resolve — logged in `grant_gems`). The webhook
    /// returns 200 regardless so Stripe stops retrying.
    GemsGranted,
    Error { msg: String },
}

fn deny(msg: &str) -> AuthOutcome { AuthOutcome::Error { msg: msg.to_string() } }

// ---- Sim-thread application (runs under the World write lock, between ticks) --------------------

pub fn apply(world: &mut World, op: AuthOp) -> AuthOutcome {
    match op {
        AuthOp::Register { handle, email, phone, password, color, hue_idx } =>
            do_register(world, handle, email, phone, password, color, hue_idx),
        AuthOp::VerifyEmail { reg_id, code } => verify(world, reg_id, code, true),
        AuthOp::VerifyPhone { reg_id, code } => verify(world, reg_id, code, false),
        AuthOp::Login { ident, password }    => do_login(world, &ident, &password),
        AuthOp::Forgot { email }             => forgot(world, &email),
        AuthOp::Reset { token, password }    => reset(world, &token, &password),
        AuthOp::ResendCode { reg_id }        => resend(world, reg_id),
        AuthOp::GrantGems { uid, gems, session } => grant_gems(world, uid, gems, &session),
    }
}

/// Credit `gems` to the account with this numeric id. Persists `users.json` immediately. A `false`
/// `ok` means the uid didn't match any account (stale/forged metadata) — the webhook still 200s.
///
/// **Idempotent on the Stripe `checkout.session.id`** (`session`): a webhook retry/replay for an
/// already-credited session is a no-op, so a duplicate delivery can never double-grant gems. The
/// processed-session ledger lives in `users.json` (wipe-proof), so a restart still rejects replays.
fn grant_gems(world: &mut World, uid: u32, gems: u64, session: &str) -> AuthOutcome {
    if !session.is_empty() {
        if world.auth.processed_payments.contains(session) {
            println!("[stripe] duplicate webhook for session {session} (uid {uid}) — already credited, skipped");
            return AuthOutcome::GemsGranted;
        }
        world.auth.processed_payments.insert(session.to_string());
    }
    match world.auth.users.values_mut().find(|u| u.id == uid) {
        Some(u) => {
            u.gems = u.gems.saturating_add(gems);
            world.auth.save();
        }
        None => {
            eprintln!("[stripe] webhook credited unknown uid {uid} ({gems} gems) — ignored");
            world.auth.save();   // still persist the processed-session id so a replay stays a no-op
        }
    }
    AuthOutcome::GemsGranted
}

fn do_register(world: &mut World, handle: String, email: String, phone: String,
            password: String, color: String, hue_idx: i32) -> AuthOutcome {
    let handle = handle.trim().to_string();
    let email  = email.trim().to_lowercase();
    let phone  = phone.trim().to_string();
    if handle.len() < 3 || handle.len() > 20 { return deny("Handle must be 3–20 characters"); }
    if !handle.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-') {
        return deny("Handle: letters, numbers, _ or - only");
    }
    if !valid_email(&email)    { return deny("Enter a valid email address"); }
    if password.len() < 8      { return deny("Password must be at least 8 characters"); }
    let uname = handle.to_uppercase();
    if uname == ADMIN_USERNAME                  { return deny("That handle is reserved"); }
    // Handle-taken is revealed (handles are public on the leaderboard); EMAIL-taken is NOT (below).
    if world.auth.users.contains_key(&uname)    { return deny("That handle is taken"); }

    // Validate the colour to a #rrggbb hex (or fall back) — kills a stored-XSS vector via `style=`.
    let color = sanitize_color(&color);
    // Phone verification is required only when SMS is enabled AND a phone was supplied.
    let phone_required = sms_enabled() && !phone.is_empty();

    // Enumeration-safe email-taken: respond identically to a fresh registration (the caller emails the
    // existing owner a heads-up instead of a code), so an attacker can't probe which emails exist.
    if world.auth.email_taken(&email) {
        return AuthOutcome::RegExisting { email, reg_id: crate::session::new_token(), phone_required };
    }

    let email_code = gen_code();
    let phone_code = gen_code();
    let reg_id = crate::session::new_token();

    world.pending_regs.insert(reg_id.clone(), PendingReg {
        email: email.clone(), phone: phone.clone(), handle,
        password_hash: hash_pw_argon2(&password), color, hue_idx,
        email_code: email_code.clone(), phone_code: phone_code.clone(),
        email_ok: false, phone_ok: false, phone_required,
        created_ms: current_ms(), attempts: 0,
    });
    AuthOutcome::RegPending { reg_id, email, email_code, phone, phone_code, phone_required }
}

fn verify(world: &mut World, reg_id: String, code: String, is_email: bool) -> AuthOutcome {
    let code = code.trim().to_string();
    // Phase 1: check the code + flip the channel flag (or count a failed attempt).
    let (matched, too_many) = match world.pending_regs.get_mut(&reg_id) {
        None => return deny("Registration expired — please sign up again"),
        Some(pr) => {
            if pr.attempts >= 6 { (false, true) }
            else {
                let expected = if is_email { &pr.email_code } else { &pr.phone_code };
                if &code == expected {
                    if is_email { pr.email_ok = true; } else { pr.phone_ok = true; }
                    (true, false)
                } else { pr.attempts += 1; (false, false) }
            }
        }
    };
    if too_many { world.pending_regs.remove(&reg_id); return deny("Too many attempts — please sign up again"); }
    if !matched { return deny("Incorrect code"); }

    // Phase 2: not finalized until email (+ phone when required) are both verified.
    {
        let pr = world.pending_regs.get(&reg_id).unwrap();
        if !(pr.email_ok && (!pr.phone_required || pr.phone_ok)) {
            return AuthOutcome::VerifyProgress {
                email_ok: pr.email_ok, phone_ok: pr.phone_ok, phone_required: pr.phone_required,
            };
        }
    }

    // Phase 3: promote the pending registration into a durable account.
    let pr = world.pending_regs.remove(&reg_id).unwrap();
    if world.auth.email_taken(&pr.email)   { return deny("An account with that email already exists"); }
    let uname = pr.handle.to_uppercase();
    if world.auth.users.contains_key(&uname) { return deny("That handle was just taken — pick another"); }
    let id = world.next_player_id;
    world.next_player_id += 1;
    world.auth.users.insert(uname.clone(), UserRecord {
        id, username: uname, password_hash: pr.password_hash,
        color: pr.color, hue_idx: pr.hue_idx, is_admin: false, color_chosen: true,
        peak_level: 0, email: pr.email, phone: pr.phone, handle: pr.handle.clone(),
        email_verified: true, phone_verified: pr.phone_ok,
        // The signup starter inventory counts as day one's portion — first CLAIM at next 00:00 UTC.
        last_claim_day: crate::config::utc_day(crate::config::current_ms()),
        gems: 0,   // cosmetics currency — credited via Stripe gem purchases
        owned_cosmetics: Vec::new(),
        equipped: std::collections::HashMap::new(),
        last_accrual_day: 0,
        alliance_id: None,
    });
    world.auth.save();
    AuthOutcome::Verified { uid: id, handle: pr.handle }
}

fn do_login(world: &mut World, ident: &str, password: &str) -> AuthOutcome {
    let ident = ident.trim();
    // Per-account throttle (OWASP A07): bound brute-force on one account independently of the per-IP
    // `/api/*` limiter — 10 attempts / 60 s window per identity, cleared on success. Soft (no hard
    // lockout that becomes a DoS-by-proxy).
    let key = ident.to_lowercase();
    {
        let now = current_ms();
        let e = world.login_attempts.entry(key.clone()).or_insert((now, 0));
        if now.saturating_sub(e.0) > 60_000 { *e = (now, 0); }
        e.1 += 1;
        if e.1 > 10 { return deny("Too many attempts — try again shortly"); }
    }
    // Resolve by email, then phone, then legacy username — owned clones avoid borrow conflicts.
    let rec = world.auth.find_by_email(ident).cloned()
        .or_else(|| world.auth.find_by_phone(ident).cloned())
        .or_else(|| world.auth.users.get(&ident.to_uppercase()).cloned());
    // Uniform "Invalid credentials" whether or not the account exists (no enumeration).
    let Some(rec) = rec else { return deny("Invalid credentials"); };
    if !verify_pw(&rec.password_hash, password) { return deny("Invalid credentials"); }
    if world.auth.banned.contains(&rec.username) { return deny("This account is banned"); }
    world.login_attempts.remove(&key); // a real success resets the counter
    // Transparent upgrade: a legacy SHA-256 (or under-cost) hash is re-hashed with Argon2id now that
    // we hold the plaintext and a confirmed match. One-time per account; persisted immediately.
    if needs_rehash(&rec.password_hash) {
        let new_hash = hash_pw_argon2(password);
        if let Some(u) = world.auth.users.get_mut(&rec.username) { u.password_hash = new_hash; }
        world.auth.save();
    }
    AuthOutcome::LoggedIn { uid: rec.id, handle: rec.display_name().to_string() }
}

fn forgot(world: &mut World, email: &str) -> AuthOutcome {
    let email = email.trim().to_lowercase();
    // Only attach a send job when a real verified-email account matches; the caller always 200s.
    let target = world.auth.find_by_email(&email).map(|u| (u.email.clone(), u.username.clone()));
    match target {
        Some((to, uname)) => {
            let token = crate::session::new_token();
            world.reset_tokens.insert(token.clone(), ResetToken { username: uname, created_ms: current_ms() });
            AuthOutcome::ForgotResult { send: Some((to, token)) }
        }
        None => AuthOutcome::ForgotResult { send: None },
    }
}

fn reset(world: &mut World, token: &str, password: &str) -> AuthOutcome {
    if password.len() < 8 { return deny("Password must be at least 8 characters"); }
    let rt = match world.reset_tokens.get(token).cloned() {
        Some(rt) => rt,
        None => return deny("This reset link is invalid or has expired"),
    };
    if current_ms().saturating_sub(rt.created_ms) > 3_600_000 {
        world.reset_tokens.remove(token);
        return deny("This reset link has expired");
    }
    world.reset_tokens.remove(token);
    if let Some(u) = world.auth.users.get_mut(&rt.username) {
        u.password_hash = hash_pw_argon2(password);
        world.auth.save();
        AuthOutcome::ResetOk
    } else {
        deny("Account not found")
    }
}

/// Re-issue the email verification code for a still-pending registration (the first one may have
/// expired or been lost). Generates a fresh code and restarts the 15-min expiry window, but keeps
/// the failed-attempt count so a resend can't be used to wipe the brute-force guard.
fn resend(world: &mut World, reg_id: String) -> AuthOutcome {
    match world.pending_regs.get_mut(&reg_id) {
        None => deny("Registration expired — please sign up again"),
        Some(pr) => {
            let code = gen_code();
            pr.email_code = code.clone();
            pr.created_ms = current_ms();
            AuthOutcome::ResendResult { email: pr.email.clone(), email_code: code }
        }
    }
}

/// GC expired pending registrations (>15 min) and reset tokens (>1 h). Called periodically by the
/// sim loop so the transient maps can't grow unbounded.
pub fn gc(world: &mut World) {
    let now = current_ms();
    world.pending_regs.retain(|_, pr| now.saturating_sub(pr.created_ms) < 15 * 60_000);
    world.reset_tokens.retain(|_, rt| now.saturating_sub(rt.created_ms) < 60 * 60_000);
    world.login_attempts.retain(|_, (start, _)| now.saturating_sub(*start) < 5 * 60_000);
}

fn valid_email(e: &str) -> bool {
    let parts: Vec<&str> = e.split('@').collect();
    parts.len() == 2 && !parts[0].is_empty()
        && parts[1].contains('.') && !parts[1].starts_with('.') && !parts[1].ends_with('.')
}

/// A safe `#rrggbb` colour, or the first starter hue if the input isn't a valid hex (prevents a
/// stored-XSS payload from reaching `style="background:…"` on the client).
fn sanitize_color(c: &str) -> String {
    let c = c.trim();
    if crate::config::valid_hex_color(c) { c.to_string() }
    else { HUES.first().copied().unwrap_or("#b5524a").to_string() }
}

fn gen_code() -> String { format!("{:06}", rand::thread_rng().gen_range(0..1_000_000u32)) }

// ---- Per-IP rate limiter (fixed 1-minute window) -----------------------------------------------

/// Soft cap on tracked IPs: past this, stale windows are evicted before a new IP is admitted —
/// nothing else prunes this map (it lives in axum state, outside the World and `api::gc`), so
/// without the sweep a spoofed-IP flood would grow it forever.
const MAX_TRACKED_IPS: usize = 4096;

#[derive(Clone, Default)]
pub struct RateLimiter {
    inner: Arc<Mutex<HashMap<String, (u64, u32)>>>, // ip -> (window_start_ms, count)
}

impl RateLimiter {
    /// `true` = allowed. Counts this request in the caller's 1-minute window.
    pub fn check(&self, ip: &str) -> bool {
        let now = current_ms();
        let mut g = self.inner.lock();
        if g.len() >= MAX_TRACKED_IPS && !g.contains_key(ip) {
            g.retain(|_, (start, _)| now.saturating_sub(*start) <= 60_000);
        }
        let e = g.entry(ip.to_string()).or_insert((now, 0));
        if now.saturating_sub(e.0) > 60_000 { *e = (now, 0); }
        e.1 += 1;
        e.1 <= auth_rate_per_min()
    }
}

// ---- HTTP request bodies (camelCase to match the JS landing page) ------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterBody {
    handle: String,
    email: String,
    #[serde(default)] phone: String,
    password: String,
    #[serde(default)] color: Option<String>,
    #[serde(default)] hue_idx: Option<i32>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyBody { reg_id: String, code: String }
#[derive(Deserialize)]
pub struct LoginBody { ident: String, password: String }
#[derive(Deserialize)]
pub struct ForgotBody { email: String }
#[derive(Deserialize)]
pub struct ResetBody { token: String, password: String }
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResendBody { reg_id: String }

// ---- HTTP handlers (tokio runtime; never touch the World write lock) ---------------------------

async fn call_sim(app: &AppState, op: AuthOp) -> AuthOutcome {
    let (tx, rx) = oneshot::channel();
    if app.cmd_tx.send(Cmd::AuthApi { op, reply: tx }).is_err() {
        return AuthOutcome::Error { msg: "Server unavailable".into() };
    }
    rx.await.unwrap_or(AuthOutcome::Error { msg: "Server unavailable".into() })
}

pub async fn register(State(app): State<AppState>, headers: HeaderMap, Json(b): Json<RegisterBody>) -> Response {
    if !origin_allowed(&headers) { return reject_csrf(); }
    if !app.rate.check(&client_ip(&headers)) { return reject_rate(); }
    let op = AuthOp::Register {
        handle: b.handle, email: b.email, phone: b.phone, password: b.password,
        color: b.color.unwrap_or_default(), hue_idx: b.hue_idx.unwrap_or(0),
    };
    match call_sim(&app, op).await {
        AuthOutcome::RegPending { reg_id, email, email_code, phone, phone_code, phone_required } => {
            crate::email::send_code(&email, &email_code).await;
            if !phone.is_empty() { crate::sms::send_code(&phone, &phone_code).await; }
            Json(json!({ "ok": true, "regId": reg_id, "phoneRequired": phone_required })).into_response()
        }
        AuthOutcome::RegExisting { email, reg_id, phone_required } => {
            // Same response shape as RegPending; email the existing owner instead of a code.
            crate::email::send_register_exists_notice(&email).await;
            Json(json!({ "ok": true, "regId": reg_id, "phoneRequired": phone_required })).into_response()
        }
        AuthOutcome::Error { msg } => bad(&msg),
        _ => bad("Unexpected response"),
    }
}

/// Re-send the email verification code for a pending registration. Same guards as register/verify;
/// the per-IP limiter plus the client-side cooldown bound abuse (a resend is a cost amplifier).
pub async fn resend_code(State(app): State<AppState>, headers: HeaderMap, Json(b): Json<ResendBody>) -> Response {
    if !origin_allowed(&headers) { return reject_csrf(); }
    if !app.rate.check(&client_ip(&headers)) { return reject_rate(); }
    match call_sim(&app, AuthOp::ResendCode { reg_id: b.reg_id }).await {
        AuthOutcome::ResendResult { email, email_code } => {
            crate::email::send_code(&email, &email_code).await;
            Json(json!({ "ok": true })).into_response()
        }
        AuthOutcome::Error { msg } => bad(&msg),
        _ => bad("Unexpected response"),
    }
}

pub async fn verify_email(State(app): State<AppState>, headers: HeaderMap, Json(b): Json<VerifyBody>) -> Response {
    if !origin_allowed(&headers) { return reject_csrf(); }
    if !app.rate.check(&client_ip(&headers)) { return reject_rate(); }  // throttle code brute-force
    finish_verify(&app, call_sim(&app, AuthOp::VerifyEmail { reg_id: b.reg_id, code: b.code }).await)
}
pub async fn verify_phone(State(app): State<AppState>, headers: HeaderMap, Json(b): Json<VerifyBody>) -> Response {
    if !origin_allowed(&headers) { return reject_csrf(); }
    if !app.rate.check(&client_ip(&headers)) { return reject_rate(); }
    finish_verify(&app, call_sim(&app, AuthOp::VerifyPhone { reg_id: b.reg_id, code: b.code }).await)
}

fn finish_verify(app: &AppState, outcome: AuthOutcome) -> Response {
    match outcome {
        AuthOutcome::Verified { uid, handle } => {
            let token = app.sessions.mint(uid, &handle);
            json_with_session(&token, json!({ "ok": true, "done": true, "handle": handle }))
        }
        AuthOutcome::VerifyProgress { email_ok, phone_ok, phone_required } =>
            Json(json!({ "ok": true, "done": false,
                "emailVerified": email_ok, "phoneVerified": phone_ok, "phoneRequired": phone_required })).into_response(),
        AuthOutcome::Error { msg } => bad(&msg),
        _ => bad("Unexpected response"),
    }
}

pub async fn login(State(app): State<AppState>, headers: HeaderMap, Json(b): Json<LoginBody>) -> Response {
    if !origin_allowed(&headers) { return reject_csrf(); }
    if !app.rate.check(&client_ip(&headers)) { return reject_rate(); }
    match call_sim(&app, AuthOp::Login { ident: b.ident, password: b.password }).await {
        AuthOutcome::LoggedIn { uid, handle } => {
            let token = app.sessions.mint(uid, &handle);
            json_with_session(&token, json!({ "ok": true, "handle": handle }))
        }
        AuthOutcome::Error { msg } => bad(&msg),
        _ => bad("Unexpected response"),
    }
}

/// Logout: revoke the server-side session (instant) and clear the cookie. CSRF-guarded since it's a
/// cookie-authenticated state change.
pub async fn logout(State(app): State<AppState>, headers: HeaderMap) -> Response {
    if !origin_allowed(&headers) { return reject_csrf(); }
    if let Some(tok) = crate::server::cookie_value(&headers, session_cookie_name()) {
        app.sessions.revoke(&tok);
    }
    let mut resp = Json(json!({ "ok": true })).into_response();
    if let Ok(hv) = HeaderValue::from_str(&clear_cookie_header()) {
        resp.headers_mut().insert(SET_COOKIE, hv);
    }
    resp
}

pub async fn forgot_password(State(app): State<AppState>, headers: HeaderMap, Json(b): Json<ForgotBody>) -> Response {
    if !origin_allowed(&headers) { return reject_csrf(); }
    if !app.rate.check(&client_ip(&headers)) { return reject_rate(); }
    if let AuthOutcome::ForgotResult { send: Some((to, token)) } =
        call_sim(&app, AuthOp::Forgot { email: b.email }).await
    {
        crate::email::send_reset(&to, &token).await;
    }
    // Always uniform — never reveal whether the email exists.
    Json(json!({ "ok": true })).into_response()
}

pub async fn reset_password(State(app): State<AppState>, headers: HeaderMap, Json(b): Json<ResetBody>) -> Response {
    if !origin_allowed(&headers) { return reject_csrf(); }
    match call_sim(&app, AuthOp::Reset { token: b.token, password: b.password }).await {
        AuthOutcome::ResetOk => Json(json!({ "ok": true })).into_response(),
        AuthOutcome::Error { msg } => bad(&msg),
        _ => bad("Unexpected response"),
    }
}

/// Cached, public queen roster — the FREE fallback for over-cap spectators (and the landing page's
/// "jump between queens" source when not on a live guest WS). `Cache-Control: max-age=5` means even
/// unlimited landing viewers cost ~one query per 5 s. Takes only a short read lock.
pub async fn roster(State(app): State<AppState>) -> impl IntoResponse {
    let body = {
        let w = app.world.read().await;
        let queens: Vec<_> = w.queens.iter().filter(|(_, q)| !q.dead).map(|(&id, q)| {
            let p = w.players.get(&id);
            json!({
                "id": id,
                "name":  p.map(|p| p.username.clone()).unwrap_or_default(),
                "color": p.map(|p| p.color.clone()).unwrap_or_else(|| "#888".into()),
                "level": q.level, "x": q.x, "y": q.y,
            })
        }).collect();
        json!({ "queens": queens }).to_string()
    };
    ([(CONTENT_TYPE, "application/json"), (CACHE_CONTROL, "public, max-age=5")], body)
}

// ---- gem purchasing (Stripe hosted Checkout) ---------------------------------------------------

#[derive(Deserialize)]
pub struct BuyGemsBody { pack: String }

/// `POST /api/buy-gems` — start a Stripe Checkout Session for the logged-in player to buy a gem pack.
/// Cookie-authenticated (the buyer is the session owner, NOT anything in the body). Pricing + gem
/// amount are resolved server-side from `stripe::GEM_PACKS`; the client only names a pack `id`.
/// Returns `{ok:true, url}` to redirect to, or `{ok:false, devMode:true}` when Stripe is unconfigured.
pub async fn buy_gems(State(app): State<AppState>, headers: HeaderMap, Json(b): Json<BuyGemsBody>) -> Response {
    if !origin_allowed(&headers) { return reject_csrf(); }
    if !app.rate.check(&client_ip(&headers)) { return reject_rate(); }

    let Some(uid) = session_uid_from_cookie(&headers, &app.sessions) else {
        return (StatusCode::UNAUTHORIZED, Json(json!({ "ok": false, "error": "Please log in to buy gems" }))).into_response();
    };
    let Some(pack) = crate::stripe::pack(&b.pack) else { return bad("Unknown gem pack"); };

    // Dormant until the secret key is set — never charge, just tell the client payments aren't live.
    if stripe_secret_key().is_none() {
        println!("[stripe:DEV] uid={uid} would buy '{}' ({} gems / {}¢)  \
                  (set HIVE_STRIPE_SECRET_KEY + HIVE_STRIPE_WEBHOOK_SECRET to charge)",
                 pack.id, pack.gems, pack.cents);
        let _ = std::io::Write::flush(&mut std::io::stdout());
        return Json(json!({ "ok": false, "devMode": true })).into_response();
    }

    match crate::stripe::create_checkout_session(uid, pack).await {
        Ok(url) => Json(json!({ "ok": true, "url": url })).into_response(),
        Err(msg) => bad(&msg),
    }
}

/// `POST /api/stripe-webhook` — Stripe calls this on payment events. **Intentionally exempt from the
/// Origin / cookie / rate guards** (Stripe is a server, not a browser, and has no session); the
/// `Stripe-Signature` HMAC is its sole authentication. On a verified `checkout.session.completed`,
/// credit the gems named in the session metadata. Always 200s on a valid signature so Stripe stops
/// retrying; 400s a bad/unsigned request (and credits nothing).
pub async fn stripe_webhook(State(app): State<AppState>, headers: HeaderMap, body: String) -> Response {
    let sig = headers.get("stripe-signature").and_then(|v| v.to_str().ok()).unwrap_or_default();
    let Some(event) = crate::stripe::verify_webhook(body.as_bytes(), sig) else {
        return (StatusCode::BAD_REQUEST, "invalid signature").into_response();
    };

    if event.get("type").and_then(|t| t.as_str()) == Some("checkout.session.completed") {
        let meta = event.pointer("/data/object/metadata");
        let uid  = meta.and_then(|m| m.get("uid")).and_then(|v| v.as_str()).and_then(|s| s.parse::<u32>().ok());
        let gems = meta.and_then(|m| m.get("gems")).and_then(|v| v.as_str()).and_then(|s| s.parse::<u64>().ok());
        // Checkout session id → the idempotency key (rejects webhook replays/retries).
        let session = event.pointer("/data/object/id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if let (Some(uid), Some(gems)) = (uid, gems) {
            // Credit on the sim thread (serialized with the users.json save), like every auth mutation.
            let _ = call_sim(&app, AuthOp::GrantGems { uid, gems, session }).await;
        } else {
            eprintln!("[stripe] completed checkout missing uid/gems metadata — skipped");
        }
    }
    // Acknowledge any other event type too, so Stripe doesn't retry events we don't act on.
    (StatusCode::OK, "ok").into_response()
}

// ---- small helpers -----------------------------------------------------------------------------

fn bad(msg: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "ok": false, "error": msg }))).into_response()
}
fn reject_rate() -> Response {
    (StatusCode::TOO_MANY_REQUESTS, Json(json!({ "ok": false, "error": "Too many requests — slow down" }))).into_response()
}
fn reject_csrf() -> Response {
    (StatusCode::FORBIDDEN, Json(json!({ "ok": false, "error": "bad origin" }))).into_response()
}

/// `Set-Cookie` value for a freshly minted session — hardened (`__Host-` + `Secure`) in prod, plain
/// in http dev. `HttpOnly` keeps the token out of JS entirely (XSS can't read it); `SameSite=Lax`
/// blocks cross-site sends.
fn session_cookie_header(token: &str) -> String {
    let max_age = session_ttl_hours() * 3600;
    let name = session_cookie_name();
    let secure = if secure_cookies() { "; Secure" } else { "" };
    format!("{name}={token}; HttpOnly{secure}; SameSite=Lax; Path=/; Max-Age={max_age}")
}
/// `Set-Cookie` value that immediately clears the session cookie (logout).
fn clear_cookie_header() -> String {
    let name = session_cookie_name();
    let secure = if secure_cookies() { "; Secure" } else { "" };
    format!("{name}=; HttpOnly{secure}; SameSite=Lax; Path=/; Max-Age=0")
}
/// JSON response that also sets the session cookie. The token is delivered ONLY via the `HttpOnly`
/// cookie — never in the JSON body — so client JS (and any XSS) can't read it.
fn json_with_session(token: &str, body: serde_json::Value) -> Response {
    let mut resp = Json(body).into_response();
    if let Ok(hv) = HeaderValue::from_str(&session_cookie_header(token)) {
        resp.headers_mut().insert(SET_COOKIE, hv);
    }
    resp
}

/// Best-effort client IP for rate limiting — honours the reverse proxy's forwarding headers.
fn client_ip(headers: &HeaderMap) -> String {
    headers.get("x-forwarded-for").and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next()).map(|s| s.trim().to_string())
        .or_else(|| headers.get("x-real-ip").and_then(|v| v.to_str().ok()).map(|s| s.to_string()))
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Once the tracked-IP cap is hit, lapsed windows are swept before a new IP is admitted — the
    /// map stays bounded under a spoofed-IP flood (nothing else prunes it: it lives in axum state,
    /// outside the World and `api::gc`). Entries still inside their window must survive the sweep.
    #[test]
    fn rate_limiter_evicts_stale_ips_at_cap() {
        let rl = RateLimiter::default();
        let now = current_ms();
        {
            let mut g = rl.inner.lock();
            for i in 0..MAX_TRACKED_IPS {
                g.insert(format!("10.{}.{}.{}", i >> 16, (i >> 8) & 0xFF, i & 0xFF), (now - 120_000, 5));
            }
        }
        assert!(rl.check("203.0.113.7"), "fresh caller admitted");
        assert_eq!(rl.inner.lock().len(), 1, "all lapsed windows swept; only the fresh caller remains");

        {
            let mut g = rl.inner.lock();
            for i in 0..MAX_TRACKED_IPS {
                g.insert(format!("10.{}.{}.{}", i >> 16, (i >> 8) & 0xFF, i & 0xFF), (now, 1));
            }
        }
        assert!(rl.check("203.0.113.8"));
        assert!(rl.inner.lock().len() > MAX_TRACKED_IPS, "in-window entries survive the sweep");
    }
}
