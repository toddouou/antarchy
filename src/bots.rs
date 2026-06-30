//! Organic bots — believable opponents seeded around isolated players.
//!
//! A player who deploys their queen in an empty region (a desert, an unpopulated coastline) has
//! nobody to paint against, so the area is dead and they bounce. To **guarantee everyone gets a
//! piece of the action**, when a queen is placed with NO other live queen inside [`LONE_RADIUS_MI`],
//! the server quietly seeds 2–3 bots in a 5–7-mile ring around them (see
//! [`maybe_spawn_for_lone_queen`]). When their territories grow they "run into each other," which
//! nudges players to group up with their spawn instead of homesteading a void.
//!
//! Bots are an evolution of the static admin NPCs (`p.npc = true`), with three differences:
//!  1. **Organic** — they spawn near lone players, not just by admin command (silently — no global
//!     `NPC SPAWNED` broadcast).
//!  2. **Indistinguishable** — realistic usernames drawn from [`names`], they take real leaderboard
//!     spots, and the wire `npc` tell is suppressed for them (see `network::build_leaderboard`). The
//!     ONLY pre-launch tell is the [`BOT_MARKER`] suffix on their name; set it to `""` for go-live.
//!  3. **Alive** — [`maintain`] (called off the hot tick path from `sim_loop`) keeps each bot's ant
//!     population topped up and lets it slowly gain level/territory and climb the board like a casual
//!     human. Bots only coexist & expand — no targeted hunting; conflict emerges where paint meets.
//!
//! Engine semantics are unchanged: bots carry `p.npc = true`, so they are purged on death
//! (`purge_dead_npc`), cleared by `wipe_world`, and excluded from daily claims / the online count
//! exactly like the old NPCs. The only non-durable addition is the staggered-arrival queue
//! ([`World::pending_bots`]), which degrades safely on restart (a not-yet-arrived bot just won't
//! arrive).

use std::sync::OnceLock;

use rand::Rng;

use crate::config::{cfg, current_ms, level_for_xp, total_xp_for_level, ENEMY_HUES};
use crate::world::{Ant, Player, Queen, World};

// ---- Tunables ---------------------------------------------------------------
// Kept as in-module consts (not admin sliders) so the change stays surgical; deploy is a rebuild
// anyway. Promote to `Config` later if live tuning is wanted.

/// TEMPORARY pre-launch tell appended to every bot's name on the wire. Set to `""` for go-live to
/// make bots fully indistinguishable from humans. This is the ONLY thing that marks a bot.
pub const BOT_MARKER: &str = "*";
/// Master switch for the organic-bot system (lone-player seeding + the maintenance pass).
pub const BOTS_ENABLED: bool = true;
/// A queen placed with no other live queen within this radius is "lone" → seed bots around it.
const LONE_RADIUS_MI: f64 = 10.0;
/// Bots spawn on a ring this far from the lone queen (so paint takes a while to collide).
const RING_MIN_MI: f64 = 5.0;
const RING_MAX_MI: f64 = 7.0;
/// Inclusive range of bots seeded per lone queen.
const BOT_COUNT: (u32, u32) = (2, 3);
/// Each seeded bot arrives after a randomized delay in this range (seconds) so they read as players
/// who "wandered in," not a scripted ambush.
const STAGGER_SECS: (u64, u64) = (30, 360);
/// Attempts to find a valid (in-bounds, on-land, territory-clear) ring point before giving up.
const RING_TRIES: u32 = 16;
/// Baseline live-ant target a bot is topped up toward (jittered per bot, + ~1/level).
const ANT_TARGET_BASE: usize = 6;
const ANT_TARGET_CAP: usize = 24;
/// Max ants added per bot per maintenance pass (drip-feed so they don't all refill in lockstep).
const ANT_REFILL_STEP: usize = 3;
/// Held tiles → XP, so a bot's level tracks its territory on the same curve humans climb.
const BOT_XP_PER_TILE: f64 = 1.0;
/// Bots cap out mid-level — believable, but the top ranks stay reachable by real players.
const BOT_LEVEL_CAP: u16 = 15;
/// `maintain` runs every this-many ticks (~3 s at 50 Hz). Off the hot per-tick path (sim_loop).
pub const MAINT_INTERVAL_TICKS: u64 = 150;

