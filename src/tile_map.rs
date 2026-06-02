use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

const CHUNK_SHIFT: u32   = 8;
const CHUNK_SIZE:  usize = 1 << CHUNK_SHIFT;        // 256
const CHUNK_MASK:  u32   = (CHUNK_SIZE as u32) - 1;
const CHUNK_CELLS: usize = CHUNK_SIZE * CHUNK_SIZE; // 65 536 cells

/// Palette index stored per cell. 0 = unclaimed. A `u16` halves Dense-chunk RAM vs the
/// old raw-`u32` player id; the index→player map below keeps cells compact even though
/// lifetime player ids (monotonic, never reused) exceed `u16` over a season.
type Idx = u16;

/// A 256×256 territory chunk. Solid interiors collapse to `Uniform` (a few bytes); only
/// borders / active frontiers pay the `Dense` array. This makes tile RAM scale with the
/// *perimeter* of painted territory, not its area — the planet-scale memory fix.
#[derive(Serialize, Deserialize)]
enum Chunk {
    /// Every cell in the chunk is this single nonzero index.
    Uniform(Idx),
    /// Mixed chunk: one `Idx` per cell (0 = unclaimed). `occupied` = nonzero-cell count.
    Dense { cells: Box<[Idx]>, occupied: u32 },
}

/// Sparse, content-compressed tile store.
///
/// Cells hold compact `u16` **palette indices**; `palette[idx] -> player_id` and
/// `id_to_idx[player_id] -> idx` translate at the boundary so every public method still
/// speaks raw player ids (`get`/`set`/`counts`/`clear_owner`/…) — callers are unchanged.
/// Indices are recycled through `free` the moment an owner's tile count hits 0, so the live
/// index space is bounded by the number of *concurrently painting* owners (≤ a few hundred),
/// never the lifetime id count.
#[derive(Serialize, Deserialize)]
pub struct TileMap {
    chunks:    FxHashMap<u64, Chunk>,
    /// player_id → tile count. Public + kept exact per-cell across every chunk transition.
    pub counts: FxHashMap<u32, i64>,
    /// index → player_id. `palette[0] == 0` (unclaimed) is reserved and never reused.
    palette:   Vec<u32>,
    /// player_id → index (live owners only).
    id_to_idx: FxHashMap<u32, Idx>,
    /// Recycled indices, freed when an owner's count reaches 0.
    free:      Vec<Idx>,
    /// Phase-0 egress instrumentation (runtime-only, never serialized): the last chunk a `set`
    /// actually mutated, used to count chunk-touch *transitions*.
    #[serde(skip)]
    last_touched_chunk: Option<u64>,
    /// Cumulative chunk-touch transitions — a conservative over-estimate of distinct dirty chunks
    /// per interval, used to project Phase-6 R2 Class-A write volume.
    #[serde(skip)]
    dirty_chunk_touches: u64,
}

impl Default for TileMap {
    fn default() -> Self {
        TileMap {
            chunks:    FxHashMap::default(),
            counts:    FxHashMap::default(),
            palette:   vec![0u32],          // index 0 = unclaimed
            id_to_idx: FxHashMap::default(),
            free:      Vec::new(),
            last_touched_chunk: None,
            dirty_chunk_touches: 0,
        }
    }
}

impl TileMap {
    #[inline]
    fn chunk_key(x: u32, y: u32) -> u64 {
        let cx = (x >> CHUNK_SHIFT) as u64;
        let cy = (y >> CHUNK_SHIFT) as u64;
        (cy << 32) | cx
    }

    #[inline]
    fn local(x: u32, y: u32) -> usize {
        let lx = (x & CHUNK_MASK) as usize;
        let ly = (y & CHUNK_MASK) as usize;
        ly * CHUNK_SIZE + lx
    }

    /// Index for `owner`, allocating (or recycling) one if it has none yet. `owner != 0`.
    #[inline]
    fn idx_for(&mut self, owner: u32) -> Idx {
        if let Some(&i) = self.id_to_idx.get(&owner) { return i; }
        let i = if let Some(i) = self.free.pop() {
            self.palette[i as usize] = owner;
            i
        } else {
            let i = self.palette.len() as Idx;
            self.palette.push(owner);
            i
        };
        self.id_to_idx.insert(owner, i);
        i
    }

