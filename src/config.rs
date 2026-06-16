use std::sync::{OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::sync::atomic::{AtomicBool, Ordering};

pub const ADMIN_USERNAME: &str = "ADMIN";
pub const ADMIN_PASSWORD: &str = "admin";
// Built-in NON-admin test account, always recreated like the admin (fresh start / wipe / snapshot
// that omits it). Lets the operator log straight into the normal-player experience (full fog-of-war,
// no god-view) without going through the registration flow. Login: `admin_test` / `test`.
pub const TEST_USERNAME: &str = "ADMIN_TEST";
pub const TEST_PASSWORD: &str = "test";

// 8 muted "starter" colors. Brighter/better colors will be purchasable later (not built yet).
// Keep this in lockstep with the HUES array in public/client.html — same order, same count.
pub const HUES: &[&str] = &[
    "#b5524a","#c08a52","#b8a24a","#6f8f5a",
    "#4a8f86","#4a6f9c","#6b5b95","#8a8a8a",
];

pub const ENEMY_HUES: &[&str] = &["#9b3027","#6b4423","#5a4e7c","#3d5a80","#52796f"];

// ---- Shop / nectar economy -------------------------------------------------
// Nectar is earned by killing an enemy queen (+1 each) and passively from held metro regions.
// Beta 1.0 removed the cap; balances grow with `saturating_add`, so there is no ceiling/overflow.
// Shop prices (nectar)
pub const PRICE_RELOCATE: u64 = 20;
pub const PRICE_DEFENDER: u64 = 1;
pub const PRICE_BRUTE:    u64 = 20;
pub const PRICE_SHIELD:   u64 = 10;
// Durations (ms)
pub const SHIELD_MS:   u64 = 12 * 3600 * 1000;   // 12h queen shield
pub const DEFENDER_MS: u64 = 3600 * 1000;        // 1h defender decay
// Tunables
pub const DEFENDER_RANGE: i32 = 10;   // enemy worker proximity (tiles) that triggers a defender
pub const BRUTE_DMG_MULT: f32 = 10.0;  // brute queen-damage multiplier

// ---- Alliances (late-game co-op layer) -------------------------------------
// Costs are in nectar; the whole economy is gated by `alliance_econ_enabled` (OFF by default for
// testing → create/join are free). Membership + roster persist in users.json (wipe-proof), never in
// the bincode world.snapshot. See src/alliance.rs.
pub const PRICE_ALLIANCE_CREATE: u64 = 50;   // creator pays (when econ enabled)
pub const PRICE_ALLIANCE_JOIN:   u64 = 10;   // applicant pays on request (refunded on reject) / inviter pays on invite
pub const ALLIANCE_MAX_MEMBERS: usize = 10;
pub const ALLIANCE_LEVEL_CAP:   u16 = 5;
// Combined-contribution XP curve (5 tiers). Geometric, like the queen curve but tiny. The XP
// *sources* are tunable in Config (alliance_xp_kill / alliance_xp_per_100k_day); these shape the
// ladder: cumulative L2=50, L3=150, L4=350, L5=750.
pub const ALLIANCE_XP_BASE: f64 = 50.0;   // XP to reach L2
pub const ALLIANCE_XP_EXP:  f64 = 2.0;    // per-tier growth multiplier

// ---- Progressive unlock gates (by peak level — see auth::UserRecord::peak_level) ----
// Each feature/shop item is INVISIBLE + unbuyable until the player's peak level reaches its gate.
// KEEP IN LOCKSTEP with the `GATES` table in public/client.html.
pub const GATE_SHOP:     u16 = 10;  // shop button + access; defender buyable; +1 starter nectar (once)
pub const GATE_WORKER:   u16 = 20;  // "worker" shop item (+1 inventory worker)
pub const GATE_SHIELD:   u16 = 30;
pub const GATE_BRUTE:    u16 = 40;  // brute shop item + sidebar BRUTE toggle
pub const GATE_RELOCATE: u16 = 50;
pub const PRICE_WORKER:  u64 = 10;

/// Required peak level to buy a shop `item`, or `None` if the item is ungated/unknown. The
/// authoritative server-side gate for `shop-buy` (the client also hides locked items, but this is
/// the real boundary so a crafted message can't buy past the gate).
pub fn gate_for_item(item: &str) -> Option<u16> {
    Some(match item {
        "defender" => GATE_SHOP,
        "worker"   => GATE_WORKER,
        "shield"   => GATE_SHIELD,
        "brute"    => GATE_BRUTE,
        "relocate" => GATE_RELOCATE,
        _ => return None,
    })
}

/// True for a safe `#rrggbb` colour string. Used at registration to reject anything that could carry
/// a stored-XSS payload into the client's `style="background:…"`.
pub fn valid_hex_color(c: &str) -> bool {
    let c = c.trim();
    c.len() == 7 && c.starts_with('#') && c[1..].bytes().all(|b| b.is_ascii_hexdigit())
}

/// Lifetime tile-count milestones (rounded "nice numbers"), ascending. Each is awarded **once per
/// queen** — the first tick its peak tile count (`tiles_ever_held`) reaches the threshold. The XP
/// per milestone scales with its 1-based index (`xp_tile_award × index`), so later milestones pay
/// a bit more. See `tick_world` Phase 4.
pub const TILE_MILESTONES: &[u64] = &[
    10_000, 25_000, 50_000, 100_000, 250_000, 500_000,
    1_000_000, 2_500_000, 5_000_000, 10_000_000,
    25_000_000, 50_000_000, 100_000_000, 250_000_000, 500_000_000, 1_000_000_000,
];

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub world_w: u32,
    pub world_h: u32,
    pub spawn_x: u32,
    pub spawn_y: u32,
    pub spawn_pan: f64,
    pub tick_rate: u32,
    pub lifespan: u32,
    pub bubble_r: f64,
    /// Multiplier the placement bubble reaches at the level cap, relative to `bubble_r` (L1).
    /// The radius grows linearly L1 → cap (see `bubble_r_for_level`); 4.0 = quadruple the reach.
    pub bubble_r_level_mult: f64,
    /// Queen max-HP at level 1 (the low anchor of the exponential HP curve).
    pub hp_base: i32,
    /// Queen max-HP at the level cap (the high anchor of the exponential HP curve).
    pub hp_max: i32,
    pub convert_pct: f64,
    pub daily_ants: i32,
    pub save_file: String,
    pub capitol_lat: f64,
    pub capitol_lon: f64,
    pub tile_meters: f64,
    pub xp_base: f64,
    pub xp_exp: f64,
    pub xp_level_cap: u16,
    /// Base XP for the *first* tile milestone; milestone `i` (1-based) grants `xp_tile_award × i`.
    pub xp_tile_award: f64,
    pub xp_kill: f64,
    pub xp_convert: f64,
    pub xp_heal: f64,
    pub xp_highway_tick: f64,
    pub levelup_ant_grant: i32,
    pub ant_damage: f64,
    /// How many times per second ant-position frames are pushed to each viewer (the egress
    /// firehose). Lower = cheaper; the client interpolates between frames so motion stays smooth.
    /// The viewport loop emits one ant frame every `(tick_rate / ant_hz)` ticks. Clamped 5..=60.
    pub ant_hz: u32,
    /// Max ants serialized into one viewport frame; above this the in-view set is subsampled by a
    /// stable id-stride (no per-client flicker). 0 = unlimited. Bounds worst-case dense-battle egress
    /// without thinning normal play.
    pub ant_view_cap: u32,
    /// Passive queen HP regenerated per second while below max. 0 = no regen.
    pub hp_regen: f64,
    /// Max live workers a single player may field at once (deploy blocked at the cap).
    /// Also a scale guardrail. High default so it rarely bites; admin-tunable.
    pub army_cap: i32,
    /// Season length in seconds. When server uptime exceeds it, the world auto-wipes and a
    /// fresh season begins (memory stays flat via chunk compaction). Default 30 days; admin
    /// can lower it for testing. 0 disables the auto-wipe entirely.
    pub season_secs: u64,
    /// Passive nectar granted per real day per 100,000 metro tiles a player holds (leads). Paid once
    /// per 00:00-UTC window in the sim loop. 0 disables passive accrual.
    pub nectar_per_100k_day: f64,
    // ---- Alliances ----
    /// Master switch for alliance nectar costs. OFF by default so create/join are FREE for testing;
    /// flip on for the intended late-game pricing (PRICE_ALLIANCE_*).
    pub alliance_econ_enabled: bool,
    /// United Front / Phalanx: an allied queen within this radius (tiles) of another contributes one
    /// aura stack to it. Only active at alliance L4+.
    pub phalanx_r: f64,
    /// Phalanx bonus per nearby allied queen (fraction; added to both damage and effective HP),
    /// capped at `phalanx_cap`.
    pub phalanx_per_stack: f64,
    pub phalanx_cap: f64,
    /// Mayday fires to all alliance members when a member queen under attack drops below this HP
    /// fraction. 0 disables the auto-alert.
    pub mayday_hp_pct: f64,
    /// Alliance XP granted when a member's queen kills a rival queen.
    pub alliance_xp_kill: f64,
    /// Alliance XP per real day per 100,000 tiles the alliance's members collectively hold.
    pub alliance_xp_per_100k_day: f64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            port: 8080,
            world_w:    1_500_000,
            world_h:      750_000,
            spawn_x:      750_000,
            spawn_y:      375_000,
            spawn_pan:        200.0,
            tick_rate:         15,
            lifespan:   1_296_000,
            bubble_r:          30.0,
            bubble_r_level_mult: 2.5,
            hp_base:            50,
            hp_max:          2_428,
            convert_pct:       0.65,
            daily_ants:        3,
            save_file: "world.snapshot".to_string(),
            capitol_lat: 0.0,
            capitol_lon: 0.0,
            tile_meters: 26.72,
            xp_base:         400.0,
            xp_exp:           1.14,
            xp_level_cap:    100,
            xp_tile_award:   150.0,
            xp_kill:        5000.0,
            xp_convert:        5.0,
            xp_heal:           0.0,
            xp_highway_tick:   0.5,
            levelup_ant_grant: 1,
            ant_damage:        1.0,
            ant_hz:            15,
            ant_view_cap:      4000,
            hp_regen:          0.0,
            army_cap:          1000,
            // 0 disables the automatic season wipe: the world now persists indefinitely and is
            // only cleared by the admin panel's type-"WIPE" button. Admins can re-enable a timed
            // season via the slider (apply_admin_param) if desired.
            season_secs:       0,
            nectar_per_100k_day: 1.0,
            alliance_econ_enabled: false,
            phalanx_r:              600.0,
            phalanx_per_stack:        0.05,
            phalanx_cap:              0.25,
            mayday_hp_pct:            0.5,
            alliance_xp_kill:        10.0,
            alliance_xp_per_100k_day: 5.0,
        }
    }
}

