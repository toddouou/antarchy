# HIVE — Today's Hosting & Servers Guide (your advisor for the day)

> **Date:** 2026-06-03 · **Audience:** you (the owner), assuming **zero prior infra/ops knowledge**.
> **Goal today:** stand up the **two new servers** and move the live game onto them.
> **Companion file:** `TODAY_CHECKLIST.md` — the same plan as tick-boxes in order. Read this guide
> first for the *why*, then work the checklist for the *do*.

---

## 0. TL;DR — what today actually is

You are doing **two things** that together kill the ~$10,000/month bandwidth bill:

1. **New game host** — move the live game off **Railway** (which charges **$0.10 per GB** of traffic)
   onto a **cheap-bandwidth host** (Hetzner Cloud, which *includes* ~20 TB/month and charges about
   **$0.001/GB** after — roughly **100× cheaper per gigabyte**). Same bytes, a fraction of the bill.
2. **Cloudflare R2** — a storage service that serves the *bulk* of the map (snapshot images + the base
   map) to players' browsers **directly, with $0 egress**. The game server stops paying to ship those
   bytes at all.

The code that uses both of these is **already written and tested** (see §2). Today is **not coding** —
it's **accounts, servers, DNS, and flipping switches**. Plus one genuinely delicate step: **moving the
saved game world across without losing anyone's progress.**

You told me you want to **fully cut live traffic over to the new host today**. That's the aggressive
option. I'll help you do it — but read the **Advisor's honest take (§1)** and the **Warnings (§9)**
first, because the order you do things in is what keeps it safe.

---

## 1. Advisor's honest take (read this once)

**Doing a full host move + R2 + a new code version all in one day is ambitious but doable** *if* you
treat the live cutover as the **last** step and keep Railway alive as an instant fallback the whole time.

The single rule that makes this safe:

> **Build and prove the new host privately first (on a test address). Only flip DNS to it at the very
> end, once you've played on it in a browser and it works. Do not turn Railway off today — it's your
> undo button.**

If anything goes wrong after cutover, "rolling back" is just **pointing DNS back at Railway** — seconds,
not hours. That safety net only exists if you don't tear Railway down. So: **migrate fully today, yes —
but keep the old one warm until tomorrow.**

The one part I can't make risk-free for you is **copying the saved world** (`world.snapshot` +
`users.json`) off Railway and onto the new host. That file *is* everyone's territory and accounts. We do
it carefully, twice (a practice copy early, a final copy at cutover), and I flag it loudly in §9.

---

## 2. Where the project stands (development summary)

You've been running a multi-phase rebuild (full plan: `docs/egress/EGRESS_PLAN.md`) to cut the bill
without making the game worse. Status of the *code*:

| Piece | What it does | State |
|---|---|---|
| **Phases 0–3** | Measurement + isolating server CPU + **compressing the live stream** (already ~4.2× smaller per player) | **Committed** on branch `egress-killer` (commit `16c2e0c`) |
| **Phases 4–7, ∥A, ∥B, P6** | Per-connection caps, faster saves, the **R2 snapshot pipeline**, reconnect polish, base-map hook | **Built & tested, NOT yet committed** (in your working files right now; `cargo test` = 30 passing) |

What this means in plain terms: **the new code is finished and verified on your machine, but it is not
live anywhere.** The expensive old version is still what's running in production and bleeding money. So
today's deploy is also the moment all this work finally starts paying off.

**Crucial design choice the developers made for you:** the R2 feature is **dormant until you set
environment variables.** There's no risky rebuild tomorrow — you flip it on with settings. Same for the
safety caps. That's why today is "switches, not code."

The two new pieces of infrastructure map to the plan like this:
- **New game host** = the plan's *"$/GB lever"* (host move) — cuts the price of every byte.
- **Cloudflare R2** = the plan's *Phase 6 + Parallel-B* — removes most of the bytes from the bill entirely.

They're **complementary**: the host move makes bytes cheap; R2 removes bytes. Doing both is how
~$10k/mo becomes ~$50–200/mo.

---

## 3. Crash course — the concepts you need today

Skim this once; refer back when a checklist step uses a term.

- **Egress / bandwidth:** data your server *sends out* to players. This is what Railway bills at
  $0.10/GB and what's costing you ~$10k/mo. **Lower bytes + cheaper-per-byte host = the whole project.**
