use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use serde_json::{json, Value};

use crate::config::{cfg, total_xp_for_level, calc_score, current_ms};
use crate::fog::{compute_fog_field_slice, PAD};
use crate::world::World;

const MAX_DIM: i32 = 800;

pub fn get_palette(world: &World) -> Value {
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
            let score = calc_score(q.cached_tiles, p.queen_placed_at, q.kills);
            Some(json!({
                "id":       id,
                "name":     p.username,
                "color":    p.color,
                "tiles":    q.cached_tiles,
                "level":    q.level,
                "kills":    q.kills,
                "prestige": p.prestige,
                "region":   q.region,
                "score":    score as i64,
                "npc":      p.npc,
            }))
        })
        .collect();
    // Rank by Grand Score (tiles + time alive + kills), not tiles alone.
    entries.sort_by(|a, b| {
        let sa = a["score"].as_i64().unwrap_or(0);
        let sb = b["score"].as_i64().unwrap_or(0);
        sb.cmp(&sa)
    });
    json!({"t": "leaderboard", "entries": entries}).to_string()
}

/// World-wide server stats pushed to every connected client (~1 Hz). Replaces per-client
/// `/health` polling — built once, broadcast to all, so cost is O(1) not O(players) HTTP
/// hits against the world lock. Drives the header bar + admin status cards.
pub fn build_server_stats(world: &World) -> String {
    let online = world.players.values().filter(|p| !p.npc && p.tx.is_some()).count();
    let queens = world.queens.values().filter(|q| !q.dead).count();
    json!({
        "t":       "server-stats",
        "online":  online,
        "ants":    world.ants.len(),
        "queens":  queens,
        "tick":    world.tick,
        "tps":     cfg().tick_rate,
        "uptimeMs": current_ms() - world.started_at,
    }).to_string()
}

/// Metro king-of-the-hill holders for the client (region switcher + leaderboard region tabs).
/// Text only — no map borders. Broadcast on the holder-recompute cadence (simulation.rs).
pub fn build_region_holders(world: &World) -> String {
    let holders: Vec<Value> = world.metro_holders.iter().map(|h| {
        let (name, color) = match h.owner.and_then(|id| world.players.get(&id)) {
            Some(p) => (Some(p.username.clone()), Some(p.color.clone())),
            None    => (None, None),
        };
        json!({
            "name":     h.name,
            "holderId": h.owner,
            "holder":   name,
            "color":    color,
            "tiles":    h.tiles,
        })
    }).collect();
    json!({"t": "region-holders", "holders": holders}).to_string()
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

    // Player's own live workers for the WORKERS active-list (capped at 120; each entry is
    // [id, remaining_lifespan_ticks, kind]). Early-out once we have enough to bound the
    // per-player scan a little. Client renders lifespan bars from this + cfg LIFESPAN.
    let mut my_ants: Vec<Value> = Vec::new();
    for a in world.ants.iter() {
        if a.owner != player_id { continue; }
        my_ants.push(json!([a.id, a.lifespan.saturating_sub(a.age), a.kind]));
        if my_ants.len() >= 120 { break; }
    }

    let mut visited_countries: Vec<&String> = p.visited_countries.iter().collect();
    visited_countries.sort();
    let mut visited_continents: Vec<&String> = p.visited_continents.iter().collect();
    visited_continents.sort();

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
        "prestige":  p.prestige,
        "credits":   p.credits,
        "region":    q.map(|q| q.region.clone()).unwrap_or_default(),
        "defenders": p.defenders.len(),
        "visitedCountries":  visited_countries,
        "visitedContinents": visited_continents,
        "lifetimeKills":     p.lifetime_kills,
        "lifetimePeakTiles": p.lifetime_peak_tiles,
        "queensFielded":     p.queens_fielded,
        "shield":    q.map(|q| q.shield).unwrap_or(0),
        "stats": { "tiles": tiles, "secs": secs, "kills": q.map(|q|q.kills).unwrap_or(0), "score": score as i64 },
        "army": world.ant_counts.get(&player_id).copied().unwrap_or(0),
        "ants": my_ants,
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
            "LIFESPAN":   c.lifespan,
            "ARMY_CAP":   c.army_cap,
        }
    }).to_string()
}

/// One visible queen, snapshotted with the strings it needs so `finish_view` can run
/// without touching the World.
struct QueenLite {
    qid: u32, x: i32, y: i32, size: u8, hp: i32, max_hp: i32, level: u16,
    color: String, username: String, prestige: u32, shield: i32,
    bubble_r: f64,
    /// Always visible (own queen / admin) → no fog gate, and `bubbleR` is included.
    reveal: bool,
}