static GLOBAL_CFG: OnceLock<RwLock<Config>> = OnceLock::new();

fn lock() -> &'static RwLock<Config> {
    GLOBAL_CFG.get_or_init(|| {
        let mut c = Config::default();
        // Optional PORT env override — lets a second instance run for testing without
        // disturbing a server already on the default port. Defaults to 8080 in prod.
        if let Ok(p) = std::env::var("PORT") {
            if let Ok(p) = p.parse::<u16>() { c.port = p; }
        }
        // Optional HIVE_DATA_DIR — directory holding the persisted `world.snapshot` + `users.json`.
        // On Railway, point this at a mounted Volume (e.g. /data) so state survives a redeploy;
        // without a Volume the container FS is ephemeral. Defaults to the working directory.
        if let Ok(dir) = std::env::var("HIVE_DATA_DIR") {
            let dir = dir.trim_end_matches(['/', '\\']);
            if !dir.is_empty() { c.save_file = format!("{dir}/world.snapshot"); }
        }
        RwLock::new(c)
    })
}

pub fn cfg() -> RwLockReadGuard<'static, Config> {
    lock().read().unwrap()
}

pub fn cfg_write() -> RwLockWriteGuard<'static, Config> {
    lock().write().unwrap()
}

const ADMIN_CLAMP: &[(&str, f64, f64)] = &[
    ("tick_rate",         1.0,        50.0),
    ("lifespan",       1000.0, 8_640_000.0),
    ("bubble_r",          5.0,     5_000.0),
    ("bubble_r_level_mult", 1.0,      20.0),
    ("hp_base",           1.0, 1_000_000.0),
    ("hp_max",            1.0, 1_000_000_000.0),
    ("convert_pct",       0.1,         1.0),
    ("daily_ants",        0.0,     1_000.0),
    ("xp_base",           1.0, 1_000_000.0),
    ("xp_exp",            1.0,         2.0),
    ("xp_kill",           0.0, 1_000_000.0),
    ("xp_convert",        0.0,    10_000.0),
    ("xp_tile_award",     0.0,    10_000.0),
    ("xp_highway_tick",   0.0,       100.0),
    ("levelup_ant_grant", 0.0,       100.0),
    ("spawn_pan",        10.0,    10_000.0),
    ("ant_damage",        0.1,        50.0),
    ("ant_hz",            5.0,        60.0),
    ("ant_view_cap",      0.0,   100_000.0),
    ("hp_regen",          0.0,       100.0),
    ("army_cap",          1.0, 1_000_000.0),
    ("season_secs",       0.0, 31_536_000.0),   // 0 (off) … 365 days
    ("nectar_per_100k_day", 0.0,   1_000.0),
    ("alliance_econ_enabled", 0.0,          1.0),
    ("phalanx_r",             0.0,     50_000.0),
    ("phalanx_per_stack",     0.0,          1.0),
    ("phalanx_cap",           0.0,          2.0),
    ("mayday_hp_pct",         0.0,          1.0),
    ("alliance_xp_kill",      0.0,  1_000_000.0),
    ("alliance_xp_per_100k_day", 0.0,  100_000.0),
];

