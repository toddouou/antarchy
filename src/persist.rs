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
//! **∥A off-lock persistence (Phase v2).** The heavy save is split so the sim tick is never stalled:
//! `serialize_world` (fast **bincode** encode) runs under a short **read** lock — which the viewport
//! thread shares, so it doesn't block frame delivery — and the slow gzip + atomic disk write
//! (`write_snapshot_bytes`) runs **off-lock** on a dedicated thread. The format moved JSON → bincode
//! (`Box<[u16]>` cells = 2 bytes vs ASCII int-lists → much smaller + faster); `load` still accepts
//! the old v1 gzip-JSON so a live `world.snapshot` survives the upgrade with no data loss.

use std::fs;
use std::io::{self, BufReader, BufWriter, Read, Write};

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};

use crate::tile_map::TileMap;
use crate::world::{Ant, Player, Queen, World};

/// Snapshot layout/format version. v1 = gzip-JSON (legacy, still loadable); v2 = gzip-bincode.
const SNAPSHOT_VERSION: u32 = 2;

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
            egress_meter:        None,
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

/// **Fast, under-(read)-lock half of the save:** bincode-encode the world into an in-memory buffer.
/// No gzip, no disk I/O — so the caller holds the lock only for the cheap encode, then releases it
/// and hands the bytes to `write_snapshot_bytes` off-lock. Returns the raw (uncompressed) bincode.
pub fn serialize_world(world: &World) -> io::Result<Vec<u8>> {
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
    bincode::serialize(&snap).map_err(|e| io::Error::new(io::ErrorKind::Other, e))
}

