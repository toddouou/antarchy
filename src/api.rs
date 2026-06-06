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
    http::header::{CACHE_CONTROL, CONTENT_TYPE},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use parking_lot::Mutex;
use rand::Rng;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::oneshot;

use crate::auth::{hash_pw, UserRecord};
use crate::config::{auth_rate_per_min, current_ms, sms_enabled, ADMIN_USERNAME, HUES};
use crate::server::{AppState, Cmd};
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
}

/// The sim thread's reply for an [`AuthOp`]. The HTTP handler turns it into JSON and does any sending.
pub enum AuthOutcome {
    /// Registration accepted; the handler must send the code(s). `email_code`/`phone_code` never reach
    /// the client — only the server console (dev) or the email/SMS provider.
    RegPending { reg_id: String, email: String, email_code: String, phone: String, phone_code: String, phone_required: bool },
    /// A code matched but the account isn't finalized yet (other channel still pending).
    VerifyProgress { email_ok: bool, phone_ok: bool, phone_required: bool },
    /// Account created/active — mint a session token for this identity.
    Verified { uid: u32, handle: String },
    /// Login OK — mint a session token.
    LoggedIn { uid: u32, handle: String },
    /// Forgot-password processed. `send` is `Some((email, token))` only when a real account matched;
    /// the handler still returns a uniform 200 to the client (enumeration-safe).
    ForgotResult { send: Option<(String, String)> },
    ResetOk,
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
    }
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
    if world.auth.users.contains_key(&uname)    { return deny("That handle is taken"); }
    if world.auth.email_taken(&email)           { return deny("An account with that email already exists"); }

    let color = if color.trim().is_empty() {
        HUES.first().copied().unwrap_or("#c0392b").to_string()
    } else { color };

    let email_code = gen_code();
    let phone_code = gen_code();
    let reg_id = crate::session::new_token();
    // Phone verification is required only when SMS is enabled AND a phone was supplied.
    let phone_required = sms_enabled() && !phone.is_empty();

    world.pending_regs.insert(reg_id.clone(), PendingReg {
        email: email.clone(), phone: phone.clone(), handle,
        password_hash: hash_pw(&password), color, hue_idx,
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
    });
    world.auth.save();
    AuthOutcome::Verified { uid: id, handle: pr.handle }
}

fn do_login(world: &mut World, ident: &str, password: &str) -> AuthOutcome {
    let ident = ident.trim();
    // Resolve by email, then phone, then legacy username — owned clones avoid borrow conflicts.
    let rec = world.auth.find_by_email(ident).cloned()
        .or_else(|| world.auth.find_by_phone(ident).cloned())
        .or_else(|| world.auth.users.get(&ident.to_uppercase()).cloned());
    // Uniform "Invalid credentials" whether or not the account exists (no enumeration).
    let Some(rec) = rec else { return deny("Invalid credentials"); };
    if rec.password_hash != hash_pw(password) { return deny("Invalid credentials"); }
    if world.auth.banned.contains(&rec.username) { return deny("This account is banned"); }
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
        u.password_hash = hash_pw(password);
        world.auth.save();
        AuthOutcome::ResetOk
    } else {
        deny("Account not found")
    }
}

/// GC expired pending registrations (>15 min) and reset tokens (>1 h). Called periodically by the
/// sim loop so the transient maps can't grow unbounded.
pub fn gc(world: &mut World) {
    let now = current_ms();
    world.pending_regs.retain(|_, pr| now.saturating_sub(pr.created_ms) < 15 * 60_000);
    world.reset_tokens.retain(|_, rt| now.saturating_sub(rt.created_ms) < 60 * 60_000);
}

fn valid_email(e: &str) -> bool {
    let parts: Vec<&str> = e.split('@').collect();
    parts.len() == 2 && !parts[0].is_empty()
        && parts[1].contains('.') && !parts[1].starts_with('.') && !parts[1].ends_with('.')
}

fn gen_code() -> String { format!("{:06}", rand::thread_rng().gen_range(0..1_000_000u32)) }

// ---- Per-IP rate limiter (fixed 1-minute window) -----------------------------------------------

#[derive(Clone, Default)]
pub struct RateLimiter {
    inner: Arc<Mutex<HashMap<String, (u64, u32)>>>, // ip -> (window_start_ms, count)
}

