use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{error::TrySendError, Sender};
use tokio::sync::watch;

use crate::auth::Auth;
use crate::config::{bubble_r_for_level, max_hp_for_level, queen_size_for_level, Config};
use crate::tile_map::TileMap;

/// A bounded per-connection outbound sender (OWASP A02/A10 — backpressure). Wraps a bounded
/// `tokio::mpsc::Sender` and sends **non-blocking**: when the connection's queue is full the message
/// is dropped rather than buffered, so a slow or malicious consumer can't grow server memory without
/// bound. The `send` method keeps every existing `tx.send(x)` call site unchanged. Depth is
/// `config::ws_send_queue()`.
#[derive(Clone, Debug)]
pub struct BoundedTx<T>(Sender<T>);

impl<T> BoundedTx<T> {
    pub fn new(inner: Sender<T>) -> Self { Self(inner) }
    /// Try to enqueue `msg`; `Err(Full)` (queue at capacity) or `Err(Closed)` (peer gone) just drops it.
    pub fn send(&self, msg: T) -> Result<(), TrySendError<T>> { self.0.try_send(msg) }
}

/// Phase-4 per-connection egress meter: a sliding byte counter incremented by the WS write task at
/// each real `ws_tx.send`, and read by `viewport_loop` to decide whether a connection is over its
/// `EGRESS_CAP_KBPS` budget and must be down-shifted. Lives behind an `Arc` so the write task (tokio)
/// and the viewport thread share one counter. `window_start_ms` anchors the current rate window.
#[derive(Debug, Default)]
pub struct EgressMeter {
    pub bytes:           AtomicU64,
    pub window_start_ms: AtomicU64,
}

impl EgressMeter {
    /// Record `len` billed bytes at `now_ms`. Rolls a fresh ~1 s window when the current one ages
    /// out, so `kbps` reads an approximate sliding rate. Lock-free (Relaxed is fine: a slightly
    /// stale rate only mis-times one down-shift decision, never corrupts state).
    pub fn add(&self, len: usize, now_ms: u64) {
        use std::sync::atomic::Ordering::Relaxed;
        let ws = self.window_start_ms.load(Relaxed);
        if now_ms.saturating_sub(ws) > 1000 {
            self.window_start_ms.store(now_ms, Relaxed);
            self.bytes.store(len as u64, Relaxed);
        } else {
            self.bytes.fetch_add(len as u64, Relaxed);
        }
    }

    /// Approximate current send rate in KB/s over the live window.
    pub fn kbps(&self, now_ms: u64) -> f64 {
        use std::sync::atomic::Ordering::Relaxed;
        let ws = self.window_start_ms.load(Relaxed);
        let age = (now_ms.saturating_sub(ws).max(1) as f64) / 1000.0;
        (self.bytes.load(Relaxed) as f64 / 1024.0) / age
    }
}

// ---- Ant ------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ant {
    pub id:            u32,
    pub owner:         u32,
    pub x:             i32,
    pub y:             i32,
    pub dx:            i8,
    pub dy:            i8,
    pub age:           u32,
    pub lifespan:      u32,
    pub highway_ticks: u8,
    /// 0 = normal worker, 1 = brute (2×2, slow, no self-erase, heavy damage)
    #[serde(default)]
    pub kind:          u8,
    #[serde(skip)]
    pub _nx: i32,
    #[serde(skip)]
    pub _ny: i32,
    #[serde(skip)]
    pub _ndx: i8,
    #[serde(skip)]
    pub _ndy: i8,
}

impl Ant {
    pub fn new(id: u32, owner: u32, x: i32, y: i32, dx: i8, dy: i8, lifespan: u32) -> Self {
        Ant::new_kind(id, owner, x, y, dx, dy, lifespan, 0)
    }
    // A worker carries position, heading, lifespan, and kind; grouping them into a struct
    // would just be unpacked again at the single call site, so allow the wide signature.
    #[allow(clippy::too_many_arguments)]
    pub fn new_kind(id: u32, owner: u32, x: i32, y: i32, dx: i8, dy: i8, lifespan: u32, kind: u8) -> Self {
        Ant { id, owner, x, y, dx, dy, age: 0, lifespan, highway_ticks: 0, kind,
              _nx: x, _ny: y, _ndx: dx, _ndy: dy }
    }
}

