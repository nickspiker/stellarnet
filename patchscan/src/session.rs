//! One target's measurement: the steps in the target's own order, what each captured, reflectance against the flat, the colour of a spectrum, and the VSF it is all saved as.
//!
//! The file is `<encryptcal>/pnx/<serial>.vsf` — one per target, rewritten after every capture and replaced by a rerun, never a second copy. Sections: `scan` (instrument, reference, target type and serial, when the session began, every pixel's wavelength), `rs50` (the reference's certified reflectance), one `indicator` per named indicator patch, then one section per capture — `dark`, `flat` (two: before and after the patches, to measure lamp drift), `patch` (one per patch) — each with its `index`, its `step` in the session, when it was taken, and both integrations' per-pixel means and noise, raw.

use vsf::colour::convert::apply_matrix_3x3_f32;
use vsf::colour::legacy::constants::VSF_RGB2SRGB;
use vsf::colour::spectral::constants::{LMS2VSF_RGB, LMS_2000_10DEG_1NM};
use vsf::file_format::VsfSection;
use vsf::types::eagle_time::EtType;
use vsf::types::tensor::Tensor;
use vsf::{Ascii, VsfBuilder, VsfType};

use crate::device::Exposure;

#[derive(Clone, Copy, PartialEq)]
pub enum Role {
    Dark,
    Flat,
    Patch,
}

pub struct Step {
    pub role: Role,
    /// The patch number (0–24) for patches; for the flat, 0 before the patches and 1 after; 0 for the dark.
    pub index: usize,
    pub short: Option<Exposure>,
    pub long: Option<Exposure>,
    /// Eagle oscillations when it was captured.
    pub at: i64,
}

/// The indicators after the 21 colour patches, by the target's own names.
pub const INDICATORS: [&str; 4] = ["IR Black", "UVI", "IR White", "Life"];

/// Where each patch sits on the 7 × 6 grid, (row, column) from 0, as chameleon reads the target (VERICHROME logo on the left, the dot code lower right): 0–20 row by row, then the indicators 21–24.
pub fn layout() -> Vec<(usize, usize)> {
    let mut v = Vec::new();
    for (row, cols) in [(0, 2..5), (1, 1..6), (2, 1..6), (3, 1..5), (4, 2..6)] {
        for c in cols {
            v.push((row, c));
        }
    }
    v.extend([(2, 6), (3, 0), (3, 6), (4, 1)]);
    v
}

impl Step {
    pub fn label(&self) -> String {
        match self.role {
            Role::Dark => "dark".into(),
            Role::Flat if self.index == 0 => "flat, before the patches".into(),
            Role::Flat => "flat, after the patches".into(),
            Role::Patch if self.index >= 21 => format!("patch {} — {}", self.index, INDICATORS[self.index - 21]),
            Role::Patch => format!("patch {}", self.index),
        }
    }

    pub fn instruction(&self) -> String {
        match self.role {
            Role::Dark => "Cover the fibre completely. The lights can stay on.".into(),
            Role::Flat if self.index == 0 => "Fibre on the RS50 puck.".into(),
            Role::Flat => "Fibre back on the RS50 puck.".into(),
            Role::Patch => {
                let (row, col) = layout()[self.index];
                format!("Fibre on the highlighted patch: row {row}, column {col}, counting from 0 with the logo on the left.")
            }
        }
    }

    pub fn captured(&self) -> bool {
        self.short.is_some() && self.long.is_some()
    }
}

pub fn steps() -> Vec<Step> {
    let mk = |role, index| Step { role, index, short: None, long: None, at: 0 };
    let mut s = vec![mk(Role::Dark, 0), mk(Role::Flat, 0)];
    s.extend((0..25).map(|i| mk(Role::Patch, i)));
    s.push(mk(Role::Flat, 1));
    s
}

/// The RS50's certified reflectance, from the CSV at `path` (`--rs50`).
pub fn rs50(path: &str) -> Result<Vec<(f32, f32)>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    Ok(text.lines().skip(1).filter_map(|l| { let (a, b) = l.split_once(',')?; Some((a.trim().parse().ok()?, b.trim().parse().ok()?)) }).collect())
}

pub fn interp(curve: &[(f32, f32)], x: f32) -> f32 {
    if curve.is_empty() {
        return 1.;
    }
    if x <= curve[0].0 {
        return curve[0].1;
    }
    for w in curve.windows(2) {
        if x <= w[1].0 {
            let t = (x - w[0].0) / (w[1].0 - w[0].0);
            return w[0].1 * (1. - t) + w[1].1 * t;
        }
    }
    curve[curve.len() - 1].1
}

/// Reflectance per pixel: (sample − dark) ÷ (flat − dark) × RS50. Pixels where the flat carries too little light to divide by are NaN.
pub fn reflectance(nm: &[f32], sample: &Exposure, flat: &Exposure, dark: &Exposure, rs: &[(f32, f32)]) -> Vec<f32> {
    (0..nm.len())
        .map(|k| {
            let f = flat.counts[k] - dark.counts[k];
            if f < 200. { f32::NAN } else { (sample.counts[k] - dark.counts[k]) / f * interp(rs, nm[k]) }
        })
        .collect()
}

