use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};

/// A lock-free read view of one chunk's cells, handed to the Phase-6 snapshot rasterizer so it can
/// turn palette indices → owner ids without touching the private `chunks`/`palette` internals.
pub enum ChunkView<'a> {
    /// Every cell is this single nonzero palette index.
    Uniform(u16),
    /// One palette index per cell (0 = unclaimed), row-major 256×256.
    Dense(&'a [u16]),
}

const CHUNK_SHIFT: u32   = 8;
const CHUNK_SIZE:  usize = 1 << CHUNK_SHIFT;        // 256
const CHUNK_MASK:  u32   = (CHUNK_SIZE as u32) - 1;
const CHUNK_CELLS: usize = CHUNK_SIZE * CHUNK_SIZE; // 65 536 cells

/// Public chunk dimension (256) for the Phase-6 rasterizer, which renders one PNG per chunk.
pub const CHUNK_DIM: usize = CHUNK_SIZE;

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
    /// Phase-6 EXACT dirty-chunk set: chunk keys mutated since the last `drain_dirty_chunks`. The
    /// snapshot writer drains this each interval to re-upload only changed chunks (bounds R2
    /// Class-A writes). Seeded with every chunk on boot/restore so a loaded world fully uploads once.
    #[serde(skip)]
    dirty_chunks: FxHashSet<u64>,
    /// player_id → painted-territory bounding box `[min_x, min_y, max_x, max_y]`. Expand-only while
    /// the owner holds ≥1 tile (dropped when their count hits 0). Drives the camera "home region"
    /// pan radius (`build_player_info`) so a player can always see their whole territory. Runtime-only
    /// (rebuildable from cells); restored via `rebuild_bounds`.
    #[serde(skip)]
    bounds: FxHashMap<u32, [u32; 4]>,
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
            dirty_chunks: FxHashSet::default(),
            bounds:    FxHashMap::default(),
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
                self.bounds.remove(&owner);   // last tile gone → reset the pan box on next paint
                if let Some(i) = self.id_to_idx.remove(&owner) {
                    self.free.push(i);
                }
            }
        }
    }

    /// Expand `owner`'s territory bounding box to include `(x, y)` (expand-only; reset when the owner
    /// drops to 0 tiles). Cheap O(1) per paint — keeps the camera home-region radius current.
    #[inline]
    fn expand_bounds(&mut self, owner: u32, x: u32, y: u32) {
        let b = self.bounds.entry(owner).or_insert([x, y, x, y]);
        if x < b[0] { b[0] = x; }
        if y < b[1] { b[1] = y; }
        if x > b[2] { b[2] = x; }
        if y > b[3] { b[3] = y; }
    }

    /// `owner`'s painted-territory bounding box `[min_x, min_y, max_x, max_y]`, or `None` if they
    /// hold no tiles. Used to grow a player's pan radius so they can always see their whole territory.
    pub fn owner_bounds(&self, owner: u32) -> Option<[u32; 4]> {
        self.bounds.get(&owner).copied()
    }

    /// Recompute every owner's bounding box from the live cells. Called after a snapshot restore
    /// (where `bounds` deserialized empty) so returning players keep their grown pan radius.
    pub fn rebuild_bounds(&mut self) {
        self.bounds.clear();
        let keys: Vec<u64> = self.chunks.keys().copied().collect();
        for key in keys {
            let (cx, cy) = Self::chunk_coords(key);
            let (bx, by) = (cx << CHUNK_SHIFT, cy << CHUNK_SHIFT);
            match self.chunks.get(&key) {
                Some(Chunk::Uniform(i)) => {
                    let owner = self.palette[*i as usize];
                    if owner != 0 {
                        self.expand_bounds(owner, bx, by);
                        self.expand_bounds(owner, bx + CHUNK_MASK, by + CHUNK_MASK);
                    }
                }
                Some(Chunk::Dense { cells, .. }) => {
                    // Resolve idx→owner up front (needs &self.palette) so the expand loop can borrow mut.
                    let owned: Vec<(u32, u32)> = cells.iter().enumerate().filter_map(|(li, &i)| {
                        if i == 0 { return None; }
                        let owner = self.palette[i as usize];
                        if owner == 0 { return None; }
                        Some((owner, li as u32))
                    }).collect();
                    for (owner, li) in owned {
                        let x = bx + (li & CHUNK_MASK);
                        let y = by + (li >> CHUNK_SHIFT);
                        self.expand_bounds(owner, x, y);
                    }
                }
                None => {}
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
        self.dirty_chunks.insert(key);
    }

    /// Cumulative chunk-touch transitions since boot (an over-estimate of distinct dirty chunks).
    /// Diff two samples over a window to project Phase-6 R2 Class-A (chunk-upload) volume.
    pub fn dirty_chunk_touches(&self) -> u64 { self.dirty_chunk_touches }

    /// Phase-6: drain the EXACT set of chunk keys mutated since the last drain. The snapshot writer
    /// calls this each interval; chunks not returned are unchanged and need no re-upload.
    pub fn drain_dirty_chunks(&mut self) -> Vec<u64> {
        self.dirty_chunks.drain().collect()
    }

    /// How many distinct chunks are pending upload (for `/egress-stats` / projection).
    pub fn dirty_chunk_len(&self) -> usize { self.dirty_chunks.len() }

    /// Mark every currently-populated chunk dirty (seed on boot / persist-restore so a freshly
    /// loaded world re-uploads its whole painted canvas to R2 exactly once).
    pub fn mark_all_dirty(&mut self) {
        for &k in self.chunks.keys() { self.dirty_chunks.insert(k); }
    }

    /// Lock-free read view of one chunk's cells, or `None` if the chunk is empty (all ocean). The
    /// rasterizer pairs this with `palette_owner` to colour each cell. `key` is a `chunk_key`.
    pub fn view_chunk(&self, key: u64) -> Option<ChunkView<'_>> {
        match self.chunks.get(&key) {
            Some(Chunk::Uniform(i))          => Some(ChunkView::Uniform(*i)),
            Some(Chunk::Dense { cells, .. })  => Some(ChunkView::Dense(cells)),
            None                             => None,
        }
    }

    /// Translate a palette index → owner player id (0 = unclaimed). For the rasterizer's per-cell
    /// colour lookup. Out-of-range indices read as unclaimed.
    #[inline]
    pub fn palette_owner(&self, idx: u16) -> u32 {
        self.palette.get(idx as usize).copied().unwrap_or(0)
    }

    /// Decompose a `chunk_key` back into chunk coords `(cx, cy)` (each = tile coord >> 8). The
    /// snapshot tile key is `/snap/{epoch}/0/{cx}/{cy}.png`.
    #[inline]
    pub fn chunk_coords(key: u64) -> (u32, u32) {
        ((key & 0xFFFF_FFFF) as u32, (key >> 32) as u32)
    }

    /// Build a `chunk_key` from chunk coords `(cx, cy)` — inverse of `chunk_coords`. The Phase-6
    /// super-tile rasterizer needs this to view each constituent chunk of a super-tile by coord.
    #[inline]
    pub fn chunk_key_from_coords(cx: u32, cy: u32) -> u64 {
        ((cy as u64) << 32) | cx as u64
    }

    /// Phase-6 lever B: how many distinct **super-tiles** (S×S chunk blocks) the pending dirty set
    /// coalesces into — i.e. the real R2 Class-A key count the next writer cycle will push. Project
    /// monthly Class-A ≈ this × intervals/mo. `s` must be a power of two ≥ 1.
    pub fn dirty_supertiles_len(&self, s: u32) -> usize {
        if s <= 1 { return self.dirty_chunks.len(); }
        let mut supers: FxHashSet<u64> = FxHashSet::default();
        for &k in &self.dirty_chunks {
            let (cx, cy) = Self::chunk_coords(k);
            supers.insert(((cy / s) as u64) << 32 | (cx / s) as u64);
        }
        supers.len()
    }

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
        self.expand_bounds(new_owner, x, y);
        self.note_dirty(key);
    }

    pub fn clear(&mut self) {
        self.chunks.clear();
        self.counts.clear();
        self.palette.clear();
        self.palette.push(0);   // restore the reserved unclaimed slot
        self.id_to_idx.clear();
        self.free.clear();
        self.dirty_chunks.clear();
        self.last_touched_chunk = None;
        self.bounds.clear();
    }

    /// Clear every tile owned by `owner` (→ unclaimed) and drop the owner's count + index.
    /// Used on queen death — its territory is forfeited and turns blank. A `Uniform` chunk
    /// owned by the loser is dropped whole (no per-cell scan); emptied `Dense` chunks drop too.
    /// `remaining` lets later chunks skip the scan once all of the owner's tiles are found.
    pub fn clear_owner(&mut self, owner: u32) {
        if owner == 0 { return; }
        self.bounds.remove(&owner);   // territory forfeited → reset the pan box
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
        let mut touched: Vec<u64> = Vec::new();
        self.chunks.retain(|&k, chunk| {
            if remaining <= 0 { return true; }
            match chunk {
                Chunk::Uniform(ui) => {
                    if *ui == oi { remaining -= CHUNK_CELLS as i64; touched.push(k); false } else { true }
                }
                Chunk::Dense { cells, occupied } => {
                    if *occupied == 0 { return false; }
                    let mut zeroed: u32 = 0;
                    for c in cells.iter_mut() {
                        if *c == oi { *c = 0; zeroed += 1; }
                    }
                    if zeroed > 0 { touched.push(k); }
                    *occupied -= zeroed;
                    remaining -= zeroed as i64;
                    *occupied != 0
                }
            }
        });
        // Phase-6: a dead queen's forfeited chunks changed → re-upload them next snapshot interval.
        self.dirty_chunks.extend(touched);
        self.counts.remove(&owner);
        self.id_to_idx.remove(&owner);
        self.free.push(oi);
    }

    /// ∥A WAL replay: overwrite a whole chunk from a 256×256 array of owner ids (0 = unclaimed).
    /// Reuses `set` per cell so `counts`/`palette` stay exact. `owners.len()` should be `CHUNK_CELLS`
    /// (shorter → trailing cells cleared). Used only when replaying the write-ahead log on boot.
    pub fn restore_chunk_owners(&mut self, key: u64, owners: &[u32]) {
        let (cx, cy) = Self::chunk_coords(key);
        let (bx, by) = (cx << CHUNK_SHIFT, cy << CHUNK_SHIFT);
        for li in 0..CHUNK_CELLS {
            let owner = owners.get(li).copied().unwrap_or(0);
            let x = bx | (li as u32 & CHUNK_MASK);
            let y = by | ((li as u32 >> CHUNK_SHIFT) & CHUNK_MASK);
            self.set(x, y, owner);
        }
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
    fn owner_bounds_expand_reset_and_rebuild() {
        let mut tm = TileMap::default();
        assert_eq!(tm.owner_bounds(5), None, "no tiles → no bounds");
        tm.set(100, 200, 5);
        tm.set(120, 180, 5);
        tm.set(90, 260, 5);
        // bbox = [min_x, min_y, max_x, max_y] over all of owner 5's cells.
        assert_eq!(tm.owner_bounds(5), Some([90, 180, 120, 260]));

        // Expand-only: clearing the western-most cell does NOT shrink the live bbox.
        tm.set(90, 260, 0);
        assert_eq!(tm.owner_bounds(5), Some([90, 180, 120, 260]));

        // Dropping the owner's last tile resets the box.
        tm.set(100, 200, 0);
        tm.set(120, 180, 0);
        assert_eq!(tm.owner_bounds(5), None, "0 tiles → bounds dropped");

        // rebuild_bounds reconstructs from live cells (covers the post-restore path).
        tm.set(1000, 2000, 7);
        tm.set(1050, 1990, 7);
        tm.bounds.clear();                       // simulate a serde-skipped restore
        assert_eq!(tm.owner_bounds(7), None);
        tm.rebuild_bounds();
        assert_eq!(tm.owner_bounds(7), Some([1000, 1990, 1050, 2000]));
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
