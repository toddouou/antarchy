# HIVE — Today's Migration Checklist (work top to bottom)

> Companion to `TODAY_SETUP_GUIDE.md` (read that first for the *why*). Do sections **in order** —
> nothing touches live players until **Section 8**. Keep Railway running the whole time as rollback.
>
> **Legend:** 🪟 = run on your Windows machine · 🐧 = run on the VPS (over SSH) · ☁️ = Cloudflare/Hetzner
> dashboard (web) · 🌐 = your domain's DNS panel · 👀 = check/verify, no command.
>
> Replace placeholders: `play.yourgame.com` (your domain), `<VPS_IP>`, `<ACCOUNT_ID>`, bucket name, keys.

---

## Section 0 — Pre-flight: accounts & decisions (do first, ~15 min)
- [ ] 👀 Confirm you have a **domain** (or subdomain) you control with DNS access. *No domain = blocker
      for HTTPS/`wss://`; get a cheap one before proceeding.*
- [ ] ☁️ **Hetzner Cloud** account created (or logged in). Payment method added.
- [ ] ☁️ **Cloudflare** account created (or logged in). (Free plan is fine for R2.)
- [ ] 👀 Decide host size: **CPX31 (4 vCPU / 8 GB), Ubuntu 24.04** (recommended) and a **region** near
      your players.
- [ ] 👀 Confirm you can reach the **current Railway world data** (`world.snapshot` + `users.json`) —
      via Railway CLI (`railway ssh`/`railway run`) or a download. *You'll need this in Sections 7–8.*
- [ ] 🌐 **Lower the TTL** on the DNS record you'll use for the game (e.g. to **300 seconds**) **now**,
      so the later cutover/rollback is fast. (Leave the record pointing at Railway for now.)

---

## Section 1 — Commit & prepare the code (🪟, ~10 min)
- [ ] 🪟 Confirm you're on the right branch and review what's about to be committed:
      ```powershell
      git -C "C:\Users\Todd\OneDrive\Desktop\langton's ant\sim" status
      git -C "C:\Users\Todd\OneDrive\Desktop\langton's ant\sim" branch --show-current   # expect: egress-killer
      ```
- [ ] 🪟 Commit the finished Phase 4–7 / R2 work (the new `src/snapshot.rs` + modified files):
      ```powershell
      cd "C:\Users\Todd\OneDrive\Desktop\langton's ant\sim"
      git add -A
      git commit -m "HIVE egress rebuild: Phases 4-7 + persistence + R2 snapshot pipeline"
      ```
- [ ] 👀 Decide how the code reaches the VPS — **pick ONE**:
  - [ ] **Git remote:** push the branch (`git push origin egress-killer`) so you can `git clone` on the VPS.
  - [ ] **Direct copy:** you'll `scp` the folder up in Section 3 (no remote needed).

---

## Section 2 — Provision the new game host (☁️ + 🐧, ~15 min)
- [ ] ☁️ Hetzner → **Create Server**: Ubuntu 24.04, **CPX31**, chosen region, **add your SSH key**.
- [ ] ☁️ (Hetzner) Create/attach a **Firewall**: allow inbound **22 (SSH)**, **80 (HTTP)**, **443 (HTTPS)**.
- [ ] 👀 Note the server's **public IP** → this is `<VPS_IP>`.
- [ ] 🐧 SSH in: `ssh root@<VPS_IP>`
- [ ] 🐧 Update + install build tools:
      ```bash
      apt update && apt -y upgrade
      apt -y install build-essential pkg-config git curl ufw
      ```
- [ ] 🐧 (If you did NOT pick CPX31/8 GB) add swap so the build won't OOM:
      ```bash
      fallocate -l 4G /swapfile && chmod 600 /swapfile && mkswap /swapfile && swapon /swapfile
      ```
- [ ] 🐧 Install Rust:
      ```bash
      curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
      source "$HOME/.cargo/env"
      ```
- [ ] 🐧 OS firewall (matches the cloud firewall):
      ```bash
      ufw allow 22 && ufw allow 80 && ufw allow 443 && ufw --force enable
      ```

---

## Section 3 — Build & test-run the game privately (🪟/🐧, ~10–15 min) — NO live traffic
- [ ] 🐧 **Get the code onto the VPS** (matching your Section 1 choice):
  - Git: `git clone -b egress-killer <YOUR_REMOTE_URL> hive && cd hive`
  - Or from Windows (🪟), copy it up, then 🐧 `cd ~/hive`:
      ```powershell
      scp -r "C:\Users\Todd\OneDrive\Desktop\langton's ant\sim" root@<VPS_IP>:~/hive
      ```
- [ ] 🐧 Build the release binary (several minutes — this is the heavy one):
      ```bash
      cd ~/hive && cargo build --release
      ```
- [ ] 🐧 Create the persistent data dir: `mkdir -p /var/lib/hive`
- [ ] 🐧 **Test run on a throwaway port**, with a local-disk tile sink to prove the map renderer (no R2
      yet, fresh empty world — this is just a smoke test):
      ```bash
      HIVE_DATA_DIR=/var/lib/hive-test PORT=8090 HIVE_SNAP_DIR=/tmp/snaptest ./target/release/hive-sim
      ```
