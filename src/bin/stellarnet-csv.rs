//! stellarnet-csv — a collection out as CSV, for tools that cannot read VSF. One row per saved spectrum, to stdout:
//!
//! `name, time (UTC, for reading), quantity, exposure_ms, frames, clipped, L, M, S, then one column per wavelength`
//!
//! Usage: stellarnet-csv <collection.vsf> [--quantity shown|counts|ratio|emission] [--nm LO:HI:STEP]
//!
//! The quantity is the one on screen at each save unless `--quantity` names one, computed by the same code the window uses. A spectrum without the dark and reference its quantity needs is left out, with a note on stderr. The columns are the instrument's own pixels unless `--nm` asks for a grid, each grid point the mean of the pixels within half a step of it (linear between pixels where the step is finer than they are); every spectrum must share pixels for the native columns.

use std::io::Write;
use std::path::Path;

use stellarnet_view::collection::{self, Spectrum};
use stellarnet_view::quantity::compute;
use stellarnet_view::{EagleTime, Quantity, colour};

fn usage() -> ! {
    eprintln!("usage: stellarnet-csv <collection.vsf> [--quantity shown|counts|ratio|emission] [--nm LO:HI:STEP]");
    std::process::exit(2)
}

/// The value at each grid point: the mean of the pixels within half a step of it, or linear between the two pixels around it when no pixel is that close.
fn regrid(nm: &[f32], v: &[f32], grid: &[f32], step: f32) -> Vec<f32> {
    grid.iter()
        .map(|&g| {
            let (lo, hi) = (g - step * 0.5, g + step * 0.5);
            let (a, b) = (nm.partition_point(|&l| l < lo), nm.partition_point(|&l| l < hi));
            if b > a {
                return v[a..b].iter().sum::<f32>() / (b - a) as f32;
            }
            let j = nm.partition_point(|&l| l <= g);
            if j == 0 || j == nm.len() {
                return f32::NAN;
            }
            let t = (g - nm[j - 1]) / (nm[j] - nm[j - 1]);
            v[j - 1] * (1. - t) + v[j] * t
        })
        .collect()
}

/// A CSV field for human words: quoted, quotes doubled.
fn quoted(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// A number, or nothing where there is none.
fn num(x: f32) -> String {
    if x.is_finite() { x.to_string() } else { String::new() }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut path = None;
    let mut quantity: Option<Quantity> = None;
    let mut grid: Option<(Vec<f32>, f32)> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--quantity" => {
                i += 1;
                quantity = match args.get(i).map(String::as_str) {
                    Some("shown") => None,
                    Some(q) => Some(Quantity::ALL.into_iter().find(|k| k.name().eq_ignore_ascii_case(q)).unwrap_or_else(|| usage())),
                    None => usage(),
                };
            }
            "--nm" => {
                i += 1;
                let parts: Vec<f32> = args.get(i).unwrap_or_else(|| usage()).split(':').map(|p| p.parse().unwrap_or_else(|_| usage())).collect();
                let [lo, hi, step] = parts[..] else { usage() };
                if !(step > 0. && hi >= lo) {
                    usage();
                }
                let n = ((hi - lo) / step).round() as usize;
                grid = Some(((0..=n).map(|k| lo + k as f32 * step).collect(), step));
            }
            p if path.is_none() && !p.starts_with("--") => path = Some(p.to_string()),
            _ => usage(),
        }
        i += 1;
    }
    let path = path.unwrap_or_else(|| usage());
    let spectra: Vec<Spectrum> = collection::load(Path::new(&path)).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1)
    });
    if grid.is_none() && spectra.windows(2).any(|w| w[0].nm != w[1].nm) {
        eprintln!("{path}: its spectra are on different pixels (another instrument?); give a grid with --nm LO:HI:STEP");
        std::process::exit(1);
    }
    let columns: Vec<f32> = match &grid {
        Some((g, _)) => g.clone(),
        None => spectra.first().map(|s| s.nm.clone()).unwrap_or_default(),
    };
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    let head: Vec<String> = ["name", "time_utc", "quantity", "exposure_ms", "frames", "clipped", "L", "M", "S"].iter().map(|s| s.to_string()).chain(columns.iter().map(|l| l.to_string())).collect();
    writeln!(out, "{}", head.join(",")).unwrap();
    for sp in &spectra {
        let q = quantity.unwrap_or(sp.shown);
        let Some(v) = compute(q, &sp.nm, &sp.sample, sp.dark.as_ref(), sp.reference.as_ref()) else {
            eprintln!("left out \"{}\": {} needs a dark and a reference, and it was saved without", sp.name, q.name());
            continue;
        };
        // Counts give a colour only less the dark, as in the window.
        let lms = (q != Quantity::Counts || sp.dark.is_some()).then(|| colour::spectrum_lms(&sp.nm, &v));
        let values = match &grid {
            Some((g, step)) => regrid(&sp.nm, &v, g, *step),
            None => v,
        };
        let when = EagleTime::from_oscillations(sp.at).to_datetime_opt().map_or(String::new(), |t| t.to_string());
        let mut row = vec![
            quoted(&sp.name),
            when,
            q.name().to_string(),
            sp.sample.ms.to_string(),
            sp.sample.scans.to_string(),
            sp.sample.clipped.iter().filter(|&&c| c).count().to_string(),
        ];
        row.extend(lms.map_or([f32::NAN; 3], |l| l).iter().map(|&x| num(x)));
        row.extend(values.iter().map(|&x| num(x)));
        writeln!(out, "{}", row.join(",")).unwrap();
    }
}