- **VPS (Virtual Private Server):** a Linux computer you rent in a data center (Hetzner). Unlike Railway
  (which auto-builds and runs your app for you), a VPS is **raw** — *you* install things and start the
  program. More work, far cheaper bandwidth. We'll set it up step by step.
- **DNS / domain:** the phone book of the internet. `play.yourgame.com` → an IP address like
  `5.75.x.x`. **"Cutover" = changing that pointer** from Railway's address to your new VPS's address.
  Lowering the **TTL** (time-to-live) beforehand makes that change — and any rollback — take effect fast.
- **TLS / HTTPS / WSS:** the padlock. Browsers on `https://` pages are **not allowed** to open an
  insecure `ws://` game connection — it **must** be `wss://` (secure WebSocket). Railway gave you HTTPS
  for free; on a raw VPS **you must provide it**. We use **Caddy**, a tiny web server that gets a free
  certificate automatically and forwards traffic to the game. **Forget this and the game breaks for
  everyone** — see §9.
- **Reverse proxy:** the front-door program (Caddy) that listens on the public 443 (HTTPS) port,
  handles the padlock, and quietly passes requests to the game running on `localhost:8080`.
- **systemd:** Linux's "keep this program running" manager. It restarts the game if it crashes or the
  server reboots, and — importantly — it shuts the game down *gracefully* (the game **auto-saves the
  world on shutdown**, so a clean stop never loses progress).
