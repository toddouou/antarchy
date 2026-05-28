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
use tokio::sync::{mpsc, oneshot, RwLock};

use crate::config::cfg;
use crate::handlers::handle_message;
use crate::network::{build_leaderboard, build_player_info, build_view_update};
use crate::simulation::tick_world;
use crate::world::World;

pub type WorldState = Arc<RwLock<World>>;

/// Messages pushed from WebSocket tasks → sim loop.
/// WS tasks never lock the World — they just push here.
pub enum Cmd {
    /// Auth (register / login): sim fills in player_id and signals reply.
    Auth {
        raw:    String,
        out_tx: mpsc::UnboundedSender<String>,
        reply:  oneshot::Sender<Option<u32>>,
    },
    /// Any non-auth message from an authenticated connection.
    Message { pid: u32, raw: String },
    /// Connection closed.
    Disconnect { pid: u32 },
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
    let body = serde_json::json!({
        "tick":     w.tick,
        "ants":     w.ants.len(),
        "queens":   w.queens.values().filter(|q| !q.dead).count(),
        "players":  w.players.values().filter(|p| !p.npc).count(),
        "uptimeMs": crate::config::current_ms() - w.started_at,
    }).to_string();
    (StatusCode::OK, [("Content-Type", "application/json")], body)
}

// ---- WebSocket connection -------------------------------------------------

async fn handle_ws_connection(socket: WebSocket, cmd_tx: CmdTx) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (mpsc_tx, mut mpsc_rx) = mpsc::unbounded_channel::<String>();

    // Dedicated write task: drains the per-connection outbox → WebSocket.
    tokio::spawn(async move {
        while let Some(msg) = mpsc_rx.recv().await {
            if ws_tx.send(Message::Text(msg)).await.is_err() { break; }
        }
    });

    let mut player_id: Option<u32> = None;
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
            if cmd_tx.send(Cmd::Auth { raw: text, out_tx: mpsc_tx.clone(), reply: reply_tx }).is_err() {
                break;
            }
            if let Ok(Some(pid)) = reply_rx.await {
                player_id = Some(pid);
            }
        } else if let Some(pid) = player_id {
            // Non-auth: fire-and-forget, sim processes at next tick start
            if cmd_tx.send(Cmd::Message { pid, raw: text }).is_err() { break; }
        } else {
            let _ = mpsc_tx.send(r#"{"t":"err","msg":"Not logged in"}"#.to_string());
        }
    }

    if let Some(pid) = player_id {
        let _ = cmd_tx.send(Cmd::Disconnect { pid });
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

            // Drain all pending WebSocket commands before ticking.
            // WS tasks never lock World — they push here, we process here.
            loop {
                match cmd_rx.try_recv() {
                    Ok(Cmd::Auth { raw, out_tx, reply }) => {
                        let parsed = match serde_json::from_str::<serde_json::Value>(&raw) {
                            Ok(v) => v,
                            Err(_) => { let _ = reply.send(None); continue; }
                        };
                        let mut pid: Option<u32> = None;
                        handle_message(&mut w, &mut pid, &out_tx, parsed);
                        let _ = reply.send(pid);
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
                    }
                    Ok(Cmd::Disconnect { pid }) => {
                        let uname = w.players.get(&pid).map(|p| p.username.clone()).unwrap_or_default();
                        if let Some(p) = w.players.get_mut(&pid) { p.tx = None; }
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
        }

        // ---- Read-only viewport delivery ----
        {
            let w = world.blocking_read();
            send_viewports(&w);
        }

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

fn send_viewports(world: &World) {
    if world.tick % 2 != 0 { return; }  // 25 Hz delivery

    let pids: Vec<u32> = world.players.iter()
        .filter(|(_, p)| !p.npc && p.tx.is_some())
        .map(|(&id, _)| id)
        .collect();

    // Build all viewport updates in parallel (fog + tile work is O(viewport_area) per player)
    let updates: Vec<(u32, Option<String>, String)> = pids.par_iter()
        .map(|&pid| (pid, build_view_update(world, pid), build_player_info(world, pid)))
        .collect();

    for (pid, view, info) in updates {
        if let Some(p) = world.players.get(&pid) {
            if let Some(tx) = &p.tx {
                if let Some(v) = view { let _ = tx.send(v); }
                let _ = tx.send(info);
            }
        }
    }

    if world.tick % 20 == 0 {
        let lb = build_leaderboard(world);
        world.broadcast(&lb);
    }
}

// ---- Server startup -------------------------------------------------------

pub async fn run(world: WorldState, cmd_tx: CmdTx) {
    let port = cfg().port;

    let app = Router::new()
        .route("/",       get(root_handler))
        .route("/health", get(health_handler))
        .with_state(AppState { world, cmd_tx });

    let addr = format!("0.0.0.0:{port}");
    let listener = TcpListener::bind(&addr).await
        .unwrap_or_else(|e| panic!("Cannot bind {addr}: {e}"));

    println!("Listening on http://localhost:{port}");
    axum::serve(listener, app).await.unwrap();
}
