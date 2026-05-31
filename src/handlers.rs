use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;

use crate::auth::{hash_pw, UserRecord};
use crate::config::{
    cfg, apply_admin_param, reset_to_defaults, queen_size_for_level, total_xp_for_level, level_for_xp,
    current_ms, HUES, ADMIN_USERNAME,
};
use crate::network::{build_leaderboard, build_player_info};
use crate::simulation::{kill_queen, spawn_npc, wipe_world};
use crate::world::{Ant, Player, PlayerView, Queen, World};

fn err(msg: &str) -> String {
    json!({"t":"err","msg":msg}).to_string()
}

/// Returns (player_id, msg_to_send) or None if message is silently ignored.
pub fn handle_message(
    world:     &mut World,
    player_id: &mut Option<u32>,
    tx:        &UnboundedSender<String>,
    msg:       Value,
) {
    let t = msg.get("t").and_then(Value::as_str).unwrap_or("");

    // ---- Register ----
    if t == "register" {
        let raw_u = msg["username"].as_str().unwrap_or("").trim().to_uppercase();
        let pw    = msg["password"].as_str().unwrap_or("");
        if raw_u.len() < 3 || raw_u.len() > 20 {
            let _ = tx.send(err("Username 3-20 chars")); return;
        }
        if pw.len() < 4 {
            let _ = tx.send(err("Password min 4 chars")); return;
        }
        if world.auth.users.contains_key(&raw_u) {
            let _ = tx.send(err("Username taken")); return;
        }
        let hue_idx = msg["hueIdx"].as_i64().unwrap_or(0) as i32;
        let color   = msg["color"].as_str().unwrap_or(HUES.first().copied().unwrap_or("#ff2e3f")).to_string();
        let id      = world.next_player_id;
        world.next_player_id += 1;
        world.auth.users.insert(raw_u.clone(), UserRecord {
            id, username: raw_u.clone(),
            password_hash: hash_pw(pw),
            color: color.clone(), hue_idx,
            is_admin: false, color_chosen: true,
        });
        world.auth.save();
        let welcome = create_or_reconnect_player(world, id, &raw_u, &color, hue_idx, false, tx.clone());
        *player_id = Some(id);
        let me = build_player_info(world, id);
        let lb = build_leaderboard(world);
        let _ = tx.send(json!({"t":"logged-in","me":serde_json::from_str::<Value>(&me).unwrap_or(Value::Null)}).to_string());
        let _ = tx.send(lb);
        if let Some(w) = welcome { let _ = tx.send(w); }
        return;
    }

    // ---- Login ----
    if t == "login" {
        let u   = msg["username"].as_str().unwrap_or("").trim().to_uppercase();
        let pw  = msg["password"].as_str().unwrap_or("");
        let rec = world.auth.users.get(&u).cloned();
        let Some(rec) = rec else { let _ = tx.send(err("Invalid credentials")); return; };
        if rec.password_hash != hash_pw(pw) { let _ = tx.send(err("Invalid credentials")); return; }
        if world.auth.banned.contains(&u) { let _ = tx.send(err("BANNED")); return; }
        let welcome = create_or_reconnect_player(world, rec.id, &rec.username, &rec.color, rec.hue_idx, rec.is_admin, tx.clone());
        *player_id = Some(rec.id);
        let me = build_player_info(world, rec.id);
        let lb = build_leaderboard(world);
        let _ = tx.send(json!({"t":"logged-in","me":serde_json::from_str::<Value>(&me).unwrap_or(Value::Null)}).to_string());
        let _ = tx.send(lb);
        if let Some(w) = welcome { let _ = tx.send(w); }
        return;
    }

    // All subsequent messages require a logged-in player
    let Some(pid) = *player_id else {
        let _ = tx.send(err("Not logged in")); return;
    };

    let is_admin = world.auth.is_admin_id(pid);

    // ---- Set color (admin only) ----
    if t == "set-color" {
        if !is_admin { return; }
        let color = msg["color"].as_str().unwrap_or("").to_string();
        if !HUES.contains(&color.as_str()) { let _ = tx.send(err("Invalid color")); return; }
        let hue_idx = msg["hueIdx"].as_i64().unwrap_or(0) as i32;
        if let Some(p) = world.players.get_mut(&pid) {
            p.color = color.clone();
            p.hue_idx = hue_idx;
        }
        if let Some(u) = world.auth.users.get_mut(ADMIN_USERNAME) {
            u.color = color;
            u.hue_idx = hue_idx;
            u.color_chosen = true;
        }
        world.auth.save();
        let me = build_player_info(world, pid);
        let lb = build_leaderboard(world);
        let _ = tx.send(json!({"t":"logged-in","me":serde_json::from_str::<Value>(&me).unwrap_or(Value::Null)}).to_string());
        let _ = tx.send(lb);
        return;
    }

    // ---- View set ----
    if t == "view-set" {
        let x0 = msg["x0"].as_i64().unwrap_or(0) as i32;
        let y0 = msg["y0"].as_i64().unwrap_or(0) as i32;
        let x1 = msg["x1"].as_i64().unwrap_or(0) as i32;
        let y1 = msg["y1"].as_i64().unwrap_or(0) as i32;
        if let Some(p) = world.players.get_mut(&pid) {
            p.view = Some(PlayerView { x0, y0, x1, y1 });
        }
        // Viewport is delivered by sim_loop on the next send_viewports cycle (≤ 40 ms).
        // Building it here under the write lock would block all other players during fog computation.
        return;
    }

    // ---- Forbidden zones ----
    if t == "get-forbidden-zones" {
        let zones: Vec<Value> = world.queens.iter()
            .filter(|(&qid, q)| !q.dead && qid != pid)
            .map(|(_, q)| {
                json!({
                    "x": q.x as f64 + q.size as f64 / 2.0,
                    "y": q.y as f64 + q.size as f64 / 2.0,
                    "r": q.bubble_r,
                })
            })
            .collect();
        let _ = tx.send(json!({"t":"forbidden-zones","zones":zones}).to_string());
        return;
    }

    // ---- Place queen ----
    if t == "place-queen" {
        if world.queens.get(&pid).map(|q| !q.dead).unwrap_or(false) {
            let _ = tx.send(err("Already have a queen")); return;
        }
        let x = msg["x"].as_i64().unwrap_or(-1) as i32;
        let y = msg["y"].as_i64().unwrap_or(-1) as i32;
        let c = cfg();
        let ww = world.world_w as i32; let wh = world.world_h as i32;
        if x < 2 || y < 2 || x >= ww - 8 || y >= wh - 8 {
            let _ = tx.send(err("Out of bounds")); return;
        }
        if world.too_close_to_queen(x, y, pid) {
            let _ = tx.send(err("Too close to another queen")); return;
        }
        let size = queen_size_for_level(1);
        let max_hp = c.hp_base;
        let bubble_r = c.bubble_r;
        drop(c);
        let (q_country, q_cont) = crate::regions::country_and_continent(x, y);
        if let Some(p) = world.players.get_mut(&pid) {
            p.queen_placed_at = Some(current_ms());
            p.queens_fielded += 1;
            if q_country != "Open Water" && q_country != "Unknown" {
                p.visited_countries.insert(q_country);
                if !q_cont.is_empty() { p.visited_continents.insert(q_cont); }
            }
        }
        world.queens.insert(pid, Queen {
            x, y, size, hp: max_hp, max_hp, level: 1, xp: 0.0, kills: 0,
            bubble_r, last_attacker: None, dead: false,
            tiles_ever_held: 0, cached_tiles: 0, npc: false,
            shield: 0, shield_expiry: None,
            region: crate::regions::region_for(x, y),
        });
        world.queen_map_dirty = true;
        world.dirty_tick = world.tick; // tiles are about to change
        world.paint_queen_body(x, y, size, pid);
        let _ = tx.send(json!({"t":"queen-placed","x":x,"y":y}).to_string());
        let _ = tx.send(json!({"t":"event","msg":"QUEEN PLACED · DEPLOY ANTS WITHIN BUBBLE"}).to_string());
        return;
    }

    // ---- Place ant ----
    if t == "place-ant" {
        let c = cfg();
        // Copy out p/q data before dropping borrows — get_queen_map() needs &mut World
        let ants_avail = match world.players.get(&pid) {
            Some(p) => p.ants_avail,
            None => return,
        };
        if ants_avail <= 0 { let _ = tx.send(err("No ants available")); return; }
        let army = world.ant_counts.get(&pid).copied().unwrap_or(0) as i32;
        if army >= c.army_cap { let _ = tx.send(err("Army at capacity")); return; }
        let queen_data = world.queens.get(&pid)
            .filter(|q| !q.dead)
            .map(|q| (q.x, q.y, q.size, q.bubble_r));
        let Some((qx, qy, qs, bubble_r)) = queen_data else {
            let _ = tx.send(err("Place queen first")); return;
        };
        // p and q borrows are dead here — safe to call get_queen_map (&mut World)
        let x = msg["x"].as_i64().unwrap_or(-1) as i32;
        let y = msg["y"].as_i64().unwrap_or(-1) as i32;
        let vdx = msg["dx"].as_i64().unwrap_or(0) as i8;
        let vdy = msg["dy"].as_i64().unwrap_or(-1) as i8;
        let (adx, ady) = match validate_worker_placement(world, pid, (x, y), (vdx, vdy), (qx, qy, qs, bubble_r)) {
            Ok(d)  => d,
            Err(e) => { let _ = tx.send(err(e)); return; }
        };
        let lifespan = c.lifespan;
        drop(c);
        // Silently skip placement if an identical ant (same owner, position, direction) already
        // exists here. Two stacked same-direction ants cancel each other's painting and travel
        // in a straight line forever — the tick dedup catches existing cases, this prevents new ones.
        if world.ants.iter().any(|a| a.owner == pid && a.x == x && a.y == y && a.dx == adx && a.dy == ady) {
            return;
        }
        let ant_id = rand::random::<u32>();
        world.ants.push(Ant::new(ant_id, pid, x, y, adx, ady, lifespan));
        if let Some(p) = world.players.get_mut(&pid) { p.ants_avail -= 1; }
        let _ = tx.send(json!({"t":"ant-placed","x":x,"y":y}).to_string());
        return;
    }

    // ---- Admin slider ----
    if t == "admin" {
        if !is_admin { let _ = tx.send(err("Admin only")); return; }
        let key = msg["key"].as_str().unwrap_or("");
        let val = msg["value"].as_f64().unwrap_or(f64::NAN);
        if val.is_nan() { return; }
        if let Some(clamped) = apply_admin_param(key, val) {
            let cfg_msg = json!({"t":"cfg","key":key,"value":clamped}).to_string();
            world.broadcast(&cfg_msg);
        }
        return;
    }

    // ---- Admin actions ----
    if t == "admin-action" {
        if !is_admin { let _ = tx.send(err("Admin only")); return; }
        let act = msg["action"].as_str().unwrap_or("");
        let target_id = msg["target"].as_u64().map(|v| v as u32).unwrap_or(pid);

        match act {
            "add-ants" => {
                if let Some(tp) = world.players.get_mut(&target_id) {
                    tp.ants_avail += 10;
                    if let Some(ttx) = &tp.tx {
                        let _ = ttx.send(json!({"t":"event","msg":"+10 ANTS (ADMIN)"}).to_string());
                    }
                }
            }
            "heal-queen" => {
                if let Some(qh) = world.queens.get_mut(&target_id) {
                    qh.hp = qh.max_hp;
                }
                if let Some(tp) = world.players.get(&target_id) {
                    if let Some(ttx) = &tp.tx {
                        let _ = ttx.send(json!({"t":"event","msg":"QUEEN HEALED (ADMIN)"}).to_string());
                    }
                }
            }
            "level-up" => {
                let c = cfg().clone();
                // Read queen state first, drop borrow, then mutate separately
                let queen_state = world.queens.get(&target_id)
                    .filter(|q| !q.dead)
                    .map(|q| (q.level, q.hp));
                let Some((cur_level, cur_hp)) = queen_state else { return };
                if cur_level >= c.xp_level_cap { return; }
                let new_xp  = total_xp_for_level(cur_level + 1, &c);
                let new_lvl = level_for_xp(new_xp, &c);
                let ants_gained = (new_lvl as i32 - cur_level as i32).max(0) * c.levelup_ant_grant;
                if let Some(ql) = world.queens.get_mut(&target_id) {
                    ql.xp    = new_xp;
                    ql.set_level(new_lvl, &c);
                    ql.hp    = ql.max_hp.min(cur_hp + c.hp_base);
                }
                if ants_gained > 0 {
                    if let Some(tp) = world.players.get_mut(&target_id) {
                        tp.ants_avail += ants_gained;
                    }
                }
                world.queen_map_dirty = true;
                world.send_to(target_id, json!({"t":"event","msg":"[ADMIN] LEVEL UP"}).to_string());
            }
            "level-down" => {
                let c = cfg().clone();
                let queen_state = world.queens.get(&target_id)
                    .filter(|q| !q.dead && q.level > 1)
                    .map(|q| (q.level, q.xp, q.hp));
                let Some((cur_level, cur_xp, cur_hp)) = queen_state else { return };
                let new_level = cur_level - 1;
                let floor = total_xp_for_level(new_level, &c);
                let ceil  = total_xp_for_level(new_level + 1, &c) - 1.0;
                if let Some(qd) = world.queens.get_mut(&target_id) {
                    qd.set_level(new_level, &c);
                    qd.hp     = cur_hp.min(qd.max_hp);
                    qd.xp     = cur_xp.clamp(floor, ceil);
                }
                world.queen_map_dirty = true;
                world.send_to(target_id, json!({"t":"event","msg":"[ADMIN] LEVEL DOWN"}).to_string());
            }
            "spawn-npc"  => spawn_npc(world, pid, None, None),
            "wipe-world" => wipe_world(world),
            _ => {}
        }
        return;
    }

    // ---- Admin target ----
    if t == "admin-target" {
        if !is_admin { let _ = tx.send(err("Admin only")); return; }
        let tid    = msg["targetId"].as_u64().unwrap_or(0) as u32;
        let action = msg["action"].as_str().unwrap_or("");
        match action {
            "reset-hp" => {
                if let Some(q) = world.queens.get_mut(&tid) { q.hp = q.max_hp; }
            }
            "delete-queen" => kill_queen(world, tid, None, "admin"),
            "move-queen" => {
                let nx = msg["x"].as_i64().unwrap_or(0) as i32;
                let ny = msg["y"].as_i64().unwrap_or(0) as i32;
                // Read old position — borrow released immediately after .map()
                let old_pos = world.queens.get(&tid).map(|q| (q.x, q.y, q.size));
                let Some((ox, oy, sz)) = old_pos else { return };
                world.clear_queen_body(ox, oy, sz, tid);
                if let Some(q) = world.queens.get_mut(&tid) { q.x = nx; q.y = ny; q.region = crate::regions::region_for(nx, ny); }
                world.paint_queen_body(nx, ny, sz, tid);
                world.queen_map_dirty = true;
            }
            "ban-player" => {
                if let Some(bp) = world.players.get(&tid) {
                    let uname = bp.username.clone();
                    if let Some(btx) = &bp.tx { let _ = btx.send("".to_string()); }
                    world.auth.banned.insert(uname.clone());
                    let msg = json!({"t":"event","msg":format!("[ADMIN] {uname} BANNED")}).to_string();
                    world.broadcast(&msg);
                }
            }
            _ => {}
        }
        return;
    }

    // ---- Admin pause ----
    if t == "admin-pause" {
        if !is_admin { let _ = tx.send(err("Admin only")); return; }
        let paused = msg["paused"].as_bool().unwrap_or(false);
        world.paused = paused;
        let msg = json!({"t":"admin-pause","paused":paused}).to_string();
        world.broadcast(&msg);
        return;
    }

    // ---- Admin kick (disconnect, no ban) ----
    if t == "admin-kick" {
        if !is_admin { let _ = tx.send(err("Admin only")); return; }
        let tid = msg["targetId"].as_u64().unwrap_or(0) as u32;
        if let Some(tp) = world.players.get(&tid) {
            let uname = tp.username.clone();
            if let Some(ttx) = &tp.tx { let _ = ttx.send("".to_string()); }
            let ev = json!({"t":"event","msg":format!("[ADMIN] {uname} KICKED")}).to_string();
            world.broadcast(&ev);
        }
        return;
    }

    // ---- Admin set level (exact) ----
    if t == "admin-set-level" {
        if !is_admin { let _ = tx.send(err("Admin only")); return; }
        let tid   = msg["targetId"].as_u64().unwrap_or(0) as u32;
        let level = msg["level"].as_u64().unwrap_or(1) as u16;
        let c = cfg().clone();
        let level = level.clamp(1, c.xp_level_cap);
        if let Some(q) = world.queens.get_mut(&tid) {
            if !q.dead {
                let old_lvl = q.level;
                q.set_level(level, &c);
                q.xp      = total_xp_for_level(level, &c);
                q.hp      = q.hp.min(q.max_hp);
                let ants_delta = (level as i32 - old_lvl as i32).max(0) * c.levelup_ant_grant;
                if ants_delta > 0 {
                    if let Some(tp) = world.players.get_mut(&tid) { tp.ants_avail += ants_delta; }
                }
                world.queen_map_dirty = true;
            }
        }
        world.send_to(tid, json!({"t":"event","msg":format!("[ADMIN] LEVEL SET TO {level}")}).to_string());
        return;
    }

    // ---- Admin give XP ----
    if t == "admin-give-xp" {
        if !is_admin { let _ = tx.send(err("Admin only")); return; }
        let tid = msg["targetId"].as_u64().unwrap_or(0) as u32;
        let xp  = msg["xp"].as_f64().unwrap_or(0.0);
        if xp <= 0.0 { return; }
        let c = cfg().clone();
        if let Some(q) = world.queens.get_mut(&tid) {
            if !q.dead {
                q.xp += xp;
                let new_lvl = level_for_xp(q.xp, &c);
                if new_lvl > q.level {
                    let ants_delta = (new_lvl as i32 - q.level as i32).max(0) * c.levelup_ant_grant;
                    q.set_level(new_lvl, &c);
                    q.hp      = q.max_hp;
                    world.queen_map_dirty = true;
                    if ants_delta > 0 {
                        if let Some(tp) = world.players.get_mut(&tid) { tp.ants_avail += ants_delta; }
                    }
                    world.send_to(tid, json!({"t":"level-up","level":new_lvl}).to_string());
                }
            }
        }
        world.send_to(tid, json!({"t":"event","msg":format!("+{xp} XP (ADMIN)")}).to_string());
        return;
    }

    // ---- Admin set ants ----
    if t == "admin-set-ants" {
        if !is_admin { let _ = tx.send(err("Admin only")); return; }
        let tid   = msg["targetId"].as_u64().unwrap_or(0) as u32;
        let count = msg["count"].as_i64().unwrap_or(0).max(0) as i32;
        if let Some(tp) = world.players.get_mut(&tid) {
            tp.ants_avail = count;
            if let Some(ttx) = &tp.tx {
                let _ = ttx.send(json!({"t":"event","msg":format!("ANTS SET TO {count} (ADMIN)")}).to_string());
            }
        }
        return;
    }

    // ---- Admin broadcast ----
    if t == "admin-broadcast" {
        if !is_admin { let _ = tx.send(err("Admin only")); return; }
        let raw_msg = msg["msg"].as_str().unwrap_or("").to_string();
        if raw_msg.is_empty() { return; }
        let target_id = msg["targetId"].as_u64().map(|v| v as u32);
        let ev = json!({"t":"event","msg":format!("[ADMIN] {raw_msg}")}).to_string();
        if let Some(tid) = target_id {
            world.send_to(tid, ev);
        } else {
            world.broadcast(&ev);
        }
        return;
    }

    // ---- Admin spawn NPC at position ----
    if t == "admin-spawn-at" {
        if !is_admin { let _ = tx.send(err("Admin only")); return; }
        let x = msg["x"].as_i64().unwrap_or(0) as i32;
        let y = msg["y"].as_i64().unwrap_or(0) as i32;
        spawn_npc(world, pid, Some(x), Some(y));
        return;
    }

    // ---- Admin place ant for a target player (override) ----
    if t == "admin-place-ant" {
        if !is_admin { let _ = tx.send(err("Admin only")); return; }
        let target_id = msg["targetId"].as_u64().unwrap_or(0) as u32;
        // The owner id drives palette color / rendering; a live queen is not required.
        if !world.players.contains_key(&target_id) { return; }
        let x = msg["x"].as_i64().unwrap_or(-1) as i32;
        let y = msg["y"].as_i64().unwrap_or(-1) as i32;
        let ww = world.world_w as i32; let wh = world.world_h as i32;
        if x < 0 || y < 0 || x >= ww || y >= wh { let _ = tx.send(err("Out of bounds")); return; }
        // Default direction up; no bubble / territory / ants_avail restrictions (admin override).
        let (adx, ady): (i8, i8) = (0, -1);
        // Skip placement if an identical ant (same owner, position, direction) already exists —
        // two stacked same-direction ants cancel each other's painting and run straight forever.
        if world.ants.iter().any(|a| a.owner == target_id && a.x == x && a.y == y && a.dx == adx && a.dy == ady) {
            return;
        }
        let lifespan = cfg().lifespan;
        let ant_id = rand::random::<u32>();
        world.ants.push(Ant::new(ant_id, target_id, x, y, adx, ady, lifespan));
        world.dirty_tick = world.tick; // tiles are about to change
        let _ = tx.send(json!({"t":"ant-placed","x":x,"y":y}).to_string());
        return;
    }

    // ---- Admin player list ----
    if t == "admin-player-list" {
        if !is_admin { let _ = tx.send(err("Admin only")); return; }
        let players: Vec<serde_json::Value> = world.players.iter()
            .map(|(id, p)| {
                let q = world.queens.get(id);
                json!({
                    "id":       id,
                    "username": p.username,
                    "npc":      p.npc,
                    "online":   p.tx.is_some(),
                    "level":    q.map(|q| q.level).unwrap_or(0),
                    "tiles":    q.map(|q| q.cached_tiles).unwrap_or(0),
                    "hp":       q.map(|q| q.hp).unwrap_or(0),
                    "maxHp":    q.map(|q| q.max_hp).unwrap_or(0),
                    "ants":     p.ants_avail,
                    "prestige": p.prestige,
                    "credits":  p.credits,
                    "qx":       q.map(|q| q.x).unwrap_or(-1),
                    "qy":       q.map(|q| q.y).unwrap_or(-1),
                })
            })
            .collect();
        let _ = tx.send(json!({"t":"admin-player-list","players":players}).to_string());
        return;
    }

    // ---- Shop buy ----
    if t == "shop-buy" {
        use crate::config::{
            PRICE_HIGHWAY, PRICE_RELOCATE, PRICE_DEFENDER,
            PRICE_BRUTE, PRICE_SHIELD, SHIELD_MS, DEFENDER_MS,
            HIGHWAY_LEN, HIGHWAY_NEAR,
        };

        let item = msg["item"].as_str().unwrap_or("").to_string();

        // Alliance is WIP — no charge
        if item == "alliance" {
            let _ = tx.send(json!({"t":"event","msg":"Alliances — coming soon!"}).to_string());
            return;
        }

        let credits = world.players.get(&pid).map(|p| p.credits).unwrap_or(0);
        let price: u64 = match item.as_str() {
            "highway"  => PRICE_HIGHWAY,
            "relocate" => PRICE_RELOCATE,
            "defender" => PRICE_DEFENDER,
            "brute"    => PRICE_BRUTE,
            "shield"   => PRICE_SHIELD,
            _ => { let _ = tx.send(err("Unknown item")); return; }
        };
        if credits < price { let _ = tx.send(err("Not enough credits")); return; }

        match item.as_str() {
            "highway" => {
                let queen_alive = world.queens.get(&pid).map(|q| !q.dead).unwrap_or(false);
                if !queen_alive { let _ = tx.send(err("Need a live queen")); return; }
                let x   = msg["x"].as_i64().unwrap_or(-1) as i32;
                let y   = msg["y"].as_i64().unwrap_or(-1) as i32;
                let vdx = msg["dx"].as_i64().unwrap_or(1) as i8;
                let vdy = msg["dy"].as_i64().unwrap_or(1) as i8;
                let ww = world.world_w as i32; let wh = world.world_h as i32;
                if x < 0 || y < 0 || x >= ww || y >= wh { let _ = tx.send(err("Out of bounds")); return; }
                if world.tiles.get(x as u32, y as u32) != 0 { let _ = tx.send(err("Start on a white tile")); return; }
                let hn = HIGHWAY_NEAR;
                let mut near_friendly = false;
                'near: for dy in -hn..=hn {
                    for dx in -hn..=hn {
                        let tx2 = x + dx; let ty2 = y + dy;
                        if tx2 < 0 || ty2 < 0 || tx2 >= ww || ty2 >= wh { continue; }
                        if world.tiles.get(tx2 as u32, ty2 as u32) == pid { near_friendly = true; break 'near; }
                    }
                }
                if !near_friendly { let _ = tx.send(err("Too far from your territory")); return; }
                let adx: i8 = if vdx >= 0 { 1 } else { -1 };
                let ady: i8 = if vdy >= 0 { 1 } else { -1 };
                if let Some(p) = world.players.get_mut(&pid) { p.credits -= price; }
                let c = cfg(); let lifespan = c.lifespan; drop(c);
                let mut cx = x; let mut cy = y; let mut painted = 0u32;
                for _ in 0..HIGHWAY_LEN {
                    if cx < 0 || cy < 0 || cx >= ww || cy >= wh { break; }
                    if world.tiles.get(cx as u32, cy as u32) == 0 {
                        world.tiles.set(cx as u32, cy as u32, pid);
                        painted += 1;
                    }
                    cx += adx as i32; cy += ady as i32;
                }
                let tip_x = (cx - adx as i32).clamp(0, ww - 1);
                let tip_y = (cy - ady as i32).clamp(0, wh - 1);
                world.ants.push(Ant::new(rand::random::<u32>(), pid, tip_x, tip_y, adx, ady, lifespan));
                world.dirty_tick = world.tick;
                let _ = tx.send(json!({"t":"shop-ok","item":"highway","painted":painted}).to_string());
            }
            "relocate" => {
                let old_pos = world.queens.get(&pid).filter(|q| !q.dead).map(|q| (q.x, q.y, q.size));
                let Some((ox, oy, sz)) = old_pos else { let _ = tx.send(err("Need a live queen")); return; };
                let x = msg["x"].as_i64().unwrap_or(-1) as i32;
                let y = msg["y"].as_i64().unwrap_or(-1) as i32;
                let ww = world.world_w as i32; let wh = world.world_h as i32;
                if x < 2 || y < 2 || x >= ww - 8 || y >= wh - 8 { let _ = tx.send(err("Out of bounds")); return; }
                if world.too_close_to_queen(x, y, pid) { let _ = tx.send(err("Too close to another queen")); return; }
                if let Some(p) = world.players.get_mut(&pid) { p.credits -= price; }
                world.clear_queen_body(ox, oy, sz, pid);
                if let Some(q) = world.queens.get_mut(&pid) { q.x = x; q.y = y; q.region = crate::regions::region_for(x, y); }
                world.paint_queen_body(x, y, sz, pid);
                world.queen_map_dirty = true; world.dirty_tick = world.tick;
                let _ = tx.send(json!({"t":"shop-ok","item":"relocate","x":x,"y":y}).to_string());
            }
            "defender" => {
                let queen_alive = world.queens.get(&pid).map(|q| !q.dead).unwrap_or(false);
                if !queen_alive { let _ = tx.send(err("Need a live queen")); return; }
                let expiry = current_ms() + DEFENDER_MS;
                if let Some(p) = world.players.get_mut(&pid) { p.credits -= price; p.defenders.push(expiry); }
                let _ = tx.send(json!({"t":"shop-ok","item":"defender"}).to_string());
            }
            "brute" => {
                let queen_data = world.queens.get(&pid).filter(|q| !q.dead)
                    .map(|q| (q.x, q.y, q.size, q.bubble_r));
                let Some((qx, qy, qs, bubble_r)) = queen_data else { let _ = tx.send(err("Need a live queen")); return; };
                let army = world.ant_counts.get(&pid).copied().unwrap_or(0) as i32;
                if army >= cfg().army_cap { let _ = tx.send(err("Army at capacity")); return; }
                let x   = msg["x"].as_i64().unwrap_or(-1) as i32;
                let y   = msg["y"].as_i64().unwrap_or(-1) as i32;
                let vdx = msg["dx"].as_i64().unwrap_or(0) as i8;
                let vdy = msg["dy"].as_i64().unwrap_or(-1) as i8;
                let (adx, ady) = match validate_worker_placement(world, pid, (x, y), (vdx, vdy), (qx, qy, qs, bubble_r)) {
                    Ok(d)  => d,
                    Err(e) => { let _ = tx.send(err(e)); return; }
                };
                let lifespan = cfg().lifespan;
                if let Some(p) = world.players.get_mut(&pid) { p.credits -= price; }
                world.ants.push(crate::world::Ant::new_kind(rand::random::<u32>(), pid, x, y, adx, ady, lifespan, 1));
                world.dirty_tick = world.tick;
                let _ = tx.send(json!({"t":"shop-ok","item":"brute","x":x,"y":y}).to_string());
            }
            "shield" => {
                let queen_alive = world.queens.get(&pid).map(|q| !q.dead).unwrap_or(false);
                if !queen_alive { let _ = tx.send(err("Need a live queen")); return; }
                let now = current_ms();
                // One shield at a time — block stacking until it depletes or expires.
                let active = world.queens.get(&pid)
                    .map(|q| q.shield > 0 && q.shield_expiry.is_some_and(|e| e > now))
                    .unwrap_or(false);
                if active { let _ = tx.send(err("Shield already active")); return; }
                if let Some(p) = world.players.get_mut(&pid) { p.credits -= price; }
                if let Some(q) = world.queens.get_mut(&pid) {
                    q.shield = q.max_hp;
                    q.shield_expiry = Some(now + SHIELD_MS);
                }
                let _ = tx.send(json!({"t":"shop-ok","item":"shield"}).to_string());
            }
            _ => {}
        }
        return;
    }

    // ---- Admin give credits ----
    if t == "admin-give-credits" {
        if !is_admin { let _ = tx.send(err("Admin only")); return; }
        let tid    = msg["targetId"].as_u64().unwrap_or(0) as u32;
        let amount = msg["amount"].as_u64().unwrap_or(0);
        if let Some(tp) = world.players.get_mut(&tid) {
            tp.credits = (tp.credits + amount).min(crate::config::CREDIT_CAP);
            let new_cr = tp.credits;
            if let Some(ttx) = &tp.tx {
                let _ = ttx.send(json!({"t":"event","msg":format!("+{amount} CREDITS (ADMIN) · total {new_cr}")}).to_string());
            }
        }
        return;
    }

    // ---- Admin set credits ----
    if t == "admin-set-credits" {
        if !is_admin { let _ = tx.send(err("Admin only")); return; }
        let tid    = msg["targetId"].as_u64().unwrap_or(0) as u32;
        let amount = msg["amount"].as_u64().unwrap_or(0).min(crate::config::CREDIT_CAP);
        if let Some(tp) = world.players.get_mut(&tid) {
            tp.credits = amount;
            if let Some(ttx) = &tp.tx {
                let _ = ttx.send(json!({"t":"event","msg":format!("CREDITS SET TO {amount} (ADMIN)")}).to_string());
            }
        }
        return;
    }

    // ---- Admin cfg reset ----
    if t == "admin-cfg-reset" {
        if !is_admin { let _ = tx.send(err("Admin only")); return; }
        let vals = reset_to_defaults();
        for (key, value) in vals {
            let cfg_msg = json!({"t":"cfg","key":key.to_uppercase(),"value":value}).to_string();
            world.broadcast(&cfg_msg);
        }
    }
}

