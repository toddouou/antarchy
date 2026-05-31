const FOG_CLEAR_DIST: f32 = 20.0;
const FOG_GRAD_DIST:  f32 = 30.0;
/// Padding (in tiles) the ownership slice must carry around the visible rect so the
/// distance transform sees owned tiles just off-screen. == FOG_GRAD_DIST as i32.
pub const PAD: i32 = 30;

/// Two-pass Chebyshev distance transform over a pre-extracted ownership slice.
///
/// `owners` is a row-major `pw × ph` grid covering the visible rect padded by `pad`
/// on every side; a cell equal to `owner_id` counts as owned (distance 0). Out-of-world
/// cells must be stored as 0 by the caller (they simply aren't seeds). Returns a `w × h`
/// Uint8 fog array (0 = clear, 100 = fully fogged) for the inner, unpadded region.
///
/// Operating on a slice (not `&World`) is deliberate: it lets fog run **outside** the
/// World read lock, off the simulation thread. Admin no-fog is handled by the caller.
pub fn compute_fog_field_slice(
    owners: &[u32], pw: usize, ph: usize, pad: usize, w: usize, h: usize, owner_id: u32,
) -> Vec<u8> {
    let n = pw * ph;
    let big: f32 = FOG_GRAD_DIST + 5.0;
    let mut dist = vec![big; n];

    // Seed: own tiles → distance 0
    for i in 0..n {
        if owners[i] == owner_id { dist[i] = 0.0; }
    }

    const D1: f32 = 1.0;
    const D2: f32 = std::f32::consts::SQRT_2;

    // Forward pass (top-left → bottom-right)
    for py in 0..ph {
        for px in 0..pw {
            let i = py * pw + px;
            let mut d = dist[i];
            if py > 0 {
                let prev_row = i - pw;
                if px > 0 { d = d.min(dist[prev_row - 1] + D2); }
                           d = d.min(dist[prev_row]     + D1);
                if px + 1 < pw { d = d.min(dist[prev_row + 1] + D2); }
            }
            if px > 0 { d = d.min(dist[i - 1] + D1); }
            dist[i] = d;
        }
    }

    // Backward pass (bottom-right → top-left)
    for py in (0..ph).rev() {
        for px in (0..pw).rev() {
            let i = py * pw + px;
            let mut d = dist[i];
            if py + 1 < ph {
                let next_row = i + pw;
                if px + 1 < pw { d = d.min(dist[next_row + 1] + D2); }
                                 d = d.min(dist[next_row]     + D1);
                if px > 0      { d = d.min(dist[next_row - 1] + D2); }
            }
            if px + 1 < pw { d = d.min(dist[i + 1] + D1); }
            dist[i] = d;
        }
    }

    // Map distances to 0..100 fog values
    let mut out = vec![0u8; w * h];
    for oy in 0..h {
        for ox in 0..w {
            let d = dist[(oy + pad) * pw + (ox + pad)];
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
