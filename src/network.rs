use std::io::Write;
use flate2::{write::DeflateEncoder, Compression};
use rustc_hash::FxHashMap;
use serde_json::{json, Value};

use crate::config::{cfg, total_xp_for_level, calc_score, current_ms};
use crate::fog::{compute_fog_field_slice, PAD};
use crate::world::World;

const MAX_DIM: i32 = 800;
/// Leaderboard broadcast is capped to the top N by Grand Score (rest is future-proofing headroom).
const LEADERBOARD_TOP_N: usize = 100;

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
    // Top-N broadcast only — clients show their own standing from the `me` frame if off-list.
    entries.truncate(LEADERBOARD_TOP_N);
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

/// Build the `me` payload. `full=true` (sent once in `logged-in`) includes the **static** block
/// (`tickRate`, world size, spawn, geo projection, cfg constants); `full=false` (the ≥1 Hz periodic
/// push) omits it — the client retains the static fields from `logged-in` and merges the dynamic
/// ones. Splitting this is the Phase-3 `me` trim (egress: the static block was ~half the payload).
pub fn build_player_info(world: &World, player_id: u32, full: bool) -> String {
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

    // Dynamic fields — sent on every periodic `me` (≥1 Hz).
    let mut info = json!({
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
    });

    // Static block — only in `logged-in` (full); the client retains + merges it across periodic `me`.
    if full {
        info["tickRate"] = json!(c.tick_rate);
        info["worldW"]   = json!(world.world_w);
        info["worldH"]   = json!(world.world_h);
        info["spawnX"]   = json!(c.spawn_x);
        info["spawnY"]   = json!(c.spawn_y);
        info["spawnPan"] = json!(c.spawn_pan);
        info["geo"] = json!({
            "capitolLat": c.capitol_lat,
            "capitolLon": c.capitol_lon,
            "tileMeters":  c.tile_meters,
        });
        info["cfg"] = json!({
            "BUBBLE_R":   c.bubble_r,
            "DAILY_ANTS": c.daily_ants,
            "LEVEL_CAP":  c.xp_level_cap,
            "LIFESPAN":   c.lifespan,
            "ARMY_CAP":   c.army_cap,
        });
    }
    info.to_string()
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

/// Visible-ant cap (Phase 3B): subsample `ants` in place to ≈`cap` by a **stable id-stride**, so an
/// ant's keep/drop status holds frame-to-frame (no flicker) while the kept subset stays spatially
/// uniform. `cap == 0` or `len <= cap` ⇒ unchanged. Bounds worst-case dense-battle egress.
fn cap_ants_by_id(ants: &mut Vec<(u32, i32, i32, i8, i8, u32, u8)>, cap: usize) {
    if cap > 0 && ants.len() > cap {
        let stride = ants.len().div_ceil(cap);
        ants.retain(|t| (t.0 as usize) % stride == 0);
    }
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
        let mut a: Vec<(u32, i32, i32, i8, i8, u32, u8)> = world.ants.iter()
            .filter(|a| a.x >= x0 && a.x < x1 && a.y >= y0 && a.y < y1)
            .map(|a| (a.id, a.x, a.y, a.dx, a.dy, a.owner, a.kind))
            .collect();
        cap_ants_by_id(&mut a, cfg().ant_view_cap as usize);
        a
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

/// Keyframe cadence: emit at least one self-contained keyframe every this many tile frames so a
/// client that missed a delta (the latest-wins channel can drop frames) resyncs within ~1.5 s.
const KF_INTERVAL: u32 = 15;
const DEFAULT_COLOR: &str = "#9b3027";

/// Per-client tile state retained by the viewport thread between cycles (NOT stored in `World`).
/// Lets us send small **deltas** (only changed cells) between periodic **keyframes**. Local ids
/// are kept *sticky* across a keyframe→delta run so a delta cell can reference a `u16 local` index
/// the client already knows (extended by `tileIdsAdd` as new owners appear).
pub struct PrevGrid {
    geom: (i32, i32, usize, usize, i32), // x0,y0,w,h,lod_step — a geometry change forces a keyframe
    local: Vec<u16>,                     // inner w*h served grid in sticky local ids
    id_to_local: FxHashMap<u32, u16>,
    tile_ids: Vec<u32>,                  // local -> global owner id
    tile_colors: Vec<String>,            // local -> colour (parallel to tile_ids)
    seq: u32,
    frames_since_kf: u32,
}

impl PrevGrid {
    /// Rough retained-bytes estimate for the per-connection memory gauge (`metrics::PREVGRID_BYTES`).
    /// Dominated by the `u16` served grid (`w*h*2`); the palette tables are tiny by comparison.
    pub fn est_bytes(&self) -> usize {
        self.local.len() * 2
            + self.tile_ids.len() * 4
            + self.tile_colors.iter().map(|c| c.len() + 24).sum::<usize>()
            + self.id_to_local.len() * 24
    }
}

/// Raw DEFLATE (RFC 1951, no zlib/gzip header) to match the client's `DecompressionStream('deflate-raw')`.
fn deflate_raw(data: &[u8]) -> Vec<u8> {
    let mut e = DeflateEncoder::new(Vec::new(), Compression::default());
    let _ = e.write_all(data);
    e.finish().unwrap_or_default()
}

// ---- Phase-3 binary control frames (kinds 16..=20) ------------------------
// One byte-0 namespace shared with the viewport frames (0..=3): the client routes `frame[0] < 16`
// → `handleViewBin`, `frame[0] >= 16` → inflate → `JSON.parse` → `handle()`. `metrics::kind_for_bin`
// already classifies these for the egress ledger.
pub const CTL_ME: u8 = 16;
pub const CTL_LEADERBOARD: u8 = 17;
pub const CTL_STATS: u8 = 18;
pub const CTL_REGION_HOLDERS: u8 = 19;

/// Build a compressed binary CONTROL frame: `[u8 kind]` + `deflate_raw(json)`. Mirrors the viewport
/// frame envelope so the client can route purely on `frame[0]`. Deflate the (large, periodic)
/// control JSON once here, then fan the bytes out via `World::broadcast_ctl` / a per-client `ctl_tx`.
pub fn ctl_frame(kind: u8, json: &str) -> Vec<u8> {
    let deflated = deflate_raw(json.as_bytes());
    let mut frame = Vec::with_capacity(deflated.len() + 1);
    frame.push(kind);
    frame.extend_from_slice(&deflated);
    frame
}

fn color_for(palette: &Value, id: u32) -> String {
    palette.get(id.to_string()).and_then(|v| v.as_str()).unwrap_or(DEFAULT_COLOR).to_string()
}

/// Keyframe body: `[u32 LE headerLen][header (padded even)][tiles u16 LE * n][fog u8 * n]`.
/// The header is padded to an even length so the client's `Uint16Array` view over `tiles` (which
/// starts at `4 + headerLen`) is 2-byte aligned.
fn body_keyframe(header: &str, local: &[u16], fog: &[u8]) -> Vec<u8> {
    let mut hb = header.as_bytes().to_vec();
    if hb.len() % 2 == 1 { hb.push(b' '); }
    let mut body = Vec::with_capacity(4 + hb.len() + local.len() * 2 + fog.len());
    body.extend_from_slice(&(hb.len() as u32).to_le_bytes());
    body.extend_from_slice(&hb);
    for &l in local { body.extend_from_slice(&l.to_le_bytes()); }
    body.extend_from_slice(fog);
    body
}

/// Delta body: `[u32 LE headerLen][header][fog u8 * n][(u32 LE index, u16 LE local) * nChanged]`.
fn body_delta(header: &str, fog: &[u8], changed: &[(u32, u16)]) -> Vec<u8> {
    let mut hb = header.as_bytes().to_vec();
    if hb.len() % 2 == 1 { hb.push(b' '); }
    let mut body = Vec::with_capacity(4 + hb.len() + fog.len() + changed.len() * 6);
    body.extend_from_slice(&(hb.len() as u32).to_le_bytes());
    body.extend_from_slice(&hb);
    body.extend_from_slice(fog);
    for &(idx, l) in changed {
        body.extend_from_slice(&idx.to_le_bytes());
        body.extend_from_slice(&l.to_le_bytes());
    }
    body
}

/// Build a full keyframe (kind 1): a fresh sticky local palette + the whole served grid.
fn build_keyframe(
    raw: &RawView, palette: &Value, fog: &[u8], ants: &[Value], queens: &[Value], prev_seq: Option<u32>,
) -> (Vec<u8>, PrevGrid) {
    let mut id_to_local: FxHashMap<u32, u16> = FxHashMap::default();
    id_to_local.insert(0, 0);
    let mut tile_ids: Vec<u32> = vec![0];
    let mut tile_colors: Vec<String> = vec!["#ffffff".to_string()];
    let mut local: Vec<u16> = Vec::with_capacity(raw.w * raw.h);
    for yi in 0..raw.h {
        let row = (yi + raw.pad) * raw.pw + raw.pad;
        for xi in 0..raw.w {
            let id = raw.owners[row + xi];
            let l = match id_to_local.get(&id) {
                Some(&l) => l,
                None if tile_ids.len() >= 0xFFFF => 0xFFFF,
                None => {
                    let l = tile_ids.len() as u16;
                    id_to_local.insert(id, l);
                    tile_ids.push(id);
                    tile_colors.push(color_for(palette, id));
                    l
                }
            };
            local.push(l);
        }
    }
    let seq = prev_seq.map(|s| s.wrapping_add(1)).unwrap_or(0);
    let header = json!({
        "x0": raw.x0, "y0": raw.y0, "w": raw.w, "h": raw.h, "lod": raw.lod_step, "tick": raw.tick,
        "seq": seq, "ants": ants, "queens": queens,
        "tileIds": tile_ids, "tileColors": tile_colors,
    }).to_string();
    let deflated = deflate_raw(&body_keyframe(&header, &local, fog));
    let mut frame = Vec::with_capacity(deflated.len() + 1);
    frame.push(1u8);
    frame.extend_from_slice(&deflated);
    let np = PrevGrid {
        geom: (raw.x0, raw.y0, raw.w, raw.h, raw.lod_step),
        local, id_to_local, tile_ids, tile_colors, seq, frames_since_kf: 0,
    };
    (frame, np)
}

/// Phase B (no lock held): turn a `RawView` into a **binary, deflate-compressed** frame.
/// `[u8 kind][payload]` where kind 0 = ants-only (raw JSON, uncompressed), 1 = tile keyframe,
/// 2 = tile delta. Tile frames always carry the full fog field (it ripples when ownership changes,
/// so deltaing it isn't worth it) plus — for deltas — only the cells that changed since `prev`.
/// Returns the frame and the tile state to retain for this client's next cycle (ants-only frames
/// pass `prev` through untouched).
pub fn finish_view(raw: &RawView, palette: &Value, prev: Option<PrevGrid>, bin: bool) -> (Vec<u8>, Option<PrevGrid>) {
    if !raw.include_tiles {
        if bin {
            // kind 3: packed binary + deflate (Phase 3). Body (LE):
            //   [i32 x0][i32 y0][u16 w][u16 h][u64 tick][u32 n]
            //   then n × [u32 id][i32 x][i32 y][i8 dx][i8 dy][u32 owner][u8 kind]  (19 bytes/ant).
            // No fog filter here (same as the legacy frame) — the client hides fogged ants at draw
            // time using the fog field it already holds from tile frames.
            let mut body = Vec::with_capacity(24 + raw.ants.len() * 19);
            body.extend_from_slice(&raw.x0.to_le_bytes());
            body.extend_from_slice(&raw.y0.to_le_bytes());
            body.extend_from_slice(&(raw.w as u16).to_le_bytes());
            body.extend_from_slice(&(raw.h as u16).to_le_bytes());
            body.extend_from_slice(&raw.tick.to_le_bytes());
            body.extend_from_slice(&(raw.ants.len() as u32).to_le_bytes());
            for &(id, x, y, dx, dy, owner, kind) in &raw.ants {
                body.extend_from_slice(&id.to_le_bytes());
                body.extend_from_slice(&x.to_le_bytes());
                body.extend_from_slice(&y.to_le_bytes());
                body.push(dx as u8);
                body.push(dy as u8);
                body.extend_from_slice(&owner.to_le_bytes());
                body.push(kind);
            }
            let deflated = deflate_raw(&body);
            let mut frame = Vec::with_capacity(deflated.len() + 1);
            frame.push(3u8);
            frame.extend_from_slice(&deflated);
            return (frame, prev);
        }
        // kind 0 legacy (non-`bin` clients): raw JSON, uncompressed.
        let ants: Vec<Value> = raw.ants.iter()
            .map(|&(id, x, y, dx, dy, owner, kind)| json!([id, x, y, dx, dy, owner, kind]))
            .collect();
        let js = json!({
            "t": "view", "x0": raw.x0, "y0": raw.y0, "w": raw.w, "h": raw.h,
            "ants": ants, "tick": raw.tick,
        }).to_string();
        let mut frame = Vec::with_capacity(js.len() + 1);
        frame.push(0u8);
        frame.extend_from_slice(js.as_bytes());
        return (frame, prev);
    }

    // Fog (admins see everything).
    let fog: Vec<u8> = if raw.is_admin {
        vec![0u8; raw.w * raw.h]
    } else {
        compute_fog_field_slice(&raw.owners, raw.pw, raw.ph, raw.pad, raw.w, raw.h, raw.player_id)
    };

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

    let geom = (raw.x0, raw.y0, raw.w, raw.h, raw.lod_step);
    let n = raw.w * raw.h;

    // Keyframe when there's no prior grid, the geometry changed (pan/zoom), or the periodic
    // refresh is due. Otherwise build a delta, falling back to a keyframe if too much changed.
    let force_kf = match &prev {
        None => true,
        Some(p) => p.geom != geom || p.frames_since_kf >= KF_INTERVAL,
    };

    if !force_kf {
        let mut p = prev.unwrap();
        // Current grid against the sticky local palette, recording any newly-appeared owners.
        let mut ids_add: Vec<u32> = Vec::new();
        let mut colors_add: Vec<String> = Vec::new();
        let mut cur: Vec<u16> = Vec::with_capacity(n);
        for yi in 0..raw.h {
            let row = (yi + raw.pad) * raw.pw + raw.pad;
            for xi in 0..raw.w {
                let id = raw.owners[row + xi];
                let l = match p.id_to_local.get(&id) {
                    Some(&l) => l,
                    None if p.tile_ids.len() >= 0xFFFF => 0xFFFF,
                    None => {
                        let l = p.tile_ids.len() as u16;
                        p.id_to_local.insert(id, l);
                        p.tile_ids.push(id);
                        let c = color_for(palette, id);
                        p.tile_colors.push(c.clone());
                        ids_add.push(id);
                        colors_add.push(c);
                        l
                    }
                };
                cur.push(l);
            }
        }
        let mut changed: Vec<(u32, u16)> = Vec::new();
        for i in 0..n {
            if cur[i] != p.local[i] { changed.push((i as u32, cur[i])); }
        }
        if changed.len() <= n / 4 {
            let seq = p.seq.wrapping_add(1);
            // Fog-on-keyframes-only (Phase 3): `bin` clients retain the last keyframe's fog and we
            // omit it from deltas (`nofog:1`, ~25 KB/s/viewer saved). Legacy clients still get it.
            let send_fog: &[u8] = if bin { &[] } else { &fog };
            let header = json!({
                "x0": raw.x0, "y0": raw.y0, "w": raw.w, "h": raw.h, "lod": raw.lod_step, "tick": raw.tick,
                "seq": seq, "baseSeq": p.seq, "ants": ants, "queens": queens,
                "tileIdsAdd": ids_add, "tileColorsAdd": colors_add, "nChanged": changed.len(),
                "nofog": if bin { 1 } else { 0 },
            }).to_string();
            let deflated = deflate_raw(&body_delta(&header, send_fog, &changed));
            let mut frame = Vec::with_capacity(deflated.len() + 1);
            frame.push(2u8);
            frame.extend_from_slice(&deflated);
            let np = PrevGrid {
                geom, local: cur, id_to_local: p.id_to_local, tile_ids: p.tile_ids,
                tile_colors: p.tile_colors, seq, frames_since_kf: p.frames_since_kf + 1,
            };
            return (frame, Some(np));
        }
        // Too much changed → keyframe instead (seq stays monotonic).
        let (frame, np) = build_keyframe(raw, palette, &fog, &ants, &queens, Some(p.seq));
        return (frame, Some(np));
    }

    let prev_seq = prev.as_ref().map(|p| p.seq);
    let (frame, np) = build_keyframe(raw, palette, &fog, &ants, &queens, prev_seq);
    (frame, Some(np))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Read;

    fn inflate(data: &[u8]) -> Vec<u8> {
        let mut d = flate2::read::DeflateDecoder::new(data);
        let mut out = Vec::new();
        d.read_to_end(&mut out).unwrap();
        out
    }

    fn header_of(body: &[u8]) -> (Value, usize) {
        let hlen = u32::from_le_bytes([body[0], body[1], body[2], body[3]]) as usize;
        (serde_json::from_slice(&body[4..4 + hlen]).unwrap(), 4 + hlen)
    }

    fn raw2x2(owners: Vec<u32>, tick: u64) -> RawView {
        RawView {
            x0: 0, y0: 0, w: 2, h: 2, tick, include_tiles: true, player_id: 999, is_admin: true,
            pad: 0, pw: 2, ph: 2, owners, ants: Vec::new(), queens: Vec::new(), lod_step: 1,
        }
    }

    /// A keyframe must round-trip owner ids > 65,535 through the per-frame local palette.
    /// The old `id.min(0xFFFF)` clamp collapsed every such id onto 65,535 on the wire.
    #[test]
    fn keyframe_round_trips_ids_above_u16() {
        let owners = vec![0u32, 70_000, 65_535, 70_000];
        let raw = raw2x2(owners.clone(), 1);

        let (frame, prev) = finish_view(&raw, &json!({}), None, false);
        assert_eq!(frame[0], 1, "first tile frame is a keyframe");
        let body = inflate(&frame[1..]);
        let (header, toff) = header_of(&body);

        let tile_ids: Vec<u32> = header["tileIds"].as_array().unwrap()
            .iter().map(|x| x.as_u64().unwrap() as u32).collect();
        assert_eq!(tile_ids[0], 0, "local index 0 must stay unclaimed");

        for cell in 0..owners.len() {
            let local = u16::from_le_bytes([body[toff + cell * 2], body[toff + cell * 2 + 1]]) as usize;
            assert_eq!(tile_ids[local], owners[cell],
                "cell {cell}: local index {local} must map back to the true owner id");
        }
        assert!(tile_ids.contains(&70_000), "the >65,535 owner survived end-to-end");

        let foff = toff + owners.len() * 2;     // fog follows the tiles
        assert_eq!(body.len(), foff + owners.len(), "fog is the full w*h tail");
        assert_eq!(prev.unwrap().seq, 0);
    }

    /// After a keyframe, an unchanged geometry with a small change must produce a delta carrying
    /// only the changed cell, the new owner's colour patch, and a matching baseSeq.
    #[test]
    fn delta_encodes_only_changed_cells() {
        let (kf, prev) = finish_view(&raw2x2(vec![0, 5, 5, 0], 1), &json!({"5": "#abcdef"}), None, false);
        assert_eq!(kf[0], 1);

        // Change cell index 2 from owner 5 → new owner 7.
        let palette = json!({"5": "#abcdef", "7": "#123456"});
        let (df, _prev2) = finish_view(&raw2x2(vec![0, 5, 7, 0], 2), &palette, prev, false);
        assert_eq!(df[0], 2, "same geometry + small change ⇒ delta");

        let body = inflate(&df[1..]);
        let (header, after_hdr) = header_of(&body);
        assert_eq!(header["baseSeq"].as_u64().unwrap(), 0);
        assert_eq!(header["seq"].as_u64().unwrap(), 1);
        assert_eq!(header["nChanged"].as_u64().unwrap(), 1);

        let add: Vec<u32> = header["tileIdsAdd"].as_array().unwrap()
            .iter().map(|x| x.as_u64().unwrap() as u32).collect();
        assert!(add.contains(&7), "the newly-appeared owner is appended to the sticky palette");

        // Layout after the header: full fog (w*h = 4), then the changed-cell record (index u32).
        let coff = after_hdr + 4;
        let idx = u32::from_le_bytes([body[coff], body[coff + 1], body[coff + 2], body[coff + 3]]);
        assert_eq!(idx, 2, "the changed cell index is encoded");
    }

    /// Phase-3 `bin` delta: fog-on-keyframes-only. The delta drops the full-fog block and tags
    /// `nofog:1`, so the changed-cell records start immediately after the header (no w*h tail).
    #[test]
    fn bin_delta_omits_fog() {
        let (kf, prev) = finish_view(&raw2x2(vec![0, 5, 5, 0], 1), &json!({"5": "#abcdef"}), None, true);
        assert_eq!(kf[0], 1, "keyframe still carries fog regardless of bin");

        let palette = json!({"5": "#abcdef", "7": "#123456"});
        let (df, _) = finish_view(&raw2x2(vec![0, 5, 7, 0], 2), &palette, prev, true);
        assert_eq!(df[0], 2);
        let body = inflate(&df[1..]);
        let (header, after_hdr) = header_of(&body);
        assert_eq!(header["nofog"].as_u64().unwrap(), 1, "bin delta omits fog");
        assert_eq!(header["nChanged"].as_u64().unwrap(), 1);
        let idx = u32::from_le_bytes([body[after_hdr], body[after_hdr + 1], body[after_hdr + 2], body[after_hdr + 3]]);
        assert_eq!(idx, 2, "changed cell index immediately follows the header (no fog block)");
        assert_eq!(body.len(), after_hdr + 6, "exactly header + one 6-byte changed record");
    }

    /// Phase-3B visible-ant cap: subsampling holds at/under the cap, is a no-op under it, and is
    /// deterministic by id (same input → same kept set ⇒ no cross-frame flicker).
    #[test]
    fn ant_cap_subsamples_stably_by_id() {
        let mk = |n: u32| (0..n).map(|i| (i, 0i32, 0i32, 0i8, 0i8, 1u32, 0u8)).collect::<Vec<_>>();

        // Under cap → untouched.
        let mut a = mk(100);
        cap_ants_by_id(&mut a, 4000);
        assert_eq!(a.len(), 100);

        // cap == 0 → unlimited (untouched).
        let mut b = mk(10_000);
        cap_ants_by_id(&mut b, 0);
        assert_eq!(b.len(), 10_000);

        // Over cap → trimmed to ≤ cap, deterministic, and every survivor matches the id-stride.
        let mut c = mk(10_000);
        cap_ants_by_id(&mut c, 4000);
        assert!(c.len() <= 4000, "trimmed to at most the cap (got {})", c.len());
        let stride = 10_000usize.div_ceil(4000); // = 3
        assert!(c.iter().all(|t| (t.0 as usize) % stride == 0), "survivors are exactly the id-stride set");
        let mut c2 = mk(10_000);
        cap_ants_by_id(&mut c2, 4000);
        assert_eq!(c, c2, "same input ⇒ same kept set (frame-stable, no flicker)");
    }

    /// Phase-3 `bin` ants-only frame is packed binary (kind 3) + deflate; each record round-trips.
    #[test]
    fn packed_ants_round_trip() {
        let raw = RawView {
            x0: 10, y0: 20, w: 4, h: 4, tick: 7, include_tiles: false, player_id: 1, is_admin: false,
            pad: 0, pw: 0, ph: 0, owners: Vec::new(),
            ants: vec![(42, 13, 25, 1, 0, 9, 1), (43, 11, 22, 0, -1, 9, 0)],
            queens: Vec::new(), lod_step: 1,
        };
        let (frame, _) = finish_view(&raw, &json!({}), None, true);
        assert_eq!(frame[0], 3, "bin ants-only frame is kind 3");
        let body = inflate(&frame[1..]);
        assert_eq!(u32::from_le_bytes([body[20], body[21], body[22], body[23]]), 2, "n ants");
        assert_eq!(u32::from_le_bytes([body[24], body[25], body[26], body[27]]), 42, "first ant id");
        assert_eq!(i32::from_le_bytes([body[28], body[29], body[30], body[31]]), 13, "first ant x");
    }
}
