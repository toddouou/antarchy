use serde_json::{json, Value};

use crate::auth::{hash_pw_argon2, needs_rehash, verify_pw, UserRecord};
use crate::config::{
    cfg, apply_admin_param, reset_to_defaults, queen_size_for_level, total_xp_for_level, level_for_xp,
    current_ms, HUES, ADMIN_USERNAME,
};
use crate::network::{build_leaderboard, build_player_info, build_queen_roster};
use crate::simulation::{apply_peak_unlocks, kill_queen, spawn_npc, wipe_world, wipe_world_and_users};
use crate::world::{Ant, BoundedTx, Player, PlayerView, Queen, World};

fn err(msg: &str) -> String {
    json!({"t":"err","msg":msg}).to_string()
}

/// Anti-replay (OWASP A01/A06): accept a mutating command only when its client `seq` strictly exceeds
/// the last one applied for this player. Messages without a `seq` (legacy clients / tooling) are
/// accepted unprotected. A replayed or out-of-order `seq` is dropped — logged, with no client error
/// (the original already applied). Per-connection state is reset on (re)connect and cleared on
/// disconnect. NB: keyed by player id, so it assumes the one-connection-per-account model (the same as
/// `conn_gen`); two simultaneous tabs on one account is not a supported configuration.
fn check_seq(world: &mut World, pid: u32, msg: &Value) -> bool {
    let Some(seq) = msg.get("seq").and_then(Value::as_u64) else { return true; };
    let last = world.last_seq.entry(pid).or_insert(0);
    if seq <= *last {
        println!("[anticheat] dropped replay/out-of-order seq pid={pid} seq={seq} last={last}");
        return false;
    }
    *last = seq;
    true
}

/// Deduct a shop purchase from the player's balance. The caller has already verified
/// affordability; `saturating_sub` keeps the arithmetic safe regardless.
fn charge(world: &mut World, pid: u32, price: u64, unlimited: bool) {
    if unlimited { return; }
    if let Some(p) = world.players.get_mut(&pid) {
        p.nectar = p.nectar.saturating_sub(price);
    }
}