/// A bot scheduled to arrive at `due_tick` (staggered seeding). RAM-only (not persisted).
#[derive(Debug, Clone)]
pub struct PendingBot {
    pub x: i32,
    pub y: i32,
    pub level: u16,
    pub due_tick: u64,
}

// ---- Name pool --------------------------------------------------------------

const BOT_NAMES_JSON: &str = include_str!("../data/bot_names.json");

/// The 1,000-name pool, parsed once. Each obeys the human handle rules (3–20 chars, `[A-Za-z0-9_-]`).
/// Regenerate with `python scripts/build_bot_names.py` (rebuild to embed).
fn names() -> &'static Vec<String> {
    static NAMES: OnceLock<Vec<String>> = OnceLock::new();
    NAMES.get_or_init(|| {
        let mut v: Vec<String> = serde_json::from_str(BOT_NAMES_JSON).unwrap_or_default();
        // Defensive: drop anything that wouldn't pass the human handle rules so a bad edit to the
        // data file can never produce an out-of-spec username on the wire.
        v.retain(|n| {
            let len_ok = (3..=20).contains(&n.chars().count());
            let charset_ok = n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            len_ok && charset_ok
        });
        v
    })
}

/// Pick a bot name not currently in use by a live player (case-insensitive). Falls back to a numeric
/// suffix if the pool is saturated, and ultimately to `bot_{id}`-style only if the pool is empty.
fn pick_bot_name(world: &World, rng: &mut impl Rng) -> String {
    let pool = names();
    if pool.is_empty() {
        return format!("player_{}", world.next_player_id);
    }
    let in_use = |name: &str| {
        let lc = name.to_ascii_lowercase();
        world.players.values().any(|p| p.username.to_ascii_lowercase() == lc)
    };
    for _ in 0..24 {
        let cand = &pool[rng.gen_range(0..pool.len())];
        if !in_use(cand) {
            return cand.clone();
        }
    }
    // Saturated: append a 2-digit suffix (keeps ≤ 20 chars for our names; trim base if needed).
    let base = &pool[rng.gen_range(0..pool.len())];
    for _ in 0..40 {
        let n: u32 = rng.gen_range(10..100);
        let trimmed: String = base.chars().take(17).collect(); // leave room for "_NN"
        let cand = format!("{trimmed}_{n}");
        if !in_use(&cand) {
            return cand;
        }
    }
    format!("player_{}", world.next_player_id)
}

/// Display name for the wire: appends [`BOT_MARKER`] for bots while it is non-empty. Humans (and any
/// future non-npc) are returned verbatim. Used at every site a username reaches a client.
pub fn display_name(p: &Player) -> String {
    if p.npc && !BOT_MARKER.is_empty() {
        format!("{}{}", p.username, BOT_MARKER)
    } else {
        p.username.clone()
    }
}

// ---- Distance helpers -------------------------------------------------------

/// Real-world miles → world tiles, using the live Mercator scale. Raw tiles (no latitude correction)
/// to stay consistent with the existing bubble / placement-clearance math, which is also raw-tile.
fn miles_to_tiles(mi: f64) -> f64 {
    mi * 1609.344 / cfg().tile_meters.max(0.0001)
}

/// Count live queens (human OR bot) whose centre lies within `r_tiles` of (`cx`,`cy`), excluding
/// `exclude`. O(queens) — cheap, and only called on the rare human `place-queen` event.
fn count_live_queens_within(world: &World, cx: i32, cy: i32, r_tiles: f64, exclude: u32) -> usize {
    let r2 = (r_tiles * r_tiles) as i64;
    world.queens.iter()
        .filter(|(&id, q)| id != exclude && !q.dead)
        .filter(|(_, q)| {
            let dx = (q.x + q.size as i32 / 2 - cx) as i64;
            let dy = (q.y + q.size as i32 / 2 - cy) as i64;
            dx * dx + dy * dy <= r2
        })
        .count()
}

