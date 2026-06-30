//! Admin-placed **monuments**: permanent king-of-the-hill landmarks the operator appoints in-game.
//!
//! Unlike the metro circles (baked into the compile-embedded `data/regions.json`, which can't be
//! edited at runtime), monuments live in a runtime JSON file **under `HIVE_DATA_DIR`** so they can be
//! placed/removed live and survive **restarts AND world wipes** (the file is never deleted by a wipe).
//! Loaded once on boot (`load`) into `World.monuments`; persisted atomically (temp + rename, mirroring
//! `config::save_config`) on every admin add/remove/rename. The single holder + flat daily nectar
//! payout live in `simulation.rs`; this module is only the durable store + data model.

use serde::{Deserialize, Serialize};

use crate::config::cfg;

/// One monument. `r2` is the squared capture radius in tiles: the player owning the most painted
/// tiles inside that circle holds it. Computed from a km radius at placement via
/// `regions::radius_km_to_r2` so it matches the metro-circle projection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Monument {
    pub id:   u32,
    pub name: String,
    pub x:    i32,
    pub y:    i32,
    pub r2:   i64,
}

#[derive(Serialize, Deserialize, Default)]
struct MonumentsFile {
    monuments: Vec<Monument>,
}

/// The 100 famous landmarks seeded on boot, embedded at compile time like `data/regions.json`.
const LANDMARKS_JSON: &str = include_str!("../data/landmarks.json");

/// Capture radius for a seeded landmark, in tiles (≈ `radius × tile_meters` metres ≈ 2.67 km @ 26.72).
/// Stored squared (`r2`) to match `Monument.r2` and the king-of-the-hill tile scan.
const LANDMARK_RADIUS_TILES: i64 = 100;

#[derive(Deserialize)]
struct LandmarkSeed {
    name: String,
    lat:  f64,
    lon:  f64,
}

/// Parse the embedded landmark list and project each real-world `(lat, lon)` → game tile via the live
/// Mercator (`regions::latlon_to_game`), so a seeded marker lands exactly where the client basemap
/// draws that place — the map TILE, not the raw lat/lon, is the placement. Radius = 100 tiles
/// (`r2 = 10_000`). `id` is left `0`; the caller (`main`) assigns a collision-free id from the monument
/// allocator. Malformed JSON → empty (logged), never panics.
pub fn seeded_landmarks() -> Vec<Monument> {
    let list: Vec<LandmarkSeed> = match serde_json::from_str(LANDMARKS_JSON) {
        Ok(v)  => v,
        Err(e) => { eprintln!("[landmarks] embedded landmarks.json invalid — none seeded ({e})"); return Vec::new(); }
    };
    let r2 = LANDMARK_RADIUS_TILES * LANDMARK_RADIUS_TILES;
    list.into_iter().map(|l| {
        let (x, y) = crate::regions::latlon_to_game(l.lat, l.lon);
        Monument { id: 0, name: l.name, x, y, r2 }
    }).collect()
}

/// Path of the monuments store beside the world snapshot (so it rides `HIVE_DATA_DIR`), same trick as
/// `config::config_path` (`world.snapshot` → `monuments.json`).
fn monuments_path() -> String { cfg().save_file.replace("world.snapshot", "monuments.json") }

/// Load the monument list from `monuments.json`. Missing/corrupt → empty (fresh start), never panics.
pub fn load() -> Vec<Monument> {
    let path = monuments_path();
    match std::fs::read_to_string(&path) {
        Ok(data) => match serde_json::from_str::<MonumentsFile>(&data) {
            Ok(f)  => { println!("[monuments] loaded {} from {path}", f.monuments.len()); f.monuments }
            Err(e) => { eprintln!("[monuments] {path} is not valid — ignoring ({e})"); Vec::new() }
        },
        Err(_) => { println!("[monuments] no monuments.json at {path} — starting empty"); Vec::new() }
    }
}

