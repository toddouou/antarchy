use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use serde_json::{json, Value};

use crate::config::{cfg, total_xp_for_level, calc_score, current_ms};
use crate::fog::compute_fog_field;
use crate::world::World;

const MAX_DIM: i32 = 800;

fn get_palette(world: &World) -> Value {
    let mut p = serde_json::Map::new();
    p.insert("0".to_string(), json!("#ffffff"));
    for (id, pl) in &world.players {
        p.insert(id.to_string(), json!(pl.color));
    }
    Value::Object(p)
}

pub fn build_leaderboard(world: &World) -> String {
    let mut entries: Vec<Value> = world.queens.iter()
        .filter(|(_, q)| !q.dead)
        .filter_map(|(&id, q)| {
            let p = world.players.get(&id)?;
            Some(json!({
                "id":       id,
                "name":     p.username,
                "color":    p.color,
                "tiles":    q.cached_tiles,
                "level":    q.level,
                "kills":    q.kills,
                "prestige": p.prestige,
            }))
        })
        .collect();
    entries.sort_by(|a, b| {
        let ta = a["tiles"].as_u64().unwrap_or(0);
        let tb = b["tiles"].as_u64().unwrap_or(0);
        tb.cmp(&ta)
    });
    json!({"t": "leaderboard", "entries": entries}).to_string()
}

pub fn build_player_info(world: &World, player_id: u32) -> String {
    let c = cfg().clone();
    let Some(p) = world.players.get(&player_id) else {
        return json!({"t":"err","msg":"player not found"}).to_string();
    };
    let q = world.queens.get(&player_id);
    let tiles   = q.map(|q| q.cached_tiles).unwrap_or(0);
    let secs    = p.queen_placed_at
        .map(|t| (current_ms() - t) / 1000)
        .unwrap_or(0);
    let score   = calc_score(tiles, p.queen_placed_at, q.map(|q| q.kills).unwrap_or(0));
    let xp      = q.map(|q| q.xp).unwrap_or(0.0);
    let xp_this = q.map(|q| total_xp_for_level(q.level, &c)).unwrap_or(0.0);
    let xp_next = q.map(|q| total_xp_for_level(q.level + 1, &c)).unwrap_or(c.xp_base);

    let is_admin = world.auth.is_admin_id(player_id);
    let color_chosen = if is_admin {
        world.auth.users.get(&p.username).map(|u| u.color_chosen).unwrap_or(false)
    } else {
        true
    };

    let queen_val: Value = if let Some(q) = q {
        json!({
            "x": q.x, "y": q.y, "size": q.size,
            "hp": q.hp, "maxHp": q.max_hp,
            "level": q.level, "dead": q.dead,
            "tiles": tiles,
            "tilesEverHeld": q.tiles_ever_held,
            "milestones": q.tiles_ever_held / c.xp_tile_milestone.max(1),
            "xp": xp as i64,
            "xpThisLevel": xp_this as i64,
            "xpNextLevel": xp_next as i64,
            "xpIntoLevel": (xp - xp_this) as i64,
            "xpLevelSpan":  (xp_next - xp_this) as i64,
            "bubbleR": q.bubble_r,
        })
    } else {
        Value::Null
    };

    json!({
        "t": "me",
        "id": player_id,
        "username": p.username,
        "color": p.color,
        "isAdmin": is_admin,
        "colorChosen": color_chosen,
        "antsAvail": p.ants_avail,
        "nextRefillMs": (p.next_refill as i64 - current_ms() as i64).max(0),
        "queen": queen_val,
        "prestige": p.prestige,
        "credits":  p.credits,
        "stats": { "tiles": tiles, "secs": secs, "kills": q.map(|q|q.kills).unwrap_or(0), "score": score as i64 },
        "army": world.ant_counts.get(&player_id).copied().unwrap_or(0),
        "tick": world.tick,
        "tickRate": c.tick_rate,
        "worldW": world.world_w,
        "worldH": world.world_h,
        "spawnX": c.spawn_x,
        "spawnY": c.spawn_y,
        "spawnPan": c.spawn_pan,
        "geo": {
            "capitolLat": c.capitol_lat,
            "capitolLon": c.capitol_lon,
            "tileMeters":  c.tile_meters,
        },
        "cfg": {
            "BUBBLE_R":   c.bubble_r,
            "DAILY_ANTS": c.daily_ants,
            "LEVEL_CAP":  c.xp_level_cap,
        }
    }).to_string()
}

