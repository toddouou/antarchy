use std::collections::{HashMap, HashSet};
use std::fs;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use argon2::{Algorithm, Argon2, Params, PasswordHash, PasswordHasher, PasswordVerifier, Version};
use argon2::password_hash::SaltString;
use rand::RngCore;
use subtle::ConstantTimeEq;

use crate::alliance::Alliance;
use crate::config::{cfg, argon2_lanes, argon2_mem_kib, argon2_time, ADMIN_USERNAME, ADMIN_PASSWORD,
                    TEST_USERNAME, TEST_PASSWORD, HUES};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserRecord {
    pub id:            u32,
    pub username:      String,
    pub password_hash: String,
    pub color:         String,
    pub hue_idx:       i32,
    pub is_admin:      bool,
    pub color_chosen:  bool,
    /// Highest queen level this USER has ever reached. Survives queen death / prestige / redeploy
    /// (Queen.level resets to 1). Drives progressive unlock visibility + one-time unlock popups.
    /// JSON-persisted (users.json); additive — pre-existing records load as 0.
    #[serde(default)]
    pub peak_level:    u16,
    // ---- beta-v2 account identity (all additive: old users.json loads with these defaulted) ----
    /// Lowercase email — the primary login identity for beta-v2 accounts. "" for legacy/admin.
    #[serde(default)]
    pub email:          String,
    /// E.164 phone — secondary login identity. "" if none / not collected.
    #[serde(default)]
    pub phone:          String,
    /// Public display name shown on queens + leaderboard. Falls back to `username` when "".
    #[serde(default)]
    pub handle:         String,
    #[serde(default)]
    pub email_verified: bool,
    #[serde(default)]
    pub phone_verified: bool,
    /// UTC day number (`config::utc_day`) of the last claimed daily-ant window; 0 = never claimed.
    /// One portion per 00:00-UTC window, granted ONLY by the `claim-daily` handler (idempotent via
    /// this field). Lives here (users.json, additive serde default) — NOT in the bincode world
    /// snapshot — so it survives restarts without invalidating snapshots.
    #[serde(default)]
    pub last_claim_day: u64,
    /// Gems — the cosmetics-line currency (Group C UI; earn/spend wired in a later group). Lives
    /// here (users.json, additive serde default) — NOT in the bincode world snapshot — so a cosmetics
    /// balance survives the season wipe, matching `peak_level` / `last_claim_day`.
    #[serde(default)]
    pub gems: u64,
    /// Cosmetic ids this account has purchased (spends `gems`). Lives here (users.json, additive
    /// serde default) so cosmetics survive the season wipe, matching `gems`. Source of truth is
    /// `cosmetics::COSMETICS`; this just records ownership.
    #[serde(default)]
    pub owned_cosmetics: Vec<String>,
    /// Equipped cosmetics keyed by **slot** (e.g. `tile_fx` → `glow`) — one cosmetic per slot, so
    /// equipping replaces. Additive serde default; wipe-proof home like `owned_cosmetics`.
    #[serde(default)]
    pub equipped: HashMap<String, String>,
    /// UTC day number (`config::utc_day`) of the last passive metro-nectar accrual; 0 = never.
    /// Idempotency guard for the once-per-00:00-UTC accrual (server.rs sim loop). Same wipe-proof
    /// home as `last_claim_day` — NOT the bincode snapshot.
    #[serde(default)]
    pub last_accrual_day: u64,
    /// Alliance this account belongs to (id into `Auth.alliances`), or `None`. Wipe-proof home like
    /// `gems`/`peak_level` — survives a world (season) wipe; NOT the bincode snapshot. The runtime
    /// `World.player_alliance` index is rebuilt from the roster, but this is the durable source.
    #[serde(default)]
    pub alliance_id:   Option<u32>,
}

impl UserRecord {
    /// Public display name: the chosen `handle`, or the legacy `username` if no handle was set.
    pub fn display_name(&self) -> &str {
        if self.handle.is_empty() { &self.username } else { &self.handle }
    }
}

#[derive(Debug, Default)]
pub struct Auth {
    pub users:  HashMap<String, UserRecord>,  // keyed by UPPERCASE username
    pub banned: HashSet<String>,
    /// Alliances, keyed by alliance id. Persisted in users.json (wipe-proof). The `World` mirrors a
    /// player→alliance index from this for the hot path; this map is the durable source of truth.
    pub alliances: HashMap<u32, Alliance>,
    /// Monotonic alliance-id allocator. 0 → the first `create_alliance` mints id 1.
    pub next_alliance_id: u32,
    /// Stripe `checkout.session.id`s already credited — the **payment idempotency** ledger. A webhook
    /// replay/retry for the same session is a no-op (can't double-credit gems). Persisted in users.json
    /// (wipe-proof) so a restart still rejects replays. Grows one short id per purchase (bounded; prune
    /// later if ever needed).
    pub processed_payments: HashSet<String>,
}

