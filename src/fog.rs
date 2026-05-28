use crate::world::World;

const FOG_CLEAR_DIST: f32 = 20.0;
const FOG_GRAD_DIST:  f32 = 30.0;
const PAD: i32 = 30;  // == FOG_GRAD_DIST as i32

/// Two-pass Chebyshev distance transform: identical algorithm to simulation.js.
/// Returns a Uint8 fog array for the viewport [x0,y0, w×h].
/// 0 = fully clear, 100 = fully fogged.
/// Admin players receive all-zeros (no fog).
pub fn compute_fog_field(world: &World, player_id: u32, x0: i32, y0: i32, w: usize, h: usize) -> Vec<u8> {
    if let Some(p) = world.players.get(&player_id) {
        if world.auth.is_admin_id(p.id) {
            return vec![0u8; w * h];
        }
    }

    let pw = w as i32 + PAD * 2;
    let ph = h as i32 + PAD * 2;
    let n  = (pw * ph) as usize;
    let big: f32 = FOG_GRAD_DIST + 5.0;

    let mut dist = vec![big; n];

    let ww = world.world_w as i64;
    let wh = world.world_h as i64;

    // Seed: own tiles → distance 0
    for py in 0..ph {
        let wy: i64 = y0 as i64 - PAD as i64 + py as i64;
        if wy < 0 || wy >= wh { continue; }
        for px in 0..pw {
            let wx: i64 = x0 as i64 - PAD as i64 + px as i64;
            if wx < 0 || wx >= ww { continue; }
            if world.tiles.get(wx as u32, wy as u32) == player_id {
                dist[(py * pw + px) as usize] = 0.0;
            }
        }
    }

    const D1: f32 = 1.0;
    const D2: f32 = 1.4142;

    // Forward pass (top-left → bottom-right)
    for py in 0..ph as usize {
        for px in 0..pw as usize {
            let i = py * pw as usize + px;
            let mut d = dist[i];
            if py > 0 {
                let prev_row = i - pw as usize;
                if px > 0 { d = d.min(dist[prev_row - 1] + D2); }
                           d = d.min(dist[prev_row]     + D1);
                if px + 1 < pw as usize { d = d.min(dist[prev_row + 1] + D2); }
            }
            if px > 0 { d = d.min(dist[i - 1] + D1); }
            dist[i] = d;
        }
    }

    // Backward pass (bottom-right → top-left)
    for py in (0..ph as usize).rev() {
        for px in (0..pw as usize).rev() {
            let i = py * pw as usize + px;
            let mut d = dist[i];
            if py + 1 < ph as usize {
                let next_row = i + pw as usize;
                if px + 1 < pw as usize { d = d.min(dist[next_row + 1] + D2); }
                                          d = d.min(dist[next_row]     + D1);
                if px > 0               { d = d.min(dist[next_row - 1] + D2); }
            }
            if px + 1 < pw as usize { d = d.min(dist[i + 1] + D1); }
            dist[i] = d;
        }
    }

    // Map distances to 0..100 fog values
    let mut out = vec![0u8; w * h];
    for oy in 0..h {
        for ox in 0..w {
            let d = dist[(oy as i32 + PAD) as usize * pw as usize + (ox as i32 + PAD) as usize];
            out[oy * w + ox] = if d <= FOG_CLEAR_DIST {
                0
            } else if d >= FOG_GRAD_DIST {
                100
            } else {
                ((d - FOG_CLEAR_DIST).round() * 10.0) as u8
            };
        }
    }
    out
}
