//! World + account persistence: zstd-compressed bincode snapshots so a restart (e.g. a service
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
//! **∥A off-lock persistence.** The heavy save is split so the sim tick is never stalled:
//! `serialize_world` (fast **bincode 2** encode) runs under a short **read** lock — which the
//! viewport thread shares, so it doesn't block frame delivery — and the slow zstd compression +
//! atomic disk write (`write_snapshot_bytes`) runs **off-lock** on a dedicated thread. The **v3
//! format = zstd-framed bincode 2** (`Box<[u16]>` cells = 2 bytes); it is a deliberate hard break
//! from the older gzip-bincode (v2) / gzip-JSON (v1) snapshots, which no longer load — acceptable
//! for a wipe-tolerant world.

use std::fs;
use std::io::{self, BufReader, BufWriter, Write};

use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};

use crate::tile_map::TileMap;
use crate::world::{Ant, Player, Queen, World};

/// Snapshot layout/format version. v3 = zstd-framed bincode 2 (current). Older gzip-bincode (v2) and
/// gzip-JSON (v1) snapshots are a deliberate hard break and no longer load.
const SNAPSHOT_VERSION: u32 = 3;

/// zstd compression level for `world.snapshot` + the WAL. Level 3 is zstd's default — a strong
/// ratio/speed balance that keeps the off-lock encode cheap.
const ZSTD_LEVEL: i32 = 3;

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
    // Field was previously named `prestige` but the runtime struct renamed it to `deaths`.
    // The bincode layout is POSITIONAL (no field names on the wire), so this rename is
    // layout-compatible — no snapshot version bump required.
    deaths:              u32,
    nectar:             u64,
    defenders:           Vec<u64>,
    passport_countries:   FxHashSet<String>,
    passport_continents:  FxHashSet<String>,
    lifetime_kills:      u32,
    lifetime_peak_tiles: u64,
    queens_fielded:      u32,
    #[serde(default)]
    unlimited_nectar:   bool,
    #[serde(default)]
    unlimited_ants:      bool,
    #[serde(default)]
    killed_by:           FxHashMap<String, u32>,
    #[serde(default)]
    kills_of:            FxHashMap<String, u32>,
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
            deaths:              p.deaths,
            nectar:             p.nectar,
            defenders:           p.defenders.clone(),
            passport_countries:   p.passport_countries.clone(),
            passport_continents:  p.passport_continents.clone(),
            lifetime_kills:      p.lifetime_kills,
            lifetime_peak_tiles: p.lifetime_peak_tiles,
            queens_fielded:      p.queens_fielded,
            unlimited_nectar:   p.unlimited_nectar,
            unlimited_ants:      p.unlimited_ants,
            killed_by:           p.killed_by.clone(),
            kills_of:            p.kills_of.clone(),
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
            guest:               false,
            view:                None,
            tx:                  None,
            view_tx:             None,
            ctl_tx:              None,
            egress_meter:        None,
            bin:                 false,
            conn_gen:            0,
            deaths:              self.deaths,
            nectar:             self.nectar,
            defenders:           self.defenders,
            passport_countries:   self.passport_countries,
            passport_continents:  self.passport_continents,
            lifetime_kills:      self.lifetime_kills,
            lifetime_peak_tiles: self.lifetime_peak_tiles,
            queens_fielded:      self.queens_fielded,
            unlimited_nectar:   self.unlimited_nectar,
            unlimited_ants:      self.unlimited_ants,
            killed_by:           self.killed_by,
            kills_of:            self.kills_of,
            away:                None,
            had_queen_this_season: false,   // runtime-only; always false on load (set on next place-queen)
            tile_fx:             None,   // runtime caches; reloaded from the account on connect
            aura:                None,
            trail:               None,
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
        // Guests are ephemeral spectators (RAM-only) — never persist them.
        players:        world.players.values().filter(|p| !p.guest)
                            .map(PlayerSnapshot::from_player).collect(),
        next_player_id: world.next_player_id,
        tick:           world.tick,
        started_at:     world.started_at,
    };
    bincode::serde::encode_to_vec(&snap, bincode::config::standard()).map_err(io::Error::other)
}

