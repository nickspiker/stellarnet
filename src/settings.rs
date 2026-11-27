//! What the tool keeps between runs: the wavelength range the plot opens on and returns to. One VSF beside the collections, `<dir>/.stellarnet.vsf` — a name no collection can take (`collection::path_for` refuses names that start with '.') — written whole on every change, through a hidden temp and a rename, as a collection is.

use std::path::{Path, PathBuf};

use vsf::verification::read_verified;
use vsf::{VsfBuilder, VsfHeader, VsfSection, VsfType};

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Settings {
    /// The plot's wavelength range, nm: what it opens on, and what Home or a right click returns it to.
    pub nm_min: f32,
    pub nm_max: f32,
}

pub fn path(dir: &Path) -> PathBuf {
    dir.join(".stellarnet.vsf")
}

fn float(sec: &VsfSection, name: &str) -> Option<f32> {
    match sec.get_field(name).and_then(|f| f.values.first()) {
        Some(VsfType::f5(v)) => Some(*v),
        Some(VsfType::f6(v)) => Some(*v as f32),
        _ => None,
    }
}

/// The settings in `dir`, or None when there are none yet; an error if the file is there but is not settings.
pub fn load(dir: &Path) -> Result<Option<Settings>, String> {
    let p = path(dir);
    if !p.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
    let (header, end): (VsfHeader, usize) = read_verified(&bytes, None).map_err(|e| format!("{}: {e}", p.display()))?;
    let sections = header.sections(&bytes, end).map_err(|e| format!("{}: {e}", p.display()))?;
    let view = sections.iter().find(|s| s.name == "view").ok_or_else(|| format!("{}: no view section", p.display()))?;
    match (float(view, "nm_min"), float(view, "nm_max")) {
        (Some(nm_min), Some(nm_max)) if nm_min < nm_max => Ok(Some(Settings { nm_min, nm_max })),
        _ => Err(format!("{}: no wavelength range in it", p.display())),
    }
}

pub fn save(dir: &Path, s: &Settings) -> Result<(), String> {
    let bytes = VsfBuilder::new()
        .creation_time_oscillations(vsf::eagle_time_oscillations())
        .add_section("view", vec![("nm_min".into(), VsfType::f5(s.nm_min)), ("nm_max".into(), VsfType::f5(s.nm_max))])
        .build()?;
    let p = path(dir);
    let tmp = dir.join(".stellarnet.vsf.part");
    std::fs::write(&tmp, &bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &p).map_err(|e| format!("{}: {e}", p.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let dir = std::env::temp_dir().join(format!("stellarnet-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(load(&dir).unwrap(), None);
        let s = Settings { nm_min: 380., nm_max: 780.5 };
        save(&dir, &s).unwrap();
        assert_eq!(load(&dir).unwrap(), Some(s));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
