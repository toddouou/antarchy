//! Region tagging: which **metro** (fightable "pride" zone) or **country** a world tile sits in.
//!
//! Metros come from the editable `data/regions.json` (10 starters; embedded via `include_str!`,
//! so adding regions = edit + rebuild). Everywhere outside every metro radius falls back to the
//! country name via point-in-polygon over Natural Earth `data/countries.geojson`. Both data files
//! are parsed once at startup (`init()`), projected with the SAME Mercator math as the client
//! (`latLonToGame` / `gameToLatLon`), and cached behind a `OnceLock`.

use std::f64::consts::{PI, FRAC_PI_2, FRAC_PI_4};
use std::sync::OnceLock;

use serde::Deserialize;
use serde_json::Value;

use crate::config::cfg;

const METROS_JSON: &str       = include_str!("../data/regions.json");
const COUNTRIES_GEOJSON: &str = include_str!("../data/countries.geojson");

#[derive(Deserialize)]
struct MetroDef { name: String, lat: f64, lon: f64, radius_km: f64 }
#[derive(Deserialize)]
struct MetrosFile { metros: Vec<MetroDef> }

struct Metro { name: String, cx: i32, cy: i32, r2: i64 }

struct Country {
    name: String,
    continent: String,
    /// ISO-3166 alpha-2 code, lowercase (e.g. "fr", "us"). The Natural Earth 110m dataset
    /// uses "-99" for several countries (France, Norway, Kosovo, …); the build step applies a
    /// small hardcoded override table to recover correct codes for those cases.
    iso_a2: String,
    min_lon: f64, min_lat: f64, max_lon: f64, max_lat: f64,
    /// Each polygon is a list of rings (outer + holes); ring = list of (lon, lat).
    polygons: Vec<Vec<Vec<(f64, f64)>>>,
}

pub struct Regions {
    metros: Vec<Metro>,
    countries: Vec<Country>,
    /// country name → (iso_a2, continent) — built once during `build()`.
    country_iso: std::collections::HashMap<String, (String, String)>,
}

static REGIONS: OnceLock<Regions> = OnceLock::new();

/// Build the region index eagerly (parses the geojson once). Call at startup so the first
/// queen placement never pays the parse cost under the world lock.
pub fn init() { let _ = regions(); }
fn regions() -> &'static Regions { REGIONS.get_or_init(build) }

// ---- Projection (mirror client latLonToGame / gameToLatLon) ----------------

fn proj_params() -> (f64, f64, f64, f64, f64) {
    let c = cfg();
    (c.capitol_lat, c.capitol_lon, c.tile_meters, c.spawn_x as f64, c.spawn_y as f64)
}

fn lat_lon_to_game(lat: f64, lon: f64) -> (i32, i32) {
    let (cap_lat, cap_lon, tm, sx, sy) = proj_params();
    let cos_lat = { let c = (cap_lat * PI / 180.0).cos(); if c == 0.0 { 1.0 } else { c } };
    let m_y  = (lat * PI / 360.0 + FRAC_PI_4).tan().ln();
    let m_y0 = (cap_lat * PI / 360.0 + FRAC_PI_4).tan().ln();
    let x = (sx + (lon - cap_lon) * 111111.0 * cos_lat / tm).round() as i32;
    let y = (sy - (m_y - m_y0) * 111111.0 * (180.0 / PI) * cos_lat / tm).round() as i32;
    (x, y)
}

fn game_to_lat_lon(x: i32, y: i32) -> (f64, f64) {
    let (cap_lat, cap_lon, tm, sx, sy) = proj_params();
    let cos_lat = { let c = (cap_lat * PI / 180.0).cos(); if c == 0.0 { 1.0 } else { c } };
    let m_y0 = (cap_lat * PI / 360.0 + FRAC_PI_4).tan().ln();
    let m_y = m_y0 + (sy - y as f64) * tm / (111111.0 * (180.0 / PI) * cos_lat);
    let lat = (2.0 * m_y.exp().atan() - FRAC_PI_2) * 180.0 / PI;
    let lon = cap_lon + (x as f64 - sx) * tm / (111111.0 * cos_lat);
    (lat, lon)
}

