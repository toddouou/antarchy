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
        create_or_reconnect_player(world, id, &raw_u, &color, hue_idx, false, tx.clone());
        *player_id = Some(id);
        let me = build_player_info(world, id);
        let lb = build_leaderboard(world);
        let _ = tx.send(json!({"t":"logged-in","me":serde_json::from_str::<Value>(&me).unwrap_or(Value::Null)}).to_string());
        let _ = tx.send(lb);
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
        create_or_reconnect_player(world, rec.id, &rec.username, &rec.color, rec.hue_idx, rec.is_admin, tx.clone());
        *player_id = Some(rec.id);
        let me = build_player_info(world, rec.id);
        let lb = build_leaderboard(world);
        let _ = tx.send(json!({"t":"logged-in","me":serde_json::from_str::<Value>(&me).unwrap_or(Value::Null)}).to_string());
        let _ = tx.send(lb);
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
        if let Some(u) = world.auth.users.get_mut(&ADMIN_USERNAME.to_string()) {
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
        if world.queens.contains_key(&pid) {
            let _ = tx.send(err("Already have a queen")); return;
        }
        let x = msg["x"].as_i64().unwrap_or(-1) as i32;
        let y = msg["y"].as_i64().unwrap_or(-1) as i32;
        let c = cfg();
        let ww = world.world_w as i32; let wh = world.world_h as i32;
        if x < 2 || y < 2 || x >= ww - 8 || y >= wh - 8 {
            let _ = tx.send(err("Out of bounds")); return;
        }
        for (_, q) in world.queens.iter().filter(|(_, q)| !q.dead) {
            let ddx = q.x + q.size as i32 / 2 - x;
            let ddy = q.y + q.size as i32 / 2 - y;
            let min_dist = q.bubble_r;
            if ((ddx*ddx + ddy*ddy) as f64).sqrt() < min_dist {
                let _ = tx.send(err("Too close to another queen")); return;
            }
        }
        let size = queen_size_for_level(1);
        let max_hp = c.hp_base;
        let bubble_r = c.bubble_r;
        drop(c);
        if let Some(p) = world.players.get_mut(&pid) {
            p.queen_placed_at = Some(current_ms());
        }
        world.queens.insert(pid, Queen {
            x, y, size, hp: max_hp, max_hp, level: 1, xp: 0.0, kills: 0,
            bubble_r, last_attacker: None, dead: false,
            tiles_ever_held: 0, cached_tiles: 0, npc: false,
        });
        world.queen_map_dirty = true;
        for dy in 0..size as i32 {
            for dx in 0..size as i32 {
                world.tiles.set((x + dx) as u32, (y + dy) as u32, pid);
            }
        }
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
        let queen_data = world.queens.get(&pid)
            .filter(|q| !q.dead)
            .map(|q| (q.x, q.y, q.size, q.bubble_r));
        let Some((qx, qy, qs, bubble_r)) = queen_data else {
            let _ = tx.send(err("Place queen first")); return;
        };
        // p and q borrows are dead here — safe to call get_queen_map (&mut World)
        let x = msg["x"].as_i64().unwrap_or(-1) as i32;
        let y = msg["y"].as_i64().unwrap_or(-1) as i32;
        let ww = world.world_w as i32; let wh = world.world_h as i32;
        if x < 0 || y < 0 || x >= ww || y >= wh { let _ = tx.send(err("Out of bounds")); return; }
        world.get_queen_map();
        let ck = world.cell_key(x, y);
        if world.queen_map.contains_key(&ck) { let _ = tx.send(err("Cannot place on a queen")); return; }
        let dxq = x - (qx + qs as i32 / 2); let dyq = y - (qy + qs as i32 / 2);
        let dist_q = ((dxq*dxq + dyq*dyq) as f64).sqrt();
        let in_bubble = dist_q <= bubble_r;
        let tile = world.tiles.get(x as u32, y as u32);
        let on_friendly = tile == pid;
        let is_enemy = tile != 0 && tile != pid;
        if in_bubble {
            if is_enemy { let _ = tx.send(err("Enemy tile inside bubble")); return; }
        } else {
            if !on_friendly { let _ = tx.send(err("Place inside your bubble or on your territory")); return; }
        }
        let vdx = msg["dx"].as_i64().unwrap_or(0) as i8;
        let vdy = msg["dy"].as_i64().unwrap_or(-1) as i8;
        const DIRS: [(i8,i8); 4] = [(0,-1),(1,0),(0,1),(-1,0)];
        let (adx, ady) = DIRS.iter().copied()
            .find(|&(a,b)| a == vdx && b == vdy)
            .unwrap_or((0,-1));
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
                    ql.level = new_lvl;
                    ql.size  = queen_size_for_level(new_lvl);
                    ql.max_hp = c.hp_base * new_lvl as i32;
                    ql.hp    = ql.max_hp.min(cur_hp + c.hp_base);
                }
                if ants_gained > 0 {
                    if let Some(tp) = world.players.get_mut(&target_id) {
                        tp.ants_avail += ants_gained;
                    }
                }
                world.queen_map_dirty = true;
                let ttx = world.players.get(&target_id).and_then(|p| p.tx.clone());
                if let Some(ttx) = ttx {
                    let _ = ttx.send(json!({"t":"event","msg":"[ADMIN] LEVEL UP"}).to_string());
                }
            }
            "level-down" => {
                let c = cfg().clone();
                let queen_state = world.queens.get(&target_id)
                    .filter(|q| !q.dead && q.level > 1)
                    .map(|q| (q.level, q.xp, q.hp));
                let Some((cur_level, cur_xp, cur_hp)) = queen_state else { return };
                let new_level = cur_level - 1;
                let new_max_hp = c.hp_base * new_level as i32;
                let floor = total_xp_for_level(new_level, &c);
                let ceil  = total_xp_for_level(new_level + 1, &c) - 1.0;
                if let Some(qd) = world.queens.get_mut(&target_id) {
                    qd.level  = new_level;
                    qd.size   = queen_size_for_level(new_level);
                    qd.max_hp = new_max_hp;
                    qd.hp     = cur_hp.min(new_max_hp);
                    qd.xp     = cur_xp.clamp(floor, ceil);
                }
                world.queen_map_dirty = true;
                let ttx = world.players.get(&target_id).and_then(|p| p.tx.clone());
                if let Some(ttx) = ttx {
                    let _ = ttx.send(json!({"t":"event","msg":"[ADMIN] LEVEL DOWN"}).to_string());
                }
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
                // Clear old body tiles (no queen borrow active)
                for dy in 0..sz as i32 {
                    for dx in 0..sz as i32 {
                        if world.tiles.get((ox+dx) as u32, (oy+dy) as u32) == tid {
                            world.tiles.set((ox+dx) as u32, (oy+dy) as u32, 0);
                        }
                    }
                }
                // Update queen position
                if let Some(q) = world.queens.get_mut(&tid) { q.x = nx; q.y = ny; }
                // Paint new body tiles
                for dy in 0..sz as i32 {
                    for dx in 0..sz as i32 {
                        world.tiles.set((nx+dx) as u32, (ny+dy) as u32, tid);
                    }
                }
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
                q.level   = level;
                q.xp      = total_xp_for_level(level, &c);
                q.size     = queen_size_for_level(level);
                q.max_hp  = c.hp_base * level as i32;
                q.hp      = q.hp.min(q.max_hp);
                let ants_delta = (level as i32 - old_lvl as i32).max(0) * c.levelup_ant_grant;
                if ants_delta > 0 {
                    if let Some(tp) = world.players.get_mut(&tid) { tp.ants_avail += ants_delta; }
                }
                world.queen_map_dirty = true;
            }
        }
        let ttx = world.players.get(&tid).and_then(|p| p.tx.clone());
        if let Some(ttx) = ttx {
            let _ = ttx.send(json!({"t":"event","msg":format!("[ADMIN] LEVEL SET TO {level}")}).to_string());
        }
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
                    q.level   = new_lvl;
                    q.size     = queen_size_for_level(new_lvl);
                    q.max_hp  = c.hp_base * new_lvl as i32;
                    q.hp      = q.max_hp;
                    world.queen_map_dirty = true;
                    if ants_delta > 0 {
                        if let Some(tp) = world.players.get_mut(&tid) { tp.ants_avail += ants_delta; }
                    }
                    let ttx = world.players.get(&tid).and_then(|p| p.tx.clone());
                    if let Some(ttx) = ttx {
                        let _ = ttx.send(json!({"t":"level-up","level":new_lvl}).to_string());
                    }
                }
            }
        }
        let ttx = world.players.get(&tid).and_then(|p| p.tx.clone());
        if let Some(ttx) = ttx {
            let _ = ttx.send(json!({"t":"event","msg":format!("+{xp} XP (ADMIN)")}).to_string());
        }
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

    // ---- Admin spawn N NPCs ----
    if t == "admin-spawn-n" {
        if !is_admin { let _ = tx.send(err("Admin only")); return; }
        let count = msg["count"].as_u64().unwrap_or(1).min(50) as u32;
        for _ in 0..count {
            spawn_npc(world, pid, None, None);
        }
        let ev = json!({"t":"event","msg":format!("[ADMIN] SPAWNED {count} NPCs")}).to_string();
        world.broadcast(&ev);
        return;
    }

    // ---- Admin player list ----
    if t == "admin-player-list" {
        if !is_admin { let _ = tx.send(err("Admin only")); return; }
        let players: Vec<serde_json::Value> = world.players.iter()
            .filter(|(_, p)| !p.npc)
            .map(|(id, p)| {
                let q = world.queens.get(id);
                json!({
                    "id":       id,
                    "username": p.username,
                    "online":   p.tx.is_some(),
                    "level":    q.map(|q| q.level).unwrap_or(0),
                    "tiles":    q.map(|q| q.cached_tiles).unwrap_or(0),
                    "hp":       q.map(|q| q.hp).unwrap_or(0),
                    "maxHp":    q.map(|q| q.max_hp).unwrap_or(0),
                    "ants":     p.ants_avail,
                })
            })
            .collect();
        let _ = tx.send(json!({"t":"admin-player-list","players":players}).to_string());
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
        return;
    }
}

fn create_or_reconnect_player(
    world: &mut World,
    id: u32, username: &str, color: &str, hue_idx: i32,
    _is_admin: bool,
    tx: UnboundedSender<String>,
) {
    use crate::config::current_ms;
    let c = cfg();
    let daily = c.daily_ants;
    drop(c);
    let now = current_ms();

    if let Some(p) = world.players.get_mut(&id) {
        p.tx = Some(tx);
        println!("[reconnect] {username} ({})", id);
    } else {
        world.players.insert(id, Player {
            id, username: username.to_string(), color: color.to_string(),
            hue_idx,
            ants_avail: daily,
            next_refill: now + 24 * 3600 * 1000,
            queen_placed_at: None,
            npc: false, view: None,
            tx: Some(tx),
        });
        println!("[connect] {username} ({})", id);
    }
}
