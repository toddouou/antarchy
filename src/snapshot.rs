//! **Phase 6 — R2 snapshot tiles.** Rasterizes painted territory into immutable per-chunk PNG tiles
//! and ships them to a sink (Cloudflare R2 over the S3 API, a local dir for inspection, or a no-op).
//! The browser fetches these tiles **directly** ($0-egress R2 read) for initial-load / reconnect /
//! far-zoom; the live in-view frontier stays WS-keyframe-authoritative (see EGRESS_PLAN §3).
//!
//! Keying is **game-space** (NOT Mercator): one tile per native 256×256 chunk,
//! `snap/{epoch}/0/{cx}/{cy}.png` where `(cx,cy) = (x>>8, y>>8)`.
//!
//! The whole pipeline is **runtime-gated** (`SNAPSHOT_CDN` + R2 creds, or `HIVE_SNAP_DIR`); with no
//! sink configured nothing is rasterized or uploaded, so this ships dormant until the operator wires
//! Cloudflare creds — zero rebuild, zero overhead on the live server.

use crate::config::{self, R2Config};
use crate::tile_map::{ChunkView, TileMap, CHUNK_DIM};
use rustc_hash::FxHashMap;

use s3::creds::Credentials;
use s3::region::Region;
use s3::Bucket;

/// One chunk captured in owner-id form **under the read lock**, so the PNG encode + upload runs
/// off-lock. Cleared chunks become `Empty` → a fully transparent tile (territory blanked).
pub enum ChunkSnap {
    Empty,
    Uniform(u32),
    Dense(Vec<u32>),
}

/// View one chunk by key and convert its palette indices → owner ids (`None` chunk → `Empty`).
/// Shared by the per-chunk (`snapshot_dirty`) and super-tile (`snapshot_supertiles`) capture paths.
fn capture_chunk(tiles: &TileMap, key: u64) -> ChunkSnap {
    match tiles.view_chunk(key) {
        None => ChunkSnap::Empty,
        Some(ChunkView::Uniform(idx)) => ChunkSnap::Uniform(tiles.palette_owner(idx)),
        Some(ChunkView::Dense(cells)) => {
            let mut v = Vec::with_capacity(cells.len());
            for &c in cells {
                v.push(if c == 0 { 0 } else { tiles.palette_owner(c) });
            }
            ChunkSnap::Dense(v)
        }
    }
}

/// Under the caller's read lock, convert each dirty chunk key into owner-id form. Cheap relative to
/// PNG encoding (the expensive part), so the lock is held only briefly. Returns `(cx, cy, snap)`.
pub fn snapshot_dirty(tiles: &TileMap, keys: &[u64]) -> Vec<(u32, u32, ChunkSnap)> {
    keys.iter().map(|&k| {
        let (cx, cy) = TileMap::chunk_coords(k);
        (cx, cy, capture_chunk(tiles, k))
    }).collect()
}

/// A super-tile (R2 Class-A lever B): one PNG covering an S×S block of native chunks. `subs` holds
/// each constituent chunk in owner-id form with its in-block offset `(scx, scy) ∈ 0..s`. `all_empty`
/// lets the writer skip encoding a fully-transparent PNG and `delete` the key instead (free op).
pub struct SuperSnap {
    pub sx: u32,
    pub sy: u32,
    pub subs: Vec<(u32, u32, ChunkSnap)>,
    pub all_empty: bool,
}

