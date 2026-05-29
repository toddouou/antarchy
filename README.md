# HIVE-SIM

A real-time, planet-scale multiplayer **Langton's-ant territory war**, written in Rust. Players
place a queen on a world map, deploy Langton's-ant workers that paint territory under the classic
turn-and-flip rule, and fight for tiles across a world the size of Earth.

This is the Rust engine that replaced the original Node.js prototype.

## Quick start

```bash
cargo run            # debug
cargo run --release  # optimized (recommended for real play)
```

Then open <http://localhost:8080> in one or more browser tabs — each tab is a separate player.
The admin account is `ADMIN` / `admin`.

- **Health:** <http://localhost:8080/health>
- **World info:** <http://localhost:8080/world-info>

> The browser client (`public/client.html`) is compiled **into** the binary via `include_str!`.
> After editing it you must **recompile** (`cargo run`) — refreshing the browser is not enough.

## How it works (one-paragraph tour)

Everything runs in one process. A dedicated OS thread runs the simulation at a fixed tick rate
(default 50 Hz): it drains queued WebSocket commands, advances every ant one Langton step, repaints
tiles, resolves 5×5 majority clashes, applies queen damage, and flushes queued XP. The tokio side
serves HTTP + WebSocket; connection tasks never touch the world directly — they push commands onto
a channel the sim thread drains. Twice per tick the server streams each player a viewport-clipped,
base64-encoded tile snapshot plus a server-computed fog-of-war field, building all viewports in
parallel with rayon. The world lives entirely in memory as a sparse grid of 256×256 tile chunks.

The world is **1,500,000 × 750,000 tiles** at ≈26.72 m/tile (≈ Earth's circumference). The client
overlays the territory grid on OpenStreetMap raster tiles using a Mercator projection.

## Layout

See [`CLAUDE.md`](./CLAUDE.md) for the full file-by-file map, architecture notes, the WebSocket
message protocol, and the simulation invariants.

```
src/         Rust engine (main, server, simulation, world, tile_map, fog, network, handlers, auth, config)
public/      client.html — single-file canvas client (embedded into the binary)
Cargo.toml   manifest
```

## Status / roadmap

The longer-term product vision (visual overhaul, persistence, planet-scale sharding) lives in the
parent project's `ROADMAP.md`.