- [ ] 🐧 In a second SSH session, verify it's alive: `curl -s http://127.0.0.1:8090/health`
- [ ] 👀 As an admin in-game (or via admin tools) paint a little, then 🐧 confirm PNG tiles appeared:
      `ls -R /tmp/snaptest/snap | head` → you should see `.png` files. *Renderer works.*
- [ ] 🐧 Stop the test run (`Ctrl-C`). Clean up: `rm -rf /var/lib/hive-test /tmp/snaptest`

---

## Section 4 — Set up Cloudflare R2 (☁️, ~15 min)
- [ ] ☁️ Cloudflare → **R2** → **Create bucket** (e.g. `hive-snapshots`).
- [ ] ☁️ R2 → **Manage API Tokens** → create a token with **Object Read & Write** on that bucket.
      Save the **Access Key ID** + **Secret Access Key** (shown once).
- [ ] 👀 Note your **endpoint**: `https://<ACCOUNT_ID>.r2.cloudflarestorage.com` (shown in R2 settings).
- [ ] ☁️ Bucket → **Settings** → enable **Public access** (r2.dev) → copy the **public URL** (e.g.
      `https://pub-xxxx.r2.dev`). This is `R2_PUBLIC_BASE` (**no trailing slash**).
- [ ] ☁️ Bucket → **Settings** → **CORS Policy** → add (replace the origin with your real game domain):
      ```json
      [ { "AllowedOrigins": ["https://play.yourgame.com"],
          "AllowedMethods": ["GET"], "AllowedHeaders": ["*"], "MaxAgeSeconds": 86400 } ]
      ```

---

## Section 5 — Wire R2 in + verify uploads (🐧, ~10 min) — still no live traffic
- [ ] 🐧 Quick R2 connectivity test run (real R2 this time, still test port + test data dir):
      ```bash
      HIVE_DATA_DIR=/var/lib/hive-test2 PORT=8090 \
      SNAPSHOT_CDN=on \
      R2_ENDPOINT="https://<ACCOUNT_ID>.r2.cloudflarestorage.com" \
      R2_BUCKET="hive-snapshots" \
      R2_ACCESS_KEY_ID="<ACCESS_KEY>" \
      R2_SECRET_ACCESS_KEY="<SECRET_KEY>" \
      R2_PUBLIC_BASE="https://pub-xxxx.r2.dev" \
      ./target/release/hive-sim
      ```
- [ ] 👀 In the startup log, look for **`[snapshot] R2 sink active (bucket hive-snapshots)`**. *(If you
      see "creds incomplete" or "init failed", recheck the 4 `R2_*` values.)*
- [ ] 👀 Paint a little as admin, wait ~1–2 min, then ☁️ check the R2 bucket → objects under
      `snap/.../*.png` should appear. *Uploads work.*
- [ ] 🐧 Stop (`Ctrl-C`); clean up: `rm -rf /var/lib/hive-test2`

---