/// Under the caller's read lock, capture each requested super-tile `(sx,sy)` into owner-id form by
/// viewing all `s*s` constituent chunks (missing chunk → `Empty`/transparent — a solid sub-chunk
/// that didn't re-dirty this cycle still has to render, so every constituent is captured). The PNG
/// encode runs off-lock via `rasterize_supertile`. `s` is a power of two ≥ 1; `s == 1` reduces to one
/// chunk per tile, identical to the legacy per-chunk path.
pub fn snapshot_supertiles(tiles: &TileMap, super_keys: &[(u32, u32)], s: u32) -> Vec<SuperSnap> {
    super_keys.iter().map(|&(sx, sy)| {
        let mut subs = Vec::with_capacity((s * s) as usize);
        let mut all_empty = true;
        for scy in 0..s {
            for scx in 0..s {
                let key = TileMap::chunk_key_from_coords(sx * s + scx, sy * s + scy);
                let snap = capture_chunk(tiles, key);
                if !matches!(snap, ChunkSnap::Empty) { all_empty = false; }
                subs.push((scx, scy, snap));
            }
        }
        SuperSnap { sx, sy, subs, all_empty }
    }).collect()
}

/// Parse `#rrggbb` (or `rrggbb`) → RGB. Unknown/short strings fall back to mid-gray so territory is
/// still visible rather than invisible.
pub fn parse_hex_color(s: &str) -> [u8; 3] {
    let h = s.trim().trim_start_matches('#');
    if h.len() >= 6 {
        if let (Ok(r), Ok(g), Ok(b)) = (
            u8::from_str_radix(&h[0..2], 16),
            u8::from_str_radix(&h[2..4], 16),
            u8::from_str_radix(&h[4..6], 16),
        ) {
            return [r, g, b];
        }
    }
    [128, 128, 128]
}

/// Blit one 256×256 chunk's cells into a `dim_px × dim_px` RGBA buffer at pixel offset `(ox, oy)`.
/// Unclaimed/ocean cells (owner 0) are left untouched (transparent), so a tile's byte size scales
/// with painted perimeter, not area. `colors` maps owner id → RGB; missing owners fall back to gray.
/// Shared by the single-chunk and super-tile rasterizers so the colour/transparency rules live once.
fn blit_chunk_into(rgba: &mut [u8], dim_px: usize, ox: usize, oy: usize,
                   snap: &ChunkSnap, colors: &FxHashMap<u32, [u8; 3]>) {
    let put = |rgba: &mut [u8], px: usize, py: usize, owner: u32| {
        if owner == 0 { return; } // leave transparent
        let c = colors.get(&owner).copied().unwrap_or([128, 128, 128]);
        let o = (py * dim_px + px) * 4;
        rgba[o] = c[0]; rgba[o + 1] = c[1]; rgba[o + 2] = c[2]; rgba[o + 3] = 255;
    };
    match snap {
        ChunkSnap::Empty => {} // all transparent
        ChunkSnap::Uniform(owner) => {
            for ly in 0..CHUNK_DIM { for lx in 0..CHUNK_DIM { put(rgba, ox + lx, oy + ly, *owner); } }
        }
        ChunkSnap::Dense(cells) => {
            for (i, &owner) in cells.iter().enumerate() {
                put(rgba, ox + (i % CHUNK_DIM), oy + (i / CHUNK_DIM), owner);
            }
        }
    }
}

/// Encode a `w × h` RGBA buffer → PNG bytes. Best-effort: a malformed encode just yields an empty
/// Vec (the writer logs + skips).
fn encode_png(rgba: &[u8], w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        if let Ok(mut writer) = enc.write_header() {
            let _ = writer.write_image_data(rgba);
        }
    }
    out
}

/// Encode one chunk → a 256×256 RGBA PNG. Returns the PNG bytes (off-lock; no `World` access).
pub fn rasterize_chunk(snap: &ChunkSnap, colors: &FxHashMap<u32, [u8; 3]>) -> Vec<u8> {
    let mut rgba = vec![0u8; CHUNK_DIM * CHUNK_DIM * 4];
    blit_chunk_into(&mut rgba, CHUNK_DIM, 0, 0, snap, colors);
    encode_png(&rgba, CHUNK_DIM as u32, CHUNK_DIM as u32)
}

