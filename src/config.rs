use std::sync::{OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard};

pub const ADMIN_USERNAME: &str = "ADMIN";
pub const ADMIN_PASSWORD: &str = "admin";

pub const HUES: &[&str] = &[
    "#ff2e3f","#ff8c42","#ffb800","#f4d35e","#7fb069","#52b788",
    "#43aa8b","#4d908e","#577590","#5e60ce","#7400b8","#9d4edd",
    "#c77dff","#e0aaff","#ff70a6","#ff006e","#fb5607","#ffbe0b",
    "#8338ec","#3a86ff","#06d6a0","#118ab2","#84a98c","#ef476f",
];

pub const ENEMY_HUES: &[&str] = &["#9b3027","#6b4423","#5a4e7c","#3d5a80","#52796f"];

// ---- Shop / credit economy -------------------------------------------------
/// Credits are earned only by killing an enemy queen (+1 each), and never exceed this.
pub const CREDIT_CAP: u64 = 100;
// Shop prices (credits)
pub const PRICE_HIGHWAY:  u64 = 10;
pub const PRICE_RELOCATE: u64 = 20;
pub const PRICE_DEFENDER: u64 = 1;
/// Reserved for the WIP alliance feature; the shop "alliance" item is a no-charge stub
/// (handlers.rs) until alliances ship.
#[allow(dead_code)]
pub const PRICE_ALLIANCE: u64 = 80;
pub const PRICE_BRUTE:    u64 = 20;
pub const PRICE_SHIELD:   u64 = 10;
// Durations (ms)
pub const SHIELD_MS:   u64 = 12 * 3600 * 1000;   // 12h queen shield
pub const DEFENDER_MS: u64 = 3600 * 1000;        // 1h defender decay
// Tunables
pub const DEFENDER_RANGE: i32 = 10;   // enemy worker proximity (tiles) that triggers a defender
pub const HIGHWAY_LEN:    i32 = 150;  // diagonal road length (tiles)
pub const HIGHWAY_NEAR:   i32 = 30;   // start must be within this many tiles of friendly territory
pub const BRUTE_DMG_MULT: f32 = 3.0;  // brute queen-damage multiplier

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
            tick_rate:         50,
            lifespan:   4_320_000,
            bubble_r:          30.0,
            hp_base:            10,
            hp_max:        100_000,
            convert_pct:       0.65,
            daily_ants:        5,
            save_file: "world.snapshot".to_string(),
            capitol_lat: 0.0,
            capitol_lon: 0.0,
            tile_meters: 26.72,
            xp_base:         500.0,
            xp_exp:            2.2,
            xp_level_cap:    100,
            xp_tile_award:   250.0,
            xp_kill:        5000.0,
            xp_convert:        5.0,
            xp_heal:           1.0,
            xp_highway_tick:   1.0,
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
    ("tick_rate",         1.0,       500.0),
    ("lifespan",       1000.0, 8_640_000.0),
    ("bubble_r",          5.0,     5_000.0),
    ("hp_base",           1.0, 1_000_000.0),
    ("hp_max",            1.0, 1_000_000_000.0),
    ("convert_pct",       0.1,         1.0),
    ("daily_ants",        0.0,     1_000.0),
    ("xp_base",           1.0, 1_000_000.0),
    ("xp_exp",            0.5,         5.0),
    ("xp_kill",           0.0, 1_000_000.0),
    ("xp_convert",        0.0,    10_000.0),
    ("xp_tile_award",     0.0,    10_000.0),
    ("levelup_ant_grant", 0.0,       100.0),
    ("spawn_pan",        10.0,    10_000.0),
    ("ant_damage",        0.1,        50.0),
    ("ant_hz",            5.0,        60.0),
    ("ant_view_cap",      0.0,   100_000.0),
    ("hp_regen",          0.0,       100.0),
    ("army_cap",          1.0, 1_000_000.0),
    ("season_secs",       0.0, 31_536_000.0),   // 0 (off) … 365 days
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
        "hp_base"           => c.hp_base            = v as i32,
        "hp_max"            => c.hp_max             = v as i32,
        "convert_pct"       => c.convert_pct        = v,
        "daily_ants"        => c.daily_ants         = v as i32,
        "xp_base"           => c.xp_base            = v,
        "xp_exp"            => c.xp_exp             = v,
        "xp_kill"           => c.xp_kill            = v,
        "xp_convert"        => c.xp_convert         = v,
        "xp_tile_award"     => c.xp_tile_award      = v,
        "levelup_ant_grant" => c.levelup_ant_grant  = v as i32,
        "spawn_pan"         => c.spawn_pan          = v,
        "ant_damage"        => c.ant_damage         = v,
        "ant_hz"            => c.ant_hz             = v as u32,
        "ant_view_cap"      => c.ant_view_cap       = v as u32,
        "hp_regen"          => c.hp_regen           = v,
        "army_cap"          => c.army_cap           = v as i32,
        "season_secs"       => c.season_secs        = v as u64,
        _ => return None,
    }
    Some(v)
}

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