/// A per-client viewport snapshot taken under the World read lock. Everything needed to
/// serialize the frame lives here as owned data, so `finish_view` (fog transform + base64
/// + JSON) can run on another thread with no lock held. See `snapshot_view`/`finish_view`.
pub struct RawView {
    x0: i32, y0: i32, w: usize, h: usize,
    tick: u64,
    include_tiles: bool,
    player_id: u32,
    is_admin: bool,
    /// Padded (pw×ph) ownership slice for tile blob + fog; empty on ants-only frames.
    pad: usize, pw: usize, ph: usize,
    owners: Vec<u32>,
    /// In-rect ants (id,x,y,dx,dy,owner,kind); fog visibility applied in `finish_view`.
    ants: Vec<(u32, i32, i32, i8, i8, u32, u8)>,
    queens: Vec<QueenLite>,
    /// LOD step: tiles per served grid cell. 1 = normal 1:1; >1 = zoomed-out overview where
    /// each `owners` cell samples one tile every `lod_step` tiles (the territory pyramid).
    lod_step: i32,
}

/// Phase A (under the World read lock): extract the minimal owned data for one client's
/// viewport. Cheap relative to fog/base64/JSON — those happen in `finish_view`, unlocked.
pub fn snapshot_view(world: &World, player_id: u32, include_tiles: bool) -> Option<RawView> {
    let p  = world.players.get(&player_id)?;
    let v  = p.view.as_ref()?;
    let ww = world.world_w as i32;
    let wh = world.world_h as i32;

    let x0 = v.x0.max(0);
    let y0 = v.y0.max(0);
    let fw = (v.x1.min(ww) - x0).max(0);   // full requested tile span (clamped to world)
    let fh = (v.y1.min(wh) - y0).max(0);
    if fw == 0 || fh == 0 { return None; }

    // LOD step: when the requested span exceeds MAX_DIM, downsample so the served grid stays
    // ≤ MAX_DIM cells/axis (each cell = `step` tiles). step == 1 is the normal 1:1 path.
    let step = ((fw.max(fh) as usize).div_ceil(MAX_DIM as usize).max(1)) as i32;
    let lod  = step > 1;

    // Zoomed-out ants-only frames carry nothing (ants are sub-pixel) — skip them entirely.
    if lod && !include_tiles { return None; }

    let w  = (((fw + step - 1) / step) as usize).clamp(1, MAX_DIM as usize);
    let h  = (((fh + step - 1) / step) as usize).clamp(1, MAX_DIM as usize);
    let x1 = x0 + (w as i32) * step;
    let y1 = y0 + (h as i32) * step;

    let is_admin = world.auth.is_admin_id(player_id);

    // In-rect ants (fog filtering deferred to finish_view). None while zoomed out (LOD).
    let ants: Vec<(u32, i32, i32, i8, i8, u32, u8)> = if lod {
        Vec::new()
    } else {
        world.ants.iter()
            .filter(|a| a.x >= x0 && a.x < x1 && a.y >= y0 && a.y < y1)
            .map(|a| (a.id, a.x, a.y, a.dx, a.dy, a.owner, a.kind))
            .collect()
    };

    // Tiles + fog ownership slice + queens only matter on tile frames. At LOD, each grid cell
    // samples the tile `step` apart (nearest-sample territory pyramid); fog runs on the grid.
    let (pad, pw, ph, owners, queens) = if include_tiles {
        let pad = PAD as usize;
        let pw = w + 2 * pad;
        let ph = h + 2 * pad;
        let mut owners = vec![0u32; pw * ph];
        for py in 0..ph {
            let wy = y0 + (py as i32 - pad as i32) * step;
            if wy < 0 || wy >= wh { continue; }
            let row = py * pw;
            for px in 0..pw {
                let wx = x0 + (px as i32 - pad as i32) * step;
                if wx < 0 || wx >= ww { continue; }
                owners[row + px] = world.tiles.get(wx as u32, wy as u32);
            }
        }
        let queens: Vec<QueenLite> = world.queens.iter()
            .filter(|(_, q)| !q.dead)
            .filter(|(_, q)| !((q.x + q.size as i32) < x0 || q.x > x1 || (q.y + q.size as i32) < y0 || q.y > y1))
            .map(|(&qid, q)| {
                let qp = world.players.get(&qid);
                QueenLite {
                    qid, x: q.x, y: q.y, size: q.size, hp: q.hp, max_hp: q.max_hp, level: q.level,
                    color:    qp.map(|p| p.color.clone()).unwrap_or_else(|| "#888".into()),
                    username: qp.map(|p| p.username.clone()).unwrap_or_else(|| "???".into()),
                    prestige: qp.map(|p| p.prestige).unwrap_or(0),
                    shield: q.shield,
                    bubble_r: q.bubble_r,
                    reveal: qid == player_id || is_admin,
                }
            })
            .collect();
        (pad, pw, ph, owners, queens)
    } else {
        (0, 0, 0, Vec::new(), Vec::new())
    };

    Some(RawView {
        x0, y0, w, h, tick: world.tick, include_tiles, player_id, is_admin,
        pad, pw, ph, owners, ants, queens, lod_step: step,
    })
}

