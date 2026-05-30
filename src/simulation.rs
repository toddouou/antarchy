use rustc_hash::FxHashMap;
use rand::Rng;
use rayon::prelude::*;
use serde_json::json;

use crate::config::{
    cfg, queen_size_for_level, level_for_xp, current_ms, ENEMY_HUES,
};
use crate::world::{Ant, Player, Queen, QueenHit, World, XpGrant};

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
                q.level  = new_lvl;
                q.size   = queen_size_for_level(new_lvl);
                q.max_hp = c.hp_base * new_lvl as i32;
                q.hp     = q.max_hp.min(q.hp + c.hp_base);
                Some((old_lvl, new_lvl))
            } else {
                None
            }
        };

        // Award 1 credit per XP point earned (persists through prestige)
        if let Some(p) = world.players.get_mut(&g.player_id) {
            p.credits += g.amount.max(0.0) as u64;
        }

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
    let (qx, qy) = {
        let Some(q) = world.queens.get_mut(&loser_id) else { return };
        if q.dead { return; }
        q.dead = true;
        (q.x, q.y)
    };
    world.queen_map_dirty = true;

    // Prestige: increment on each queen death
    if let Some(p) = world.players.get_mut(&loser_id) {
        p.prestige += 1;
    }

    let near_msg = json!({"t":"queen-killed","x":qx,"y":qy}).to_string();
    world.broadcast_near(qx, qy, &near_msg);

    let loser_name = world.players.get(&loser_id)
        .map(|p| p.username.clone())
        .unwrap_or_else(|| loser_id.to_string());
    world.broadcast(&json!({"t":"event","msg":format!("♛ {loser_name} has fallen! ({reason})")}).to_string());

    if let Some(kid) = killer_id {
        if let Some(kq) = world.queens.get_mut(&kid) { kq.kills += 1; }
        let kill_xp = cfg().xp_kill;
        award_xp(world, kid, kill_xp, "kill", qx, qy);
        flush_xp(world);
        world.send_to(kid, json!({"t":"event","msg":format!("KILL! +{} XP", kill_xp as i64)}).to_string());
    }

    world.ants.retain(|a| a.owner != loser_id);
    world.send_to(loser_id, json!({"t":"queen-dead"}).to_string());
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
    let ww = world.world_w as i32;
    let wh = world.world_h as i32;
    drop(c);

    let npc_size: u8 = 2;
    world.queens.insert(id, Queen {
        x: cx, y: cy, size: npc_size,
        hp: 100, max_hp: 100, level: 1, xp: 0.0, kills: 0,
        bubble_r, last_attacker: None, dead: false,
        tiles_ever_held: 0, cached_tiles: 0, npc: true,
    });
    world.players.insert(id, Player {
        id, username: format!("NPC_{id}"), color: hue,
        hue_idx: hue_idx as i32,
        ants_avail: 0, next_refill: 0, queen_placed_at: None,
        npc: true, view: None, tx: None, view_tx: None, conn_gen: 0,
        prestige: 0, credits: 0, last_sent_dirty: 0,
    });
    world.queen_map_dirty = true;

    for dy in 0..npc_size as i32 {
        for dx in 0..npc_size as i32 {
            world.tiles.set((cx + dx) as u32, (cy + dy) as u32, id);
        }
    }

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
    if world.tick % 64 == 0 && !world.ants.is_empty() {
        world.ants.sort_unstable_by_key(|a| (a.owner, a.x, a.y, a.dx as i32, a.dy as i32));
        world.ants.dedup_by_key(|a| (a.owner, a.x, a.y, a.dx, a.dy));
    }

    // --- Spatial sort every 50 ticks: group ants by 256×256 chunk for cache locality ---
    if world.tick % 50 == 0 && !world.ants.is_empty() {
        let chunk_w = world.world_w / 256 + 1;
        world.ants.sort_unstable_by_key(|a| {
            let cx = a.x as u32 / 256;
            let cy = a.y as u32 / 256;
            cy * chunk_w + cx
        });
    }

    // =========================================================================
    // Phase 1: Plan moves — Rayon parallel, no Mutex, fold/reduce for accumulation
    // =========================================================================
    let (hits, xp_grants) = {
        let tiles     = &world.tiles;
        let queen_map = &world.queen_map;

        world.ants.par_iter_mut()
            .fold(
                || (Vec::<QueenHit>::new(), Vec::<XpGrant>::new()),
                |(mut hits, mut xp), ant| {
                    let cur = tiles.get(ant.x as u32, ant.y as u32);

                    let (mut ndx, mut ndy) = if cur == 0 {
                        turn_ccw(ant.dx, ant.dy)
                    } else if cur == ant.owner {
                        turn_cw(ant.dx, ant.dy)
                    } else {
                        (ant.dx, ant.dy)
                    };

                    let mut nx = ant.x + ndx as i32;
                    let mut ny = ant.y + ndy as i32;

                    if nx < 0 || nx >= ww || ny < 0 || ny >= wh {
                        ndx = -ndx; ndy = -ndy;
                        nx = (ant.x + ndx as i32).clamp(0, ww - 1);
                        ny = (ant.y + ndy as i32).clamp(0, wh - 1);
                    }

                    let dest_key = ny as u64 * ww_u64 + nx as u64;
                    if let Some(&queen_id) = queen_map.get(&dest_key) {
                        if queen_id != ant.owner {
                            hits.push(QueenHit { queen_id, attacker: ant.owner, is_own: false });
                            xp.push(XpGrant { player_id: ant.owner, amount: 0.5, reason: "hit", x: ant.x, y: ant.y });
                        } else {
                            hits.push(QueenHit { queen_id, attacker: ant.owner, is_own: true });
                        }
                        (ndx, ndy) = turn_cw(ndx, ndy);
                        nx = ant.x; ny = ant.y;
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
            *damage_map.entry(hit.queen_id).or_insert(0.0) += c.ant_damage;
            last_attacker_map.insert(hit.queen_id, hit.attacker);
        }
    }

    // Apply batched damage and send one notification per struck queen
    for (&queen_id, &total_dmg) in &damage_map {
        let (qx, qy) = {
            let Some(q) = world.queens.get_mut(&queen_id) else { continue };
            if q.dead { continue; }
            q.hp = (q.hp as f64 - total_dmg).max(0.0) as i32;
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
            let (ax, ay, anx, any, andx, andy) = {
                let a = &world.ants[idx];
                (a.x, a.y, a._nx, a._ny, a._ndx, a._ndy)
            };
            if anx == ax && any == ay { continue; }
            let (cdx, cdy) = turn_cw(andx, andy);
            let mut cx = ax + cdx as i32;
            let mut cy = ay + cdy as i32;
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
        ant.dx = ant._ndx; ant.dy = ant._ndy;
        ant.x  = ant._nx;  ant.y  = ant._ny;
        ant.age += 1;
    }

    // =========================================================================
    // Phase 4: Tile milestone XP + cache update
    // =========================================================================
    let milestone  = c.xp_tile_milestone;
    let tile_award = c.xp_tile_award;
    let queen_ids: Vec<u32> = world.queens.keys().copied().collect();
    for pid in queen_ids {
        let tiles   = world.tiles.counts.get(&pid).copied().unwrap_or(0).max(0) as u64;
        let is_npc  = world.players.get(&pid).map(|p| p.npc).unwrap_or(true);

        let milestone_gain = {
            let Some(q) = world.queens.get_mut(&pid) else { continue };
            if q.dead { continue; }
            q.cached_tiles = tiles;
            if !is_npc && tiles > q.tiles_ever_held {
                let prev = q.tiles_ever_held;
                q.tiles_ever_held = tiles;
                let m_before = prev  / milestone;
                let m_now    = tiles / milestone;
                if m_now > m_before {
                    let gained = (m_now - m_before) as f64 * tile_award;
                    Some((gained, q.x + q.size as i32 / 2, q.y + q.size as i32 / 2))
                } else { None }
            } else { None }
        };

        if let Some((gained, qx, qy)) = milestone_gain {
            world.xp_queue.push(XpGrant { player_id: pid, amount: gained, reason: "milestone", x: qx, y: qy });
            let tx = world.players.get(&pid).and_then(|p| p.tx.clone());
            if let Some(tx) = tx {
                let _ = tx.send(json!({"t":"event","msg":format!("TILE MILESTONE: +{} XP!", gained as i64)}).to_string());
            }
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
            for j in cstart..ci {
                let idx = pairs[j].1 as usize;
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
    // Phase 8: Passive queen HP regen (1 HP per 10 ticks)
    // =========================================================================
    if world.tick % 10 == 0 {
        for q in world.queens.values_mut() {
            if !q.dead && q.hp < q.max_hp { q.hp = (q.hp + 1).min(q.max_hp); }
        }
    }

    // =========================================================================
    // Phase 9: Queen-queen physical collision
    // =========================================================================
    let live: Vec<u32> = world.queens.iter().filter(|(_, q)| !q.dead).map(|(&id, _)| id).collect();
    for i in 0..live.len() {
        for j in i + 1..live.len() {
            let (a, b) = (live[i], live[j]);
            let (qa_data, qb_data) = {
                let qa = world.queens.get(&a).unwrap();
                let qb = world.queens.get(&b).unwrap();
                ((qa.x, qa.y, qa.size, qa.level, qa.hp), (qb.x, qb.y, qb.size, qb.level, qb.hp))
            };
            let min_dist = (qa_data.2 as f64 + qb_data.2 as f64) / 2.0 + 1.0;
            let dx = qa_data.0 - qb_data.0; let dy = qa_data.1 - qb_data.1;
            if ((dx * dx + dy * dy) as f64).sqrt() < min_dist {
                let (loser, winner) = if qa_data.3 < qb_data.3 || (qa_data.3 == qb_data.3 && qa_data.4 < qb_data.4) {
                    (a, b)
                } else { (b, a) };
                kill_queen(world, loser, Some(winner), "collision");
            }
        }
    }

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
    // Phase 11: Flush XP
    // =========================================================================
    flush_xp(world);
}

// ---- Helpers ----------------------------------------------------------------

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
