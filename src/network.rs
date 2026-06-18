use std::io::Write;
use flate2::{write::DeflateEncoder, Compression};
use rustc_hash::FxHashMap;
use serde_json::{json, Value};

use crate::config::{cfg, total_xp_for_level, calc_score, current_ms};
use crate::fog::{compute_fog_field_slice, MAX_PAD};
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

/// Per-owner tile-effect map `{ idStr: "glow" }` for owners with an equipped `tile_fx` cosmetic.
/// Built once per tile cycle (beside `get_palette`) and shared via `Arc` across every client's
/// `finish_view`. Owners with no tile-fx are simply absent (→ `fx_for` yields `""`). This is what
/// makes a player's cosmetic visible to *every* viewer, not just themselves.
pub fn get_fx_palette(world: &World) -> Value {
    let mut p = serde_json::Map::new();
    for (id, pl) in &world.players {
        if let Some(fx) = pl.tile_fx.as_deref() {
            if !fx.is_empty() { p.insert(id.to_string(), json!(fx)); }
        }
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

/// Display name for an account id, resolving via the live player first, then the account record.
fn account_name(world: &World, id: u32) -> String {
    world.players.get(&id).map(|p| p.username.clone())
        .or_else(|| world.auth.users.values().find(|u| u.id == id).map(|u| u.display_name().to_string()))
        .unwrap_or_else(|| id.to_string())
}

/// Full state of one alliance for its members' panel: roster (with per-member live stats), pending
/// requests/invites, tier + buffs + XP progress. Sent on join/leave/level-change and on `alliance-get`.
pub fn build_alliance_state(world: &World, aid: u32) -> String {
    let Some(al) = world.auth.alliances.get(&aid) else {
        return json!({"t":"alliance-state","alliance":null}).to_string();
    };
    let level = al.level();
    let buffs = crate::config::alliance_buffs(level);
    let members: Vec<Value> = al.members.iter().map(|&id| {
        let q = world.queens.get(&id).filter(|q| !q.dead);
        json!({
            "id":     id,
            "name":   account_name(world, id),
            // Each member keeps their OWN colour in the panel (no shared alliance colour).
            "color":  world.players.get(&id).map(|p| p.color.clone()),
            "level":  q.map(|q| q.level).unwrap_or(0),
            "tiles":  q.map(|q| q.cached_tiles).unwrap_or(0),
            "kills":  q.map(|q| q.kills).unwrap_or(0),
            "online": world.players.get(&id).map(|p| p.tx.is_some()).unwrap_or(false),
            "alive":  q.is_some(),
            "leader": id == al.leader_id,
        })
    }).collect();
    let requests: Vec<Value> = al.requests.iter().map(|&id| json!({"id":id,"name":account_name(world,id)})).collect();
    let invites:  Vec<Value> = al.invites.iter().map(|&id| json!({"id":id,"name":account_name(world,id)})).collect();
    let cap = crate::config::ALLIANCE_LEVEL_CAP;
    let next_tier_xp = if level >= cap { Value::Null } else { json!(crate::config::alliance_total_xp_for_level(level + 1)) };
    json!({
        "t": "alliance-state",
        "alliance": {
            "id": aid, "name": al.name, "icon": al.icon, "color": al.color,
            "leaderId": al.leader_id, "level": level, "levelCap": cap,
            "xp": al.xp, "tierStartXp": crate::config::alliance_total_xp_for_level(level), "nextTierXp": next_tier_xp,
            "maxMembers": crate::config::ALLIANCE_MAX_MEMBERS,
            "buffs": {
                "dmg": buffs.dmg_mult, "hp": buffs.hp_mult, "shield": buffs.shield_mult,
                "defenderBonus": buffs.defender_bonus, "phalanx": buffs.phalanx,
            },
            "members": members, "requests": requests, "invites": invites,
        }
    }).to_string()
}

/// Faction leaderboard + recolor roster: one entry per alliance (name/icon/colour/tier, summed member
/// score/tiles/kills, and the member-id list the client maps to the banner recolour). Ranked by score.
pub fn build_factions(world: &World) -> String {
    let mut entries: Vec<Value> = world.auth.alliances.values().map(|al| {
        let (mut score, mut tiles, mut kills) = (0i64, 0u64, 0u32);
        for &id in &al.members {
            if let Some(q) = world.queens.get(&id).filter(|q| !q.dead) {
                let placed = world.players.get(&id).and_then(|p| p.queen_placed_at);
                score += calc_score(q.cached_tiles, placed, q.kills) as i64;
                tiles += q.cached_tiles;
                kills += q.kills;
            }
        }
        json!({
            "id": al.id, "name": al.name, "icon": al.icon, "color": al.color,
            "level": al.level(), "members": al.members, "memberCount": al.members.len(),
            "tiles": tiles, "kills": kills, "score": score,
        })
    }).collect();
    entries.sort_by(|a, b| b["score"].as_i64().unwrap_or(0).cmp(&a["score"].as_i64().unwrap_or(0)));
    json!({"t": "factions", "entries": entries}).to_string()
}

/// Compact roster of live queens — the spectator landing page's "jump between queens" source. Guests
/// receive NO queens in viewport frames (queens ride tile frames, which guests never get), so this is
/// how the spectator canvas knows where queens are. Capped to the top N by level so the frame stays
/// small; sent only to guests on the leaderboard cadence (≈0.75 Hz) → negligible egress.
const ROSTER_TOP_N: usize = 200;
pub fn build_queen_roster(world: &World) -> String {
    let mut qs: Vec<(u16, Value)> = world.queens.iter()
        .filter(|(_, q)| !q.dead)
        .map(|(&id, q)| {
            let p = world.players.get(&id);
            (q.level, json!({
                "id":    id,
                "name":  p.map(|p| p.username.clone()).unwrap_or_default(),
                "color": p.map(|p| p.color.clone()).unwrap_or_else(|| "#888".into()),
                "level": q.level,
                "x": q.x, "y": q.y,
            }))
        })
        .collect();
    qs.sort_by_key(|q| std::cmp::Reverse(q.0));
    qs.truncate(ROSTER_TOP_N);
    let queens: Vec<Value> = qs.into_iter().map(|(_, v)| v).collect();
    json!({"t": "queen-roster", "queens": queens}).to_string()
}

/// World-wide server stats pushed to every connected client (~1 Hz). Replaces per-client
/// `/health` polling — built once, broadcast to all, so cost is O(1) not O(players) HTTP
/// hits against the world lock. Drives the header bar + admin status cards.
pub fn build_server_stats(world: &World) -> String {
    let online = world.players.values().filter(|p| !p.npc && !p.guest && p.tx.is_some()).count();
    let spectators = world.players.values().filter(|p| p.guest).count();
    let queens = world.queens.values().filter(|q| !q.dead).count();
    json!({
        "t":       "server-stats",
        "online":  online,
        "spectators": spectators,
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
///
/// `ants_by_owner`, when `Some`, is a per-cycle precomputed `owner → [[id, remainingLife, kind], …]`
/// map (each capped at 120) so the periodic ≥1 Hz `me` for every connected player doesn't each rescan
/// the *whole* ant vec — that was O(players × total_ants) under the viewport read lock. `None` falls
/// back to the direct scan (used by the rare one-shot login/spectate calls).
pub fn build_player_info(
    world: &World, player_id: u32, full: bool,
    ants_by_owner: Option<&FxHashMap<u32, Vec<Value>>>,
) -> String {
    let c = cfg().clone();
    let Some(p) = world.players.get(&player_id) else {
        return json!({"t":"err","msg":"player not found"}).to_string();
    };
    let q = world.queens.get(&player_id);
    let tiles   = q.map(|q| q.cached_tiles).unwrap_or(0);
    let secs    = p.queen_placed_at
        .map(|t| current_ms().saturating_sub(t) / 1000)   // saturating: tolerate a backwards clock step
        .unwrap_or(0);
    let score   = calc_score(tiles, p.queen_placed_at, q.map(|q| q.kills).unwrap_or(0));
    let xp      = q.map(|q| q.xp).unwrap_or(0.0);
    let xp_this = q.map(|q| total_xp_for_level(q.level, &c)).unwrap_or(0.0);
    let xp_next = q.map(|q| total_xp_for_level(q.level + 1, &c)).unwrap_or(c.xp_base);

    // Player's own live workers for the WORKERS active-list (capped at 120; each entry is
    // [id, remaining_lifespan_ticks, kind]). Use the per-cycle precomputed map when present (one pass
    // over all ants shared across every viewer); else fall back to a direct, early-out scan.
    let my_ants: Vec<Value> = match ants_by_owner {
        Some(map) => map.get(&player_id).cloned().unwrap_or_default(),
        None => {
            let mut v: Vec<Value> = Vec::new();
            for a in world.ants.iter() {
                if a.owner != player_id { continue; }
                // remaining = LIVE global lifespan − age (not the spawn-time `a.lifespan`), so the
                // WORKERS bar drains at the current admin "WORKER RETURN" rate and matches Phase-6 expiry.
                v.push(json!([a.id, c.lifespan.saturating_sub(a.age), a.kind]));
                if v.len() >= 120 { break; }
            }
            v
        }
    };

    let mut visited_countries: Vec<&String> = p.visited_countries.iter().collect();
    visited_countries.sort();
    let mut visited_continents: Vec<&String> = p.visited_continents.iter().collect();
    visited_continents.sort();

    // Top rivalries (Discovery): the 5 biggest counts in each direction + lifetime totals.
    let top_rivals = |m: &FxHashMap<String, u32>| -> (Vec<Value>, u64) {
        let total: u64 = m.values().map(|&v| v as u64).sum();
        let mut v: Vec<(&String, u32)> = m.iter().map(|(k, &c)| (k, c)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        let top: Vec<Value> = v.iter().take(5).map(|(name, c)| json!({"name": name, "count": c})).collect();
        (top, total)
    };
    let (killed_by_top, total_deaths) = top_rivals(&p.killed_by);
    let (kills_of_top,  total_kills)  = top_rivals(&p.kills_of);

    let is_admin = world.auth.is_admin_id(player_id);
    let color_chosen = if is_admin {
        world.auth.users.get(&p.username).map(|u| u.color_chosen).unwrap_or(false)
    } else {
        true
    };
    // Highest level this USER has ever reached — drives client-side unlock gating (see auth).
    let peak_level = world.auth.users.get(&p.username).map(|u| u.peak_level).unwrap_or(0);
    // Daily claim state (00:00-UTC windows): true while the current window's portion is unclaimed.
    // Display only — the `claim-daily` handler re-validates against the server clock.
    let now_ms = current_ms();
    let claim_ready = !p.npc && !p.guest && world.auth.users.get(&p.username)
        .map(|u| u.last_claim_day < crate::config::utc_day(now_ms))
        .unwrap_or(false);

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

    // Camera home-region half-extent (tiles): the default radius, grown to enclose the player's whole
    // territory (+margin) so they can always see it. Sent on every `me` so the box expands live.
    let owner_bounds = world.tiles.owner_bounds(player_id);
    let pan_radius: u32 = {
        let def = crate::config::home_pan_radius();
        match (q, owner_bounds) {
            (Some(q), Some(b)) => {
                // Compute in i64 (queen coords are signed, bounds are u32) → widest half-extent from
                // the queen to any territory edge, + margin, clamped back into u32 and floored at def.
                let (qx, qy) = (q.x as i64, q.y as i64);
                let hx = (qx - b[0] as i64).max(b[2] as i64 - qx);
                let hy = (qy - b[1] as i64).max(b[3] as i64 - qy);
                let grown = hx.max(hy).max(0) as u64 + crate::config::pan_margin() as u64;
                (grown.min(u32::MAX as u64) as u32).max(def)
            }
            _ => def,
        }
    };

    // Level-scaled fog-of-war load radii (tiles) + territory AABB. These drive the client's
    // three-zone fog (clear/fog/void), its free-pan zoom-out cap, and its void fetch-gate.
    // `ownerBounds` is null until the player owns tiles (the client then falls back to the queen).
    let fog_level   = q.map(|q| q.level).unwrap_or(1);
    let clear_r     = crate::config::fog_clear_r(fog_level, &c).ceil() as u32;
    let load_osm_r  = crate::config::fog_grad_r(fog_level, &c).ceil() as u32;
    let void_r      = crate::config::fog_void_r(fog_level, &c).ceil() as u32;
    let owner_bounds_json = match owner_bounds {
        Some(b) => json!([b[0], b[1], b[2], b[3]]),
        None    => Value::Null,
    };
    // Zoom-out anchor: the AABB of the territory island CONTAINING the queen (chunk-grid flood
    // fill). After a relocation splits the empire, this keeps the max zoom-out framed on the
    // queen's island instead of the planetary global AABB; islands merge back automatically once
    // a painted trail reconnects them. Pan leash keeps `ownerBounds`, so far islands stay pannable.
    let zoom_bounds_json = match q.filter(|q| !q.dead) {
        Some(q) => match world.tiles.queen_island_bounds(player_id, q.x.max(0) as u32, q.y.max(0) as u32) {
            Some(b) => json!([b[0], b[1], b[2], b[3]]),
            None    => Value::Null,
        },
        None => owner_bounds_json.clone(),
    };

    // Dynamic fields — sent on every periodic `me` (≥1 Hz).
    let mut info = json!({
        "t": "me",
        "id": player_id,
        "username": p.username,
        "color": p.color,
        "isAdmin": is_admin,
        "colorChosen": color_chosen,
        "peakLevel": peak_level,
        "antsAvail": p.ants_avail,
        // Countdown to the next 00:00 UTC reset + whether the current window is still claimable.
        "nextResetMs": crate::config::next_utc_midnight_ms(now_ms).saturating_sub(now_ms),
        "claimReady": claim_ready,
        "queen": queen_val,
        "prestige":  p.prestige,
        "nectar":    p.nectar,
        // Cosmetics currency — sourced from the account record (wipe-proof, like peak_level).
        "gems":      world.auth.users.get(&p.username).map(|u| u.gems).unwrap_or(0),
        // Owned + equipped cosmetics (same wipe-proof account source) — drive the shop UI + glow.
        "ownedCosmetics": world.auth.users.get(&p.username).map(|u| u.owned_cosmetics.clone()).unwrap_or_default(),
        "equipped":  world.auth.users.get(&p.username).map(|u| json!(u.equipped)).unwrap_or_else(|| json!({})),
        // Alliance membership (id or null) — the client uses it to gate the panel + apply buffs/UI.
        "allianceId": world.alliance_of(player_id),
        "region":    q.map(|q| q.region.clone()).unwrap_or_default(),
        "defenders": p.defenders.len(),
        "visitedCountries":  visited_countries,
        "visitedContinents": visited_continents,
        "lifetimeKills":     p.lifetime_kills,
        "lifetimePeakTiles": p.lifetime_peak_tiles,
        "queensFielded":     p.queens_fielded,
        "unlimitedNectar":   p.unlimited_nectar,
        "unlimitedAnts":     p.unlimited_ants,
        "topRivalries": {
            "killedBy":    killed_by_top,
            "youKilled":   kills_of_top,
            "totalDeaths": total_deaths,
            "totalKills":  total_kills,
        },
        "shield":    q.map(|q| q.shield).unwrap_or(0),
        // The client clamps `view.x/y` to this box (half-extent, tiles) around the queen — keeps
        // players near their region and bounds the basemap tile universe (R2 free-tier).
        "panRadius": pan_radius,
        // Level-scaled fog-of-war: clear (full detail) / OSM-load / void (no-fetch) radii in tiles,
        // plus the territory bounding box the client anchors all three zones on.
        "clearR":      clear_r,
        "loadOsmR":    load_osm_r,
        "voidR":       void_r,
        "ownerBounds": owner_bounds_json,
        "zoomBounds":  zoom_bounds_json,
        "stats": { "tiles": tiles, "secs": secs, "kills": q.map(|q|q.kills).unwrap_or(0), "score": score as i64 },
        "army": world.ant_counts.get(&player_id).copied().unwrap_or(0),
        "ants": my_ants,
        "tick": world.tick,
        // Phase-6: current R2 snapshot generation (changes on wipe → client cache-busts its tiles).
        "epoch": world.epoch,
    });

    // Static block — only in `logged-in` (full); the client retains + merges it across periodic `me`.
    if full {
        info["tickRate"] = json!(c.tick_rate);
        info["worldW"]   = json!(world.world_w);
        info["worldH"]   = json!(world.world_h);
        info["spawnX"]   = json!(c.spawn_x);
        info["spawnY"]   = json!(c.spawn_y);
        info["spawnPan"] = json!(c.spawn_pan);
        // Geo projection (game↔lat/lon) goes ONLY to authed players — the `/play` client needs it for
        // the basemap + geolocation. Guests/spectators are deliberately denied it so the public landing
        // page can't reverse-project a queen's game coords to its real-world location (concealment).
        // Same reason `basemapUrl` is withheld below — the landing page draws no basemap.
        if !p.guest {
            info["geo"] = json!({
                "capitolLat": c.capitol_lat,
                "capitolLon": c.capitol_lon,
                "tileMeters":  c.tile_meters,
            });
        }
        // Full tunable config so EVERY admin slider hydrates from the live server value — not just
        // the 5 fields the client used to receive (the rest sat at their HTML defaults and could
        // stomp the real config on the next preset-load/drag). Keys match the client's
        // CFG_SLIDER_MAP (uppercase); CONVERT_PCT stays 0–1 (the client scales ×100 for display).
        // Non-admin clients simply ignore the keys they don't render.
        info["cfg"] = json!({
            "TICK_RATE":         c.tick_rate,
            "LIFESPAN":          c.lifespan,
            "BUBBLE_R":          c.bubble_r,
            "HP_BASE":           c.hp_base,
            "HP_REGEN":          c.hp_regen,
            "SPAWN_PAN":         c.spawn_pan,
            "ANT_DAMAGE":        c.ant_damage,
            "CONVERT_PCT":       c.convert_pct,
            "DAILY_ANTS":        c.daily_ants,
            "XP_BASE":           c.xp_base,
            "XP_EXP":            c.xp_exp,
            "XP_KILL":           c.xp_kill,
            "XP_CONVERT":        c.xp_convert,
            "XP_TILE_AWARD":     c.xp_tile_award,
            "XP_HIGHWAY_TICK":   c.xp_highway_tick,
            "LEVELUP_ANT_GRANT": c.levelup_ant_grant,
            "LEVEL_CAP":         c.xp_level_cap,
            "ARMY_CAP":          c.army_cap,
            "NECTAR_PER_100K_DAY": c.nectar_per_100k_day,
        });
        // Phase-6 / ∥B static config: where the browser fetches R2 snapshot tiles + the base map.
        // Empty → both client features stay dormant (legacy WS-keyframe + raw OSM).
        if let Some(base) = crate::config::snapshot_client_base() { info["snapshotBase"] = json!(base); }
        // Basemap only for authed players (guests render territory from R2 on a blank background).
        if !p.guest {
            if let Some(bm) = crate::config::basemap_url() { info["basemapUrl"] = json!(bm); }
        }
        // Phase-6 lever B: super-tile span in game cells (S×256). The client keys snapshot tiles by
        // `floor(gx / snapTileCells)`, which must equal the server's `(sx,sy)` super-tile index.
        info["snapTileCells"] = json!(crate::config::snapshot_tile_chunks() * 256);
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
    /// When true, `finish_view` emits an all-zero fog field (god view): set for admins UNLESS they
    /// enabled the fog preview. Non-admins are never skip_fog, so the cleared path can't leak to them.
    skip_fog: bool,
    /// Padded (pw×ph) ownership slice for tile blob + fog; empty on ants-only frames.
    pad: usize, pw: usize, ph: usize,
    owners: Vec<u32>,
    /// Level-scaled clear / fully-fogged distances (in served-grid cells) for this viewer's fog.
    clear_r: f32, grad_r: f32,
    /// In-rect ants (id,x,y,dx,dy,owner,kind); fog visibility applied in `finish_view`.
    ants: Vec<(u32, i32, i32, i8, i8, u32, u8)>,
    queens: Vec<QueenLite>,
    /// LOD step: tiles per served grid cell. 1 = normal 1:1; >1 = zoomed-out overview where
    /// each `owners` cell samples one tile every `lod_step` tiles (the territory pyramid).
    lod_step: i32,
    /// Alliance co-members (player ids, excl. self). Their tiles seed shared fog, and their
    /// ants/queens are always visible to this viewer. Empty for unaffiliated players.
    allies: Vec<u32>,
}

/// Visible-ant cap (Phase 3B): subsample `ants` in place to ≈`cap` by a **stable id-stride**, so an
/// ant's keep/drop status holds frame-to-frame (no flicker) while the kept subset stays spatially
/// uniform. `cap == 0` or `len <= cap` ⇒ unchanged. Bounds worst-case dense-battle egress.
fn cap_ants_by_id(ants: &mut Vec<(u32, i32, i32, i8, i8, u32, u8)>, cap: usize) {
    if cap > 0 && ants.len() > cap {
        let stride = ants.len().div_ceil(cap);
        ants.retain(|t| (t.0 as usize).is_multiple_of(stride));
    }
}

/// AoI queen cap (OWASP A01): trim `queens` to at most `cap`, always keeping `reveal` queens (the
/// viewer's own / admin) and otherwise the nearest to (`cx`,`cy`). No-op at/under the cap. Squared
/// distance is computed in `i64` so far-apart world coords can't overflow.
fn cap_queens_to(queens: &mut Vec<QueenLite>, cx: i32, cy: i32, cap: usize) {
    if queens.len() <= cap { return; }
    queens.sort_by_key(|q| {
        let dx = (q.x - cx) as i64;
        let dy = (q.y - cy) as i64;
        (!q.reveal, dx * dx + dy * dy)
    });
    queens.truncate(cap);
}

/// Phase A (under the World read lock): extract the minimal owned data for one client's
/// viewport. Cheap relative to fog/base64/JSON — those happen in `finish_view`, unlocked.
pub fn snapshot_view(world: &World, player_id: u32, include_tiles: bool) -> Option<RawView> {
    let p  = world.players.get(&player_id)?;
    let v  = p.view.as_ref()?;
    let ww = world.world_w as i32;
    let wh = world.world_h as i32;

    // AoI hard cap (OWASP A01, defense-in-depth): re-clamp the stored span so even a view that
    // bypassed `view-set` can't widen the live-entity window past max_view_span. `view-set` already
    // clamps on store; this guarantees the invariant at the serialization boundary too.
    let (vx0, vy0, vx1, vy1) = crate::config::clamp_view_span(v.x0, v.y0, v.x1, v.y1);

    let x0 = vx0.max(0);
    let y0 = vy0.max(0);
    let fw = (vx1.min(ww) - x0).max(0);   // full requested tile span (clamped to world)
    let fh = (vy1.min(wh) - y0).max(0);
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

    // Level-scaled fog radii, in served-grid cells (÷ step at LOD), from the viewer's own queen
    // level (default L1 while placing). `pad` (off-screen ownership ring the distance transform
    // needs) is sized to the feather end so panning toward off-screen territory clears smoothly.
    // Admins get an all-zero fog field (god view) → skip the padded slice entirely (pad 0, radii
    // unused) UNLESS they enabled the fog preview, in which case they're treated like a player.
    let skip_fog = is_admin && !world.admin_fog_preview.contains(&player_id);
    let (clear_grid, grad_grid, fog_pad) = if include_tiles && !skip_fog {
        let c = cfg();
        let lvl = world.queens.get(&player_id).map(|q| q.level).unwrap_or(1);
        let cr = crate::config::fog_clear_r(lvl, &c) / step as f32;
        let gr = crate::config::fog_grad_r(lvl, &c)  / step as f32;
        let pad = (gr.ceil() as i32).clamp(1, MAX_PAD) as usize;
        (cr, gr, pad)
    } else {
        (0.0, 0.0, 0usize)
    };

    // In-rect ants (fog filtering deferred to finish_view). None while zoomed out (LOD).
    let ants: Vec<(u32, i32, i32, i8, i8, u32, u8)> = if lod {
        Vec::new()
    } else {
        let mut a: Vec<(u32, i32, i32, i8, i8, u32, u8)> = world.ants.iter()
            .filter(|a| a.x >= x0 && a.x < x1 && a.y >= y0 && a.y < y1)
            .map(|a| (a.id, a.x, a.y, a.dx, a.dy, a.owner, a.kind))
            .collect();
        // EGRESS GUARD: guests get a much tighter ant cap than players (territory is free via R2).
        let ant_cap = if p.guest { crate::config::guest_ant_cap() } else { cfg().ant_view_cap as usize };
        cap_ants_by_id(&mut a, ant_cap);
        a
    };

    // Tiles + fog ownership slice + queens only matter on tile frames. At LOD, each grid cell
    // samples the tile `step` apart (nearest-sample territory pyramid); fog runs on the grid.
    let (pad, pw, ph, owners, queens) = if include_tiles {
        let pad = fog_pad;
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
        let mut queens: Vec<QueenLite> = world.queens.iter()
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
        // AoI entity cap (defense-in-depth): keep the viewer's own/revealed queens, then the
        // nearest-to-centre, up to max_queens_per_frame — a crafted wide view can't enumerate all.
        cap_queens_to(&mut queens, x0 + (x1 - x0) / 2, y0 + (y1 - y0) / 2,
                      crate::config::max_queens_per_frame());
        (pad, pw, ph, owners, queens)
    } else {
        (0, 0, 0, Vec::new(), Vec::new())
    };

    // Shared-fog / shared-vision roster: co-members other than self (empty if unaffiliated).
    let allies: Vec<u32> = world.alliance_member_ids(player_id)
        .into_iter().filter(|&id| id != player_id).collect();

    Some(RawView {
        x0, y0, w, h, tick: world.tick, include_tiles, player_id, skip_fog,
        pad, pw, ph, owners, clear_r: clear_grid, grad_r: grad_grid, ants, queens, lod_step: step,
        allies,
    })
}

/// Keyframe cadence: emit at least one self-contained keyframe every this many *tile* frames so a
/// client that missed a delta (the latest-wins channel can drop frames; the client then freezes its
/// territory until the next keyframe, see `baseSeq` check in client.html) resyncs promptly.
///
/// This is the **drop-recovery latency**, so it must stay roughly constant in wall-clock time no
/// matter what `tile_hz` the operator picks — a fixed frame *count* would stretch to many seconds
/// once tile frames are spaced out. We target ~1.5 s: `1.5 × tile_hz` frames, floored at 8 so a very
/// low `tile_hz` still resyncs reasonably. (Geometry changes / large deltas still force a keyframe
/// on demand, so active play keyframes more often than this floor anyway.)
fn kf_interval() -> u32 {
    (crate::config::tile_hz().saturating_mul(3) / 2).max(8)
}
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
    tile_fx: Vec<String>,                // local -> tile-fx code ("" = none; parallel to tile_ids)
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
            + self.tile_fx.iter().map(|c| c.len() + 24).sum::<usize>()
            + self.id_to_local.len() * 24
    }
}

/// Raw DEFLATE (RFC 1951, no zlib/gzip header) to match the client's `DecompressionStream('deflate-raw')`.
/// Uses the MAX compression level (9) rather than the default (6): every WS viewport/ant/control
/// frame goes through here, the work runs lock-free in parallel per-client (rayon), and the VPS has
/// abundant idle CPU — so spending a little more CPU here directly buys ~15–30% less outgoing
/// bandwidth, which is the actual bottleneck. (Disk snapshots in `persist.rs` stay at the default
/// level — those run under the world lock's encode path and aren't on the egress wire.)
fn deflate_raw(data: &[u8]) -> Vec<u8> {
    let mut e = DeflateEncoder::new(Vec::new(), Compression::best());
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

/// Equipped tile-effect code for an owner (`""` when none) — sibling of `color_for`, read from the
/// per-cycle FX palette built by `get_fx_palette`.
fn fx_for(fx_palette: &Value, id: u32) -> String {
    fx_palette.get(id.to_string()).and_then(|v| v.as_str()).unwrap_or("").to_string()
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
    raw: &RawView, palette: &Value, fx_palette: &Value, fog: &[u8], ants: &[Value], queens: &[Value],
    prev_seq: Option<u32>,
) -> (Vec<u8>, PrevGrid) {
    let mut id_to_local: FxHashMap<u32, u16> = FxHashMap::default();
    id_to_local.insert(0, 0);
    let mut tile_ids: Vec<u32> = vec![0];
    let mut tile_colors: Vec<String> = vec!["#ffffff".to_string()];
    let mut tile_fx: Vec<String> = vec![String::new()];   // index 0 = unclaimed, never has an effect
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
                    tile_fx.push(fx_for(fx_palette, id));
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
        "tileIds": tile_ids, "tileColors": tile_colors, "tileFx": tile_fx,
    }).to_string();
    let deflated = deflate_raw(&body_keyframe(&header, &local, fog));
    let mut frame = Vec::with_capacity(deflated.len() + 1);
    frame.push(1u8);
    frame.extend_from_slice(&deflated);
    let np = PrevGrid {
        geom: (raw.x0, raw.y0, raw.w, raw.h, raw.lod_step),
        local, id_to_local, tile_ids, tile_colors, tile_fx, seq, frames_since_kf: 0,
    };
    (frame, np)
}

/// Phase B (no lock held): turn a `RawView` into a **binary, deflate-compressed** frame.
/// `[u8 kind][payload]` where kind 0 = ants-only (raw JSON, uncompressed), 1 = tile keyframe,
/// 2 = tile delta. Tile frames always carry the full fog field (it ripples when ownership changes,
/// so deltaing it isn't worth it) plus — for deltas — only the cells that changed since `prev`.
/// Returns the frame and the tile state to retain for this client's next cycle (ants-only frames
/// pass `prev` through untouched).
pub fn finish_view(raw: &RawView, palette: &Value, fx_palette: &Value, prev: Option<PrevGrid>, bin: bool) -> (Vec<u8>, Option<PrevGrid>) {
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

    // Fog (skip_fog ⇒ all-zero god view; admins without the preview, never non-admins).
    let fog: Vec<u8> = if raw.skip_fog {
        vec![0u8; raw.w * raw.h]
    } else {
        compute_fog_field_slice(&raw.owners, raw.pw, raw.ph, raw.pad, raw.w, raw.h, raw.player_id,
                                &raw.allies, raw.clear_r, raw.grad_r)
    };

    // Visible ants (own + allied always; others only where fog is not full).
    let ants: Vec<Value> = raw.ants.iter()
        .filter(|&&(_, x, y, _, _, owner, _)| {
            if owner == raw.player_id || raw.allies.contains(&owner) { return true; }
            let fi = (y - raw.y0) as usize * raw.w + (x - raw.x0) as usize;
            fog.get(fi).copied().unwrap_or(100) < 100
        })
        .map(|&(id, x, y, dx, dy, owner, kind)| json!([id, x, y, dx, dy, owner, kind]))
        .collect();

    // Visible queens.
    let queens: Vec<Value> = raw.queens.iter()
        .filter_map(|q| {
            if !q.reveal && !raw.allies.contains(&q.qid) {
                // Map world → served grid cell (÷ lod_step) before sampling fog. Own + allied queens
                // skip the cull (shared vision); rivals appear only where the fog field is not full.
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
        Some(p) => p.geom != geom || p.frames_since_kf >= kf_interval(),
    };

    if !force_kf {
        let mut p = prev.unwrap();
        // Current grid against the sticky local palette, recording any newly-appeared owners.
        let mut ids_add: Vec<u32> = Vec::new();
        let mut colors_add: Vec<String> = Vec::new();
        let mut fx_add: Vec<String> = Vec::new();
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
                        let fx = fx_for(fx_palette, id);
                        p.tile_fx.push(fx.clone());
                        ids_add.push(id);
                        colors_add.push(c);
                        fx_add.push(fx);
                        l
                    }
                };
                cur.push(l);
            }
        }
        let changed: Vec<(u32, u16)> = cur.iter().zip(&p.local).enumerate()
            .filter(|(_, (c, l))| c != l)
            .map(|(i, (&c, _))| (i as u32, c))
            .collect();
        if changed.len() <= n / 4 {
            let seq = p.seq.wrapping_add(1);
            // Fog-on-keyframes-only (Phase 3): `bin` clients retain the last keyframe's fog and we
            // omit it from deltas (`nofog:1`, ~25 KB/s/viewer saved). Legacy clients still get it.
            let send_fog: &[u8] = if bin { &[] } else { &fog };
            let header = json!({
                "x0": raw.x0, "y0": raw.y0, "w": raw.w, "h": raw.h, "lod": raw.lod_step, "tick": raw.tick,
                "seq": seq, "baseSeq": p.seq, "ants": ants, "queens": queens,
                "tileIdsAdd": ids_add, "tileColorsAdd": colors_add, "tileFxAdd": fx_add,
                "nChanged": changed.len(),
                "nofog": if bin { 1 } else { 0 },
            }).to_string();
            let deflated = deflate_raw(&body_delta(&header, send_fog, &changed));
            let mut frame = Vec::with_capacity(deflated.len() + 1);
            frame.push(2u8);
            frame.extend_from_slice(&deflated);
            let np = PrevGrid {
                geom, local: cur, id_to_local: p.id_to_local, tile_ids: p.tile_ids,
                tile_colors: p.tile_colors, tile_fx: p.tile_fx, seq, frames_since_kf: p.frames_since_kf + 1,
            };
            return (frame, Some(np));
        }
        // Too much changed → keyframe instead (seq stays monotonic).
        let (frame, np) = build_keyframe(raw, palette, fx_palette, &fog, &ants, &queens, Some(p.seq));
        return (frame, Some(np));
    }

    let prev_seq = prev.as_ref().map(|p| p.seq);
    let (frame, np) = build_keyframe(raw, palette, fx_palette, &fog, &ants, &queens, prev_seq);
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
            x0: 0, y0: 0, w: 2, h: 2, tick, include_tiles: true, player_id: 999, skip_fog: true,
            pad: 0, pw: 2, ph: 2, owners, clear_r: 0.0, grad_r: 0.0,
            ants: Vec::new(), queens: Vec::new(), lod_step: 1, allies: Vec::new(),
        }
    }

    /// A keyframe must round-trip owner ids > 65,535 through the per-frame local palette.
    /// The old `id.min(0xFFFF)` clamp collapsed every such id onto 65,535 on the wire.
    #[test]
    fn keyframe_round_trips_ids_above_u16() {
        let owners = vec![0u32, 70_000, 65_535, 70_000];
        let raw = raw2x2(owners.clone(), 1);

        // 70_000 has glow equipped → its tileFx code must round-trip alongside the colour palette.
        let (frame, prev) = finish_view(&raw, &json!({}), &json!({"70000": "glow"}), None, false);
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

        // tileFx parallels tileIds: the glowing owner carries "glow", index 0 (unclaimed) is "".
        let tile_fx: Vec<String> = header["tileFx"].as_array().unwrap()
            .iter().map(|x| x.as_str().unwrap().to_string()).collect();
        assert_eq!(tile_fx.len(), tile_ids.len(), "tileFx is parallel to tileIds");
        assert_eq!(tile_fx[0], "", "unclaimed local index carries no effect");
        let gi = tile_ids.iter().position(|&id| id == 70_000).unwrap();
        assert_eq!(tile_fx[gi], "glow", "the glowing owner's fx code round-trips on the keyframe");

        let foff = toff + owners.len() * 2;     // fog follows the tiles
        assert_eq!(body.len(), foff + owners.len(), "fog is the full w*h tail");
        assert_eq!(prev.unwrap().seq, 0);
    }

    /// After a keyframe, an unchanged geometry with a small change must produce a delta carrying
    /// only the changed cell, the new owner's colour patch, and a matching baseSeq.
    #[test]
    fn delta_encodes_only_changed_cells() {
        let (kf, prev) = finish_view(&raw2x2(vec![0, 5, 5, 0], 1), &json!({"5": "#abcdef"}), &json!({}), None, false);
        assert_eq!(kf[0], 1);

        // Change cell index 2 from owner 5 → new owner 7, who has glow equipped.
        let palette = json!({"5": "#abcdef", "7": "#123456"});
        let (df, _prev2) = finish_view(&raw2x2(vec![0, 5, 7, 0], 2), &palette, &json!({"7": "glow"}), prev, false);
        assert_eq!(df[0], 2, "same geometry + small change ⇒ delta");

        let body = inflate(&df[1..]);
        let (header, after_hdr) = header_of(&body);
        assert_eq!(header["baseSeq"].as_u64().unwrap(), 0);
        assert_eq!(header["seq"].as_u64().unwrap(), 1);
        assert_eq!(header["nChanged"].as_u64().unwrap(), 1);

        let add: Vec<u32> = header["tileIdsAdd"].as_array().unwrap()
            .iter().map(|x| x.as_u64().unwrap() as u32).collect();
        assert!(add.contains(&7), "the newly-appeared owner is appended to the sticky palette");

        // tileFxAdd parallels tileIdsAdd: the new owner's glow rides the delta.
        let fx_add: Vec<String> = header["tileFxAdd"].as_array().unwrap()
            .iter().map(|x| x.as_str().unwrap().to_string()).collect();
        let pos = add.iter().position(|&id| id == 7).unwrap();
        assert_eq!(fx_add[pos], "glow", "the new owner's fx code rides the delta");

        // Layout after the header: full fog (w*h = 4), then the changed-cell record (index u32).
        let coff = after_hdr + 4;
        let idx = u32::from_le_bytes([body[coff], body[coff + 1], body[coff + 2], body[coff + 3]]);
        assert_eq!(idx, 2, "the changed cell index is encoded");
    }

    /// Phase-3 `bin` delta: fog-on-keyframes-only. The delta drops the full-fog block and tags
    /// `nofog:1`, so the changed-cell records start immediately after the header (no w*h tail).
    #[test]
    fn bin_delta_omits_fog() {
        let (kf, prev) = finish_view(&raw2x2(vec![0, 5, 5, 0], 1), &json!({"5": "#abcdef"}), &json!({}), None, true);
        assert_eq!(kf[0], 1, "keyframe still carries fog regardless of bin");

        let palette = json!({"5": "#abcdef", "7": "#123456"});
        let (df, _) = finish_view(&raw2x2(vec![0, 5, 7, 0], 2), &palette, &json!({}), prev, true);
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
        assert!(c.iter().all(|t| (t.0 as usize).is_multiple_of(stride)), "survivors are exactly the id-stride set");
        let mut c2 = mk(10_000);
        cap_ants_by_id(&mut c2, 4000);
        assert_eq!(c, c2, "same input ⇒ same kept set (frame-stable, no flicker)");
    }

    fn ql(qid: u32, x: i32, y: i32, reveal: bool) -> QueenLite {
        QueenLite {
            qid, x, y, size: 2, hp: 10, max_hp: 10, level: 1, color: "#888".into(),
            username: "q".into(), prestige: 0, shield: 0, bubble_r: 30.0, reveal,
        }
    }

    /// AoI queen cap keeps revealed (own/admin) queens unconditionally, then the nearest to centre,
    /// and drops the rest — so a crafted wide view can't enumerate every queen on the map.
    #[test]
    fn queen_cap_keeps_revealed_and_nearest() {
        let mut qs = vec![
            ql(1, 10, 0, false),     // near
            ql(2, 1_000, 0, false),  // far
            ql(3, 500, 0, true),     // far-ish but REVEALED → must survive
            ql(4, 20, 0, false),     // near
        ];
        cap_queens_to(&mut qs, 0, 0, 2);
        let ids: Vec<u32> = qs.iter().map(|q| q.qid).collect();
        assert_eq!(qs.len(), 2);
        assert!(ids.contains(&3), "revealed queen is never dropped");
        assert!(ids.contains(&1), "nearest non-revealed kept");
        assert!(!ids.contains(&2), "farthest queen dropped");
    }

    #[test]
    fn queen_cap_is_noop_under_cap() {
        let mut qs = vec![ql(1, 0, 0, false), ql(2, 5, 0, false)];
        cap_queens_to(&mut qs, 0, 0, 256);
        assert_eq!(qs.len(), 2);
    }

    /// Phase-3 `bin` ants-only frame is packed binary (kind 3) + deflate; each record round-trips.
    #[test]
    fn packed_ants_round_trip() {
        let raw = RawView {
            x0: 10, y0: 20, w: 4, h: 4, tick: 7, include_tiles: false, player_id: 1, skip_fog: false,
            pad: 0, pw: 0, ph: 0, owners: Vec::new(), clear_r: 0.0, grad_r: 0.0,
            ants: vec![(42, 13, 25, 1, 0, 9, 1), (43, 11, 22, 0, -1, 9, 0)],
            queens: Vec::new(), lod_step: 1, allies: Vec::new(),
        };
        let (frame, _) = finish_view(&raw, &json!({}), &json!({}), None, true);
        assert_eq!(frame[0], 3, "bin ants-only frame is kind 3");
        let body = inflate(&frame[1..]);
        assert_eq!(u32::from_le_bytes([body[20], body[21], body[22], body[23]]), 2, "n ants");
        assert_eq!(u32::from_le_bytes([body[24], body[25], body[26], body[27]]), 42, "first ant id");
        assert_eq!(i32::from_le_bytes([body[28], body[29], body[30], body[31]]), 13, "first ant x");
    }
}