/// Encode one super-tile → an `(s*256)²` RGBA PNG by blitting each constituent chunk at its pixel
/// offset `(scx*256, scy*256)`. Off-lock (no `World` access). Transparent for unclaimed/ocean cells,
/// so byte size still tracks painted perimeter despite the larger canvas.
pub fn rasterize_supertile(snap: &SuperSnap, colors: &FxHashMap<u32, [u8; 3]>, s: u32) -> Vec<u8> {
    let dim_px = s as usize * CHUNK_DIM;
    let mut rgba = vec![0u8; dim_px * dim_px * 4];
    for (scx, scy, sub) in &snap.subs {
        blit_chunk_into(&mut rgba, dim_px, *scx as usize * CHUNK_DIM, *scy as usize * CHUNK_DIM, sub, colors);
    }
    encode_png(&rgba, dim_px as u32, dim_px as u32)
}

/// A destination for rasterized tiles. `put` is synchronous (the writer runs on its own OS thread);
/// the R2 impl blocks on an internal runtime.
pub trait SnapshotSink: Send + Sync {
    fn put(&self, key: &str, bytes: &[u8]) -> Result<(), String>;
    /// Delete one object (reclaim a chunk whose territory was fully cleared → no tile needed).
    /// Idempotent: a missing key is success (the client renders a 404 as unpainted/ocean anyway).
    fn delete(&self, key: &str) -> Result<(), String>;
    /// Delete every object under `prefix` (e.g. `"snap/123/"`). Returns the count removed. Used on a
    /// world wipe to drop the whole outgoing epoch's tile generation off R2.
    fn delete_prefix(&self, prefix: &str) -> Result<u32, String>;
    fn label(&self) -> &'static str;
}

/// No-op (default when nothing is configured).
pub struct NullSink;
impl SnapshotSink for NullSink {
    fn put(&self, _key: &str, _bytes: &[u8]) -> Result<(), String> { Ok(()) }
    fn delete(&self, _key: &str) -> Result<(), String> { Ok(()) }
    fn delete_prefix(&self, _prefix: &str) -> Result<u32, String> { Ok(0) }
    fn label(&self) -> &'static str { "null" }
}

/// Dev sink: writes tiles to `<root>/<key>` on local disk so the rasterizer can be inspected with
/// no Cloudflare account. Creates parent dirs.
pub struct LocalDiskSink {
    pub root: std::path::PathBuf,
}
impl SnapshotSink for LocalDiskSink {
    fn put(&self, key: &str, bytes: &[u8]) -> Result<(), String> {
        let path = self.root.join(key);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, bytes).map_err(|e| e.to_string())
    }
    fn delete(&self, key: &str) -> Result<(), String> {
        match std::fs::remove_file(self.root.join(key)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()), // already gone
            Err(e) => Err(e.to_string()),
        }
    }
    fn delete_prefix(&self, prefix: &str) -> Result<u32, String> {
        let dir = self.root.join(prefix);
        let n = count_files_rec(&dir);
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => Ok(n),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e.to_string()),
        }
    }
    fn label(&self) -> &'static str { "local-disk" }
}

/// Recursively count regular files under `dir` (for the local-disk sink's `delete_prefix` tally).
fn count_files_rec(dir: &std::path::Path) -> u32 {
    let mut n = 0;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() { n += count_files_rec(&p); } else { n += 1; }
        }
    }
    n
}

