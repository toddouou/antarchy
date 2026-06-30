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

#[derive(Debug, Default)]
pub struct Player {
    /// Redundant with the `players` map key; kept for clarity/Debug. Not read directly.
    #[allow(dead_code)]
    pub id:              u32,
    pub username:        String,
    pub color:           String,
    pub hue_idx:         i32,
    pub ants_avail:      i32,
    /// LEGACY — the old rolling-24 h auto-refill deadline. No longer read for granting (daily
    /// ants are claim-only, keyed off `UserRecord.last_claim_day`); kept so the bincode snapshot
    /// layout stays stable.
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
    /// Fallen-queen death count for this account (incremented by `kill_queen`). Renamed from
    /// `prestige` (the old name) — bincode snapshot field name is unchanged (kept `prestige` in
    /// PlayerSnapshot for layout stability). Drives the in-game death counter / death screen.
    pub deaths:          u32,
    pub nectar:          u64,
    /// Queued shop defenders: each entry is an expiry timestamp (ms). When an enemy
    /// worker nears this player's queen, one is consumed to spawn a free distraction ant.
    pub defenders:       Vec<u64>,
    // ---- Passport (account-level; survive queen death because Player outlives the Queen) ----
    /// Distinct countries / continents this account's queens & ants have set foot in.
    pub passport_countries:   FxHashSet<String>,
    pub passport_continents:  FxHashSet<String>,
    /// Lifetime accumulators folded in from each fallen queen (for the USER-vs-QUEEN compare).
    pub lifetime_kills:      u32,
    pub lifetime_peak_tiles: u64,
    pub queens_fielded:      u32,
    // ---- Admin god-mode (per-target toggles; persisted) ----
    /// When set, this player never spends nectar (shop is free).
    pub unlimited_nectar:    bool,
    /// When set, this player has an infinite worker pool (no ants_avail / army_cap gating).
    pub unlimited_ants:      bool,
    // ---- Rivalries (account-level, keyed by rival username; survive queen death) ----
    /// How many times each named rival's queen has slain THIS account's queen.
    pub killed_by:           FxHashMap<String, u32>,
    /// How many times THIS account's queen has slain each named rival's queen.
    pub kills_of:            FxHashMap<String, u32>,
    /// Snapshot taken at disconnect; diffed on reconnect for the welcome-back summary.
    pub away:                Option<AwaySnapshot>,
    /// Set to `true` on the first `place-queen` of a season and used by `wipe_world` to credit
    /// `UserRecord.seasons_played`. RUNTIME-ONLY — never added to `PlayerSnapshot` or persist.rs,
    /// so a restart doesn't change season accounting (a wipe can only happen while the server runs).
    pub had_queen_this_season: bool,
    /// Runtime cache of the equipped `tile_fx` cosmetic id (e.g. `Some("glow")`), loaded from the
    /// account record on connect and refreshed on equip. Read by `get_fx_palette` each tile cycle so
    /// the per-owner tile effect reaches every viewer. NOT persisted — the authoritative source is
    /// `UserRecord.equipped["tile_fx"]` in users.json.
    pub tile_fx:             Option<String>,
    /// Runtime caches of the equipped `aura` / `trail` cosmetic ids — entity and freshly-painted-tile
    /// decorations. Loaded on connect, refreshed on equip. Read by `network::build_cosmetics_roster`
    /// (~1 Hz, send-on-change) so they reach every viewer without any per-tile bytes. NOT persisted —
    /// authoritative source is `UserRecord.equipped`. (A `recolor` slot is handled differently: it
    /// overrides `color` directly, riding the normal owner palette. The queen-emblem slot was retired.)
    pub aura:                Option<String>,
    pub trail:               Option<String>,
}