/// Returns the clamped value, or None if key is unknown.
pub fn apply_admin_param(key: &str, value: f64) -> Option<f64> {
    let key_lc = key.to_lowercase();
    let key = key_lc.as_str();
    let &(_, lo, hi) = ADMIN_CLAMP.iter().find(|(k, _, _)| *k == key)?;
    let v = value.clamp(lo, hi);
    let mut c = cfg_write();
    match key {
        "tick_rate"         => c.tick_rate          = v as u32,
        "lifespan"          => c.lifespan           = v as u32,
        "bubble_r"          => c.bubble_r           = v,
        "bubble_r_level_mult" => c.bubble_r_level_mult = v,
        "hp_base"           => c.hp_base            = v as i32,
        "hp_max"            => c.hp_max             = v as i32,
        "convert_pct"       => c.convert_pct        = v,
        "daily_ants"        => c.daily_ants         = v as i32,
        "xp_base"           => c.xp_base            = v,
        "xp_exp"            => c.xp_exp             = v,
        "xp_kill"           => c.xp_kill            = v,
        "xp_convert"        => c.xp_convert         = v,
        "xp_tile_award"     => c.xp_tile_award      = v,
        "xp_highway_tick"   => c.xp_highway_tick    = v,
        "levelup_ant_grant" => c.levelup_ant_grant  = v as i32,
        "spawn_pan"         => c.spawn_pan          = v,
        "ant_damage"        => c.ant_damage         = v,
        "ant_hz"            => c.ant_hz             = v as u32,
        "ant_view_cap"      => c.ant_view_cap       = v as u32,
        "hp_regen"          => c.hp_regen           = v,
        "army_cap"          => c.army_cap           = v as i32,
        "season_secs"       => c.season_secs        = v as u64,
        "nectar_per_100k_day" => c.nectar_per_100k_day = v,
        "alliance_econ_enabled" => c.alliance_econ_enabled = v != 0.0,
        "phalanx_r"             => c.phalanx_r             = v,
        "phalanx_per_stack"     => c.phalanx_per_stack     = v,
        "phalanx_cap"           => c.phalanx_cap           = v,
        "mayday_hp_pct"         => c.mayday_hp_pct         = v,
        "alliance_xp_kill"      => c.alliance_xp_kill      = v,
        "alliance_xp_per_100k_day" => c.alliance_xp_per_100k_day = v,
        _ => return None,
    }
    // A tunable changed → flag for the next off-lock persist (server.rs autosave / shutdown), so
    // admin tuning survives a restart instead of reverting to the compiled defaults.
    CONFIG_DIRTY.store(true, Ordering::Relaxed);
    Some(v)
}

/// Set by `apply_admin_param` / `reset_to_defaults` whenever a tunable changes; the sim loop's
/// off-lock autosave (and the shutdown handler) consults it and writes `config.json`. An
/// `AtomicBool` so the WS/sim path never blocks on disk I/O.
pub static CONFIG_DIRTY: AtomicBool = AtomicBool::new(false);

/// Master switch for the Phase-3 binary/compressed wire protocol. The per-connection `bin`
/// capability (client-advertised `{"bin":1}` at auth) is AND-ed with this, so setting
/// `HIVE_BIN_CTL=0` (or `off`/`false`/`no`) forces every client back to the legacy uncompressed
/// text + JSON protocol — an emergency rollback that takes effect on reconnect/redeploy. Read once.
pub fn bin_ctl_enabled() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| match std::env::var("HIVE_BIN_CTL") {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "off" | "false" | "no"),
        Err(_) => true,
    })
}

/// Phase-4 per-connection egress cap in **KB/s**. `None` = disabled (the default), so the cap is
/// dormant until an operator sets `EGRESS_CAP_KBPS`. When set, a connection sending faster than this
/// over the sliding window is progressively down-shifted (tile interval → visible-ant N → ant
/// cadence, never below the ~6 Hz floor); NEVER-DROP one-shots are exempt. Read once.
pub fn egress_cap_kbps() -> Option<f64> {
    static V: OnceLock<Option<f64>> = OnceLock::new();
    *V.get_or_init(|| std::env::var("EGRESS_CAP_KBPS").ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|v| *v > 0.0))
}

/// Phase-∥A WAL persistence. `HIVE_WAL=on/1/true` enables the chunk-delta journal; default **off**,
/// so the proven off-lock full-snapshot save remains the live path. Read once.
pub fn wal_enabled() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| matches!(
        std::env::var("HIVE_WAL").unwrap_or_default().trim().to_ascii_lowercase().as_str(),
        "1" | "on" | "true" | "yes"))
}

/// Phase-6 master switch for the R2 snapshot CDN. `SNAPSHOT_CDN=on/1/true` AND valid R2 creds
/// (`r2_config`) are both required before the snapshot-writer task uploads. Default **off** — the
/// engine ships the pipeline dormant so tomorrow's setup is pure env-vars with zero rebuild.
pub fn snapshot_cdn_enabled() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| matches!(
        std::env::var("SNAPSHOT_CDN").unwrap_or_default().trim().to_ascii_lowercase().as_str(),
        "1" | "on" | "true" | "yes"))
}

/// Optional local directory for the dev snapshot sink: when `HIVE_SNAP_DIR` is set, the writer task
/// rasterizes dirty chunks to `<dir>/snap/{epoch}/{lod}/{cx}/{cy}.png` on disk (no R2 needed) so the
/// rasterizer can be inspected without a Cloudflare account. Read once.
pub fn snapshot_dir() -> Option<String> {
    static V: OnceLock<Option<String>> = OnceLock::new();
    V.get_or_init(|| std::env::var("HIVE_SNAP_DIR").ok().filter(|s| !s.trim().is_empty())).clone()
}

/// Phase-6 snapshot-writer cadence in seconds (R2 Class-A lever A). `HIVE_SNAP_INTERVAL_SECS`,
/// default 300, clamped to [30, 3600]. Longer = fewer PutObject ops, staler cold-load tiles
/// (the live view is WS-authoritative, so staleness only affects pre-first-frame paint). Read once.
pub fn snapshot_interval_secs() -> u64 {
    static V: OnceLock<u64> = OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("HIVE_SNAP_INTERVAL_SECS").ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(300)
            .clamp(30, 3600)
    })
}

/// Territory tile-frame cadence in Hz (egress lever): the viewport loop ships at most one tile
/// (keyframe/delta) frame every `(tick_rate / tile_hz)` ticks. `HIVE_TILE_HZ`, default **5**, clamped
/// [1, 30]. Tile frames are the heavy ones (the full ownership grid + fog), so this is the single
/// biggest authed-player egress knob. The old hard-coded formula was `tick_rate / 10`, which at the
/// live 15 Hz tick degenerated to *every tick* (15 Hz); 5 Hz cuts tile bytes ~3× and — because the
/// smaller, less frequent frames stop saturating the per-conn send queue — actually makes territory
/// *arrive* faster, not slower. Ants ride their own (`ant_hz`) cadence, so motion is unaffected.
/// Read once.
pub fn tile_hz() -> u32 {
    static V: OnceLock<u32> = OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("HIVE_TILE_HZ").ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(5)
            .clamp(1, 30)
    })
}

/// Phase-6 super-tile factor (R2 Class-A lever B): each snapshot PNG covers S×S native chunks
/// (S×256 px square). `HIVE_SNAP_TILE_CHUNKS`, default **8** (→2048 px = 64 chunks/key), restricted
/// to {1,2,4,8,16}; invalid → 8. Higher = fewer Class-A keys when activity clusters (a contiguous
/// frontier collapses up to S² chunks into one PutObject), larger PNGs (transient RGBA buffer =
/// (S*256)²·4 B on the writer thread, one at a time — S=8→16 MB, S=16→64 MB). The bigger cold-load
/// fetch costs only ~free egress/Class-B, so S trades cheap CPU/bytes for the scarce Class-A op. MUST
/// reach the client (sent as `snapTileCells = S*256`) so the browser keys the same `(sx,sy)` tiles.
/// Read once.
pub fn snapshot_tile_chunks() -> u32 {
    static V: OnceLock<u32> = OnceLock::new();
    *V.get_or_init(|| {
        let s = std::env::var("HIVE_SNAP_TILE_CHUNKS").ok()
            .and_then(|s| s.trim().parse::<u32>().ok()).unwrap_or(8);
        if [1, 2, 4, 8, 16].contains(&s) { s } else { 8 }
    })
}

