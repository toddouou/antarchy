//! Per-account **persistent event log** — the in-game "EVENTS" feed.
//!
//! Events are keyed by **username** (account-level), so they survive queen death, logout and
//! reconnect, and persist to `events.json` beside the world snapshot (under `HIVE_DATA_DIR`) — the
//! same trick `monuments.json` / `config.json` use. Each entry is a bold **head** (one-line summary)
//! plus a dashed **sub** (the detail: who, what, whether you were hit). Bounded per account by age
//! (~20 h) and count (300) so neither RAM nor the file grows without limit. The window pages the
//! history newest-first; live entries also ride `World::log_event` to the connected player.

use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

const MAX_AGE_MS:   u64   = 20 * 60 * 60 * 1000; // keep ~20 h of history per account
const MAX_PER_USER: usize = 300;                 // hard per-account cap (a busy account can't grow unbounded)
pub const PAGE_SIZE: usize = 30;                 // entries per client page (newest-first)

#[derive(Clone, Serialize, Deserialize)]
pub struct EventEntry {
    pub ts:   u64,
    pub head: String,
    pub sub:  String,
}

/// Username → ring of entries (oldest at the front, newest at the back).
#[derive(Default)]
pub struct EventStore {
    logs: FxHashMap<String, VecDeque<EventEntry>>,
}

impl EventStore {
    /// Append an entry for `username`, then trim by count and by age (relative to the newest entry).
    pub fn push(&mut self, username: &str, ts: u64, head: &str, sub: &str) {
        let dq = self.logs.entry(username.to_string()).or_default();
        dq.push_back(EventEntry { ts, head: head.to_string(), sub: sub.to_string() });
        while dq.len() > MAX_PER_USER { dq.pop_front(); }
        let cutoff = ts.saturating_sub(MAX_AGE_MS);
        while dq.front().is_some_and(|e| e.ts < cutoff) { dq.pop_front(); }
    }

    /// Newest-first page. `before = None` → most-recent page; `before = Some(ts)` → entries strictly
    /// older than `ts` (the "load older" cursor). Returns `(page, more)` where `more` means older
    /// entries remain beyond this page.
    pub fn page(&self, username: &str, before: Option<u64>, n: usize) -> (Vec<EventEntry>, bool) {
        let Some(dq) = self.logs.get(username) else { return (Vec::new(), false); };
        let mut it = dq.iter().rev().filter(|e| before.is_none_or(|b| e.ts < b));
        let page: Vec<EventEntry> = it.by_ref().take(n).cloned().collect();
        let more = it.next().is_some();
        (page, more)
    }

    pub fn clear_user(&mut self, username: &str) { self.logs.remove(username); }
    pub fn clear_all(&mut self) { self.logs.clear(); }

    /// `world.snapshot` → `events.json`, so the store rides `HIVE_DATA_DIR` like the other sidecars.
    fn path() -> String { crate::config::cfg().save_file.replace("world.snapshot", "events.json") }

    /// Load from `events.json`. Missing/corrupt → empty (fresh start), never panics.
    pub fn load() -> EventStore {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(data) => match serde_json::from_str::<FxHashMap<String, VecDeque<EventEntry>>>(&data) {
                Ok(logs) => { println!("[events] loaded {} account logs from {path}", logs.len()); EventStore { logs } }
                Err(e)   => { eprintln!("[events] {path} is not valid — ignoring ({e})"); EventStore::default() }
            },
            Err(_) => { println!("[events] no events.json at {path} — starting empty"); EventStore::default() }
        }
    }

    /// Persist atomically (temp file + rename), like `monuments.json`. Best-effort: a write failure is
    /// logged, never fatal (the in-memory store stays authoritative for the session).
    pub fn save(&self) {
        let path = Self::path();
        let body = match serde_json::to_string(&self.logs) {
            Ok(s)  => s,
            Err(e) => { eprintln!("[events] serialize failed: {e}"); return; }
        };
        let tmp = format!("{path}.tmp");
        if let Err(e) = std::fs::write(&tmp, body) { eprintln!("[events] write {tmp} failed: {e}"); return; }
        if let Err(e) = std::fs::rename(&tmp, &path) { eprintln!("[events] rename → {path} failed: {e}"); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_is_newest_first_and_pages_older() {
        let mut s = EventStore::default();
        for i in 0..5u64 { s.push("alice", 1000 + i, &format!("h{i}"), &format!("s{i}")); }
        // Most-recent page of 3 → newest first (ts 1004,1003,1002), with more remaining.
        let (p, more) = s.page("alice", None, 3);
        assert_eq!(p.iter().map(|e| e.ts).collect::<Vec<_>>(), vec![1004, 1003, 1002]);
        assert!(more);
        // Older page before the last shown ts → 1001, 1000, no more.
        let (p2, more2) = s.page("alice", Some(1002), 3);
        assert_eq!(p2.iter().map(|e| e.ts).collect::<Vec<_>>(), vec![1001, 1000]);
        assert!(!more2);
    }

    #[test]
    fn trims_by_count() {
        let mut s = EventStore::default();
        for i in 0..(MAX_PER_USER as u64 + 50) { s.push("bob", 1 + i, "h", "s"); }
        let (all, _) = s.page("bob", None, MAX_PER_USER + 100);
        assert_eq!(all.len(), MAX_PER_USER);
        // Pushed ts 1..=(MAX_PER_USER+50); newest kept is the very last push.
        assert_eq!(all.first().unwrap().ts, MAX_PER_USER as u64 + 50); // newest kept
        assert_eq!(all.last().unwrap().ts, 51);                        // oldest survivor after trim
    }

    #[test]
    fn trims_by_age() {
        let mut s = EventStore::default();
        s.push("carol", 1, "old", "s");                 // far in the past
        s.push("carol", MAX_AGE_MS + 100, "new", "s");  // pushes the cutoff past the old entry
        let (p, _) = s.page("carol", None, 10);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].head, "new");
    }
}