/// Cloudflare R2 sink (S3 API). Owns a current-thread tokio runtime so the sync `put` can block on
/// the async upload from the writer's OS thread.
pub struct R2Sink {
    rt: tokio::runtime::Runtime,
    bucket: Box<Bucket>,
}
impl R2Sink {
    pub fn new(c: &R2Config) -> Result<Self, String> {
        let creds = Credentials::new(Some(&c.access_key), Some(&c.secret_key), None, None, None)
            .map_err(|e| format!("R2 creds: {e}"))?;
        // Custom S3 endpoint (the account's r2.cloudflarestorage.com); path-style addressing.
        let region = Region::Custom { region: "auto".to_string(), endpoint: c.endpoint.clone() };
        let bucket = Bucket::new(&c.bucket, region, creds)
            .map_err(|e| format!("R2 bucket: {e}"))?
            .with_path_style();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all().build().map_err(|e| format!("R2 runtime: {e}"))?;
        Ok(R2Sink { rt, bucket })
    }
}
impl SnapshotSink for R2Sink {
    fn put(&self, key: &str, bytes: &[u8]) -> Result<(), String> {
        crate::metrics::record_class_a(1); // PutObject = Class A ($4.50/M)
        let resp = self.rt
            .block_on(self.bucket.put_object_with_content_type(key, bytes, "image/png"))
            .map_err(|e| e.to_string())?;
        let code = resp.status_code();
        if (200..300).contains(&code) { Ok(()) } else { Err(format!("R2 HTTP {code}")) }
    }
    fn delete(&self, key: &str) -> Result<(), String> {
        crate::metrics::record_r2_delete(1); // DeleteObject is free, but track for visibility
        let resp = self.rt.block_on(self.bucket.delete_object(key)).map_err(|e| e.to_string())?;
        let code = resp.status_code();
        // 404 = already absent (e.g. an empty chunk that was never uploaded) → treat as success.
        if (200..300).contains(&code) || code == 404 { Ok(()) } else { Err(format!("R2 HTTP {code}")) }
    }
    fn delete_prefix(&self, prefix: &str) -> Result<u32, String> {
        let prefix = prefix.to_string();
        self.rt.block_on(async {
            // rust-s3 paginates internally and returns every page's listing.
            let pages = self.bucket.list(prefix, None).await.map_err(|e| e.to_string())?;
            crate::metrics::record_class_a(pages.len() as u64); // each ListObjects page = Class A
            let keys: Vec<String> = pages.into_iter()
                .flat_map(|p| p.contents.into_iter().map(|o| o.key))
                .collect();
            crate::metrics::record_r2_delete(keys.len() as u64);
            // Delete with bounded concurrency (the current-thread runtime still interleaves the I/O),
            // tallying objects that came back 2xx or 404.
            let bucket = &self.bucket;
            use futures_util::stream::{self, StreamExt};
            let deleted = stream::iter(keys)
                .map(|k| async move {
                    matches!(bucket.delete_object(k).await, Ok(r) if {
                        let c = r.status_code(); (200..300).contains(&c) || c == 404
                    })
                })
                .buffer_unordered(32)
                .fold(0u32, |acc, ok| async move { acc + ok as u32 })
                .await;
            Ok(deleted)
        })
    }
    fn label(&self) -> &'static str { "r2" }
}

/// Is any sink configured? (Decides whether `main` spawns the snapshot-writer thread at all — so an
/// unconfigured server pays zero cost.)
pub fn sink_active() -> bool {
    (config::snapshot_cdn_enabled() && config::r2_config().is_some()) || config::snapshot_dir().is_some()
}

