//! Cosmetics registry — the **server-authoritative** catalogue of gem-purchasable cosmetics. This is
//! the source of truth for what exists, what it costs, which **category/slot** it occupies, and the
//! small render hints (`params`) the client needs. The client only ever sends a cosmetic `id` / a
//! `slot` (see `handlers::cosmetic-buy` / `cosmetic-equip` / `bundle-buy`), never a price. Ownership +
//! equipped state live per-account in `users.json` (`UserRecord.owned_cosmetics` /
//! `UserRecord.equipped`), wipe-proof like `gems`.
//!
//! **Slots** keep categories mutually exclusive — one `tile_fx` at a time, one `aura`, one `trail`,
//! one `recolor`. Equipping replaces within a slot. (The old `queen_emblem` slot was retired.)
//!
//! **How each category reaches other viewers (render-only, no per-tile bytes):**
//! - `recolor`  — a flat premium colour. Equipping just sets the player's `color`; it rides the normal
//!                owner→colour palette already on the wire. No extra broadcast.
//! - `tile_fx`  — a whole-territory overlay (glow, gilded, …). Carried by the per-owner **fx palette**
//!                (`network::get_fx_palette`), bounded by *visible owners* per tile frame.
//! - `aura` / `trail` — entity / freshly-painted-tile decorations. Carried by
//!                the low-rate **cosmetics roster** (`network::build_cosmetics_roster`, ~1 Hz,
//!                send-on-change), keyed by player id. The client renders them from data it already
//!                holds (entity owner, delta-changed cell owner). Nothing is streamed per tile.
//!
//! Adding a cosmetic = one entry here. The client fetches the catalogue from the server
//! (`network::build_catalog`, sent in the `me`/login payload), so there is no hand-mirrored list to
//! keep in sync.

// The catalogue table reads best with one wide constructor + arrow-aligned doc lists; both are
// deliberate, so quiet the two style lints they trip.
#![allow(clippy::too_many_arguments, clippy::doc_overindented_list_items)]

use serde_json::{json, Value};

/// A purchasable cosmetic. `slot` groups mutually-exclusive cosmetics (equipping one replaces the
/// other in that slot — by convention `slot == category`). `price_gems` is debited from
/// `UserRecord.gems` on purchase (ignored when `bundle_only`). `params` is a tiny render hint the
/// client reads by `category`:
///   - `recolor` → a single `#RRGGBB` hex (the premium colour to apply).
///   - `trail`   → `>`-separated `#RRGGBB` stops, freshest→aged (e.g. `#000000>#E63946`); the client
///                 ripens freshly-painted cells across these stops over ~10 ticks, ending at the
///                 owner's true colour.
///   - `aura` / `tile_fx` → a style key the client switches on (e.g. `halo`, `gilded`); extra tint
///                 hints may ride in `params` as a hex.
#[allow(dead_code)] // `experimental` is reserved for a future "beta" badge; not read server-side yet.
pub struct Cosmetic {
    pub id:           &'static str,
    pub name:         &'static str,
    pub desc:         &'static str,
    pub icon:         &'static str,
    pub category:     &'static str,   // recolor | aura | tile_fx | trail
    pub slot:         &'static str,   // equip slot (== category)
    pub rarity:       &'static str,   // common | rare | epic | legendary
    pub price_gems:   u64,
    pub params:       &'static str,
    pub bundle_only:  bool,           // never sold à la carte (granted only via a bundle)
    pub experimental: bool,
}

/// A discounted bundle: buying it grants every `members` id at once (idempotent — already-owned
/// members are skipped). Bundles are the price anchor; some members are `bundle_only` exclusives.
pub struct Bundle {
    pub id:         &'static str,
    pub name:       &'static str,
    pub desc:       &'static str,
    pub icon:       &'static str,
    pub price_gems: u64,
    /// Sticker (sum of à-la-carte member prices) for the strikethrough — display only.
    pub sticker_gems: u64,
    pub members:    &'static [&'static str],
}