/// On-disk form of `Auth` (users.json). Carries both accounts and the ban list so a restart
/// restores moderation state too. `banned` defaults to empty for forward/backward compatibility.
#[derive(Debug, Serialize, Deserialize)]
struct AuthSave {
    users:  HashMap<String, UserRecord>,
    #[serde(default)]
    banned: HashSet<String>,
    #[serde(default)]
    alliances: HashMap<u32, Alliance>,
    #[serde(default)]
    next_alliance_id: u32,
    #[serde(default)]
    processed_payments: HashSet<String>,
}

/// LEGACY password hash — SHA-256 with a fixed string salt. **Do not use for new hashes.** Kept only
/// to *verify* (and then transparently upgrade) accounts created before the Argon2id migration. New
/// and re-hashed passwords go through [`hash_pw_argon2`].
pub fn hash_pw(pw: &str) -> String {
    let mut h = Sha256::new();
    h.update(format!("{}hive-salt", pw));
    format!("{:x}", h.finalize())
}

/// Hash a password with **Argon2id** (OWASP A07), returning a self-describing PHC string
/// (`$argon2id$v=19$m=…,t=…,p=…$salt$hash`) that carries its own salt + parameters. Cost comes from
/// the `HIVE_ARGON2_*` env knobs (default OWASP-minimum m=19 MiB, t=2, p=1). Parameters are clamped to
/// a valid range so the hasher can never fail on a misconfigured value.
pub fn hash_pw_argon2(pw: &str) -> String {
    let mut salt_bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut salt_bytes);
    let salt = SaltString::encode_b64(&salt_bytes).expect("16-byte salt always encodes to b64");

    let lanes = argon2_lanes().max(1);
    let mem   = argon2_mem_kib().max(8 * lanes); // Argon2 requires m_cost ≥ 8 × p_cost
    let params = Params::new(mem, argon2_time().max(1), lanes, Some(32))
        .unwrap_or_default(); // unreachable after clamping, but never panic
    let hasher = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    hasher
        .hash_password(pw.as_bytes(), &salt)
        .expect("argon2id hashing of valid input + clamped params is infallible")
        .to_string()
}

/// Verify `pw` against a stored hash, transparently handling both formats:
/// Argon2id PHC strings (constant-time, params read from the hash) and the legacy SHA-256 hex
/// (constant-time compare via `subtle`). Returns `false` on any malformed stored value.
pub fn verify_pw(stored: &str, pw: &str) -> bool {
    if stored.starts_with("$argon2") {
        match PasswordHash::new(stored) {
            Ok(parsed) => Argon2::default().verify_password(pw.as_bytes(), &parsed).is_ok(),
            Err(_) => false,
        }
    } else {
        // Legacy SHA-256 hex — compare in constant time (both sides are fixed-length hex).
        hash_pw(pw).as_bytes().ct_eq(stored.as_bytes()).into()
    }
}

/// True when a stored hash should be re-hashed on the next successful login: legacy SHA-256, a
/// non-Argon2id variant, or Argon2id with parameters below the current target cost.
pub fn needs_rehash(stored: &str) -> bool {
    if !stored.starts_with("$argon2id$") { return true; }
    match PasswordHash::new(stored).ok().and_then(|h| Params::try_from(&h).ok()) {
        Some(p) => p.m_cost() < argon2_mem_kib()
                || p.t_cost() < argon2_time()
                || p.p_cost() < argon2_lanes(),
        None => true,
    }
}

impl Auth {
    pub fn new() -> Self {
        let mut auth = Auth::default();
        auth.reset_to_admin_only();
        auth
    }

    /// The built-in admin account (`ADMIN` / `admin`, id 1). Always present so the operator can
    /// never be locked out — recreated on fresh start, wipe, or a snapshot that omits it.
    fn admin_record() -> UserRecord {
        UserRecord {
            id:            1,
            username:      ADMIN_USERNAME.to_string(),
            password_hash: hash_pw_argon2(ADMIN_PASSWORD),
            color:         "#000000".to_string(),
            hue_idx:       -1,
            is_admin:      true,
            color_chosen:  false,
            peak_level:    0,
            ..Default::default()   // email/phone/handle "", verified flags false
        }
    }

    /// Built-in NON-admin test account (`ADMIN_TEST` / `test`, id 2). Mirrors the admin's "always
    /// present" guarantee but with `is_admin: false` + a pre-chosen colour, so the operator can log
    /// straight into the normal-player experience (real fog-of-war, no god-view) without registering.
    fn test_record() -> UserRecord {
        UserRecord {
            id:            2,
            username:      TEST_USERNAME.to_string(),
            password_hash: hash_pw_argon2(TEST_PASSWORD),
            color:         HUES.get(3).copied().unwrap_or("#6f8f5a").to_string(),
            hue_idx:       3,
            is_admin:      false,
            color_chosen:  true,   // skip the colour-pick step → drop into placement immediately
            peak_level:    0,
            ..Default::default()
        }
    }

    /// Clear all accounts + bans down to the two built-ins (admin + non-admin test). Used at fresh
    /// start and on admin WIPE — both built-ins are permanent fixtures of the beta.
    pub fn reset_to_admin_only(&mut self) {
        self.users.clear();
        self.banned.clear();
        self.alliances.clear();
        self.next_alliance_id = 0;
        self.users.insert(ADMIN_USERNAME.to_string(), Self::admin_record());
        self.users.insert(TEST_USERNAME.to_string(), Self::test_record());
    }