/// Returns (player_id, msg_to_send) or None if message is silently ignored.
pub fn handle_message(
    world:     &mut World,
    player_id: &mut Option<u32>,
    tx:        &BoundedTx<String>,
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
        // Charset cap (also closes a stored-XSS vector via usernames rendered in the client).
        if !raw_u.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            let _ = tx.send(err("Username: letters, numbers, _ or - only")); return;
        }
        if pw.len() < 4 {
            let _ = tx.send(err("Password min 4 chars")); return;
        }
        if world.auth.users.contains_key(&raw_u) {
            let _ = tx.send(err("Username taken")); return;
        }
        let hue_idx = msg["hueIdx"].as_i64().unwrap_or(0) as i32;
        // Validate the colour to a #rrggbb hex; fall back to a starter hue (never store raw input).
        let color = {
            let c = msg["color"].as_str().unwrap_or("");
            if crate::config::valid_hex_color(c) { c.trim().to_string() }
            else { HUES.first().copied().unwrap_or("#b5524a").to_string() }
        };
        let id      = world.next_player_id;
        world.next_player_id += 1;
        world.auth.users.insert(raw_u.clone(), UserRecord {
            id, username: raw_u.clone(),
            password_hash: hash_pw_argon2(pw),
            color: color.clone(), hue_idx,
            is_admin: false, color_chosen: true,
            peak_level: 0,
            // Legacy WS register has no email/phone; handle mirrors the username. beta-v2 accounts
            // come through the REST /api/* path which sets email/phone/handle properly.
            handle: raw_u.clone(),
            // The signup starter inventory counts as day one's portion — first CLAIM at next 00:00 UTC.
            last_claim_day: crate::config::utc_day(current_ms()),
            ..Default::default()
        });
        world.auth.save();
        let welcome = create_or_reconnect_player(world, id, &raw_u, &color, hue_idx, tx.clone());
        *player_id = Some(id);
        apply_bin_cap(world, id, &msg);
        let me = build_player_info(world, id, true, None);
        let lb = build_leaderboard(world);
        let _ = tx.send(json!({"t":"logged-in","me":serde_json::from_str::<Value>(&me).unwrap_or(Value::Null)}).to_string());
        let _ = tx.send(lb);
        // Push the current aura/trail roster immediately so this client renders everyone's (incl. its
        // own, already reloaded above) equipped decorations at once — not after the ~5 s heartbeat.
        let _ = tx.send(crate::network::build_cosmetics_roster(world));
        if let Some(w) = welcome { let _ = tx.send(w); }
        return;
    }

    // ---- Login ----
    if t == "login" {
        let u   = msg["username"].as_str().unwrap_or("").trim().to_uppercase();
        let pw  = msg["password"].as_str().unwrap_or("");
        let rec = world.auth.users.get(&u).cloned();
        let Some(rec) = rec else { let _ = tx.send(err("Invalid credentials")); return; };
        if !verify_pw(&rec.password_hash, pw) { let _ = tx.send(err("Invalid credentials")); return; }
        if world.auth.banned.contains(&u) { let _ = tx.send(err("BANNED")); return; }
        if needs_rehash(&rec.password_hash) {
            let new_hash = hash_pw_argon2(pw);
            if let Some(ur) = world.auth.users.get_mut(&u) { ur.password_hash = new_hash; }
            world.auth.save();
        }
        let welcome = create_or_reconnect_player(world, rec.id, &rec.username, &rec.color, rec.hue_idx, tx.clone());
        *player_id = Some(rec.id);
        // Pre-existing high-level accounts predate the unlock system: silently raise peak_level to
        // their current queen level so their chrome shows immediately — WITHOUT firing popups/nectar.
        backfill_peak_level(world, rec.id);
        apply_bin_cap(world, rec.id, &msg);
        let me = build_player_info(world, rec.id, true, None);
        let lb = build_leaderboard(world);
        let _ = tx.send(json!({"t":"logged-in","me":serde_json::from_str::<Value>(&me).unwrap_or(Value::Null)}).to_string());
        let _ = tx.send(lb);
        let _ = tx.send(crate::network::build_cosmetics_roster(world));
        if let Some(w) = welcome { let _ = tx.send(w); }
        return;
    }

    // ---- Session-token login (game client at /play) ----
    // The WS task already validated the token against AppState.sessions and rewrote it into this
    // trusted internal message carrying the resolved user id — so we skip the password check.
    if t == "session-login" {
        let id  = msg["id"].as_u64().unwrap_or(0) as u32;
        let rec = world.auth.find_by_id(id).cloned();
        let Some(rec) = rec else { let _ = tx.send(err("Account not found")); return; };
        if world.auth.banned.contains(&rec.username) { let _ = tx.send(err("BANNED")); return; }
        let welcome = create_or_reconnect_player(world, rec.id, &rec.username, &rec.color, rec.hue_idx, tx.clone());
        *player_id = Some(rec.id);
        backfill_peak_level(world, rec.id);
        apply_bin_cap(world, rec.id, &msg);
        let me = build_player_info(world, rec.id, true, None);
        let lb = build_leaderboard(world);
        let _ = tx.send(json!({"t":"logged-in","me":serde_json::from_str::<Value>(&me).unwrap_or(Value::Null)}).to_string());
        let _ = tx.send(lb);
        let _ = tx.send(crate::network::build_cosmetics_roster(world));
        if let Some(w) = welcome { let _ = tx.send(w); }
        return;
    }

    // ---- Spectate (landing-page guest: ephemeral, read-only, egress-capped) ----
    if t == "spectate" {
        let max = crate::config::max_guests();
        if max == 0 || world.guest_count() >= max {
            // At the guest ceiling — the landing page falls back to the FREE path (R2 + /api/roster).
            let _ = tx.send(json!({"t":"spectator-full"}).to_string());
            return; // player_id stays None → this connection can't drive the sim further
        }
        let id = world.alloc_guest_id();
        world.players.insert(id, Player {
            id, username: "spectator".into(), color: "#8a8a8a".into(), hue_idx: -1,
            guest: true,
            tx: Some(tx.clone()),
            conn_gen: 1,
            ..Default::default()
        });
        *player_id = Some(id);
        // bin negotiation → guests get the compressed ant frames (kind 3) for minimal egress.
        apply_bin_cap(world, id, &msg);
        // Reuse the normal logged-in payload so the guest gets the snapshot config (R2 base/epoch/
        // super-tile span) + geo to render territory from FREE R2, plus the initial queen roster.
        let me = build_player_info(world, id, true, None);
        let _ = tx.send(json!({"t":"logged-in","spectator":true,"me":serde_json::from_str::<Value>(&me).unwrap_or(Value::Null)}).to_string());
        // Metro centres so the landing-page spectator camera can prioritise queens inside metros.
        // ANONYMIZED for guests (no city names) — geo-concealment: paired with a queen's public coords,
        // a named city centre would let a guest reverse-project the Mercator projection. See
        // `regions::metros_anon_json` + the guest-skipping leaderboard/region-holders broadcasts.
        let _ = tx.send(json!({"t":"regions","metros":crate::regions::metros_anon_json()}).to_string());
        let _ = tx.send(build_queen_roster(world));
        // Monuments (king-of-the-hill landmarks) so the spectator camera can frame them immediately
        // instead of waiting for the ~10 s holder-broadcast cycle. Tiny — only if any exist.
        if !world.monuments.is_empty() {
            let _ = tx.send(crate::network::build_monuments(world));
        }
        return;
    }

    // Viewport pause/resume can race ahead of auth on mobile: `visibilitychange` fires during the
    // keyboard/app-switch churn of the signup flow, before the player is logged in. Treat those as
    // no-ops when not yet authenticated instead of replying `err("Not logged in")` — that error
    // used to bounce the client back to the login screen mid-registration.
    if (t == "view-pause" || t == "view-resume") && player_id.is_none() { return; }

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
        let me = build_player_info(world, pid, true, None);
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
        // Store the rect clamped to the (generous) TERRITORY span so a zoomed-out player can frame a
        // whole empire — the served grid is still bounded to MAX_DIM cells by the LOD pyramid, so this
        // is egress-free. The live-entity (ant/queen) AoI is clamped SEPARATELY to the much tighter
        // max_view_span inside snapshot_view (the real anti map-hack boundary), so a wide stored rect
        // cannot leak live entities. EXCEPTION: an admin in god-view (fog preview OFF) stores the RAW
        // span so they can see the whole planet; the LOD pyramid caps the served grid regardless.
        let god_view = is_admin && !world.admin_fog_preview.contains(&pid);
        let (x0, y0, x1, y1) = if god_view { (x0, y0, x1, y1) }
                               else { crate::config::clamp_tile_view_span(x0, y0, x1, y1) };
        if let Some(p) = world.players.get_mut(&pid) {
            p.view = Some(PlayerView { x0, y0, x1, y1 });
        }
        // Viewport is delivered by sim_loop on the next send_viewports cycle (≤ 40 ms).
        // Building it here under the write lock would block all other players during fog computation.
        return;
    }

    // ---- Phase-7 visibility cull: hidden tab → stop viewport frames (~0 egress) ----
    if t == "view-pause"  { world.paused_views.insert(pid); return; }
    if t == "view-resume" { world.paused_views.remove(&pid); world.dirty_tick = world.tick; return; }

    // ---- Admin fog-of-war preview (server-authoritative; gated on is_admin) ----
    // Admins normally get an all-zero fog field (god view). Toggling this ON makes the server compute
    // the REAL fog for this admin so they can preview what players see. Non-admins can't reach it, so
    // the cleared-fog path is never exposed to them.
    if t == "fog-view" {
        if !is_admin { return; }
        if msg["on"].as_bool().unwrap_or(false) { world.admin_fog_preview.insert(pid); }
        else { world.admin_fog_preview.remove(&pid); }
        world.dirty_tick = world.tick;   // force the next viewport cycle to re-send tiles + fog
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
        // The requester's prospective bubble radius: queen placement must keep BOTH bubbles
        // apart (zones may never intersect), so the client inflates each zone by `placeR`.
        // Ant placement uses the raw `r`. Live queen → relocate at its current radius.
        let place_r = world.queens.get(&pid).filter(|q| !q.dead)
            .map(|q| q.bubble_r)
            .unwrap_or_else(|| cfg().bubble_r);
        let _ = tx.send(json!({"t":"forbidden-zones","zones":zones,"placeR":place_r}).to_string());
        return;
    }

    // ---- Place queen ----
    if t == "place-queen" {
        if !check_seq(world, pid, &msg) { return; }
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
        let size = queen_size_for_level(1);
        // No-overlap rule: the new queen's L1 bubble may not intersect any existing bubble.
        if world.queen_zone_overlaps(x + size as i32 / 2, y + size as i32 / 2, c.bubble_r, pid) {
            let _ = tx.send(err("Too close to another queen")); return;
        }
        let max_hp = crate::config::max_hp_for_level(1, &c);
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

    // ---- Daily claim (the ONLY daily-ant grant path; the rolling auto-refill is gone) ----
    // One portion per 00:00-UTC window, idempotent via UserRecord.last_claim_day. The rewarded-ad
    // gate (Group C) will become a precondition here — keep every grant inside claim_daily.
    if t == "claim-daily" {
        if !check_seq(world, pid, &msg) { return; }
        match claim_daily(world, pid, current_ms()) {
            Ok(amount) => {
                world.auth.save();   // persist the claimed window (users.json)
                let avail = world.players.get(&pid).map(|p| p.ants_avail).unwrap_or(0);
                let _ = tx.send(json!({"t":"daily-claimed","amount":amount,"antsAvail":avail}).to_string());
                let _ = tx.send(json!({"t":"event","msg":format!("+{amount} DAILY WORKERS")}).to_string());
            }
            Err(e) => { let _ = tx.send(err(e)); }
        }
        return;
    }

    // ---- Place ant ----
    if t == "place-ant" {
        if !check_seq(world, pid, &msg) { return; }
        let c = cfg();
        // Copy out p/q data before dropping borrows — get_queen_map() needs &mut World
        let (ants_avail, unlimited_ants) = match world.players.get(&pid) {
            Some(p) => (p.ants_avail, p.unlimited_ants),
            None => return,
        };
        if !unlimited_ants && ants_avail <= 0 { let _ = tx.send(err("No ants available")); return; }
        // Army is uncapped — deployment is bounded only by `ants_avail` (the worker inventory the
        // player actually owns), not a hard ceiling.
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
        if !unlimited_ants { if let Some(p) = world.players.get_mut(&pid) { p.ants_avail -= 1; } }
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
                    .map(|q| (q.level, q.hp, q.max_hp));
                let Some((cur_level, cur_hp, old_max)) = queen_state else { return };
                if cur_level >= c.xp_level_cap { return; }
                let new_xp  = total_xp_for_level(cur_level + 1, &c);
                let new_lvl = level_for_xp(new_xp, &c);
                let ants_gained = (new_lvl as i32 - cur_level as i32).max(0) * c.levelup_ant_grant;
                if let Some(ql) = world.queens.get_mut(&target_id) {
                    ql.xp    = new_xp;
                    ql.set_level(new_lvl, &c);
                    ql.hp    = (cur_hp + (ql.max_hp - old_max).max(0)).min(ql.max_hp);
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
            // beta-v2: the everyday WIPE clears the world but CONSERVES every account forever
            // (accounts + verification state live in users.json, untouched by wipe_world).
            "wipe-world" => wipe_world(world),
            // The rare full reset: also delete all non-admin accounts (type-"NUKE" button).
            "wipe-world-and-users" => wipe_world_and_users(world),
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
                // NPCs have no account to ban — remove the dummy entirely (kill_queen purges it).
                if world.players.get(&tid).map(|p| p.npc).unwrap_or(false) {
                    kill_queen(world, tid, None, "admin");
                } else if let Some(uname) = world.players.get(&tid).map(|p| p.username.clone()) {
                    world.auth.banned.insert(uname.clone());
                    world.auth.save(); // moderation state must survive a crash, not just the next graceful save
                    world.force_logout(tid, "BANNED BY ADMIN");
                    let msg = json!({"t":"event","msg":format!("[ADMIN] {uname} BANNED")}).to_string();
                    world.broadcast(&msg);
                }
            }
            "unlimited-nectar" => {
                // .map() releases the &mut players borrow before world.send_to() re-borrows world.
                let st = world.players.get_mut(&tid).map(|tp| {
                    tp.unlimited_nectar = !tp.unlimited_nectar;
                    (tp.unlimited_nectar, tp.username.clone())
                });
                if let Some((on, uname)) = st {
                    let lbl = if on { "ON" } else { "OFF" };
                    let _ = tx.send(json!({"t":"event","msg":format!("[ADMIN] {uname} · UNLIMITED NECTAR {lbl}")}).to_string());
                    world.send_to(tid, json!({"t":"event","msg":format!("UNLIMITED NECTAR {lbl}")}).to_string());
                }
            }
            "unlimited-ants" => {
                let st = world.players.get_mut(&tid).map(|tp| {
                    tp.unlimited_ants = !tp.unlimited_ants;
                    (tp.unlimited_ants, tp.username.clone())
                });
                if let Some((on, uname)) = st {
                    let lbl = if on { "ON" } else { "OFF" };
                    let _ = tx.send(json!({"t":"event","msg":format!("[ADMIN] {uname} · UNLIMITED ANTS {lbl}")}).to_string());
                    world.send_to(tid, json!({"t":"event","msg":format!("UNLIMITED ANTS {lbl}")}).to_string());
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
        // NPCs have no session to disconnect — remove the dummy entirely (kill_queen purges it).
        if world.players.get(&tid).map(|p| p.npc).unwrap_or(false) {
            kill_queen(world, tid, None, "admin");
            return;
        }
        if let Some(uname) = world.players.get(&tid).map(|p| p.username.clone()) {
            world.force_logout(tid, "KICKED BY ADMIN");
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
        if let Some(lvl) = world.queens.get(&tid).filter(|q| !q.dead).map(|q| q.level) {
            apply_peak_unlocks(world, tid, lvl);   // reveal gates + fire unlock popups on admin level-up too
        }
        world.send_to(tid, json!({"t":"event","msg":format!("[ADMIN] LEVEL SET TO {level}")}).to_string());
        return;
    }

    // ---- Admin give XP ----
    if t == "admin-give-xp" {
        if !is_admin { let _ = tx.send(err("Admin only")); return; }
        let tid = msg["targetId"].as_u64().unwrap_or(0) as u32;
        let xp  = msg["xp"].as_f64().unwrap_or(0.0);
        if !xp.is_finite() || xp <= 0.0 { return; }  // reject NaN/∞ before it poisons q.xp
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
        if let Some(lvl) = world.queens.get(&tid).filter(|q| !q.dead).map(|q| q.level) {
            apply_peak_unlocks(world, tid, lvl);   // reveal gates + fire unlock popups on admin XP too
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

    // ---- Admin: place a permanent MONUMENT at a map location (persisted to monuments.json) ----
    // Monuments are king-of-the-hill landmarks whose single holder (top tile-owner in the radius)
    // earns flat daily nectar. They live in their own runtime file → survive restarts AND wipes.
    if t == "admin-place-monument" {
        if !is_admin { let _ = tx.send(err("Admin only")); return; }
        let x = msg["x"].as_i64().unwrap_or(-1) as i32;
        let y = msg["y"].as_i64().unwrap_or(-1) as i32;
        let ww = world.world_w as i32; let wh = world.world_h as i32;
        if x < 0 || y < 0 || x >= ww || y >= wh { let _ = tx.send(err("Out of bounds")); return; }
        let id = world.next_monument_id;
        let name: String = {
            let n: String = msg["name"].as_str().unwrap_or("").trim().chars().take(40).collect();
            if n.is_empty() { format!("Monument {id}") } else { n }
        };
        // Default 40 km capture radius (≈ a metro); admin may override via `radiusKm`.
        let radius_km = msg["radiusKm"].as_f64().filter(|r| *r > 0.0).unwrap_or(40.0).clamp(1.0, 2000.0);
        let r2 = crate::regions::radius_km_to_r2(x, y, radius_km);
        world.next_monument_id += 1;
        world.monuments.push(crate::monuments::Monument { id, name: name.clone(), x, y, r2 });
        crate::monuments::save(&world.monuments);
        crate::simulation::recompute_monument_holders(world); // populate the holder for the new spot now
        world.broadcast(&crate::network::build_monuments(world));
        let _ = tx.send(json!({"t":"event","msg":format!("MONUMENT PLACED · {name}")}).to_string());
        return;
    }

    // ---- Admin: remove a monument by id ----
    if t == "admin-remove-monument" {
        if !is_admin { let _ = tx.send(err("Admin only")); return; }
        let id = msg["id"].as_u64().unwrap_or(0) as u32;
        let before = world.monuments.len();
        world.monuments.retain(|m| m.id != id);
        if world.monuments.len() == before { let _ = tx.send(err("No such monument")); return; }
        world.monument_holders.retain(|h| h.id != id);
        crate::monuments::save(&world.monuments);
        world.broadcast(&crate::network::build_monuments(world));
        let _ = tx.send(json!({"t":"event","msg":"MONUMENT REMOVED"}).to_string());
        return;
    }

    // ---- Admin: rename a monument by id ----
    if t == "admin-rename-monument" {
        if !is_admin { let _ = tx.send(err("Admin only")); return; }
        let id = msg["id"].as_u64().unwrap_or(0) as u32;
        let name: String = msg["name"].as_str().unwrap_or("").trim().chars().take(40).collect();
        if name.is_empty() { let _ = tx.send(err("Bad name")); return; }
        let Some(m) = world.monuments.iter_mut().find(|m| m.id == id) else {
            let _ = tx.send(err("No such monument")); return;
        };
        m.name = name;
        crate::monuments::save(&world.monuments);
        world.broadcast(&crate::network::build_monuments(world));
        let _ = tx.send(json!({"t":"event","msg":"MONUMENT RENAMED"}).to_string());
        return;
    }

    // ---- Admin player list ----
    if t == "admin-player-list" {
        if !is_admin { let _ = tx.send(err("Admin only")); return; }
        let players: Vec<serde_json::Value> = world.players.iter()
            .map(|(id, p)| {
                let q = world.queens.get(id);
                let acct = world.auth.users.get(&p.username);
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
                    "nectar":   p.nectar,
                    // Account currency + cosmetics (users.json) — for the admin gem/cosmetic controls.
                    "gems":      acct.map(|u| u.gems).unwrap_or(0),
                    "cosmetics": acct.map(|u| u.owned_cosmetics.len()).unwrap_or(0),
                    "unlimitedNectar":  p.unlimited_nectar,
                    "unlimitedAnts":    p.unlimited_ants,
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
        if !check_seq(world, pid, &msg) { return; }
        use crate::config::{
            PRICE_RELOCATE, PRICE_DEFENDER, PRICE_WORKER,
            PRICE_BRUTE, PRICE_SHIELD, SHIELD_MS, DEFENDER_MS,
        };

        let item = msg["item"].as_str().unwrap_or("").to_string();

        // Server-side unlock gate: reject buys below the item's required peak level. The client also
        // hides locked items, but this is the authoritative boundary (a crafted message can't bypass).
        let uname = world.players.get(&pid).map(|p| p.username.clone());
        let peak  = uname.as_deref().and_then(|u| world.auth.users.get(u)).map(|u| u.peak_level).unwrap_or(0);
        if let Some(req) = crate::config::gate_for_item(&item) {
            if peak < req { let _ = tx.send(err("Locked")); return; }
        }

        let (nectar, unlimited_nectar) = world.players.get(&pid)
            .map(|p| (p.nectar, p.unlimited_nectar)).unwrap_or((0, false));
        let price: u64 = match item.as_str() {
            "relocate" => PRICE_RELOCATE,
            "defender" => PRICE_DEFENDER,
            "worker"   => PRICE_WORKER,
            "brute"    => PRICE_BRUTE,
            "shield"   => PRICE_SHIELD,
            _ => { let _ = tx.send(err("Unknown item")); return; }
        };
        if !unlimited_nectar && nectar < price { let _ = tx.send(err("Not enough nectar")); return; }

        match item.as_str() {
            "relocate" => {
                let old_pos = world.queens.get(&pid).filter(|q| !q.dead).map(|q| (q.x, q.y, q.size, q.bubble_r));
                let Some((ox, oy, sz, my_r)) = old_pos else { let _ = tx.send(err("Need a live queen")); return; };
                let x = msg["x"].as_i64().unwrap_or(-1) as i32;
                let y = msg["y"].as_i64().unwrap_or(-1) as i32;
                let ww = world.world_w as i32; let wh = world.world_h as i32;
                if x < 2 || y < 2 || x >= ww - 8 || y >= wh - 8 { let _ = tx.send(err("Out of bounds")); return; }
                // No-overlap rule: relocating keeps the queen's current bubble — same circle test.
                if world.queen_zone_overlaps(x + sz as i32 / 2, y + sz as i32 / 2, my_r, pid) { let _ = tx.send(err("Too close to another queen")); return; }
                charge(world, pid, price, unlimited_nectar);
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
                charge(world, pid, price, unlimited_nectar);
                if let Some(p) = world.players.get_mut(&pid) { p.defenders.push(expiry); }
                let _ = tx.send(json!({"t":"shop-ok","item":"defender"}).to_string());
            }
            "worker" => {
                // +1 inventory worker — a stock top-up, so no live-queen requirement.
                charge(world, pid, price, unlimited_nectar);
                if let Some(p) = world.players.get_mut(&pid) { p.ants_avail += 1; }
                let _ = tx.send(json!({"t":"shop-ok","item":"worker"}).to_string());
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
                charge(world, pid, price, unlimited_nectar);
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
                charge(world, pid, price, unlimited_nectar);
                // Alliance shield buff: tiers extend the shield's duration (× shield_mult).
                let dur = (SHIELD_MS as f64 * crate::config::alliance_buffs(world.alliance_level(pid)).shield_mult) as u64;
                if let Some(q) = world.queens.get_mut(&pid) {
                    q.shield = q.max_hp;
                    q.shield_expiry = Some(now + dur);
                }
                let _ = tx.send(json!({"t":"shop-ok","item":"shield"}).to_string());
            }
            _ => {}
        }
        return;
    }

    // ---- Cosmetics: buy with gems + equip per slot (account-level, wipe-proof) ----
    if t == "cosmetic-buy" {
        if !check_seq(world, pid, &msg) { return; }
        let id = msg["id"].as_str().unwrap_or("").to_string();
        let Some(cos) = crate::cosmetics::get(&id) else { let _ = tx.send(err("Unknown cosmetic")); return; };
        let Some(uname) = world.players.get(&pid).map(|p| p.username.clone()) else { return; };
        let Some(user) = world.auth.users.get_mut(&uname) else { let _ = tx.send(err("No account")); return; };
        if user.owned_cosmetics.iter().any(|c| c == &id) { let _ = tx.send(err("Already owned")); return; }
        if user.gems < cos.price_gems { let _ = tx.send(err("Not enough gems")); return; }
        user.gems -= cos.price_gems;
        user.owned_cosmetics.push(id.clone());
        world.auth.save();
        let _ = tx.send(json!({"t":"cosmetic-ok","action":"buy","id":id}).to_string());
        let _ = tx.send(build_player_info(world, pid, true, None)); // push fresh gems/owned immediately
        return;
    }
    if t == "cosmetic-equip" {
        if !check_seq(world, pid, &msg) { return; }
        let slot = msg["slot"].as_str().unwrap_or("").to_string();
        let id   = msg["id"].as_str().unwrap_or("").to_string();   // "" = unequip the slot
        if slot.is_empty() { let _ = tx.send(err("Bad slot")); return; }
        let Some(uname) = world.players.get(&pid).map(|p| p.username.clone()) else { return; };
        // Equipping (non-empty id) must name an OWNED cosmetic whose registry slot matches.
        if !id.is_empty() {
            let Some(cos) = crate::cosmetics::get(&id) else { let _ = tx.send(err("Unknown cosmetic")); return; };
            if cos.slot != slot { let _ = tx.send(err("Wrong slot")); return; }
            let owned = world.auth.users.get(&uname)
                .map(|u| u.owned_cosmetics.iter().any(|c| c == &id)).unwrap_or(false);
            if !owned { let _ = tx.send(err("Not owned")); return; }
        }
        if let Some(user) = world.auth.users.get_mut(&uname) {
            if id.is_empty() { user.equipped.remove(&slot); }
            else { user.equipped.insert(slot.clone(), id.clone()); }
        }
        world.auth.save();
        // Mirror the equipped slot onto the live Player so renders update without a reconnect.
        // `recolor` overrides the live colour (equip) / reverts to the account base colour (unequip);
        // the rest feed the fx palette (tile_fx) or the ~1 Hz cosmetics roster (aura/trail).
        if slot == "recolor" {
            let base = world.auth.users.get(&uname).map(|u| u.color.clone()).unwrap_or_default();
            let new_color = if id.is_empty() { base.clone() }
                else { crate::cosmetics::get(&id).map(|c| c.params.to_string())
                       .filter(|s| !s.is_empty()).unwrap_or(base) };
            // The queen's rendered colour is pulled from the player's colour at snapshot time
            // (network.rs), so updating `p.color` recolours tiles, ants, AND the queen together.
            if let Some(p) = world.players.get_mut(&pid) { p.color = new_color; }
        } else if let Some(p) = world.players.get_mut(&pid) {
            let val = if id.is_empty() { None } else { Some(id.clone()) };
            match slot.as_str() {
                "tile_fx"      => p.tile_fx = val,
                "aura"         => p.aura = val,
                "trail"        => p.trail = val,
                _ => {}
            }
        }
        let action = if id.is_empty() { "unequip" } else { "equip" };
        let _ = tx.send(json!({"t":"cosmetic-ok","action":action,"slot":slot,"id":id}).to_string());
        let _ = tx.send(build_player_info(world, pid, true, None));
        return;
    }
    // Bundle purchase: debit once, grant every (not-already-owned) member atomically.
    if t == "bundle-buy" {
        if !check_seq(world, pid, &msg) { return; }
        let id = msg["id"].as_str().unwrap_or("").to_string();
        let Some(bundle) = crate::cosmetics::get_bundle(&id) else { let _ = tx.send(err("Unknown bundle")); return; };
        let Some(uname) = world.players.get(&pid).map(|p| p.username.clone()) else { return; };
        let Some(user) = world.auth.users.get_mut(&uname) else { let _ = tx.send(err("No account")); return; };
        if bundle.members.iter().all(|m| user.owned_cosmetics.iter().any(|o| o == m)) {
            let _ = tx.send(err("Bundle already owned")); return;
        }
        if user.gems < bundle.price_gems { let _ = tx.send(err("Not enough gems")); return; }
        user.gems -= bundle.price_gems;
        for m in bundle.members {
            if !user.owned_cosmetics.iter().any(|o| o == m) { user.owned_cosmetics.push(m.to_string()); }
        }
        world.auth.save();
        let _ = tx.send(json!({"t":"cosmetic-ok","action":"bundle","id":id}).to_string());
        let _ = tx.send(build_player_info(world, pid, true, None));
        return;
    }

    // ---- Alliances (open to everyone for testing; economy gated by cfg.alliance_econ_enabled) ----
    if t == "alliance-get"            { send_alliance_self(world, pid, tx);
                                        let _ = tx.send(crate::network::build_factions(world)); return; }
    if t == "alliance-create"         { alliance_create(world, pid, &msg, tx);          return; }
    if t == "alliance-request"        { alliance_request(world, pid, &msg, tx);         return; }
    if t == "alliance-cancel-request" { alliance_cancel_request(world, pid, &msg, tx);  return; }
    if t == "alliance-accept"         { alliance_decide(world, pid, &msg, tx, true);    return; }
    if t == "alliance-reject"         { alliance_decide(world, pid, &msg, tx, false);   return; }
    if t == "alliance-invite"         { alliance_invite(world, pid, &msg, tx);          return; }
    if t == "alliance-invite-accept"  { alliance_invite_accept(world, pid, &msg, tx);   return; }
    if t == "alliance-invite-decline" { alliance_invite_decline(world, pid, &msg, tx);  return; }
    if t == "alliance-leave"          { alliance_leave(world, pid, tx);                 return; }

    // ---- Admin give nectar ----
    if t == "admin-give-nectar" {
        if !is_admin { let _ = tx.send(err("Admin only")); return; }
        let tid    = msg["targetId"].as_u64().unwrap_or(0) as u32;
        let amount = msg["amount"].as_u64().unwrap_or(0);
        if let Some(tp) = world.players.get_mut(&tid) {
            // saturating: a huge admin amount must not overflow the balance.
            tp.nectar = tp.nectar.saturating_add(amount);
            let new_cr = tp.nectar;
            if let Some(ttx) = &tp.tx {
                let _ = ttx.send(json!({"t":"event","msg":format!("+{amount} NECTAR (ADMIN) · total {new_cr}")}).to_string());
            }
        }
        return;
    }

    // ---- Admin set nectar ----
    if t == "admin-set-nectar" {
        if !is_admin { let _ = tx.send(err("Admin only")); return; }
        let tid    = msg["targetId"].as_u64().unwrap_or(0) as u32;
        let amount = msg["amount"].as_u64().unwrap_or(0);
        if let Some(tp) = world.players.get_mut(&tid) {
            tp.nectar = amount;
            if let Some(ttx) = &tp.tx {
                let _ = ttx.send(json!({"t":"event","msg":format!("NECTAR SET TO {amount} (ADMIN)")}).to_string());
            }
        }
        return;
    }

    // ---- Admin give gems (account currency → users.json, like the cosmetic handlers) ----
    if t == "admin-give-gems" || t == "admin-set-gems" {
        if !is_admin { let _ = tx.send(err("Admin only")); return; }
        let tid    = msg["targetId"].as_u64().unwrap_or(0) as u32;
        let amount = msg["amount"].as_u64().unwrap_or(0);
        let Some(uname) = world.players.get(&tid).map(|p| p.username.clone()) else { return; };
        let new_total = if let Some(u) = world.auth.users.get_mut(&uname) {
            u.gems = if t == "admin-give-gems" { u.gems.saturating_add(amount) } else { amount };
            u.gems
        } else { return };   // NPC / no account → no-op
        world.auth.save();
        let verb = if t == "admin-give-gems" { format!("+{amount} GEMS (ADMIN) · total {new_total}") }
                   else { format!("GEMS SET TO {amount} (ADMIN)") };
        world.send_to(tid, json!({"t":"event","msg":verb}).to_string());
        // Push a fresh `me` so the target's balance/shop reflect it immediately (covers self-grants).
        world.send_to(tid, build_player_info(world, tid, true, None));
        return;
    }

    // ---- Admin grant all cosmetics (one-click test-bed: seed the whole catalogue on a target) ----
    if t == "admin-grant-cosmetics" {
        if !is_admin { let _ = tx.send(err("Admin only")); return; }
        let tid = msg["targetId"].as_u64().unwrap_or(0) as u32;
        let Some(uname) = world.players.get(&tid).map(|p| p.username.clone()) else { return; };
        if let Some(u) = world.auth.users.get_mut(&uname) {
            for c in crate::cosmetics::COSMETICS {
                if !u.owned_cosmetics.iter().any(|o| o == c.id) { u.owned_cosmetics.push(c.id.to_string()); }
            }
        } else { return };   // NPC / no account → no-op
        world.auth.save();
        world.send_to(tid, json!({"t":"event","msg":"ALL COSMETICS GRANTED (ADMIN)"}).to_string());
        world.send_to(tid, build_player_info(world, tid, true, None));
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

/// Grant the daily ant portion for the current 00:00-UTC window. Idempotent: the account's
/// `last_claim_day` (users.json) marks the window claimed — a replay/double-call is rejected, and
/// an unclaimed window is simply forfeited once the next one starts (no carry-over, no stacking).
/// Pure state change; the caller persists via `Auth::save` on success. Returns the portion size.
fn claim_daily(world: &mut World, pid: u32, now: u64) -> Result<i32, &'static str> {
    let Some(p) = world.players.get(&pid) else { return Err("Not logged in"); };
    if p.npc || p.guest { return Err("Spectators cannot claim"); }
    let uname = p.username.clone();
    let day = crate::config::utc_day(now);
    let Some(u) = world.auth.users.get_mut(&uname) else { return Err("No account record"); };
    if u.last_claim_day >= day { return Err("Already claimed — next portion at 00:00 UTC"); }
    u.last_claim_day = day;
    let daily = cfg().daily_ants;
    if let Some(p) = world.players.get_mut(&pid) { p.ants_avail += daily; }
    Ok(daily)
}

// ======================================================================================
// Alliances — server-authoritative create / join / invite / leave + buffs, Mayday, progression.
// Membership lives in users.json (UserRecord.alliance_id + Auth.alliances), so every mutation ends
// with `rebuild_player_alliance` (refresh the hot-path index) + `auth.save` (persist). The economy is
// gated by `cfg.alliance_econ_enabled` — OFF for testing, so create/join are free.
// ======================================================================================

/// True if `pid` can pay `price` nectar (always true when the alliance economy is off / unlimited).
fn can_afford(world: &World, pid: u32, price: u64) -> bool {
    if price == 0 || !cfg().alliance_econ_enabled { return true; }
    world.players.get(&pid).map(|p| p.unlimited_nectar || p.nectar >= price).unwrap_or(false)
}

/// Deduct `price` from `pid` (no-op when the economy is off / unlimited / price 0). Returns false if
/// unaffordable (callers pre-check via `can_afford`, but this keeps the arithmetic safe).
fn alliance_charge(world: &mut World, pid: u32, price: u64) -> bool {
    if price == 0 || !cfg().alliance_econ_enabled { return true; }
    let Some(p) = world.players.get_mut(&pid) else { return false };
    if p.unlimited_nectar { return true; }
    if p.nectar < price { return false; }
    p.nectar = p.nectar.saturating_sub(price);
    true
}

/// Refund `price` to `pid` (mirror of `alliance_charge`; the reject/decline path).
fn alliance_refund(world: &mut World, pid: u32, price: u64) {
    if price == 0 || !cfg().alliance_econ_enabled { return; }
    if let Some(p) = world.players.get_mut(&pid) {
        if !p.unlimited_nectar { p.nectar = p.nectar.saturating_add(price); }
    }
}

/// Resolve a target account id from `playerId` (preferred) or a case-insensitive `name`/handle.
fn resolve_target(world: &World, msg: &Value) -> Option<u32> {
    if let Some(id) = msg.get("playerId").and_then(Value::as_u64) { return Some(id as u32); }
    let name = msg.get("name").and_then(Value::as_str)?.trim().to_string();
    if name.is_empty() { return None; }
    let nl = name.to_lowercase();
    world.auth.users.values()
        .find(|u| u.username.to_lowercase() == nl || u.handle.to_lowercase() == nl)
        .map(|u| u.id)
}

/// Set (or clear) a player's persisted `alliance_id` by id → username lookup (works offline too).
fn set_member_alliance(world: &mut World, pid: u32, aid: Option<u32>) {
    // Prefer the live Player username (the uppercase auth key); fall back to scanning users by id.
    let uname = world.players.get(&pid).map(|p| p.username.clone())
        .or_else(|| world.auth.users.values().find(|u| u.id == pid).map(|u| u.username.clone()));
    if let Some(u) = uname.and_then(|n| world.auth.users.get_mut(&n)) { u.alliance_id = aid; }
}

/// Send `pid` their own alliance state (or `{alliance:null}` when unaffiliated).
fn send_alliance_self(world: &World, pid: u32, tx: &BoundedTx<String>) {
    let s = match world.alliance_of(pid) {
        Some(aid) => crate::network::build_alliance_state(world, aid),
        None      => json!({"t":"alliance-state","alliance":null}).to_string(),
    };
    let _ = tx.send(s);
}

/// Push the full alliance state to every member (after any roster/level change).
pub fn broadcast_alliance_state(world: &mut World, aid: u32) {
    let state = crate::network::build_alliance_state(world, aid);
    let members = world.auth.alliances.get(&aid).map(|a| a.members.clone()).unwrap_or_default();
    for m in members { world.send_to(m, state.clone()); }
}

/// Broadcast the faction leaderboard / recolor roster to everyone (small, infrequent).
fn push_factions(world: &World) { world.broadcast(&crate::network::build_factions(world)); }

fn alliance_create(world: &mut World, pid: u32, msg: &Value, tx: &BoundedTx<String>) {
    use crate::config::{PRICE_ALLIANCE_CREATE, valid_hex_color};
    if world.alliance_of(pid).is_some() { let _ = tx.send(err("You're already in an alliance")); return; }
    if world.players.get(&pid).map(|p| p.npc || p.guest).unwrap_or(true) {
        let _ = tx.send(err("An account is required")); return;
    }
    let Some(name) = crate::alliance::sanitize_name(msg["name"].as_str().unwrap_or("")) else {
        let _ = tx.send(err("Pick an alliance name")); return;
    };
    let icon = msg["icon"].as_str().unwrap_or("").to_string();
    if !crate::alliance::valid_icon(&icon) { let _ = tx.send(err("Pick a valid icon")); return; }
    // Alliance colour is no longer user-facing (members keep their own colours); the client sends
    // none. Keep a neutral stored default so existing readers of `al.color` stay valid.
    let raw_color = msg["color"].as_str().unwrap_or("").to_string();
    let color = if valid_hex_color(&raw_color) { raw_color } else { "#8a8a8a".to_string() };
    if !can_afford(world, pid, PRICE_ALLIANCE_CREATE) { let _ = tx.send(err("Not enough nectar")); return; }
    alliance_charge(world, pid, PRICE_ALLIANCE_CREATE);

    world.auth.next_alliance_id += 1;
    let aid = world.auth.next_alliance_id;
    let al = crate::alliance::Alliance {
        id: aid, name: name.clone(), icon, color, leader_id: pid,
        members: vec![pid], requests: Vec::new(), invites: Vec::new(),
        xp: 0.0, created_at: current_ms(),
    };
    world.auth.alliances.insert(aid, al);
    set_member_alliance(world, pid, Some(aid));
    world.rebuild_player_alliance();
    world.auth.save();
    recompute_alliance_auras(world);
    world.send_to(pid, json!({"t":"event","msg":format!("ALLIANCE \"{name}\" FOUNDED")}).to_string());
    send_alliance_self(world, pid, tx);
    broadcast_alliance_state(world, aid);
    push_factions(world);
}

fn alliance_request(world: &mut World, pid: u32, msg: &Value, tx: &BoundedTx<String>) {
    use crate::config::PRICE_ALLIANCE_JOIN;
    if world.alliance_of(pid).is_some() { let _ = tx.send(err("You're already in an alliance")); return; }
    if world.players.get(&pid).map(|p| p.npc || p.guest).unwrap_or(true) {
        let _ = tx.send(err("An account is required")); return;
    }
    let aid = msg["allianceId"].as_u64().unwrap_or(0) as u32;
    let (full, dup, leader, name) = match world.auth.alliances.get(&aid) {
        Some(al) => (al.is_full(), al.requests.contains(&pid), al.leader_id, al.name.clone()),
        None => { let _ = tx.send(err("No such alliance")); return; }
    };
    if full { let _ = tx.send(err("That alliance is full")); return; }
    if dup  { let _ = tx.send(err("Request already pending")); return; }
    if !can_afford(world, pid, PRICE_ALLIANCE_JOIN) { let _ = tx.send(err("Not enough nectar")); return; }
    alliance_charge(world, pid, PRICE_ALLIANCE_JOIN);
    if let Some(al) = world.auth.alliances.get_mut(&aid) { al.requests.push(pid); }
    world.auth.save();
    let applicant = world.players.get(&pid).map(|p| p.username.clone()).unwrap_or_default();
    world.send_to(pid, json!({"t":"event","msg":format!("Requested to join \"{name}\"")}).to_string());
    world.send_to(leader, json!({"t":"event","msg":format!("{applicant} wants to join your alliance")}).to_string());
    send_alliance_self(world, pid, tx);
    broadcast_alliance_state(world, aid);
}

fn alliance_cancel_request(world: &mut World, pid: u32, _msg: &Value, tx: &BoundedTx<String>) {
    use crate::config::PRICE_ALLIANCE_JOIN;
    // Find any alliance that lists pid as an applicant; remove + refund.
    let aid = world.auth.alliances.iter()
        .find(|(_, al)| al.requests.contains(&pid)).map(|(&aid, _)| aid);
    let Some(aid) = aid else { let _ = tx.send(err("No pending request")); return; };
    if let Some(al) = world.auth.alliances.get_mut(&aid) { al.requests.retain(|&r| r != pid); }
    alliance_refund(world, pid, PRICE_ALLIANCE_JOIN);
    world.auth.save();
    world.send_to(pid, json!({"t":"event","msg":"Request cancelled"}).to_string());
    send_alliance_self(world, pid, tx);
    broadcast_alliance_state(world, aid);
}

fn alliance_decide(world: &mut World, pid: u32, msg: &Value, tx: &BoundedTx<String>, accept: bool) {
    use crate::config::PRICE_ALLIANCE_JOIN;
    let applicant = msg["playerId"].as_u64().unwrap_or(0) as u32;
    let Some(aid) = world.alliance_of(pid) else { let _ = tx.send(err("You're not in an alliance")); return; };
    let (is_leader, has_req, full, name) = match world.auth.alliances.get(&aid) {
        Some(al) => (al.leader_id == pid, al.requests.contains(&applicant), al.is_full(), al.name.clone()),
        None => { let _ = tx.send(err("No such alliance")); return; }
    };
    if !is_leader { let _ = tx.send(err("Only the leader can do that")); return; }
    if !has_req   { let _ = tx.send(err("No such request")); return; }
    if accept {
        // The applicant may have joined elsewhere meanwhile, or the alliance filled up.
        if world.alliance_of(applicant).is_some() || full {
            if let Some(al) = world.auth.alliances.get_mut(&aid) { al.requests.retain(|&r| r != applicant); }
            alliance_refund(world, applicant, PRICE_ALLIANCE_JOIN);
            world.auth.save();
            let _ = tx.send(err(if full { "Alliance is full" } else { "Applicant is unavailable" }));
            broadcast_alliance_state(world, aid);
            return;
        }
        if let Some(al) = world.auth.alliances.get_mut(&aid) {
            al.requests.retain(|&r| r != applicant);
            al.members.push(applicant);
        }
        set_member_alliance(world, applicant, Some(aid));
        world.rebuild_player_alliance();
        world.auth.save();
        recompute_alliance_auras(world);
        world.send_to(applicant, json!({"t":"event","msg":format!("You joined \"{name}\"!")}).to_string());
        broadcast_alliance_state(world, aid);
        push_factions(world);
    } else {
        if let Some(al) = world.auth.alliances.get_mut(&aid) { al.requests.retain(|&r| r != applicant); }
        alliance_refund(world, applicant, PRICE_ALLIANCE_JOIN);
        world.auth.save();
        world.send_to(applicant, json!({"t":"event","msg":format!("Your request to \"{name}\" was declined")}).to_string());
        broadcast_alliance_state(world, aid);
    }
}

fn alliance_invite(world: &mut World, pid: u32, msg: &Value, tx: &BoundedTx<String>) {
    use crate::config::PRICE_ALLIANCE_JOIN;
    let Some(aid) = world.alliance_of(pid) else { let _ = tx.send(err("You're not in an alliance")); return; };
    let Some(target) = resolve_target(world, msg) else { let _ = tx.send(err("No such player")); return; };
    if target == pid { let _ = tx.send(err("You can't invite yourself")); return; }
    let (is_leader, full, dup, name) = match world.auth.alliances.get(&aid) {
        Some(al) => (al.leader_id == pid, al.is_full(), al.invites.contains(&target), al.name.clone()),
        None => return,
    };
    if !is_leader { let _ = tx.send(err("Only the leader can invite")); return; }
    if full       { let _ = tx.send(err("Your alliance is full")); return; }
    if dup        { let _ = tx.send(err("Already invited")); return; }
    if world.alliance_of(target).is_some() { let _ = tx.send(err("They're already in an alliance")); return; }
    if world.auth.users.values().all(|u| u.id != target) { let _ = tx.send(err("No such account")); return; }
    if !can_afford(world, pid, PRICE_ALLIANCE_JOIN) { let _ = tx.send(err("Not enough nectar")); return; }
    alliance_charge(world, pid, PRICE_ALLIANCE_JOIN);   // the inviter (leader) pays
    if let Some(al) = world.auth.alliances.get_mut(&aid) { al.invites.push(target); }
    world.auth.save();
    world.send_to(target, json!({"t":"alliance-invite","allianceId":aid,"name":name}).to_string());
    world.send_to(target, json!({"t":"event","msg":format!("You've been invited to \"{name}\"")}).to_string());
    world.send_to(pid, json!({"t":"event","msg":"Invite sent"}).to_string());
    broadcast_alliance_state(world, aid);
}

fn alliance_invite_accept(world: &mut World, pid: u32, msg: &Value, tx: &BoundedTx<String>) {
    let aid = msg["allianceId"].as_u64().unwrap_or(0) as u32;
    if world.alliance_of(pid).is_some() { let _ = tx.send(err("You're already in an alliance")); return; }
    let (invited, full, name) = match world.auth.alliances.get(&aid) {
        Some(al) => (al.invites.contains(&pid), al.is_full(), al.name.clone()),
        None => { let _ = tx.send(err("That alliance no longer exists")); return; }
    };
    if !invited { let _ = tx.send(err("No invite from that alliance")); return; }
    if full     { let _ = tx.send(err("That alliance is full")); return; }
    if let Some(al) = world.auth.alliances.get_mut(&aid) {
        al.invites.retain(|&i| i != pid);
        al.members.push(pid);
    }
    set_member_alliance(world, pid, Some(aid));
    world.rebuild_player_alliance();
    world.auth.save();
    recompute_alliance_auras(world);
    world.send_to(pid, json!({"t":"event","msg":format!("You joined \"{name}\"!")}).to_string());
    send_alliance_self(world, pid, tx);
    broadcast_alliance_state(world, aid);
    push_factions(world);
}

fn alliance_invite_decline(world: &mut World, pid: u32, msg: &Value, tx: &BoundedTx<String>) {
    use crate::config::PRICE_ALLIANCE_JOIN;
    let aid = msg["allianceId"].as_u64().unwrap_or(0) as u32;
    let leader = match world.auth.alliances.get_mut(&aid) {
        Some(al) => { al.invites.retain(|&i| i != pid); al.leader_id }
        None => { let _ = tx.send(err("That alliance no longer exists")); return; }
    };
    alliance_refund(world, leader, PRICE_ALLIANCE_JOIN);   // the inviter's fee comes back
    world.auth.save();
    world.send_to(pid, json!({"t":"event","msg":"Invite declined"}).to_string());
    send_alliance_self(world, pid, tx);
    broadcast_alliance_state(world, aid);
}

fn alliance_leave(world: &mut World, pid: u32, tx: &BoundedTx<String>) {
    let Some(aid) = world.alliance_of(pid) else { let _ = tx.send(err("You're not in an alliance")); return; };
    let (disbanded, name) = {
        let Some(al) = world.auth.alliances.get_mut(&aid) else { return; };
        al.members.retain(|&m| m != pid);
        al.requests.retain(|&m| m != pid);
        al.invites.retain(|&m| m != pid);
        let name = al.name.clone();
        if al.members.is_empty() { (true, name) }
        else { if al.leader_id == pid { al.leader_id = al.members[0]; } (false, name) }
    };
    set_member_alliance(world, pid, None);
    if disbanded { world.auth.alliances.remove(&aid); }
    world.rebuild_player_alliance();
    world.auth.save();
    recompute_alliance_auras(world);   // the leaver loses buffs; survivors may lose a Phalanx stack
    world.send_to(pid, json!({"t":"event","msg":format!("You left \"{name}\"")}).to_string());
    send_alliance_self(world, pid, tx);   // pushes {alliance:null} to the leaver
    if !disbanded { broadcast_alliance_state(world, aid); }
    push_factions(world);
}

/// Add combined-contribution XP to `pid`'s alliance. On a (rare) tier change it re-applies member
/// buffs, notifies the roster, refreshes the faction board, and persists. No-op for the unaffiliated.
/// XP between tier-ups is intentionally NOT flushed to disk per call (kills can be frequent) — it
/// rides the next membership change / tier-up save; losing a little progress on a crash is acceptable.
pub fn award_alliance_xp(world: &mut World, pid: u32, amount: f64) {
    if amount <= 0.0 { return; }
    let Some(aid) = world.alliance_of(pid) else { return; };
    let (old_lvl, new_lvl, name) = {
        let Some(al) = world.auth.alliances.get_mut(&aid) else { return; };
        let old = al.level();
        al.xp += amount;
        (old, al.level(), al.name.clone())
    };
    if new_lvl != old_lvl {
        world.auth.save();
        recompute_alliance_auras(world);
        let msg = json!({"t":"event","msg":format!("ALLIANCE \"{name}\" REACHED TIER {new_lvl}!")}).to_string();
        let members = world.auth.alliances.get(&aid).map(|a| a.members.clone()).unwrap_or_default();
        for m in members { world.send_to(m, msg.clone()); }
        broadcast_alliance_state(world, aid);
        push_factions(world);
    }
}

/// Daily combined-contribution accrual: each alliance gains XP for the tiles its members collectively
/// hold (`tiles / 100k × alliance_xp_per_100k_day`). Called once per UTC day from the sim loop.
pub fn accrue_alliance_territory(world: &mut World) {
    let rate = cfg().alliance_xp_per_100k_day;
    if rate <= 0.0 { return; }
    let aids: Vec<u32> = world.auth.alliances.keys().copied().collect();
    for aid in aids {
        let members = world.auth.alliances.get(&aid).map(|a| a.members.clone()).unwrap_or_default();
        let Some(&anchor) = members.first() else { continue };
        let tiles: u64 = members.iter()
            .map(|m| world.tiles.counts.get(m).copied().unwrap_or(0).max(0) as u64)
            .sum();
        let gain = (tiles as f64 / 100_000.0) * rate;
        if gain > 0.0 { award_alliance_xp(world, anchor, gain); }
    }
    world.auth.save();   // persist the day's XP (award_alliance_xp only saves on a tier change)
}

/// United Front: recompute every member queen's Phalanx stacks (allied queens within `phalanx_r`)
/// and buffed `max_hp` (tier hp_mult + Phalanx hp bonus), and reset any non-member queen back to its
/// level base. Runs on the holders cadence + immediately after a membership change. Bounded work
/// (≤ member cap per alliance; one O(queens) reset sweep).
pub fn recompute_alliance_auras(world: &mut World) {
    let c = cfg().clone();
    let pr2 = (c.phalanx_r * c.phalanx_r) as i64;
    let per = c.phalanx_per_stack;
    let cap = c.phalanx_cap;
    world.phalanx_stacks.clear();

    let alliances: Vec<(u16, Vec<u32>)> = world.auth.alliances.values()
        .map(|al| (al.level(), al.members.clone())).collect();

    for (level, members) in &alliances {
        let buffs = crate::config::alliance_buffs(*level);
        // (id, centre x, centre y) of each live member queen.
        let qpos: Vec<(u32, i32, i32)> = members.iter().filter_map(|&m| {
            world.queens.get(&m).filter(|q| !q.dead)
                .map(|q| (m, q.x + q.size as i32 / 2, q.y + q.size as i32 / 2))
        }).collect();
        if buffs.phalanx {
            for &(id, x, y) in &qpos {
                let mut stacks = 0u8;
                for &(oid, ox, oy) in &qpos {
                    if oid == id { continue; }
                    let dx = (x - ox) as i64; let dy = (y - oy) as i64;
                    if dx * dx + dy * dy <= pr2 { stacks = stacks.saturating_add(1); }
                }
                if stacks > 0 { world.phalanx_stacks.insert(id, stacks); }
            }
        }
        for &(id, _, _) in &qpos {
            let stacks = world.phalanx_stacks.get(&id).copied().unwrap_or(0) as f64;
            let phalanx_hp = if buffs.phalanx { (stacks * per).min(cap) } else { 0.0 };
            let mult = buffs.hp_mult + phalanx_hp;
            if let Some(q) = world.queens.get_mut(&id) {
                let target = (((crate::config::max_hp_for_level(q.level, &c) as f64) * mult).round() as i32).max(1);
                if q.max_hp != target { q.max_hp = target; if q.hp > q.max_hp { q.hp = q.max_hp; } }
            }
        }
    }

    // Reset queens that are NOT in any alliance back to their level base (they may carry a stale buff
    // from before they left).
    let mut buffed: std::collections::HashSet<u32> = std::collections::HashSet::new();
    for (_, members) in &alliances { for &m in members { buffed.insert(m); } }
    for (&id, q) in world.queens.iter_mut() {
        if buffed.contains(&id) || q.dead { continue; }
        let base = crate::config::max_hp_for_level(q.level, &c);
        if q.max_hp != base { q.max_hp = base; if q.hp > q.max_hp { q.hp = q.max_hp; } }
    }
}

/// MAYDAY: when a member queen under enemy fire drops to/below `cfg.mayday_hp_pct`, ping the whole
/// alliance (shared-map marker + events line). Throttled to one alert per queen per 30 s. No-op for
/// unaffiliated queens or when the feature is disabled (pct == 0).
pub fn maybe_mayday(world: &mut World, queen_id: u32, qx: i32, qy: i32) {
    let pct = cfg().mayday_hp_pct;
    if pct <= 0.0 { return; }
    let Some(aid) = world.alliance_of(queen_id) else { return; };
    let (hp, maxhp) = match world.queens.get(&queen_id) {
        Some(q) if !q.dead => (q.hp as f64, q.max_hp.max(1) as f64),
        _ => return,
    };
    if hp / maxhp > pct { return; }
    let now = current_ms();
    if now.saturating_sub(world.mayday_last.get(&queen_id).copied().unwrap_or(0)) < 30_000 { return; }
    world.mayday_last.insert(queen_id, now);
    let name = world.players.get(&queen_id).map(|p| p.username.clone()).unwrap_or_default();
    let hp_pct = ((hp / maxhp) * 100.0).round() as i32;
    let msg = json!({"t":"mayday","queenId":queen_id,"x":qx,"y":qy,"name":name,"allianceId":aid,"hpPct":hp_pct}).to_string();
    let members = world.auth.alliances.get(&aid).map(|a| a.members.clone()).unwrap_or_default();
    for m in members { world.send_to(m, msg.clone()); }
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
    // Enemy spawn zones are absolute no-deploy areas — even on your own painted tiles.
    if world.too_close_to_queen(x, y, pid) {
        return Err("Too close to an enemy queen");
    }
    let dxq = (x - (qx + qs as i32 / 2)) as i64;
    let dyq = (y - (qy + qs as i32 / 2)) as i64;
    let in_bubble = ((dxq * dxq + dyq * dyq) as f64).sqrt() <= bubble_r;
    let tile = world.tiles.get(x as u32, y as u32);
    // Allied tiles count as your own ground (cross-placement): an ally's territory is placeable
    // anywhere, and an ally's tile inside your bubble is fine too.
    if in_bubble {
        if tile != 0 && !world.is_friendly(pid, tile) {
            return Err("Enemy tile inside bubble");
        }
    } else if !world.is_friendly(pid, tile) {
        return Err("Place inside your bubble or on your territory");
    }
    const DIRS: [(i8, i8); 4] = [(0, -1), (1, 0), (0, 1), (-1, 0)];
    Ok(DIRS.iter().copied().find(|&(a, b)| a == vdx && b == vdy).unwrap_or((0, -1)))
}

/// Record whether this connection negotiated the Phase-3 binary/compressed protocol. The client
/// advertises `{"bin":1}` in its login/register payload; we AND it with the server master flag so
/// `HIVE_BIN_CTL=0` forces legacy text for everyone. A client (e.g. an old tab after a redeploy)
/// that omits the flag keeps the legacy text/JSON protocol transparently.
fn apply_bin_cap(world: &mut World, id: u32, msg: &Value) {
    let bin = msg.get("bin").and_then(Value::as_i64).unwrap_or(0) != 0
        && crate::config::bin_ctl_enabled();
    if let Some(p) = world.players.get_mut(&id) { p.bin = bin; }
}

/// Silently raise a user's persisted `peak_level` to at least their current live-queen level. Used
/// on login so accounts that were already high-level before the unlock system shipped get all their
/// unlocked chrome immediately, WITHOUT firing the one-time unlock popups or starter nectar (those
/// fire only through `flush_xp` on a genuine level-up). No-op once peak ≥ current level.
fn backfill_peak_level(world: &mut World, id: u32) {
    let lvl = world.queens.get(&id).map(|q| q.level).unwrap_or(0);
    let Some(uname) = world.players.get(&id).map(|p| p.username.clone()) else { return };
    let changed = if let Some(u) = world.auth.users.get_mut(&uname) {
        if lvl > u.peak_level { u.peak_level = lvl; true } else { false }
    } else { false };
    if changed { world.auth.save(); }
}

fn create_or_reconnect_player(
    world: &mut World,
    id: u32, username: &str, color: &str, hue_idx: i32,
    tx: BoundedTx<String>,
) -> Option<String> {
    use crate::config::current_ms;
    // Anti-replay: a (re)connect begins a fresh client command-seq stream (page reload resets the
    // client counter), so reset the server-side last-seq for this player.
    world.last_seq.remove(&id);
    let c = cfg();
    let daily = c.daily_ants;
    drop(c);
    let now = current_ms();

    // Metro list for the header region switcher (name + centre tile to fly to).
    let _ = tx.send(json!({"t":"regions","metros":crate::regions::metros_json()}).to_string());
    // Monument markers + their current holders, so a fresh client renders them at once (don't wait
    // for the next ~10 s holder-broadcast cycle). Tiny — only sent when any monument exists.
    if !world.monuments.is_empty() {
        let _ = tx.send(crate::network::build_monuments(world));
    }

    // Equipped cosmetics (wipe-proof account state) → live runtime caches so they reach every viewer
    // (tile_fx via the fx palette; aura/trail/emblem via the ~1 Hz roster). A `recolor` overrides the
    // player's colour. All computed up front in a block so the &auth borrow drops before &mut players.
    let (tile_fx, aura, trail, eff_color) = {
        let eq = world.auth.users.get(username).map(|u| &u.equipped);
        let get = |slot: &str| eq.and_then(|e| e.get(slot).cloned());
        let eff_color = eq.and_then(|e| e.get("recolor"))
            .and_then(|id| crate::cosmetics::get(id))
            .filter(|c| c.category == "recolor" && !c.params.is_empty())
            .map(|c| c.params.to_string())
            .unwrap_or_else(|| color.to_string());
        (get("tile_fx"), get("aura"), get("trail"), eff_color)
    };

    if world.players.contains_key(&id) {
        // Reconnect: reattach + bump conn_gen, and take the disconnect snapshot for welcome-back.
        let away = {
            let p = world.players.get_mut(&id).unwrap();
            p.conn_gen += 1;
            p.tx = Some(tx);
            p.tile_fx = tile_fx;
            p.aura = aura; p.trail = trail;
            p.color = eff_color;
            p.away.take()
        };
        println!("[reconnect] {username} ({})", id);
        return away.and_then(|s| build_welcome_back(world, id, &s, now));
    }
    world.players.insert(id, Player {
        id, username: username.to_string(), color: eff_color,
        hue_idx,
        ants_avail: daily,
        next_refill: now + 24 * 3600 * 1000,
        tx: Some(tx),
        conn_gen: 1,
        tile_fx, aura, trail,
        ..Default::default()
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

    #[test]
    fn worker_placement_rejects_enemy_queen_zone() {
        let (mut w, pid) = world_with_queen();
        let q = (1000, 1000, 2u8, 30.0);
        // A rival queen at (2000,1000), bubble 30 (centre ≈ (2001,1001)).
        w.queens.insert(200, Queen { x: 2000, y: 1000, size: 2, hp: 100, max_hp: 100,
            level: 1, xp: 0.0, kills: 0, bubble_r: 30.0, last_attacker: None, dead: false,
            tiles_ever_held: 0, cached_tiles: 0, npc: false, shield: 0, shield_expiry: None,
            region: String::new() });
        w.queen_map_dirty = true;
        // Inside the enemy bubble → rejected, even before territory rules apply.
        assert_eq!(validate_worker_placement(&mut w, pid, (2010, 1000), (0, -1), q),
                   Err("Too close to an enemy queen"));
        // Even on the player's OWN painted tile inside the enemy bubble.
        w.tiles.set(2015, 1001, pid);
        assert_eq!(validate_worker_placement(&mut w, pid, (2015, 1001), (0, -1), q),
                   Err("Too close to an enemy queen"));
        // Just outside the enemy bubble on an owned tile → legal.
        w.tiles.set(2040, 1001, pid);
        assert_eq!(validate_worker_placement(&mut w, pid, (2040, 1001), (0, -1), q), Ok((0, -1)));
        // A dead enemy queen no longer projects a zone.
        w.queens.get_mut(&200).unwrap().dead = true;
        w.queen_map_dirty = true;
        w.tiles.set(2015, 1001, 0);
        assert_eq!(validate_worker_placement(&mut w, pid, (2015, 1001), (0, -1), q),
                   Err("Place inside your bubble or on your territory"),
                   "dead queen's zone gone — falls through to the territory rule");
    }

    #[test]
    fn queen_zones_can_never_intersect() {
        let (w, pid) = world_with_queen(); // queen centre ≈ (1001,1001), bubble 30
        // Point test (worker rule) — strictly inside vs outside the bubble edge.
        assert!(w.too_close_to_queen(1020, 1001, 999), "point inside bubble");
        assert!(!w.too_close_to_queen(1035, 1001, 999), "point outside bubble");
        // Circle-overlap (queen placement): a second 30-radius bubble needs ≥60 separation.
        assert!(w.queen_zone_overlaps(1055, 1001, 30.0, 999), "54 apart < 30+30 → bubbles overlap");
        assert!(!w.queen_zone_overlaps(1065, 1001, 30.0, 999), "64 apart ≥ 30+30 → legal");
        // The placing player's own queen never blocks them (relocate case).
        assert!(!w.queen_zone_overlaps(1001, 1001, 30.0, pid));
    }

    #[test]
    fn daily_claim_grants_once_per_utc_window() {
        const DAY: u64 = 86_400_000;
        let mut w = World::new();
        let pid = 7u32;
        w.players.insert(pid, Player { id: pid, username: "BOB".into(), ..Default::default() });
        w.auth.users.insert("BOB".into(), UserRecord { id: pid, username: "BOB".into(), ..Default::default() });
        let now = 20_000 * DAY + 5_000;   // a few seconds into an arbitrary UTC day
        let daily = cfg().daily_ants;

        // First claim grants exactly one portion.
        assert_eq!(claim_daily(&mut w, pid, now), Ok(daily));
        assert_eq!(w.players[&pid].ants_avail, daily);
        // Replay / double-click anywhere in the same window: rejected, nothing granted.
        assert!(claim_daily(&mut w, pid, now).is_err());
        assert!(claim_daily(&mut w, pid, (20_001 * DAY) - 1).is_err(), "23:59:59.999 same window");
        assert_eq!(w.players[&pid].ants_avail, daily);
        // The next window opens at 00:00 UTC sharp.
        assert_eq!(claim_daily(&mut w, pid, 20_001 * DAY), Ok(daily));
        // Skipping windows forfeits them — three days later still yields ONE portion.
        assert_eq!(claim_daily(&mut w, pid, 20_004 * DAY + 123), Ok(daily));
        assert!(claim_daily(&mut w, pid, 20_004 * DAY + 999).is_err());
        assert_eq!(w.players[&pid].ants_avail, 3 * daily);

        // No account record / not logged in → no grant.
        assert!(claim_daily(&mut w, 999, now).is_err());
        // Guests can never claim.
        w.players.insert(8, Player { id: 8, username: "BOB".into(), guest: true, ..Default::default() });
        assert!(claim_daily(&mut w, 8, 30_000 * DAY).is_err());
    }

    #[test]
    fn seq_rejects_replay_and_out_of_order() {
        let mut w = World::new();
        let pid = 7u32;
        assert!(check_seq(&mut w, pid, &serde_json::json!({"seq": 1})));
        assert!(check_seq(&mut w, pid, &serde_json::json!({"seq": 2})));
        assert!(!check_seq(&mut w, pid, &serde_json::json!({"seq": 2})), "duplicate rejected");
        assert!(!check_seq(&mut w, pid, &serde_json::json!({"seq": 1})), "out-of-order rejected");
        assert!(check_seq(&mut w, pid, &serde_json::json!({"seq": 3})), "advancing seq accepted");
        assert!(check_seq(&mut w, pid, &serde_json::json!({})), "legacy (no seq) accepted unprotected");
        w.last_seq.remove(&pid); // a (re)connect resets the stream
        assert!(check_seq(&mut w, pid, &serde_json::json!({"seq": 1})), "post-reset low seq accepted");
    }
}