- **Cloudflare R2:** an internet storage bucket. The game **uploads** small map images to it; players'
  **browsers download** them straight from R2. Downloads cost **$0 egress** (that's the magic). R2 *does*
  charge a tiny amount for **uploads/writes** — negligible at your scale, but we keep an eye on it (§9).
- **Environment variables (env vars):** named settings you hand the program when it starts (e.g.
  `SNAPSHOT_CDN=on`). This is how every feature below is switched on without touching code.

---

## 4. The two servers — what each one is and does

```
                 players' browsers
                  /            \
        live game │             │ map images + base map
        (wss://)  │             │ (plain https, cached)
                  ▼             ▼
   ┌────────────────────┐   ┌──────────────────────────┐
   │  NEW GAME HOST      │   │  CLOUDFLARE R2 (bucket)  │
   │  Hetzner VPS        │   │  - snapshot map tiles    │
   │  - runs `hive-sim`  │──▶│  - (later) base map      │
   │  - Caddy = HTTPS    │   │  $0 egress to browsers   │
   │  - saves world here │   └──────────────────────────┘
   │  cheap bandwidth    │      ▲ game uploads tiles here
   └────────────────────┘──────┘
```

**Server #1 — the game host (Hetzner VPS).** Runs the actual game (the `hive-sim` binary). Serves the
page, the live WebSocket (ants, territory, killfeed, leaderboard). This replaces Railway. Its job is to
handle the *live, changing edge* of the game cheaply.

**Server #2 — Cloudflare R2 (the storage "server").** Not a computer you manage — a storage bucket. The
game periodically renders the painted map into small PNG images and uploads them here; browsers pull
them directly. This offloads the *bulk* (the big, slowly-changing accumulated map) off your billed host
entirely. It's also where the base map will live later (so you stop hammering free OpenStreetMap, which
will ban a busy server without warning — that's a known launch blocker, handled in a later step).

---

## 5. Recommended choices (so you don't stall on decisions)

You'll be asked to pick a few things. My recommendations for *today*, optimizing for "works, cheap,
reversible":

| Decision | Recommendation | Why |
|---|---|---|
| Host | **Hetzner Cloud**, **CPX31** (4 vCPU / 8 GB RAM), Ubuntu 24.04 | Generous included traffic; 8 GB RAM builds the Rust release without choking (the build is heavy — see §9). You can downsize later. |
| Region | Closest to most players (e.g. **Falkenstein/Nuremberg** EU, or **Ashburn/Hillsboro** US) | Lower latency; all regions have the same cheap traffic. |
| TLS | **Caddy** reverse proxy (automatic free HTTPS) | One-line config, auto-renewing cert, handles `wss://` transparently. |
| R2 public access (today) | **r2.dev public URL** to start; custom domain later | Fastest path to working tiles today; a custom domain is a nice-to-have, not a blocker. |
| Base map (∥B) | **Leave on OpenStreetMap fallback today** (don't set `HIVE_BASEMAP_URL`) | One less thing today; it already falls back to OSM. Schedule the licensed source as a near-term follow-up before any big public push. |
| Code version | Build branch **`egress-killer`** (after committing today's work) | This is the version with all the savings. |

**Inputs only you can provide today** (have these ready — they're in the checklist's Section 0):
- A **domain name** you control (or a subdomain), and access to its DNS settings. *You need this for
  HTTPS/`wss://`.* If the game currently lives on a `*.railway.app` address with no custom domain, getting
  a cheap domain is a hard prerequisite — flag me if you don't have one.
- **How the code reaches the VPS:** a private **GitHub remote** to `git clone`, *or* you copy the folder
  up with `scp`. (Checklist Section 1 covers both.)
- **Access to the current Railway world data** (`world.snapshot` + `users.json`) so we can carry
  progress over. This is the delicate one (§9).

---

## 6. The plan for today, in order (and why this order)

1. **Commit the finished code** so there's a clean, named version to deploy and roll back to.
2. **Provision the Hetzner VPS** and install the toolchain.
3. **Build the game on the VPS** and run it on a **test port** — *no public traffic yet.* Prove the
   binary works in isolation.
4. **Set up Cloudflare R2** (bucket + access keys + public URL + CORS).
5. **Wire R2 into the game** via env vars and confirm map tiles actually upload and load.
6. **Add HTTPS** (Caddy + your domain) and run the game as a managed service (systemd).
7. **Practice-copy the world data** from Railway to the VPS and smoke-test with real data.
8. **Final data sync + DNS cutover** — the live switch. Keep Railway running.
9. **Verify in a browser** (the "is the game still good?" checklist) and **watch the meters**.

**Why this order:** every step before #8 is invisible to your players and fully reversible. You only
touch live traffic once everything else is proven. Data is copied twice — a practice run (#7) to shake
out problems, and a final run (#8) so you don't lose the last hour of play.

---

## 7. Detailed walk-through of the trickier steps

The checklist has the exact commands. Here's the *understanding* behind the steps people get wrong.

### 7.1 Building on the VPS (not on Windows)
The game is compiled for **Linux**. Build it **on the VPS itself** so it matches the OS exactly. The
release build uses heavy optimization (`lto`, `codegen-units=1`) — it's **slow (several minutes) and
RAM-hungry**. That's why I recommend 8 GB RAM; on a smaller box, add swap first (checklist covers it) or
the build can be killed silently. The finished program is one file: `target/release/hive-sim`.

### 7.2 Environment variables — the control panel
Everything is switched on with env vars. Here's the **complete set** the game reads, and what to do with
each today:

| Variable | Set it to | Purpose / note |
|---|---|---|
| `PORT` | `8080` | Port the game listens on (behind Caddy). |
| `HIVE_DATA_DIR` | e.g. `/var/lib/hive` | **CRITICAL.** Folder for the saved world. Must be a **persistent** path you control — never `/tmp`. This is where the migrated data goes. |
| `SNAPSHOT_CDN` | `on` | Turns on the R2 uploader (needs the 4 `R2_*` vars too). |
| `R2_ENDPOINT` | `https://<ACCOUNT_ID>.r2.cloudflarestorage.com` | R2's S3 address (from the Cloudflare dashboard). |
| `R2_BUCKET` | your bucket name | e.g. `hive-snapshots`. |
| `R2_ACCESS_KEY_ID` | from R2 API token | The "username" for uploads. |
| `R2_SECRET_ACCESS_KEY` | from R2 API token | The "password" — **secret**, keep out of git. |
| `R2_PUBLIC_BASE` | your public tile URL | What **browsers** use to fetch tiles, e.g. `https://pub-xxxx.r2.dev`. **No trailing slash.** |
| `HIVE_BIN_CTL` | *(leave unset)* | Defaults **on** = the compressed protocol. Emergency rollback only: set `0` to force the old uncompressed protocol. |
| `EGRESS_CAP_KBPS` | *(leave unset today)* | Per-player bandwidth cap. Leave off at launch; turn on only if a few connections misbehave. |
| `HIVE_WAL` | *(leave unset)* | Experimental faster-save journal. Keep **off**. |
| `HIVE_BASEMAP_URL` | *(leave unset today)* | Licensed base map (later). Unset = falls back to OpenStreetMap. |
| `HIVE_VIEWPORT_THREADS` | *(leave unset)* | Auto-sizes to ~half the CPU cores. Fine as default. |
| `HIVE_SNAP_DIR` | *(test only)* | If set, writes tiles to **local disk** instead of R2 — handy to prove the renderer works **before** you have R2 keys. Unset it once R2 is wired. |

**Tip:** during step #3 (test run, no R2 yet) you can set `HIVE_SNAP_DIR=/tmp/snaptest` to watch PNG
tiles appear on disk — proof the map renderer works without needing Cloudflare yet. Remove it when you
switch to real R2.

### 7.3 Cloudflare R2 setup (what the clicks mean)
1. **Create the bucket** (e.g. `hive-snapshots`). This is the container for the tile images.
2. **Create an R2 API token** with **Object Read & Write** for that bucket. Cloudflare gives you an
   **Access Key ID** + **Secret Access Key** (the `R2_*` creds) and shows your **account endpoint**
   (`https://<ACCOUNT_ID>.r2.cloudflarestorage.com`). The *game* uses these to **upload**.
3. **Enable public read access** so *browsers* can download tiles. The quick path is the bucket's
   **r2.dev public URL** — that becomes `R2_PUBLIC_BASE`. (A custom domain is cleaner for production but
   not needed today.)
4. **Set CORS** on the bucket so the game page is allowed to load the images onto its canvas. Allow your
   game's origin for `GET`. Example policy:
   ```json
   [
     { "AllowedOrigins": ["https://play.yourgame.com"],
       "AllowedMethods": ["GET"],
       "AllowedHeaders": ["*"],
       "MaxAgeSeconds": 86400 }
   ]
   ```
   *Without this, the map images silently fail to draw in the browser.*

The game uploads tiles under keys like `snap/{epoch}/0/{cx}/{cy}.png`; the browser fetches
`R2_PUBLIC_BASE + /snap/...`. So `R2_PUBLIC_BASE` must point at the **bucket root**.

### 7.4 HTTPS with Caddy (the `wss://` fix)
Caddy is two lines of config:
```
play.yourgame.com {
    reverse_proxy localhost:8080
}
```
Point `play.yourgame.com`'s DNS at the VPS, start Caddy, and it **automatically** fetches a Let's
Encrypt certificate and serves HTTPS — and it forwards WebSocket upgrades transparently, so `wss://`
just works. **This is the step that makes the game reachable securely from a browser.**

### 7.5 Migrating the saved world (the careful one)
The live world is two files in Railway's data volume: **`world.snapshot`** (all territory/ants/queens/
players) and **`users.json`** (accounts + bans). To carry progress over:
- **Practice copy (step #7):** get a *recent* copy from Railway onto the VPS's `HIVE_DATA_DIR`, start the
  game on the test port, and confirm the world loads (your queen, territory, accounts all present).
- **Final copy (step #8):** right before cutover, take a **fresh** copy (the practice one is now stale by
  however long you've been working) so you don't lose recent play.

How to get the files off Railway depends on your setup (Railway CLI `railway ssh`/`railway run` into the
volume, or a temporary download). **This is the highest-care step** — see §9. The new code reads the old
`world.snapshot` format and upgrades it automatically, so an older copy still loads fine.

---

## 8. How you'll know it worked (verification)

**Server health (in a terminal or browser):**
- `https://play.yourgame.com/health` → returns tick count, ant/queen/player counts, uptime, tick timing.
  Non-zero counts = your migrated world loaded.
- `https://play.yourgame.com/egress-stats` → per-message-type byte counters. This is your **bill meter**.
- The game log prints `[snapshot] R2 sink active (bucket …)` when R2 is correctly wired.

**The "is the game still good?" browser check (do after cutover, with a real account):**
- Page loads over **https** with a padlock; you can log in and see your world.
- Place ~10 ants → they **crawl** cell-by-cell (not slide/teleport); a turning ant makes an L-shape.
- A kill **toasts in the killfeed**; the **leaderboard** and **region tabs** update live.
- The **WORKERS panel** lifespan bars tick down; army/credits/refill/HP/XP all update.
- Pan to a painted area → territory fills in (this is where R2 tiles + live updates combine).
- Refresh / briefly disconnect → it reconnects cleanly without a long white flash.

**The win condition:** over the next hours/days, `/egress-stats` and your **Hetzner traffic graph**
should show dramatically lower billed bytes than Railway did, and R2's dashboard should show tiles being
read by browsers (the offloaded bulk). The real proof is the **next bill**.

---

## 9. Warnings — the things that actually bite people

Ranked by how badly they hurt:

1. **Losing the world during data migration.** `world.snapshot` is everyone's progress. **Never** start
   the new server pointed at an empty `HIVE_DATA_DIR` and then let people play — that creates a *fresh*
   world that will overwrite your copy on the next autosave. Always migrate the file **before** first
   real use, verify counts on `/health`, and do a **final fresh copy at cutover**.
2. **Two servers writing the same world = divergence.** Once you cut over, **Railway and the VPS are two
   different running worlds.** Don't let players keep hitting Railway *and* the VPS — DNS sends everyone
   to one place, which is the point. Keep Railway **running but unvisited** as a rollback; just know its
   world is now frozen/stale.
3. **Forgetting HTTPS/`wss://`.** A browser on an `https://` page **cannot** open an insecure game
   socket. If you skip Caddy/TLS, the page may load but **the game never connects** for anyone. Always
   reach the game via `https://your-domain`, never the raw `http://IP:8080` in production.
4. **`HIVE_DATA_DIR` not persistent.** If it points at a temp/ephemeral path, a reboot wipes the world.
   Use a real directory like `/var/lib/hive` that you control, and make sure systemd's user can write it.
5. **Don't touch the admin "WIPE" button.** It's the *only* thing that erases the world (and all
   non-admin accounts). Irrelevant to hosting — just don't, especially while testing.
6. **R2 charges for uploads (writes), not downloads.** Downloads are the $0 magic; **writes** (tile
   uploads) have a free tier (~1M/month) then a small cost. At your scale this is tiny, but **watch the
   dirty-chunk/write volume** the first day. If it ever looks high, the snapshot interval can be widened.
   (Egress to players is still $0 either way.)
7. **R2 CORS missing → blank map.** If you set up R2 but tiles don't draw, it's almost always CORS not
   allowing your game origin. (§7.3.)
8. **The Rust build can OOM on a small VPS.** The release build is heavy. Use **≥8 GB RAM** or add swap
   first; otherwise the build dies silently partway.
9. **DNS takes time to propagate.** **Lower the TTL** (e.g. to 60–300s) on the game's DNS record **early
   today** (hours before cutover) so the switch — and any rollback — is fast instead of waiting on a long
   cached TTL.
10. **Keep secrets out of git.** The `R2_SECRET_ACCESS_KEY` and any tokens go in the systemd
    environment / an env file with locked-down permissions — **never** committed.
11. **OpenStreetMap base map is a launch risk at scale.** It's fine today, but a busy server hammering
    free OSM tiles will get rate-limited/banned without notice (blanks the map for everyone). Plan the
    licensed base-map source (the `HIVE_BASEMAP_URL` hook is already in the code) **before** any big
    public spike. Not a blocker for today's migration.

---

## 10. Rollback plan (your safety net)

If the new host misbehaves after cutover:
- **Fastest:** point the game's DNS record **back at Railway**. With a low TTL this is near-instant, and
  Railway's world is intact (just frozen since cutover). Players reconnect to the old version.
- **Disable R2 only** (keep the new host): set `SNAPSHOT_CDN=off` and restart — the game falls back to
  serving the map live over the WebSocket (more bandwidth, but fully functional).
- **Disable the compressed protocol only:** `HIVE_BIN_CTL=0` and restart — reverts to the old wire
  format if a client-compatibility problem ever appears.

Because every new feature is env-gated and Railway stays warm, **nothing today is a one-way door.**

---

## 11. What is NOT for today (so you don't scope-creep)

- **Licensed base map** (`HIVE_BASEMAP_URL`) — near-term follow-up; OSM fallback covers today.
- **R2 custom domain / far-zoom map pyramid / shareable-PNG & spectate features** — these are the upside
  the R2 pipeline unlocks *later*; the bucket just needs to exist and work today.
- **Turning Railway off** — leave it running as rollback; decommission it tomorrow once the new host has
  proven itself for a day and the bill graph confirms the win.
- **Tuning the per-connection caps / WAL** — leave dormant; only reach for them if a problem appears.

---

### Final word from your advisor
Work the **checklist** top to bottom — it's this guide turned into ordered tick-boxes. Don't skip the
test-run and practice-copy steps to save time; they're exactly what makes a same-day full migration
safe. Flip DNS last, keep Railway warm, and you've got a clean undo at every point. You've got this.
