//! A collection: one VSF file of saved spectra, `<dir>/<name>.vsf`, a `spectrum` section per save. Each holds the raw sample and whatever dark and reference applied at its exposure, so any quantity can be recomputed from it later; derived values are never stored.
//!
//! Every save rewrites the whole file: written beside it under one fixed hidden name, then renamed over it, so a kill mid-write leaves that temp, never a half-written collection.

use std::path::{Path, PathBuf};

use vsf::verification::read_verified;
use vsf::{BitPackedTensor, EtType, Tensor, VsfBuilder, VsfHeader, VsfSection, VsfType};

use crate::quantity::{Frame, Quantity};


pub struct Spectrum {
    /// What the sample is, in the user's words.
    pub name: String,
    /// When it was saved, eagle-time oscillations.
    pub at: i64,
    pub model: String,
    pub serial: String,
    /// The quantity on screen when it was saved.
    pub shown: Quantity,
    /// Wavelength of each spectral pixel, nm.
    pub nm: Vec<f32>,
    pub sample: Frame,
    pub dark: Option<Frame>,
    pub reference: Option<Frame>,
}

/// The file for a collection named `name` in `dir`. A name is the file's own name: not empty, no path separator, not hidden.
pub fn path_for(dir: &Path, name: &str) -> Result<PathBuf, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("name the collection first".into());
    }
    if name.contains('/') || name.starts_with('.') {
        return Err(format!("\"{name}\" cannot be a file name: no '/', and not starting with '.'"));
    }
    Ok(dir.join(format!("{name}.vsf")))
}

fn put_frame(sec: &mut VsfSection, prefix: &str, f: &Frame) {
    let u = |v: u32| VsfType::u(v as usize, false);
    sec.add_field(format!("{prefix}_ms"), u(f.ms));
    sec.add_field(format!("{prefix}_scans"), u(f.scans));
    sec.add_field(format!("{prefix}_counts"), VsfType::t_f5(Tensor::new(vec![f.counts.len()], f.counts.clone())));
    let bits: Vec<u8> = f.clipped.iter().map(|&c| c as u8).collect();
    sec.add_field(format!("{prefix}_clipped"), VsfType::p(BitPackedTensor::pack(1, vec![bits.len()], &bits)));
    if let Some(b) = f.black {
        sec.add_field(format!("{prefix}_black"), VsfType::f5(b));
    }
}

fn floats(sec: &VsfSection, name: &str) -> Option<Vec<f32>> {
    match sec.get_field(name).and_then(|f| f.values.first()) {
        Some(VsfType::t_f5(t)) => Some(t.data.clone()),
        _ => None,
    }
}

/// The frame stored under `prefix`, None if there is none; an error if it is there but not whole, or not `n` pixels.
fn get_frame(sec: &VsfSection, prefix: &str, n: usize) -> Result<Option<Frame>, String> {
    let Some(counts) = floats(sec, &format!("{prefix}_counts")) else { return Ok(None) };
    let num = |field: &str| sec.uint(&format!("{prefix}_{field}")).map(|v| v as u32).ok_or(format!("{prefix} has no {field}"));
    let clipped: Vec<bool> = match sec.get_field(&format!("{prefix}_clipped")).and_then(|f| f.values.first()) {
        Some(VsfType::p(bits)) if bits.bit_depth == 1 => bits.unpack_u8().into_iter().map(|b| b != 0).collect(),
        _ => return Err(format!("{prefix} has no clipping mask")),
    };
    if counts.len() != n || clipped.len() != n {
        return Err(format!("{prefix} has {} counts and {} clip flags for {n} pixels", counts.len(), clipped.len()));
    }
    // The baseline came in 2026-11-27; a frame saved before it has none and is taken as it was.
    let black = match sec.get_field(&format!("{prefix}_black")).and_then(|f| f.values.first()) {
        Some(VsfType::f5(v)) => Some(*v),
        _ => None,
    };
    Ok(Some(Frame { ms: num("ms")?, scans: num("scans")?, counts, clipped, black }))
}

/// Every spectrum in the collection at `path`, after the file's provenance checks out.
pub fn load(path: &Path) -> Result<Vec<Spectrum>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    decode(&bytes).map_err(|e| format!("{}: {e}", path.display()))
}