/// Pick a valid spawn point on the 5–7-mile ring around (`cx`,`cy`): in-bounds, on real land (not
/// ocean), and clear of all existing territory + queen bubbles. `None` if none found in [`RING_TRIES`].
fn pick_ring_point(world: &World, cx: i32, cy: i32, rng: &mut impl Rng) -> Option<(i32, i32)> {
    let c = cfg();
    let ww = world.world_w as i32;
    let wh = world.world_h as i32;
    let clear_r = c.place_clear_r;
    let r_min = miles_to_tiles(RING_MIN_MI);
    let r_max = miles_to_tiles(RING_MAX_MI);
    drop(c);
    for _ in 0..RING_TRIES {
        let ang = rng.gen_range(0.0..std::f64::consts::TAU);
        let r = rng.gen_range(r_min..=r_max);
        let x = (cx as f64 + r * ang.cos()).round() as i32;
        let y = (cy as f64 + r * ang.sin()).round() as i32;
        // Same bounds the place-queen handler enforces (leaves room for the queen footprint).
        if x < 2 || y < 2 || x >= ww - 8 || y >= wh - 8 {
            continue;
        }
        // u32::MAX = "belongs to no one", so ANY painted tile / ANY queen bubble disqualifies the
        // spot → bots only land on genuinely clear ground.
        if world.range_has_foreign_tile(x + 1, y + 1, clear_r, u32::MAX) {
            continue;
        }
        if world.too_close_to_queen(x + 1, y + 1, u32::MAX) {
            continue;
        }
        // Keep bots out of the ocean (oceans are never painted; a queen in open water looks fake).
        if crate::regions::country_and_continent(x, y).0 == "Open Water" {
            continue;
        }
        return Some((x, y));
    }
    None
}

// ---- Spawning ---------------------------------------------------------------

/// Shared bot/NPC insertion: allocate an id, build a level-`level` queen (size/HP/bubble via
/// `Queen::set_level` so the queen-map can't desync), paint the body, and seed 4 cardinal ants — the
/// same shape the old `spawn_npc` produced. `username = None` → legacy `NPC_{id}` (admin path);
/// `announce = true` fires the global `[ADMIN] NPC SPAWNED` event (organic bots pass `false`).
/// Returns the new player id.
pub fn spawn_npc_core(
    world: &mut World,
    cx: i32,
    cy: i32,
    level: u16,
    username: Option<String>,
    announce: bool,
) -> u32 {
    let id = world.next_player_id;
    world.next_player_id += 1;

    let mut rng = rand::thread_rng();
    let hue_idx = rng.gen_range(0..ENEMY_HUES.len());
    let hue = ENEMY_HUES[hue_idx].to_string();

    let c = cfg();
    let lifespan = c.lifespan;
    let lvl = level.clamp(1, c.xp_level_cap);

    let mut q = Queen {
        x: cx, y: cy, size: 2,
        hp: 0, max_hp: 0, level: 1, xp: 0.0, kills: 0,
        bubble_r: 0.0, last_attacker: None, dead: false,
        tiles_ever_held: 0, cached_tiles: 0, npc: true,
        shield: 0, shield_expiry: None,
        region: crate::regions::region_for(cx, cy),
    };
    q.set_level(lvl, &c);     // sets size, max_hp, bubble_r for the level
    q.hp = q.max_hp;
    q.xp = total_xp_for_level(lvl, &c); // xp consistent with the level it spawned at
    let size = q.size;
    drop(c);

    world.queens.insert(id, q);
    world.players.insert(id, Player {
        id,
        username: username.unwrap_or_else(|| format!("NPC_{id}")),
        color: hue,
        hue_idx: hue_idx as i32,
        npc: true,
        // A placed queen has a placement time → bots accrue the leaderboard's time-alive component
        // like a real player, so they age into the standings naturally.
        queen_placed_at: Some(current_ms()),
        ..Default::default()
    });
    world.queen_map_dirty = true;

    let ww = world.world_w as i32;
    let wh = world.world_h as i32;
    world.paint_queen_body(cx, cy, size, id);

    let spread = [
        (0i32, -(size as i32 + 1), 0i8, -1i8),
        (size as i32 + 1, 0, 1, 0),
        (0, size as i32 + 1, 0, 1),
        (-(size as i32 + 1), 0, -1, 0),
    ];
    for (k, (ox, oy, adx, ady)) in spread.iter().enumerate() {
        let ax = (cx + ox).clamp(0, ww - 1);
        let ay = (cy + oy).clamp(0, wh - 1);
        world.ants.push(Ant::new(rng.gen(), id, ax, ay, *adx, *ady, lifespan));
        world.ants.last_mut().unwrap().age = k as u32;
    }
    world.dirty_tick = world.tick;

    if announce {
        world.broadcast(&serde_json::json!({
            "t": "event", "msg": format!("[ADMIN] NPC SPAWNED (id {id})")
        }).to_string());
    }
    id
}

