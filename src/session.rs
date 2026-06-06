//! Session tokens — the beta-v2 auth handoff. The landing page authenticates via the `/api/*` REST
//! endpoints and receives an opaque token; the game client (`/play`) then presents that token over
//! WebSocket (`{t:"session",token}`) to enter the game. Tokens are 256-bit random strings stored
//! server-side (so they're revocable) and persisted to `sessions.json` beside `users.json`, so a
//! redeploy (SIGTERM) doesn't bounce every logged-in player back to the login screen.
//!
//! The store lives behind its OWN `parking_lot::RwLock` — **never** the World lock — so token
//! validation on every `/play` connect can't contend the sim thread's per-tick `blocking_write`.

use std::collections::HashMap;
use std::fs;
use std::sync::Arc;

use parking_lot::RwLock;
use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::config::{cfg, current_ms, session_ttl_hours};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub user_id:    u32,
    pub handle:     String,
    pub issued_ms:  u64,
    pub expires_ms: u64,
}

#[derive(Clone)]
pub struct SessionStore {
    inner: Arc<RwLock<HashMap<String, Session>>>,
}

impl Default for SessionStore {
    fn default() -> Self { Self::new() }
}

impl SessionStore {
    pub fn new() -> Self { Self { inner: Arc::new(RwLock::new(HashMap::new())) } }

    /// `sessions.json` path — derived from the world-snapshot path (lands beside `users.json`).
    fn path() -> String { cfg().save_file.replace("world.snapshot", "sessions.json") }

    /// Load persisted sessions, dropping any already expired. Best-effort (missing/corrupt → empty).
    pub fn load() -> Self {
        let store = Self::new();
        if let Ok(data) = fs::read_to_string(Self::path()) {
            if let Ok(map) = serde_json::from_str::<HashMap<String, Session>>(&data) {
                let now = current_ms();
                let mut g = store.inner.write();
                for (k, v) in map { if v.expires_ms > now { g.insert(k, v); } }
            }
        }
        store
    }

    fn save(&self) {
        let g = self.inner.read();
        if let Ok(json) = serde_json::to_string(&*g) { let _ = fs::write(Self::path(), json); }
    }

    /// Mint a fresh token for `user_id`, persist, and return it.
    pub fn mint(&self, user_id: u32, handle: &str) -> String {
        let token = new_token();
        let now = current_ms();
        let sess = Session {
            user_id, handle: handle.to_string(), issued_ms: now,
            expires_ms: now + session_ttl_hours() * 3600 * 1000,
        };
        self.inner.write().insert(token.clone(), sess);
        self.save();
        token
    }

    /// Resolve a token to its (unexpired) `Session`.
    pub fn validate(&self, token: &str) -> Option<Session> {
        let now = current_ms();
        self.inner.read().get(token).filter(|s| s.expires_ms > now).cloned()
    }

    /// Revoke a token (logout). Persists if it removed anything.
    #[allow(dead_code)]
    pub fn revoke(&self, token: &str) {
        if self.inner.write().remove(token).is_some() { self.save(); }
    }

    /// Drop expired tokens; persists if any were removed. Call occasionally.
    #[allow(dead_code)]
    pub fn prune(&self) {
        let now = current_ms();
        let mut g = self.inner.write();
        let before = g.len();
        g.retain(|_, s| s.expires_ms > now);
        let removed = before - g.len();
        drop(g);
        if removed > 0 { self.save(); }
    }
}

/// A 256-bit random token as lowercase hex.
pub fn new_token() -> String {
    let mut b = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut b);
    hex::encode(b)
}