/// Validate a worker placement at (`x`,`y`) heading (`vdx`,`vdy`) for `pid`, given their live
/// queen's footprint. Returns the resolved cardinal direction, or a client-facing error string.
/// Shared verbatim by place-ant and the shop brute (identical legality rules).
fn validate_worker_placement(
    world: &mut World, pid: u32, pos: (i32, i32), dir: (i8, i8), queen: (i32, i32, u8, f64),
) -> Result<(i8, i8), &'static str> {
    let (x, y) = pos;
    let (vdx, vdy) = dir;
    let (qx, qy, qs, bubble_r) = queen;
    let ww = world.world_w as i32;
    let wh = world.world_h as i32;
    if x < 0 || y < 0 || x >= ww || y >= wh { return Err("Out of bounds"); }
    world.get_queen_map();
    if world.queen_map.contains_key(&world.cell_key(x, y)) {
        return Err("Cannot place on a queen");
    }
    let dxq = (x - (qx + qs as i32 / 2)) as i64;
    let dyq = (y - (qy + qs as i32 / 2)) as i64;
    let in_bubble = ((dxq * dxq + dyq * dyq) as f64).sqrt() <= bubble_r;
    let tile = world.tiles.get(x as u32, y as u32);
    if in_bubble {
        if tile != 0 && tile != pid { return Err("Enemy tile inside bubble"); }
    } else if tile != pid {
        return Err("Place inside your bubble or on your territory");
    }
    const DIRS: [(i8, i8); 4] = [(0, -1), (1, 0), (0, 1), (-1, 0)];
    Ok(DIRS.iter().copied().find(|&(a, b)| a == vdx && b == vdy).unwrap_or((0, -1)))
}