/// Snapshot of a player's state at disconnect → diffed on reconnect for the welcome-back summary.
#[derive(Debug, Clone)]
pub struct AwaySnapshot {
    pub at_ms:             u64,
    pub tiles:             u64,
    pub kills:             u32,
    pub level:             u16,
    pub army:              u32,
    pub passport_countries: usize,
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

/// An issued daily-claim nonce: the server's half of the two-step ad-gate handshake.
/// Created by `daily-claim-begin`, consumed by `claim-daily`.  RUNTIME-ONLY — never persisted,
/// so a restart just forces a fresh `daily-claim-begin` (5-minute window is short anyway).
///
/// `verified = true` is the PLUGGABLE SSV HOOK: a future `POST /api/ad-ssv` endpoint would
/// flip this flag after the ad network confirms the rewarded-ad was watched.  For now it is
/// set to `true` immediately so the two-step flow works end-to-end without a live ad network.
#[derive(Debug, Clone)]
pub struct PendingDailyClaim {
    pub pid:        u32,
    pub expires_ms: u64,
    /// Server-side-verified: `true` = the "ad was watched" precondition is satisfied.
    /// Currently always set true at issuance (pluggable hook placeholder; see above).
    pub verified:   bool,
}

// ---- World ----------------------------------------------------------------

/// King-of-the-hill result for one metro: who holds the most painted tiles inside its radius.
#[derive(Debug, Clone)]
pub struct MetroHolder {
    pub name:  String,
    pub owner: Option<u32>,
    pub tiles: u64,
}

/// King-of-the-hill result for one admin-placed monument: its single holder (top tile-owner inside
/// the capture radius) + the sampled tile count. `id` keys back to the `World.monuments` entry (which
/// owns the name/position), so the client matches markers to holders by id.
#[derive(Debug, Clone)]
pub struct MonumentHolder {
    pub id:    u32,
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
    /// Connections currently in queen-PLACEMENT mode (client sent `place-mode {on}`). Members get an
    /// anonymized, foreign-only, von-Neumann-dilated occupancy MASK from `snapshot_view` (no real owner
    /// ids, ants, queens, or fog) so a placing player sees where they can't drop a queen without being
    /// able to reverse-engineer who owns what or where queens sit. Runtime-only, never persisted.
    pub placing_views:   FxHashSet<u32>,
    /// Admins who toggled the fog-of-war PREVIEW on (server-authoritative; gated on `is_admin` in
    /// handlers). Members get the REAL fog field instead of the admin all-zero god-view, so the
    /// operator can see exactly what players see. Runtime-only, never persisted; a non-admin id can
    /// never appear here, so flipping it can't leak hidden tiles to non-admins.
    pub admin_fog_preview: FxHashSet<u32>,
    /// Tracks the last tick on which tiles changed; used to skip viewport delivery when idle.
    pub dirty_tick:      u64,
    /// Metro king-of-the-hill holders, recomputed on a throttle (simulation.rs::recompute_holders).
    pub metro_holders:   Vec<MetroHolder>,
    /// Admin-placed monuments (permanent landmarks). Loaded from `monuments.json` under HIVE_DATA_DIR
    /// on boot (NOT the bincode world.snapshot, and NOT cleared by a wipe), saved atomically on every
    /// admin add/remove/rename. See `crate::monuments`.
    pub monuments:       Vec<crate::monuments::Monument>,
    /// King-of-the-hill holders for `monuments`, recomputed on the same cadence as `metro_holders`.
    pub monument_holders: Vec<MonumentHolder>,
    /// Monotonic id allocator for monuments (never reused, so client markers stay stable).
    pub next_monument_id: u32,
    /// Round-robin cursor into `ants` for throttled passport (visited-region) sampling.
    pub passport_sample_cursor: usize,
    /// Per-account persistent event feed (the in-game EVENTS log). Keyed by username so it survives
    /// queen death / logout; persisted to `events.json` on the autosave + shutdown cadence.
    pub events: crate::events::EventStore,
    /// Ring buffer of recent tick-window durations (ms) for `/health` p50/p99 — the scaling
    /// metric that tells us whether a tick holds its budget at load. `tick_ms_pos` is the
    /// write cursor once the ring fills.
    pub tick_ms_ring: Vec<f32>,
    pub tick_ms_pos:  usize,
    /// Phase-6 snapshot generation tag. Forms the R2 key prefix `snap/{epoch}/…` so a WIPE serves a
    /// fresh tile set instead of a stale cached one. Bumped only on wipe (`rotate_epoch_retiring_old`,
    /// which also queues the old generation for deletion). **Persisted across restarts** via a sidecar
    /// file (`persist::save_epoch`/`load_epoch`) and re-adopted on restore in `main`, so a redeploy
    /// re-uses the existing generation rather than minting a new one and orphaning the old canvas on
    /// R2 forever. Freshly minted (seconds-resolution time seed) only on a genuine fresh start.
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
    /// Pending daily-claim nonces issued by `daily-claim-begin`, keyed by nonce string.
    /// Consumed + removed by `claim-daily` (RUNTIME-ONLY — not persisted; 5-min TTL).
    pub pending_daily_claims: FxHashMap<String, PendingDailyClaim>,
    /// Rolling counter for allocating guest spectator ids in the reserved hi range (`GUEST_ID_BASE+`).
    pub next_guest_seq:  u32,
    /// Anti-replay (OWASP A01/A06): the last accepted client command `seq` per connected player id.
    /// Mutating gameplay messages must carry a strictly increasing `seq`; duplicates / out-of-order
    /// are rejected. Transient (RAM-only, never persisted); reset on (re)connect, cleared on disconnect.
    pub last_seq:        FxHashMap<u32, u64>,
    /// Per-account login throttle (OWASP A07): `ident → (window_start_ms, attempts)`. Bounds password
    /// brute-force per account independently of the per-IP `/api/*` limiter. Transient; GC'd in `api::gc`.
    pub login_attempts:  FxHashMap<String, (u64, u32)>,
    // ---- Alliances (runtime indices; the durable source is `auth.alliances` + `UserRecord.alliance_id`) ----
    /// player id → alliance id. Rebuilt from `auth.alliances` on boot + on every membership change;
    /// the O(1) friend-vs-foe lookup used all over the tick. Never serialized.
    pub player_alliance: FxHashMap<u32, u32>,
    /// United Front: player id → current Phalanx aura stacks (allied queens within range), recomputed
    /// on the holders cadence. Runtime-only.
    pub phalanx_stacks:  FxHashMap<u32, u8>,
    /// Mayday throttle: queen-owner id → last alert timestamp (ms). Runtime-only.
    pub mayday_last:     FxHashMap<u32, u64>,
    /// Clash-conversion digest: (winner_id, loser_id) → (workers_converted, last_x, last_y).
    /// Accumulated by Phase 5 each tick and flushed to the EVENTS log on a throttled cadence
    /// (`flush_conversion_log`) so a running battle reads as one "converted N of X's workers" entry
    /// instead of per-tile spam. Runtime-only (never persisted; cleared on flush + on wipe).
    pub conversion_tally: FxHashMap<(u32, u32), (u32, i32, i32)>,
    /// Organic bots seeded around lone players, queued for staggered arrival (see `crate::bots`).
    /// RUNTIME-ONLY — never persisted; a not-yet-arrived bot simply won't arrive after a restart.
    pub pending_bots:    Vec<crate::bots::PendingBot>,
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
            placing_views:   FxHashSet::default(),
            admin_fog_preview: FxHashSet::default(),
            dirty_tick:      0,
            metro_holders:   Vec::new(),
            monuments:       Vec::new(),
            monument_holders: Vec::new(),
            next_monument_id: 1,
            passport_sample_cursor: 0,
            events: crate::events::EventStore::default(),
            tick_ms_ring:    Vec::with_capacity(TICK_RING_CAP),
            tick_ms_pos:     0,
            epoch:           current_ms() / 1000,
            snapshot_retire: Vec::new(),
            pending_regs:         FxHashMap::default(),
            reset_tokens:         FxHashMap::default(),
            pending_daily_claims: FxHashMap::default(),
            next_guest_seq:  0,
            last_seq:        FxHashMap::default(),
            login_attempts:  FxHashMap::default(),
            player_alliance: FxHashMap::default(),
            phalanx_stacks:  FxHashMap::default(),
            mayday_last:     FxHashMap::default(),
            conversion_tally: FxHashMap::default(),
            pending_bots:    Vec::new(),
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

