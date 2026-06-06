# Codebase Audit — antarchy.fun / HIVE-SIM

**Date:** 2026-06-06 · **Branch:** `cleanup/audit-2026-06-06` (sim, off `security-p0`) +
matching branch in the parent repo (off `main`). Discovery/assessment were read-only; this report
records every change made in the cleanup that follows it.

The workspace is two nested git repos:
- **Parent** `langton's ant/` — design/marketing assets + product docs (its own repo).
- **`sim/`** — the live Rust game engine (its own repo). This is what ships.

---

## Summary by category

| Category | Count | Disposition |
|---|---|---|
| Active / production | all 17 `src/*.rs` + manifests + `data/` + `public/` + `favicon.png` + `SECURITY.md` + `CHANGELOG-security.md` + READMEs + parent `prototype.html`/`landing.html` | Keep |
| Active support (content corrected) | `docs/` checkpoint+architecture docs, both `CLAUDE.md`, memory store | Keep + rewrite |
| Stale | `PROJECT.MD`, `TODAY_CHECKLIST.md`, `TODAY_SETUP_GUIDE.md`, `node_modules/` gitignore line, `HIVE - Development Doc.docx`, `education.html` | Delete |
| Redundant | `tools/ne_110m_admin_0_countries.geojson`, deps `tower-http` + `smallvec` | Delete / remove |
| Unknown | none remaining (all resolved with the owner) | — |

---

## Deletions (with recorded reason)

| Path | Reason |
|---|---|
| `sim/TODAY_CHECKLIST.md` | Railway→VPS+R2 migration tick-list; migration completed (game live on VPS). |
| `sim/TODAY_SETUP_GUIDE.md` | "Why" companion to the above; same completed one-time migration. |
| `sim/tools/ne_110m_admin_0_countries.geojson` | Byte-identical to `data/countries.geojson` (same git blob `1e6ab74c…`); only `data/` is embedded via `regions.rs` `include_str!`; `tools/` is unreferenced. |
| `tower-http` (`Cargo.toml`) | Zero references in `src/` (only appeared in the manifest). Build-verified after removal. |
| `smallvec` (`Cargo.toml`) | Zero references in `src/` (only appeared in CLAUDE.md prose). Build-verified after removal. |
| `PROJECT.MD` (parent) | 0-byte placeholder, never filled. |
| `node_modules/` line in parent `.gitignore` | Relic of the removed Node.js prototype; no Node project remains. |
| `HIVE - Development Doc.docx` (parent, untracked) | Unused design doc, never referenced by code. ⚠️ untracked — not git-recoverable. |
| `education.html` (parent, untracked) | Never committed; references scrubbed from `README.md` + parent `CLAUDE.md`. ⚠️ untracked. |

## Documentation kept but corrected (content drift, not deletion)

- `docs/ARCHITECTURE.md` — claimed the world "is intentionally not persisted"; contradicts
  `persist.rs` + autosave. Also predated the auth/api rebuild. → corrected.
- `ROADMAP.md` (parent) H0 — listed fixed items as blockers ("World resets on every restart",
  "WebSocket auth (SHA-256)"). → marked shipped (persistence, Argon2id, R2+VPS).
- `sim/CLAUDE.md` — file-layout/dependency/WS-message lists omitted the newer modules
  (`api/session/email/sms/metrics/snapshot`), deps, `enter`+`/api/*`, R2 pipeline, security. → updated.
- `docs/egress/EGRESS_PLAN.md` + `docs/egress/R2_CLASSA_PLAN.md` — said "hosted on Railway";
  game moved to a VPS. → factual reference corrected, checkpoint structure preserved.
- Parent `CLAUDE.md` + `README.md` — referenced the now-deleted `education.html`. → references removed.

## In-repo checkpoints kept as-is (live)

`docs/security/SECURITY_P0_PLAN.md` (P0 complete, paused for P1/P2), `docs/egress/EGRESS_PLAN.md`,
`docs/egress/R2_CLASSA_PLAN.md` — kept as resumable checkpoints per the owner's standing preference;
only factual drift corrected.

---

## Code-quality findings (noted; not changed beyond the dep removal)

- **Dead functions:** `snapshot::snapshot_dirty` (`src/snapshot.rs:47`) and `snapshot::rasterize_chunk`
  (`src/snapshot.rs:142`) are `pub fn`s never called (this is a binary, so `pub` doesn't export them) —
  the only two compiler warnings. Candidates for removal in a future pass; left untouched here.
- `tokio-tungstenite` is used only by `server.rs`'s `egress_bench` — candidate to move to
  `[dev-dependencies]` (optional; left as a regular dep for now).
- Three build dirs (`target/`, `target-dev/`, `target-check/`) — all gitignored, disk-only.

## Verification — results

- `cargo build` + `cargo build --release` (into `target-dev`): **succeed** (only the two pre-existing
  dead-code warnings above; no error from the dep removal).
- `cargo test`: **44 passed, 0 failed, 4 ignored** (the `#[ignore]` benchmarks).
- `git grep`: no references to any deleted path in code/docs; all `include_str!`/`include_bytes!`
  targets still present.

## Verification

- `cargo build` + `cargo build --release` succeed after dep removal (`Cargo.lock` refreshed).
- `cargo test` passes.
- `git grep` finds no references to any deleted path.