/// Public base URL the **browser** uses to fetch snapshot tiles directly from R2 (the `$0`-egress
/// path; never via the Railway origin). Sent to the client in `logged-in`/`world-info`; empty →
/// the client snapshot compositor stays dormant. Read once.
pub fn snapshot_public_base() -> Option<String> {
    static V: OnceLock<Option<String>> = OnceLock::new();
    V.get_or_init(|| std::env::var("R2_PUBLIC_BASE").ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())).clone()
}

/// Dev-only origin fallback for snapshot tiles. With `HIVE_SNAP_LOCAL=1/on/true/yes` the game server
/// rasterizes territory super-tiles into an in-memory store and serves them itself at `/snap/...`
/// (see `snapshot::MemorySink` + the `/snap/*` route), so the landing page shows painted territory
/// **without** R2 wired. **Opt-in** so prod never accidentally serves tiles from the origin (the whole
/// point of R2 is `$0` egress); prod sets `R2_PUBLIC_BASE` instead and ignores this. Read once.
pub fn snapshot_local_serve() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| matches!(
        std::env::var("HIVE_SNAP_LOCAL").unwrap_or_default().trim().to_ascii_lowercase().as_str(),
        "1" | "on" | "true" | "yes"))
}

/// Base URL actually advertised to the client for snapshot tiles: the real R2 URL when set (prod),
/// else an **empty string** (origin-relative `/snap/...`) when the dev origin fallback is on, else
/// `None` (compositor dormant). An empty-but-`Some` base is still "enabled" — the client keys off
/// presence, not truthiness.
pub fn snapshot_client_base() -> Option<String> {
    snapshot_public_base().or_else(|| snapshot_local_serve().then(String::new))
}

/// Optional licensed/self-hosted base-map tile URL template (Parallel-B). Sent to the client; when
/// empty the client falls back to raw OpenStreetMap. Template may contain `{z}/{x}/{y}` and `{s}`
/// (subdomain). Read once.
pub fn basemap_url() -> Option<String> {
    static V: OnceLock<Option<String>> = OnceLock::new();
    V.get_or_init(|| std::env::var("HIVE_BASEMAP_URL").ok().filter(|s| !s.trim().is_empty())).clone()
}

/// Default half-extent (in tiles) of the camera "home region" a player may pan within, centred on
/// their queen. `HIVE_HOME_RADIUS`, default 7500 (≈ ±200 km at tile_meters≈26.72 → ~400 km box),
/// clamped to [512, world]. The client clamps `view.x/y` to this box (Part 1) so the basemap tile
/// universe — and thus R2 Class-A writes / storage — stays bounded: we're a game, not a map viewer.
/// The effective radius grows with territory (see `build_player_info`), never shrinking below this.
/// Read once.
pub fn home_pan_radius() -> u32 {
    static V: OnceLock<u32> = OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("HIVE_HOME_RADIUS").ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(7500)
            .clamp(512, 1_500_000)
    })
}

/// Extra margin (tiles) added beyond a player's territory bounding-box when growing their pan radius,
/// so the camera can see a little past the frontier. `HIVE_PAN_MARGIN`, default 1024. Read once.
pub fn pan_margin() -> u32 {
    static V: OnceLock<u32> = OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("HIVE_PAN_MARGIN").ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(1024)
            .clamp(0, 100_000)
    })
}

/// R2 / S3-compatible credentials for the snapshot writer. `Some` only when all four vars are set.
#[derive(Clone)]
pub struct R2Config {
    pub endpoint:   String,
    pub bucket:     String,
    pub access_key: String,
    pub secret_key: String,
}

/// Reads R2 creds from the environment (`R2_ENDPOINT`, `R2_BUCKET`, `R2_ACCESS_KEY_ID`,
/// `R2_SECRET_ACCESS_KEY`). `None` if any are missing → the R2 sink is unavailable and the writer
/// falls back to the local-disk or null sink. Read once.
pub fn r2_config() -> Option<R2Config> {
    static V: OnceLock<Option<R2Config>> = OnceLock::new();
    V.get_or_init(|| {
        let g = |k: &str| std::env::var(k).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        Some(R2Config {
            endpoint:   g("R2_ENDPOINT")?,
            bucket:     g("R2_BUCKET")?,
            access_key: g("R2_ACCESS_KEY_ID")?,
            secret_key: g("R2_SECRET_ACCESS_KEY")?,
        })
    }).clone()
}

// ---- beta-v2 auth / landing / spectator env (all read once via OnceLock) -----------------------

/// Resend API key for transactional email (verification codes + reset links). `None` → email send
/// is **dormant** and `email::send_*` logs the code to the server console instead (dev fallback).
pub fn resend_api_key() -> Option<String> {
    static V: OnceLock<Option<String>> = OnceLock::new();
    V.get_or_init(|| std::env::var("HIVE_RESEND_API_KEY").ok().filter(|s| !s.trim().is_empty())).clone()
}

/// Verified sender address Resend mails are sent `from` (e.g. "Antarchy <noreply@antarchy.fun>").
pub fn email_from() -> Option<String> {
    static V: OnceLock<Option<String>> = OnceLock::new();
    V.get_or_init(|| std::env::var("HIVE_EMAIL_FROM").ok().filter(|s| !s.trim().is_empty())).clone()
}

/// SMS/phone verification master switch. Default **off** → the phone-code step is optional (accounts
/// finalize on email verification alone) and `sms::send_code` just logs. Flip on once a provider is
/// wired so phone verification becomes required.
pub fn sms_enabled() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| matches!(
        std::env::var("HIVE_SMS_ENABLED").unwrap_or_default().trim().to_ascii_lowercase().as_str(),
        "1" | "on" | "true" | "yes"))
}

// ---- Argon2id password-hashing parameters (OWASP A07) ------------------------------------------
// OWASP minimum profile by default (m=19 MiB, t=2, p=1); raise via env on a RAM-rich host. Read once.
// Bounds keep `Params::new` valid so the hasher can never panic on a misconfigured value.

/// Argon2id memory cost in KiB. `HIVE_ARGON2_MEM_KIB`, default 19456 (19 MiB), min 8.
pub fn argon2_mem_kib() -> u32 {
    static V: OnceLock<u32> = OnceLock::new();
    *V.get_or_init(|| std::env::var("HIVE_ARGON2_MEM_KIB").ok()
        .and_then(|s| s.trim().parse::<u32>().ok()).unwrap_or(19456).max(8))
}

/// Argon2id time cost (iterations). `HIVE_ARGON2_TIME`, default 2, min 1.
pub fn argon2_time() -> u32 {
    static V: OnceLock<u32> = OnceLock::new();
    *V.get_or_init(|| std::env::var("HIVE_ARGON2_TIME").ok()
        .and_then(|s| s.trim().parse::<u32>().ok()).unwrap_or(2).max(1))
}

/// Argon2id parallelism (lanes). `HIVE_ARGON2_LANES`, default 1, min 1.
pub fn argon2_lanes() -> u32 {
    static V: OnceLock<u32> = OnceLock::new();
    *V.get_or_init(|| std::env::var("HIVE_ARGON2_LANES").ok()
        .and_then(|s| s.trim().parse::<u32>().ok()).unwrap_or(1).max(1))
}

/// Session-token lifetime in hours. `HIVE_SESSION_TTL_HOURS`, default 720 (30 days), min 1.
pub fn session_ttl_hours() -> u64 {
    static V: OnceLock<u64> = OnceLock::new();
    *V.get_or_init(|| std::env::var("HIVE_SESSION_TTL_HOURS").ok()
        .and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(720).max(1))
}