    /// Roll the Phase-6 snapshot epoch to a fresh value (seconds since the Unix epoch) so clients
    /// never composite a stale season's R2 tiles. Used only via `rotate_epoch_retiring_old` (wipe);
    /// the restart/restore path instead re-adopts the *persisted* epoch (see `Self::epoch`).
    pub fn fresh_epoch(&mut self) {
        self.epoch = crate::config::current_ms() / 1000;
    }

    /// Roll to a fresh epoch **and** queue the outgoing one for R2 deletion. Used by the wipe paths:
    /// once the epoch rolls, every `snap/{old}/…` tile is orphaned, so the snapshot writer reclaims
    /// it. (Restart/restore does NOT roll the epoch — it re-uses the persisted generation — so a
    /// redeploy can't leak a fresh canvas copy onto R2.)
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

    /// Record a structured gameplay event (bold `head` + dashed `sub`) for a player's persistent,
    /// account-level EVENTS feed, AND push it live to them if connected. Account-only: guests and
    /// NPCs have no durable record, so their events are dropped (the durable store is keyed by
    /// username). Admin/system one-liners keep using the ephemeral `{"t":"event","msg":…}` form.
    pub fn log_event(&mut self, player_id: u32, head: String, sub: String) {
        let username = match self.players.get(&player_id) {
            Some(p) if !p.npc && !p.guest => p.username.clone(),
            _ => return,
        };
        let ts = crate::config::current_ms();
        self.events.push(&username, ts, &head, &sub);
        self.send_to(player_id, serde_json::json!({
            "t": "event", "ts": ts, "head": head, "sub": sub,
        }).to_string());
    }