/// Hook from the `place-queen` success path: if the just-placed queen is isolated, queue 2–3 bots to
/// arrive (staggered) on a 5–7-mile ring around it. No-op if bots are disabled, the queen is gone, or
/// any live queen already sits within [`LONE_RADIUS_MI`] (so we never stack bots on a populated area
/// or let a relocating/re-dying player summon endless swarms).
pub fn maybe_spawn_for_lone_queen(world: &mut World, pid: u32) {
    if !BOTS_ENABLED {
        return;
    }
    let Some(q) = world.queens.get(&pid).filter(|q| !q.dead) else { return };
    let (qcx, qcy) = (q.x + q.size as i32 / 2, q.y + q.size as i32 / 2);

    let lone_r = miles_to_tiles(LONE_RADIUS_MI);
    if count_live_queens_within(world, qcx, qcy, lone_r, pid) > 0 {
        return; // not lone — there's already action nearby
    }

    let mut rng = rand::thread_rng();
    let n = rng.gen_range(BOT_COUNT.0..=BOT_COUNT.1);
    let tick_rate = cfg().tick_rate.max(1) as u64;
    for _ in 0..n {
        if let Some((x, y)) = pick_ring_point(world, qcx, qcy, &mut rng) {
            let delay = rng.gen_range(STAGGER_SECS.0..=STAGGER_SECS.1) * tick_rate;
            world.pending_bots.push(PendingBot {
                x, y,
                level: rng.gen_range(1..=2),
                due_tick: world.tick + delay,
            });
        }
    }
}

// ---- Maintenance (off the hot tick path) -----------------------------------