// ---- Queen ----------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Queen {
    pub x:              i32,
    pub y:              i32,
    pub size:           u8,
    pub hp:             i32,
    pub max_hp:         i32,
    pub level:          u16,
    pub xp:             f64,
    pub kills:          u32,
    pub bubble_r:       f64,
    pub last_attacker:  Option<u32>,
    pub dead:           bool,
    pub tiles_ever_held: u64,
    #[serde(default)]
    pub cached_tiles:   u64,
    pub npc:            bool,
    /// Shop shield: a separate damage-absorb pool (set to max_hp on cast); damage
    /// hits this before hp. Expires at shield_expiry. 0 = no shield.
    #[serde(default)]
    pub shield:         i32,
    #[serde(default)]
    pub shield_expiry:  Option<u64>,
    /// Region this queen sits in (metro name, else country, else "Open Water"). Recomputed
    /// on placement / relocate / admin-move. Drives the leaderboard region column + tabs.
    #[serde(default)]
    pub region:         String,
}

impl Queen {
    /// Apply a level change consistently: set `level`, derive the footprint `size`, the `max_hp`
    /// ceiling, and the placement-bubble `bubble_r` (which grows with level). Callers still set
    /// `hp`/`xp`/ant grants per their own policy. Centralising this stops a caller changing `level`
    /// but forgetting `size` (which desyncs the queen map) or `bubble_r` (which desyncs the glow).
    pub fn set_level(&mut self, lvl: u16, c: &Config) {
        self.level    = lvl;
        self.size     = queen_size_for_level(lvl);
        self.max_hp   = max_hp_for_level(lvl, c);
        self.bubble_r = bubble_r_for_level(lvl, c);
    }
}

// ---- Player ---------------------------------------------------------------

#[derive(Debug)]
pub struct Player {
    /// Redundant with the `players` map key; kept for clarity/Debug. Not read directly.
    #[allow(dead_code)]
    pub id:              u32,
    pub username:        String,
    pub color:           String,
    pub hue_idx:         i32,
    pub ants_avail:      i32,
    pub next_refill:     u64,
    pub queen_placed_at: Option<u64>,
    pub npc:             bool,
    /// Ephemeral read-only spectator (landing-page guest). Receives viewport **ant** frames but
    /// NEVER tile frames (its territory renders from free R2 super-tiles), and is excluded from the
    /// leaderboard, daily refills, and persistence. Allocated a reserved hi-range id; removed on
    /// disconnect. The single authoritative "this connection costs minimal egress" marker.
    pub guest:           bool,
    pub view:            Option<PlayerView>,
    pub tx:              Option<BoundedTx<String>>,
    pub view_tx:         Option<watch::Sender<Option<Vec<u8>>>>,
    /// Phase-3 egress: ordered, never-dropped BINARY control channel (compressed `me`/leaderboard/
    /// stats/region-holders — kinds 16–20). Parallel to `tx`; only fed when `bin` is set. Carries an
    /// `Arc<[u8]>` (Phase-5) so a broadcast frame is built once and fanned out by refcount-clone, not
    /// a per-recipient `Vec` copy — the O(connections) control fan-out is the hot path.
    pub ctl_tx:          Option<BoundedTx<Arc<[u8]>>>,
    /// Phase-4 per-connection egress meter (shared with the WS write task). `None` until auth wires
    /// it on, or for NPCs / never-connected players.
    pub egress_meter:    Option<Arc<EgressMeter>>,
    /// Client advertised the Phase-3 binary/compressed protocol (`{"bin":1}` at auth) **and** the
    /// server master flag (`config::bin_ctl_enabled`) is on. Gates ant-frame kind 3, fog-on-
    /// keyframes, and the binary control channel. Old tabs across a redeploy never advertise it →
    /// they transparently keep the legacy text/JSON protocol (no broken control frames).
    pub bin:             bool,
    pub conn_gen:        u64,
    pub prestige:        u32,
    pub credits:         u64,
    /// Queued shop defenders: each entry is an expiry timestamp (ms). When an enemy
    /// worker nears this player's queen, one is consumed to spawn a free distraction ant.
    pub defenders:       Vec<u64>,
    // ---- Discovery (account-level; survive queen death because Player outlives the Queen) ----
    /// Distinct countries / continents this account's queens & ants have set foot in.
    pub visited_countries:   FxHashSet<String>,
    pub visited_continents:  FxHashSet<String>,
    /// Lifetime accumulators folded in from each fallen queen (for the USER-vs-QUEEN compare).
    pub lifetime_kills:      u32,
    pub lifetime_peak_tiles: u64,
    pub queens_fielded:      u32,
    // ---- Admin god-mode (per-target toggles; persisted) ----
    /// When set, this player never spends credits (shop is free).
    pub unlimited_credits:   bool,
    /// When set, this player has an infinite worker pool (no ants_avail / army_cap gating).
    pub unlimited_ants:      bool,
    // ---- Rivalries (account-level, keyed by rival username; survive queen death) ----
    /// How many times each named rival's queen has slain THIS account's queen.
    pub killed_by:           FxHashMap<String, u32>,
    /// How many times THIS account's queen has slain each named rival's queen.
    pub kills_of:            FxHashMap<String, u32>,
    /// Snapshot taken at disconnect; diffed on reconnect for the welcome-back summary.
    pub away:                Option<AwaySnapshot>,
}