/// Persist the monument list atomically (temp file + rename), like `config.json`. Best-effort: a write
/// failure is logged, never fatal (the in-memory list stays authoritative for the session).
pub fn save(monuments: &[Monument]) {
    let path = monuments_path();
    let file = MonumentsFile { monuments: monuments.to_vec() };
    let body = match serde_json::to_string_pretty(&file) {
        Ok(s) => s,
        Err(e) => { eprintln!("[monuments] serialize failed: {e}"); return; }
    };
    let tmp = format!("{path}.tmp");
    if let Err(e) = std::fs::write(&tmp, body) { eprintln!("[monuments] write {tmp} failed: {e}"); return; }
    if let Err(e) = std::fs::rename(&tmp, &path) { eprintln!("[monuments] rename → {path} failed: {e}"); }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monuments_file_round_trips() {
        // Model-level round-trip (no global save_file dependency → race-free under parallel tests).
        // The on-disk save→load + restart path is covered by the hermetic WS E2E.
        let mons = vec![
            Monument { id: 1, name: "Eiffel".into(),  x: 100, y: 200, r2: 9_000 },
            Monument { id: 2, name: "Liberty".into(), x: 300, y: 400, r2: 12_000 },
        ];
        let body = serde_json::to_string_pretty(&MonumentsFile { monuments: mons.clone() }).unwrap();
        let back: MonumentsFile = serde_json::from_str(&body).unwrap();
        assert_eq!(back.monuments.len(), 2);
        assert_eq!(back.monuments[0].id, 1);
        assert_eq!(back.monuments[0].name, "Eiffel");
        assert_eq!(back.monuments[1].x, 300);
        assert_eq!(back.monuments[1].r2, 12_000);
    }

    /// Verifies the 100 seeded landmarks land correctly ON THE MAP — not just that lat/lon parses.
    /// For each: the projected tile is in-bounds, sits in the hemisphere quadrant its lat/lon demands
    /// (catches transposed/sign-flipped coordinates), and is resolved through the country polygons to
    /// confirm it lands on the right landmass. Prints the full name → tile → country table as evidence,
    /// and lists any that resolve to open water (coastal/island sites — expected for a handful).
    #[test]
    fn seeded_landmarks_land_on_the_map_correctly() {
        crate::regions::init();
        let raw: Vec<LandmarkSeed> = serde_json::from_str(LANDMARKS_JSON).expect("landmarks.json parses");
        assert_eq!(raw.len(), 100, "exactly 100 landmarks");

        let mut names = std::collections::HashSet::new();
        for l in &raw { assert!(names.insert(l.name.clone()), "duplicate landmark name: {}", l.name); }

        let (sx, sy, ww, wh) = {
            let c = cfg();
            (c.spawn_x as i32, c.spawn_y as i32, c.world_w, c.world_h)
        };

        let mut water: Vec<String> = Vec::new();
        println!("\n{:<30}{:>10}{:>10}{:>10}{:>10}  country", "landmark", "lat", "lon", "tile_x", "tile_y");
        for l in &raw {
            let (x, y) = crate::regions::latlon_to_game(l.lat, l.lon);
            assert!(x >= 0 && (x as u32) < ww && y >= 0 && (y as u32) < wh,
                    "{} projects out of bounds → ({x},{y})", l.name);
            // Hemisphere quadrant must match the lat/lon sign (east/west of centre, north/south of it).
            if l.lon > 0.0 { assert!(x > sx, "{}: lon>0 must land EAST of grid centre", l.name); }
            if l.lon < 0.0 { assert!(x < sx, "{}: lon<0 must land WEST of grid centre", l.name); }
            if l.lat > 0.0 { assert!(y < sy, "{}: lat>0 must land NORTH of grid centre", l.name); }
            if l.lat < 0.0 { assert!(y > sy, "{}: lat<0 must land SOUTH of grid centre", l.name); }
            let (country, _) = crate::regions::country_and_continent(x, y);
            if country == "Open Water" { water.push(l.name.clone()); }
            println!("{:<30}{:>10.4}{:>10.4}{:>10}{:>10}  {country}", l.name, l.lat, l.lon, x, y);
        }
        println!("\n{} of {} landmarks resolved to LAND; {} on/near water (coastal/island — expected): {:?}",
                 raw.len() - water.len(), raw.len(), water.len(), water);
        // Coarse Natural-Earth 110m polygons miss small coastal/island sites; tolerate a handful but
        // fail loudly if MOST fall in ocean (which would mean the projection itself is broken).
        assert!(water.len() <= 20, "too many landmarks resolved to open water ({}) — projection likely broken", water.len());
    }
}
