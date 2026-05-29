use rustc_hash::FxHashMap;

const CHUNK_SHIFT: u32  = 8;
const CHUNK_SIZE:  usize = 1 << CHUNK_SHIFT;   // 256
const CHUNK_MASK:  u32   = (CHUNK_SIZE as u32) - 1;
const CHUNK_CELLS: usize = CHUNK_SIZE * CHUNK_SIZE; // 65 536 cells, 256 KB each

struct TileChunk {
    cells:    Box<[u32]>,   // len = CHUNK_CELLS, heap-allocated, avoids stack pressure
    occupied: u32,
}

impl TileChunk {
    fn new() -> Self {
        TileChunk {
            cells:    vec![0u32; CHUNK_CELLS].into_boxed_slice(),
            occupied: 0,
        }
    }
}

pub struct TileMap {
    chunks:      FxHashMap<u64, TileChunk>,
    pub counts:  FxHashMap<u32, i64>,   // player_id → tile count
}

impl Default for TileMap {
    fn default() -> Self {
        TileMap {
            chunks: FxHashMap::default(),
            counts: FxHashMap::default(),
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

    #[inline]
    pub fn get(&self, x: u32, y: u32) -> u32 {
        match self.chunks.get(&Self::chunk_key(x, y)) {
            Some(c) => c.cells[Self::local(x, y)],
            None    => 0,
        }
    }

    pub fn set(&mut self, x: u32, y: u32, new_owner: u32) {
        let key = Self::chunk_key(x, y);
        let idx = Self::local(x, y);

        let old_owner = if new_owner != 0 {
            let chunk = self.chunks.entry(key).or_insert_with(TileChunk::new);
            let old   = chunk.cells[idx];
            if old == new_owner { return; }
            chunk.cells[idx] = new_owner;
            if old == 0 { chunk.occupied += 1; }
            old
        } else {
            let Some(chunk) = self.chunks.get_mut(&key) else { return; };
            let old = chunk.cells[idx];
            if old == 0 { return; }
            chunk.cells[idx] = 0;
            chunk.occupied   -= 1;
            let occupied = chunk.occupied;
            if occupied == 0 { self.chunks.remove(&key); }
            old
        };

        if old_owner != 0 {
            let e = self.counts.entry(old_owner).or_insert(0);
            *e -= 1;
            if *e == 0 { self.counts.remove(&old_owner); }
        }
        if new_owner != 0 {
            *self.counts.entry(new_owner).or_insert(0) += 1;
        }
    }

    pub fn clear(&mut self) {
        self.chunks.clear();
        self.counts.clear();
    }

    #[allow(dead_code)]
    pub fn total_tiles(&self) -> usize {
        self.chunks.values().map(|c| c.occupied as usize).sum()
    }

    #[allow(dead_code)]
    pub fn iter_tiles(&self) -> impl Iterator<Item = (u32, u32, u32)> + '_ {
        self.chunks.iter().flat_map(|(&ck, chunk)| {
            let cx = (ck & 0xFFFF_FFFF) as u32;
            let cy = (ck >> 32) as u32;
            chunk.cells.iter().enumerate().filter_map(move |(idx, &pid)| {
                if pid == 0 { return None; }
                let lx = (idx % CHUNK_SIZE) as u32;
                let ly = (idx / CHUNK_SIZE) as u32;
                Some(((cx << CHUNK_SHIFT) | lx, (cy << CHUNK_SHIFT) | ly, pid))
            })
        })
    }
}

// TileMap is Sync because FxHashMap<u64, TileChunk>: Sync (all fields Sync).
// This is needed for &TileMap to be passed into rayon parallel closures.
unsafe impl Sync for TileMap {}