/// Build the active sink, preferring R2 (when `SNAPSHOT_CDN=on` + creds), else local-disk
/// (`HIVE_SNAP_DIR`), else null. Logs the choice once.
pub fn make_sink() -> Box<dyn SnapshotSink> {
    if config::snapshot_cdn_enabled() {
        if let Some(r2) = config::r2_config() {
            match R2Sink::new(&r2) {
                Ok(s) => { println!("[snapshot] R2 sink active (bucket {})", r2.bucket); return Box::new(s); }
                Err(e) => eprintln!("[snapshot] R2 init failed ({e}); falling back"),
            }
        } else {
            eprintln!("[snapshot] SNAPSHOT_CDN set but R2 creds incomplete; falling back");
        }
    }
    if let Some(dir) = config::snapshot_dir() {
        println!("[snapshot] local-disk sink at {dir}");
        return Box::new(LocalDiskSink { root: dir.into() });
    }
    Box::new(NullSink)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rasterizes_uniform_and_dense_to_valid_png() {
        let mut colors = FxHashMap::default();
        colors.insert(7u32, [255, 0, 0]);
        let png = rasterize_chunk(&ChunkSnap::Uniform(7), &colors);
        // PNG magic number.
        assert_eq!(&png[0..8], &[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']);
        assert!(png.len() > 50);

        let mut cells = vec![0u32; CHUNK_DIM * CHUNK_DIM];
        cells[0] = 7;
        let png2 = rasterize_chunk(&ChunkSnap::Dense(cells), &colors);
        assert_eq!(&png2[0..4], &[0x89, b'P', b'N', b'G']);

        // Empty chunk still produces a (transparent) valid PNG.
        let png3 = rasterize_chunk(&ChunkSnap::Empty, &colors);
        assert_eq!(&png3[0..4], &[0x89, b'P', b'N', b'G']);
    }

    #[test]
    fn parses_hex_colors() {
        assert_eq!(parse_hex_color("#ff8c42"), [0xff, 0x8c, 0x42]);
        assert_eq!(parse_hex_color("06d6a0"), [0x06, 0xd6, 0xa0]);
        assert_eq!(parse_hex_color("bogus"), [128, 128, 128]);
    }

    #[test]
    fn local_disk_delete_and_delete_prefix() {
        let root = std::env::temp_dir().join(format!("hive-snap-del-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let sink = LocalDiskSink { root: root.clone() };

        sink.put("snap/9/0/1/2.png", b"a").unwrap();
        sink.put("snap/9/0/3/4.png", b"b").unwrap();
        assert!(root.join("snap/9/0/1/2.png").exists());

        // delete one object
        sink.delete("snap/9/0/1/2.png").unwrap();
        assert!(!root.join("snap/9/0/1/2.png").exists());
        // deleting a missing key is idempotent success
        sink.delete("snap/9/0/1/2.png").unwrap();

        // delete_prefix reclaims the rest of the epoch and reports the count
        let n = sink.delete_prefix("snap/9/").unwrap();
        assert_eq!(n, 1, "only snap/9/0/3/4.png remained");
        assert!(!root.join("snap/9").exists());
        // a missing prefix is success → 0
        assert_eq!(sink.delete_prefix("snap/9/").unwrap(), 0);

        let _ = std::fs::remove_dir_all(&root);
    }

    // When a queen dies, a chunk it *shared* with other players must be re-rendered (not deleted);
    // only a chunk it owned alone goes `Empty`. This is what lets the writer delete vs. regenerate.
    #[test]
    fn shared_chunk_regenerates_while_solo_chunk_snaps_empty() {
        use crate::tile_map::TileMap;
        let mut t = TileMap::default();
        t.set(0, 0, 7);     // chunk (0,0): owner 7 …
        t.set(1, 0, 8);     // chunk (0,0): … shared with owner 8
        t.set(256, 0, 7);   // chunk (1,0): owner 7 alone
        let _ = t.drain_dirty_chunks(); // discard the paint-time dirty set

        t.clear_owner(7); // queen 7 dies → its tiles cleared
        let keys = t.drain_dirty_chunks();
        let snaps = snapshot_dirty(&t, &keys);

        let shared = snaps.iter().find(|(cx, cy, _)| *cx == 0 && *cy == 0).expect("shared chunk dirty");
        let solo   = snaps.iter().find(|(cx, cy, _)| *cx == 1 && *cy == 0).expect("solo chunk dirty");
        assert!(!matches!(shared.2, ChunkSnap::Empty), "shared chunk must regenerate, not delete");
        assert!( matches!(solo.2,   ChunkSnap::Empty), "fully-cleared chunk must snap Empty (→ delete)");
    }

    // Lever B: a super-tile composites its S×S constituent chunks at the right pixel offsets. Catches
    // the half-chunk-offset alignment bug — paint only chunk (1,0); its cells must land in the RIGHT
    // sub-block of the 512×512 PNG, and the unpainted (0,0) sub-block must stay transparent.
    #[test]
    fn supertile_blits_subchunks_at_correct_offset() {
        use crate::tile_map::TileMap;
        let mut colors = FxHashMap::default();
        colors.insert(7u32, [255, 0, 0]);

        let mut t = TileMap::default();
        for y in 0..256u32 { for x in 256..512u32 { t.set(x, y, 7); } } // fill chunk (1,0)

        let s = 2u32; // super-tile (0,0) covers chunks (0,0),(1,0),(0,1),(1,1)
        let snaps = snapshot_supertiles(&t, &[(0, 0)], s);
        assert_eq!(snaps.len(), 1);
        assert!(!snaps[0].all_empty);

        let png = rasterize_supertile(&snaps[0], &colors, s);
        assert_eq!(&png[0..4], &[0x89, b'P', b'N', b'G']);

        // Decode and probe pixels.
        let mut reader = png::Decoder::new(&png[..]).read_info().expect("decode header");
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).expect("decode frame");
        assert_eq!((info.width, info.height), (512, 512), "S=2 → 512px PNG");
        let px = |x: usize, y: usize| {
            let o = (y * 512 + x) * 4;
            [buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]
        };
        assert_eq!(px(300, 10), [255, 0, 0, 255], "chunk (1,0) cells land in the right sub-block, opaque");
        assert_eq!(px(10, 10)[3], 0, "unpainted chunk (0,0) sub-block stays transparent");
    }

    #[test]
    fn supertile_all_empty_when_no_chunks_painted() {
        let t = crate::tile_map::TileMap::default();
        let snaps = snapshot_supertiles(&t, &[(5, 5)], 4);
        assert_eq!(snaps.len(), 1);
        assert!(snaps[0].all_empty, "no painted chunks → all_empty (writer deletes the key, a free op)");
        assert_eq!(snaps[0].subs.len(), 16, "S=4 captures all 16 constituent chunks");
    }

    // End-to-end of everything the writer does (minus the loop): paint a multi-chunk block, coalesce
    // dirty chunks → super-tile keys exactly like `snapshot_writer_loop`, rasterize at S=4, and write
    // through the real LocalDiskSink. Asserts the on-disk key path + that the PNG is 1024×1024 + that
    // coalescing genuinely reduces the Class-A key count (9 painted chunks → 1 super-tile).
    #[test]
    fn writer_pipeline_writes_1024px_supertiles_and_coalesces() {
        use crate::tile_map::TileMap;
        let root = std::env::temp_dir().join(format!("hive-snap-super-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let sink = LocalDiskSink { root: root.clone() };
        let mut colors = FxHashMap::default();
        colors.insert(7u32, [10, 200, 60]);

        // Tiles [0,600)² touch chunks (0..=2, 0..=2) = 9 native chunks, all inside super-tile (0,0) at S=4.
        let mut t = TileMap::default();
        for y in 0..600u32 { for x in 0..600u32 { t.set(x, y, 7); } }

        let s = 4u32;
        let keys = t.drain_dirty_chunks();
        let mut set = std::collections::HashSet::new();
        for &k in &keys { let (cx, cy) = TileMap::chunk_coords(k); set.insert((cx / s, cy / s)); }
        let super_keys: Vec<(u32, u32)> = set.into_iter().collect();
        assert_eq!(keys.len(), 9, "600² spans a 3×3 chunk block");
        assert_eq!(super_keys.len(), 1, "all 9 chunks coalesce to one S=4 super-tile (Class-A 9→1)");

        let epoch = 42u64;
        for snap in &snapshot_supertiles(&t, &super_keys, s) {
            assert!(!snap.all_empty);
            let key = format!("snap/{epoch}/0/{}/{}.png", snap.sx, snap.sy);
            sink.put(&key, &rasterize_supertile(snap, &colors, s)).unwrap();
            let bytes = std::fs::read(root.join(&key)).expect("super-tile PNG on disk at the keyed path");
            let mut reader = png::Decoder::new(&bytes[..]).read_info().unwrap();
            let info = reader.next_frame(&mut vec![0u8; reader.output_buffer_size()]).unwrap();
            assert_eq!((info.width, info.height), (1024, 1024), "S=4 → 1024px PNG on disk");
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
