//! The spectrum plot's drawing: a curve traced at sub-pixel steps along x, filled beneath with a per-column tint by area coverage, and stroked by distance to the trace with a smooth (logistic) edge. Everything lands `under` what is already in the buffer: callers paint front to back.

use fluor::canvas::Canvas;
use fluor::paint::{BlendMode, pack_argb};
use fluor::pixel::Blend;

/// Trace points per pixel column.
pub const SUB: usize = 1 << 3;

/// The plot's pixel rectangle and the wavelength span across it.
#[derive(Clone, Copy)]
pub struct Plot {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
    pub nm_lo: f32,
    pub nm_hi: f32,
}

impl Plot {
    pub fn x_of(&self, nm: f32) -> f32 {
        self.x as f32 + (nm - self.nm_lo) / (self.nm_hi - self.nm_lo) * self.w as f32
    }

    pub fn nm_of(&self, x: f32) -> f32 {
        self.nm_lo + (x - self.x as f32) / self.w as f32 * (self.nm_hi - self.nm_lo)
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x as f32 && x < (self.x + self.w) as f32 && y >= self.y as f32 && y < (self.y + self.h) as f32
    }
}

/// The curve's height in plot rows (0 at the top, `h` at the bottom, past either end where the value is off the axis) at every trace point: `y[k]` for the instrument pixel at `nm[k]`, linearly between pixels. NaN where the instrument has no pixel, or its value is not finite.
pub fn trace(p: &Plot, nm: &[f32], y: &[f32]) -> Vec<f32> {
    let n = p.w * SUB;
    let mut out = vec![f32::NAN; n];
    let mut j = 0;
    for (k, o) in out.iter_mut().enumerate() {
        let l = p.nm_lo + (k as f32 + 0.5) / n as f32 * (p.nm_hi - p.nm_lo);
        if l < nm[0] || l >= nm[nm.len() - 1] {
            continue;
        }
        while nm[j + 1] <= l {
            j += 1;
        }
        let t = (l - nm[j]) / (nm[j + 1] - nm[j]);
        *o = y[j] * (1. - t) + y[j + 1] * t;
    }
    out
}

/// Fill beneath the trace: each pixel's opacity is the share of its column's trace points it lies under (the top row of each by its fraction), times `alpha`; its colour `tint(column)`.
/// Fill beneath the trace: each pixel's opacity is the share of its column's trace points it lies under (the top row of each by its fraction), times `alpha`; its colour `tint(column)`. Built per column as a difference array and summed down it, so a column costs its height once whatever its trace points do.
pub fn fill(canvas: &mut Canvas, p: &Plot, ys: &[f32], alpha: f32, tint: impl Fn(usize) -> [u8; 3]) {
    let mut diff = vec![0f32; p.h + 1];
    let scale = alpha * 255. / SUB as f32;
    for c in 0..p.w {
        diff.fill(0.);
        for &y in &ys[c * SUB..(c + 1) * SUB] {
            // Off the axis either way is real data (a value above the top fills the whole column, one below fills none), and NaN fills nothing; only a top row inside the plot gets its fraction.
            if !(y < p.h as f32) {
                continue;
            }
            if y < 0. {
                diff[0] += 1.;
                continue;
            }
            let r = y as usize;
            let frac = y - r as f32;
            diff[r] += 1. - frac;
            diff[r + 1] += frac;
        }
        let [r, g, b] = tint(c);
        let mut cov = 0.;
        for row in 0..p.h {
            cov += diff[row];
            if cov > 0. {
                let i = (p.y + row) * canvas.width + p.x + c;
                canvas.pixels[i] = canvas.pixels[i].under(pack_argb(r, g, b, (cov * scale) as u8), BlendMode::Normal);
            }
        }
    }
}

/// The polyline through the data: each pixel's (plot-local x, row), straight between pixels as the trace is.
pub fn vertices(p: &Plot, nm: &[f32], rows: &[f32]) -> Vec<(f32, f32)> {
    nm.iter().zip(rows).map(|(&l, &y)| (p.x_of(l) - p.x as f32, y)).collect()
}

/// Stroke the polyline `t` pixels thick, broken where a point is not finite: each pixel's opacity is a logistic in its distance to the nearest segment, ½ at `t/2` and falling over about a pixel, times `alpha`. Each segment touches only the pixels within reach of it, keeping the strongest coverage each pixel gets, so the cost follows the curve's length rather than the plot's area.
pub fn line(canvas: &mut Canvas, p: &Plot, pts: &[(f32, f32)], t: f32, colour: [u8; 3], alpha: f32) {
    // Past t/2 + 2 the logistic is under 1/256 of full: nothing to paint.
    let reach = t * 0.5 + 2.;
    let mut cov = vec![0u8; p.w * p.h];
    for s in pts.windows(2) {
        let ((ax, ay), (bx, by)) = (s[0], s[1]);
        if !(ay.is_finite() && by.is_finite()) {
            continue;
        }
        // The segment's reach, cut to the plot: the data runs past both ends of the view and off the axis wherever it is off scale. Below zero the float-to-usize cast already gives 0.
        let x0 = (ax.min(bx) - reach) as usize;
        let x1 = (ax.max(bx) + reach + 1.).min(p.w as f32) as usize;
        let y0 = (ay.min(by) - reach) as usize;
        let y1 = (ay.max(by) + reach + 1.).min(p.h as f32) as usize;
        let (dx, dy) = (bx - ax, by - ay);
        let len2 = dx * dx + dy * dy;
        for row in y0..y1 {
            let yc = row as f32 + 0.5;
            for col in x0..x1 {
                let xc = col as f32 + 0.5;
                // The nearest point of the segment: the projection onto its line, held to the segment (geometry, not a guard). Two pixels can land on the same point when zoomed far out; that segment is a point, and dividing by its zero length would make NaN.
                let u = if len2 > 0. { (((xc - ax) * dx + (yc - ay) * dy) / len2).clamp(0., 1.) } else { 0. };
                let (ex, ey) = (ax + u * dx - xc, ay + u * dy - yc);
                let d = (ex * ex + ey * ey).sqrt();
                let a = (alpha * 255. / (1. + ((d - t * 0.5) * 4.).exp())) as u8;
                let k = row * p.w + col;
                cov[k] = cov[k].max(a);
            }
        }
    }
    for row in 0..p.h {
        for col in 0..p.w {
            let a = cov[row * p.w + col];
            if a > 0 {
                let i = (p.y + row) * canvas.width + p.x + col;
                canvas.pixels[i] = canvas.pixels[i].under(pack_argb(colour[0], colour[1], colour[2], a), BlendMode::Normal);
            }
        }
    }
}