// Convenience constructors keep the table readable.
const fn c(id:&'static str, name:&'static str, desc:&'static str, icon:&'static str,
           category:&'static str, rarity:&'static str, price_gems:u64, params:&'static str) -> Cosmetic {
    Cosmetic { id, name, desc, icon, category, slot: category, rarity, price_gems, params,
               bundle_only:false, experimental:false }
}

/// The catalogue. Order here is the shop display order within each category.
pub const COSMETICS: &[Cosmetic] = &[
    // ---- Recolors (flat premium colours; equipping sets the player's colour) ----
    c("rc_crimson",  "CRIMSON",   "A vivid blood-red for your colony.",        "🟥", "recolor", "common", 100, "#E63946"),
    c("rc_amber",    "AMBER",     "Warm golden amber territory.",              "🟧", "recolor", "common", 100, "#FFB703"),
    c("rc_cobalt",   "COBALT",    "Deep electric blue.",                       "🟦", "recolor", "common", 100, "#3A86FF"),
    c("rc_emerald",  "EMERALD",   "Lush saturated green.",                     "🟩", "recolor", "common", 100, "#06D6A0"),
    c("rc_violet",   "VIOLET",    "Rich royal purple.",                        "🟪", "recolor", "common", 100, "#8338EC"),
    c("rc_fuchsia",  "FUCHSIA",   "Hot magenta-pink.",                         "🌸", "recolor", "common", 100, "#FF006E"),
    c("rc_tangerine","TANGERINE", "Bright burnt orange.",                      "🔶", "recolor", "common", 100, "#FB5607"),
    c("rc_aqua",     "AQUA",      "Glowing cyan-teal.",                        "💧", "recolor", "common", 100, "#00F5D4"),
    c("rc_gold",     "GOLD",      "Lustrous metallic gold.",                   "🏅", "recolor", "rare",   250, "#D4AF37"),
    c("rc_onyx",     "ONYX",      "Near-black stealth colour.",                "⬛", "recolor", "rare",   250, "#1B1B1E"),
    c("rc_ice",      "ICE",       "Pale frozen blue-white.",                   "🧊", "recolor", "rare",   250, "#CAF0F8"),
    c("rc_ultra",    "ULTRAVIOLET","Deep saturated indigo.",                   "🔮", "recolor", "rare",   250, "#5A189A"),

    // ---- Trails (ripening gradient behind your ants; freshest→aged→true colour) ----
    c("tr_chameleon","CHAMELEON", "Tiles paint dark and ripen into your colour — a living trail.", "🦎", "trail", "epic", 600, "#0d0d0d>"),
    c("tr_aurora",   "AURORA",    "Green-cyan-violet shimmer that settles into your colour.",      "🌌", "trail", "rare", 250, "#06D6A0>#00F5D4>#8338EC>"),
    c("tr_glacier",  "GLACIER",   "Tiles freeze white then thaw to your colour.",                  "❄️", "trail", "rare", 250, "#FFFFFF>#90E0EF>"),
    c("tr_ember",    "EMBER",     "New tiles burn in from ember-orange.",                          "🔥", "trail", "rare", 250, "#6A040F>#FFB703>"),

    // ---- Auras (bounded particle / light systems around your ants + queen) ----
    c("au_halo",     "HALO",       "A soft ring of light with orbiting motes.",        "😇", "aura", "epic", 500, "#FFD166"),
    c("au_phantom",  "COMET TRAIL","A tail of fading sparks streams off your colony.",  "☄",  "aura", "epic", 500, "#cfe3ff"),
    c("au_sparkle",  "STARDUST",   "Drifting twinkles rise around your entities.",      "✨", "aura", "epic", 600, "#FFFFFF"),
    c("au_cyber",    "SONAR",      "Expanding pulse rings sweep outward from you.",     "📡", "aura", "epic", 700, "#00F5D4"),
    Cosmetic { id:"au_prism", name:"PRISM HALO", desc:"A refractive rainbow ring of motes — bundle exclusive.",
               icon:"🌈", category:"aura", slot:"aura", rarity:"epic", price_gems:700, params:"#ff66cc",
               bundle_only:true, experimental:false },

    // ---- Tile effects (whole-territory viewport overlay) ----
    Cosmetic { id:"glow", name:"BLOOM", desc:"Your territory breathes with a soft additive bloom.",
               icon:"💫", category:"tile_fx", slot:"tile_fx", rarity:"legendary", price_gems:500, params:"",
               bundle_only:false, experimental:false },
    c("tf_holo",     "IRIDESCENT","An oil-slick chromatic shimmer plays over your land.",  "🪩", "tile_fx", "legendary", 900, ""),
    c("tf_galaxy",   "GALAXY",    "A drifting starfield twinkles across your territory.",   "🌠", "tile_fx", "legendary", 900, "#b388ff"),
    c("tf_gilded",   "GILDED",    "Molten gold leaf with a sweeping specular sheen.",       "🏆", "tile_fx", "legendary", 1000, "#D4AF37"),
    Cosmetic { id:"tf_auroraveil", name:"AURORA VEIL", desc:"A drifting aurora sheen — bundle exclusive.",
               icon:"🌈", category:"tile_fx", slot:"tile_fx", rarity:"legendary", price_gems:1000, params:"#7CFFCB",
               bundle_only:true, experimental:false },
];

