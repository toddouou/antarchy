use rustc_hash::FxHashMap;
use rand::Rng;
use rayon::prelude::*;
use serde_json::json;

use crate::config::{
    cfg, level_for_xp, current_ms, ENEMY_HUES,
    BRUTE_DMG_MULT, CREDIT_CAP, DEFENDER_RANGE, TILE_MILESTONES,
};
use crate::world::{Ant, MetroHolder, Player, Queen, QueenHit, World, XpGrant};

// ---- Discovery + metro-holder throttles ----
const HOLDER_INTERVAL:    u64   = 500;  // ~10 s @ 50 Hz — king-of-the-hill recompute cadence
const HOLDER_STRIDE:      u32   = 4;    // sample every 4th cell in each axis (scaling care)
const DISCOVERY_INTERVAL: u64   = 50;   // ~1 s — visited-region sampling cadence
const DISCOVERY_SAMPLE_N: usize = 64;   // ants sampled per pass (round-robin, army-size-independent)
const COMPACT_INTERVAL:   u64   = 1500; // ~30 s @ 50 Hz — Dense→Uniform tile-RAM compaction sweep

// ---- Direction helpers ----------------------------------------------------

#[inline]
pub fn turn_ccw(dx: i8, dy: i8) -> (i8, i8) { (dy, -dx) }
#[inline]
pub fn turn_cw(dx: i8, dy: i8) -> (i8, i8) { (-dy, dx) }

// ---- XP helpers -----------------------------------------------------------

pub fn award_xp(world: &mut World, player_id: u32, amount: f64, reason: &'static str, x: i32, y: i32) {
    world.xp_queue.push(XpGrant { player_id, amount, reason, x, y });
}

pub fn flush_xp(world: &mut World) {
    let grants: Vec<XpGrant> = std::mem::take(&mut world.xp_queue);
    let c = cfg().clone();
    for g in grants {
        let is_npc = world.players.get(&g.player_id).map(|p| p.npc).unwrap_or(true);
        if is_npc { continue; }
        let queen_alive = world.queens.get(&g.player_id).map(|q| !q.dead).unwrap_or(false);
        if !queen_alive { continue; }

        if g.amount >= 3.0 {
            let tx = world.players.get(&g.player_id).and_then(|p| p.tx.clone());
            if let Some(tx) = tx {
                let _ = tx.send(json!({
                    "t":"xp-gain","amount":g.amount as i64,"x":g.x,"y":g.y,"reason":g.reason
                }).to_string());
            }
        }

        let level_up_result = {
            let Some(q) = world.queens.get_mut(&g.player_id) else { continue };
            q.xp = (q.xp + g.amount).max(0.0);
            let old_lvl = q.level;
            let new_lvl = level_for_xp(q.xp, &c);
            if new_lvl > old_lvl {
                let old_max = q.max_hp;
                q.set_level(new_lvl, &c);
                // Grant the new HP headroom as healing: a full queen stays full, a damaged one
                // keeps its damage but gains the level's added pool (a flat +hp_base would be
                // negligible against the exponential curve).
                q.hp = (q.hp + (q.max_hp - old_max).max(0)).min(q.max_hp);
                Some((old_lvl, new_lvl))
            } else {
                None
            }
        };

        if let Some((old_lvl, new_lvl)) = level_up_result {
            let ants_gained = (new_lvl - old_lvl) as i32 * c.levelup_ant_grant;
            if let Some(p) = world.players.get_mut(&g.player_id) {
                p.ants_avail += ants_gained;
            }
            world.queen_map_dirty = true;
            let tx = world.players.get(&g.player_id).and_then(|p| p.tx.clone());
            if let Some(tx) = tx {
                let _ = tx.send(json!({
                    "t":"event","msg":format!("▲ LEVEL UP → LV{new_lvl} · +{ants_gained} ANTS")
                }).to_string());
                let _ = tx.send(json!({"t":"level-up","level":new_lvl}).to_string());
            }
        }
    }
}

// ---- Kill queen -----------------------------------------------------------

pub fn kill_queen(world: &mut World, loser_id: u32, killer_id: Option<u32>, reason: &str) {
    let (qx, qy, victim_level, peak_tiles, victim_region, victim_kills) = {
        let Some(q) = world.queens.get_mut(&loser_id) else { return };
        if q.dead { return; }
        q.dead = true;
        q.cached_tiles = 0;
        (q.x, q.y, q.level, q.tiles_ever_held, q.region.clone(), q.kills)
    };
    world.queen_map_dirty = true;

    // Forfeit all territory — the fallen queen's tiles turn blank, so a respawn (or any
    // future queen for this player) starts from zero tiles.
    world.tiles.clear_owner(loser_id);
    world.dirty_tick = world.tick;

    // Prestige: increment on each queen death. Real players also lose their standing
    // army — workers reset to the starter count and the daily timer restarts, so no
    // refill arrives until tomorrow. (Existing live ants are removed below.)
    let daily = cfg().daily_ants;
    let now = current_ms();
    if let Some(p) = world.players.get_mut(&loser_id) {
        p.prestige += 1;
        // Fold the fallen queen's life into account-level lifetime stats (USER-vs-QUEEN compare).
        p.lifetime_kills += victim_kills;
        p.lifetime_peak_tiles = p.lifetime_peak_tiles.max(peak_tiles);
        if !p.npc {
            p.ants_avail  = daily;
            p.next_refill = now + 24 * 3600 * 1000;
        }
    }

    // Capture the fallen queen's life stats for the player's death-screen summary.
    let (secs_alive, new_prestige, credits) = match world.players.get(&loser_id) {
        Some(p) => (
            p.queen_placed_at.map(|t| now.saturating_sub(t) / 1000).unwrap_or(0),
            p.prestige,
            p.credits,
        ),
        None => (0, 0, 0),
    };

    let near_msg = json!({"t":"queen-killed","x":qx,"y":qy}).to_string();
    world.broadcast_near(qx, qy, &near_msg);

    let loser_name = world.players.get(&loser_id)
        .map(|p| p.username.clone())
        .unwrap_or_else(|| loser_id.to_string());
    let victim_color = world.players.get(&loser_id)
        .map(|p| p.color.clone()).unwrap_or_else(|| "#888".into());
    let (killer_name, killer_color) = match killer_id {
        Some(kid) => {
            let kp = world.players.get(&kid);
            (kp.map(|p| p.username.clone()), kp.map(|p| p.color.clone()))
        }
        None => (None, None),
    };
    // Structured kill → drives the CS1.6-style killfeed (top-right). Replaces the old
    // global "has fallen" toast so deaths aren't double-announced.
    world.broadcast(&json!({
        "t":"kill",
        "killer":      killer_name.clone(),
        "killerColor": killer_color,
        "victim":      loser_name,
        "victimColor": victim_color,
        "victimLevel": victim_level,
        "cause":       reason,
        "x": qx, "y": qy,
    }).to_string());

    if let Some(kid) = killer_id {
        if let Some(kq) = world.queens.get_mut(&kid) { kq.kills += 1; }
        if let Some(kp) = world.players.get_mut(&kid) {
            kp.credits = (kp.credits + 1).min(CREDIT_CAP);
        }
        let kill_xp = cfg().xp_kill;
        award_xp(world, kid, kill_xp, "kill", qx, qy);
        flush_xp(world);
        world.send_to(kid, json!({"t":"event","msg":format!("KILL! +{} XP", kill_xp as i64)}).to_string());
    }

    world.ants.retain(|a| a.owner != loser_id);
    world.send_to(loser_id, json!({
        "t":"queen-dead",
        "peakTiles": peak_tiles,
        "kills":     victim_kills,
        "level":     victim_level,
        "secsAlive": secs_alive,
        "region":    victim_region,
        "prestige":  new_prestige,
        "credits":   credits,
        "killer":    killer_name,
        "cause":     reason,
    }).to_string());
}

