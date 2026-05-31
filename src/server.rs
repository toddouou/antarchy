use std::sync::Arc;
use std::time::{Duration, Instant};
use rayon::prelude::*;

use axum::{
    extract::{State, ws::{Message, WebSocket, WebSocketUpgrade}},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, watch, RwLock};

use crate::config::cfg;
use crate::handlers::handle_message;
use crate::network::{build_leaderboard, build_player_info, build_server_stats, finish_view, get_palette, snapshot_view, RawView};
use crate::simulation::tick_world;
use crate::world::World;

pub type WorldState = Arc<RwLock<World>>;

/// Messages pushed from WebSocket tasks → sim loop.
/// WS tasks never lock the World — they just push here.
pub enum Cmd {
    /// Auth (register / login): sim fills in player_id and conn_gen, signals reply.
    Auth {
        raw:     String,
        out_tx:  mpsc::UnboundedSender<String>,
        view_tx: watch::Sender<Option<String>>,
        reply:   oneshot::Sender<Option<(u32, u64)>>,
    },
    /// Any non-auth message from an authenticated connection.
    Message { pid: u32, raw: String },
    /// Connection closed — only clears tx if conn_gen still matches.
    Disconnect { pid: u32, conn_gen: u64 },
}

pub type CmdTx = mpsc::UnboundedSender<Cmd>;
type CmdRx = mpsc::UnboundedReceiver<Cmd>;

#[derive(Clone)]
pub struct AppState {
    pub world:  WorldState,
    pub cmd_tx: CmdTx,
}

static CLIENT_HTML: &str = include_str!("../public/client.html");

// ---- HTTP handlers --------------------------------------------------------

async fn root_handler(
    ws_opt: Option<WebSocketUpgrade>,
    State(app): State<AppState>,
) -> Response {
    match ws_opt {
        Some(ws) => ws.on_upgrade(|socket| handle_ws_connection(socket, app.cmd_tx)).into_response(),
        None     => Html(CLIENT_HTML).into_response(),
    }
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
    let connected = w.players.values().filter(|p| !p.npc && p.tx.is_some()).count();

    let body = serde_json::json!({
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
    }).to_string();
    (StatusCode::OK, [("Content-Type", "application/json")], body)
}

// ---- WebSocket connection -------------------------------------------------