/// Public base URL the **browser** uses to fetch snapshot tiles directly from R2 (the `$0`-egress
/// path; never via the Railway origin). Sent to the client in `logged-in`/`world-info`; empty →
/// the client snapshot compositor stays dormant. Read once.
pub fn snapshot_public_base() -> Option<String> {
    static V: OnceLock<Option<String>> = OnceLock::new();
    V.get_or_init(|| std::env::var("R2_PUBLIC_BASE").ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())).clone()
}

/// Optional licensed/self-hosted base-map tile URL template (Parallel-B). Sent to the client; when
/// empty the client falls back to raw OpenStreetMap. Template may contain `{z}/{x}/{y}` and `{s}`
/// (subdomain). Read once.
pub fn basemap_url() -> Option<String> {
    static V: OnceLock<Option<String>> = OnceLock::new();
    V.get_or_init(|| std::env::var("HIVE_BASEMAP_URL").ok().filter(|s| !s.trim().is_empty())).clone()
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

pub fn reset_to_defaults() -> Vec<(&'static str, f64)> {
    let d = Config::default();
    let vals: &[(&'static str, f64)] = &[
        ("tick_rate",         d.tick_rate as f64),
        ("lifespan",          d.lifespan as f64),
        ("bubble_r",          d.bubble_r),
        ("hp_base",           d.hp_base as f64),
        ("hp_max",            d.hp_max as f64),
        ("convert_pct",       d.convert_pct),
        ("daily_ants",        d.daily_ants as f64),
        ("xp_base",           d.xp_base),
        ("xp_exp",            d.xp_exp),
        ("xp_kill",           d.xp_kill),
        ("xp_convert",        d.xp_convert),
        ("xp_tile_award",     d.xp_tile_award),
        ("levelup_ant_grant", d.levelup_ant_grant as f64),
        ("spawn_pan",         d.spawn_pan),
        ("ant_damage",        d.ant_damage),
        ("ant_hz",            d.ant_hz as f64),
        ("ant_view_cap",      d.ant_view_cap as f64),
        ("hp_regen",          d.hp_regen),
        ("army_cap",          d.army_cap as f64),
        ("season_secs",       d.season_secs as f64),
    ];
    let mut out = Vec::with_capacity(vals.len());
    let mut c = cfg_write();
    c.tick_rate          = d.tick_rate;
    c.lifespan           = d.lifespan;
    c.bubble_r           = d.bubble_r;
    c.hp_base            = d.hp_base;
    c.hp_max             = d.hp_max;
    c.convert_pct        = d.convert_pct;
    c.daily_ants         = d.daily_ants;
    c.xp_base            = d.xp_base;
    c.xp_exp             = d.xp_exp;
    c.xp_kill            = d.xp_kill;
    c.xp_convert         = d.xp_convert;
    c.xp_tile_award      = d.xp_tile_award;
    c.levelup_ant_grant  = d.levelup_ant_grant;
    c.spawn_pan          = d.spawn_pan;
    c.ant_damage         = d.ant_damage;
    c.hp_regen           = d.hp_regen;
    c.army_cap           = d.army_cap;
    c.season_secs        = d.season_secs;
    drop(c);
    for &(k, v) in vals { out.push((k, v)); }
    out
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

/// Queen max-HP for a level. HP grows **exponentially** between two anchors: `hp_base` at level 1
/// and `hp_max` at the level cap (`xp_level_cap`). With the defaults this is 10 HP at L1 →
/// 100,000 HP at L100. `lvl` is clamped to `[1, cap]`.
pub fn max_hp_for_level(lvl: u16, cfg: &Config) -> i32 {
    let cap  = cfg.xp_level_cap.max(2);
    let lvl  = lvl.clamp(1, cap);
    let base = cfg.hp_base.max(1) as f64;
    if lvl <= 1 { return base as i32; }
    let top  = (cfg.hp_max as f64).max(base);
    let t    = (lvl - 1) as f64 / (cap - 1) as f64;   // 0..=1 across [1, cap]
    (base * (top / base).powf(t)).round() as i32
}

pub fn total_xp_for_level(n: u16, cfg: &Config) -> f64 {
    if n <= 1 { return 0.0; }
    (cfg.xp_base * ((n - 1) as f64).powf(cfg.xp_exp)).floor()
}

pub fn level_for_xp(xp: f64, cfg: &Config) -> u16 {
    let mut lvl: u16 = 1;
    while lvl < cfg.xp_level_cap && total_xp_for_level(lvl + 1, cfg) <= xp {
        lvl += 1;
    }
    lvl
}

pub fn calc_score(tiles: u64, queen_placed_at_ms: Option<u64>, kills: u32) -> f64 {
    let secs = queen_placed_at_ms
        .map(|t| (current_ms() - t) / 1000)
        .unwrap_or(0);
    tiles as f64 + secs as f64 * 0.5 + kills as f64 * 500.0
}

pub fn current_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