/// Whether session cookies use the hardened `__Host-` prefix + `Secure` (HTTPS-only). Default **off**
/// for local http dev (browsers reject `Secure`/`__Host-` over http); set `HIVE_SECURE_COOKIES=1` in
/// production (behind Caddy TLS). Read once.
pub fn secure_cookies() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| matches!(
        std::env::var("HIVE_SECURE_COOKIES").unwrap_or_default().trim().to_ascii_lowercase().as_str(),
        "1" | "on" | "true" | "yes"))
}

/// The session cookie name: `__Host-`-prefixed (hardened) in prod, plain in http dev.
pub fn session_cookie_name() -> &'static str {
    if secure_cookies() { "__Host-antarchy_session" } else { "antarchy_session" }
}

/// Public base URL of the deployment (e.g. "https://antarchy.fun") used to build verification / reset
/// links in emails. Empty → links fall back to a relative path.
pub fn public_base_url() -> Option<String> {
    static V: OnceLock<Option<String>> = OnceLock::new();
    V.get_or_init(|| std::env::var("HIVE_PUBLIC_BASE_URL").ok()
        .map(|s| s.trim().trim_end_matches('/').to_string()).filter(|s| !s.is_empty())).clone()
}

/// Per-IP rate limit for the `/api/*` auth endpoints, requests per minute. `HIVE_AUTH_RATE_PER_MIN`,
/// default 20, min 1. Guards the cost-amplifying send endpoints (register / forgot-password).
pub fn auth_rate_per_min() -> u32 {
    static V: OnceLock<u32> = OnceLock::new();
    *V.get_or_init(|| std::env::var("HIVE_AUTH_RATE_PER_MIN").ok()
        .and_then(|s| s.trim().parse::<u32>().ok()).unwrap_or(20).max(1))
}

/// EGRESS GUARD — hard ceiling on concurrent guest spectators. `HIVE_MAX_GUESTS`, default 200.
/// Worst-case guest live-egress is bounded by `max_guests × guest_egress_kbps`. Beyond it the landing
/// page degrades to the free path (R2 territory + cached roster). 0 disables guest spectating.
pub fn max_guests() -> usize {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| std::env::var("HIVE_MAX_GUESTS").ok()
        .and_then(|s| s.trim().parse::<usize>().ok()).unwrap_or(200))
}

/// EGRESS GUARD — per-guest egress cap in KB/s, reusing the Phase-4 down-shift. `HIVE_GUEST_EGRESS_KBPS`,
/// default 24. Over-budget guests skip heavy viewport cycles (the watch slot coalesces). Independent of
/// the global `EGRESS_CAP_KBPS` (which stays the players' cap).
pub fn guest_egress_kbps() -> f64 {
    static V: OnceLock<f64> = OnceLock::new();
    *V.get_or_init(|| std::env::var("HIVE_GUEST_EGRESS_KBPS").ok()
        .and_then(|s| s.trim().parse::<f64>().ok()).filter(|v| *v > 0.0).unwrap_or(24.0))
}

/// EGRESS GUARD — max ants shipped to a guest per viewport frame (vs the 4000 player `ant_view_cap`).
/// `HIVE_GUEST_ANT_CAP`, default 400. A tight follow-cam rarely even hits it; the cap bounds the
/// pathological zoomed-in-but-wide guest view.
pub fn guest_ant_cap() -> usize {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| std::env::var("HIVE_GUEST_ANT_CAP").ok()
        .and_then(|s| s.trim().parse::<usize>().ok()).unwrap_or(400).max(1))
}

/// Denial-of-wallet alert (OWASP A09): the snapshot writer logs when R2 Class-A ops exceed this rate
/// (ops/min) over a cycle — catching a runaway write loop or wipe-storm before the invoice does.
/// `HIVE_CLASSA_ALERT_PER_MIN`, default 600 (well above steady-state ~tens/cycle). 0 disables. Read once.
pub fn classa_alert_per_min() -> u64 {
    static V: OnceLock<u64> = OnceLock::new();
    *V.get_or_init(|| std::env::var("HIVE_CLASSA_ALERT_PER_MIN").ok()
        .and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(600))
}

/// Egress alert: log when a single connection's send rate exceeds this (KB/s). `HIVE_EGRESS_ALERT_KBPS`,
/// default 0 = off (the WS layer is already bounded by `EGRESS_CAP_KBPS` / guest knobs). Read once.
pub fn egress_alert_kbps() -> f64 {
    static V: OnceLock<f64> = OnceLock::new();
    *V.get_or_init(|| std::env::var("HIVE_EGRESS_ALERT_KBPS").ok()
        .and_then(|s| s.trim().parse::<f64>().ok()).filter(|v| *v > 0.0).unwrap_or(0.0))
}

// ---- AoI / map-hack guards (OWASP A01) ---------------------------------------------------------

/// Hard cap on the **live viewport span** in world tiles. The client's requested view rectangle is
/// clamped (center-preserving) to at most this on each axis *before* it decides which live ants/queens
/// are serialized — independent of client zoom. This is what stops "zoom out to see the whole map's
/// enemy queens": zoomed-out territory still renders from the free R2 super-tiles, but live entities
/// never leak beyond the bound. `HIVE_MAX_VIEW_SPAN`, default 4000, min 64. Read once.
pub fn max_view_span() -> i32 {
    static V: OnceLock<i32> = OnceLock::new();
    *V.get_or_init(|| std::env::var("HIVE_MAX_VIEW_SPAN").ok()
        .and_then(|s| s.trim().parse::<i32>().ok()).unwrap_or(4000).max(64))
}

/// Hard cap on the number of live queens serialized into a single viewport frame (defense-in-depth
/// for AoI: even a crafted view can't enumerate every queen). Nearest-to-centre are kept; the
/// viewer's own / revealed queens are always kept. `HIVE_MAX_QUEENS_PER_FRAME`, default 256, min 1.
pub fn max_queens_per_frame() -> usize {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| std::env::var("HIVE_MAX_QUEENS_PER_FRAME").ok()
        .and_then(|s| s.trim().parse::<usize>().ok()).unwrap_or(256).max(1))
}

/// Center-preserving clamp of a requested view rectangle so neither axis span exceeds
/// [`max_view_span`]. Also normalizes (`x0<=x1`, `y0<=y1`). The single source of truth used by both
/// `view-set` (on store) and `snapshot_view` (defensively, on read). Computed in `i64` so an
/// adversarial rect (e.g. spanning the whole `i32` range) can't overflow on the subtraction.
pub fn clamp_view_span(x0: i32, y0: i32, x1: i32, y1: i32) -> (i32, i32, i32, i32) {
    let max = max_view_span() as i64;
    let (mut x0, mut x1) = (x0 as i64, x1 as i64);
    let (mut y0, mut y1) = (y0 as i64, y1 as i64);
    if x0 > x1 { std::mem::swap(&mut x0, &mut x1); }
    if y0 > y1 { std::mem::swap(&mut y0, &mut y1); }
    let w = x1 - x0;
    if w > max { let cx = x0 + w / 2; x0 = cx - max / 2; x1 = x0 + max; }
    let h = y1 - y0;
    if h > max { let cy = y0 + h / 2; y0 = cy - max / 2; y1 = y0 + max; }
    (x0 as i32, y0 as i32, x1 as i32, y1 as i32)
}

// ---- WebSocket hardening (OWASP A02/A10) -------------------------------------------------------