/// Snapshot of a player's state at disconnect → diffed on reconnect for the welcome-back summary.
#[derive(Debug, Clone)]
pub struct AwaySnapshot {
    pub at_ms:             u64,
    pub tiles:             u64,
    pub kills:             u32,
    pub level:             u16,
    pub army:              u32,
    pub visited_countries: usize,
    pub queen_alive:       bool,
}

#[derive(Debug, Clone)]
pub struct PlayerView {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

// ---- XP / hit accumulation -----------------------------------------------

#[derive(Debug)]
pub struct XpGrant {
    pub player_id: u32,
    pub amount:    f64,
    pub reason:    &'static str,
    pub x:         i32,
    pub y:         i32,
}

#[derive(Debug, Clone)]
pub struct QueenHit {
    pub queen_id:  u32,
    pub attacker:  u32,
    pub is_own:    bool,
    /// Damage multiplier for this hit (1.0 normal, >1 for brute ants).
    pub dmg_mult:  f32,
}

// ---- beta-v2 auth: transient pending state (RAM-only, NEVER persisted) ----------------------------

/// A registration awaiting email/phone code entry. Lives only in `World.pending_regs` keyed by a
/// random `regId`; a restart just asks the user to register again (keeps it out of world.snapshot,
/// avoiding the bincode-wipe risk). Promoted into a durable `auth.UserRecord` only on verification.
#[derive(Debug, Clone)]
pub struct PendingReg {
    pub email:          String,  // lowercase
    pub phone:          String,  // E.164 ("" if none)
    pub handle:         String,  // public display name
    pub password_hash:  String,
    pub color:          String,
    pub hue_idx:        i32,
    pub email_code:     String,  // 6-digit
    pub phone_code:     String,  // 6-digit
    pub email_ok:       bool,
    pub phone_ok:       bool,
    /// Whether phone verification is required to finalize (captured = sms_enabled() at register time).
    pub phone_required: bool,
    pub created_ms:     u64,
    /// Failed code-entry attempts; the pending reg is dropped past a small cap (brute-force guard).
    pub attempts:       u32,
}

/// A password-reset grant: a random token → the account it resets, with a creation stamp for expiry.
/// Transient (RAM-only) — an expired/lost reset simply isn't honoured after a restart.
#[derive(Debug, Clone)]
pub struct ResetToken {
    pub username:   String,  // UPPERCASE key into auth.users
    pub created_ms: u64,
}

// ---- World ----------------------------------------------------------------

/// King-of-the-hill result for one metro: who holds the most painted tiles inside its radius.
#[derive(Debug, Clone)]
pub struct MetroHolder {
    pub name:  String,
    pub owner: Option<u32>,
    pub tiles: u64,
}

pub struct World {
    pub tiles:           TileMap,
    pub ants:            Vec<Ant>,
    pub queens:          FxHashMap<u32, Queen>,
    pub players:         FxHashMap<u32, Player>,
    pub queen_map:       FxHashMap<u64, u32>,
    pub queen_map_dirty: bool,
    pub tick:            u64,
    pub next_player_id:  u32,
    pub xp_queue:        Vec<XpGrant>,
    pub world_w:         u32,
    pub world_h:         u32,
    pub auth:            Auth,
    pub started_at:      u64,
    /// Reusable sorted (dest_key, ant_idx) pairs — cleared each phase, no allocation after warmup.
    pub scratch_pairs:   Vec<(u64, u32)>,
    /// Per-owner ant counts, rebuilt each tick after age-out. Used by build_player_info.
    pub ant_counts:      FxHashMap<u32, u32>,
    pub paused:          bool,
    /// Phase-7: connections whose tab is hidden/backgrounded (client sent `view-pause`). The
    /// viewport loop skips frame delivery for these → ~0 egress for hidden tabs. Runtime-only.
    pub paused_views:    FxHashSet<u32>,
    /// Tracks the last tick on which tiles changed; used to skip viewport delivery when idle.
    pub dirty_tick:      u64,
    /// Metro king-of-the-hill holders, recomputed on a throttle (simulation.rs::recompute_holders).
    pub metro_holders:   Vec<MetroHolder>,
    /// Round-robin cursor into `ants` for throttled discovery (visited-region) sampling.
    pub visit_sample_cursor: usize,
    /// Ring buffer of recent tick-window durations (ms) for `/health` p50/p99 — the scaling
    /// metric that tells us whether a tick holds its budget at load. `tick_ms_pos` is the
    /// write cursor once the ring fills.
    pub tick_ms_ring: Vec<f32>,
    pub tick_ms_pos:  usize,
    /// Phase-6 snapshot generation tag. Forms the R2 key prefix `snap/{epoch}/…` so a wipe / restart
    /// serves a fresh tile set instead of a stale cached one. Bumped on wipe + restore; not persisted
    /// (the canvas is re-uploaded under a fresh epoch each boot). Seconds-resolution time seed.
    pub epoch: u64,
    /// Phase-6 R2 cleanup queue: epochs whose tile generation is now orphaned (set on wipe, when the
    /// epoch rolls). The snapshot writer thread drains this and deletes `snap/{epoch}/` off-lock, so
    /// a wipe reclaims R2 space instead of leaking the whole previous canvas. Transient; not persisted.
    pub snapshot_retire: Vec<u64>,
    // ---- beta-v2 auth: transient maps + guest allocator (RAM-only, never persisted) ----
    /// Unverified registrations awaiting code entry, keyed by random regId. GC'd in the tick loop.
    pub pending_regs:    FxHashMap<String, PendingReg>,
    /// Live password-reset tokens, keyed by random token. GC'd in the tick loop.
    pub reset_tokens:    FxHashMap<String, ResetToken>,
    /// Rolling counter for allocating guest spectator ids in the reserved hi range (`GUEST_ID_BASE+`).
    pub next_guest_seq:  u32,
    /// Anti-replay (OWASP A01/A06): the last accepted client command `seq` per connected player id.
    /// Mutating gameplay messages must carry a strictly increasing `seq`; duplicates / out-of-order
    /// are rejected. Transient (RAM-only, never persisted); reset on (re)connect, cleared on disconnect.
    pub last_seq:        FxHashMap<u32, u64>,
}

/// Base of the reserved guest-spectator id range (disjoint from real player ids, which start at 100
/// and increment via `next_player_id`). Guests get `GUEST_ID_BASE + (seq % GUEST_ID_SPAN)`.
pub const GUEST_ID_BASE: u32 = 0xF000_0000;
pub const GUEST_ID_SPAN: u32 = 0x0FFF_FFFF;

/// True if `id` is a guest-spectator id (the reserved hi range).
pub fn is_guest_id(id: u32) -> bool { id >= GUEST_ID_BASE }

/// Capacity of the tick-duration ring (≈ a few seconds of history at 50 Hz).
pub const TICK_RING_CAP: usize = 240;

impl World {
    pub fn new() -> Self {
        use crate::config::{cfg, current_ms};
        let c = cfg();
        let world_w = c.world_w;
        let world_h = c.world_h;
        drop(c);
        World {
            tiles:           TileMap::default(),
            ants:            Vec::new(),
            queens:          FxHashMap::default(),
            players:         FxHashMap::default(),
            queen_map:       FxHashMap::default(),
            queen_map_dirty: true,
            tick:            0,
            next_player_id:  100,
            xp_queue:        Vec::new(),
            world_w,
            world_h,
            auth:            Auth::new(),
            started_at:      current_ms(),
            scratch_pairs:   Vec::new(),
            ant_counts:      FxHashMap::default(),
            paused:          false,
            paused_views:    FxHashSet::default(),
            dirty_tick:      0,
            metro_holders:   Vec::new(),
            visit_sample_cursor: 0,
            tick_ms_ring:    Vec::with_capacity(TICK_RING_CAP),
            tick_ms_pos:     0,
            epoch:           current_ms() / 1000,
            snapshot_retire: Vec::new(),
            pending_regs:    FxHashMap::default(),
            reset_tokens:    FxHashMap::default(),
            next_guest_seq:  0,
            last_seq:        FxHashMap::default(),
        }
    }

