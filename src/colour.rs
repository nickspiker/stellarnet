//! Colour from spectra: Stockman & Sharpe (2000) 10° cone fundamentals (vsf's table, 390–830 nm) into VSF RGB, then out to the window's sRGB surface.

use std::sync::OnceLock;

use vsf::colour::convert::apply_matrix_3x3_f32;
use vsf::colour::legacy::constants::VSF_RGB2SRGB;
use vsf::colour::spectral::constants::{LMS2PHOTOPIC, LMS2VSF_RGB, LMS_2000_10DEG_1NM};

/// The cone table's span, nm.
pub const LMS_NM: (f32, f32) = (390., 830.);

/// The cone fundamentals at `l`, linearly interpolated; zero outside the table.
pub fn lms_at(l: f32) -> [f32; 3] {
    let s = &LMS_2000_10DEG_1NM;
    let x = l - s.start_nm;
    // The wavelength comes from the instrument or the plot's axis and runs well past the table at both ends; outside it the eye has no response.
    if !(0. ..(s.data.len() / 3 - 1) as f32).contains(&x) {
        return [0.; 3];
    }
    let i = x as usize;
    let t = x - i as f32;
    [0, 1, 2].map(|c| s.data[i * 3 + c] * (1. - t) + s.data[(i + 1) * 3 + c] * t)
}

fn lms_to_srgb_linear(lms: [f32; 3]) -> [f32; 3] {
    apply_matrix_3x3_f32(&VSF_RGB2SRGB, &apply_matrix_3x3_f32(&LMS2VSF_RGB, &lms))
}

/// Relative photopic luminance of an LMS triple: equal-energy white (1, 1, 1) is 1. Never negative, since cone responses are not; sRGB luma of an out-of-gamut colour can be.
fn photopic(lms: [f32; 3]) -> f32 {
    lms[0] * LMS2PHOTOPIC[0] + lms[1] * LMS2PHOTOPIC[1] + lms[2] * LMS2PHOTOPIC[2]
}

/// Linear sRGB of an LMS triple, pulled into gamut by desaturating toward the neutral of the same photopic luminance exactly as far as its most negative channel needs: hue and luminance kept, no channel cut. Also whether it had to be.
fn in_gamut(lms: [f32; 3]) -> ([f32; 3], bool) {
    let rgb = lms_to_srgb_linear(lms);
    let y = photopic(lms);
    if y <= 0. {
        return ([0.; 3], false);
    }
    let grey = lms_to_srgb_linear([1.; 3]).map(|w| w * y);
    let k = (0..3).filter(|&c| rgb[c] < 0.).map(|c| grey[c] / (grey[c] - rgb[c])).fold(1f32, f32::min);
    ([0, 1, 2].map(|c| grey[c] + (rgb[c] - grey[c]) * k), k < 1.)
}

/// Linear sRGB in 0..=1 to 8-bit sRGB.
pub fn encode(rgb: [f32; 3]) -> [u8; 3] {
    #[allow(deprecated)]
    rgb.map(|v| (vsf::colour::legacy::transfer::srgb_oetf(v) * 255.).round() as u8)
}

/// The tint under the plot's curve at `l`: the monochromatic colour at that wavelength over a dim neutral that carries the UV and IR. Hue at the fullest saturation the screen allows; brightness the cube root of the light's relative luminance (so violet and deep red stay readable), faded to nothing over the last few nm of the cone table with a C∞ edge.
pub fn tint(l: f32) -> [f32; 3] {
    const BASE: f32 = 1. / (1 << 4) as f32;
    let lms = lms_at(l);
    let (rgb, _) = in_gamut(lms);
    let peak = rgb[0].max(rgb[1]).max(rgb[2]);
    if peak <= 0. {
        return [BASE; 3];
    }
    let edge = |d: f32| 1. - (-(d / (1 << 3) as f32).powi(2)).exp();
    let fade = edge(l - LMS_NM.0) * edge(LMS_NM.1 - l);
    let bright = (photopic(lms) / peak_photopic()).cbrt() * fade;
    rgb.map(|c| BASE + c / peak * bright * (1. - BASE))
}

/// The most luminous monochromatic light in the table: the 1 of `tint`'s brightness.
fn peak_photopic() -> f32 {
    static PEAK: OnceLock<f32> = OnceLock::new();
    *PEAK.get_or_init(|| (0..440).map(|k| photopic(lms_at(LMS_NM.0 + k as f32))).fold(0., f32::max))
}

/// A spectrum's cone response: Σ v·lms·Δλ over the instrument's pixels, divided by the same sum for an equal-energy spectrum of 1, so a flat 1 gives (1, 1, 1). Pixels whose value is not finite are left out of both sums.
pub fn spectrum_lms(nm: &[f32], v: &[f32]) -> [f32; 3] {
    let (mut acc, mut white) = ([0f32; 3], [0f32; 3]);
    for k in 1..nm.len() {
        let l = (nm[k] + nm[k - 1]) * 0.5;
        let dl = nm[k] - nm[k - 1];
        let x = (v[k] + v[k - 1]) * 0.5;
        if !x.is_finite() {
            continue;
        }
        let m = lms_at(l);
        for c in 0..3 {
            acc[c] += x * m[c] * dl;
            white[c] += m[c] * dl;
        }
    }
    [0, 1, 2].map(|c| if white[c] > 0. { acc[c] / white[c] } else { 0. })
}

/// The swatch for an LMS triple, and whether it had to be pulled into the screen's gamut or brought down to fit it. `absolute` keeps the level: a ratio of 1 everywhere (equal-energy white, whose brightest sRGB channel is a little over 1) is the brightest white the screen shows, so the screen's 1 is E-white's peak channel. Otherwise the brightest channel is brought to 1, so a light shows its colour whatever its strength.
pub fn swatch(lms: [f32; 3], absolute: bool) -> ([u8; 3], bool) {
    let (rgb, out) = in_gamut(lms);
    let peak = rgb[0].max(rgb[1]).max(rgb[2]);
    if peak <= 0. {
        return ([0; 3], out);
    }
    let white = lms_to_srgb_linear([1.; 3]);
    let one = white[0].max(white[1]).max(white[2]);
    let over = absolute && peak > one * (1. + 1. / (1 << 6) as f32);
    let scale = if absolute && !over { 1. / one } else { 1. / peak };
    (encode(rgb.map(|c| c * scale)), out || over)
}
