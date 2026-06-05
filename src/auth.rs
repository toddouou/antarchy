use std::collections::{HashMap, HashSet};
use std::fs;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{cfg, ADMIN_USERNAME, ADMIN_PASSWORD};

#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

#[derive(Debug, Default)]
pub struct Auth {
    pub users:  HashMap<String, UserRecord>,  // keyed by UPPERCASE username
    pub banned: HashSet<String>,
}

/// On-disk form of `Auth` (users.json). Carries both accounts and the ban list so a restart
/// restores moderation state too. `banned` defaults to empty for forward/backward compatibility.
#[derive(Debug, Serialize, Deserialize)]
struct AuthSave {
    users:  HashMap<String, UserRecord>,
    #[serde(default)]
    banned: HashSet<String>,
}

pub fn hash_pw(pw: &str) -> String {
    let mut h = Sha256::new();
    h.update(format!("{}hive-salt", pw));
    format!("{:x}", h.finalize())
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
            password_hash: hash_pw(ADMIN_PASSWORD),
            color:         "#000000".to_string(),
            hue_idx:       -1,
            is_admin:      true,
            color_chosen:  false,
            peak_level:    0,
        }
    }

    /// Clear all accounts + bans down to just the admin. Used at fresh start and on admin WIPE.
    pub fn reset_to_admin_only(&mut self) {
        self.users.clear();
        self.banned.clear();
        self.users.insert(ADMIN_USERNAME.to_string(), Self::admin_record());
    }

    pub fn save(&self) {
        let save_path = {
            let c = cfg();
            c.save_file.replace("world.snapshot", "users.json")
        };
        let data = AuthSave { users: self.users.clone(), banned: self.banned.clone() };
        if let Ok(json) = serde_json::to_string_pretty(&data) {
            let _ = fs::write(&save_path, json);
        }
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
                auth.users.entry(ADMIN_USERNAME.to_string())
                    .or_insert_with(Self::admin_record);
                return auth;
            }
        }
        auth.reset_to_admin_only();
        auth
    }

    pub fn is_admin_id(&self, id: u32) -> bool {
        self.users.values().any(|u| u.id == id && u.is_admin)
    }

    #[allow(dead_code)]
    pub fn next_id(&self) -> u32 {
        self.users.values().map(|u| u.id).max().unwrap_or(1) + 1
    }
}