## Section 6 — HTTPS + run as a service (🐧 + 🌐, ~15 min)
- [ ] 🌐 Point an **A record** for `play.yourgame.com` → `<VPS_IP>` (TTL already low from Section 0).
      *(This subdomain isn't the one players use yet — it's for setting up HTTPS. Use your real game
      hostname here only if you're ready; otherwise use a staging subdomain and switch in Section 8.)*
- [ ] 🐧 Install **Caddy** (auto-HTTPS reverse proxy):
      ```bash
      apt -y install debian-keyring debian-archive-keyring apt-transport-https
      curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' | gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
      curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' | tee /etc/apt/sources.list.d/caddy-stable.list
      apt update && apt -y install caddy
      ```
- [ ] 🐧 Write `/etc/caddy/Caddyfile`:
      ```
      play.yourgame.com {
          reverse_proxy localhost:8080
      }
      ```
      then: `systemctl restart caddy`
- [ ] 🐧 Create the env file `/etc/hive.env` (lock it down; **secrets live here, not in git**):
      ```bash
      cat > /etc/hive.env <<'EOF'
      PORT=8080
      HIVE_DATA_DIR=/var/lib/hive
      SNAPSHOT_CDN=on
      R2_ENDPOINT=https://<ACCOUNT_ID>.r2.cloudflarestorage.com
      R2_BUCKET=hive-snapshots
      R2_ACCESS_KEY_ID=<ACCESS_KEY>
      R2_SECRET_ACCESS_KEY=<SECRET_KEY>
      R2_PUBLIC_BASE=https://pub-xxxx.r2.dev
      EOF
      chmod 600 /etc/hive.env
      ```
- [ ] 🐧 Create the systemd service `/etc/systemd/system/hive.service`:
      ```ini
      [Unit]
      Description=HIVE game server
      After=network.target

      [Service]
      EnvironmentFile=/etc/hive.env
      ExecStart=/root/hive/target/release/hive-sim
      WorkingDirectory=/root/hive
      Restart=always
      RestartSec=2
      TimeoutStopSec=30

      [Install]
      WantedBy=multi-user.target
      ```
      *(`TimeoutStopSec=30` gives the game time to auto-save on stop.)*
- [ ] 🐧 **Do NOT start it yet** if `/var/lib/hive` is empty — migrate data first (Section 7) to avoid
      creating a fresh world. (`systemctl daemon-reload` now is fine.)

---

## Section 7 — Practice data migration (🪟/🐧, ~10 min) — shakeout, not the final copy
- [ ] 🪟/🐧 Pull a **recent** copy of `world.snapshot` + `users.json` off Railway (CLI/download) and place
      them in the VPS's `HIVE_DATA_DIR`:
      ```bash
      # files end up at:
      /var/lib/hive/world.snapshot
      /var/lib/hive/users.json
      ```
- [ ] 🐧 Start the service and watch the log:
      ```bash
      systemctl enable --now hive
      journalctl -u hive -f
      ```
- [ ] 👀 `curl -s http://127.0.0.1:8080/health` → **non-zero** queen/player/tile counts = your world
      loaded. (Counts at zero = data didn't load — fix before continuing.)
- [ ] 👀 Visit `https://play.yourgame.com` in a browser → padlock present, log in, **your world/accounts
      are there**, you can play. Place ants → they crawl; kill → killfeed; leaderboard updates.
- [ ] 👀 `curl -s http://127.0.0.1:8080/egress-stats` returns counters (the bill meter is live).
- [ ] 👀 ☁️ R2 bucket shows tiles being written; loading the map in the browser pulls them.

---

## Section 8 — Final sync + CUTOVER (the live switch, ~10 min)
> Everything above was invisible to players. This is the live moment. **Keep Railway running.**
- [ ] 🐧 Stop the VPS game to swap in fresh data: `systemctl stop hive`
- [ ] 🪟/🐧 Take a **FRESH** copy of `world.snapshot` + `users.json` from Railway (the practice copy is now
      stale) and overwrite the two files in `/var/lib/hive`.
- [ ] 🐧 Start it back up: `systemctl start hive` → confirm 👀 `/health` counts look current.
- [ ] 🌐 **Cutover:** change the **real game hostname**'s DNS record to point at `<VPS_IP>`. *(If you set
      up Caddy on a staging subdomain, add the real hostname to the Caddyfile and `systemctl restart
      caddy` first.)*
- [ ] 👀 After TTL (~5 min), the live domain serves from the VPS. Hard-refresh and confirm.
- [ ] 👀 **Do NOT shut Railway down.** Leave it running, unvisited, as instant rollback.

---

## Section 9 — Post-cutover verification (👀, ~10 min)
Run the "is the game still good?" pass with a real account on the live domain:
- [ ] Page loads over **https** (padlock); log in; your world is present.
- [ ] Place ~10 ants → they **crawl** cell-by-cell (not slide); a turning ant makes an **L-shape**.
- [ ] A kill **toasts in the killfeed**; **leaderboard** + **region tabs** update live.
- [ ] **WORKERS panel** lifespan bars tick; army/credits/refill/HP/XP update.
- [ ] Pan across painted territory → it fills in (R2 tiles + live updates).
- [ ] Refresh / brief disconnect → reconnects cleanly, **no long white flash**.
- [ ] `/egress-stats` and the **Hetzner traffic graph** show traffic flowing through the new host.
- [ ] ☁️ R2 dashboard shows tile **reads** climbing (bulk offloaded, $0 egress).

---

## Section 10 — Monitor & keep rollback ready (ongoing today)
- [ ] 👀 Watch `journalctl -u hive -f` for errors for the first hour.
- [ ] 👀 Watch ☁️ **R2 write/Class-A volume** the first day (should be modest; if high, widen the
      snapshot interval later — egress to players stays $0 regardless).
- [ ] 👀 Spot-check `/health` tick timing stays healthy under real players.
- [ ] 👀 Keep **Railway warm until tomorrow**; decommission only after a full day proves the new host +
      the bill graph confirms the drop.

### 🔻 Rollback (if anything goes wrong)
- [ ] **Fastest:** 🌐 point the game's DNS back at **Railway** (low TTL = near-instant; old world intact).
- [ ] **R2 trouble only:** 🐧 set `SNAPSHOT_CDN=off` in `/etc/hive.env`, `systemctl restart hive` (map
      serves live over WS instead).
- [ ] **Protocol/client trouble only:** 🐧 add `HIVE_BIN_CTL=0` to `/etc/hive.env`, restart (reverts to
      the old uncompressed wire format).

---

## ⛔ Not today (don't scope-creep)
- Licensed base map (`HIVE_BASEMAP_URL`) — near-term follow-up; OSM fallback is on today.
- R2 custom domain / far-zoom pyramid / shareable-PNG / spectate links — later upside.
- Turning Railway off — tomorrow, after a day of proof.
- Tuning `EGRESS_CAP_KBPS` / `HIVE_WAL` — leave dormant unless a problem appears.