// ---- Wipe world -----------------------------------------------------------

pub fn wipe_world(world: &mut World) {
    world.tiles.clear();
    world.ants.clear();
    world.queens.clear();
    world.queen_map.clear();
    world.queen_map_dirty = false;
    world.tick = 0;
    let c = cfg();
    let daily = c.daily_ants;
    drop(c);
    let now = current_ms();
    let npc_ids: Vec<u32> = world.players.iter()
        .filter(|(_, p)| p.npc)
        .map(|(&id, _)| id)
        .collect();
    for id in npc_ids { world.players.remove(&id); }
    for (_, p) in world.players.iter_mut() {
        p.ants_avail    = daily;
        p.next_refill   = now + 24 * 3600 * 1000;
        p.queen_placed_at = None;
        if let Some(tx) = &p.tx {
            let _ = tx.send(r#"{"t":"world-wiped"}"#.to_string());
        }
    }
    world.broadcast(r#"{"t":"event","msg":"[ADMIN] WORLD WIPED · ALL QUEENS REMOVED"}"#);
}

// ---- Spawn NPC ------------------------------------------------------------

pub fn spawn_npc(world: &mut World, near_player_id: u32, spawn_x: Option<i32>, spawn_y: Option<i32>) {
    let id = world.next_player_id;
    world.next_player_id += 1;

    let mut rng = rand::thread_rng();
    let hue_idx = rng.gen_range(0..ENEMY_HUES.len());
    let hue = ENEMY_HUES[hue_idx].to_string();

    let c = cfg();
    let (cx, cy) = if let (Some(sx), Some(sy)) = (spawn_x, spawn_y) {
        (sx, sy)
    } else {
        let pq = world.queens.get(&near_player_id);
        let bx = pq.map(|q| q.x).unwrap_or(c.spawn_x as i32);
        let by = pq.map(|q| q.y).unwrap_or(c.spawn_y as i32);
        (bx + rng.gen_range(-30..30), by + rng.gen_range(-30..30))
    };
    let lifespan = c.lifespan;
    let bubble_r = c.bubble_r;
    let npc_hp   = crate::config::max_hp_for_level(1, &c);   // level-1 queen HP
    let ww = world.world_w as i32;
    let wh = world.world_h as i32;
    drop(c);

    let npc_size: u8 = 2;
    world.queens.insert(id, Queen {
        x: cx, y: cy, size: npc_size,
        hp: npc_hp, max_hp: npc_hp, level: 1, xp: 0.0, kills: 0,
        bubble_r, last_attacker: None, dead: false,
        tiles_ever_held: 0, cached_tiles: 0, npc: true,
        shield: 0, shield_expiry: None,
        region: crate::regions::region_for(cx, cy),
    });
    world.players.insert(id, Player {
        id, username: format!("NPC_{id}"), color: hue,
        hue_idx: hue_idx as i32,
        ants_avail: 0, next_refill: 0, queen_placed_at: None,
        npc: true, view: None, tx: None, view_tx: None, conn_gen: 0,
        prestige: 0, credits: 0,
        defenders: Vec::new(),
        visited_countries: Default::default(), visited_continents: Default::default(),
        lifetime_kills: 0, lifetime_peak_tiles: 0, queens_fielded: 0, away: None,
    });
    world.queen_map_dirty = true;

    world.paint_queen_body(cx, cy, npc_size, id);

    let spread = [
        (0i32, -(npc_size as i32 + 1), 0i8, -1i8),
        (npc_size as i32 + 1, 0, 1, 0),
        (0, npc_size as i32 + 1, 0, 1),
        (-(npc_size as i32 + 1), 0, -1, 0),
    ];
    for (k, (ox, oy, adx, ady)) in spread.iter().enumerate() {
        let ax = (cx + ox).clamp(0, ww - 1);
        let ay = (cy + oy).clamp(0, wh - 1);
        world.ants.push(Ant::new(rng.gen(), id, ax, ay, *adx, *ady, lifespan));
        world.ants.last_mut().unwrap().age = k as u32;
    }

    world.broadcast(&json!({"t":"event","msg":format!("[ADMIN] NPC SPAWNED (id {id})")}).to_string());
}

// ---- Main tick ------------------------------------------------------------

pub fn tick_world(world: &mut World) {
    world.tick += 1;
    // Mark the world dirty whenever there are active ants (tiles will change this tick)
    if !world.ants.is_empty() { world.dirty_tick = world.tick; }
    let c = cfg().clone();
    let ww     = world.world_w as i32;
    let wh     = world.world_h as i32;
    let ww_u64 = world.world_w as u64;

    world.get_queen_map();

    // --- Occasional dedup (every 64 ticks): removes stacked same-owner same-direction ants ---
    // Parallel sort (rayon) keeps this O(n log n) pass off the critical path at 100k ants;
    // the dedup itself stays serial (it only walks the now-sorted vec once).
    if world.tick % 64 == 0 && !world.ants.is_empty() {
        world.ants.par_sort_unstable_by_key(|a| (a.owner, a.x, a.y, a.dx as i32, a.dy as i32));
        world.ants.dedup_by_key(|a| (a.owner, a.x, a.y, a.dx, a.dy));
    }

    // --- Spatial sort every 50 ticks: group ants by 256×256 chunk for cache locality ---
    if world.tick % 50 == 0 && !world.ants.is_empty() {
        let chunk_w = world.world_w / 256 + 1;
        world.ants.par_sort_unstable_by_key(|a| {
            let cx = a.x as u32 / 256;
            let cy = a.y as u32 / 256;
            cy * chunk_w + cx
        });
    }

    // =========================================================================
    // Phase 1: Plan moves — Rayon parallel, no Mutex, fold/reduce for accumulation
    // =========================================================================
    let is_even = world.tick % 2 == 0;
    let (hits, xp_grants) = {
        let tiles     = &world.tiles;
        let queen_map = &world.queen_map;

        world.ants.par_iter_mut()
            .fold(
                || (Vec::<QueenHit>::new(), Vec::<XpGrant>::new()),
                |(mut hits, mut xp), ant| {
                    // Brute ants only move on even ticks
                    if ant.kind == 1 && !is_even {
                        ant._nx = ant.x; ant._ny = ant.y;
                        ant._ndx = ant.dx; ant._ndy = ant.dy;
                        return (hits, xp);
                    }

                    let (mut ndx, mut ndy) = if ant.kind == 1 {
                        // Brute Langton variant over the 2×2 footprint at the current position:
                        //   all friendly → 90° CW · all white → 90° CCW · mixed → straight.
                        // The footprint is always (re)painted friendly in Phase 3, so every case
                        // leaves the block owned by the brute.
                        let mut all_friendly = true;
                        let mut all_white    = true;
                        for bdy in 0i32..2 {
                            for bdx in 0i32..2 {
                                let (bx, by) = (ant.x + bdx, ant.y + bdy);
                                if bx >= ww || by >= wh { continue; }   // skip off-edge cells
                                let t = tiles.get(bx as u32, by as u32);
                                if t != ant.owner { all_friendly = false; }
                                if t != 0         { all_white    = false; }
                            }
                        }
                        if all_friendly {
                            turn_cw(ant.dx, ant.dy)
                        } else if all_white {
                            turn_ccw(ant.dx, ant.dy)
                        } else {
                            (ant.dx, ant.dy)
                        }
                    } else {
                        let cur = tiles.get(ant.x as u32, ant.y as u32);
                        if cur == 0 {
                            turn_ccw(ant.dx, ant.dy)
                        } else if cur == ant.owner {
                            turn_cw(ant.dx, ant.dy)
                        } else {
                            (ant.dx, ant.dy)
                        }
                    };

                    // Brutes hop a whole 2×2 (step 2) so consecutive footprints never overlap —
                    // each step reads a fresh set of 4 cells. Stepping 1 would re-read 2 of its
                    // own just-painted cells → permanent "mixed → straight" highway loop.
                    let step = if ant.kind == 1 { 2 } else { 1 };
                    let mut nx = ant.x + ndx as i32 * step;
                    let mut ny = ant.y + ndy as i32 * step;

                    if nx < 0 || nx >= ww || ny < 0 || ny >= wh {
                        ndx = -ndx; ndy = -ndy;
                        nx = (ant.x + ndx as i32 * step).clamp(0, ww - 1);
                        ny = (ant.y + ndy as i32 * step).clamp(0, wh - 1);
                    }

                    if ant.kind == 1 {
                        // Brute: check 2×2 destination block; deduplicate per queen
                        let mut brute_hit = false;
                        let mut hit_queens: Vec<u32> = Vec::new();
                        for bdy in 0i32..2 {
                            for bdx in 0i32..2 {
                                let (bx, by) = (nx + bdx, ny + bdy);
                                if bx >= ww || by >= wh { continue; }  // never index past the world edge
                                let dest_key = by as u64 * ww_u64 + bx as u64;
                                if let Some(&queen_id) = queen_map.get(&dest_key) {
                                    if queen_id != ant.owner && !hit_queens.contains(&queen_id) {
                                        hit_queens.push(queen_id);
                                        hits.push(QueenHit { queen_id, attacker: ant.owner, is_own: false, dmg_mult: BRUTE_DMG_MULT });
                                        xp.push(XpGrant { player_id: ant.owner, amount: 0.5, reason: "hit", x: ant.x, y: ant.y });
                                    }
                                    brute_hit = true;
                                }
                            }
                        }
                        if brute_hit {
                            (ndx, ndy) = turn_cw(ndx, ndy);
                            nx = ant.x; ny = ant.y;
                        }
                    } else {
                        let dest_key = ny as u64 * ww_u64 + nx as u64;
                        if let Some(&queen_id) = queen_map.get(&dest_key) {
                            if queen_id != ant.owner {
                                hits.push(QueenHit { queen_id, attacker: ant.owner, is_own: false, dmg_mult: 1.0 });
                                xp.push(XpGrant { player_id: ant.owner, amount: 0.5, reason: "hit", x: ant.x, y: ant.y });
                            } else {
                                hits.push(QueenHit { queen_id, attacker: ant.owner, is_own: true, dmg_mult: 1.0 });
                            }
                            (ndx, ndy) = turn_cw(ndx, ndy);
                            nx = ant.x; ny = ant.y;
                        }
                    }

                    ant._ndx = ndx; ant._ndy = ndy;
                    ant._nx  = nx;  ant._ny  = ny;
                    (hits, xp)
                }
            )
            .reduce(
                || (Vec::new(), Vec::new()),
                |(mut ha, mut xa), (mut hb, mut xb)| {
                    ha.append(&mut hb); xa.append(&mut xb);
                    (ha, xa)
                }
            )
    };

    // Apply hits: batch damage per queen + heal touches
    let heal_xp = c.xp_heal;
    let mut damage_map: FxHashMap<u32, f64> = FxHashMap::default();
    let mut last_attacker_map: FxHashMap<u32, u32> = FxHashMap::default();

    for hit in &hits {
        if hit.is_own {
            if let Some(q) = world.queens.get_mut(&hit.queen_id) {
                if !q.dead && q.hp < q.max_hp {
                    q.hp = (q.hp + 1).min(q.max_hp);
                }
            }
            world.xp_queue.push(XpGrant { player_id: hit.attacker, amount: heal_xp, reason: "heal", x: 0, y: 0 });
        } else {
            // An ant's bite equals its queen's level; brutes carry a 3× multiplier (dmg_mult).
            // `ant_damage` stays as a global admin scalar (default 1.0).
            let atk_lvl = world.queens.get(&hit.attacker).map(|q| q.level).unwrap_or(1).max(1) as f64;
            *damage_map.entry(hit.queen_id).or_insert(0.0) += c.ant_damage * atk_lvl * hit.dmg_mult as f64;
            last_attacker_map.insert(hit.queen_id, hit.attacker);
        }
    }

    // Apply batched damage and send one notification per struck queen
    for (&queen_id, &total_dmg) in &damage_map {
        let (qx, qy) = {
            let Some(q) = world.queens.get_mut(&queen_id) else { continue };
            if q.dead { continue; }
            // Absorb from shield before reducing HP
            let remaining = if q.shield > 0 {
                let absorbed = (total_dmg as i32).min(q.shield);
                q.shield -= absorbed;
                total_dmg - absorbed as f64
            } else {
                total_dmg
            };
            q.hp = (q.hp as f64 - remaining).max(0.0) as i32;
            q.last_attacker = last_attacker_map.get(&queen_id).copied();
            (q.x, q.y)
        };
        if let Some(attacker_id) = last_attacker_map.get(&queen_id) {
            let atx = world.players.get(attacker_id).and_then(|p| if !p.npc { p.tx.clone() } else { None });
            if let Some(tx) = atx {
                let _ = tx.send(json!({"t":"damage-dealt","tx":qx,"ty":qy}).to_string());
            }
        }
        let dtx = world.players.get(&queen_id).and_then(|p| if !p.npc { p.tx.clone() } else { None });
        if let Some(tx) = dtx {
            let _ = tx.send(json!({"t":"damage-taken","sx":qx,"sy":qy}).to_string());
        }
    }
    world.xp_queue.extend(xp_grants);

    // =========================================================================
    // Phase 2: Ant-ant collision — sorted Vec, no per-tick HashMap allocation
    // =========================================================================
    build_sorted_pairs(&mut world.scratch_pairs, &world.ants, ww_u64, true);

    // Process collision groups (sorted pairs, different field from ants — no borrow conflict)
    let pair_len = world.scratch_pairs.len();
    let mut gi = 0;
    while gi < pair_len {
        let k = world.scratch_pairs[gi].0;
        let gstart = gi;
        while gi < pair_len && world.scratch_pairs[gi].0 == k { gi += 1; }
        if gi - gstart < 2 { continue; }

        for j in gstart..gi {
            let idx = world.scratch_pairs[j].1 as usize;
            // Copy out ant data to release immutable borrow before mutable borrow below
            let (ax, ay, anx, any, andx, andy, akind) = {
                let a = &world.ants[idx];
                (a.x, a.y, a._nx, a._ny, a._ndx, a._ndy, a.kind)
            };
            if anx == ax && any == ay { continue; }
            // Keep brutes on their 2×2 lattice: the collision nudge moves 2 as well, not 1.
            let step = if akind == 1 { 2 } else { 1 };
            let (cdx, cdy) = turn_cw(andx, andy);
            let mut cx = ax + cdx as i32 * step;
            let mut cy = ay + cdy as i32 * step;
            if cx < 0 || cx >= ww || cy < 0 || cy >= wh { cx = ax; cy = ay; }
            if world.queen_map.contains_key(&(cy as u64 * ww_u64 + cx as u64)) { cx = ax; cy = ay; }
            let a = &mut world.ants[idx];
            a._ndx = cdx; a._ndy = cdy; a._nx = cx; a._ny = cy;
        }
    }

    // Rebuild sorted pairs with post-collision destinations for Phase 5 clash resolution
    build_sorted_pairs(&mut world.scratch_pairs, &world.ants, ww_u64, true);

    // =========================================================================
    // Phase 3: Paint + commit move
    // =========================================================================
    let highway_xp = c.xp_highway_tick;
    for ant in world.ants.iter_mut() {
        let cur_key = ant.y as u64 * ww_u64 + ant.x as u64;
        if !world.queen_map.contains_key(&cur_key) {
            if ant.kind == 1 {
                // Brute: paint 2×2 block on even ticks only; never erase own tiles
                if is_even {
                    for bdy in 0i32..2 {
                        for bdx in 0i32..2 {
                            let (bxi, byi) = (ant.x + bdx, ant.y + bdy);
                            if bxi >= ww || byi >= wh { continue; }  // never paint past the world edge
                            let (bx, by) = (bxi as u32, byi as u32);
                            if world.tiles.get(bx, by) != ant.owner {
                                world.tiles.set(bx, by, ant.owner);
                            }
                        }
                    }
                }
                ant.highway_ticks = 0;
            } else {
                let cur = world.tiles.get(ant.x as u32, ant.y as u32);
                if cur == 0 {
                    world.tiles.set(ant.x as u32, ant.y as u32, ant.owner);
                    ant.highway_ticks += 1;
                    if ant.highway_ticks >= 3 {
                        world.xp_queue.push(XpGrant { player_id: ant.owner, amount: highway_xp, reason: "highway", x: ant.x, y: ant.y });
                        ant.highway_ticks = 0;
                    }
                } else if cur == ant.owner {
                    world.tiles.set(ant.x as u32, ant.y as u32, 0);
                    ant.highway_ticks = 0;
                } else {
                    world.tiles.set(ant.x as u32, ant.y as u32, ant.owner);
                    ant.highway_ticks = 0;
                }
            }
        }
        ant.dx = ant._ndx; ant.dy = ant._ndy;
        ant.x  = ant._nx;  ant.y  = ant._ny;
        ant.age += 1;
    }

    // =========================================================================
    // Phase 4: Tile milestones (one-time per queen) + cached-tile update
    // =========================================================================
    // A milestone fires the first tick a queen's peak tile count (`tiles_ever_held`) reaches a
    // rounded threshold (10k, 25k, 50k, 100k, …). Each grants `xp_tile_award × index` XP — no
    // toast: the queued XP surfaces only as the floating "+N XP" on the queen + ping
    // (flush_xp → "xp-gain"). Because it keys off the high-water mark, each threshold pays out once.
    let tile_award = c.xp_tile_award;
    let queen_ids: Vec<u32> = world.queens.keys().copied().collect();
    for pid in queen_ids {
        let tiles   = world.tiles.counts.get(&pid).copied().unwrap_or(0).max(0) as u64;
        let is_npc  = world.players.get(&pid).map(|p| p.npc).unwrap_or(true);

        // (xp, qx, qy) for each milestone newly crossed this tick — usually empty or one entry.
        let grants: Vec<(f64, i32, i32)> = {
            let Some(q) = world.queens.get_mut(&pid) else { continue };
            if q.dead { continue; }
            q.cached_tiles = tiles;
            if is_npc || tiles <= q.tiles_ever_held {
                Vec::new()
            } else {
                let prev = q.tiles_ever_held;
                q.tiles_ever_held = tiles;
                let (qx, qy) = (q.x + q.size as i32 / 2, q.y + q.size as i32 / 2);
                TILE_MILESTONES.iter().enumerate()
                    .filter(|&(_, &t)| prev < t && t <= tiles)   // crossed this milestone this tick
                    .map(|(i, _)| (tile_award * (i + 1) as f64, qx, qy))
                    .collect()
            }
        };

        for (gained, qx, qy) in grants {
            world.xp_queue.push(XpGrant { player_id: pid, amount: gained, reason: "milestone", x: qx, y: qy });
        }
    }

    // =========================================================================
    // Phase 5: Clash resolution — reuse sorted pairs from Phase 2 rebuild
    // =========================================================================
    let convert_pct = c.convert_pct;
    let xp_convert  = c.xp_convert;

    let pairs = std::mem::take(&mut world.scratch_pairs);
    let n = pairs.len();
    let mut ci = 0;

    while ci < n {
        let k = pairs[ci].0;
        let cstart = ci;
        while ci < n && pairs[ci].0 == k { ci += 1; }
        if ci - cstart < 2 { continue; }

        // Only process cells with multiple owners
        let first_owner = world.ants[pairs[cstart].1 as usize].owner;
        let multi = (cstart + 1..ci).any(|j| world.ants[pairs[j].1 as usize].owner != first_owner);
        if !multi { continue; }

        let cell_y = (k / ww_u64) as i32;
        let cell_x = (k % ww_u64) as i32;

        let mut counts: FxHashMap<u32, u32> = FxHashMap::default();
        let mut total = 0u32;
        for dy in -2i32..=2 {
            for dx in -2i32..=2 {
                let tx = cell_x + dx; let ty = cell_y + dy;
                if tx < 0 || tx >= ww || ty < 0 || ty >= wh { continue; }
                let t = world.tiles.get(tx as u32, ty as u32);
                if t == 0 { continue; }
                *counts.entry(t).or_insert(0) += 1;
                total += 1;
            }
        }
        let dom = counts.iter().max_by_key(|(_, &v)| v).map(|(&k, _)| k).unwrap_or(0);
        let dom_count = counts.get(&dom).copied().unwrap_or(0);
        if total > 0 && dom_count as f64 / total as f64 >= convert_pct {
            let mut converted = false;
            for &(_, idx) in &pairs[cstart..ci] {
                let idx = idx as usize;
                if world.ants[idx].owner != dom {
                    world.ants[idx].owner = dom;
                    converted = true;
                }
            }
            if converted {
                world.xp_queue.push(XpGrant { player_id: dom, amount: xp_convert, reason: "convert", x: cell_x, y: cell_y });
                world.broadcast_near(cell_x, cell_y, &json!({"t":"clash","x":cell_x,"y":cell_y}).to_string());
            }
        }
    }
    world.scratch_pairs = pairs;

    // =========================================================================
    // Phase 6: Remove expired ants
    // =========================================================================
    world.ants.retain(|a| a.age <= a.lifespan);

    // =========================================================================
    // Phase 7: Rebuild ant_counts (replaces O(n_ants × n_players) scan in player_info)
    // =========================================================================
    world.ant_counts.clear();
    for a in &world.ants {
        *world.ant_counts.entry(a.owner).or_insert(0) += 1;
    }

    // =========================================================================
    // Phase 8: Passive queen HP regen (c.hp_regen HP per second) + shield expiry
    // =========================================================================
    let ticks_per_sec = c.tick_rate.max(1) as u64;
    if c.hp_regen > 0.0 && world.tick % ticks_per_sec == 0 {
        let gain = c.hp_regen.round() as i32;
        if gain > 0 {
            for q in world.queens.values_mut() {
                if !q.dead && q.hp < q.max_hp { q.hp = (q.hp + gain).min(q.max_hp); }
            }
        }
    }
    if world.tick % 50 == 0 {
        let now = current_ms();
        for q in world.queens.values_mut() {
            if let Some(exp) = q.shield_expiry {
                if now >= exp { q.shield = 0; q.shield_expiry = None; }
            }
        }
    }

    // =========================================================================
    // Phase 8b: Defender trigger (throttled every 25 ticks)
    // =========================================================================
    if world.tick % 25 == 0 && !world.ants.is_empty() {
        let now = current_ms();
        let def_range = DEFENDER_RANGE;
        let def_lifespan = c.lifespan;
        let player_ids: Vec<u32> = world.players.keys().copied().collect();
        for def_pid in player_ids {
            let is_npc = world.players.get(&def_pid).map(|p| p.npc).unwrap_or(true);
            if is_npc { continue; }
            let has_defenders = world.players.get(&def_pid).map(|p| !p.defenders.is_empty()).unwrap_or(false);
            if !has_defenders { continue; }
            let queen_data = world.queens.get(&def_pid)
                .filter(|q| !q.dead)
                .map(|q| (q.x, q.y, q.size as i32));
            let Some((qx, qy, qsize)) = queen_data else { continue };
            let qcx = qx + qsize / 2;
            let qcy = qy + qsize / 2;
            world.players.get_mut(&def_pid).unwrap().defenders.retain(|&exp| exp > now);
            if world.players.get(&def_pid).map(|p| p.defenders.is_empty()).unwrap_or(true) { continue; }
            // Nearest in-range enemy ant — the defender deploys toward it.
            let mut nearest: Option<(i32, i32, i32)> = None; // (chebyshev, ex, ey)
            for a in world.ants.iter() {
                if a.owner == def_pid { continue; }
                let ed = (a.x - qcx).abs().max((a.y - qcy).abs());
                if ed <= def_range && nearest.is_none_or(|(d, _, _)| ed < d) {
                    nearest = Some((ed, a.x, a.y));
                }
            }
            let Some((_, ex, ey)) = nearest else { continue };
            world.players.get_mut(&def_pid).unwrap().defenders.remove(0);
            // Spawn one tile OUTSIDE the queen footprint on the side facing the enemy, heading
            // outward — so the defender never lands on queen tiles (which left it stuck once the
            // queen's footprint grew with level).
            let (dx, dy) = (ex - qcx, ey - qcy);
            let (sx, sy, adx, ady) = if dx.abs() >= dy.abs() {
                if dx >= 0 { (qx + qsize, qcy, 1i8, 0i8) } else { (qx - 1, qcy, -1i8, 0i8) }
            } else if dy >= 0 {
                (qcx, qy + qsize, 0i8, 1i8)
            } else {
                (qcx, qy - 1, 0i8, -1i8)
            };
            let sx = sx.clamp(0, ww - 1);
            let sy = sy.clamp(0, wh - 1);
            world.ants.push(Ant::new(rand::random::<u32>(), def_pid, sx, sy, adx, ady, def_lifespan));
            let ev = json!({"t":"event","msg":"DEFENDER ACTIVATED!"}).to_string();
            world.send_to(def_pid, ev);
        }
    }

    // =========================================================================
    // Phase 9: Queen-queen physical collision
    // =========================================================================
    resolve_queen_collisions(world);

    // =========================================================================
    // Phase 10: HP-zero deaths
    // =========================================================================
    let dead: Vec<(u32, Option<u32>)> = world.queens.iter()
        .filter(|(_, q)| q.hp <= 0 && !q.dead)
        .map(|(&id, q)| (id, q.last_attacker))
        .collect();
    for (id, attacker) in dead {
        kill_queen(world, id, attacker, "hp");
    }

    // =========================================================================
    // Phase 11: Flush XP + throttled discovery / metro-holder upkeep
    // =========================================================================
    flush_xp(world);

    if world.tick % DISCOVERY_INTERVAL == 0 { sample_visited(world); }
    if world.tick % HOLDER_INTERVAL == 0 {
        recompute_holders(world);
        world.broadcast(&crate::network::build_region_holders(world));
    }

    // Season upkeep: sweep Dense chunks that became solid-one-owner (via clash conversions,
    // which bypass the inline fill-compaction) back into Uniform — keeps tile RAM proportional
    // to the painted *perimeter* rather than area over a month of churn. Cheap; throttled.
    if world.tick % COMPACT_INTERVAL == 0 {
        world.tiles.compact_pass();
    }
}

// ---- Queen-queen collision (Phase 9) ----------------------------------------

/// Resolve queen-queen physical overlaps: two live queens collide when their centres are within
/// `(avg size)/2 + 1` tiles; the lower-level one (tie → lower hp) is killed.
///
/// Queens can only touch within a handful of tiles, but a naive all-pairs scan is O(queens²)
/// (~500k pair tests at 1,000 queens, every tick, even with zero overlaps). Instead we bucket
/// queens into a coarse grid whose cell is larger than any possible collision distance, then only
/// compare queens within the same cell and its 8 neighbours — ~O(queens) for spread-out queens.
fn resolve_queen_collisions(world: &mut World) {
    use rustc_hash::FxHashMap;
    // Max centre distance at which any two queens collide: (8+8)/2 + 1 = 9 (size is 2..=8). A cell
    // of CELL tiles guarantees colliders share a cell or are in adjacent cells.
    const CELL: i64 = 16;
    // Snapshot (id, x, y, level, hp, size) once, then bucket — avoids the per-pair HashMap lookups.
    let qs: Vec<(u32, i32, i32, u16, i32, u8)> = world.queens.iter()
        .filter(|(_, q)| !q.dead)
        .map(|(&id, q)| (id, q.x, q.y, q.level, q.hp, q.size))
        .collect();
    if qs.len() < 2 { return; }

    let mut grid: FxHashMap<(i64, i64), Vec<usize>> = FxHashMap::default();
    for (idx, q) in qs.iter().enumerate() {
        grid.entry((q.1 as i64 / CELL, q.2 as i64 / CELL)).or_default().push(idx);
    }

    // For each queen, test only candidates in its cell + the 8 neighbours, and only the (a<b)
    // half to avoid testing a pair twice. Collisions are recorded, then applied after the scan
    // so kill_queen (which mutates world.queens) never runs mid-iteration.
    let mut collisions: Vec<(u32, u32)> = Vec::new();   // (loser, winner)
    for (a_idx, qa) in qs.iter().enumerate() {
        let (cx, cy) = (qa.1 as i64 / CELL, qa.2 as i64 / CELL);
        for ny in cy - 1..=cy + 1 {
            for nx in cx - 1..=cx + 1 {
                let Some(bucket) = grid.get(&(nx, ny)) else { continue };
                for &b_idx in bucket {
                    if b_idx <= a_idx { continue; }      // each unordered pair once
                    let qb = &qs[b_idx];
                    let min_dist = (qa.5 as f64 + qb.5 as f64) / 2.0 + 1.0;
                    let dx = (qa.1 - qb.1) as i64;
                    let dy = (qa.2 - qb.2) as i64;
                    if ((dx * dx + dy * dy) as f64).sqrt() < min_dist {
                        let (loser, winner) = if qa.3 < qb.3 || (qa.3 == qb.3 && qa.4 < qb.4) {
                            (qa.0, qb.0)
                        } else { (qb.0, qa.0) };
                        collisions.push((loser, winner));
                    }
                }
            }
        }
    }
    for (loser, winner) in collisions {
        kill_queen(world, loser, Some(winner), "collision");
    }
}

// ---- Helpers ----------------------------------------------------------------

/// Throttled discovery: sample up to `DISCOVERY_SAMPLE_N` live ants round-robin, map each to its
/// country + continent, and union into the owning account's visited sets. Cost is independent of
/// army size (fixed sample cap). NPC ants are skipped (no discovery view).
fn sample_visited(world: &mut World) {
    let n = world.ants.len();
    if n == 0 { return; }
    let take = DISCOVERY_SAMPLE_N.min(n);
    let mut idx = world.visit_sample_cursor % n;
    // Collect (owner, x, y) for real players first so we can drop the &ants borrow before mutating.
    let mut samples: Vec<(u32, i32, i32)> = Vec::with_capacity(take);
    for _ in 0..take {
        let a = &world.ants[idx];
        let is_npc = world.players.get(&a.owner).is_none_or(|p| p.npc);
        if !is_npc { samples.push((a.owner, a.x, a.y)); }
        idx += 1; if idx >= n { idx = 0; }
    }
    world.visit_sample_cursor = idx;
    for (owner, x, y) in samples {
        let (country, continent) = crate::regions::country_and_continent(x, y);
        if country == "Open Water" || country == "Unknown" { continue; }
        if let Some(p) = world.players.get_mut(&owner) {
            p.visited_countries.insert(country);
            if !continent.is_empty() { p.visited_continents.insert(continent); }
        }
    }
}

/// Throttled king-of-the-hill: for each metro, tally painted-tile owners within its radius (strided
/// sample) and record the leader. Bounded by the present chunks near each metro (sparse TileMap).
fn recompute_holders(world: &mut World) {
    let metros = crate::regions::metros_for_holder();
    let mut holders: Vec<MetroHolder> = Vec::with_capacity(metros.len());
    let mut tally: FxHashMap<u32, u64> = FxHashMap::default();
    let scale = (HOLDER_STRIDE as u64) * (HOLDER_STRIDE as u64);
    for (name, cx, cy, r2) in metros {
        tally.clear();
        world.tiles.tally_owners_in_circle(cx, cy, r2, HOLDER_STRIDE, &mut tally);
        let best = tally.iter().max_by_key(|(_, &c)| c).map(|(&id, &c)| (id, c));
        let (owner, tiles) = match best {
            Some((id, c)) => (Some(id), c * scale),
            None          => (None, 0),
        };
        holders.push(MetroHolder { name, owner, tiles });
    }
    world.metro_holders = holders;
}

/// Fills `out` with (dest_key, ant_index) pairs sorted by dest_key.
/// Uses `_nx/_ny` (planned destination) when `use_planned` is true, else `x/y` (current).
/// Reuses existing Vec allocation — clear() preserves capacity.
fn build_sorted_pairs(out: &mut Vec<(u64, u32)>, ants: &[crate::world::Ant], ww_u64: u64, use_planned: bool) {
    out.clear();
    if use_planned {
        out.extend(ants.iter().enumerate().map(|(i, a)| {
            (a._ny as u64 * ww_u64 + a._nx as u64, i as u32)
        }));
    } else {
        out.extend(ants.iter().enumerate().map(|(i, a)| {
            (a.y as u64 * ww_u64 + a.x as u64, i as u32)
        }));
    }
    out.par_sort_unstable_by_key(|&(k, _)| k);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::{Ant, World};

    /// Boundary regression: a brute's 2×2 footprint must never paint (or probe a queen cell)
    /// past the east/south world edge. Before the clamp it wrote a phantom tile at x == world_w
    /// and, via the `y*W + x` key, aliased onto row y+1 column 0. See tick_world Phase 1/3.
    #[test]
    fn brute_does_not_paint_past_world_edge() {
        crate::regions::init();
        let mut w = World::new();
        let ww = w.world_w as i32;
        let lifespan = crate::config::cfg().lifespan;
        // Brute one tile shy of the east edge; its 2×2 block reaches x == world_w.
        w.ants.push(Ant::new_kind(1, 100, ww - 1, 100, 1, 0, lifespan, 1));
        for _ in 0..6 { tick_world(&mut w); }      // brutes act on even ticks
        assert!(w.tiles.total_tiles() > 0, "brute should have painted in-bounds tiles");
        for y in 99..103u32 {
            assert_eq!(w.tiles.get(ww as u32, y), 0, "phantom paint at x=world_w (y={y})");
        }
    }

    /// The brute's Langton variant turns by its 2×2 footprint: all-friendly → 90° CW,
    /// all-white → 90° CCW, mixed → straight. Brutes act on even ticks, so we step to tick 2.
    #[test]
    fn brute_turns_by_2x2_footprint() {
        crate::regions::init();
        let lifespan = crate::config::cfg().lifespan;
        let owner = 7u32;

        // Heading of the lone brute after its first acting (even) tick, given a paint setup.
        let dir_after = |paint: &dyn Fn(&mut World, i32, i32)| -> (i8, i8) {
            let mut w = World::new();
            let (cx, cy) = (w.world_w as i32 / 2, w.world_h as i32 / 2);
            paint(&mut w, cx, cy);
            w.ants.push(Ant::new_kind(1, owner, cx, cy, 1, 0, lifespan, 1)); // heading east
            tick_world(&mut w);  // tick 1 (odd)  — brute idle
            tick_world(&mut w);  // tick 2 (even) — brute acts
            let a = &w.ants[0];
            (a.dx, a.dy)
        };

        // Behavior 3 — all white → CCW: turn_ccw(1,0) = (0,-1).
        assert_eq!(dir_after(&|_w, _x, _y| {}), (0, -1), "all-white footprint turns CCW");

        // Behavior 1 — all friendly → CW: turn_cw(1,0) = (0,1).
        assert_eq!(dir_after(&|w, x, y| {
            for dy in 0..2 { for dx in 0..2 { w.tiles.set((x + dx) as u32, (y + dy) as u32, owner); } }
        }), (0, 1), "all-friendly footprint turns CW");

        // Behavior 2 — mixed (one friendly cell, rest white) → straight: heading stays (1,0).
        assert_eq!(dir_after(&|w, x, y| {
            w.tiles.set(x as u32, y as u32, owner);
        }), (1, 0), "mixed footprint goes straight");
    }

    /// A brute hops a full 2×2 (2 tiles) per acting step, so each new footprint reads fresh cells
    /// (the fix for the 1-tile overlap → straight-line "highway loop"). Contrast: normal ants step 1.
    #[test]
    fn brute_hops_two_tiles() {
        crate::regions::init();
        let lifespan = crate::config::cfg().lifespan;
        let mut w = World::new();
        let (cx, cy) = (w.world_w as i32 / 2, w.world_h as i32 / 2);

        // Brute on an all-white field; a normal ant from the same spot for contrast.
        w.ants.push(Ant::new_kind(1, 7, cx, cy, 1, 0, lifespan, 1)); // [0] brute, east
        w.ants.push(Ant::new(2, 8, cx, cy, 1, 0, lifespan));         // [1] normal, east
        tick_world(&mut w);  // tick 1 (odd)  — brute idle
        tick_world(&mut w);  // tick 2 (even) — brute acts

        let b = &w.ants[0];
        assert_eq!((b.x - cx).abs() + (b.y - cy).abs(), 2, "brute hops exactly 2 tiles");
        // Axis-aligned hop (one axis by 2, the other unchanged).
        assert!((b.x == cx) ^ (b.y == cy), "brute hop stays axis-aligned");
    }

    fn mk_queen(x: i32, y: i32, level: u16, hp: i32) -> crate::world::Queen {
        crate::world::Queen { x, y, size: 2, hp, max_hp: 100, level, xp: 0.0, kills: 0,
            bubble_r: 30.0, last_attacker: None, dead: false, tiles_ever_held: 0, cached_tiles: 0,
            npc: false, shield: 0, shield_expiry: None, region: String::new() }
    }

    fn mk_player(id: u32) -> crate::world::Player {
        crate::world::Player {
            id, username: String::new(), color: String::new(), hue_idx: 0,
            ants_avail: 0, next_refill: 0, queen_placed_at: None, npc: false,
            view: None, tx: None, view_tx: None, conn_gen: 0, prestige: 0, credits: 0,
            defenders: Vec::new(), visited_countries: Default::default(),
            visited_continents: Default::default(), lifetime_kills: 0,
            lifetime_peak_tiles: 0, queens_fielded: 0, away: None,
        }
    }

    /// A tile milestone fires once when peak tiles first reach a threshold, and never again.
    /// With no ants, milestone XP is the only XP source, so the queen's xp isolates the award.
    #[test]
    fn tile_milestone_awards_once_per_threshold() {
        crate::regions::init();
        let award = crate::config::cfg().xp_tile_award;
        let mut w = World::new();
        let pid = 5u32;
        w.players.insert(pid, mk_player(pid));
        w.queens.insert(pid, mk_queen(2000, 2000, 1, 100));  // far from the painted block
        w.queen_map_dirty = true;

        // Paint exactly 10,000 tiles → crosses the first milestone (index 0 → award × 1).
        for y in 0..100u32 { for x in 0..100u32 { w.tiles.set(x, y, pid); } }
        assert_eq!(w.tiles.counts.get(&pid).copied().unwrap_or(0), 10_000);

        tick_world(&mut w);
        let xp = w.queens.get(&pid).unwrap().xp;
        assert_eq!(xp, award, "first milestone (10k) grants xp_tile_award");

        // No new ground gained → the threshold must not pay out a second time.
        tick_world(&mut w);
        assert_eq!(w.queens.get(&pid).unwrap().xp, xp, "milestone is one-time per queen");
    }

    #[test]
    fn queen_collision_lower_level_loses_others_untouched() {
        let mut w = World::new();
        w.queens.insert(1, mk_queen(100, 100, 5, 100));            // higher level → winner
        w.queens.insert(2, mk_queen(101, 100, 3, 100));            // 1 tile away → overlaps, loses
        w.queens.insert(3, mk_queen(500_000, 500_000, 1, 100));    // far away → untouched
        resolve_queen_collisions(&mut w);
        assert!(!w.queens.get(&1).unwrap().dead, "higher level survives");
        assert!( w.queens.get(&2).unwrap().dead, "lower level dies");
        assert!(!w.queens.get(&3).unwrap().dead, "distant queen untouched");
    }

    #[test]
    fn queen_collision_detected_across_cell_boundary() {
        // The bucketing must still catch colliders that straddle a grid-cell edge.
        let mut w = World::new();
        w.queens.insert(1, mk_queen(15, 100, 5, 100));   // cell x=0 (CELL=16)
        w.queens.insert(2, mk_queen(16, 100, 3, 100));   // cell x=1, 1 tile away → overlaps
        resolve_queen_collisions(&mut w);
        assert!(w.queens.get(&2).unwrap().dead, "collision found across the cell boundary");
    }

    #[test]
    fn queen_collision_ignores_distant_queens() {
        let mut w = World::new();
        w.queens.insert(1, mk_queen(100, 100, 5, 100));
        w.queens.insert(2, mk_queen(150, 100, 3, 100));  // 50 tiles apart → no collision
        resolve_queen_collisions(&mut w);
        assert!(!w.queens.get(&1).unwrap().dead);
        assert!(!w.queens.get(&2).unwrap().dead);
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use crate::world::{Ant, World};

    /// P3 acceptance: a tick must hold its budget at 100k ants. Run with
    /// `cargo test bench_tick_100k_ants -- --ignored --nocapture`. Asserts mean tick < 20 ms
    /// (the 50 Hz budget) — visual smoothness then comes from client interpolation (Phase 1).
    #[test]
    #[ignore]
    fn bench_tick_100k_ants() {
        crate::regions::init();
        let mut w = World::new();
        let ww = w.world_w as i32;
        let wh = w.world_h as i32;
        let lifespan = crate::config::cfg().lifespan;
        let n: usize = 100_000;

        // Deterministic LCG so the layout is stable across runs (no Math.random equivalent).
        let mut seed: u64 = 0x9E3779B97F4A7C15;
        let mut rng = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (seed >> 33) as u32 };

        // Cluster ants in a ~6000×6000 band around center (realistic density: many share chunks)
        // plus a spread across the four corners so all regions of the tick are exercised.
        let (cx, cy) = (ww / 2, wh / 2);
        let dirs = [(0i8, -1i8), (1, 0), (0, 1), (-1, 0)];
        for i in 0..n {
            let (bx, by) = match i % 5 {
                0 => (cx, cy),
                1 => (ww / 6, wh / 6),
                2 => (5 * ww / 6, wh / 6),
                3 => (ww / 6, 5 * wh / 6),
                _ => (5 * ww / 6, 5 * wh / 6),
            };
            let x = (bx + (rng() % 6000) as i32 - 3000).clamp(0, ww - 1);
            let y = (by + (rng() % 6000) as i32 - 3000).clamp(0, wh - 1);
            let (dx, dy) = dirs[i % 4];
            let owner = 100 + (i % 50) as u32; // 50 distinct owners
            w.ants.push(Ant::new(rng(), owner, x, y, dx, dy, lifespan));
        }

        for _ in 0..5 { tick_world(&mut w); }           // warm caches / first sorts
        let iters = 60;
        let t0 = std::time::Instant::now();
        for _ in 0..iters { tick_world(&mut w); }
        let per = t0.elapsed().as_secs_f64() * 1000.0 / iters as f64;
        let (chunks, uniform, dense, bytes) = w.tiles.stats();
        println!(
            "bench_tick_100k_ants: {:.3} ms/tick | ants={} chunks={} (u{} d{}) tileMB={:.2}",
            per, w.ants.len(), chunks, uniform, dense, bytes as f64 / (1024.0 * 1024.0)
        );
        assert!(per < 20.0, "tick {per:.3}ms exceeds the 20ms (50Hz) budget at 100k ants");
    }

    /// Scale acceptance for the OTHER axis: 1,000 live queens + 100k ants. The 100k-ant bench
    /// above runs with zero queens, so it never exercises Phase 9 (queen-queen collision), which
    /// is O(queens²). Queens here are spread far apart (no actual collisions — the realistic case),
    /// yet the pair scan still runs over all ~500k pairs every tick. Run with
    /// `cargo test bench_tick_1k_queens_100k_ants --release -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn bench_tick_1k_queens_100k_ants() {
        use crate::world::{Player, Queen};
        crate::regions::init();
        let mut w = World::new();
        let ww = w.world_w as i32;
        let wh = w.world_h as i32;
        let lifespan = crate::config::cfg().lifespan;

        const COLS: i32 = 40;
        const ROWS: i32 = 25;            // 40×25 = 1000 queens
        let qx = |j: i32| (ww * ((j % COLS) + 1)) / (COLS + 1);
        let qy = |j: i32| (wh * ((j / COLS) + 1)) / (ROWS + 1);

        for j in 0..1000u32 {
            let id = 1000 + j;
            let (x, y) = (qx(j as i32), qy(j as i32));
            w.queens.insert(id, Queen { x, y, size: 2, hp: 100, max_hp: 100, level: 1, xp: 0.0,
                kills: 0, bubble_r: 30.0, last_attacker: None, dead: false, tiles_ever_held: 0,
                cached_tiles: 0, npc: false, shield: 0, shield_expiry: None, region: String::new() });
            w.players.insert(id, Player { id, username: String::new(), color: String::new(),
                hue_idx: 0, ants_avail: 0, next_refill: 0, queen_placed_at: None, npc: false,
                view: None, tx: None, view_tx: None, conn_gen: 0, prestige: 0, credits: 0,
                defenders: Vec::new(), visited_countries: Default::default(),
                visited_continents: Default::default(), lifetime_kills: 0, lifetime_peak_tiles: 0,
                queens_fielded: 0, away: None });
        }
        w.queen_map_dirty = true;

        let mut seed: u64 = 0x1234_5678_9ABC_DEF0;
        let mut rng = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (seed >> 33) as u32 };
        let dirs = [(0i8, -1i8), (1, 0), (0, 1), (-1, 0)];
        for i in 0..100_000usize {
            let j = (i % 1000) as i32;                       // ants clustered near their owner queen
            let x = (qx(j) + (rng() % 400) as i32 - 200).clamp(0, ww - 1);
            let y = (qy(j) + (rng() % 400) as i32 - 200).clamp(0, wh - 1);
            let (dx, dy) = dirs[i % 4];
            w.ants.push(Ant::new(rng(), 1000 + j as u32, x, y, dx, dy, lifespan));
        }

        for _ in 0..5 { tick_world(&mut w); }
        let iters = 60;
        let t0 = std::time::Instant::now();
        for _ in 0..iters { tick_world(&mut w); }
        let per = t0.elapsed().as_secs_f64() * 1000.0 / iters as f64;
        let live = w.queens.values().filter(|q| !q.dead).count();
        println!("bench_tick_1k_queens_100k_ants: {per:.3} ms/tick | queens(live)={live} ants={}", w.ants.len());
        assert!(per < 20.0, "tick {per:.3}ms exceeds the 20ms (50Hz) budget at 1k queens / 100k ants");
    }

    /// Isolated cost of Phase 9 alone at 1,000 spread-out (non-colliding) queens — the metric the
    /// spatial bucketing targets. Run with
    /// `cargo test bench_queen_collisions_1k --release -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn bench_queen_collisions_1k() {
        use crate::world::Queen;
        let mut w = World::new();
        let ww = w.world_w as i32;
        let wh = w.world_h as i32;
        const COLS: i32 = 40;
        const ROWS: i32 = 25;
        for j in 0..1000i32 {
            let x = (ww * ((j % COLS) + 1)) / (COLS + 1);
            let y = (wh * ((j / COLS) + 1)) / (ROWS + 1);
            w.queens.insert(1000 + j as u32, Queen { x, y, size: 2, hp: 100, max_hp: 100, level: 1,
                xp: 0.0, kills: 0, bubble_r: 30.0, last_attacker: None, dead: false,
                tiles_ever_held: 0, cached_tiles: 0, npc: false, shield: 0, shield_expiry: None,
                region: String::new() });
        }
        let iters = 2000;
        let t0 = std::time::Instant::now();
        for _ in 0..iters { resolve_queen_collisions(&mut w); }
        let per_us = t0.elapsed().as_secs_f64() * 1e6 / iters as f64;
        let live = w.queens.values().filter(|q| !q.dead).count();
        println!("bench_queen_collisions_1k: {per_us:.2} us/call | queens={live}");
    }
}