/// **Slow, off-lock half of the save:** gzip the bincode buffer and write it atomically
/// (temp file + rename). Touches no `World`, so it runs on a dedicated thread without any lock.
pub fn write_snapshot_bytes(raw: &[u8], path: &str) -> io::Result<()> {
    let tmp = format!("{path}.tmp");
    {
        let f = fs::File::create(&tmp)?;
        let mut enc = GzEncoder::new(BufWriter::new(f), Compression::default());
        enc.write_all(raw)?;
        let mut w = enc.finish()?;
        w.flush()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Persist the world to `path` and accounts to `users.json`. Convenience wrapper used by the
/// **shutdown** path (synchronous is fine there); the periodic autosave uses the split
/// `serialize_world` (under lock) + `write_snapshot_bytes` (off-lock) directly.
pub fn save(world: &World, path: &str) -> io::Result<()> {
    let raw = serialize_world(world)?;
    write_snapshot_bytes(&raw, path)?;
    // Accounts persist alongside the world so both files move together.
    world.auth.save();
    Ok(())
}

/// Load a world snapshot from `path`. Accepts both the new **v2 gzip-bincode** and the legacy
/// **v1 gzip-JSON** (detected by the first decompressed byte: `{` → JSON), so an existing live
/// snapshot loads unchanged after the format upgrade. Returns `None` if absent/unreadable/corrupt.
pub fn load(path: &str) -> Option<WorldSnapshot> {
    let f = fs::File::open(path).ok()?;
    let mut dec = GzDecoder::new(BufReader::new(f));
    let mut raw = Vec::new();
    dec.read_to_end(&mut raw).ok()?;

    // First non-whitespace byte distinguishes the format: JSON objects start with '{'.
    let is_json = raw.iter().find(|b| !b.is_ascii_whitespace()).copied() == Some(b'{');
    let snap: WorldSnapshot = if is_json {
        serde_json::from_slice(&raw).ok()?
    } else {
        bincode::deserialize(&raw).ok()?
    };
    if snap.version > SNAPSHOT_VERSION {
        eprintln!("[persist] ignoring snapshot: version {} > {SNAPSHOT_VERSION}", snap.version);
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
    // Phase-6: a restored world must re-upload its whole painted canvas to R2 once.
    world.tiles.mark_all_dirty();

    let tiles = world.tiles.total_tiles();
    println!("[persist] restored: {tiles} tiles, {queens} queens, {players} players, {ants} ants (tick {})", world.tick);
}

// ---- ∥A Write-Ahead Log (gated off by HIVE_WAL) -------------------------------------------------
//
// A chunk-delta journal: each record holds the full owner-id state of the chunks that changed since
// the previous record. Replayed on top of a base snapshot to recover the latest world without
// rewriting the whole canvas each save. Provided as tested, opt-in functions: the off-lock full save
// already removes the autosave tick-stall, and wiring the WAL drain into the live autosave would race
// the Phase-6 snapshot writer for the dirty set (both drain it) — that needs a shared dirty-tracking
// design, so the live default stays the proven full save. `replay_wal` is invoked at boot when
// `HIVE_WAL` is on; `append_wal` is the writer half operators can schedule.

#[derive(Serialize, Deserialize)]
struct WalChunk { key: u64, owners: Vec<u32> }
#[derive(Serialize, Deserialize)]
struct WalRecord { tick: u64, chunks: Vec<WalChunk> }

/// WAL file path, derived from the snapshot path (`world.snapshot` → `world.snapshot.wal`).
pub fn wal_path(snapshot_path: &str) -> String { format!("{snapshot_path}.wal") }

/// Append a gzip-bincode WAL record capturing the current owner-id state of `keys` (the chunks
/// changed since the last record). Length-prefixed so records read back sequentially. The writer
/// half operators schedule when running with `HIVE_WAL`; not on the default autosave path.
#[allow(dead_code)]
pub fn append_wal(world: &World, keys: &[u64], path: &str) -> io::Result<()> {
    use crate::tile_map::{ChunkView, CHUNK_DIM};
    let cells = CHUNK_DIM * CHUNK_DIM;
    let chunks: Vec<WalChunk> = keys.iter().map(|&key| {
        let owners = match world.tiles.view_chunk(key) {
            None                          => Vec::new(), // cleared → all unclaimed on replay
            Some(ChunkView::Uniform(idx)) => vec![world.tiles.palette_owner(idx); cells],
            Some(ChunkView::Dense(c))     => c.iter().map(|&i| world.tiles.palette_owner(i)).collect(),
        };
        WalChunk { key, owners }
    }).collect();
    let rec = WalRecord { tick: world.tick, chunks };
    let raw = bincode::serialize(&rec).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    let mut gz = Vec::new();
    { let mut enc = GzEncoder::new(&mut gz, Compression::default()); enc.write_all(&raw)?; enc.finish()?; }
    let mut f = fs::OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(&(gz.len() as u32).to_le_bytes())?;
    f.write_all(&gz)?;
    Ok(())
}

/// Replay every WAL record in `path` onto `world` (after a base snapshot restore). Missing/corrupt
/// file → best-effort partial replay. Returns the number of chunks applied.
pub fn replay_wal(world: &mut World, path: &str) -> usize {
    let Ok(bytes) = fs::read(path) else { return 0; };
    let mut off = 0usize;
    let mut applied = 0usize;
    while off + 4 <= bytes.len() {
        let len = u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]]) as usize;
        off += 4;
        if off + len > bytes.len() { break; }
        let mut dec = GzDecoder::new(&bytes[off..off + len]);
        off += len;
        let mut raw = Vec::new();
        if dec.read_to_end(&mut raw).is_err() { break; }
        let Ok(rec) = bincode::deserialize::<WalRecord>(&raw) else { break; };
        for ch in rec.chunks { world.tiles.restore_chunk_owners(ch.key, &ch.owners); applied += 1; }
        world.tick = world.tick.max(rec.tick);
    }
    if applied > 0 { world.queen_map_dirty = true; }
    applied
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
            view: None, tx: None, view_tx: None, ctl_tx: None, egress_meter: None, bin: false, conn_gen: 5,
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

    #[test]
    fn v2_bincode_round_trips_in_memory() {
        // serialize_world → write_snapshot_bytes → load reconstructs tiles + counters (no live
        // server needed). Exercises the off-lock split + the bincode format end-to-end.
        let mut w = World::new();
        for x in 0..300u32 { w.tiles.set(x, 50, 11); }   // spans a chunk boundary, some Dense
        w.tiles.set(1000, 1000, 22);
        w.next_player_id = 77;
        w.tick = 4242;

        let raw = serialize_world(&w).expect("serialize");
        // Sanity: it is NOT JSON (first byte is the bincode u32 version little-endian = 0x02).
        assert_ne!(raw.first().copied(), Some(b'{'));

        let dir = std::env::temp_dir().join(format!("hive_v2_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("world.snapshot").to_str().unwrap().to_string();
        write_snapshot_bytes(&raw, &path).expect("write");

        let snap = load(&path).expect("v2 loads");
        let mut w2 = World::new();
        restore(&mut w2, snap);
        assert_eq!(w2.tiles.get(0, 50), 11);
        assert_eq!(w2.tiles.get(299, 50), 11);
        assert_eq!(w2.tiles.get(1000, 1000), 22);
        assert_eq!(w2.next_player_id, 77);
        assert_eq!(w2.tick, 4242);
        // restore seeded the dirty set so the snapshot writer re-uploads the canvas.
        assert!(w2.tiles.dirty_chunk_len() > 0, "restore marks chunks dirty for R2 re-upload");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn legacy_v1_json_snapshot_still_loads() {
        // A hand-written v1 gzip-JSON snapshot must still load after the bincode upgrade, so a live
        // world.snapshot survives the first deploy of the new binary.
        let mut w = World::new();
        w.tiles.set(5, 5, 9);
        w.next_player_id = 3;
        w.tick = 7;
        let snap = WorldSnapshotRef {
            version: 1, tiles: &w.tiles, ants: &w.ants, queens: &w.queens,
            players: w.players.values().map(PlayerSnapshot::from_player).collect(),
            next_player_id: w.next_player_id, tick: w.tick, started_at: w.started_at,
        };
        let json = serde_json::to_vec(&snap).unwrap();

        let dir = std::env::temp_dir().join(format!("hive_v1_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("world.snapshot").to_str().unwrap().to_string();
        // gzip the JSON exactly like the old save did.
        write_gzip_raw(&json, &path).unwrap();

        let loaded = load(&path).expect("v1 JSON still loads");
        let mut w2 = World::new();
        restore(&mut w2, loaded);
        assert_eq!(w2.tiles.get(5, 5), 9);
        assert_eq!(w2.tick, 7);

        std::fs::remove_dir_all(&dir).ok();
    }

    // Test helper: gzip arbitrary bytes to `path` (used to forge a v1 JSON snapshot).
    fn write_gzip_raw(raw: &[u8], path: &str) -> io::Result<()> {
        super::write_snapshot_bytes(raw, path)
    }

    #[test]
    fn wal_replays_chunk_deltas_onto_base() {
        // Base snapshot has one painted cell; then more cells change. The WAL captures the changed
        // chunks; replaying it onto the base reconstructs the full latest state.
        let mut w = World::new();
        w.tiles.set(10, 10, 5);
        let base = serialize_world(&w).unwrap();

        let dir = std::env::temp_dir().join(format!("hive_wal_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("world.snapshot").to_str().unwrap().to_string();
        write_snapshot_bytes(&base, &path).unwrap();

        // Changes after the base — different chunks (10,10) and (2000,2000).
        w.tiles.set(11, 10, 5);
        w.tiles.set(2000, 2000, 9);
        let keys = w.tiles.drain_dirty_chunks();
        let wal = super::wal_path(&path);
        super::append_wal(&w, &keys, &wal).unwrap();

        // Load the base (only cell 10,10), then replay the WAL.
        let mut w2 = World::new();
        restore(&mut w2, load(&path).unwrap());
        assert_eq!(w2.tiles.get(11, 10), 0, "base alone lacks the post-base change");
        let n = super::replay_wal(&mut w2, &wal);
        assert!(n >= 2, "applied {n} chunks");
        assert_eq!(w2.tiles.get(10, 10), 5);
        assert_eq!(w2.tiles.get(11, 10), 5);
        assert_eq!(w2.tiles.get(2000, 2000), 9);

        std::fs::remove_dir_all(&dir).ok();
    }
}