    /// Allocate the next guest-spectator id in the reserved hi range.
    pub fn alloc_guest_id(&mut self) -> u32 {
        let id = GUEST_ID_BASE + (self.next_guest_seq % GUEST_ID_SPAN);
        self.next_guest_seq = self.next_guest_seq.wrapping_add(1);
        id
    }

    /// Current number of connected guest spectators (for the `HIVE_MAX_GUESTS` ceiling).
    pub fn guest_count(&self) -> usize {
        self.players.values().filter(|p| p.guest).count()
    }

    /// Roll the Phase-6 snapshot epoch to a fresh value (seconds since the Unix epoch). Called on
    /// wipe and on persist-restore so clients never composite a stale season's R2 tiles.
    pub fn fresh_epoch(&mut self) {
        self.epoch = crate::config::current_ms() / 1000;
    }

    /// Roll to a fresh epoch **and** queue the outgoing one for R2 deletion. Used by the wipe paths:
    /// once the epoch rolls, every `snap/{old}/…` tile is orphaned, so the snapshot writer should
    /// reclaim it. (Plain `fresh_epoch` is kept for the restart/restore path, which intentionally
    /// leaves the prior tiles in place as a reconnect fallback until the new epoch re-uploads.)
    pub fn rotate_epoch_retiring_old(&mut self) {
        let old = self.epoch;
        self.fresh_epoch();
        if self.epoch != old {
            self.snapshot_retire.push(old);
        }
    }

