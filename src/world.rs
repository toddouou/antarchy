use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;

use crate::auth::Auth;
use crate::tile_map::TileMap;

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
    pub view:            Option<PlayerView>,
    pub tx:              Option<UnboundedSender<String>>,
    pub view_tx:         Option<watch::Sender<Option<String>>>,
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
}

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
            dirty_tick:      0,
            metro_holders:   Vec::new(),
            visit_sample_cursor: 0,
            tick_ms_ring:    Vec::with_capacity(TICK_RING_CAP),
            tick_ms_pos:     0,
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

    pub fn broadcast_near(&self, cx: i32, cy: i32, msg: &str) {
        for p in self.players.values() {
            if let (Some(tx), Some(v)) = (&p.tx, &p.view) {
                if cx >= v.x0 && cx < v.x1 && cy >= v.y0 && cy < v.y1 {
                    let _ = tx.send(msg.to_string());
                }
            }
        }
    }
}