// ---- Build -----------------------------------------------------------------

/// Hardcoded overrides for Natural Earth 110m countries whose `ISO_A2` property is "-99".
/// Keyed by the `NAME`/`ADMIN` property value as it appears in the GeoJSON; value is the
/// correct lowercase ISO-3166-1 alpha-2 code. Extend as needed.
fn iso_override(name: &str) -> Option<&'static str> {
    // Natural Earth 110m known -99 cases (verified against ISO-3166 registry + NE docs).
    match name {
        "France"          => Some("fr"),
        "Norway"          => Some("no"),
        "Kosovo"          => Some("xk"),  // provisional IANA/Unicode code
        "Northern Cyprus" => Some("cy"),  // treated as Cyprus for flag purposes
        "Somaliland"      => Some("so"),  // unrecognised; map to Somalia
        "N. Cyprus"       => Some("cy"),
        _                 => None,
    }
}

fn build() -> Regions {
    let mf: MetrosFile = serde_json::from_str(METROS_JSON).unwrap_or(MetrosFile { metros: vec![] });
    let tm = cfg().tile_meters;
    let metros: Vec<Metro> = mf.metros.into_iter().map(|m| {
        let (cx, cy) = lat_lon_to_game(m.lat, m.lon);
        // Mercator is conformal: local metres-per-tile ≈ tile_meters·cos(lat). Convert the
        // metro's ground radius into the (distorted) tile grid so the circle covers the city.
        let cos = (m.lat * PI / 180.0).cos().abs().max(0.05);
        let r_tiles = m.radius_km * 1000.0 / (tm * cos);
        Metro { name: m.name, cx, cy, r2: (r_tiles * r_tiles) as i64 }
    }).collect();

    let mut countries = Vec::new();
    if let Ok(v) = serde_json::from_str::<Value>(COUNTRIES_GEOJSON) {
        if let Some(feats) = v.get("features").and_then(Value::as_array) {
            for f in feats {
                let name = f.pointer("/properties/NAME").and_then(Value::as_str)
                    .or_else(|| f.pointer("/properties/ADMIN").and_then(Value::as_str))
                    .unwrap_or("Unknown").to_string();
                let continent = f.pointer("/properties/CONTINENT").and_then(Value::as_str)
                    .unwrap_or("").to_string();

                // Resolve ISO alpha-2: prefer the GeoJSON property; fall back to the override
                // table for the known -99 / empty cases in Natural Earth 110m.
                let raw_iso = f.pointer("/properties/ISO_A2").and_then(Value::as_str)
                    .unwrap_or("-99");
                let iso_a2 = if raw_iso == "-99" || raw_iso.is_empty() {
                    iso_override(&name).unwrap_or("").to_string()
                } else {
                    raw_iso.to_lowercase()
                };

                let Some(geom) = f.get("geometry") else { continue };
                let gtype = geom.get("type").and_then(Value::as_str).unwrap_or("");
                let Some(coords) = geom.get("coordinates") else { continue };

                let mut polygons: Vec<Vec<Vec<(f64, f64)>>> = Vec::new();
                if gtype == "Polygon" {
                    if let Some(poly) = parse_polygon(coords) { polygons.push(poly); }
                } else if gtype == "MultiPolygon" {
                    if let Some(arr) = coords.as_array() {
                        for poly_v in arr {
                            if let Some(poly) = parse_polygon(poly_v) { polygons.push(poly); }
                        }
                    }
                }
                if polygons.is_empty() { continue; }

                let (mut mnx, mut mny, mut mxx, mut mxy) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
                for poly in &polygons { for ring in poly { for &(lon, lat) in ring {
                    if lon < mnx { mnx = lon; }
                    if lon > mxx { mxx = lon; }
                    if lat < mny { mny = lat; }
                    if lat > mxy { mxy = lat; }
                }}}
                countries.push(Country { name, continent, iso_a2, min_lon: mnx, min_lat: mny,
                    max_lon: mxx, max_lat: mxy, polygons });
            }
        }
    }

    // Build the name→(iso, continent) lookup map once, so `iso_and_continent_for_country`
    // is an O(1) hash probe instead of an O(countries) linear scan.
    let country_iso: std::collections::HashMap<String, (String, String)> = countries.iter()
        .map(|c| (c.name.clone(), (c.iso_a2.clone(), c.continent.clone())))
        .collect();

    println!("[regions] {} metros, {} countries", metros.len(), countries.len());
    Regions { metros, countries, country_iso }
}