    /// Record one tick-window duration into the ring (overwrites oldest once full).
    pub fn record_tick_ms(&mut self, ms: f32) {
        if self.tick_ms_ring.len() < TICK_RING_CAP {
            self.tick_ms_ring.push(ms);
        } else {
            self.tick_ms_ring[self.tick_ms_pos] = ms;
            self.tick_ms_pos = (self.tick_ms_pos + 1) % TICK_RING_CAP;
        }
    }

    pub fn cell_key(&self, x: i32, y: i32) -> u64 {
        y as u64 * self.world_w as u64 + x as u64
    }

    pub fn get_queen_map(&mut self) -> &FxHashMap<u64, u32> {
        if self.queen_map_dirty {
            self.queen_map.clear();
            let ww = self.world_w as u64;
            for (&qid, q) in &self.queens {
                if q.dead { continue; }
                for dy in 0..q.size as i32 {
                    for dx in 0..q.size as i32 {
                        let k = (q.y + dy) as u64 * ww + (q.x + dx) as u64;
                        self.queen_map.insert(k, qid);
                    }
                }
            }
            self.queen_map_dirty = false;
        }
        &self.queen_map
    }

    pub fn send_to(&self, player_id: u32, msg: String) {
        if let Some(p) = self.players.get(&player_id) {
            if let Some(tx) = &p.tx {
                let _ = tx.send(msg);
            }
        }
    }

    pub fn broadcast(&self, msg: &str) {
        for p in self.players.values() {
            if let Some(tx) = &p.tx {
                let _ = tx.send(msg.to_string());
            }
        }
    }

    /// Phase-3 control fan-out. `frame` is a pre-built `[kind][deflated json]` binary control frame
    /// (build it once with `network::ctl_frame`); `json_fallback` is the identical message as raw
    /// text. Players that negotiated the binary protocol (`bin` + a live `ctl_tx`) get the compressed
    /// binary frame; everyone else gets the legacy uncompressed text on `tx`. Deflate happens once in
    /// the caller, so this is the O(connections) fan-out only.
    pub fn broadcast_ctl(&self, frame: Arc<[u8]>, json_fallback: &str) {
        for p in self.players.values() {
            if p.bin {
                // refcount-clone of the single built frame — no per-recipient byte copy.
                if let Some(ctl) = &p.ctl_tx { let _ = ctl.send(frame.clone()); continue; }
            }
            if let Some(tx) = &p.tx { let _ = tx.send(json_fallback.to_string()); }
        }
    }

    pub fn broadcast_near(&self, cx: i32, cy: i32, msg: &str) {
        for p in self.players.values() {
            if let (Some(tx), Some(v)) = (&p.tx, &p.view) {
                if cx >= v.x0 && cx < v.x1 && cy >= v.y0 && cy < v.y1 {
                    let _ = tx.send(msg.to_string());
                }
            }
        }
    }