    pub fn save(&self) {
        let save_path = {
            let c = cfg();
            c.save_file.replace("world.snapshot", "users.json")
        };
        let data = AuthSave {
            users:  self.users.clone(),
            banned: self.banned.clone(),
            alliances: self.alliances.clone(),
            next_alliance_id: self.next_alliance_id,
            processed_payments: self.processed_payments.clone(),
        };
        let json = match serde_json::to_string_pretty(&data) {
            Ok(j)  => j,
            Err(e) => { eprintln!("[auth] serialize users.json failed: {e}"); return; }
        };
        // This is the account database — write atomically (temp + fsync + rename, like the world
        // snapshot) so a crash mid-write can never leave a truncated users.json behind, and say so
        // out loud when the disk lets us down instead of silently dropping accounts.
        let tmp = format!("{save_path}.tmp");
        let res = fs::File::create(&tmp)
            .and_then(|mut f| { use std::io::Write; f.write_all(json.as_bytes())?; f.sync_all() })
            .and_then(|_| fs::rename(&tmp, &save_path));
        if let Err(e) = res { eprintln!("[auth] write users.json failed: {e}"); }
    }

    /// Load accounts + bans from `users.json` (path derived from the world snapshot path). Falls
    /// back to admin-only if the file is missing or unreadable; guarantees the admin account exists.
    pub fn load(path: &str) -> Self {
        let mut auth = Auth::default();
        let users_path = path.replace("world.snapshot", "users.json");
        if let Ok(data) = fs::read_to_string(&users_path) {
            if let Ok(saved) = serde_json::from_str::<AuthSave>(&data) {
                auth.users  = saved.users;
                auth.banned = saved.banned;
                auth.alliances = saved.alliances;
                auth.next_alliance_id = saved.next_alliance_id;
                auth.processed_payments = saved.processed_payments;
                auth.users.entry(ADMIN_USERNAME.to_string())
                    .or_insert_with(Self::admin_record);
                auth.users.entry(TEST_USERNAME.to_string())
                    .or_insert_with(Self::test_record);
                return auth;
            }
        }
        auth.reset_to_admin_only();
        auth
    }

    pub fn is_admin_id(&self, id: u32) -> bool {
        self.users.values().any(|u| u.id == id && u.is_admin)
    }

    /// Look up an account by (case-insensitive) email — beta-v2's primary login identity. Linear
    /// scan over `users`, consistent with `is_admin_id`'s scan; fine at this scale (hundreds–low
    /// thousands of accounts). Empty email never matches (legacy/admin records have `email == ""`).
    pub fn find_by_email(&self, email: &str) -> Option<&UserRecord> {
        let e = email.trim().to_lowercase();
        if e.is_empty() { return None; }
        self.users.values().find(|u| u.email == e)
    }

    /// Look up an account by phone (exact match on the stored E.164 string). Empty never matches.
    pub fn find_by_phone(&self, phone: &str) -> Option<&UserRecord> {
        let p = phone.trim();
        if p.is_empty() { return None; }
        self.users.values().find(|u| u.phone == p)
    }

    /// Look up an account by numeric id (used by the session-token WS login path). Linear scan.
    pub fn find_by_id(&self, id: u32) -> Option<&UserRecord> {
        self.users.values().find(|u| u.id == id)
    }

    /// True if some existing account already claims this (case-insensitive) email. Used to enforce
    /// email uniqueness at registration/verification time.
    pub fn email_taken(&self, email: &str) -> bool {
        self.find_by_email(email).is_some()
    }

    #[allow(dead_code)]
    pub fn next_id(&self) -> u32 {
        self.users.values().map(|u| u.id).max().unwrap_or(1) + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argon2_hash_verifies_and_is_salted() {
        let h1 = hash_pw_argon2("correct horse battery staple");
        let h2 = hash_pw_argon2("correct horse battery staple");
        assert!(h1.starts_with("$argon2id$"), "expected a PHC string, got {h1}");
        assert_ne!(h1, h2, "a random per-password salt must yield different encoded hashes");
        assert!(verify_pw(&h1, "correct horse battery staple"));
        assert!(!verify_pw(&h1, "wrong password"));
        assert!(!needs_rehash(&h1), "a fresh hash is already at the target cost");
    }

    #[test]
    fn legacy_sha256_verifies_then_wants_rehash() {
        let legacy = hash_pw("hunter2");
        assert!(verify_pw(&legacy, "hunter2"), "legacy hash still authenticates");
        assert!(!verify_pw(&legacy, "Hunter2"), "wrong password is rejected");
        assert!(needs_rehash(&legacy), "legacy SHA-256 must upgrade on next login");
    }

    #[test]
    fn malformed_hash_never_verifies() {
        assert!(!verify_pw("", "x"));
        assert!(!verify_pw("$argon2id$not-a-real-phc-string", "x"));
    }
}