fn create_or_reconnect_player(
    world: &mut World,
    id: u32, username: &str, color: &str, hue_idx: i32,
    _is_admin: bool,
    tx: UnboundedSender<String>,
) -> Option<String> {
    use crate::config::current_ms;
    let c = cfg();
    let daily = c.daily_ants;
    drop(c);
    let now = current_ms();

    // Metro list for the header region switcher (name + centre tile to fly to).
    let _ = tx.send(json!({"t":"regions","metros":crate::regions::metros_json()}).to_string());

    if world.players.contains_key(&id) {
        // Reconnect: reattach + bump conn_gen, and take the disconnect snapshot for welcome-back.
        let away = {
            let p = world.players.get_mut(&id).unwrap();
            p.conn_gen += 1;
            p.tx = Some(tx);
            p.away.take()
        };
        println!("[reconnect] {username} ({})", id);
        return away.and_then(|s| build_welcome_back(world, id, &s, now));
    }
    world.players.insert(id, Player {
        id, username: username.to_string(), color: color.to_string(),
        hue_idx,
        ants_avail: daily,
        next_refill: now + 24 * 3600 * 1000,
        queen_placed_at: None,
        npc: false, view: None,
        tx: Some(tx), view_tx: None,
        conn_gen: 1,
        prestige: 0, credits: 0,
        defenders: Vec::new(),
        visited_countries: Default::default(), visited_continents: Default::default(),
        lifetime_kills: 0, lifetime_peak_tiles: 0, queens_fielded: 0, away: None,
    });
    println!("[connect] {username} ({})", id);
    None
}