/// The spectra in a collection's bytes, after their provenance checks out.
pub fn decode(bytes: &[u8]) -> Result<Vec<Spectrum>, String> {
    let (header, end): (VsfHeader, usize) = read_verified(bytes, None)?;
    let sections = header.sections(bytes, end)?;
    let mut out = Vec::new();
    for (i, s) in sections.iter().filter(|s| s.name == "spectrum").enumerate() {
        let bad = |e: String| format!("spectrum {i} (counting from 0): {e}");
        let text = |f: &str| s.text(f).map(str::to_string).ok_or_else(|| bad(format!("no {f}")));
        let nm = floats(s, "nm").ok_or_else(|| bad("no nm".into()))?;
        let n = nm.len();
        out.push(Spectrum {
            name: text("name")?,
            at: s.eagle("at").ok_or_else(|| bad("no time".into()))?,
            model: text("model")?,
            serial: text("serial")?,
            shown: s.uint("shown").and_then(Quantity::from_code).ok_or_else(|| bad("no quantity".into()))?,
            sample: get_frame(s, "sample", n).map_err(bad)?.ok_or_else(|| bad("no sample".into()))?,
            dark: get_frame(s, "dark", n).map_err(bad)?,
            reference: get_frame(s, "reference", n).map_err(bad)?,
            nm,
        });
    }
    Ok(out)
}

/// Write `spectra` as the collection at `path`, replacing it.
pub fn save(path: &Path, spectra: &[Spectrum]) -> Result<(), String> {
    let bytes = encode(spectra)?;
    let tmp = path.with_file_name(format!(".{}.part", path.file_name().and_then(|f| f.to_str()).unwrap_or("collection")));
    std::fs::write(&tmp, &bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// A collection's bytes: a complete VSF file, a `spectrum` section per spectrum.
pub fn encode(spectra: &[Spectrum]) -> Result<Vec<u8>, String> {
    let mut b = VsfBuilder::new().creation_time_oscillations(vsf::eagle_time_oscillations());
    for sp in spectra {
        let mut sec = VsfSection::new("spectrum");
        sec.add_field("name", VsfType::x(sp.name.clone()));
        sec.add_field("at", VsfType::e(EtType::e6(sp.at)));
        sec.add_field("model", VsfType::x(sp.model.clone()));
        sec.add_field("serial", VsfType::x(sp.serial.clone()));
        sec.add_field("shown", VsfType::u(sp.shown.code() as usize, false));
        sec.add_field("nm", VsfType::t_f5(Tensor::new(vec![sp.nm.len()], sp.nm.clone())));
        put_frame(&mut sec, "sample", &sp.sample);
        if let Some(d) = &sp.dark {
            put_frame(&mut sec, "dark", d);
        }
        if let Some(r) = &sp.reference {
            put_frame(&mut sec, "reference", r);
        }
        b = b.add_section_direct(sec);
    }
    b.build()
}

/// Add `sp` to the collection at `path` (made if it is not there yet), and say how many it now holds.
pub fn append(path: &Path, sp: Spectrum) -> Result<usize, String> {
    let mut all = if path.exists() { load(path)? } else { Vec::new() };
    all.push(sp);
    save(path, &all)?;
    Ok(all.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(ms: u32, base: f32, n: usize) -> Frame {
        Frame { ms, scans: 1 << 2, counts: (0..n).map(|k| base + k as f32 * 0.25).collect(), clipped: (0..n).map(|k| k % 7 == 3).collect(), black: Some(base * 0.25) }
    }

    /// Everything saved comes back as it went in: names in any script, the time, the quantity, every pixel, every clip flag, and the dark and reference only where they were.
    #[test]
    fn round_trip() {
        let n = 2016;
        let nm: Vec<f32> = (0..n).map(|k| 233. + k as f32 * 0.43).collect();
        let spectra = vec![
            Spectrum { name: "room light".into(), at: 1 << 40, model: "BLACK CXR-SR-X".into(), serial: "24032812".into(), shown: Quantity::Counts, nm: nm.clone(), sample: frame(64, 2000., n), dark: None, reference: None },
            Spectrum { name: "tint “Ω” 2".into(), at: (1 << 40) + 1, model: "BLACK CXR-SR-X".into(), serial: "24032812".into(), shown: Quantity::Emission, nm: nm.clone(), sample: frame(64, 9000., n), dark: Some(frame(64, 1950., n)), reference: Some(frame(64, 30000., n)) },
        ];
        let back = decode(&encode(&spectra).unwrap()).unwrap();
        assert_eq!(back.len(), 2);
        for (a, b) in spectra.iter().zip(&back) {
            assert_eq!((&a.name, a.at, &a.model, &a.serial, a.shown), (&b.name, b.at, &b.model, &b.serial, b.shown));
            assert_eq!(a.nm, b.nm);
            for (x, y) in [(Some(&a.sample), Some(&b.sample)), (a.dark.as_ref(), b.dark.as_ref()), (a.reference.as_ref(), b.reference.as_ref())] {
                assert_eq!(x.is_some(), y.is_some());
                if let (Some(x), Some(y)) = (x, y) {
                    assert_eq!((x.ms, x.scans, &x.counts, &x.clipped, x.black), (y.ms, y.scans, &y.counts, &y.clipped, y.black));
                }
            }
        }
    }
}
