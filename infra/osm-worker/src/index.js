// antarchy.fun — lazy-fill OpenStreetMap raster tile cache, backed by Cloudflare R2.
//
// Request:  GET /{z}/{x}/{y}.png   (also accepts .webp in the path; bytes are PNG today)
// Flow:     Cloudflare edge cache  →  R2  →  (cold miss) fetch OSM once → store in R2 → serve.
//
// WHY: serving raw tile.openstreetmap.org to many concurrent players violates OSM's tile usage
// policy and gets the egress IP banned (the map blanks for everyone). This cache fetches each tile
// from OSM AT MOST ONCE — then serves it from R2 forever — and Cloudflare's edge IPs spread that
// one-time fetch out. Reads are $0 egress (R2 via Cloudflare); the only metered op is one R2
// PutObject (Class A) per unique tile, bounded by the client's pan clamp + the ZMAX cap below.
//
// COST GUARDRAILS:
//   - ZMAX caps the zoom (→ bounds the unique-tile universe → bounds Class-A writes + storage).
//   - Immutable Cache-Control → browser + edge cache → almost no repeat R2 reads (Class-B stays free).
//   - The game client clamps panning to a region around each queen, so the working set is tiny.
//
// Keep OSM FREE & policy-clean: descriptive User-Agent with contact, fetch-once, attribution shown
// in the game client. Do NOT turn this into a transparent pass-through (would re-hit OSM per request).

const ZMAX = 17;                 // hard zoom cap (OSM goes to 19; we don't need that for a game)
const TTL  = 31536000;           // 1 year — tiles are effectively immutable
const OSM_BASE = "https://tile.openstreetmap.org";
// Identify ourselves per OSM policy. Update the contact if it changes.
const USER_AGENT = "antarchy.fun-basemap-cache/1.0 (+https://antarchy.fun; contact: toddoliverlong@gmail.com)";

const PATH_RE = /^\/(\d{1,2})\/(\d{1,7})\/(\d{1,7})\.(png|webp)$/;

export default {
  async fetch(request, env, ctx) {
    if (request.method !== "GET" && request.method !== "HEAD") {
      return new Response("method not allowed", { status: 405, headers: { "Allow": "GET, HEAD" } });
    }

    const url = new URL(request.url);
    const m = url.pathname.match(PATH_RE);
    if (!m) return notFound();

    const z = +m[1], x = +m[2], y = +m[3];
    const n = 1 << z;
    if (z > ZMAX || x < 0 || y < 0 || x >= n || y >= n) return notFound();

    // 1) Cloudflare edge cache (per-PoP). Repeat requests within TTL never touch R2.
    const cache = caches.default;
    const cacheKey = new Request(url.toString(), { method: "GET" });
    const hit = await cache.match(cacheKey);
    if (hit) return request.method === "HEAD" ? bodyless(hit) : hit;

    const key = `${z}/${x}/${y}`;   // R2 object key (PNG bytes)

    // 2) R2.
    let bytes;
    const obj = await env.TILES.get(key);
    if (obj) {
      bytes = await obj.arrayBuffer();
    } else {
      // 3) Cold miss → fetch from OSM ONCE, then persist.
      let up;
      try {
        up = await fetch(`${OSM_BASE}/${z}/${x}/${y}.png`, {
          headers: { "User-Agent": USER_AGENT, "Referer": "https://antarchy.fun/" },
          cf: { cacheTtl: TTL, cacheEverything: true },
        });
      } catch (e) {
        return new Response("upstream fetch failed", { status: 502 });
      }
      if (!up.ok) return new Response("upstream " + up.status, { status: 502 });
      bytes = await up.arrayBuffer();
      // One PutObject (Class A) per unique tile, with immutable cache headers (so a direct-from-R2
      // custom-domain read is also long-cached). Fire via waitUntil so we don't block the response.
      ctx.waitUntil(env.TILES.put(key, bytes, {
        httpMetadata: { contentType: "image/png", cacheControl: `public, max-age=${TTL}, immutable` },
      }));
    }

    const resp = tileResponse(bytes);
    ctx.waitUntil(cache.put(cacheKey, resp.clone()));
    return request.method === "HEAD" ? bodyless(resp) : resp;
  },
};

function tileResponse(bytes) {
  return new Response(bytes, {
    status: 200,
    headers: {
      "Content-Type": "image/png",
      "Cache-Control": `public, max-age=${TTL}, immutable`,
      "Access-Control-Allow-Origin": "*",          // the game canvas reads tile pixels (water check)
      "Cross-Origin-Resource-Policy": "cross-origin",
    },
  });
}
function bodyless(resp) { return new Response(null, { status: resp.status, headers: resp.headers }); }
function notFound() { return new Response("not found", { status: 404 }); }