/// Bundles (price anchors). Members may include `bundle_only` exclusives.
pub const BUNDLES: &[Bundle] = &[
    Bundle {
        id: "bn_glowup", name: "GLOW-UP STARTER", icon: "🌟",
        desc: "Vibrant colours + the Bloom effect + a Halo aura.",
        price_gems: 800, sticker_gems: 1400,
        members: &["rc_crimson","rc_cobalt","rc_emerald","rc_violet","glow","au_halo"],
    },
    Bundle {
        id: "bn_cosmic", name: "COSMIC SET", icon: "🌌",
        desc: "Deep space — Galaxy territory, a Stardust aura, an Aurora trail, and cosmic colours.",
        price_gems: 1200, sticker_gems: 2200,
        members: &["tf_galaxy","au_sparkle","tr_aurora","rc_ultra","rc_cobalt","rc_violet"],
    },
    Bundle {
        id: "bn_prismatic", name: "PRISMATIC BUNDLE", icon: "💎",
        desc: "The flagship — exclusive Aurora Veil + Prism Halo, plus Iridescent, Gilded, Stardust, colours, and an Aurora trail.",
        price_gems: 2000, sticker_gems: 5250,
        members: &["tf_auroraveil","tf_holo","au_prism","au_sparkle","rc_crimson","rc_cobalt",
                   "rc_emerald","rc_violet","rc_amber","rc_fuchsia","rc_tangerine","rc_aqua",
                   "tr_aurora","tf_gilded"],
    },
];

/// Look up a cosmetic by id (the only thing the client is trusted to name).
pub fn get(id: &str) -> Option<&'static Cosmetic> {
    COSMETICS.iter().find(|c| c.id == id)
}

/// Look up a bundle by id.
pub fn get_bundle(id: &str) -> Option<&'static Bundle> {
    BUNDLES.iter().find(|b| b.id == id)
}

/// Server-sent catalogue (the client renders the shop from this, so there's no mirrored list to drift).
pub fn build_catalog() -> Value {
    let items: Vec<Value> = COSMETICS.iter().map(|c| json!({
        "id": c.id, "name": c.name, "desc": c.desc, "icon": c.icon,
        "category": c.category, "slot": c.slot, "rarity": c.rarity,
        "price": c.price_gems, "params": c.params, "bundleOnly": c.bundle_only,
    })).collect();
    let bundles: Vec<Value> = BUNDLES.iter().map(|b| json!({
        "id": b.id, "name": b.name, "desc": b.desc, "icon": b.icon,
        "price": b.price_gems, "sticker": b.sticker_gems, "members": b.members,
    })).collect();
    json!({ "items": items, "bundles": bundles })
}