fn parse_polygon(v: &Value) -> Option<Vec<Vec<(f64, f64)>>> {
    let rings = v.as_array()?;
    let mut out = Vec::with_capacity(rings.len());
    for ring_v in rings {
        let pts = ring_v.as_array()?;
        let mut ring = Vec::with_capacity(pts.len());
        for p in pts {
            let pa = p.as_array()?;
            ring.push((pa.first()?.as_f64()?, pa.get(1)?.as_f64()?));
        }
        out.push(ring);
    }
    Some(out)
}

/// Even-odd ray cast over all rings of one polygon — outer + holes together gives correct
/// hole handling (a point inside a hole flips parity back to outside).
fn point_in_polygon(lon: f64, lat: f64, poly: &[Vec<(f64, f64)>]) -> bool {
    let mut inside = false;
    for ring in poly {
        let n = ring.len();
        if n < 3 { continue; }
        let mut j = n - 1;
        for i in 0..n {
            let (xi, yi) = ring[i];
            let (xj, yj) = ring[j];
            if ((yi > lat) != (yj > lat))
                && (lon < (xj - xi) * (lat - yi) / (yj - yi) + xi) {
                inside = !inside;
            }
            j = i;
        }
    }
    inside
}

// ---- Public API ------------------------------------------------------------

/// Region name for a world tile: nearest containing metro, else country, else "Open Water".
pub fn region_for(x: i32, y: i32) -> String {
    region_and_continent(x, y).0
}

/// (region_name, continent). Metros resolve their continent via the underlying country so the
/// Passport feature can still credit a continent for a metro hit.
pub fn region_and_continent(x: i32, y: i32) -> (String, String) {
    let r = regions();
    let mut best: Option<(&Metro, i64)> = None;
    for m in &r.metros {
        let dx = (x - m.cx) as i64;
        let dy = (y - m.cy) as i64;
        let d2 = dx * dx + dy * dy;
        if d2 <= m.r2 && best.is_none_or(|(_, bd)| d2 < bd) {
            best = Some((m, d2));
        }
    }
    if let Some((m, _)) = best {
        let (_, cont) = country_and_continent(x, y);
        return (m.name.clone(), cont);
    }
    country_and_continent(x, y)
}

/// The underlying **country** (+ its continent) at a tile, ignoring metros — used by the Passport
/// feature, which collects real countries (not metro names). "Open Water" outside all polygons.
pub fn country_and_continent(x: i32, y: i32) -> (String, String) {
    let r = regions();
    let (lat, lon) = game_to_lat_lon(x, y);
    for c in &r.countries {
        if lon < c.min_lon || lon > c.max_lon || lat < c.min_lat || lat > c.max_lat { continue; }
        for poly in &c.polygons {
            if point_in_polygon(lon, lat, poly) {
                return (c.name.clone(), c.continent.clone());
            }
        }
    }
    ("Open Water".to_string(), String::new())
}

/// Metro list for the client (header switcher + region tabs): name + centre tile to fly to.
/// `r2` (squared radius in tiles) lets the spectator camera test "is this queen inside a metro".
pub fn metros_json() -> Vec<Value> {
    regions().metros.iter()
        .map(|m| serde_json::json!({ "name": m.name, "x": m.cx, "y": m.cy, "r2": m.r2 }))
        .collect()
}

/// **Geo-concealment variant for GUESTS** — metro hotspot geometry (`x`/`y`/`r2`) with the real
/// city **name stripped**. The landing spectator can still frame "a queen inside a hotspot", but a
/// guest can no longer pair a known city (public lat/lon) with its exact game coords to solve the
/// Mercator projection and reverse-project every queen. Authed players keep the named [`metros_json`]
/// (they already receive the projection for the basemap, so withholding it from them buys nothing).
pub fn metros_anon_json() -> Vec<Value> {
    regions().metros.iter()
        .map(|m| serde_json::json!({ "x": m.cx, "y": m.cy, "r2": m.r2 }))
        .collect()
}

