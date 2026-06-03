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

/// Under the caller's read lock, convert each dirty chunk key into owner-id form. Cheap relative to
/// PNG encoding (the expensive part), so the lock is held only briefly. Returns `(cx, cy, snap)`.
pub fn snapshot_dirty(tiles: &TileMap, keys: &[u64]) -> Vec<(u32, u32, ChunkSnap)> {
    keys.iter().map(|&k| {
        let (cx, cy) = TileMap::chunk_coords(k);
        let snap = match tiles.view_chunk(k) {
            None => ChunkSnap::Empty,
            Some(ChunkView::Uniform(idx)) => ChunkSnap::Uniform(tiles.palette_owner(idx)),
            Some(ChunkView::Dense(cells)) => {
                let mut v = Vec::with_capacity(cells.len());
                for &c in cells {
                    v.push(if c == 0 { 0 } else { tiles.palette_owner(c) });
                }
                ChunkSnap::Dense(v)
            }
        };
        (cx, cy, snap)
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

/// Encode one chunk → a 256×256 RGBA PNG. Unclaimed/ocean cells are transparent (α=0), so a tile's
/// byte size scales with painted perimeter, not area. `colors` maps owner id → RGB; missing owners
/// fall back to gray. Returns the PNG bytes (off-lock; no `World` access).
pub fn rasterize_chunk(snap: &ChunkSnap, colors: &FxHashMap<u32, [u8; 3]>) -> Vec<u8> {
    let n = CHUNK_DIM * CHUNK_DIM;
    let mut rgba = vec![0u8; n * 4];
    let put = |rgba: &mut [u8], i: usize, owner: u32| {
        if owner == 0 { return; } // leave transparent
        let c = colors.get(&owner).copied().unwrap_or([128, 128, 128]);
        let o = i * 4;
        rgba[o] = c[0]; rgba[o + 1] = c[1]; rgba[o + 2] = c[2]; rgba[o + 3] = 255;
    };
    match snap {
        ChunkSnap::Empty => {} // all transparent
        ChunkSnap::Uniform(owner) => {
            for i in 0..n { put(&mut rgba, i, *owner); }
        }
        ChunkSnap::Dense(cells) => {
            for (i, &owner) in cells.iter().enumerate() { put(&mut rgba, i, owner); }
        }
    }

    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, CHUNK_DIM as u32, CHUNK_DIM as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        // Best-effort: a malformed encode just yields an empty Vec (writer logs + skips).
        if let Ok(mut w) = enc.write_header() {
            let _ = w.write_image_data(&rgba);
        }
    }
    out
}

/// A destination for rasterized tiles. `put` is synchronous (the writer runs on its own OS thread);
/// the R2 impl blocks on an internal runtime.
pub trait SnapshotSink: Send + Sync {
    fn put(&self, key: &str, bytes: &[u8]) -> Result<(), String>;
    fn label(&self) -> &'static str;
}

/// No-op (default when nothing is configured).
pub struct NullSink;
impl SnapshotSink for NullSink {
    fn put(&self, _key: &str, _bytes: &[u8]) -> Result<(), String> { Ok(()) }
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
    fn label(&self) -> &'static str { "local-disk" }
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
        let resp = self.rt
            .block_on(self.bucket.put_object_with_content_type(key, bytes, "image/png"))
            .map_err(|e| e.to_string())?;
        let code = resp.status_code();
        if (200..300).contains(&code) { Ok(()) } else { Err(format!("R2 HTTP {code}")) }
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
}