/// **Slow, off-lock half of the save:** zstd-compress the bincode buffer and write it atomically
/// (temp file + rename). Touches no `World`, so it runs on a dedicated thread without any lock.
pub fn write_snapshot_bytes(raw: &[u8], path: &str) -> io::Result<()> {
    let tmp = format!("{path}.tmp");
    {
        let f = fs::File::create(&tmp)?;
        let mut enc = zstd::Encoder::new(BufWriter::new(f), ZSTD_LEVEL)?;
        enc.write_all(raw)?;
        let w = enc.finish()?; // flush the zstd frame; returns the inner BufWriter
        // fsync before the rename — the rename must never promote a not-yet-durable temp file to
        // being the live snapshot (a power cut could otherwise leave a truncated one behind it).
        w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
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
    // Persist the R2 tile-generation epoch beside the snapshot (see `save_epoch`).
    save_epoch(world, path);
    // Accounts persist alongside the world so both files move together.
    world.auth.save();
    Ok(())
}

/// Path of the epoch sidecar (`world.snapshot.epoch`) beside the world snapshot.
fn epoch_path(world_path: &str) -> String { format!("{world_path}.epoch") }

/// Persist the R2 snapshot **epoch** (tile-generation tag) in its own tiny sidecar file. Kept out
/// of the bincode `WorldSnapshot` deliberately: that format reads fields positionally, so adding a
/// field would break loading an existing live snapshot (bincode has no "missing field" default).
///
/// **Why this exists:** the epoch is the `snap/{epoch}/…` R2 key prefix. It used to be re-minted
/// from the boot clock on every start, so each restart re-uploaded the whole canvas under a *fresh*
/// generation and orphaned the previous one on R2 **forever** — an unbounded storage leak that a
/// world WIPE could not reclaim (wipe only retires the single current generation). Persisting the
/// epoch keeps the generation STABLE across restarts: a redeploy re-uses the existing objects
/// instead of leaking a new copy. Cheap (a few bytes); written on every `save` (shutdown + post-wipe).
pub fn save_epoch(world: &World, world_path: &str) {
    if let Err(e) = fs::write(epoch_path(world_path), world.epoch.to_string()) {
        eprintln!("[persist] epoch sidecar save failed: {e}");
    }
}

/// Read the persisted snapshot epoch, if present and valid. `None` → no sidecar yet (first boot
/// after this change, or a fresh data dir) → the caller keeps the freshly-minted boot epoch.
pub fn load_epoch(world_path: &str) -> Option<u64> {
    fs::read_to_string(epoch_path(world_path)).ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
}

/// Load a **v3 zstd-bincode** world snapshot from `path`. Older gzip-bincode (v2) / gzip-JSON (v1)
/// files are a deliberate hard break: zstd decode fails on them → `None` → the caller starts fresh.
/// Returns `None` if absent / unreadable / corrupt / wrong-version.
pub fn load(path: &str) -> Option<WorldSnapshot> {
    let f = fs::File::open(path).ok()?;
    let raw = zstd::decode_all(BufReader::new(f)).ok()?;
    let snap: WorldSnapshot =
        bincode::serde::decode_from_slice(&raw, bincode::config::standard()).ok()?.0;
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
    // bubble_r now scales with level; snapshots from before that change stored the flat base for
    // every queen, so recompute it from each queen's level (keeps placement/glow correct on boot).
    {
        let c = crate::config::cfg();
        for q in world.queens.values_mut() {
            q.bubble_r = crate::config::bubble_r_for_level(q.level, &c);
        }
    }
    world.players        = snap.players.into_iter().map(|ps| (ps.id, ps.into_player())).collect();
    world.next_player_id = snap.next_player_id;
    world.tick           = snap.tick;
    world.started_at     = snap.started_at;

    world.queen_map.clear();
    world.queen_map_dirty = true;
    world.dirty_tick = world.tick;
    // Phase-6: a restored world must re-upload its whole painted canvas to R2 once.
    world.tiles.mark_all_dirty();
    // Pan-radius bounds are runtime-only (serde-skipped) → rebuild from the restored cells so
    // returning players keep the home region that matches their existing territory.
    world.tiles.rebuild_bounds();

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

/// Append a zstd-bincode WAL record capturing the current owner-id state of `keys` (the chunks
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
    let raw = bincode::serde::encode_to_vec(&rec, bincode::config::standard()).map_err(io::Error::other)?;
    let comp = zstd::encode_all(&raw[..], ZSTD_LEVEL)?;
    let mut f = fs::OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(&(comp.len() as u32).to_le_bytes())?;
    f.write_all(&comp)?;
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
        let Ok(raw) = zstd::decode_all(&bytes[off..off + len]) else { break; };
        off += len;
        let Ok((rec, _)) = bincode::serde::decode_from_slice::<WalRecord, _>(&raw, bincode::config::standard())
        else { break; };
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
            ants_avail: 7, next_refill: 123_456, queen_placed_at: Some(42), conn_gen: 5,
            deaths: 2, nectar: 50, defenders: vec![1, 2, 3],
            passport_countries:  ["US".to_string()].into_iter().collect(),
            passport_continents: ["NA".to_string()].into_iter().collect(),
            lifetime_kills: 9, lifetime_peak_tiles: 1234, queens_fielded: 2,
            unlimited_nectar: true,
            killed_by: [("BOB".to_string(), 2)].into_iter().collect(),
            ..Default::default()
        });

        // Point persistence at a unique temp dir; the path keeps the `world.snapshot` name so the
        // auth `users.json` derivation lands beside it. Cleaned up at the end.
        let dir = std::env::temp_dir().join(format!("antarchy_persist_test_{}", std::process::id()));
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
        assert_eq!(p.nectar, 50);
        assert_eq!(p.defenders, vec![1, 2, 3]);
        assert!(p.passport_countries.contains("US"));
        assert!(p.tx.is_none() && p.view.is_none(), "runtime channels not persisted");
        assert_eq!(p.conn_gen, 0, "conn_gen reset on restore");
        assert!(p.unlimited_nectar && !p.unlimited_ants, "god-mode flags survive round-trip");
        assert_eq!(p.killed_by.get("BOB").copied(), Some(2), "rivalry map survives round-trip");

        assert!(w2.queen_map_dirty, "queen map flagged for rebuild");

        // The R2 epoch is persisted in its own sidecar (NOT the bincode snapshot) so it survives a
        // restart → the snapshot generation stays stable instead of leaking a fresh R2 canvas copy
        // every boot. `save` wrote it above; it must read back as the exact epoch we saved.
        assert_eq!(load_epoch(&path), Some(w.epoch), "epoch sidecar round-trips via save()");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn epoch_sidecar_absent_then_present() {
        let dir = std::env::temp_dir().join(format!("antarchy_epoch_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("world.snapshot").to_str().unwrap().to_string();

        // No sidecar yet → None (boot keeps its freshly-minted epoch).
        assert_eq!(load_epoch(&path), None);

        let mut w = World::new();
        w.epoch = 1_700_000_000;
        save_epoch(&w, &path);
        assert_eq!(load_epoch(&path), Some(1_700_000_000));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bincode_round_trips_in_memory() {
        // serialize_world → write_snapshot_bytes → load reconstructs tiles + counters (no live
        // server needed). Exercises the off-lock split + the v3 zstd-bincode format end-to-end.
        let mut w = World::new();
        for x in 0..300u32 { w.tiles.set(x, 50, 11); }   // spans a chunk boundary, some Dense
        w.tiles.set(1000, 1000, 22);
        w.next_player_id = 77;
        w.tick = 4242;

        let raw = serialize_world(&w).expect("serialize");
        // Sanity: bincode, not JSON (its first byte is the version varint = 0x03, never '{').
        assert_ne!(raw.first().copied(), Some(b'{'));

        let dir = std::env::temp_dir().join(format!("antarchy_snapshot_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("world.snapshot").to_str().unwrap().to_string();
        write_snapshot_bytes(&raw, &path).expect("write");

        let snap = load(&path).expect("snapshot loads");
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
    fn wal_replays_chunk_deltas_onto_base() {
        // Base snapshot has one painted cell; then more cells change. The WAL captures the changed
        // chunks; replaying it onto the base reconstructs the full latest state.
        let mut w = World::new();
        w.tiles.set(10, 10, 5);
        let base = serialize_world(&w).unwrap();

        let dir = std::env::temp_dir().join(format!("antarchy_wal_test_{}", std::process::id()));
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
