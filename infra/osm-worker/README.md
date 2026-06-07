# antarchy OSM tile cache (Cloudflare Worker + R2)

Self-hosts the OpenStreetMap basemap so the game **doesn't hammer `tile.openstreetmap.org`** (which
gets the egress IP banned at scale) and so tiles are served **fast and at $0 egress** from Cloudflare/R2.

Each tile is fetched from OSM **at most once**, then served from R2 forever. Reads are free
(R2-via-Cloudflare egress is $0; Class-B reads are free to 10M/mo). The only metered op is **one R2
PutObject (Class-A) per unique tile** — bounded by the game's pan clamp + the `ZMAX` zoom cap, so it
stays inside R2's free tier (1M Class-A/mo). **Result: the OSM map stays free.**

## How it fits the game

- The Rust server sends the client a basemap URL via `HIVE_BASEMAP_URL` (read in `config.rs::basemap_url`,
  shipped in the `logged-in` payload, consumed in `public/client.html` `osmTileUrl`).
- The client clamps panning to a region around the player's queen (`clampView` in `client.html`), which
  bounds how many distinct tiles can ever be requested.
- The client persists tiles in IndexedDB, so a returning player loads the region instantly and rarely
  re-reads R2.

## One-time setup

```bash
npm i -g wrangler            # or: npx wrangler ...
wrangler login

# 1) Create the (separate) basemap bucket
wrangler r2 bucket create antarchy-basemap

# 2) Deploy the Worker
cd sim/infra/osm-worker
wrangler deploy

# 3) wrangler will publish on the account's *.workers.dev subdomain (antarchy.fun is not a Cloudflare
#    zone, so no custom domain). LIVE URL:  https://antarchy-osm-cache.toddlooong.workers.dev
#    (For a custom tiles.antarchy.fun later: move antarchy.fun onto Cloudflare, set workers_dev=false,
#     uncomment the [[routes]] custom_domain block in wrangler.toml, redeploy.)
```

> wrangler deploys to your Cloudflare ACCOUNT — it does not matter which machine you run it from, and
> it does **not** need to be re-run on the VPS. The bucket + Worker are global once deployed.

## Point the game at it

In the service env file (`/etc/antarchy.env`):

```
HIVE_BASEMAP_URL=https://antarchy-osm-cache.toddlooong.workers.dev/{z}/{x}/{y}.png
```

The env var is read at startup (future URL changes = restart only). Empty/unset → the client falls
back to raw OSM (dev only — do **not** ship that publicly). NOTE: the game server never touches the
`antarchy-basemap` bucket; only the Worker does, so the VPS needs **no new R2 creds**.

## Verify

```bash
curl -I https://antarchy-osm-cache.toddlooong.workers.dev/10/301/384.png   # 200, Cache-Control: ... immutable
curl -I https://antarchy-osm-cache.toddlooong.workers.dev/10/301/384.png   # again: cached (no OSM/R2 touch)
curl -I https://antarchy-osm-cache.toddlooong.workers.dev/22/1/1.png       # 404 (above ZMAX)
```

Watch **Cloudflare dashboard → R2 → antarchy-basemap** metrics: Class-A ≈ unique tiles ever viewed,
Class-B low (edge/browser cache absorb repeats), egress $0.

## Tuning / future levers (deferred — not needed for free-tier)

- `ZMAX` (in `src/index.js`): lower it to shrink the tile universe further.
- **WebP transcode**: storing/serving WebP shrinks bytes/storage. Needs Cloudflare Images (paid) or a
  WASM encoder; skipped because egress is already $0 and storage is pennies. The client decodes WebP
  transparently if enabled — just change the URL/content-type.
- **Ocean/blank dedup** (content-addressed identical tiles): marginal here because the pan clamp keeps
  players over land near their queen; it also complicates the key→object read path, so it's omitted.