async fn handle_ws_connection(socket: WebSocket, cmd_tx: CmdTx) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Priority channel: events, confirmations, me-updates — never dropped, ordered.
    let (prio_tx, mut prio_rx) = mpsc::unbounded_channel::<String>();
    // Viewport slot: latest-wins — stale snapshots are replaced, never backlogged.
    // watch::Sender is Clone; we keep one here and send one clone per auth to the sim loop.
    let (view_tx_conn, mut view_rx) = watch::channel::<Option<String>>(None);

    // Write task: delivers priority messages immediately; for viewports, only the latest
    // frame is sent — tokio::select! ensures a slow network never blocks event delivery.
    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased; // drain priority queue before checking viewport
                msg = prio_rx.recv() => {
                    match msg {
                        Some(m) => { if ws_tx.send(Message::Text(m)).await.is_err() { break; } }
                        None    => break,
                    }
                }
                result = view_rx.changed() => {
                    match result {
                        Ok(()) => {
                            let v = view_rx.borrow_and_update().clone();
                            if let Some(v) = v {
                                if ws_tx.send(Message::Text(v)).await.is_err() { break; }
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

    while let Some(Ok(msg)) = ws_rx.next().await {
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

        if t == "register" || t == "login" {
            // Auth: push to cmd queue, await reply (≤ 1 tick = ~20 ms)
            let (reply_tx, reply_rx) = oneshot::channel();
            // Clone the viewport sender so the sim loop can write to this connection's slot
            if cmd_tx.send(Cmd::Auth {
                raw: text,
                out_tx: prio_tx.clone(),
                view_tx: view_tx_conn.clone(),
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
            let _ = prio_tx.send(r#"{"t":"err","msg":"Not logged in"}"#.to_string());
        }
    }

    // view_tx_conn is dropped here; sim loop will drop p.view_tx on Disconnect,
    // causing write task's view_rx.changed() to return Err → write task exits.
    if let Some(pid) = player_id {
        let _ = cmd_tx.send(Cmd::Disconnect { pid, conn_gen });
    }
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

    loop {
        // ---- Exclusive tick window ----
        {
            let mut w = world.blocking_write();
            let tick_start = Instant::now();

            // Drain all pending WebSocket commands before ticking.
            // WS tasks never lock World — they push here, we process here.
            loop {
                match cmd_rx.try_recv() {
                    Ok(Cmd::Auth { raw, out_tx, view_tx, reply }) => {
                        let parsed = match serde_json::from_str::<serde_json::Value>(&raw) {
                            Ok(v) => v,
                            Err(_) => { let _ = reply.send(None); continue; }
                        };
                        let mut pid: Option<u32> = None;
                        handle_message(&mut w, &mut pid, &out_tx, parsed);
                        // Wire up the viewport watch channel for this connection
                        if let Some(id) = pid {
                            if let Some(p) = w.players.get_mut(&id) {
                                p.view_tx = Some(view_tx);
                                p.last_sent_dirty = 0; // force a full viewport send on login
                            }
                        }
                        let result = pid.map(|id| {
                            let gen = w.players.get(&id).map(|p| p.conn_gen).unwrap_or(0);
                            (id, gen)
                        });
                        let _ = reply.send(result);
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
                                p.tx      = None;
                                p.view_tx = None; // drops sender → write task's view_rx.changed() returns Err
                                p.away    = Some(snap);
                            }
                        }
                        println!("[disconnect] {uname} ({pid})");
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

            // Season rollover: once uptime exceeds the configured season length, wipe the
            // world and start a fresh season (compaction keeps RAM flat across the churn).
            let season_secs = cfg().season_secs;
            if season_secs > 0 && now.saturating_sub(w.started_at) >= season_secs * 1000 {
                crate::simulation::wipe_world(&mut w);
                w.started_at = now;
                w.broadcast(r#"{"t":"event","msg":"◆ NEW SEASON — WORLD RESET"}"#);
            }

            // Record this tick window's duration for /health p50/p99.
            w.record_tick_ms(tick_start.elapsed().as_secs_f32() * 1000.0);
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
    view_tx: Option<watch::Sender<Option<String>>>,
    tx:      Option<mpsc::UnboundedSender<String>>,
    raw:     Option<RawView>,
    me:      Option<String>,
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
pub fn viewport_loop(world: WorldState) {
    let mut last_tick:      u64 = u64::MAX;
    let mut last_tile_tick: u64 = 0;
    let mut last_me_tick:   u64 = 0;
    let mut last_lb_tick:   u64 = 0;
    let mut last_stats_tick: u64 = 0;

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
                let any_ants   = !w.ants.is_empty();
                let send_me    = tick.saturating_sub(last_me_tick) >= tr;
                let lb_due     = tick.saturating_sub(last_lb_tick) >= 20;
                let do_clients = include_tiles || any_ants;

                last_tick = tick;

                // Leaderboard is cheap (O(queens)); keep it under the lock.
                if lb_due {
                    let lb = build_leaderboard(&w);
                    w.broadcast(&lb);
                    last_lb_tick = tick;
                }

                // Server stats (~1 Hz) — header bar + admin cards for every client.
                if tick.saturating_sub(last_stats_tick) >= tr {
                    let stats = build_server_stats(&w);
                    w.broadcast(&stats);
                    last_stats_tick = tick;
                }

                if !do_clients && !send_me {
                    None
                } else {
                    if include_tiles { last_tile_tick = tick; }
                    if send_me       { last_me_tick = tick; }

                    let pids: Vec<u32> = w.players.iter()
                        .filter(|(_, p)| !p.npc && (p.tx.is_some() || p.view_tx.is_some()))
                        .map(|(&id, _)| id)
                        .collect();
                    let palette = Arc::new(get_palette(&w));
                    let wref: &World = &w;
                    let jobs: Vec<ClientJob> = pids.par_iter().map(|&pid| {
                        let p = wref.players.get(&pid);
                        ClientJob {
                            view_tx: p.and_then(|p| p.view_tx.clone()),
                            tx:      p.and_then(|p| p.tx.clone()),
                            raw:     if do_clients { snapshot_view(wref, pid, include_tiles) } else { None },
                            me:      if send_me { Some(build_player_info(wref, pid)) } else { None },
                        }
                    }).collect();
                    Some((jobs, palette))
                }
            }
        };

        // ---- Phase B: serialize + send, no lock held ----
        if let Some((jobs, palette)) = batch {
            let frames: Vec<Option<String>> = jobs.par_iter()
                .map(|job| job.raw.as_ref().map(|r| finish_view(r, palette.as_ref())))
                .collect();
            for (job, frame) in jobs.iter().zip(frames) {
                // Tile/ant frame → viewport watch slot (latest-wins, never backlogged)
                if let Some(frame) = frame {
                    if let Some(vtx) = &job.view_tx { let _ = vtx.send(Some(frame)); }
                }
                // me update → priority channel
                if let Some(me) = &job.me {
                    if let Some(tx) = &job.tx { let _ = tx.send(me.clone()); }
                }
            }
        }

        // Pace at up to ~120 Hz; never busy-spin when the sim is idle/slow.
        let el = cycle_start.elapsed();
        let min_cycle = Duration::from_millis(8);
        if el < min_cycle { std::thread::sleep(min_cycle - el); }
    }
}

// ---- Server startup -------------------------------------------------------

async fn world_info_handler() -> impl IntoResponse {
    let c = cfg();
    let body = serde_json::json!({
        "worldW":     c.world_w,
        "worldH":     c.world_h,
        "spawnX":     c.spawn_x,
        "spawnY":     c.spawn_y,
        "capitolLat": c.capitol_lat,
        "capitolLon": c.capitol_lon,
        "tileMeters": c.tile_meters,
    }).to_string();
    (StatusCode::OK, [("Content-Type", "application/json")], body)
}

pub async fn run(world: WorldState, cmd_tx: CmdTx) {
    let port = cfg().port;

    let app = Router::new()
        .route("/",           get(root_handler))
        .route("/health",     get(health_handler))
        .route("/world-info", get(world_info_handler))
        .with_state(AppState { world, cmd_tx });

    let addr = format!("0.0.0.0:{port}");
    let listener = TcpListener::bind(&addr).await
        .unwrap_or_else(|e| panic!("Cannot bind {addr}: {e}"));

    println!("Listening on http://localhost:{port}");
    axum::serve(listener, app).await.unwrap();
}
