//! World + account persistence: gzip-compressed JSON snapshots so a restart (e.g. a Railway
//! redeploy) restores the map, queens, players, and territory instead of starting empty.
//!
//! Two files, both under the directory from `HIVE_DATA_DIR` (see `config::lock`):
//!   - `world.snapshot` — this module: tiles, ants, queens, players (durable fields), counters.
//!   - `users.json`     — `Auth::save`/`Auth::load`: accounts + bans.
//!
//! Writes are **atomic** (temp file + rename) so a crash mid-write can't corrupt the live
//! snapshot. Reads are best-effort: a missing / corrupt / wrong-version file yields `None` and the
//! caller starts fresh rather than crashing.
//!
//! Note: `save` serializes under whatever lock the caller holds (the sim thread's write lock).
//! That briefly pauses the tick; fine at current scale. At planet scale this should move to a
//! clone-then-serialize-off-lock pattern like the viewport thread (and/or JSON → bincode).

use std::fs;
use std::io::{self, BufReader, BufWriter, Write};

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};

use crate::tile_map::TileMap;
use crate::world::{Ant, Player, Queen, World};

/// Bump when the on-disk layout changes incompatibly; older snapshots are then ignored.
const SNAPSHOT_VERSION: u32 = 1;

/// Borrowing view of the world used **only for saving** — avoids cloning the (potentially large)
/// tile map / ant vec just to serialize them. Field names must match `WorldSnapshot` so the JSON
/// round-trips.
#[derive(Serialize)]
struct WorldSnapshotRef<'a> {
    version:        u32,
    tiles:          &'a TileMap,
    ants:           &'a [Ant],
    queens:         &'a FxHashMap<u32, Queen>,
    players:        Vec<PlayerSnapshot>,
    next_player_id: u32,
    tick:           u64,
    started_at:     u64,
}

/// Owning form used **only for loading**.
#[derive(Deserialize)]
pub struct WorldSnapshot {
    version:        u32,
    tiles:          TileMap,
    ants:           Vec<Ant>,
    queens:         FxHashMap<u32, Queen>,
    players:        Vec<PlayerSnapshot>,
    next_player_id: u32,
    tick:           u64,
    started_at:     u64,
}

/// Durable subset of `Player` — everything except live connection / runtime state (channels,
/// viewport, conn_gen, away snapshot), which is re-established when the player reconnects.
#[derive(Serialize, Deserialize)]
struct PlayerSnapshot {
    id:                  u32,
    username:            String,
    color:               String,
    hue_idx:             i32,
    ants_avail:          i32,
    next_refill:         u64,
    queen_placed_at:     Option<u64>,
    npc:                 bool,
    prestige:            u32,
    credits:             u64,
    defenders:           Vec<u64>,
    visited_countries:   FxHashSet<String>,
    visited_continents:  FxHashSet<String>,
    lifetime_kills:      u32,
    lifetime_peak_tiles: u64,
    queens_fielded:      u32,
}

impl PlayerSnapshot {
    fn from_player(p: &Player) -> Self {
        PlayerSnapshot {
            id:                  p.id,
            username:            p.username.clone(),
            color:               p.color.clone(),
            hue_idx:             p.hue_idx,
            ants_avail:          p.ants_avail,
            next_refill:         p.next_refill,
            queen_placed_at:     p.queen_placed_at,
            npc:                 p.npc,
            prestige:            p.prestige,
            credits:             p.credits,
            defenders:           p.defenders.clone(),
            visited_countries:   p.visited_countries.clone(),
            visited_continents:  p.visited_continents.clone(),
            lifetime_kills:      p.lifetime_kills,
            lifetime_peak_tiles: p.lifetime_peak_tiles,
            queens_fielded:      p.queens_fielded,
        }
    }

    fn into_player(self) -> Player {
        Player {
            id:                  self.id,
            username:            self.username,
            color:               self.color,
            hue_idx:             self.hue_idx,
            ants_avail:          self.ants_avail,
            next_refill:         self.next_refill,
            queen_placed_at:     self.queen_placed_at,
            npc:                 self.npc,
            view:                None,
            tx:                  None,
            view_tx:             None,
            ctl_tx:              None,
            bin:                 false,
            conn_gen:            0,
            prestige:            self.prestige,
            credits:             self.credits,
            defenders:           self.defenders,
            visited_countries:   self.visited_countries,
            visited_continents:  self.visited_continents,
            lifetime_kills:      self.lifetime_kills,
            lifetime_peak_tiles: self.lifetime_peak_tiles,
            queens_fielded:      self.queens_fielded,
            away:                None,
        }
    }
}

/// Persist the world to `path` (gzip JSON, atomic temp+rename) and the accounts to `users.json`.
pub fn save(world: &World, path: &str) -> io::Result<()> {
    let snap = WorldSnapshotRef {
        version:        SNAPSHOT_VERSION,
        tiles:          &world.tiles,
        ants:           &world.ants,
        queens:         &world.queens,
        players:        world.players.values().map(PlayerSnapshot::from_player).collect(),
        next_player_id: world.next_player_id,
        tick:           world.tick,
        started_at:     world.started_at,
    };

    let tmp = format!("{path}.tmp");
    {
        let f = fs::File::create(&tmp)?;
        let mut enc = GzEncoder::new(BufWriter::new(f), Compression::default());
        serde_json::to_writer(&mut enc, &snap)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        // finish() flushes the gzip trailer into the BufWriter; then flush the BufWriter to disk.
        let mut w = enc.finish()?;
        w.flush()?;
    }
    fs::rename(&tmp, path)?;

    // Accounts persist alongside the world so both files move together.
    world.auth.save();
    Ok(())
}