    pub fn broadcast(&self, msg: &str) {
        for p in self.players.values() {
            if let Some(tx) = &p.tx {
                let _ = tx.send(msg.to_string());
            }
        }
    }

    /// Force-disconnect a connected player: send the client a `force-logout` (it toasts the
    /// reason, drops stored creds, and returns to the landing page), then sever the connection's
    /// channels. The write task drains out, and the sim loop's `Cmd::Message` arm drops anything
    /// else that socket sends (it only dispatches while `tx` is `Some`). Used by admin kick/ban
    /// and the world wipe.
    pub fn force_logout(&mut self, pid: u32, reason: &str) {
        if let Some(p) = self.players.get_mut(&pid) {
            if let Some(tx) = &p.tx {
                let _ = tx.send(serde_json::json!({"t":"force-logout","reason":reason}).to_string());
            }
            p.tx           = None;
            p.view_tx      = None;
            p.ctl_tx       = None;
            p.egress_meter = None;
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

    /// Like [`broadcast_ctl`], but **skips guest connections**. Used for the channels that carry
    /// real-world place names (leaderboard `region`, region-holders) — a guest must not receive them:
    /// paired with a queen's public coords (which guests DO get, by id) those names would let a guest
    /// reverse-project the Mercator projection (geo-concealment). The landing spectator renders neither
    /// channel, so skipping guests costs nothing and trims their egress. (Authed `/play` clients still
    /// get both.)
    pub fn broadcast_ctl_nonguests(&self, frame: Arc<[u8]>, json_fallback: &str) {
        for p in self.players.values() {
            if p.guest { continue; }
            if p.bin {
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

    // ---- Alliances (runtime index over auth.alliances) ----

    /// Alliance id for a player, if any (runtime index rebuilt from the persisted roster).
    pub fn alliance_of(&self, pid: u32) -> Option<u32> { self.player_alliance.get(&pid).copied() }

    /// True if `a` and `b` are DISTINCT players sharing an alliance. The hot-path friend test used
    /// throughout the tick; `a == b` is intentionally false (callers handle the same-owner case via
    /// their existing `==` checks).
    pub fn same_alliance(&self, a: u32, b: u32) -> bool {
        if a == b { return false; }
        match (self.player_alliance.get(&a), self.player_alliance.get(&b)) {
            (Some(x), Some(y)) => x == y,
            _ => false,
        }
    }

    /// True if `b`'s tiles / ants / queen should be treated as FRIENDLY by `a` (same owner OR allied).
    pub fn is_friendly(&self, a: u32, b: u32) -> bool { a == b || self.same_alliance(a, b) }

    /// Member player-ids of `pid`'s alliance (incl. self), or `[pid]` if unaffiliated.
    pub fn alliance_member_ids(&self, pid: u32) -> Vec<u32> {
        match self.alliance_of(pid).and_then(|aid| self.auth.alliances.get(&aid)) {
            Some(al) => al.members.clone(),
            None     => vec![pid],
        }
    }

    /// Alliance tier (1..=cap) for a player, or 0 if unaffiliated.
    pub fn alliance_level(&self, pid: u32) -> u16 {
        self.alliance_of(pid)
            .and_then(|aid| self.auth.alliances.get(&aid))
            .map(|a| a.level())
            .unwrap_or(0)
    }

    /// Rebuild the `player_alliance` index from `auth.alliances` (full rebuild). Call after any
    /// membership change and on boot. Also prunes roster ids whose account no longer exists and
    /// deletes any alliance left empty, so the index can never reference a ghost account.
    pub fn rebuild_player_alliance(&mut self) {
        let known: FxHashSet<u32> = self.auth.users.values().map(|u| u.id).collect();
        self.auth.alliances.retain(|_, al| {
            al.members.retain(|id| known.contains(id));
            al.requests.retain(|id| known.contains(id));
            al.invites.retain(|id| known.contains(id));
            if al.members.is_empty() { return false; }
            if !al.members.contains(&al.leader_id) { al.leader_id = al.members[0]; }
            true
        });
        self.player_alliance.clear();
        let pairs: Vec<(u32, u32)> = self.auth.alliances.iter()
            .flat_map(|(&aid, al)| al.members.iter().map(move |&pid| (pid, aid)))
            .collect();
        for (pid, aid) in pairs { self.player_alliance.insert(pid, aid); }
    }

    /// True if (`x`,`y`) lies within any live ENEMY (non-allied) queen's bubble. Gates worker
    /// placement — you can never deploy ants inside a rival queen's spawn zone, but an ALLY's zone is
    /// free real estate (cross-placement).
    pub fn too_close_to_queen(&self, x: i32, y: i32, exclude: u32) -> bool {
        self.queen_zone_overlaps(x, y, 0.0, exclude)
    }

    /// True if a bubble of radius `my_r` centred at (`x`,`y`) would intersect any live OTHER
    /// queen's bubble (circle overlap: centre distance < my_r + theirs). `my_r = 0` degenerates
    /// to a point-in-bubble test. Shared by place-queen and shop relocate so two queens' zones
    /// can never be created overlapping (level growth after placement is allowed).
    pub fn queen_zone_overlaps(&self, x: i32, y: i32, my_r: f64, exclude: u32) -> bool {
        self.queens.iter().filter(|(_, q)| !q.dead).any(|(&qid, q)| {
            // Skip the placer's own queen AND any ally's — allied zones may overlap and are valid
            // placement ground (the queens map is keyed by owner id, so `qid` is the owner).
            if qid == exclude || self.same_alliance(exclude, qid) { return false; }
            let ddx = (q.x + q.size as i32 / 2 - x) as i64;
            let ddy = (q.y + q.size as i32 / 2 - y) as i64;
            ((ddx * ddx + ddy * ddy) as f64).sqrt() < q.bubble_r + my_r
        })
    }

    /// True if any FOREIGN tile (owner != 0, != `pid`, not allied) lies within radius `r` of
    /// (`cx`,`cy`). The queen-placement legality rule: a new/relocated queen's initial range must be
    /// clear of other players' territory (own & allied tiles never block). Early-exits on the first
    /// foreign tile via `TileMap::any_tile_in_circle_where` (1–4 chunks for r≈30; ocean/empty free).
    pub fn range_has_foreign_tile(&self, cx: i32, cy: i32, r: f64, pid: u32) -> bool {
        if r <= 0.0 { return false; }
        let r2 = (r * r).ceil() as i64;
        self.tiles.any_tile_in_circle_where(cx, cy, r2, |o| o != pid && !self.same_alliance(pid, o))
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
    fn range_has_foreign_tile_excludes_self_and_respects_radius() {
        let mut w = World::new();
        // Virgin range → no foreign tiles.
        assert!(!w.range_has_foreign_tile(1000, 1000, 30.0, 7), "empty map is clear");
        // An enemy tile just inside r blocks; just outside does not.
        w.tiles.set(1000 + 20, 1000, 9);
        assert!(w.range_has_foreign_tile(1000, 1000, 30.0, 7), "enemy tile inside r blocks");
        let mut w2 = World::new();
        w2.tiles.set(1000 + 40, 1000, 9);
        assert!(!w2.range_has_foreign_tile(1000, 1000, 30.0, 7), "enemy tile outside r is fine");
        // Own tiles never block.
        let mut w3 = World::new();
        w3.tiles.set(1000 + 5, 1000, 7);
        assert!(!w3.range_has_foreign_tile(1000, 1000, 30.0, 7), "own territory never blocks");
        // r2 boundary exactness: a foreign tile at exactly distance 30 is inside (<= r2).
        let mut w4 = World::new();
        w4.tiles.set(1030, 1000, 9);
        assert!(w4.range_has_foreign_tile(1000, 1000, 30.0, 7), "tile at exactly r counts");
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

    /// Kick/ban path: the client must receive a real `force-logout` frame (NOT the old
    /// empty-string nudge, which just threw inside the client's `JSON.parse` and disconnected
    /// nobody), and every connection channel must drop so the write task exits and the sim loop
    /// stops dispatching that socket's messages (`Cmd::Message` only runs while `tx` is `Some`).
    #[test]
    fn force_logout_notifies_then_severs_channels() {
        let mut w = World::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(4);
        w.players.insert(9, Player {
            id: 9, username: "EVE".into(),
            tx: Some(BoundedTx::new(tx)),
            egress_meter: Some(Arc::new(EgressMeter::default())),
            conn_gen: 1,
            ..Default::default()
        });

        w.force_logout(9, "KICKED BY ADMIN");

        let frame = rx.try_recv().expect("client was sent a frame");
        let v: serde_json::Value = serde_json::from_str(&frame).expect("frame is valid JSON");
        assert_eq!(v["t"], "force-logout");
        assert_eq!(v["reason"], "KICKED BY ADMIN");
        let p = w.players.get(&9).unwrap();
        assert!(p.tx.is_none() && p.view_tx.is_none() && p.ctl_tx.is_none() && p.egress_meter.is_none(),
                "all connection channels severed");
        w.force_logout(12345, "NOBODY");   // unknown pid is a quiet no-op
    }
}