/// Max inbound WebSocket message/frame size in bytes — gameplay JSON is tiny, so this rejects
/// oversized payloads (memory/CPU DoS) by closing the socket. `HIVE_WS_MAX_MSG`, default 65536,
/// min 1024. Read once.
pub fn ws_max_msg() -> usize {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| std::env::var("HIVE_WS_MAX_MSG").ok()
        .and_then(|s| s.trim().parse::<usize>().ok()).unwrap_or(65536).max(1024))
}

/// Per-connection outbound send-queue depth (backpressure cap). When a connection's priority/control
/// queue is full, further messages are dropped rather than buffered — so a slow/malicious consumer
/// can't grow server memory without bound. `HIVE_WS_SEND_QUEUE`, default 1024, min 16. Read once.
pub fn ws_send_queue() -> usize {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| std::env::var("HIVE_WS_SEND_QUEUE").ok()
        .and_then(|s| s.trim().parse::<usize>().ok()).unwrap_or(1024).max(16))
}

/// Idle timeout (seconds): if no inbound frame — including the pong to our keepalive ping — arrives
/// within this window, the socket is closed (slowloris / dead-peer reclaim). `HIVE_WS_IDLE_SECS`,
/// default 60, min 10. Read once.
pub fn ws_idle_secs() -> u64 {
    static V: OnceLock<u64> = OnceLock::new();
    *V.get_or_init(|| std::env::var("HIVE_WS_IDLE_SECS").ok()
        .and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(60).max(10))
}

/// Allowlisted browser `Origin`s for the WebSocket upgrade (anti-CSWSH). Comma-separated
/// `HIVE_ALLOWED_ORIGINS`; default = the public base URL (if set) + `localhost`/`127.0.0.1` on the
/// bound port. An ABSENT `Origin` (non-browser client / same-origin navigation) is allowed —
/// cross-site WS hijack requires a browser, which always sends `Origin`; a PRESENT one must match.
pub fn allowed_origins() -> &'static [String] {
    static V: OnceLock<Vec<String>> = OnceLock::new();
    V.get_or_init(|| {
        if let Ok(s) = std::env::var("HIVE_ALLOWED_ORIGINS") {
            return s.split(',')
                .map(|x| x.trim().trim_end_matches('/').to_string())
                .filter(|x| !x.is_empty())
                .collect();
        }
        let mut v = Vec::new();
        if let Some(base) = public_base_url() { v.push(base); }
        let port = cfg().port;
        v.push(format!("http://localhost:{port}"));
        v.push(format!("http://127.0.0.1:{port}"));
        v
    })
}

/// All admin-tunable params as `(key, value)` for a given `Config` — the single source of truth for
/// the slider set, the persisted `config.json`, and the reset echo. Env/launch-derived fields
/// (`port`, `save_file`, world geometry, geo projection) are deliberately excluded: they come from
/// the environment, not the admin panel, and must not be clobbered by a persisted file.
fn params_of(c: &Config) -> Vec<(&'static str, f64)> {
    vec![
        ("tick_rate",         c.tick_rate as f64),
        ("lifespan",          c.lifespan as f64),
        ("bubble_r",          c.bubble_r),
        ("bubble_r_level_mult", c.bubble_r_level_mult),
        ("hp_base",           c.hp_base as f64),
        ("hp_max",            c.hp_max as f64),
        ("convert_pct",       c.convert_pct),
        ("daily_ants",        c.daily_ants as f64),
        ("xp_base",           c.xp_base),
        ("xp_exp",            c.xp_exp),
        ("xp_kill",           c.xp_kill),
        ("xp_convert",        c.xp_convert),
        ("xp_tile_award",     c.xp_tile_award),
        ("xp_highway_tick",   c.xp_highway_tick),
        ("levelup_ant_grant", c.levelup_ant_grant as f64),
        ("spawn_pan",         c.spawn_pan),
        ("ant_damage",        c.ant_damage),
        ("ant_hz",            c.ant_hz as f64),
        ("ant_view_cap",      c.ant_view_cap as f64),
        ("hp_regen",          c.hp_regen),
        ("army_cap",          c.army_cap as f64),
        ("season_secs",       c.season_secs as f64),
        ("nectar_per_100k_day", c.nectar_per_100k_day),
        ("alliance_econ_enabled", if c.alliance_econ_enabled { 1.0 } else { 0.0 }),
        ("phalanx_r",             c.phalanx_r),
        ("phalanx_per_stack",     c.phalanx_per_stack),
        ("phalanx_cap",           c.phalanx_cap),
        ("mayday_hp_pct",         c.mayday_hp_pct),
        ("alliance_xp_kill",      c.alliance_xp_kill),
        ("alliance_xp_per_100k_day", c.alliance_xp_per_100k_day),
    ]
}