/// Phase B (no lock held): turn a `RawView` into the JSON frame — fog distance transform
/// on the local slice, fog-gated ant/queen visibility, base64, and JSON assembly.
pub fn finish_view(raw: &RawView, palette: &Value) -> String {
    if !raw.include_tiles {
        // Ants-only frame: client keeps its cached fog for visibility decisions.
        let ants: Vec<Value> = raw.ants.iter()
            .map(|&(id, x, y, dx, dy, owner, kind)| json!([id, x, y, dx, dy, owner, kind]))
            .collect();
        return json!({
            "t": "view",
            "x0": raw.x0, "y0": raw.y0, "w": raw.w, "h": raw.h,
            "ants": ants,
            "tick": raw.tick,
        }).to_string();
    }

    // Fog (admins see everything).
    let fog: Vec<u8> = if raw.is_admin {
        vec![0u8; raw.w * raw.h]
    } else {
        compute_fog_field_slice(&raw.owners, raw.pw, raw.ph, raw.pad, raw.w, raw.h, raw.player_id)
    };
    let fog_b64 = B64.encode(&fog);

    // Tile blob: inner w×h of the padded slice, little-endian u16 → base64.
    let mut tile_bytes: Vec<u8> = Vec::with_capacity(raw.w * raw.h * 2);
    for yi in 0..raw.h {
        let row = (yi + raw.pad) * raw.pw + raw.pad;
        for xi in 0..raw.w {
            let v = raw.owners[row + xi].min(0xFFFF) as u16;
            tile_bytes.extend_from_slice(&v.to_le_bytes());
        }
    }
    let tiles_b64 = B64.encode(&tile_bytes);

    // Visible ants (own always; others only where fog is not full).
    let ants: Vec<Value> = raw.ants.iter()
        .filter(|&&(_, x, y, _, _, owner, _)| {
            if owner == raw.player_id { return true; }
            let fi = (y - raw.y0) as usize * raw.w + (x - raw.x0) as usize;
            fog.get(fi).copied().unwrap_or(100) < 100
        })
        .map(|&(id, x, y, dx, dy, owner, kind)| json!([id, x, y, dx, dy, owner, kind]))
        .collect();

    // Visible queens.
    let queens: Vec<Value> = raw.queens.iter()
        .filter_map(|q| {
            if !q.reveal {
                // Map world → served grid cell (÷ lod_step) before sampling fog.
                let gx = (((q.x - raw.x0) / raw.lod_step).max(0) as usize).min(raw.w.saturating_sub(1));
                let gy = (((q.y - raw.y0) / raw.lod_step).max(0) as usize).min(raw.h.saturating_sub(1));
                let qi = gy * raw.w + gx;
                if fog.get(qi).copied().unwrap_or(100) >= 100 { return None; }
            }
            let mut obj = json!({
                "id": q.qid, "x": q.x, "y": q.y, "size": q.size,
                "hp": q.hp, "maxHp": q.max_hp, "level": q.level,
                "color": q.color, "username": q.username, "prestige": q.prestige,
                "shield": q.shield,
            });
            if q.reveal { obj["bubbleR"] = json!(q.bubble_r); }
            Some(obj)
        })
        .collect();

    json!({
        "t": "view",
        "x0": raw.x0, "y0": raw.y0, "w": raw.w, "h": raw.h,
        "lod": raw.lod_step,
        "tiles": tiles_b64,
        "fog":   fog_b64,
        "ants":  ants,
        "queens": queens,
        "tick":  raw.tick,
        "palette": palette.clone(),
    }).to_string()
}