/// (name, centre tile x, centre tile y, squared radius in tiles) per metro — for the throttled
/// king-of-the-hill holder scan in `simulation.rs`.
pub fn metros_for_holder() -> Vec<(String, i32, i32, i64)> {
    regions().metros.iter().map(|m| (m.name.clone(), m.cx, m.cy, m.r2)).collect()
}

/// ISO-3166-1 alpha-2 code (lowercase) and continent string for a country by **name** (as stored in
/// `Player.passport_countries`). `None` if the name is not in the built index (e.g. "Open Water").
/// Used by `build_player_info` to enrich the `passportCountries` array with flag codes.
pub fn iso_and_continent_for_country(name: &str) -> Option<(String, String)> {
    regions().country_iso.get(name).cloned()
}

/// Convert a ground radius (km) at world tile (x,y) into a **squared tile radius**, using the same
/// Mercator-aware metres-per-tile as the metro circles (`build`). Used by admin monument placement so
/// a monument's capture circle covers the same real-world area a metro of that radius would.
pub fn radius_km_to_r2(x: i32, y: i32, radius_km: f64) -> i64 {
    let (lat, _lon) = game_to_lat_lon(x, y);
    let tm = cfg().tile_meters;
    let cos = (lat * PI / 180.0).cos().abs().max(0.05);
    let r_tiles = radius_km * 1000.0 / (tm * cos);
    (r_tiles * r_tiles) as i64
}

/// Public wrapper over the internal Mercator projection: a real-world (lat, lon) → game tile (x, y).
/// Used to seed landmarks/monuments at ground-truth coordinates (see `main`); mirrors the client's
/// own projection so a placed marker lands exactly where the basemap draws that place.
pub fn latlon_to_game(lat: f64, lon: f64) -> (i32, i32) { lat_lon_to_game(lat, lon) }

#[cfg(test)]
mod tests {
    use super::*;

    /// Locks the landmark projection used to seed monuments at real coordinates: the Eiffel Tower
    /// (48.8584 N, 2.2945 E) must land EAST of the prime meridian and NORTH of the equator (the
    /// Paris quadrant), inside the world. Prints the exact game tile so accuracy can be eyeballed.
    #[test]
    fn eiffel_tower_projects_into_paris_quadrant() {
        init();
        let (x, y) = latlon_to_game(48.8584, 2.2945);
        println!("Eiffel Tower (48.8584, 2.2945) -> game tile ({x}, {y})");
        assert!(x > 750_000, "lon > 0 → east of grid centre");
        assert!(y < 375_000, "lat > 0 → north of grid centre (smaller y)");
        assert!(x >= 0 && (x as u32) < 1_500_000 && y >= 0 && (y as u32) < 750_000, "in world bounds");
    }

    /// Geo-concealment invariant: the GUEST metro payload must carry hotspot geometry but **no real
    /// city name** (a named centre + a queen's public coords = a Mercator reverse-projection anchor),
    /// while the authed payload keeps names. Locks the fix so a future edit can't silently re-leak.
    #[test]
    fn guest_metros_are_anonymized_but_keep_geometry() {
        init();
        let named = metros_json();
        let anon  = metros_anon_json();
        assert_eq!(named.len(), anon.len(), "same metros, just name-stripped");
        assert!(!anon.is_empty(), "there are metros to test");
        for (n, a) in named.iter().zip(anon.iter()) {
            // Named payload exposes a real city; the guest payload must not carry ANY name field.
            assert!(n.get("name").and_then(|v| v.as_str()).is_some_and(|s| !s.is_empty()));
            assert!(a.get("name").is_none(), "guest metro must not carry a city name: {a}");
            // Geometry (x/y/r2) is preserved so the spectator camera can still frame hotspots.
            assert_eq!(a["x"], n["x"]); assert_eq!(a["y"], n["y"]); assert_eq!(a["r2"], n["r2"]);
        }
    }
}