/// Load a world snapshot from `path`. Returns `None` if the file is absent, unreadable, corrupt,
/// or written by an incompatible version — the caller then starts from an empty world.
pub fn load(path: &str) -> Option<WorldSnapshot> {
    let f = fs::File::open(path).ok()?;
    let dec = GzDecoder::new(BufReader::new(f));
    let snap: WorldSnapshot = serde_json::from_reader(dec).ok()?;
    if snap.version != SNAPSHOT_VERSION {
        eprintln!(
            "[persist] ignoring snapshot: version {} != {SNAPSHOT_VERSION}",
            snap.version
        );
        return None;
    }
    Some(snap)
}

/// Move a loaded snapshot into `world`, replacing tiles / ants / queens / players / counters.
/// Runtime-only state (queen map, dirty flag) is reset so the next tick rebuilds it.
pub fn restore(world: &mut World, snap: WorldSnapshot) {
    let queens = snap.queens.len();
    let players = snap.players.len();
    let ants = snap.ants.len();

    world.tiles          = snap.tiles;
    world.ants           = snap.ants;
    world.queens         = snap.queens;
    world.players        = snap.players.into_iter().map(|ps| (ps.id, ps.into_player())).collect();
    world.next_player_id = snap.next_player_id;
    world.tick           = snap.tick;
    world.started_at     = snap.started_at;

    world.queen_map.clear();
    world.queen_map_dirty = true;
    world.dirty_tick = world.tick;

    let tiles = world.tiles.total_tiles();
    println!("[persist] restored: {tiles} tiles, {queens} queens, {players} players, {ants} ants (tick {})", world.tick);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::cfg_write;

    #[test]
    fn save_load_restore_round_trip() {
        // A world with painted tiles (incl. a solid chunk that compacts to Uniform), a queen,
        // a player, and an ant — exercises every serialized field + the TileMap serde derives.
        let mut w = World::new();
        w.tiles.set(100, 200, 42);
        w.tiles.set(101, 200, 42);
        w.tiles.set(500, 500, 7);
        w.next_player_id = 314;
        w.tick = 999;
        w.ants.push(Ant::new(1, 42, 100, 200, 0, -1, 1000));
        w.queens.insert(42, Queen {
            x: 100, y: 200, size: 2, hp: 80, max_hp: 100, level: 3, xp: 12.5, kills: 4,
            bubble_r: 30.0, last_attacker: Some(7), dead: false, tiles_ever_held: 1000,
            cached_tiles: 2, npc: false, shield: 0, shield_expiry: None, region: "Mostar".into(),
        });
        w.players.insert(42, Player {
            id: 42, username: "ALICE".into(), color: "#abc".into(), hue_idx: 3,
            ants_avail: 7, next_refill: 123_456, queen_placed_at: Some(42), npc: false,
            view: None, tx: None, view_tx: None, ctl_tx: None, bin: false, conn_gen: 5,
            prestige: 2, credits: 50, defenders: vec![1, 2, 3],
            visited_countries:  ["US".to_string()].into_iter().collect(),
            visited_continents: ["NA".to_string()].into_iter().collect(),
            lifetime_kills: 9, lifetime_peak_tiles: 1234, queens_fielded: 2, away: None,
        });

        // Point persistence at a unique temp dir; the path keeps the `world.snapshot` name so the
        // auth `users.json` derivation lands beside it. Cleaned up at the end.
        let dir = std::env::temp_dir().join(format!("hive_persist_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("world.snapshot").to_str().unwrap().to_string();
        cfg_write().save_file = path.clone();

        save(&w, &path).expect("save");
        let snap = load(&path).expect("snapshot loads back");
        let mut w2 = World::new();
        restore(&mut w2, snap);

        assert_eq!(w2.tiles.get(100, 200), 42);
        assert_eq!(w2.tiles.get(101, 200), 42);
        assert_eq!(w2.tiles.get(500, 500), 7);
        assert_eq!(w2.tiles.counts.get(&42).copied(), Some(2), "per-owner counts survive");
        assert_eq!(w2.next_player_id, 314);
        assert_eq!(w2.tick, 999);
        assert_eq!(w2.ants.len(), 1);
        assert_eq!(w2.ants[0].owner, 42);

        let q = w2.queens.get(&42).expect("queen restored");
        assert_eq!((q.level, q.hp, q.kills), (3, 80, 4));
        assert_eq!(q.region, "Mostar");

        let p = w2.players.get(&42).expect("player restored");
        assert_eq!(p.username, "ALICE");
        assert_eq!(p.credits, 50);
        assert_eq!(p.defenders, vec![1, 2, 3]);
        assert!(p.visited_countries.contains("US"));
        assert!(p.tx.is_none() && p.view.is_none(), "runtime channels not persisted");
        assert_eq!(p.conn_gen, 0, "conn_gen reset on restore");

        assert!(w2.queen_map_dirty, "queen map flagged for rebuild");

        std::fs::remove_dir_all(&dir).ok();
    }
}