    #[inline]
    fn inc_count(&mut self, owner: u32) {
        *self.counts.entry(owner).or_insert(0) += 1;
    }

    /// Decrement an owner's count; when it reaches 0 the owner is dropped and its palette
    /// index is recycled (safe: count 0 ⇒ no cell references that index any longer).
    #[inline]
    fn dec_count(&mut self, owner: u32) {
        if let Some(e) = self.counts.get_mut(&owner) {
            *e -= 1;
            if *e <= 0 {
                self.counts.remove(&owner);
                if let Some(i) = self.id_to_idx.remove(&owner) {
                    self.free.push(i);
                }
            }
        }
    }

    /// Count a chunk-touch transition: bumps the cumulative counter only when a `set` mutates a
    /// chunk different from the previous mutated one. Single-threaded (writes hold `&mut World`),
    /// so a plain field is sufficient — no atomics. Conservative for the R2 Class-A projection.
    #[inline]
    fn note_dirty(&mut self, key: u64) {
        if self.last_touched_chunk != Some(key) {
            self.last_touched_chunk = Some(key);
            self.dirty_chunk_touches = self.dirty_chunk_touches.wrapping_add(1);
        }
    }

    /// Cumulative chunk-touch transitions since boot (an over-estimate of distinct dirty chunks).
    /// Diff two samples over a window to project Phase-6 R2 Class-A (chunk-upload) volume.
    pub fn dirty_chunk_touches(&self) -> u64 { self.dirty_chunk_touches }

    #[inline]
    pub fn get(&self, x: u32, y: u32) -> u32 {
        match self.chunks.get(&Self::chunk_key(x, y)) {
            Some(Chunk::Uniform(i))          => self.palette[*i as usize],
            Some(Chunk::Dense { cells, .. })  => self.palette[cells[Self::local(x, y)] as usize],
            None                             => 0,
        }
    }

    pub fn set(&mut self, x: u32, y: u32, new_owner: u32) {
        let key = Self::chunk_key(x, y);
        let li  = Self::local(x, y);

        // ---- Clear path (new_owner == 0): may expand a Uniform chunk to Dense ----
        if new_owner == 0 {
            let old_i = {
                let Some(chunk) = self.chunks.get_mut(&key) else { return; };
                match chunk {
                    Chunk::Uniform(ui) => {
                        let ui = *ui;
                        let mut cells = vec![ui; CHUNK_CELLS].into_boxed_slice();
                        cells[li] = 0;
                        *chunk = Chunk::Dense { cells, occupied: (CHUNK_CELLS - 1) as u32 };
                        ui
                    }
                    Chunk::Dense { cells, occupied } => {
                        let old_i = cells[li];
                        if old_i == 0 { return; }
                        cells[li] = 0;
                        *occupied -= 1;
                        if *occupied == 0 { self.chunks.remove(&key); }
                        old_i
                    }
                }
            };
            let old_owner = self.palette[old_i as usize];
            self.dec_count(old_owner);
            self.note_dirty(key);
            return;
        }

        // ---- Paint path (new_owner != 0) ----
        let new_i = self.idx_for(new_owner);
        let old_i = {
            let chunk = self.chunks.entry(key).or_insert_with(|| Chunk::Dense {
                cells:    vec![0 as Idx; CHUNK_CELLS].into_boxed_slice(),
                occupied: 0,
            });
            match chunk {
                Chunk::Uniform(ui) => {
                    if *ui == new_i { return; }      // already this owner — no change
                    let old_i = *ui;
                    // Expand to Dense filled with the old owner, then set this cell.
                    let mut cells = vec![old_i; CHUNK_CELLS].into_boxed_slice();
                    cells[li] = new_i;
                    // Was all-nonzero; replaced one nonzero with another → still full.
                    *chunk = Chunk::Dense { cells, occupied: CHUNK_CELLS as u32 };
                    old_i
                }
                Chunk::Dense { cells, occupied } => {
                    let old_i = cells[li];
                    if old_i == new_i { return; }
                    cells[li] = new_i;
                    if old_i == 0 {
                        *occupied += 1;
                        // Compact only when a fill *completes* the chunk (bounded scan; avoids
                        // churn on contested full chunks, which never hit this branch).
                        if *occupied as usize == CHUNK_CELLS && cells.iter().all(|&c| c == new_i) {
                            *chunk = Chunk::Uniform(new_i);
                        }
                    }
                    old_i
                }
            }
        };
        if old_i != 0 {
            let old_owner = self.palette[old_i as usize];
            self.dec_count(old_owner);
        }
        self.inc_count(new_owner);
        self.note_dirty(key);
    }