/// The 10° cone fundamentals at a wavelength, linearly interpolated.
fn lms_at(l: f32) -> [f32; 3] {
    let s = &LMS_2000_10DEG_1NM;
    let x = l - s.start_nm;
    if x < 0. || x >= (s.data.len() / 3 - 1) as f32 {
        return [0.; 3];
    }
    let i = x as usize;
    let t = x - i as f32;
    [0, 1, 2].map(|c| s.data[i * 3 + c] * (1. - t) + s.data[(i + 1) * 3 + c] * t)
}

/// VSF RGB light to the screen's 8-bit sRGB — the window is an sRGB surface, so this is the output transform and nothing more.
fn to_screen(vsf_rgb: [f32; 3]) -> [u8; 3] {
    let srgb = apply_matrix_3x3_f32(&VSF_RGB2SRGB, &vsf_rgb);
    #[allow(deprecated)]
    let enc = |v: f32| (vsf::colour::legacy::transfer::srgb_oetf(v.clamp(0., 1.)) * 255.).round() as u8;
    srgb.map(enc)
}

/// The colour of a reflectance spectrum under Illuminant E, through the 10° cone fundamentals into VSF RGB.
pub fn swatch(nm: &[f32], refl: &[f32]) -> [u8; 3] {
    let (mut acc, mut white) = ([0f32; 3], [0f32; 3]);
    for (k, &l) in nm.iter().enumerate() {
        if !(390. ..=730.).contains(&l) || !refl[k].is_finite() {
            continue;
        }
        let m = lms_at(l);
        for c in 0..3 {
            acc[c] += refl[k] * m[c];
            white[c] += m[c];
        }
    }
    let lms = [0, 1, 2].map(|c| if white[c] > 0. { acc[c] / white[c] } else { 0. });
    to_screen(apply_matrix_3x3_f32(&LMS2VSF_RGB, &lms))
}

/// The colour of monochromatic light at `l`, at full brightness, for the readout's curve; None outside what the eye sees.
pub fn spectral_colour(l: f32) -> Option<[u8; 3]> {
    if !(390. ..=730.).contains(&l) {
        return None;
    }
    let v = apply_matrix_3x3_f32(&LMS2VSF_RGB, &lms_at(l)).map(|c| c.max(0.));
    let peak = v[0].max(v[1]).max(v[2]);
    if peak <= 0. {
        return None;
    }
    Some(to_screen(v.map(|c| c / peak)))
}

fn eagle(osc: i64) -> VsfType {
    VsfType::e(EtType::e6(osc))
}

fn exposure_fields(sec: &mut VsfSection, prefix: &str, e: &Exposure) {
    let u = |v: usize| VsfType::u(v, false);
    sec.add_field(format!("{prefix}_ms"), u(e.ms as usize));
    sec.add_field(format!("{prefix}_scans"), u(e.scans as usize));
    sec.add_field(format!("{prefix}_saturated"), u(e.saturated as usize));
    sec.add_field(format!("{prefix}_counts"), VsfType::t_f5(Tensor::new(vec![e.counts.len()], e.counts.clone())));
    sec.add_field(format!("{prefix}_std"), VsfType::t_f5(Tensor::new(vec![e.std.len()], e.std.clone())));
}

/// Write the session to `path`, replacing whatever is there.
pub fn save(path: &str, serial: u64, started: i64, nm: &[f32], rs: &[(f32, f32)], steps: &[Step]) -> Result<(), String> {
    let mut b = VsfBuilder::new()
        .add_section(
            "scan",
            vec![
                ("instrument".into(), VsfType::a(Ascii::lit("StellarNet (stellarnet_driver3 v2.5)"))),
                ("reference".into(), VsfType::a(Ascii::lit("RS50 7 mm PTFE"))),
                ("target_type".into(), VsfType::u(1, false)),
                ("target_serial".into(), VsfType::u(serial as usize, false)),
                ("started".into(), eagle(started)),
                ("nm".into(), VsfType::t_f5(Tensor::new(vec![nm.len()], nm.to_vec()))),
            ],
        )
        .add_section(
            "rs50",
            vec![
                ("nm".into(), VsfType::t_f5(Tensor::new(vec![rs.len()], rs.iter().map(|p| p.0).collect()))),
                ("reflectance".into(), VsfType::t_f5(Tensor::new(vec![rs.len()], rs.iter().map(|p| p.1).collect()))),
            ],
        );
    for (k, name) in INDICATORS.iter().enumerate() {
        b = b.add_section("indicator", vec![("index".into(), VsfType::u(21 + k, false)), ("name".into(), VsfType::a(Ascii::lit(name)))]);
    }
    for (i, s) in steps.iter().enumerate() {
        let (Some(sh), Some(lo)) = (&s.short, &s.long) else { continue };
        let mut sec = VsfSection::new(match s.role {
            Role::Dark => "dark",
            Role::Flat => "flat",
            Role::Patch => "patch",
        });
        sec.add_field("index", VsfType::u(s.index, false));
        sec.add_field("step", VsfType::u(i, false));
        sec.add_field("at", eagle(s.at));
        exposure_fields(&mut sec, "short", sh);
        exposure_fields(&mut sec, "long", lo);
        b = b.add_section_direct(sec);
    }
    let bytes = b.build()?;
    // Written beside the target under one fixed hidden name, then renamed over it: a kill mid-write can only leave that one temp (replaced by the next Save), never a half-written target file.
    let p = std::path::Path::new(path);
    let tmp = p.with_file_name(format!(".{}.part", p.file_name().and_then(|f| f.to_str()).unwrap_or("scan")));
    std::fs::write(&tmp, &bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, p).map_err(|e| format!("{path}: {e}"))
}
