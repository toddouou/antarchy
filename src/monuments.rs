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
}