    pub fn clear(&mut self) {
        self.chunks.clear();
        self.counts.clear();
        self.palette.clear();
        self.palette.push(0);   // restore the reserved unclaimed slot
        self.id_to_idx.clear();
        self.free.clear();
    }

    /// Clear every tile owned by `owner` (→ unclaimed) and drop the owner's count + index.
    /// Used on queen death — its territory is forfeited and turns blank. A `Uniform` chunk
    /// owned by the loser is dropped whole (no per-cell scan); emptied `Dense` chunks drop too.
    /// `remaining` lets later chunks skip the scan once all of the owner's tiles are found.
    pub fn clear_owner(&mut self, owner: u32) {
        if owner == 0 { return; }
        let oi = match self.id_to_idx.get(&owner) {
            Some(&i) => i,
            None     => { self.counts.remove(&owner); return; }
        };
        let mut remaining = self.counts.get(&owner).copied().unwrap_or(0);
        if remaining <= 0 {
            self.counts.remove(&owner);
            self.id_to_idx.remove(&owner);
            self.free.push(oi);
            return;
        }
        self.chunks.retain(|_, chunk| {
            if remaining <= 0 { return true; }
            match chunk {
                Chunk::Uniform(ui) => {
                    if *ui == oi { remaining -= CHUNK_CELLS as i64; false } else { true }
                }
                Chunk::Dense { cells, occupied } => {
                    if *occupied == 0 { return false; }
                    let mut zeroed: u32 = 0;
                    for c in cells.iter_mut() {
                        if *c == oi { *c = 0; zeroed += 1; }
                    }
                    *occupied -= zeroed;
                    remaining -= zeroed as i64;
                    *occupied != 0
                }
            }
        });
        self.counts.remove(&owner);
        self.id_to_idx.remove(&owner);
        self.free.push(oi);
    }

    pub fn total_tiles(&self) -> usize {
        self.chunks.values().map(|c| match c {
            Chunk::Uniform(_)              => CHUNK_CELLS,
            Chunk::Dense { occupied, .. }  => *occupied as usize,
        }).sum()
    }

