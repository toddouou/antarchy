use std::sync::Arc;
use std::time::{Duration, Instant};
use rayon::prelude::*;

use axum::{
    extract::{State, ws::{Message, WebSocket, WebSocketUpgrade}},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Router,
};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, watch, RwLock};

use rustc_hash::{FxHashMap, FxHashSet};

use crate::config::cfg;
use crate::handlers::handle_message;
use crate::network::{
    build_leaderboard, build_player_info, build_server_stats, ctl_frame, finish_view, get_palette,
    snapshot_view, PrevGrid, RawView, CTL_LEADERBOARD, CTL_ME, CTL_STATS,
};
use crate::simulation::tick_world;
use crate::world::{EgressMeter, World};

pub type WorldState = Arc<RwLock<World>>;

/// Messages pushed from WebSocket tasks → sim loop.
/// WS tasks never lock the World — they just push here.
pub enum Cmd {
    /// Auth (register / login): sim fills in player_id and conn_gen, signals reply.
    Auth {
        raw:     String,
        out_tx:  crate::world::BoundedTx<String>,
        ctl_tx:  crate::world::BoundedTx<Arc<[u8]>>,
        view_tx: watch::Sender<Option<Vec<u8>>>,
        meter:   Arc<EgressMeter>,
        reply:   oneshot::Sender<Option<(u32, u64)>>,
    },
    /// Any non-auth message from an authenticated connection.
    Message { pid: u32, raw: String },
    /// Connection closed — only clears tx if conn_gen still matches.
    Disconnect { pid: u32, conn_gen: u64 },
    /// beta-v2 REST auth (register/verify/login/forgot/reset) from an `/api/*` HTTP handler. Runs on
    /// the sim thread (mutates only `auth` + transient maps), replies via oneshot. No conn senders —
    /// the HTTP handler does any email/SMS send + session mint after the reply.
    AuthApi { op: crate::api::AuthOp, reply: oneshot::Sender<crate::api::AuthOutcome> },
}

pub type CmdTx = mpsc::UnboundedSender<Cmd>;
type CmdRx = mpsc::UnboundedReceiver<Cmd>;

#[derive(Clone)]
pub struct AppState {
    pub world:    WorldState,
    pub cmd_tx:   CmdTx,
    /// beta-v2 session tokens (landing → game handoff). Own lock; never the World lock.
    pub sessions: crate::session::SessionStore,
    /// Per-IP rate limiter for the `/api/*` endpoints.
    pub rate:     crate::api::RateLimiter,
}

static CLIENT_HTML:  &str = include_str!("../public/client.html");
static LANDING_HTML: &str = include_str!("../public/landing.html");

// ---- HTTP handlers --------------------------------------------------------

/// Root `/` — serves the **landing page** on a normal GET, upgrades to a guest-spectator WebSocket
/// when requested. (Login/registration live on the landing page now; the game moved to `/play`.)
async fn root_handler(
    ws_opt: Option<WebSocketUpgrade>,
    headers: HeaderMap,
    State(app): State<AppState>,
) -> Response {
    match ws_opt {
        Some(ws) => upgrade_guarded(ws, &headers, app),
        None     => Html(LANDING_HTML).into_response(),
    }
}

/// `/play` — serves the game client on GET, upgrades to an authed game WebSocket when requested.
async fn play_handler(
    ws_opt: Option<WebSocketUpgrade>,
    headers: HeaderMap,
    State(app): State<AppState>,
) -> Response {
    match ws_opt {
        Some(ws) => upgrade_guarded(ws, &headers, app),
        None     => Html(CLIENT_HTML).into_response(),
    }
}

/// Shared WebSocket-upgrade hardening (OWASP A02/A10): reject disallowed browser Origins (anti-CSWSH)
/// and cap inbound frame/message size before accepting the socket.
fn upgrade_guarded(ws: WebSocketUpgrade, headers: &HeaderMap, app: AppState) -> Response {
    if !origin_allowed(headers) {
        return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
    }
    // Resolve the session cookie → uid here, while we still have the upgrade request's headers. The
    // game socket then authenticates from this server-validated id (the `enter` message), so the token
    // never has to live in client JS.
    let cookie_uid = session_uid_from_cookie(headers, &app.sessions);
    let max = crate::config::ws_max_msg();
    ws.max_message_size(max)
        .max_frame_size(max)
        .on_upgrade(move |socket| handle_ws_connection(socket, app, cookie_uid))
        .into_response()
}

/// A PRESENT `Origin` must be in the allowlist; an ABSENT one (non-browser client / same-origin
/// navigation) is allowed — cross-site WS/CSRF requires a browser, which always sends `Origin`. Shared
/// by the WS upgrade and the `/api/*` CSRF guard.
pub(crate) fn origin_allowed(headers: &HeaderMap) -> bool {
    match headers.get(axum::http::header::ORIGIN) {
        None => true,
        Some(v) => match v.to_str() {
            Ok(o) => {
                let o = o.trim_end_matches('/');
                crate::config::allowed_origins().iter().any(|a| a == o)
            }
            Err(_) => false,
        },
    }
}

/// Extract a cookie value by name from the `Cookie` header (no cookie crate). Matches `name=value`
/// exactly within the `; `-separated list.
pub(crate) fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    raw.split(';').find_map(|part| {
        part.trim().strip_prefix(name)?.strip_prefix('=').map(|v| v.to_string())
    })
}

/// Resolve the session cookie on a request to its (unexpired) user id, or `None`.
fn session_uid_from_cookie(headers: &HeaderMap, sessions: &crate::session::SessionStore) -> Option<u32> {
    let tok = cookie_value(headers, crate::config::session_cookie_name())?;
    sessions.validate(&tok).map(|s| s.user_id)
}

/// `/reset` — the landing page handles the `?token=…` reset flow client-side, so just serve it.
async fn landing_page() -> impl IntoResponse { Html(LANDING_HTML) }

static FAVICON: &[u8] = include_bytes!("../favicon.png");

async fn favicon_handler() -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "image/png")], FAVICON)
}

async fn health_handler(State(app): State<AppState>) -> impl IntoResponse {
    let w = app.world.read().await;

    // Tile RAM + chunk stats (the planet-scale memory metric: watch uniform:dense climb).
    let (chunks, uniform, dense, tile_bytes) = w.tiles.stats();
    let painted = w.tiles.total_tiles();

    // Tick-window durations p50/p99/max (the CPU-budget metric at load).
    let mut ring = w.tick_ms_ring.clone();
    ring.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let pct = |p: f64| -> f32 {
        if ring.is_empty() { return 0.0; }
        let i = ((ring.len() as f64 - 1.0) * p).round() as usize;
        ring[i.min(ring.len() - 1)]
    };
    let connected = w.players.values().filter(|p| !p.npc && !p.guest && p.tx.is_some()).count();

    // Viewport CPU + per-connection memory walls (recorded by the viewport thread, off-lock).
    let dirty_chunk_touches = w.tiles.dirty_chunk_touches();
    let (vc50, vc99, vcmax) = crate::metrics::VIEWPORT_CYCLE_MS.pct();
    let (fv50, fv99, fvmax) = crate::metrics::FINISH_VIEW_MS.pct();
    let prevgrid_mb = (crate::metrics::PREVGRID_BYTES.load(std::sync::atomic::Ordering::Relaxed) as f64
        / (1024.0 * 1024.0) * 100.0).round() / 100.0;

    let body = serde_json::json!({
        "version":   env!("CARGO_PKG_VERSION"),
        "tick":      w.tick,
        "ants":      w.ants.len(),
        "queens":    w.queens.values().filter(|q| !q.dead).count(),
        "players":   w.players.values().filter(|p| !p.npc).count(),
        "connected": connected,
        "uptimeMs":  crate::config::current_ms() - w.started_at,
        "seasonSecs": cfg().season_secs,
        // Tile store
        "tilesPainted":   painted,
        "chunks":         chunks,
        "uniformChunks":  uniform,
        "denseChunks":    dense,
        "tileBytes":      tile_bytes,
        "tileMB":         (tile_bytes as f64 / (1024.0 * 1024.0) * 100.0).round() / 100.0,
        // Tick timing (ms)
        "tickMsP50":  (pct(0.50) * 100.0).round() / 100.0,
        "tickMsP99":  (pct(0.99) * 100.0).round() / 100.0,
        "tickMsMax":  (ring.last().copied().unwrap_or(0.0) * 100.0).round() / 100.0,
        // Viewport delivery timing (ms) + per-connection memory (the CPU/mem walls, Phase 0)
        "viewportCycleMsP50": vc50, "viewportCycleMsP99": vc99, "viewportCycleMsMax": vcmax,
        "finishViewMsP50": fv50, "finishViewMsP99": fv99, "finishViewMsMax": fvmax,
        "prevGridMB": prevgrid_mb,
        "dirtyChunkTouches": dirty_chunk_touches,
    }).to_string();
    (StatusCode::OK, [("Content-Type", "application/json")], body)
}