/// Build the welcome-back summary from a disconnect snapshot vs the player's current state.
/// Returns None for trivial gaps (<30 s) or players with no live queen at disconnect.
fn build_welcome_back(world: &World, id: u32, snap: &crate::world::AwaySnapshot, now: u64) -> Option<String> {
    let away_ms = now.saturating_sub(snap.at_ms);
    if !snap.queen_alive || away_ms < 30_000 { return None; }
    let q = world.queens.get(&id);
    let queen_died  = q.is_none_or(|q| q.dead);
    let cur_tiles   = q.map(|q| q.cached_tiles).unwrap_or(0);
    let cur_kills   = q.map(|q| q.kills).unwrap_or(0);
    let cur_level   = q.map(|q| q.level).unwrap_or(0);
    let cur_army    = world.ant_counts.get(&id).copied().unwrap_or(0);
    let cur_visited = world.players.get(&id).map(|p| p.visited_countries.len()).unwrap_or(0);
    Some(json!({
        "t":              "welcome-back",
        "awayMs":         away_ms,
        "queenDied":      queen_died,
        "tilesDelta":     cur_tiles as i64 - snap.tiles as i64,
        "killsDelta":     cur_kills as i64 - snap.kills as i64,
        "levelsDelta":    cur_level as i64 - snap.level as i64,
        "armyDelta":      cur_army as i64 - snap.army as i64,
        "countriesDelta": cur_visited as i64 - snap.visited_countries as i64,
    }).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world_with_queen() -> (World, u32) {
        let mut w = World::new();
        let pid = 100u32;
        w.queens.insert(pid, Queen { x: 1000, y: 1000, size: 2, hp: 100, max_hp: 100,
            level: 1, xp: 0.0, kills: 0, bubble_r: 30.0, last_attacker: None, dead: false,
            tiles_ever_held: 0, cached_tiles: 0, npc: false, shield: 0, shield_expiry: None,
            region: String::new() });
        w.queen_map_dirty = true;
        (w, pid)
    }

    #[test]
    fn worker_placement_rules() {
        let (mut w, pid) = world_with_queen();
        let q = (1000, 1000, 2u8, 30.0);
        // Valid: inside the bubble, empty tile, default heading.
        assert_eq!(validate_worker_placement(&mut w, pid, (1005, 1005), (0, -1), q), Ok((0, -1)));
        // Out of bounds.
        assert!(validate_worker_placement(&mut w, pid, (-1, 5), (0, -1), q).is_err());
        // On the queen's own cell.
        assert_eq!(validate_worker_placement(&mut w, pid, (1000, 1000), (0, -1), q), Err("Cannot place on a queen"));
        // Far outside the bubble and not on owned territory.
        assert_eq!(validate_worker_placement(&mut w, pid, (5000, 5000), (0, -1), q), Err("Place inside your bubble or on your territory"));
        // Enemy tile inside the bubble.
        w.tiles.set(1005, 1005, 999);
        assert_eq!(validate_worker_placement(&mut w, pid, (1005, 1005), (0, -1), q), Err("Enemy tile inside bubble"));
        // An unknown heading falls back to up.
        assert_eq!(validate_worker_placement(&mut w, pid, (1006, 1004), (9, 9), q), Ok((0, -1)));
    }
}
