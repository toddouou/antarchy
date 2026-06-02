//! Lock-free egress + delivery-timing counters (Phase 0 of the egress rebuild).
//!
//! These live OUTSIDE `World` on purpose: the WebSocket **write task** and the **viewport thread**
//! both record into them WITHOUT holding the World lock, so a shared global (atomics + tiny mutexed
//! rings) is the only place they can meet. Everything here is additive instrumentation — deleting
//! the `/egress-stats` route and the `record*` call sites fully reverts it, changing no behaviour.
//!
//! The numbers feed the egress **budget ledger** (docs/egress/EGRESS_PLAN.md §5): per-kind billed
//! bytes (measured at the actual `ws_tx.send` sites, never at enqueue — the latest-wins watch slot
//! drops frames a slow client never receives), plus the two non-egress walls the rebuild also
//! guards: viewport CPU-ms and retained per-connection memory.

use std::sync::atomic::{AtomicU64, Ordering};
use once_cell::sync::Lazy;
use parking_lot::Mutex;

/// Outbound message classes for per-kind byte accounting. Enum discriminants are **array indices**
/// (0..KIND_COUNT) — distinct from the on-the-wire frame byte, which `kind_for_bin` translates.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MsgKind {
    Ant = 0,
    TileKeyframe = 1,
    TileDelta = 2,
    PackedAnt = 3,
    Me = 4,
    Leaderboard = 5,
    Stats = 6,
    RegionHolders = 7,
    Event = 8,
    Other = 9,
}

pub const KIND_COUNT: usize = 10;
pub const KIND_NAMES: [&str; KIND_COUNT] = [
    "ant", "tileKeyframe", "tileDelta", "packedAnt",
    "me", "leaderboard", "stats", "regionHolders", "event", "other",
];

/// Classify an outbound **binary** viewport/control frame by its kind byte (`frame[0]`). Viewport
/// kinds are 0..=3; the 16..=20 control kinds are the Phase-3 compressed-control namespace (counted
/// here ahead of that work so the accounting is stable across the transition).
#[inline]
pub fn kind_for_bin(frame0: u8) -> MsgKind {
    match frame0 {
        0  => MsgKind::Ant,
        1  => MsgKind::TileKeyframe,
        2  => MsgKind::TileDelta,
        3  => MsgKind::PackedAnt,
        16 => MsgKind::Me,
        17 => MsgKind::Leaderboard,
        18 => MsgKind::Stats,
        19 => MsgKind::RegionHolders,
        20 => MsgKind::Event,
        _  => MsgKind::Other,
    }
}

/// Classify an outbound **text** (JSON) control message by its `"t"` field (use `quick_msg_type`
/// to extract it cheaply). The bill-relevant periodic/broadcast kinds are named explicitly; small
/// discrete events, acks, `logged-in`/`welcome-back`, and errors fall into `Event`.
#[inline]
pub fn kind_for_text_type(t: &str) -> MsgKind {
    match t {
        "me"             => MsgKind::Me,
        "leaderboard"    => MsgKind::Leaderboard,
        "server-stats"   => MsgKind::Stats,
        "region-holders" => MsgKind::RegionHolders,
        ""               => MsgKind::Other,
        _                => MsgKind::Event,
    }
}

struct Counters {
    bytes: [AtomicU64; KIND_COUNT],
    msgs:  [AtomicU64; KIND_COUNT],
    /// Estimated WebSocket frame-header overhead across all sends (a ~5–10% TLS/TCP floor still
    /// applies on top when reconciling against billed egress).
    header_bytes: AtomicU64,
}

static EGRESS: Lazy<Counters> = Lazy::new(|| Counters {
    bytes: Default::default(),   // [AtomicU64; N] for N ≤ 32 derives Default → all-zero
    msgs:  Default::default(),
    header_bytes: AtomicU64::new(0),
});

/// Estimated header bytes for a server→client (unmasked) WebSocket frame of `payload_len`.
#[inline]
fn ws_header_bytes(payload_len: usize) -> u64 {
    if payload_len < 126 { 2 } else if payload_len < 65_536 { 4 } else { 10 }
}

/// Record one **billed** send at its actual `ws_tx.send` site. `payload_len` is the wire payload
/// (text UTF-8 byte length or binary frame length); a header estimate is added on top.
#[inline]
pub fn record(kind: MsgKind, payload_len: usize) {
    let i = kind as usize;
    EGRESS.bytes[i].fetch_add(payload_len as u64, Ordering::Relaxed);
    EGRESS.msgs[i].fetch_add(1, Ordering::Relaxed);
    EGRESS.header_bytes.fetch_add(ws_header_bytes(payload_len), Ordering::Relaxed);
}

/// `(bytes, msgs)` per kind, indexed by `MsgKind as usize` (parallel to `KIND_NAMES`).
pub fn per_kind() -> [(u64, u64); KIND_COUNT] {
    let mut out = [(0u64, 0u64); KIND_COUNT];
    for i in 0..KIND_COUNT {
        out[i] = (
            EGRESS.bytes[i].load(Ordering::Relaxed),
            EGRESS.msgs[i].load(Ordering::Relaxed),
        );
    }
    out
}

pub fn header_bytes() -> u64 { EGRESS.header_bytes.load(Ordering::Relaxed) }

// ---- Delivery-timing rings + per-connection memory gauge ------------------

const RING_CAP: usize = 240;

/// A tiny mutex-guarded ring of recent durations (ms) exposing p50/p99/max. Written by the single
/// viewport thread, read occasionally by the HTTP handlers — contention is negligible.
pub struct MsRing(Mutex<MsRingInner>);
struct MsRingInner { buf: Vec<f32>, pos: usize }

impl MsRing {
    pub const fn new() -> Self { MsRing(Mutex::new(MsRingInner { buf: Vec::new(), pos: 0 })) }

    pub fn record(&self, ms: f32) {
        let mut r = self.0.lock();
        if r.buf.len() < RING_CAP {
            r.buf.push(ms);
        } else {
            let p = r.pos;
            r.buf[p] = ms;
            r.pos = (p + 1) % RING_CAP;
        }
    }

    /// `(p50, p99, max)` in ms, each rounded to 0.01. All-zero when empty.
    pub fn pct(&self) -> (f32, f32, f32) {
        let mut v = self.0.lock().buf.clone();
        if v.is_empty() { return (0.0, 0.0, 0.0); }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let at = |p: f64| v[(((v.len() - 1) as f64) * p).round() as usize];
        let r2 = |x: f32| (x * 100.0).round() / 100.0;
        (r2(at(0.50)), r2(at(0.99)), r2(*v.last().unwrap()))
    }
}

/// Whole-cycle viewport delivery duration (Phase A snapshot under the read lock + Phase B
/// serialize/send), recorded only on cycles that produced a batch.
pub static VIEWPORT_CYCLE_MS: MsRing = MsRing::new();
/// Just the lock-free parallel `finish_view` serialization phase.
pub static FINISH_VIEW_MS: MsRing = MsRing::new();
/// Sum of retained per-connection `PrevGrid` bytes — the per-conn memory ceiling (MAX_DIM²-bounded).
pub static PREVGRID_BYTES: AtomicU64 = AtomicU64::new(0);