/// Current value of every admin-tunable param — drives `save_config` and the full-config push the
/// client uses to hydrate its sliders (`build_player_info`'s `me.cfg`).
pub fn tunable_params() -> Vec<(&'static str, f64)> { params_of(&cfg()) }

pub fn reset_to_defaults() -> Vec<(&'static str, f64)> {
    let d = Config::default();
    // Apply each default through the clamping setter so `params_of` stays the ONLY enumeration of
    // the tunable set — no parallel assignment block to drift (the previous one silently omitted
    // `ant_hz`/`ant_view_cap`, so a reset left them untouched). `apply_admin_param` also flags
    // `CONFIG_DIRTY`, so the reset itself persists.
    for &(k, v) in &params_of(&d) { apply_admin_param(k, v); }
    params_of(&d)
}

/// Path for the persisted tunable config — beside `users.json` / `world.snapshot` (under HIVE_DATA_DIR).
fn config_path() -> String { cfg().save_file.replace("world.snapshot", "config.json") }

/// Persist the current tunable params to `config.json` (atomic temp + rename). Tiny (~20 floats), so
/// it is safe to call off the world lock — from the sim loop's off-lock autosave when `CONFIG_DIRTY`
/// is set, from the shutdown handler, and after a reset — so admin tuning survives a restart instead
/// of reverting to the compiled defaults.
pub fn save_config() { save_config_to(&config_path()); }

/// `save_config` with an explicit path (test seam — the public version targets `config_path()`).
fn save_config_to(path: &str) {
    let map: serde_json::Map<String, serde_json::Value> = tunable_params().into_iter()
        .map(|(k, v)| (k.to_string(), serde_json::json!(v)))
        .collect();
    let body = match serde_json::to_string_pretty(&serde_json::Value::Object(map)) {
        Ok(s) => s,
        Err(e) => { eprintln!("[config] serialize failed: {e}"); return; }
    };
    let tmp = format!("{path}.tmp");
    if let Err(e) = std::fs::write(&tmp, body) { eprintln!("[config] write {tmp} failed: {e}"); return; }
    if let Err(e) = std::fs::rename(&tmp, path) { eprintln!("[config] rename → {path} failed: {e}"); }
}

/// Load persisted tunable params from `config.json` on boot, applying each through the clamping
/// setter. Missing/corrupt file → keep the compiled defaults (no error). Unknown keys are ignored
/// and absent keys keep their default, so the file stays forward/backward compatible across versions.
pub fn load_config() {
    let path = config_path();
    let data = match std::fs::read_to_string(&path) {
        Ok(d) => d,
        Err(_) => { println!("[config] no config.json at {path} — using defaults"); return; }
    };
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(&data) else {
        eprintln!("[config] {path} is not a JSON object — ignoring"); return;
    };
    let mut applied = 0u32;
    for (k, v) in map {
        if let Some(n) = v.as_f64() { if apply_admin_param(&k, n).is_some() { applied += 1; } }
    }
    // Loading is not a user edit — clear the dirty flag the applies above just set, so boot doesn't
    // immediately rewrite an identical file.
    CONFIG_DIRTY.store(false, Ordering::Relaxed);
    println!("[config] loaded {applied} tunables from {path}");
}

/// Placement-bubble radius for a queen at level `lvl`. Grows **linearly** from the base `bubble_r`
/// (L1) to `bubble_r × bubble_r_level_mult` at the level cap (`xp_level_cap`), so higher queens
/// project — and can place into — a larger ring. With the defaults: 30 tiles @ L1 → 120 @ L100.
/// `lvl` is clamped to `[1, cap]`.
pub fn bubble_r_for_level(lvl: u16, cfg: &Config) -> f64 {
    let cap  = cfg.xp_level_cap.max(2);
    let lvl  = lvl.clamp(1, cap);
    let base = cfg.bubble_r;
    if lvl <= 1 { return base; }
    let top  = base * cfg.bubble_r_level_mult.max(1.0);
    let t    = (lvl - 1) as f64 / (cap - 1) as f64;   // 0..=1 across [1, cap]
    base + (top - base) * t
}

pub fn queen_size_for_level(lvl: u16) -> u8 {
    if lvl >= 100 { 8 }
    else if lvl >= 90 { 7 }
    else if lvl >= 75 { 6 }
    else if lvl >= 50 { 5 }
    else if lvl >= 25 { 4 }
    else if lvl >= 10 { 3 }
    else { 2 }
}

// ---- Level-scaled fog-of-war radii (tiles, measured from nearest owned territory) ----
// The clear radius (full detail) grows LINEARLY with level so higher-level players see farther.
// Fog (solid grey) begins after a short feather past the clear edge; the void (client stops
// fetching tiles/OSM/entities) begins past `void` buffer. See fog.rs + the client void gate.
// Constants kept here (not Config/sliders yet) for a focused change — trivial to promote later.
const FOG_CLEAR_BASE:  f32 = 45.0;   // clear radius at level 1 (~+50% — more to look at)
const FOG_CLEAR_MAX:   f32 = 150.0;  // clear radius at the level cap
// TIGHT reveal: full detail out to `clearR`, then a feather where fog ramps to fully opaque. The
// feather end is also the OSM/tile/viewport boundary and the camera leash — everything stops at the
// same radius, so the map is only ever visible in a ring around your tiles (no wide gradient band,
// no map bleeding into the distance). Kept additive (clearR + feather), not a multiple of clearR, so
// the ring stays a constant width at every level. Feather is a touch wider so the drifting-mist
// "clouds part" edge is a soft gradient rather than a hard line.
const FOG_FEATHER: f32 = 20.0;       // feather width (tiles): clearR → clearR+this = fully opaque

/// Clear-zone radius (tiles) for a queen at `level` — full detail out to here. Linear from
/// `FOG_CLEAR_BASE` (L1) to `FOG_CLEAR_MAX` at the level cap. `level` clamped to `[1, cap]`.
pub fn fog_clear_r(level: u16, cfg: &Config) -> f32 {
    let cap = cfg.xp_level_cap.max(2);
    let lvl = level.clamp(1, cap);
    let t = (lvl - 1) as f32 / (cap - 1) as f32;   // 0..=1 across [1, cap]
    FOG_CLEAR_BASE + (FOG_CLEAR_MAX - FOG_CLEAR_BASE) * t
}

/// Outer reveal radius (tiles): fog reaches FULLY OPAQUE here — the end of the short feather past
/// `clearR`. Also the OSM fetch/draw boundary, the server-tile viewport boundary, and the camera
/// leash: everything stops at the same radius, so the map is only visible in the tight ring around
/// your tiles and the feather meets the dark-slate fog seamlessly.
pub fn fog_grad_r(level: u16, cfg: &Config) -> f32 { fog_clear_r(level, cfg) + FOG_FEATHER }

/// Void threshold == the outer reveal radius (everything beyond is solid fog, fetched/drawn nothing).
pub fn fog_void_r(level: u16, cfg: &Config) -> f32 { fog_grad_r(level, cfg) }

/// Queen max-HP for a level. HP grows **exponentially** between two anchors: `hp_base` at level 1
/// and `hp_max` at the level cap (`xp_level_cap`). With the defaults this is 50 HP at L1 →
/// 2,428 HP at L100 (a constant ≈+4%/level), under a 2,500 hard ceiling. `lvl` is clamped to
/// `[1, cap]`.
pub fn max_hp_for_level(lvl: u16, cfg: &Config) -> i32 {
    let cap  = cfg.xp_level_cap.max(2);
    let lvl  = lvl.clamp(1, cap);
    let base = cfg.hp_base.max(1) as f64;
    if lvl <= 1 { return base as i32; }
    let top  = (cfg.hp_max as f64).max(base);
    let t    = (lvl - 1) as f64 / (cap - 1) as f64;   // 0..=1 across [1, cap]
    (base * (top / base).powf(t)).round() as i32
}

/// Cumulative XP required to *reach* level `n` (a running total; queen.xp stores this directly).
/// `xp_exp` is the **per-level growth rate** (1.07 = +7%/level), so the cost of the single level
/// `L→L+1` is `total(L+1) − total(L) = xp_base · rate^(L-1)`, and this closed-form geometric sum is
/// its running total. With the defaults: L2 = 400, L100 ≈ 1.23 billion (very back-loaded so
/// mid/late levels are a long grind). `n ≤ 1 → 0`.
pub fn total_xp_for_level(n: u16, cfg: &Config) -> f64 {
    if n <= 1 { return 0.0; }
    let rate  = cfg.xp_exp;          // per-level XP growth multiplier (1.07 = +7%/level)
    let steps = (n - 1) as f64;
    if (rate - 1.0).abs() < 1e-9 {
        (cfg.xp_base * steps).floor()                          // degenerate (no growth): linear
    } else {
        (cfg.xp_base * (rate.powf(steps) - 1.0) / (rate - 1.0)).floor()
    }
}

pub fn level_for_xp(xp: f64, cfg: &Config) -> u16 {
    let mut lvl: u16 = 1;
    while lvl < cfg.xp_level_cap && total_xp_for_level(lvl + 1, cfg) <= xp {
        lvl += 1;
    }
    lvl
}

// ---- Alliance progression & buffs ------------------------------------------

/// Per-tier passive buffs applied to every member's queen/ants. `level == 0` = not in an alliance
/// (neutral identity), so callers can always look up by a player's alliance level unconditionally.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AllianceBuffs {
    pub dmg_mult: f64,       // × ant bite damage
    pub hp_mult: f64,        // × queen max-HP
    pub shield_mult: f64,    // × shield duration on cast
    pub defender_bonus: i32, // + defender trigger range (tiles)
    pub phalanx: bool,       // United Front aura active (L4+)
}

impl Default for AllianceBuffs {
    fn default() -> Self {
        AllianceBuffs { dmg_mult: 1.0, hp_mult: 1.0, shield_mult: 1.0, defender_bonus: 0, phalanx: false }
    }
}