impl RateLimiter {
    /// `true` = allowed. Counts this request in the caller's 1-minute window.
    pub fn check(&self, ip: &str) -> bool {
        let now = current_ms();
        let mut g = self.inner.lock();
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

// ---- HTTP handlers (tokio runtime; never touch the World write lock) ---------------------------

async fn call_sim(app: &AppState, op: AuthOp) -> AuthOutcome {
    let (tx, rx) = oneshot::channel();
    if app.cmd_tx.send(Cmd::AuthApi { op, reply: tx }).is_err() {
        return AuthOutcome::Error { msg: "Server unavailable".into() };
    }
    rx.await.unwrap_or(AuthOutcome::Error { msg: "Server unavailable".into() })
}

pub async fn register(State(app): State<AppState>, headers: HeaderMap, Json(b): Json<RegisterBody>) -> Response {
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
        AuthOutcome::Error { msg } => bad(&msg),
        _ => bad("Unexpected response"),
    }
}

pub async fn verify_email(State(app): State<AppState>, Json(b): Json<VerifyBody>) -> Response {
    finish_verify(&app, call_sim(&app, AuthOp::VerifyEmail { reg_id: b.reg_id, code: b.code }).await)
}
pub async fn verify_phone(State(app): State<AppState>, Json(b): Json<VerifyBody>) -> Response {
    finish_verify(&app, call_sim(&app, AuthOp::VerifyPhone { reg_id: b.reg_id, code: b.code }).await)
}

fn finish_verify(app: &AppState, outcome: AuthOutcome) -> Response {
    match outcome {
        AuthOutcome::Verified { uid, handle } => {
            let token = app.sessions.mint(uid, &handle);
            Json(json!({ "ok": true, "done": true, "token": token, "handle": handle })).into_response()
        }
        AuthOutcome::VerifyProgress { email_ok, phone_ok, phone_required } =>
            Json(json!({ "ok": true, "done": false,
                "emailVerified": email_ok, "phoneVerified": phone_ok, "phoneRequired": phone_required })).into_response(),
        AuthOutcome::Error { msg } => bad(&msg),
        _ => bad("Unexpected response"),
    }
}

pub async fn login(State(app): State<AppState>, headers: HeaderMap, Json(b): Json<LoginBody>) -> Response {
    if !app.rate.check(&client_ip(&headers)) { return reject_rate(); }
    match call_sim(&app, AuthOp::Login { ident: b.ident, password: b.password }).await {
        AuthOutcome::LoggedIn { uid, handle } => {
            let token = app.sessions.mint(uid, &handle);
            Json(json!({ "ok": true, "token": token, "handle": handle })).into_response()
        }
        AuthOutcome::Error { msg } => bad(&msg),
        _ => bad("Unexpected response"),
    }
}

pub async fn forgot_password(State(app): State<AppState>, headers: HeaderMap, Json(b): Json<ForgotBody>) -> Response {
    if !app.rate.check(&client_ip(&headers)) { return reject_rate(); }
    if let AuthOutcome::ForgotResult { send: Some((to, token)) } =
        call_sim(&app, AuthOp::Forgot { email: b.email }).await
    {
        crate::email::send_reset(&to, &token).await;
    }
    // Always uniform — never reveal whether the email exists.
    Json(json!({ "ok": true })).into_response()
}

pub async fn reset_password(State(app): State<AppState>, Json(b): Json<ResetBody>) -> Response {
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

// ---- small helpers -----------------------------------------------------------------------------

fn bad(msg: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "ok": false, "error": msg }))).into_response()
}
fn reject_rate() -> Response {
    (StatusCode::TOO_MANY_REQUESTS, Json(json!({ "ok": false, "error": "Too many requests — slow down" }))).into_response()
}

/// Best-effort client IP for rate limiting — honours the reverse proxy's forwarding headers.
fn client_ip(headers: &HeaderMap) -> String {
    headers.get("x-forwarded-for").and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next()).map(|s| s.trim().to_string())
        .or_else(|| headers.get("x-real-ip").and_then(|v| v.to_str().ok()).map(|s| s.to_string()))
        .unwrap_or_else(|| "unknown".to_string())
}
