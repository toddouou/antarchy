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
}

#[derive(Debug, Default)]
pub struct Auth {
    pub users:  HashMap<String, UserRecord>,  // keyed by UPPERCASE username
    pub banned: HashSet<String>,
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

    pub fn reset_to_admin_only(&mut self) {
        self.users.clear();
        self.users.insert(ADMIN_USERNAME.to_string(), UserRecord {
            id:            1,
            username:      ADMIN_USERNAME.to_string(),
            password_hash: hash_pw(ADMIN_PASSWORD),
            color:         "#000000".to_string(),
            hue_idx:       -1,
            is_admin:      true,
            color_chosen:  false,
        });
    }

    pub fn save(&self) {
        let save_path = {
            let c = cfg();
            c.save_file.replace("world.snapshot", "users.json")
        };
        if let Ok(json) = serde_json::to_string_pretty(&self.users) {
            let _ = fs::write(&save_path, json);
        }
    }

    pub fn load(path: &str) -> Self {
        let mut auth = Auth::default();
        let users_path = path.replace("world.snapshot", "users.json");
        if let Ok(data) = fs::read_to_string(&users_path) {
            if let Ok(map) = serde_json::from_str::<HashMap<String, UserRecord>>(&data) {
                auth.users = map;
                return auth;
            }
        }
        auth.reset_to_admin_only();
        auth
    }

    pub fn is_admin_id(&self, id: u32) -> bool {
        self.users.values().any(|u| u.id == id && u.is_admin)
    }

    pub fn next_id(&self) -> u32 {
        self.users.values().map(|u| u.id).max().unwrap_or(1) + 1
    }
}