/// The 5-tier buff ladder. L1 "Pact" is mostly the unlock of the four co-op rules (the cooperation
/// *is* the reward); raw stats back-load toward L5 "Sovereign". Numbers are intentionally modest —
/// stacked on level-scaled queens they still matter, and Phalanx multiplies them inside a cluster.
pub fn alliance_buffs(level: u16) -> AllianceBuffs {
    match level {
        0 => AllianceBuffs::default(),
        1 => AllianceBuffs { dmg_mult: 1.00, hp_mult: 1.05, shield_mult: 1.00, defender_bonus: 0, phalanx: false },
        2 => AllianceBuffs { dmg_mult: 1.08, hp_mult: 1.10, shield_mult: 1.00, defender_bonus: 0, phalanx: false },
        3 => AllianceBuffs { dmg_mult: 1.12, hp_mult: 1.15, shield_mult: 1.25, defender_bonus: 0, phalanx: false },
        4 => AllianceBuffs { dmg_mult: 1.18, hp_mult: 1.20, shield_mult: 1.50, defender_bonus: 2, phalanx: true },
        _ => AllianceBuffs { dmg_mult: 1.25, hp_mult: 1.30, shield_mult: 2.00, defender_bonus: 4, phalanx: true },
    }
}

/// Cumulative alliance XP required to *reach* tier `n` (geometric, like the queen curve but tiny;
/// capped at ALLIANCE_LEVEL_CAP). `n ≤ 1 → 0`.
pub fn alliance_total_xp_for_level(n: u16) -> f64 {
    if n <= 1 { return 0.0; }
    let steps = (n - 1) as f64;
    (ALLIANCE_XP_BASE * (ALLIANCE_XP_EXP.powf(steps) - 1.0) / (ALLIANCE_XP_EXP - 1.0)).floor()
}

/// Alliance tier (1..=ALLIANCE_LEVEL_CAP) for a cumulative XP total.
pub fn alliance_level_for_xp(xp: f64) -> u16 {
    let mut lvl = 1u16;
    while lvl < ALLIANCE_LEVEL_CAP && alliance_total_xp_for_level(lvl + 1) <= xp { lvl += 1; }
    lvl
}

pub fn calc_score(tiles: u64, queen_placed_at_ms: Option<u64>, kills: u32) -> f64 {
    let secs = queen_placed_at_ms
        .map(|t| current_ms().saturating_sub(t) / 1000)   // saturating: tolerate a backwards clock step
        .unwrap_or(0);
    tiles as f64 + secs as f64 * 0.5 + kills as f64 * 500.0
}

pub fn current_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// UTC day number (days since the Unix epoch) for an epoch-ms timestamp. The daily-claim window:
/// one ant portion is claimable per UTC day, resetting at **00:00 UTC** sharp.
pub fn utc_day(ms: u64) -> u64 { ms / 86_400_000 }

/// Epoch-ms of the next 00:00 UTC after `ms` — the client countdown target for the daily claim.
pub fn next_utc_midnight_ms(ms: u64) -> u64 { (utc_day(ms) + 1) * 86_400_000 }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_day_windows_split_exactly_at_midnight() {
        const DAY: u64 = 86_400_000;
        let d = 20_000u64; // an arbitrary UTC day number
        assert_eq!(utc_day(d * DAY), d, "00:00:00.000 starts the new window");
        assert_eq!(utc_day(d * DAY - 1), d - 1, "23:59:59.999 is still the old window");
        assert_eq!(utc_day(d * DAY + DAY - 1), d, "the whole day maps to one window");
        assert_eq!(next_utc_midnight_ms(d * DAY), (d + 1) * DAY);
        assert_eq!(next_utc_midnight_ms(d * DAY + DAY - 1), (d + 1) * DAY, "1 ms before reset");
    }

    /// THE AoI guarantee: a max-zoom-out / map-hack view spanning millions of tiles is clamped to at
    /// most `max_view_span` on each axis, centred on the request — so live entities can never leak
    /// beyond the server bound regardless of what the client claims.
    #[test]
    fn clamp_view_span_caps_a_giant_rect_around_its_center() {
        let max = max_view_span();
        let (cx, cy) = (750_000, 375_000);
        let (x0, y0, x1, y1) =
            clamp_view_span(cx - 5_000_000, cy - 5_000_000, cx + 5_000_000, cy + 5_000_000);
        assert!(x1 - x0 <= max, "x span clamped to <= {max}, got {}", x1 - x0);
        assert!(y1 - y0 <= max, "y span clamped to <= {max}, got {}", y1 - y0);
        assert!(((x0 + x1) / 2 - cx).abs() <= 1, "centre preserved");
        assert!(((y0 + y1) / 2 - cy).abs() <= 1, "centre preserved");
    }

    #[test]
    fn clamp_view_span_leaves_small_rect_untouched_but_normalizes() {
        assert_eq!(clamp_view_span(100, 200, 900, 1000), (100, 200, 900, 1000));
        assert_eq!(clamp_view_span(900, 1000, 100, 200), (100, 200, 900, 1000));
    }

    #[test]
    fn clamp_view_span_handles_full_i32_range_without_overflow() {
        let (x0, y0, x1, y1) = clamp_view_span(i32::MIN, i32::MIN, i32::MAX, i32::MAX);
        assert!(x1 - x0 <= max_view_span());
        assert!(y1 - y0 <= max_view_span());
    }

    /// `params_of` (drives save/load/reset + the client cfg push) and `ADMIN_CLAMP` (the slider
    /// clamp and `apply_admin_param` arms) must enumerate the SAME tunable set. When they drift, a
    /// param becomes unpersistable / unresettable / un-editable — exactly the bug class this guards
    /// (`reset_to_defaults` previously omitted `ant_hz`/`ant_view_cap`).
    #[test]
    fn params_of_and_admin_clamp_enumerate_the_same_tunables() {
        use std::collections::BTreeSet;
        let d = Config::default();
        let params: BTreeSet<&str> = params_of(&d).into_iter().map(|(k, _)| k).collect();
        let clamp:  BTreeSet<&str> = ADMIN_CLAMP.iter().map(|(k, _, _)| *k).collect();
        assert_eq!(params, clamp, "params_of vs ADMIN_CLAMP drift");
        // Every tunable must round-trip through the clamping setter (no typo'd/unhandled key).
        for &(k, v) in &params_of(&d) {
            assert!(apply_admin_param(k, v).is_some(), "apply_admin_param missing arm for {k}");
        }
    }

    /// `save_config` writes a JSON object carrying every tunable (incl. the new `xp_highway_tick`),
    /// readable back as an object — the persistence half of the "settings don't survive a restart"
    /// fix. Reads global cfg only (no mutation), so it can't race other tests.
    #[test]
    fn save_config_writes_every_tunable_as_json() {
        let dir = std::env::temp_dir().join(format!("antarchy-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let ps = path.to_str().unwrap();
        save_config_to(ps);
        let body = std::fs::read_to_string(ps).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).expect("config.json is valid JSON");
        for (k, _) in params_of(&Config::default()) {
            assert!(v.get(k).and_then(|x| x.as_f64()).is_some(), "saved config missing tunable {k}");
        }
        assert!(v.get("xp_highway_tick").is_some(), "new paint-XP knob must persist");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Locks the "very hard" difficulty intent so a future edit can't silently soften it:
    /// the first level still costs exactly `xp_base`, the curve is steeply back-loaded into the
    /// billions at the cap, and the passive self-heal XP is OFF (a queen can't level by sitting
    /// still). See the level-curve hardening pass.
    #[test]
    fn default_curve_is_steep_and_idle_heal_is_off() {
        let d = Config::default();
        assert_eq!(total_xp_for_level(2, &d), d.xp_base,
            "cost of L1→L2 must equal xp_base");
        assert!(total_xp_for_level(d.xp_level_cap, &d) > 1.0e9,
            "L{} must cost > 1 billion XP (very back-loaded curve), got {}",
            d.xp_level_cap, total_xp_for_level(d.xp_level_cap, &d));
        assert_eq!(d.xp_heal, 0.0, "idle self-heal XP must be off");
    }
}
