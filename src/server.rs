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

use rustc_hash::{FxHashMap, FxHashSet};

use crate::config::cfg;
use crate::handlers::handle_message;
use crate::network::{
    build_leaderboard, build_player_info, build_server_stats, ctl_frame, finish_view, get_palette,
    snapshot_view, PrevGrid, RawView, CTL_LEADERBOARD, CTL_ME, CTL_STATS,
};
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
        ctl_tx:  mpsc::UnboundedSender<Vec<u8>>,
        view_tx: watch::Sender<Option<Vec<u8>>>,
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

async fn handle_ws_connection(socket: WebSocket, cmd_tx: CmdTx) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Priority channel: events, confirmations, errors — raw text, never dropped, ordered.
    let (prio_tx, mut prio_rx) = mpsc::unbounded_channel::<String>();
    // Binary control channel (Phase 3): compressed me/leaderboard/stats/region-holders — also
    // never dropped + ordered, but separate so the heavy text builders ride binary without touching
    // the ~100 raw-text send sites. Only fed when the client negotiated `bin`.
    let (ctl_tx_conn, mut ctl_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    // Viewport slot: latest-wins — stale snapshots are replaced, never backlogged.
    // watch::Sender is Clone; we keep one here and send one clone per auth to the sim loop.
    let (view_tx_conn, mut view_rx) = watch::channel::<Option<Vec<u8>>>(None);

    // Write task: delivers priority messages immediately; for viewports, only the latest
    // frame is sent — tokio::select! ensures a slow network never blocks event delivery.
    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased; // text events first, then binary control, then viewport (latest-wins)
                msg = prio_rx.recv() => {
                    match msg {
                        Some(m) => {
                            crate::metrics::record(crate::metrics::kind_for_text_type(quick_msg_type(&m)), m.len());
                            if ws_tx.send(Message::Text(m)).await.is_err() { break; }
                        }
                        None    => break,
                    }
                }
                ctl = ctl_rx.recv() => {
                    match ctl {
                        Some(v) => {
                            crate::metrics::record(crate::metrics::kind_for_bin(v.first().copied().unwrap_or(0xFF)), v.len());
                            if ws_tx.send(Message::Binary(v)).await.is_err() { break; }
                        }
                        None    => break,
                    }
                }
                result = view_rx.changed() => {
                    match result {
                        Ok(()) => {
                            let v = view_rx.borrow_and_update().clone();
                            if let Some(v) = v {
                                crate::metrics::record(crate::metrics::kind_for_bin(v.first().copied().unwrap_or(0xFF)), v.len());
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
                ctl_tx: ctl_tx_conn.clone(),
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
    // Wall-clock timer for the periodic world autosave (see below).
    let mut last_save = Instant::now();

    loop {
        // ---- Exclusive tick window ----
        {
            let mut w = world.blocking_write();
            let tick_start = Instant::now();

            // Drain all pending WebSocket commands before ticking.
            // WS tasks never lock World — they push here, we process here.
            loop {
                match cmd_rx.try_recv() {
                    Ok(Cmd::Auth { raw, out_tx, ctl_tx, view_tx, reply }) => {
                        let parsed = match serde_json::from_str::<serde_json::Value>(&raw) {
                            Ok(v) => v,
                            Err(_) => { let _ = reply.send(None); continue; }
                        };
                        let mut pid: Option<u32> = None;
                        handle_message(&mut w, &mut pid, &out_tx, parsed);
                        // Wire up the viewport watch channel + binary control channel for this conn.
                        if let Some(id) = pid {
                            if let Some(p) = w.players.get_mut(&id) {
                                p.view_tx = Some(view_tx);
                                p.ctl_tx  = Some(ctl_tx);
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
                                p.ctl_tx  = None; // drops sender → write task's ctl_rx.recv() returns None
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

            // Periodic autosave: flush the world to disk every ~60 s so an unclean exit (crash,
            // SIGKILL) loses at most a minute of progress; the shutdown handler covers clean exits.
            // Serializes under the write lock — simple and fine at current scale (see persist.rs).
            if last_save.elapsed().as_secs() >= 60 {
                let path = cfg().save_file.clone();
                if let Err(e) = crate::persist::save(&w, &path) {
                    eprintln!("[persist] autosave failed: {e}");
                }
                last_save = Instant::now();
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
    pid:     u32,
    view_tx: Option<watch::Sender<Option<Vec<u8>>>>,
    tx:      Option<mpsc::UnboundedSender<String>>,
    ctl_tx:  Option<mpsc::UnboundedSender<Vec<u8>>>,
    /// This connection negotiated the Phase-3 binary protocol (ant kind 3, fog-on-keyframes, binary
    /// `me`). Decides the per-client frame encoding in the lock-free phase.
    bin:     bool,
    raw:     Option<RawView>,
    me:      Option<String>,
    /// Per-client tile state (keyframe/delta), moved out of the viewport thread's map for the
    /// lock-free parallel phase and moved back after.
    prev:    Option<PrevGrid>,
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

                // Leaderboard is cheap (O(queens)); keep it under the lock. Compress once → binary
                // for `bin` clients, raw text for the rest (broadcast_ctl handles the split).
                if lb_due {
                    let lb = build_leaderboard(&w);
                    w.broadcast_ctl(&ctl_frame(CTL_LEADERBOARD, &lb), &lb);
                    last_lb_tick = tick;
                }

                // Server stats (~1 Hz) — header bar + admin cards for every client.
                if tick.saturating_sub(last_stats_tick) >= tr {
                    let stats = build_server_stats(&w);
                    w.broadcast_ctl(&ctl_frame(CTL_STATS, &stats), &stats);
                    last_stats_tick = tick;
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
                        .filter(|(_, p)| !p.npc && (p.tx.is_some() || p.view_tx.is_some()))
                        .map(|(&id, _)| id)
                        .collect();
                    let palette = Arc::new(get_palette(&w));
                    let wref: &World = &w;
                    let jobs: Vec<ClientJob> = pool.install(|| pids.par_iter().map(|&pid| {
                        let p = wref.players.get(&pid);
                        ClientJob {
                            pid,
                            view_tx: p.and_then(|p| p.view_tx.clone()),
                            tx:      p.and_then(|p| p.tx.clone()),
                            ctl_tx:  p.and_then(|p| p.ctl_tx.clone()),
                            bin:     p.map(|p| p.bin).unwrap_or(false),
                            raw:     if do_clients { snapshot_view(wref, pid, include_tiles) } else { None },
                            // Periodic `me` carries only the DYNAMIC fields (full=false); the static
                            // cfg/geo/world/spawn block ships once in `logged-in`.
                            me:      if send_me { Some(build_player_info(wref, pid, false)) } else { None },
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
                Option<mpsc::UnboundedSender<String>>,
                Option<mpsc::UnboundedSender<Vec<u8>>>,
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
                        && ctl_tx.as_ref().map(|c| c.send(ctl_frame(CTL_ME, me)).is_ok()).unwrap_or(false);
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

/// Per-kind egress accounting + the CPU/memory walls — the data behind the budget ledger
/// (EGRESS_PLAN.md §5). Bytes are tallied at the billed `ws_tx.send` sites (`metrics::record`);
/// rates are over server uptime. Additive: remove this route + the `metrics::record` calls to
/// fully revert Phase 0.
async fn egress_stats_handler(State(app): State<AppState>) -> impl IntoResponse {
    let (uptime_ms, connected, dirty) = {
        let w = app.world.read().await;
        (
            crate::config::current_ms().saturating_sub(w.started_at),
            w.players.values().filter(|p| !p.npc && p.tx.is_some()).count(),
            w.tiles.dirty_chunk_touches(),
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
        "viewportCycleMs":      {"p50": vc50, "p99": vc99, "max": vcmax},
        "finishViewMs":         {"p50": fv50, "p99": fv99, "max": fvmax},
        "prevGridBytes":        pg,
        "prevGridMB":           (pg as f64 / (1024.0 * 1024.0) * 100.0).round() / 100.0,
    }).to_string();
    (StatusCode::OK, [("Content-Type", "application/json")], body)
}

pub async fn run(world: WorldState, cmd_tx: CmdTx) {
    let port = cfg().port;

    let app = Router::new()
        .route("/",           get(root_handler))
        .route("/health",     get(health_handler))
        .route("/world-info", get(world_info_handler))
        .route("/egress-stats", get(egress_stats_handler))
        .with_state(AppState { world, cmd_tx });

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
                    hue_idx: 0, is_admin: true, color_chosen: true,
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
                .with_state(AppState { world: world.clone(), cmd_tx: cmd_tx.clone() });
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