    #[allow(dead_code)]
    pub fn iter_tiles(&self) -> impl Iterator<Item = (u32, u32, u32)> + '_ {
        self.chunks.iter().flat_map(move |(&ck, chunk)| {
            let cx  = (ck & 0xFFFF_FFFF) as u32;
            let cy  = (ck >> 32) as u32;
            let pal = &self.palette;
            (0..CHUNK_CELLS).filter_map(move |li| {
                let i = match chunk {
                    Chunk::Uniform(ui)            => *ui,
                    Chunk::Dense { cells, .. }    => cells[li],
                };
                if i == 0 { return None; }
                let lx = (li % CHUNK_SIZE) as u32;
                let ly = (li / CHUNK_SIZE) as u32;
                Some(((cx << CHUNK_SHIFT) | lx, (cy << CHUNK_SHIFT) | ly, pal[i as usize]))
            })
        })
    }

    /// Chunk-store stats for `/health`: (chunks, uniform_chunks, dense_chunks, est_bytes).
    /// `est_bytes` counts Dense cell arrays + a rough per-chunk/per-palette overhead — the
    /// number to watch is the uniform:dense ratio climbing as solid territory compacts.
    pub fn stats(&self) -> (usize, usize, usize, u64) {
        let mut uniform = 0usize;
        let mut dense   = 0usize;
        let mut bytes   = 0u64;
        for c in self.chunks.values() {
            match c {
                Chunk::Uniform(_)          => uniform += 1,
                Chunk::Dense { cells, .. } => { dense += 1; bytes += (cells.len() * 2) as u64; }
            }
        }
        let chunks = self.chunks.len();
        bytes += (chunks as u64) * 48;                 // ~hashmap entry + enum discriminant
        bytes += (self.palette.len() as u64) * 4;      // index→id table
        (chunks, uniform, dense, bytes)
    }

    /// Periodic maintenance (season ops): collapse any `Dense` chunk that has since become
    /// solid-one-owner (e.g. via clash conversions, which never trip the inline compaction)
    /// into `Uniform`. Returns how many chunks compacted. Cheap; call on a throttle.
    pub fn compact_pass(&mut self) -> usize {
        let mut n = 0usize;
        for chunk in self.chunks.values_mut() {
            if let Chunk::Dense { cells, occupied } = chunk {
                if *occupied as usize == CHUNK_CELLS {
                    let first = cells[0];
                    if first != 0 && cells.iter().all(|&c| c == first) {
                        *chunk = Chunk::Uniform(first);
                        n += 1;
                    }
                }
            }
        }
        n
    }

    /// Tally painted-tile owners inside the circle (`cx`,`cy`,`r2`), sampling every `stride`-th
    /// cell in each axis. Iterates only chunks overlapping the circle's bounding box; absent
    /// (ocean/empty) chunks cost nothing. `Uniform` chunks read one index for the whole tile.
    /// Counts are 1/stride² of the true totals — fine for argmax (king-of-the-hill).
    pub fn tally_owners_in_circle(&self, cx: i32, cy: i32, r2: i64, stride: u32, out: &mut FxHashMap<u32, u64>) {
        let stride = stride.max(1);
        let r = (r2 as f64).sqrt().ceil() as i64;
        let x0 = (cx as i64 - r).max(0);
        let y0 = (cy as i64 - r).max(0);
        let x1 = (cx as i64 + r).max(0);
        let y1 = (cy as i64 + r).max(0);
        let (cx0, cy0) = ((x0 >> CHUNK_SHIFT) as u32, (y0 >> CHUNK_SHIFT) as u32);
        let (cx1, cy1) = ((x1 >> CHUNK_SHIFT) as u32, (y1 >> CHUNK_SHIFT) as u32);
        for cyk in cy0..=cy1 {
            for cxk in cx0..=cx1 {
                let key = ((cyk as u64) << 32) | cxk as u64;
                let Some(chunk) = self.chunks.get(&key) else { continue; };
                let base_x = cxk << CHUNK_SHIFT;
                let base_y = cyk << CHUNK_SHIFT;
                let mut ly = 0u32;
                while (ly as usize) < CHUNK_SIZE {
                    let dy = (base_y + ly) as i64 - cy as i64;
                    let mut lx = 0u32;
                    while (lx as usize) < CHUNK_SIZE {
                        let i = match chunk {
                            Chunk::Uniform(ui)         => *ui,
                            Chunk::Dense { cells, .. } => cells[(ly as usize) * CHUNK_SIZE + lx as usize],
                        };
                        if i != 0 {
                            let dx = (base_x + lx) as i64 - cx as i64;
                            if dx * dx + dy * dy <= r2 {
                                let owner = self.palette[i as usize];
                                *out.entry(owner).or_insert(0) += 1;
                            }
                        }
                        lx += stride;
                    }
                    ly += stride;
                }
            }
        }
    }
}