/// Called from `sim_loop` every [`MAINT_INTERVAL_TICKS`] (under the write lock, after `tick_world`,
/// so `tick_world` stays a pure tick). Two jobs: (1) arrive any staggered bots whose time has come;
/// (2) keep every live bot alive & growing — top its ant population up toward a per-bot target and
/// let its level track its held territory (capped at [`BOT_LEVEL_CAP`]). Cheap: O(bots) + O(pending),
/// reusing the per-tick `ant_counts` map (no extra ant sweep).
pub fn maintain(world: &mut World) {
    if !BOTS_ENABLED {
        return;
    }

    // (1) Arrive due bots. Partition without disturbing not-yet-due entries.
    if !world.pending_bots.is_empty() {
        let now = world.tick;
        let pending = std::mem::take(&mut world.pending_bots);
        let (ready, later): (Vec<PendingBot>, Vec<PendingBot>) =
            pending.into_iter().partition(|b| b.due_tick <= now);
        world.pending_bots = later;
        let mut rng = rand::thread_rng();
        for b in ready {
            // Re-validate: the ring point may have been claimed/occupied during the stagger window.
            if world.too_close_to_queen(b.x + 1, b.y + 1, u32::MAX)
                || world.range_has_foreign_tile(b.x + 1, b.y + 1, cfg().place_clear_r, u32::MAX)
            {
                continue;
            }
            let name = pick_bot_name(world, &mut rng);
            spawn_npc_core(world, b.x, b.y, b.level, Some(name), false);
        }
    }

    // (2) Keep live bots alive & growing.
    let c = cfg();
    let lifespan = c.lifespan;
    let ww = world.world_w as i32;
    let wh = world.world_h as i32;
    let bot_ids: Vec<u32> = world.queens.iter()
        .filter(|(_, q)| q.npc && !q.dead)
        .map(|(&id, _)| id)
        .collect();
    let mut rng = rand::thread_rng();
    let mut map_dirty = false;

    for id in bot_ids {
        let Some(q) = world.queens.get(&id) else { continue };
        let (qcx, qcy, size, bubble_r, level, tiles) =
            (q.x + q.size as i32 / 2, q.y + q.size as i32 / 2, q.size, q.bubble_r, q.level, q.cached_tiles);

        // --- ant top-up toward a per-bot target (id-derived jitter = a little "personality") ---
        let target = (ANT_TARGET_BASE + level as usize + (id as usize % 5)).min(ANT_TARGET_CAP);
        let have = world.ant_counts.get(&id).copied().unwrap_or(0) as usize;
        if have < target {
            let add = (target - have).min(ANT_REFILL_STEP);
            for _ in 0..add {
                // Place inside the bubble, beyond the queen body, heading a random cardinal.
                let r = rng.gen_range((size as f64 + 1.0)..=bubble_r.max(size as f64 + 2.0));
                let ang = rng.gen_range(0.0..std::f64::consts::TAU);
                let ax = (qcx as f64 + r * ang.cos()).round() as i32;
                let ay = (qcy as f64 + r * ang.sin()).round() as i32;
                let ax = ax.clamp(0, ww - 1);
                let ay = ay.clamp(0, wh - 1);
                const DIRS: [(i8, i8); 4] = [(0, -1), (1, 0), (0, 1), (-1, 0)];
                let (dx, dy) = DIRS[rng.gen_range(0..4)];
                world.ants.push(Ant::new(rng.gen(), id, ax, ay, dx, dy, lifespan));
            }
            world.dirty_tick = world.tick;
        }

        // --- organic growth: level tracks held territory on the same curve humans climb ---
        let want_xp = tiles as f64 * BOT_XP_PER_TILE;
        if let Some(q) = world.queens.get_mut(&id) {
            if want_xp > q.xp {
                q.xp = want_xp;
                let new_lvl = level_for_xp(q.xp, &c).min(BOT_LEVEL_CAP);
                if new_lvl > q.level {
                    let old_max = q.max_hp;
                    q.set_level(new_lvl, &c);
                    // Grant the added HP headroom (mirrors the human level-up in `flush_xp`).
                    q.hp = (q.hp + (q.max_hp - old_max).max(0)).min(q.max_hp);
                    map_dirty = true; // size may have grown
                }
            }
        }
    }
    drop(c);
    if map_dirty {
        world.queen_map_dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_load_and_are_valid() {
        let pool = names();
        assert!(pool.len() >= 900, "expected ~1000 bot names, got {}", pool.len());
        for n in pool {
            let len = n.chars().count();
            assert!((3..=20).contains(&len), "bad length: {n}");
            assert!(n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                    "bad charset: {n}");
        }
    }

    #[test]
    fn miles_convert_to_expected_tiles() {
        // At the default 26.72 m/tile, 10 mi ≈ 602 tiles, 5 mi ≈ 301, 7 mi ≈ 422.
        let t10 = miles_to_tiles(10.0);
        assert!((600.0..605.0).contains(&t10), "10mi -> {t10} tiles");
        assert!((299.0..303.0).contains(&miles_to_tiles(5.0)));
        assert!((419.0..424.0).contains(&miles_to_tiles(7.0)));
    }

    #[test]
    fn display_name_marks_only_bots() {
        let mut bot = Player { npc: true, ..Default::default() };
        bot.username = "based_dept".into();
        let mut human = Player { npc: false, ..Default::default() };
        human.username = "ALICE".into();
        if BOT_MARKER.is_empty() {
            assert_eq!(display_name(&bot), "based_dept");
        } else {
            assert_eq!(display_name(&bot), format!("based_dept{BOT_MARKER}"));
        }
        assert_eq!(display_name(&human), "ALICE");
    }

    #[test]
    fn lone_detection_counts_other_queens() {
        let mut w = World::new();
        let cx = w.world_w as i32 / 2;
        let cy = w.world_h as i32 / 2;
        // No queens yet → lone.
        assert_eq!(count_live_queens_within(&w, cx, cy, 600.0, 1), 0);
        // A queen 100 tiles away counts; one 5000 tiles away does not.
        let near = spawn_npc_core(&mut w, cx + 100, cy, 1, Some("near_bot".into()), false);
        spawn_npc_core(&mut w, cx + 5000, cy, 1, Some("far_bot".into()), false);
        assert_eq!(count_live_queens_within(&w, cx, cy, 600.0, u32::MAX), 1);
        // Excluding the near queen drops the count back to 0 within 600 tiles.
        assert_eq!(count_live_queens_within(&w, cx, cy, 600.0, near), 0);
    }

    #[test]
    fn pick_bot_name_avoids_live_collisions() {
        let mut w = World::new();
        let mut rng = rand::thread_rng();
        let name = pick_bot_name(&w, &mut rng);
        // Register it as in-use, then ensure the picker won't hand it back.
        w.players.insert(1, Player { username: name.clone(), ..Default::default() });
        for _ in 0..50 {
            assert_ne!(pick_bot_name(&w, &mut rng).to_ascii_lowercase(), name.to_ascii_lowercase());
        }
    }

    fn mk_queen(x: i32, y: i32) -> Queen {
        Queen {
            x, y, size: 2, hp: 100, max_hp: 100, level: 1, xp: 0.0, kills: 0, bubble_r: 30.0,
            last_attacker: None, dead: false, tiles_ever_held: 0, cached_tiles: 0, npc: false,
            shield: 0, shield_expiry: None, region: String::new(),
        }
    }

    /// Locate a deep-land tile whose 6-mile cardinal ring is also land, so the 5–7-mile spawn ring
    /// lands on terrain rather than ocean. Anchored at a Sahara-ish point (lat≈20, lon≈10 under the
    /// default capitol projection); panics with a clear message if the projection ever drifts.
    fn land_center(_w: &World) -> (i32, i32) {
        let candidates = [(791_584, 290_138), (800_000, 300_000), (820_000, 280_000), (760_000, 250_000)];
        let r = miles_to_tiles(6.0).round() as i32;
        for &(x, y) in &candidates {
            let land = |px: i32, py: i32| crate::regions::country_and_continent(px, py).0 != "Open Water";
            if land(x, y) && [(r, 0), (-r, 0), (0, r), (0, -r)].iter().all(|(dx, dy)| land(x + dx, y + dy)) {
                return (x, y);
            }
        }
        panic!("no land candidate found — the capitol projection may have changed");
    }

    #[test]
    fn lone_land_queen_seeds_bots_that_arrive_and_hide_the_npc_tell() {
        let mut w = World::new();
        let (cx, cy) = land_center(&w);

        // Simulate a human placing a queen on empty land (mirrors the place-queen success path).
        let human = 100u32;
        w.players.insert(human, Player { id: human, username: "ALICE".into(), npc: false, ..Default::default() });
        w.queens.insert(human, mk_queen(cx, cy));

        maybe_spawn_for_lone_queen(&mut w, human);
        let queued = w.pending_bots.len();
        assert!((1..=3).contains(&queued), "a lone land queen should queue 1–3 bots, got {queued}");

        // Fast-forward past the stagger delays and let them arrive.
        w.tick = w.pending_bots.iter().map(|b| b.due_tick).max().unwrap() + 1;
        maintain(&mut w);
        assert!(w.pending_bots.is_empty(), "all queued bots should have arrived");

        let bot_ids: Vec<u32> = w.players.iter().filter(|(_, p)| p.npc).map(|(&id, _)| id).collect();
        assert_eq!(bot_ids.len(), queued, "every queued bot should have spawned a live queen");
        for &id in &bot_ids {
            let p = &w.players[&id];
            assert!(!p.username.starts_with("NPC_"), "bot should use a realistic name, got {}", p.username);
            assert!(w.queens.get(&id).map(|q| !q.dead).unwrap_or(false), "bot {id} should have a live queen");
            assert!(w.ants.iter().any(|a| a.owner == id), "bot {id} should be seeded with ants");
        }

        // On the wire: the leaderboard must NOT report the npc tell (client draws a red NPC badge on
        // it), and the name carries the temporary marker so QA can still spot bots pre-launch.
        let lb: serde_json::Value = serde_json::from_str(&crate::network::build_leaderboard(&w)).unwrap();
        let entries = lb["entries"].as_array().unwrap();
        let entry = entries.iter().find(|e| e["id"].as_u64() == Some(bot_ids[0] as u64))
            .expect("bot should appear on the leaderboard");
        assert_eq!(entry["npc"], serde_json::json!(false), "bots must not leak the npc tell");
        if !BOT_MARKER.is_empty() {
            assert!(entry["name"].as_str().unwrap().ends_with(BOT_MARKER),
                    "bot name should carry the temporary marker on the wire");
        }
    }

    #[test]
    fn queen_with_a_neighbor_seeds_no_bots() {
        let mut w = World::new();
        let (cx, cy) = (700_000, 300_000);
        // An existing neighbor well inside the 10-mile (~600-tile) lone radius → not lone.
        w.players.insert(100, Player { id: 100, ..Default::default() });
        w.queens.insert(100, mk_queen(cx, cy));
        w.players.insert(101, Player { id: 101, ..Default::default() });
        w.queens.insert(101, mk_queen(cx + 200, cy));

        maybe_spawn_for_lone_queen(&mut w, 101);
        assert!(w.pending_bots.is_empty(), "a queen with a neighbor within 10 mi must seed no bots");
    }
}