    /// Paint a queen's `size`×`size` body at (`x`,`y`) as owned by `owner`, clamped to the
    /// world so a body near the edge can never write phantom out-of-world tiles. Used by
    /// every queen placement / move (place-queen, spawn_npc, admin move, shop relocate).
    pub fn paint_queen_body(&mut self, x: i32, y: i32, size: u8, owner: u32) {
        let (ww, wh) = (self.world_w as i32, self.world_h as i32);
        for dy in 0..size as i32 {
            for dx in 0..size as i32 {
                let (px, py) = (x + dx, y + dy);
                if px < 0 || py < 0 || px >= ww || py >= wh { continue; }
                self.tiles.set(px as u32, py as u32, owner);
            }
        }
    }

    /// Clear a queen's body, erasing only cells still owned by `owner` (so a move never
    /// wipes a neighbour's overlapping tiles). Clamped to the world like `paint_queen_body`.
    pub fn clear_queen_body(&mut self, x: i32, y: i32, size: u8, owner: u32) {
        let (ww, wh) = (self.world_w as i32, self.world_h as i32);
        for dy in 0..size as i32 {
            for dx in 0..size as i32 {
                let (px, py) = (x + dx, y + dy);
                if px < 0 || py < 0 || px >= ww || py >= wh { continue; }
                if self.tiles.get(px as u32, py as u32) == owner {
                    self.tiles.set(px as u32, py as u32, 0);
                }
            }
        }
    }

    /// True if (`x`,`y`) lies within any live OTHER queen's bubble. Shared by place-queen
    /// and shop relocate to enforce the no-overlap spacing rule.
    pub fn too_close_to_queen(&self, x: i32, y: i32, exclude: u32) -> bool {
        self.queens.iter().filter(|(_, q)| !q.dead).any(|(&qid, q)| {
            if qid == exclude { return false; }
            let ddx = (q.x + q.size as i32 / 2 - x) as i64;
            let ddy = (q.y + q.size as i32 / 2 - y) as i64;
            ((ddx * ddx + ddy * ddy) as f64).sqrt() < q.bubble_r
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(w: &World, owner: u32) -> i64 { w.tiles.counts.get(&owner).copied().unwrap_or(0) }

    fn mk_queen(x: i32, y: i32, bubble_r: f64) -> Queen {
        Queen { x, y, size: 2, hp: 100, max_hp: 100, level: 1, xp: 0.0, kills: 0,
            bubble_r, last_attacker: None, dead: false, tiles_ever_held: 0, cached_tiles: 0,
            npc: false, shield: 0, shield_expiry: None, region: String::new() }
    }

    #[test]
    fn paint_and_clear_queen_body() {
        let mut w = World::new();
        w.paint_queen_body(10, 20, 3, 42);
        assert_eq!(count(&w, 42), 9);
        assert_eq!(w.tiles.get(12, 22), 42);
        // A neighbour overwrites one body cell; clear must spare it.
        w.tiles.set(11, 21, 7);
        w.clear_queen_body(10, 20, 3, 42);
        assert_eq!(count(&w, 42), 0);
        assert_eq!(w.tiles.get(11, 21), 7, "neighbour tile preserved");
    }

    #[test]
    fn paint_queen_body_clamps_at_world_edge() {
        let mut w = World::new();
        let (ww, wh) = (w.world_w as i32, w.world_h as i32);
        w.paint_queen_body(ww - 1, wh - 1, 4, 5);   // only the corner cell is in-bounds
        assert_eq!(count(&w, 5), 1, "only in-bounds cells painted");
        assert_eq!(w.tiles.get(ww as u32, wh as u32), 0, "no phantom tile past the edge");
    }

    #[test]
    fn too_close_to_queen_respects_bubble_and_exclude() {
        let mut w = World::new();
        w.queens.insert(1, mk_queen(1000, 1000, 30.0));
        assert!(w.too_close_to_queen(1010, 1000, 0),  "inside the bubble");
        assert!(!w.too_close_to_queen(1100, 1000, 0), "outside the bubble");
        assert!(!w.too_close_to_queen(1010, 1000, 1), "excluded queen ignored");
    }

    #[test]
    fn set_level_derives_size_and_max_hp() {
        let c = crate::config::cfg().clone();
        let mut q = mk_queen(0, 0, 30.0);
        q.set_level(25, &c);
        assert_eq!(q.level, 25);
        assert_eq!(q.size, queen_size_for_level(25));
        assert_eq!(q.max_hp, max_hp_for_level(25, &c));
        // bubble_r scales with level, so it should match the formula and exceed the L1 base.
        assert_eq!(q.bubble_r, bubble_r_for_level(25, &c));
        assert!(q.bubble_r > c.bubble_r, "range should grow past the L1 base");
    }
}