pub fn build_view_update(world: &World, player_id: u32, include_tiles: bool) -> Option<String> {
    let p  = world.players.get(&player_id)?;
    let v  = p.view.as_ref()?;
    let ww = world.world_w as i32;
    let wh = world.world_h as i32;

    let x0 = v.x0.max(0);
    let y0 = v.y0.max(0);
    let w  = (v.x1.min(ww) - x0).min(MAX_DIM).max(0) as usize;
    let h  = (v.y1.min(wh) - y0).min(MAX_DIM).max(0) as usize;
    let x1 = x0 + w as i32;
    let y1 = y0 + h as i32;
    if w == 0 || h == 0 { return None; }

    if !include_tiles {
        // Ants-only frame: skip tile/fog computation.
        // Client retains its cached fog for visibility decisions.
        let ants: Vec<Value> = world.ants.iter()
            .filter(|a| a.x >= x0 && a.x < x1 && a.y >= y0 && a.y < y1)
            .map(|a| json!([a.id, a.x, a.y, a.dx, a.dy, a.owner]))
            .collect();
        return Some(json!({
            "t": "view",
            "x0": x0, "y0": y0, "w": w, "h": h,
            "ants": ants,
            "tick": world.tick,
        }).to_string());
    }

    // Build Uint16 tile snapshot
    let mut tile_u16 = vec![0u16; w * h];
    for yi in 0..h {
        for xi in 0..w {
            let pid = world.tiles.get((x0 as usize + xi) as u32, (y0 as usize + yi) as u32);
            tile_u16[yi * w + xi] = pid.min(0xFFFF) as u16;
        }
    }
    // Encode as little-endian bytes → base64
    let tile_bytes: Vec<u8> = tile_u16.iter()
        .flat_map(|&v| v.to_le_bytes())
        .collect();
    let tiles_b64 = B64.encode(&tile_bytes);

    let fog = compute_fog_field(world, player_id, x0, y0, w, h);
    let fog_b64 = B64.encode(&fog);

    // Collect visible ants
    let ants: Vec<Value> = world.ants.iter()
        .filter(|a| a.x >= x0 && a.x < x1 && a.y >= y0 && a.y < y1)
        .filter(|a| {
            if a.owner == player_id { return true; }
            let fi = (a.y - y0) as usize * w + (a.x - x0) as usize;
            fog.get(fi).copied().unwrap_or(100) < 100
        })
        .map(|a| json!([a.id, a.x, a.y, a.dx, a.dy, a.owner]))
        .collect();

    // Collect visible queens
    let is_admin = world.auth.is_admin_id(player_id);
    let queens: Vec<Value> = world.queens.iter()
        .filter(|(_, q)| !q.dead)
        .filter(|(_, q)| !((q.x + q.size as i32) < x0 || q.x > x1 || (q.y + q.size as i32) < y0 || q.y > y1))
        .filter_map(|(&qid, q)| {
            if qid != player_id && !is_admin {
                let qi = ((q.y - y0).max(0) as usize) * w + ((q.x - x0).max(0) as usize);
                if fog.get(qi).copied().unwrap_or(100) >= 100 { return None; }
            }
            let qp = world.players.get(&qid);
            let mut obj = json!({
                "id": qid, "x": q.x, "y": q.y, "size": q.size,
                "hp": q.hp, "maxHp": q.max_hp, "level": q.level,
                "color":    qp.map(|p| p.color.as_str()).unwrap_or("#888"),
                "username": qp.map(|p| p.username.as_str()).unwrap_or("???"),
                "prestige": qp.map(|p| p.prestige).unwrap_or(0),
            });
            if qid == player_id || is_admin {
                obj["bubbleR"] = json!(q.bubble_r);
            }
            Some(obj)
        })
        .collect();

    Some(json!({
        "t": "view",
        "x0": x0, "y0": y0, "w": w, "h": h,
        "tiles": tiles_b64,
        "fog":   fog_b64,
        "ants":  ants,
        "queens": queens,
        "tick":  world.tick,
        "palette": get_palette(world),
    }).to_string())
}