// TileMap is Sync (all fields Sync) — needed so &TileMap can be shared into rayon closures
// (Phase 1 reads tiles in parallel). No interior mutability: reads never touch `&mut`.
unsafe impl Sync for TileMap {}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(tm: &TileMap, owner: u32) -> i64 { tm.counts.get(&owner).copied().unwrap_or(0) }

    #[test]
    fn set_get_roundtrip_across_chunk_boundary() {
        let mut tm = TileMap::default();
        // Cells straddling the 256 chunk boundary in both axes.
        let pts = [(0u32, 0u32, 7u32), (255, 255, 7), (256, 0, 8), (0, 256, 9), (300, 300, 8)];
        for &(x, y, o) in &pts { tm.set(x, y, o); }
        for &(x, y, o) in &pts { assert_eq!(tm.get(x, y), o, "get({x},{y})"); }
        assert_eq!(tm.get(1, 1), 0, "untouched cell is unclaimed");
        assert_eq!(count(&tm, 7), 2);
        assert_eq!(count(&tm, 8), 2);
        assert_eq!(count(&tm, 9), 1);
    }

    #[test]
    fn counts_exact_through_overwrite_and_clear() {
        let mut tm = TileMap::default();
        tm.set(10, 10, 1);
        tm.set(10, 10, 2);            // overwrite: 1 → 2
        assert_eq!(count(&tm, 1), 0);
        assert_eq!(count(&tm, 2), 1);
        assert_eq!(tm.get(10, 10), 2);
        tm.set(10, 10, 0);            // clear
        assert_eq!(count(&tm, 2), 0);
        assert_eq!(tm.get(10, 10), 0);
        assert_eq!(tm.total_tiles(), 0);
    }

    #[test]
    fn fill_chunk_compacts_to_uniform_and_reads_back() {
        let mut tm = TileMap::default();
        for y in 0..CHUNK_SIZE as u32 {
            for x in 0..CHUNK_SIZE as u32 {
                tm.set(x, y, 5);
            }
        }
        let (chunks, uniform, dense, _) = tm.stats();
        assert_eq!((chunks, uniform, dense), (1, 1, 0), "solid chunk is Uniform");
        assert_eq!(count(&tm, 5), CHUNK_CELLS as i64);
        // Every cell still reads back correctly through the Uniform variant.
        for y in (0..CHUNK_SIZE as u32).step_by(37) {
            for x in (0..CHUNK_SIZE as u32).step_by(37) {
                assert_eq!(tm.get(x, y), 5);
            }
        }
    }

    #[test]
    fn one_differing_set_expands_uniform_to_dense() {
        let mut tm = TileMap::default();
        for y in 0..CHUNK_SIZE as u32 {
            for x in 0..CHUNK_SIZE as u32 { tm.set(x, y, 5); }
        }
        tm.set(0, 0, 6);              // forces Uniform → Dense
        let (_, uniform, dense, _) = tm.stats();
        assert_eq!((uniform, dense), (0, 1));
        assert_eq!(tm.get(0, 0), 6);
        assert_eq!(tm.get(1, 0), 5);
        assert_eq!(count(&tm, 6), 1);
        assert_eq!(count(&tm, 5), CHUNK_CELLS as i64 - 1);
        // compact_pass must NOT re-collapse a genuinely mixed chunk.
        assert_eq!(tm.compact_pass(), 0);
    }

    #[test]
    fn clear_owner_removes_only_that_owner_including_uniform() {
        let mut tm = TileMap::default();
        // Owner 5 fills one whole chunk (→ Uniform); owner 8 paints a few cells elsewhere.
        for y in 0..CHUNK_SIZE as u32 {
            for x in 0..CHUNK_SIZE as u32 { tm.set(x, y, 5); }
        }
        tm.set(300, 300, 8);
        tm.set(301, 300, 8);
        assert_eq!(count(&tm, 5), CHUNK_CELLS as i64);
        tm.clear_owner(5);
        assert_eq!(count(&tm, 5), 0);
        assert_eq!(tm.get(0, 0), 0, "owner-5 territory is blank");
        assert_eq!(count(&tm, 8), 2, "owner 8 untouched");
        assert_eq!(tm.get(300, 300), 8);
    }

    #[test]
    fn clear_empties_everything() {
        let mut tm = TileMap::default();
        tm.set(1, 1, 3);
        tm.set(9000, 9000, 4);
        tm.clear();
        assert_eq!(tm.total_tiles(), 0);
        assert_eq!(tm.get(1, 1), 0);
        assert!(tm.counts.is_empty());
        assert_eq!(tm.stats().0, 0);
    }

    #[test]
    fn freed_index_is_recycled_and_reads_stay_correct() {
        let mut tm = TileMap::default();
        tm.set(2, 2, 100);            // owner 100 → first allocated index
        tm.set(2, 2, 0);              // count(100) → 0, index freed
        assert_eq!(count(&tm, 100), 0);
        tm.set(3, 3, 200);            // owner 200 should recycle the freed index
        tm.set(4, 4, 100);            // owner 100 again
        assert_eq!(tm.get(3, 3), 200);
        assert_eq!(tm.get(4, 4), 100);
        assert_eq!(count(&tm, 200), 1);
        assert_eq!(count(&tm, 100), 1);
    }
}
