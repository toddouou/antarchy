//! Cosmetics registry — the **server-authoritative** catalogue of gem-purchasable cosmetics. This is
//! the source of truth for what exists, what it costs, and which equip **slot** it occupies; the
//! client only ever sends a cosmetic `id` / `slot` (see `handlers::cosmetic-buy` / `cosmetic-equip`),
//! never a price. Ownership + equipped state live per-account in `users.json`
//! (`UserRecord.owned_cosmetics` / `UserRecord.equipped`), wipe-proof like `gems`.
//!
//! Adding a cosmetic = one entry here (+ the mirrored client card in `client.html`). Slots keep
//! categories mutually exclusive: one `tile_fx` at a time, one `queen_skin`, etc. The first entry,
//! **glow**, occupies `tile_fx` and is rendered client-side as a pulsing white overlay on the owner's
//! territory (visible to everyone via the per-owner tile-FX palette).

/// A purchasable cosmetic. `slot` groups mutually-exclusive cosmetics (equipping one replaces the
/// other in that slot). `price_gems` is debited from `UserRecord.gems` on purchase. The presentation
/// fields (`name`/`desc`/`icon`/`experimental`) are the catalogue data the client mirrors; the
/// purchase path only reads `id`/`slot`/`price_gems`, so they're carried here for the registry + any
/// future server-sent catalogue, not yet read on the server.
#[allow(dead_code)]
pub struct Cosmetic {
    pub id:           &'static str,
    pub name:         &'static str,
    pub desc:         &'static str,
    pub icon:         &'static str,
    pub slot:         &'static str,
    pub price_gems:   u64,
    pub experimental: bool,
}

/// The catalogue. Keep the client's mirrored `COSMETIC_ITEMS` list (in `client.html`) in sync.
pub const COSMETICS: &[Cosmetic] = &[
    Cosmetic {
        id: "glow",
        name: "GLOW",
        desc: "Your territory pulses between your colour and white.",
        icon: "💫",
        slot: "tile_fx",
        price_gems: 500,
        experimental: true,
    },
];

/// Look up a cosmetic by id (the only thing the client is trusted to name).
pub fn get(id: &str) -> Option<&'static Cosmetic> {
    COSMETICS.iter().find(|c| c.id == id)
}