// ---- WebSocket connection -------------------------------------------------

async fn handle_ws_connection(socket: WebSocket, app: AppState, cookie_uid: Option<u32>) {
    let cmd_tx = app.cmd_tx.clone();
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Bounded outbound channels (OWASP A02/A10 backpressure): a slow/malicious consumer that stops
    // reading has its queue capped at `ws_send_queue`, then further messages are dropped instead of
    // growing server memory. Latest-wins viewport frames ride a separate `watch` slot (already bounded).
    let qcap = crate::config::ws_send_queue();
    // Priority channel: events, confirmations, errors — ordered text.
    let (prio_tx, mut prio_rx) = mpsc::channel::<String>(qcap);
    // Binary control channel (Phase 3): compressed me/leaderboard/stats/region-holders — separate so
    // the heavy text builders ride binary without touching the ~100 raw-text send sites. Only fed when
    // the client negotiated `bin`.
    let (ctl_tx_conn, mut ctl_rx) = mpsc::channel::<Arc<[u8]>>(qcap);
    // Viewport slot: latest-wins — stale snapshots are replaced, never backlogged.
    // watch::Sender is Clone; we keep one here and send one clone per auth to the sim loop.
    let (view_tx_conn, mut view_rx) = watch::channel::<Option<Vec<u8>>>(None);

    // Phase-4 per-connection egress meter: the write task records every billed byte here; the
    // viewport thread reads the rate to decide if this conn is over its EGRESS_CAP_KBPS budget.
    let meter = Arc::new(EgressMeter::default());
    let meter_w = meter.clone();

    // Write task: delivers priority messages immediately; for viewports, only the latest
    // frame is sent — tokio::select! ensures a slow network never blocks event delivery.
    tokio::spawn(async move {
        // Keepalive ping at half the idle window so a live client always pongs in time; the read side
        // closes the socket if no inbound frame (including that pong) arrives within the window.
        let mut ping = tokio::time::interval(
            Duration::from_secs((crate::config::ws_idle_secs() / 2).max(1)));
        loop {
            tokio::select! {
                biased; // text events, then binary control, then keepalive, then viewport (latest-wins)
                msg = prio_rx.recv() => {
                    match msg {
                        Some(m) => {
                            let n = m.len();
                            crate::metrics::record(crate::metrics::kind_for_text_type(quick_msg_type(&m)), n);
                            meter_w.add(n, crate::config::current_ms());
                            if ws_tx.send(Message::Text(m)).await.is_err() { break; }
                        }
                        None    => break,
                    }
                }
                ctl = ctl_rx.recv() => {
                    match ctl {
                        Some(v) => {
                            let n = v.len();
                            crate::metrics::record(crate::metrics::kind_for_bin(v.first().copied().unwrap_or(0xFF)), n);
                            meter_w.add(n, crate::config::current_ms());
                            // watch/mpsc carried an Arc<[u8]>; the WS frame needs an owned Vec.
                            if ws_tx.send(Message::Binary(v.to_vec())).await.is_err() { break; }
                        }
                        None    => break,
                    }
                }
                _ = ping.tick() => {
                    if ws_tx.send(Message::Ping(Vec::new())).await.is_err() { break; }
                }
                result = view_rx.changed() => {
                    match result {
                        Ok(()) => {
                            let v = view_rx.borrow_and_update().clone();
                            if let Some(v) = v {
                                let n = v.len();
                                crate::metrics::record(crate::metrics::kind_for_bin(v.first().copied().unwrap_or(0xFF)), n);
                                meter_w.add(n, crate::config::current_ms());
                                if ws_tx.send(Message::Binary(v)).await.is_err() { break; }
                            }
                        }
                        Err(_) => break, // all senders dropped = disconnected
                    }
                }
            }
        }
    });

    let mut player_id: Option<u32> = None;
    let mut conn_gen:  u64         = 0;
    let mut msg_window = Instant::now();
    let mut msg_count  = 0u32;
    const RATE_LIMIT: u32 = 120;

    let idle = Duration::from_secs(crate::config::ws_idle_secs());
    loop {
        let msg = match tokio::time::timeout(idle, ws_rx.next()).await {
            Ok(Some(Ok(m)))             => m,
            Ok(Some(Err(_))) | Ok(None) => break, // socket error / closed
            Err(_) => {                           // no inbound frame within the idle window (slowloris)
                if let Some(pid) = player_id { println!("[ws-idle] player {pid} timed out"); }
                break;
            }
        };
        let text = match msg {
            Message::Text(t)  => t,
            Message::Close(_) => break,
            _                 => continue,
        };

        // Per-connection rate limit (checked here, before touching World)
        let now = Instant::now();
        if now.duration_since(msg_window) > Duration::from_secs(1) {
            msg_window = now; msg_count = 0;
        }
        msg_count += 1;
        if msg_count > RATE_LIMIT {
            if let Some(pid) = player_id {
                println!("[rate-limit] player {} disconnected", pid);
            }
            break;
        }

        let t = quick_msg_type(&text);

        if t == "register" || t == "login" || t == "session" || t == "enter" || t == "spectate" {
            // All enter the game via the Cmd::Auth plumbing (they need the connection senders + reply).
            // `enter` authenticates from the **cookie** validated at upgrade (`cookie_uid`) — the token
            // never touches client JS. `session` is the transitional token-in-message path. Both route
            // an internal `session-login`. `spectate` creates an ephemeral guest player.
            let raw = if t == "enter" {
                match cookie_uid {
                    Some(uid) => format!(r#"{{"t":"session-login","id":{uid},"bin":1}}"#),
                    None => {
                        let _ = prio_tx.try_send(r#"{"t":"err","msg":"Please log in","code":"session"}"#.to_string());
                        continue;
                    }
                }
            } else if t == "session" {
                match session_token(&text).and_then(|tok| app.sessions.validate(&tok)) {
                    Some(s) => format!(r#"{{"t":"session-login","id":{},"bin":1}}"#, s.user_id),
                    None => {
                        let _ = prio_tx.try_send(r#"{"t":"err","msg":"Session expired — please log in again","code":"session"}"#.to_string());
                        continue;
                    }
                }
            } else { text };
            // Auth: push to cmd queue, await reply (≤ 1 tick = ~20 ms)
            let (reply_tx, reply_rx) = oneshot::channel();
            // Clone the viewport sender so the sim loop can write to this connection's slot
            if cmd_tx.send(Cmd::Auth {
                raw,
                out_tx: crate::world::BoundedTx::new(prio_tx.clone()),
                ctl_tx: crate::world::BoundedTx::new(ctl_tx_conn.clone()),
                view_tx: view_tx_conn.clone(),
                meter: meter.clone(),
                reply: reply_tx,
            }).is_err() { break; }
            if let Ok(Some((pid, gen))) = reply_rx.await {
                player_id = Some(pid);
                conn_gen  = gen;
            }
        } else if let Some(pid) = player_id {
            // Non-auth: fire-and-forget, sim processes at next tick start
            if cmd_tx.send(Cmd::Message { pid, raw: text }).is_err() { break; }
        } else {
            let _ = prio_tx.try_send(r#"{"t":"err","msg":"Not logged in"}"#.to_string());
        }
    }

    // view_tx_conn is dropped here; sim loop will drop p.view_tx on Disconnect,
    // causing write task's view_rx.changed() to return Err → write task exits.
    if let Some(pid) = player_id {
        let _ = cmd_tx.send(Cmd::Disconnect { pid, conn_gen });
    }
}

/// Extract the `token` field from a `{t:"session",token:"…"}` message (full parse — auth is rare).
fn session_token(raw: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(raw).ok()
        .and_then(|v| v.get("token").and_then(|t| t.as_str()).map(String::from))
}

/// Extract the message type string without a full JSON parse.
fn quick_msg_type(raw: &str) -> &str {
    if let Some(pos) = raw.find(r#""t":""#) {
        let start = pos + 5;
        if let Some(end) = raw[start..].find('"') {
            return &raw[start..start + end];
        }
    }
    ""
}

// ---- Sim loop (dedicated blocking OS thread) ------------------------------

pub fn sim_loop(world: WorldState, mut cmd_rx: CmdRx) {
    println!("[sim] starting at {} Hz", cfg().tick_rate);
    let mut next_tick = Instant::now();
    // Wall-clock timer for the periodic world autosave (see below).
    let mut last_save = Instant::now();

    loop {
        // ∥A off-lock save: the fast bincode encode happens under the lock below; the slow gzip +
        // disk write is handed to this slot and run after the lock is released (a detached thread),
        // so the tick is never stalled waiting on the filesystem.
        let mut pending_save: Option<(Vec<u8>, String)> = None;

        // ---- Exclusive tick window ----
        {
            let mut w = world.blocking_write();
            let tick_start = Instant::now();

            // Drain all pending WebSocket commands before ticking.
            // WS tasks never lock World — they push here, we process here.
            loop {
                match cmd_rx.try_recv() {
                    Ok(Cmd::Auth { raw, out_tx, ctl_tx, view_tx, meter, reply }) => {
                        let parsed = match serde_json::from_str::<serde_json::Value>(&raw) {
                            Ok(v) => v,
                            Err(_) => { let _ = reply.send(None); continue; }
                        };
                        let mut pid: Option<u32> = None;
                        handle_message(&mut w, &mut pid, &out_tx, parsed);
                        // Wire up the viewport watch channel + binary control channel + egress meter.
                        if let Some(id) = pid {
                            if let Some(p) = w.players.get_mut(&id) {
                                p.view_tx = Some(view_tx);
                                p.ctl_tx  = Some(ctl_tx);
                                p.egress_meter = Some(meter);
                            }
                        }
                        let result = pid.map(|id| {
                            let gen = w.players.get(&id).map(|p| p.conn_gen).unwrap_or(0);
                            (id, gen)
                        });
                        let _ = reply.send(result);
                    }
                    Ok(Cmd::AuthApi { op, reply }) => {
                        // beta-v2 REST auth: mutate auth + transient maps on the sim thread, reply.
                        let outcome = crate::api::apply(&mut w, op);
                        let _ = reply.send(outcome);
                    }
                    Ok(Cmd::Message { pid, raw }) => {
                        let parsed = match serde_json::from_str::<serde_json::Value>(&raw) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        // Borrow tx out before passing &mut w to handle_message
                        let tx = w.players.get(&pid).and_then(|p| p.tx.clone());
                        if let Some(tx) = tx {
                            let mut pid_mut = Some(pid);
                            handle_message(&mut w, &mut pid_mut, &tx, parsed);
                        }
                        // Any client message may mutate tiles — mark dirty so viewport is sent
                        w.dirty_tick = w.tick;
                    }
                    Ok(Cmd::Disconnect { pid, conn_gen }) => {
                        w.paused_views.remove(&pid); // don't leave a reconnecting player stuck paused
                        w.last_seq.remove(&pid);     // anti-replay: drop per-connection seq state
                        let uname = w.players.get(&pid).map(|p| p.username.clone()).unwrap_or_default();
                        // Snapshot current state for the welcome-back diff (read before the &mut borrow).
                        let q = w.queens.get(&pid);
                        let snap = crate::world::AwaySnapshot {
                            at_ms:    crate::config::current_ms(),
                            tiles:    q.map(|q| q.cached_tiles).unwrap_or(0),
                            kills:    q.map(|q| q.kills).unwrap_or(0),
                            level:    q.map(|q| q.level).unwrap_or(0),
                            army:     w.ant_counts.get(&pid).copied().unwrap_or(0),
                            visited_countries: w.players.get(&pid).map(|p| p.visited_countries.len()).unwrap_or(0),
                            queen_alive: q.map(|q| !q.dead).unwrap_or(false),
                        };
                        if let Some(p) = w.players.get_mut(&pid) {
                            if p.conn_gen == conn_gen {
                                p.tx           = None;
                                p.ctl_tx       = None; // drops sender → write task's ctl_rx.recv() returns None
                                p.view_tx      = None; // drops sender → write task's view_rx.changed() returns Err
                                p.egress_meter = None;
                                p.away         = Some(snap);
                            }
                        }
                        // Guests are ephemeral spectators — fully remove on disconnect (no welcome-back,
                        // no persistence). Their reserved hi-range id makes this unambiguous.
                        if crate::world::is_guest_id(pid) {
                            w.players.remove(&pid);
                        } else {
                            println!("[disconnect] {uname} ({pid})");
                        }
                    }
                    Err(_) => break,  // empty queue
                }
            }

            if !w.paused { tick_world(&mut w); }

            // Daily ant refill check
            let now = crate::config::current_ms();
            let daily = cfg().daily_ants;
            for p in w.players.values_mut() {
                if p.npc || p.tx.is_none() { continue; }
                if p.next_refill > 0 && now >= p.next_refill {
                    p.ants_avail += daily;
                    p.next_refill = now + 24 * 3600 * 1000;
                    if let Some(tx) = &p.tx {
                        let _ = tx.send(serde_json::json!({
                            "t":"event","msg":format!("+{daily} DAILY WORKERS")
                        }).to_string());
                    }
                }
            }

            // beta-v2: GC expired pending registrations + reset tokens (cheap; throttled ~40 s).
            if w.tick % 600 == 0 { crate::api::gc(&mut w); }

            // Season rollover: once uptime exceeds the configured season length, wipe the
            // world and start a fresh season (compaction keeps RAM flat across the churn).
            let season_secs = cfg().season_secs;
            if season_secs > 0 && now.saturating_sub(w.started_at) >= season_secs * 1000 {
                crate::simulation::wipe_world(&mut w);
                w.started_at = now;
                w.broadcast(r#"{"t":"event","msg":"◆ NEW SEASON — WORLD RESET"}"#);
            }

            // Periodic autosave (∥A off-lock): every ~60 s, do only the cheap bincode ENCODE under
            // the lock; the slow gzip + atomic disk write runs off-lock (below) so the viewport
            // thread never parks on the filesystem. Accounts (small JSON) still save under the lock.
            if last_save.elapsed().as_secs() >= 60 {
                let path = cfg().save_file.clone();
                match crate::persist::serialize_world(&w) {
                    Ok(raw) => { w.auth.save(); pending_save = Some((raw, path)); }
                    Err(e)  => eprintln!("[persist] autosave encode failed: {e}"),
                }
                last_save = Instant::now();
            }

            // Record this tick window's duration for /health p50/p99.
            w.record_tick_ms(tick_start.elapsed().as_secs_f32() * 1000.0);
        }

        // ∥A: the lock is now released — write the snapshot to disk on a detached thread so neither
        // the tick nor the viewport thread waits on gzip + fsync.
        if let Some((raw, path)) = pending_save {
            std::thread::spawn(move || {
                if let Err(e) = crate::persist::write_snapshot_bytes(&raw, &path) {
                    eprintln!("[persist] autosave write failed: {e}");
                }
            });
        }

        // Viewport delivery runs on its own OS thread (`viewport_loop`) so heavy tile/fog
        // serialization can never eat into this tick's budget. Keeping this loop pure-tick
        // is what makes the cadence rock-steady → smooth client-side ant interpolation.

        // Fixed-interval scheduler: sleep only if we finished early
        let tick_dur = Duration::from_micros(1_000_000 / cfg().tick_rate.max(1) as u64);
        next_tick += tick_dur;
        let now = Instant::now();
        if now < next_tick {
            std::thread::sleep(next_tick - now);
        }
        // If we overran, next_tick is in the past → skip sleep, catch up immediately
    }
}

/// One client's delivery work for a cycle. Senders are cloned under the read lock so the
/// actual serialize+send (Phase B) needs no World access.
struct ClientJob {
    pid:     u32,
    view_tx: Option<watch::Sender<Option<Vec<u8>>>>,
    tx:      Option<crate::world::BoundedTx<String>>,
    ctl_tx:  Option<crate::world::BoundedTx<Arc<[u8]>>>,
    /// This connection negotiated the Phase-3 binary protocol (ant kind 3, fog-on-keyframes, binary
    /// `me`). Decides the per-client frame encoding in the lock-free phase.
    bin:     bool,
    raw:     Option<RawView>,
    me:      Option<String>,
    /// Per-client tile state (keyframe/delta), moved out of the viewport thread's map for the
    /// lock-free parallel phase and moved back after.
    prev:    Option<PrevGrid>,
}

/// Phase-5 leaderboard change signature: an order-independent (XOR) hash over every live queen's
/// `(id, level, kills)`. Flips when a queen levels up, scores a kill, or joins/dies — i.e. the
/// standings-affecting events — but NOT on pure tile accumulation (a 5 s heartbeat refreshes those).
/// Cheap: one pass over `queens`, no sort/alloc.
fn lb_signature(w: &World) -> u64 {
    const M: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut h = 0u64;
    for (&id, q) in &w.queens {
        if q.dead { continue; }
        let mut x = (id as u64).wrapping_mul(M);
        x = (x ^ q.level as u64).wrapping_mul(M);
        x = (x ^ q.kills as u64).wrapping_mul(M);
        h ^= x;
    }
    h
}

/// Viewport delivery loop — runs on its own OS thread, separate from `sim_loop`.
///
/// Each cycle is two phases:
///   A. Under a **short** read lock: snapshot each client's viewport data + clone its
///      channel senders (`snapshot_view`), plus the cheap `me`/leaderboard strings.
///   B. With **no lock held**: fog transform + base64 + JSON (`finish_view`) in parallel,
///      then push frames to the per-connection channels.
///
/// Because the expensive serialization is out of the lock and off the sim thread, a heavy
/// tile frame can no longer delay a tick — the cadence stays steady, which is what keeps
/// client-side ant interpolation smooth. Cadence is paced on the sim's tick counter:
/// ant frames whenever the tick advances, tile frames ≤ ~10 Hz, `me` ~1 Hz, leaderboard
/// every 20 ticks.
pub fn viewport_loop(world: WorldState, pool: Arc<rayon::ThreadPool>) {
    let mut last_tick:      u64 = u64::MAX;
    let mut last_tile_tick: u64 = 0;
    let mut last_ant_tick:  u64 = 0;
    let mut last_me_tick:   u64 = 0;
    let mut last_lb_tick:   u64 = 0;
    let mut last_lb_sig:    u64 = u64::MAX; // Phase-5: leaderboard send-on-change signature
    let mut last_lb_sent:   u64 = 0;        // tick of the last leaderboard broadcast (force-refresh)
    let mut last_stats_tick: u64 = 0;
    // Per-client tile state for the keyframe/delta protocol. Lives here (not in World) so the
    // sim thread never touches it; the viewport thread is its sole owner.
    let mut prev_grids: FxHashMap<u32, PrevGrid> = FxHashMap::default();

    loop {
        let cycle_start = Instant::now();

        // ---- Phase A: snapshot under a short read lock ----
        let batch: Option<(Vec<ClientJob>, Arc<serde_json::Value>)> = {
            let w = world.blocking_read();
            let tick = w.tick;
            if tick == last_tick {
                None // sim hasn't advanced since last delivery — nothing new to send
            } else {
                let tr = cfg().tick_rate.max(1) as u64;
                let tile_every = (tr / 10).max(1);
                let include_tiles = tick.saturating_sub(last_tile_tick) >= tile_every
                    && w.dirty_tick + tile_every >= tick;
                // Ant-frame cadence (Phase 3B): one ants-only frame every `tr/ant_hz` ticks instead of
                // every tick. ant_hz ≥ tr ⇒ ant_every == 1 (every tick = legacy 50 Hz). The client's
                // grid-aware interpolation fills the wider gaps so motion still reads as crawling.
                let ant_hz     = cfg().ant_hz.max(1) as u64;
                let ant_every  = (tr / ant_hz).max(1);
                let send_ants  = !w.ants.is_empty() && tick.saturating_sub(last_ant_tick) >= ant_every;
                let send_me    = tick.saturating_sub(last_me_tick) >= tr;
                let lb_due     = tick.saturating_sub(last_lb_tick) >= 20;
                let do_clients = include_tiles || send_ants;

                last_tick = tick;

                // Leaderboard (Phase-5 send-on-change): broadcast only when the standings actually
                // change — a new kill, level-up, or queen joining/dying flips the signature — plus a
                // ~5 s heartbeat so the live tile numbers still refresh. This turns a 2.5 Hz × ~16 KB
                // firehose into near-zero steady-state bytes (the big O(connections) residual) while
                // keeping the *exciting* moments instant. The full leaderboard window is unchanged.
                if lb_due {
                    let sig   = lb_signature(&w);
                    let force = tick.saturating_sub(last_lb_sent) >= tr * 5;
                    if sig != last_lb_sig || force {
                        let lb = build_leaderboard(&w);
                        w.broadcast_ctl(Arc::from(ctl_frame(CTL_LEADERBOARD, &lb)), &lb);
                        last_lb_sig  = sig;
                        last_lb_sent = tick;
                    }
                    last_lb_tick = tick;

                    // beta-v2 spectator: the live queen roster (jump targets). Guests get NO queens in
                    // viewport frames (queens ride tile frames, which guests never receive), so they
                    // render queens from this. Sent ONLY to guests, as small text → negligible egress.
                    if w.players.values().any(|p| p.guest) {
                        let roster = crate::network::build_queen_roster(&w);
                        for p in w.players.values() {
                            if p.guest {
                                if let Some(tx) = &p.tx { let _ = tx.send(roster.clone()); }
                            }
                        }
                    }
                }

                // Server stats (~1 Hz) — header bar + admin cards for every client. Small (~150 B);
                // carries tick/uptime so it's not send-on-change-able, but the bytes are negligible.
                if tick.saturating_sub(last_stats_tick) >= tr {
                    let stats = build_server_stats(&w);
                    w.broadcast_ctl(Arc::from(ctl_frame(CTL_STATS, &stats)), &stats);
                    last_stats_tick = tick;

                    // Denial-of-wallet egress alert (OWASP A09, opt-in): flag any connection over the
                    // per-conn KB/s ceiling. ~1 Hz, off the hot path; off by default (the WS layer is
                    // already bounded by EGRESS_CAP_KBPS / guest knobs).
                    let ealert = crate::config::egress_alert_kbps();
                    if ealert > 0.0 {
                        let now_ms = crate::config::current_ms();
                        for (&id, p) in &w.players {
                            if let Some(kb) = p.egress_meter.as_ref().map(|m| m.kbps(now_ms)) {
                                if kb > ealert {
                                    println!("[egress-alert] conn {id} at {kb:.0} KB/s exceeds {ealert:.0} KB/s");
                                }
                            }
                        }
                    }
                }

                if !do_clients && !send_me {
                    None
                } else {
                    if include_tiles { last_tile_tick = tick; }
                    // Tile frames carry ants in their header too, so they also reset the ant clock —
                    // prevents an immediate redundant ants-only push right after a tile frame.
                    if send_ants || include_tiles { last_ant_tick = tick; }
                    if send_me       { last_me_tick = tick; }

                    let pids: Vec<u32> = w.players.iter()
                        .filter(|(&id, p)| !p.npc && !w.paused_views.contains(&id)
                            && (p.tx.is_some() || p.view_tx.is_some()))
                        .map(|(&id, _)| id)
                        .collect();
                    // Palette (owner→colour) is consulted ONLY by tile frames; on ants-only cycles
                    // finish_view never touches it, so skip the O(players) map build then.
                    let palette = Arc::new(if include_tiles { get_palette(&w) } else { serde_json::Value::Null });
                    // Precompute owner→ant-list ONCE per me-cycle (capped 120/owner) so each player's
                    // `me` reads its own slice instead of rescanning the whole ant vec — that was
                    // O(players × total_ants) under this read lock at 1 Hz.
                    let ants_by_owner: Option<FxHashMap<u32, Vec<serde_json::Value>>> = if send_me {
                        let mut m: FxHashMap<u32, Vec<serde_json::Value>> = FxHashMap::default();
                        for a in &w.ants {
                            let e = m.entry(a.owner).or_default();
                            if e.len() < 120 {
                                e.push(serde_json::json!([a.id, a.lifespan.saturating_sub(a.age), a.kind]));
                            }
                        }
                        Some(m)
                    } else { None };
                    // Phase-4 per-conn cap: when EGRESS_CAP_KBPS is set, a connection already sending
                    // faster than the cap skips this cycle's heavy viewport frame (the watch slot
                    // coalesces, so it just gets fewer frames). Dormant by default (cap = None).
                    // Never affects `me` or the NEVER-DROP one-shots on tx/ctl_tx.
                    let player_cap = crate::config::egress_cap_kbps();
                    let guest_cap  = crate::config::guest_egress_kbps();
                    let now_ms = crate::config::current_ms();
                    let wref: &World = &w;
                    let jobs: Vec<ClientJob> = pool.install(|| pids.par_iter().map(|&pid| {
                        let p = wref.players.get(&pid);
                        let is_guest = p.map(|p| p.guest).unwrap_or(false);
                        // EGRESS GUARD: guests NEVER receive tile frames — their territory renders from
                        // free R2 super-tiles. Force ants-only for guests regardless of the cycle, so
                        // the heavy ownership-grid bytes only ever ship to real (authed) players.
                        let it = include_tiles && !is_guest;
                        // Guests bill against the dedicated (tighter) cap; players against EGRESS_CAP_KBPS.
                        let cap = if is_guest { Some(guest_cap) } else { player_cap };
                        let over_cap = cap.is_some_and(|c|
                            p.and_then(|p| p.egress_meter.as_ref()).is_some_and(|m| m.kbps(now_ms) > c));
                        ClientJob {
                            pid,
                            view_tx: p.and_then(|p| p.view_tx.clone()),
                            tx:      p.and_then(|p| p.tx.clone()),
                            ctl_tx:  p.and_then(|p| p.ctl_tx.clone()),
                            bin:     p.map(|p| p.bin).unwrap_or(false),
                            raw:     if do_clients && !over_cap { snapshot_view(wref, pid, it) } else { None },
                            // Periodic `me` carries only the DYNAMIC fields (full=false); the static
                            // cfg/geo/world/spawn block ships once in `logged-in`.
                            me:      if send_me { Some(build_player_info(wref, pid, false, ants_by_owner.as_ref())) } else { None },
                            prev:    None,
                        }
                    }).collect());
                    Some((jobs, palette))
                }
            }
        };

        // ---- Phase B: serialize + send, no lock held ----
        if let Some((mut jobs, palette)) = batch {
            // Attach each client's retained tile state, then drop grids for clients that are no
            // longer connected (pids = the full connected set whenever a batch is produced).
            let pid_set: FxHashSet<u32> = jobs.iter().map(|j| j.pid).collect();
            for j in jobs.iter_mut() { j.prev = prev_grids.remove(&j.pid); }
            prev_grids.retain(|k, _| pid_set.contains(k));

            type Out = (
                u32,
                Option<watch::Sender<Option<Vec<u8>>>>,
                Option<Vec<u8>>,
                Option<crate::world::BoundedTx<String>>,
                Option<crate::world::BoundedTx<Arc<[u8]>>>,
                bool,
                Option<String>,
                Option<PrevGrid>,
            );
            let fv_start = Instant::now();
            let results: Vec<Out> = pool.install(move || jobs.into_par_iter().map(|job| {
                let (frame, new_prev) = match &job.raw {
                    Some(r) => { let (f, np) = finish_view(r, palette.as_ref(), job.prev, job.bin); (Some(f), np) }
                    None    => (None, job.prev),
                };
                (job.pid, job.view_tx, frame, job.tx, job.ctl_tx, job.bin, job.me, new_prev)
            }).collect());
            crate::metrics::FINISH_VIEW_MS.record(fv_start.elapsed().as_secs_f32() * 1000.0);

            for (pid, view_tx, frame, tx, ctl_tx, bin, me, new_prev) in results {
                // Tile/ant frame → viewport watch slot (latest-wins, never backlogged)
                if let Some(frame) = frame {
                    if let Some(vtx) = &view_tx { let _ = vtx.send(Some(frame)); }
                }
                // me update → binary control channel (kind 16) for `bin` clients, else raw text.
                if let Some(me) = &me {
                    let sent_bin = bin
                        && ctl_tx.as_ref().map(|c| c.send(Arc::from(ctl_frame(CTL_ME, me))).is_ok()).unwrap_or(false);
                    if !sent_bin {
                        if let Some(tx) = &tx { let _ = tx.send(me.clone()); }
                    }
                }
                // Stash updated tile state for next cycle's delta.
                if let Some(np) = new_prev { prev_grids.insert(pid, np); }
            }

            // Phase-0 metrics: per-connection retained memory + whole-cycle delivery cost.
            let pg_bytes: u64 = prev_grids.values().map(|g| g.est_bytes() as u64).sum();
            crate::metrics::PREVGRID_BYTES.store(pg_bytes, std::sync::atomic::Ordering::Relaxed);
            crate::metrics::VIEWPORT_CYCLE_MS.record(cycle_start.elapsed().as_secs_f32() * 1000.0);
        }

        // Pace at up to ~120 Hz; never busy-spin when the sim is idle/slow.
        let el = cycle_start.elapsed();
        let min_cycle = Duration::from_millis(8);
        if el < min_cycle { std::thread::sleep(min_cycle - el); }
    }
}

// ---- Phase 6: R2 snapshot writer (own OS thread, only spawned when a sink is configured) -------
//
// Every interval: under a brief WRITE lock drain the dirty-chunk set + capture epoch + the
// owner→color map (all cheap); then under a READ lock snapshot those chunks to owner-id form
// (shared with the viewport thread, so it doesn't block the tick); then OFF-LOCK rasterize each to
// a PNG and upload to the sink (R2 / local disk). Game-space key: snap/{epoch}/0/{cx}/{cy}.png.
pub fn snapshot_writer_loop(world: WorldState) {
    let sink = crate::snapshot::make_sink();
    let secs = crate::config::snapshot_interval_secs();
    let s = crate::config::snapshot_tile_chunks();
    let interval = Duration::from_secs(secs);
    println!("[snapshot] writer started (sink: {}, interval {secs}s, super-tile S={s} → {}px)",
             sink.label(), s * 256);
    let mut last_class_a = crate::metrics::r2_ops().0; // denial-of-wallet watch baseline
    // Content-dedup state (R2 Class-A lever): last-uploaded PNG hash per super-tile key, within the
    // current epoch. A dirty super-tile whose rendered bytes match its last upload costs no PutObject.
    // Process-local: empty on boot, so the first post-restart cycle re-uploads the canvas once (which
    // is the intended `mark_all_dirty` behaviour); cleared on epoch roll so a post-wipe tile at the
    // same (sx,sy) — a brand-new R2 object under the new epoch path — is never wrongly skipped.
    let mut last_hash: FxHashMap<(u32, u32), u64> = FxHashMap::default();
    let mut last_epoch: u64 = 0;
    loop {
        std::thread::sleep(interval);

        // Phase A1 (write lock, brief): drain the dirty keys + the wipe retire-queue, snapshot
        // epoch + colors.
        let (epoch, keys, retire, colors): (u64, Vec<u64>, Vec<u64>, FxHashMap<u32, [u8; 3]>) = {
            let mut w = world.blocking_write();
            let keys = w.tiles.drain_dirty_chunks();
            let retire = std::mem::take(&mut w.snapshot_retire);
            let mut colors = FxHashMap::default();
            for (&id, p) in &w.players {
                colors.insert(id, crate::snapshot::parse_hex_color(&p.color));
            }
            (w.epoch, keys, retire, colors)
        };

        // Epoch rolled (wipe/season) → the dedup cache keyed by (sx,sy) refers to the OLD epoch's
        // objects; drop it so the new epoch's tiles all upload at least once.
        if epoch != last_epoch { last_hash.clear(); last_epoch = epoch; }

        // Wipe cleanup: a rolled epoch orphaned its whole tile generation on R2 — delete the prefix.
        // Run off-lock, and before the empty-keys early-out so a wipe that left no dirty chunks still
        // reclaims space. Race-free: this thread already finished any in-flight old-epoch uploads in a
        // prior cycle, and the epoch has rolled, so nothing re-creates these keys.
        for old in retire {
            let prefix = format!("snap/{old}/");
            match sink.delete_prefix(&prefix) {
                Ok(n)  => println!("[snapshot] wipe cleanup: deleted {n} objects under {prefix}"),
                Err(e) => eprintln!("[snapshot] wipe cleanup {prefix} failed: {e}"),
            }
        }

        if keys.is_empty() { continue; }

        // Phase-6 lever B: coalesce the dirty chunks → distinct super-tile keys (S×S chunk blocks).
        // This is where the R2 Class-A reduction happens: many sub-chunks of one contiguous frontier
        // collapse to a single key/PutObject. (S=1 → one super-tile per chunk = legacy behavior.)
        let super_keys: Vec<(u32, u32)> = {
            let mut set: FxHashSet<(u32, u32)> = FxHashSet::default();
            for &k in &keys {
                let (cx, cy) = crate::tile_map::TileMap::chunk_coords(k);
                set.insert((cx / s, cy / s));
            }
            set.into_iter().collect()
        };

        // Phase A2 (read lock, shared with viewport): clone each super-tile's chunks to owner-id form.
        let snaps = {
            let w = world.blocking_read();
            crate::snapshot::snapshot_supertiles(&w.tiles, &super_keys, s)
        };

        // Phase B (no lock): rasterize + upload painted super-tiles; delete ones that went fully empty.
        // A super-tile that lost a (dead) owner but still has any painted cell re-renders via `put`
        // (the dead owner's cells go transparent, survivors intact). Only a super-tile whose every
        // constituent chunk is empty is `all_empty` → its tile is deleted so R2 tracks live territory.
        let start = Instant::now();
        let (mut ok, mut skip, mut del, mut fail) = (0u32, 0u32, 0u32, 0u32);
        for snap in &snaps {
            let key = format!("snap/{epoch}/0/{}/{}.png", snap.sx, snap.sy);
            if snap.all_empty {
                // Fully-cleared block → delete the key (free op) so R2 tracks live territory; forget
                // its hash so a future repaint to identical bytes still re-uploads.
                match sink.delete(&key) {
                    Ok(())  => { del += 1; last_hash.remove(&(snap.sx, snap.sy)); }
                    Err(e)  => { fail += 1; if fail <= 3 { eprintln!("[snapshot] {key} delete failed: {e}"); } }
                }
                continue;
            }
            let png = crate::snapshot::rasterize_supertile(snap, &colors, s);
            // Content-dedup: the owner-change-precise dirty set still flags net-no-op oscillations
            // (a cell A→B→A within one window) as dirty. Hash the rendered bytes and skip the
            // PutObject when they're byte-identical to the last upload for this key — a free Class-A
            // save. Hashing the PNG (not the owner-ids) also catches recolours, since a `set-color`
            // changes the pixels without changing ownership.
            let h = {
                use std::hash::{Hash, Hasher};
                let mut hh = rustc_hash::FxHasher::default();
                png.hash(&mut hh);
                hh.finish()
            };
            if last_hash.get(&(snap.sx, snap.sy)) == Some(&h) { skip += 1; continue; }
            match sink.put(&key, &png) {
                Ok(())  => { ok += 1; last_hash.insert((snap.sx, snap.sy), h); }
                Err(e)  => { fail += 1; if fail <= 3 { eprintln!("[snapshot] {key} failed: {e}"); } }
            }
        }
        if skip > 0 { crate::metrics::record_r2_skipped(skip as u64); }
        println!("[snapshot] epoch {epoch}: uploaded {ok}, skipped {skip} (dedup), deleted {del} ({fail} failed) in {:?}",
                 start.elapsed());

        // Denial-of-wallet watch (OWASP A09): surface a Class-A spike before the invoice does.
        let alert = crate::config::classa_alert_per_min();
        if alert > 0 {
            let ca_now = crate::metrics::r2_ops().0;
            let delta = ca_now.saturating_sub(last_class_a);
            last_class_a = ca_now;
            let per_min = delta as f64 / (secs as f64 / 60.0).max(1.0 / 60.0);
            if per_min > alert as f64 {
                eprintln!("[egress-alert] R2 Class-A {per_min:.0}/min exceeds {alert}/min \
                           ({delta} ops in {secs}s) — check for a write loop / wipe-storm");
            }
        }
    }
}

// ---- Server startup -------------------------------------------------------

async fn world_info_handler(State(app): State<AppState>) -> impl IntoResponse {
    // Read the world epoch BEFORE taking the cfg guard — a non-Send RwLockReadGuard must not be
    // held across the `.await` (it would make the handler future non-Send).
    let epoch = app.world.read().await.epoch;
    let c = cfg();
    let body = serde_json::json!({
        "worldW":     c.world_w,
        "worldH":     c.world_h,
        "spawnX":     c.spawn_x,
        "spawnY":     c.spawn_y,
        "capitolLat": c.capitol_lat,
        "capitolLon": c.capitol_lon,
        "tileMeters": c.tile_meters,
        "epoch":        epoch,
        "snapshotBase": crate::config::snapshot_public_base(),
        "basemapUrl":   crate::config::basemap_url(),
        "snapTileCells": crate::config::snapshot_tile_chunks() * 256,
    }).to_string();
    (StatusCode::OK, [("Content-Type", "application/json")], body)
}

/// Per-kind egress accounting + the CPU/memory walls — the data behind the budget ledger
/// (EGRESS_PLAN.md §5). Bytes are tallied at the billed `ws_tx.send` sites (`metrics::record`);
/// rates are over server uptime. Additive: remove this route + the `metrics::record` calls to
/// fully revert Phase 0.
async fn egress_stats_handler(State(app): State<AppState>) -> impl IntoResponse {
    let (uptime_ms, connected, dirty, dirty_pending, dirty_supers) = {
        let w = app.world.read().await;
        (
            crate::config::current_ms().saturating_sub(w.started_at),
            w.players.values().filter(|p| !p.npc && p.tx.is_some()).count(),
            w.tiles.dirty_chunk_touches(),
            w.tiles.dirty_chunk_len(),
            w.tiles.dirty_supertiles_len(crate::config::snapshot_tile_chunks()),
        )
    };
    let secs = (uptime_ms as f64 / 1000.0).max(1.0);

    let per = crate::metrics::per_kind();
    let mut total_bytes = 0u64;
    let mut total_msgs  = 0u64;
    let mut by_kind = serde_json::Map::new();
    for i in 0..crate::metrics::KIND_COUNT {
        let (b, m) = per[i];
        total_bytes += b;
        total_msgs  += m;
        by_kind.insert(crate::metrics::KIND_NAMES[i].to_string(), serde_json::json!({
            "bytes": b,
            "msgs":  m,
            "bytesPerSec": (b as f64 / secs).round() as u64,
        }));
    }

    let (vc50, vc99, vcmax) = crate::metrics::VIEWPORT_CYCLE_MS.pct();
    let (fv50, fv99, fvmax) = crate::metrics::FINISH_VIEW_MS.pct();
    let pg = crate::metrics::PREVGRID_BYTES.load(std::sync::atomic::Ordering::Relaxed);
    let per_viewer = if connected > 0 {
        (total_bytes as f64 / secs / connected as f64).round() as u64
    } else { 0 };
    // R2 op budget (denial-of-wallet): cumulative Class-A/Class-B/delete counts + the Class-A rate.
    let (r2a, r2b, r2del) = crate::metrics::r2_ops();

    let body = serde_json::json!({
        "uptimeMs":             uptime_ms,
        "connected":            connected,
        "totalBytes":           total_bytes,
        "totalMsgs":            total_msgs,
        "headerBytesEst":       crate::metrics::header_bytes(),
        "totalBytesPerSec":     (total_bytes as f64 / secs).round() as u64,
        "bytesPerSecPerViewer": per_viewer,
        "byKind":               serde_json::Value::Object(by_kind),
        "dirtyChunkTouches":    dirty,
        "dirtyChunksPending":   dirty_pending,
        "dirtySupertilesLen":   dirty_supers,
        "viewportCycleMs":      {"p50": vc50, "p99": vc99, "max": vcmax},
        "finishViewMs":         {"p50": fv50, "p99": fv99, "max": fvmax},
        "prevGridBytes":        pg,
        "prevGridMB":           (pg as f64 / (1024.0 * 1024.0) * 100.0).round() / 100.0,
        "r2ClassA":             r2a,
        "r2ClassB":             r2b,
        "r2Deletes":            r2del,
        "r2Skipped":            crate::metrics::r2_skipped(),
        "r2ClassAPerMin":       (r2a as f64 / (secs / 60.0)).round() as u64,
    }).to_string();
    (StatusCode::OK, [("Content-Type", "application/json")], body)
}

/// Content-Security-Policy (OWASP A05, defence-in-depth). Scoped to our own origins plus the external
/// services the pages legitimately use — Google Fonts, AdSense (landing), OSM/R2 map tiles, and the
/// game WebSocket. `'unsafe-inline'` is required because the client is a single inline-script document;
/// a nonce/extraction pass to drop it is a documented P1/P2 follow-up. Caddy is the prod enforcement
/// layer and may tighten this further.
const CSP: &str = "default-src 'self'; base-uri 'self'; object-src 'none'; frame-ancestors 'none'; \
img-src 'self' data: blob: https:; font-src 'self' data: https://fonts.gstatic.com; \
style-src 'self' 'unsafe-inline' https://fonts.googleapis.com; \
script-src 'self' 'unsafe-inline' https://pagead2.googlesyndication.com https://*.googlesyndication.com https://*.google.com https://*.googleadservices.com https://*.doubleclick.net; \
connect-src 'self' ws: wss: https:; \
frame-src https://*.googlesyndication.com https://*.doubleclick.net https://*.google.com";

/// Attach the static security headers (OWASP A02/A05) to every response. HSTS is a no-op over plain
/// http (browsers ignore it), so it's safe to always send and active once behind Caddy's TLS.
async fn security_headers(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("strict-origin-when-cross-origin"));
    h.insert(header::STRICT_TRANSPORT_SECURITY, HeaderValue::from_static("max-age=31536000; includeSubDomains"));
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    resp
}

pub async fn run(world: WorldState, cmd_tx: CmdTx) {
    let port = cfg().port;

    let app = Router::new()
        .route("/",           get(root_handler))        // landing page (GET) / guest spectator (WS)
        .route("/play",       get(play_handler))        // game client (GET) / authed game (WS)
        .route("/reset",      get(landing_page))        // password-reset lands here (?token=…)
        .route("/favicon.png", get(favicon_handler))
        .route("/health",     get(health_handler))
        .route("/world-info", get(world_info_handler))
        .route("/egress-stats", get(egress_stats_handler))
        // ---- beta-v2 REST auth (landing page calls these) ----
        .route("/api/register",        post(crate::api::register))
        .route("/api/verify-email",    post(crate::api::verify_email))
        .route("/api/verify-phone",    post(crate::api::verify_phone))
        .route("/api/login",           post(crate::api::login))
        .route("/api/forgot-password", post(crate::api::forgot_password))
        .route("/api/reset-password",  post(crate::api::reset_password))
        .route("/api/logout",          post(crate::api::logout))
        .route("/api/roster",          get(crate::api::roster)) // cached spectator fallback (free path)
        .layer(axum::middleware::from_fn(security_headers))
        .with_state(AppState {
            world, cmd_tx,
            sessions: crate::session::SessionStore::load(),
            rate:     crate::api::RateLimiter::default(),
        });

    let addr = format!("0.0.0.0:{port}");
    let listener = TcpListener::bind(&addr).await
        .unwrap_or_else(|e| panic!("Cannot bind {addr}: {e}"));

    println!("Listening on http://localhost:{port}");
    axum::serve(listener, app).await.unwrap();
}

// ---- Egress measurement harness (Phase 1) --------------------------------
//
// In-process load harness: boots the FULL stack (sim + viewport + axum WS) on an EPHEMERAL port
// (never :8080/:8090), seeds accounts in-memory (no disk writes), spawns an NPC swarm, then
// attaches K real `tokio-tungstenite` viewers and measures per-viewer egress + the CPU/memory
// walls as connections scale 6→300. Run (against the isolated target, never the live server):
//   CARGO_TARGET_DIR=target-dev cargo test --release bench_egress -- --ignored --nocapture
//
// Viewers are seeded as ADMIN accounts so they receive an UN-FOGGED viewport — the "densest active
// war, single viewer" worst case (every in-rect ant shipped), i.e. a conservative UPPER BOUND on
// per-viewer egress for the budget ledger (§5). The acceptance gate is **tick p99 staying flat**
// as K grows: it proves Phase-2 rayon-pool isolation (viewer serialization can't starve the tick).
#[cfg(test)]
mod egress_bench {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};
    use tokio::net::TcpListener;
    use tokio::sync::{mpsc, RwLock};
    use axum::{routing::get, Router};
    use futures_util::{SinkExt, StreamExt};
    use serde_json::json;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    use super::{root_handler, sim_loop, viewport_loop, AppState, Cmd};
    use crate::auth::{hash_pw, UserRecord};
    use crate::world::World;

    fn p99(mut v: Vec<f32>) -> f32 {
        if v.is_empty() { return 0.0; }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        v[(((v.len() - 1) as f64) * 0.99).round() as usize]
    }

    /// One viewer: connect, login (seeded ADMIN account → un-fogged view), set a viewport over the
    /// swarm, then tally every received payload byte until the task is aborted at sweep end.
    async fn run_viewer(port: u16, user: String, view: (i32, i32, i32, i32), bytes: Arc<AtomicU64>) {
        let url = format!("ws://127.0.0.1:{port}/");
        let Ok((ws, _)) = connect_async(url.as_str()).await else { return };
        let (mut tx, mut rx) = ws.split();
        let _ = tx.send(WsMessage::Text(json!({"t":"login","username":user,"password":"x","bin":1}).to_string())).await;
        let (x0, y0, x1, y1) = view;
        let _ = tx.send(WsMessage::Text(json!({"t":"view-set","x0":x0,"y0":y0,"x1":x1,"y1":y1}).to_string())).await;
        let _keep = tx; // hold the sink open for the connection's lifetime
        while let Some(Ok(msg)) = rx.next().await {
            bytes.fetch_add(msg.len() as u64, Ordering::Relaxed);
        }
    }

    #[test]
    #[ignore]
    fn bench_egress_conn_sweep() {
        // Isolate any disk writes away from live data (belt-and-suspenders; the login path here
        // never calls auth.save(), but persist/auth derive their paths from HIVE_DATA_DIR).
        let datadir = std::env::temp_dir().join("hive-egress-bench");
        let _ = std::fs::create_dir_all(&datadir);
        std::env::set_var("HIVE_DATA_DIR", datadir.to_string_lossy().to_string());

        crate::regions::init();
        let world = Arc::new(RwLock::new(World::new()));
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Cmd>();

        const MAX_VIEWERS: usize = 300;
        // Seed viewer accounts in memory as ADMINS (un-fogged worst-case viewport). No auth.save().
        {
            let mut w = world.blocking_write();
            for i in 0..MAX_VIEWERS {
                let uname = format!("V{i:05}");
                w.auth.users.insert(uname.clone(), UserRecord {
                    id: 100_000 + i as u32, username: uname,
                    password_hash: hash_pw("x"), color: "#3a86ff".into(),
                    hue_idx: 0, is_admin: true, color_chosen: true, peak_level: 0,
                    ..Default::default()
                });
            }
        }

        // sim + viewport on their own OS threads, exactly like main() (Phase-2 dedicated pool).
        let world_sim = world.clone();
        std::thread::spawn(move || sim_loop(world_sim, cmd_rx));
        let world_vp = world.clone();
        let pool = Arc::new(rayon::ThreadPoolBuilder::new().num_threads(4).build().unwrap());
        std::thread::spawn(move || viewport_loop(world_vp, pool));

        let (cx, cy) = { let c = crate::config::cfg(); (c.spawn_x as i32, c.spawn_y as i32) };
        let view = (cx - 400, cy - 300, cx + 400, cy + 300); // 800×600 over the swarm

        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let app = Router::new().route("/", get(root_handler))
                .with_state(AppState {
                    world: world.clone(), cmd_tx: cmd_tx.clone(),
                    sessions: crate::session::SessionStore::new(),
                    rate:     crate::api::RateLimiter::default(),
                });
            tokio::spawn(async move { let _ = axum::serve(listener, app).await; });
            tokio::time::sleep(Duration::from_millis(300)).await;

            // Admin: login + spawn an NPC swarm clustered in the viewport, then drain forever.
            {
                let url = format!("ws://127.0.0.1:{port}/");
                let (ws, _) = connect_async(url.as_str()).await.unwrap();
                let (mut tx, mut rx) = ws.split();
                tx.send(WsMessage::Text(json!({"t":"login","username":"ADMIN","password":"admin"}).to_string())).await.unwrap();
                let mut seed = 0xABCD_1234u64;
                let mut rng = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1); (seed >> 33) as i32 };
                for _ in 0..40 {
                    let x = cx + rng().rem_euclid(700) - 350;
                    let y = cy + rng().rem_euclid(500) - 250;
                    tx.send(WsMessage::Text(json!({"t":"admin-spawn-at","x":x,"y":y}).to_string())).await.unwrap();
                    tokio::time::sleep(Duration::from_millis(15)).await; // stay under the 120 msg/s cap
                }
                tokio::spawn(async move { let _k = tx; while let Some(Ok(_)) = rx.next().await {} });
            }
            tokio::time::sleep(Duration::from_secs(2)).await; // let the swarm paint

            const WINDOW_SECS: u64 = 4;
            let ks = [6usize, 25, 100, 300];
            let mut handles = Vec::new();
            let mut ctrs: Vec<Arc<AtomicU64>> = Vec::new();
            let mut prev = 0usize;
            println!("\n=== egress conn-sweep (un-fogged worst case, {WINDOW_SECS}s windows) ===");
            println!("{:>5} {:>7} {:>10} {:>13} {:>13} {:>10} {:>11} {:>10} {:>10}",
                "conns", "ants", "painted", "recvKB/s/vw", "sendKB/s/vw", "tickP99ms", "vpCycP99", "finP99", "prevGrMB");
            for &k in &ks {
                for i in prev..k {
                    let b = Arc::new(AtomicU64::new(0));
                    ctrs.push(b.clone());
                    handles.push(tokio::spawn(run_viewer(port, format!("V{i:05}"), view, b)));
                }
                prev = k;
                tokio::time::sleep(Duration::from_millis(600)).await; // settle new joins

                for b in &ctrs { b.store(0, Ordering::Relaxed); }
                let send0: u64 = crate::metrics::per_kind().iter().map(|t| t.0).sum();
                let t0 = Instant::now();
                tokio::time::sleep(Duration::from_secs(WINDOW_SECS)).await;
                let secs = t0.elapsed().as_secs_f64();
                let send1: u64 = crate::metrics::per_kind().iter().map(|t| t.0).sum();

                let recv_total: u64 = ctrs.iter().map(|b| b.load(Ordering::Relaxed)).sum();
                let recv_kbps = recv_total as f64 / secs / k as f64 / 1024.0;
                let send_kbps = send1.saturating_sub(send0) as f64 / secs / k as f64 / 1024.0;

                let (ants, painted, tickp99) = {
                    let w = world.read().await;
                    (w.ants.len(), w.tiles.total_tiles(), p99(w.tick_ms_ring.clone()))
                };
                let (_, vc99, _) = crate::metrics::VIEWPORT_CYCLE_MS.pct();
                let (_, fv99, _) = crate::metrics::FINISH_VIEW_MS.pct();
                let pg_mb = crate::metrics::PREVGRID_BYTES.load(Ordering::Relaxed) as f64 / (1024.0 * 1024.0);
                println!("{k:>5} {ants:>7} {painted:>10} {recv_kbps:>13.2} {send_kbps:>13.2} {tickp99:>10.3} {vc99:>11.3} {fv99:>10.3} {pg_mb:>10.2}");

                // Phase-2 gate: viewer serialization on the dedicated pool must not starve the tick.
                assert!(tickp99 < 20.0, "tick p99 {tickp99:.3}ms exceeded the 20ms (50Hz) budget at {k} conns");
            }
            // server/admin/viewer tasks abort when `rt` drops at function end.
        });
    }
}