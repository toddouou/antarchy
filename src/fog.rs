/// Upper bound on the per-player ownership-slice padding (tiles). The pad is now chosen per viewer
/// from their level-scaled `grad_r` (see network.rs); this caps the worst case (≈ clear@cap + feather)
/// so a misconfigured curve can't explode the slice. Comfortably above `fog_grad_r(cap)` (~200 now
/// that the gradient spans clearR → 2·clearR).
pub const MAX_PAD: i32 = 224;

/// Two-pass Chebyshev distance transform over a pre-extracted ownership slice.
///
/// `owners` is a row-major `pw × ph` grid covering the visible rect padded by `pad`
/// on every side; a cell equal to `owner_id` counts as owned (distance 0). Out-of-world
/// cells must be stored as 0 by the caller (they simply aren't seeds). Returns a `w × h`
/// Uint8 fog array (0 = clear, 100 = fully fogged) for the inner, unpadded region.
///
/// `clear_r`/`grad_r` are the (level-scaled) clear and fully-fogged distances in slice cells:
/// `d ≤ clear_r` → 0, `d ≥ grad_r` → 100, linear in between. The caller sizes `pad ≈ ceil(grad_r)`
/// so off-screen owned tiles within the feather still seed the transform.
///
/// Operating on a slice (not `&World`) is deliberate: it lets fog run **outside** the
/// World read lock, off the simulation thread. Admin no-fog is handled by the caller.
pub fn compute_fog_field_slice(
    owners: &[u32], pw: usize, ph: usize, pad: usize, w: usize, h: usize, owner_id: u32,
    clear_r: f32, grad_r: f32,
) -> Vec<u8> {
    let n = pw * ph;
    // Guard against a degenerate band (grad ≤ clear); always keep at least a 1-cell feather.
    let grad_r = grad_r.max(clear_r + 1.0);
    let big: f32 = grad_r + 5.0;
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

    // Map distances to 0..100 fog values across the (variable, level-scaled) feather band.
    let band = (grad_r - clear_r).max(1.0);
    let mut out = vec![0u8; w * h];
    for oy in 0..h {
        for ox in 0..w {
            let d = dist[(oy + pad) * pw + (ox + pad)];
            out[oy * w + ox] = if d <= clear_r {
                0
            } else if d >= grad_r {
                100
            } else {
                (((d - clear_r) / band) * 100.0).round() as u8
            };
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Build a square owners slice with a single owned seed at the centre and run the transform with
    // no padding (pad 0, pw=w, ph=h). Returns the fog field for the inner region.
    fn field(dim: usize, owner: u32, clear_r: f32, grad_r: f32) -> Vec<u8> {
        let mut owners = vec![0u32; dim * dim];
        owners[(dim / 2) * dim + dim / 2] = owner;
        compute_fog_field_slice(&owners, dim, dim, 0, dim, dim, owner, clear_r, grad_r)
    }

    #[test]
    fn clear_radius_scales_with_level() {
        // A larger clear radius must reveal (fog == 0) at least as many cells as a smaller one.
        let small = field(41, 7, 5.0, 8.0);
        let large = field(41, 7, 15.0, 18.0);
        let zeros = |f: &[u8]| f.iter().filter(|&&v| v == 0).count();
        assert!(zeros(&large) > zeros(&small),
                "bigger clear radius should clear more cells: {} !> {}", zeros(&large), zeros(&small));
    }

    #[test]
    fn seed_is_clear_and_far_is_fogged() {
        let f = field(41, 7, 5.0, 8.0);
        let dim = 41;
        assert_eq!(f[(dim / 2) * dim + dim / 2], 0, "owned seed cell is fully clear");
        assert_eq!(f[0], 100, "a far corner is fully fogged");
    }
}
