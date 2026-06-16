//! Alliances — the late-game co-op layer. An alliance has a name + icon + colour, up to
//! [`crate::config::ALLIANCE_MAX_MEMBERS`] members, and a combined-contribution XP total that maps to
//! one of five tiers (`crate::config::alliance_level_for_xp`). Each tier grants passive buffs to every
//! member (`crate::config::alliance_buffs`).
//!
//! **Where it lives:** the `Alliance` objects + the id counter persist inside [`crate::auth::Auth`]
//! (users.json), and per-account membership is `UserRecord.alliance_id` — both wipe-proof, NEVER in
//! the positional bincode `world.snapshot`. The `World` keeps only a runtime `player_alliance` index
//! (id → alliance id) rebuilt from this on boot + on every mutation, for O(1) friend-vs-foe lookups
//! during the tick.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alliance {
    pub id:         u32,
    pub name:       String,
    /// A short glyph/emoji from [`ALLIANCE_ICONS`] (validated server-side).
    pub icon:       String,
    /// `#rrggbb` banner colour (validated via `config::valid_hex_color`).
    pub color:      String,
    /// Current leader (founder, or the successor when a leader leaves). Always a member.
    pub leader_id:  u32,
    /// Member player-ids, leader included; insertion order = join order (oldest first → succession).
    pub members:    Vec<u32>,
    /// Pending join applicants (player ids) awaiting the leader's accept/reject.
    #[serde(default)]
    pub requests:   Vec<u32>,
    /// Pending invitees (player ids) the leader invited, awaiting their accept/decline.
    #[serde(default)]
    pub invites:    Vec<u32>,
    /// Combined-contribution XP → tier via `config::alliance_level_for_xp`.
    #[serde(default)]
    pub xp:         f64,
    #[serde(default)]
    pub created_at: u64,
}

impl Alliance {
    pub fn level(&self) -> u16 { crate::config::alliance_level_for_xp(self.xp) }
    pub fn is_full(&self) -> bool { self.members.len() >= crate::config::ALLIANCE_MAX_MEMBERS }
}

/// Curated alliance icons. The client offers exactly this set; the server validates submissions
/// against it so the field can't carry an arbitrary string into the UI.
pub const ALLIANCE_ICONS: &[&str] = &[
    "🐜","🐝","🦂","🕷","🐛","🦗","🐞","🦋",
    "👑","⚔","🛡","🔥","⚡","🌟","💀","🍯",
];

pub fn valid_icon(s: &str) -> bool { ALLIANCE_ICONS.contains(&s) }

/// Trim + length-bound an alliance name (1..=24 chars, control chars stripped). Returns `None` if it
/// is empty after sanitising. Rendering still HTML-escapes (like usernames); this just bounds it.
pub fn sanitize_name(raw: &str) -> Option<String> {
    let cleaned: String = raw.trim().chars().filter(|c| !c.is_control()).take(24).collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() { None } else { Some(cleaned) }
}
