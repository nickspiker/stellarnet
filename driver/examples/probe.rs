//! Opens the spectrometer, prints what it says about itself, and takes a few exposures: `cargo run --example probe`.

use std::time::Instant;

use stellarnet::{PIXELS, Spectrometer, XTiming};

fn main() {
    let t = Instant::now();
    let mut s = Spectrometer::open().unwrap_or_else(|e| panic!("{e}"));
    println!("{} #{} opened in {:?}", s.model, s.serial, t.elapsed());
    println!("coeffs {:?}", s.coeffs);
    let nm = s.wavelengths();
    println!("nm {:?} .. {:?}", &nm[..3], &nm[PIXELS - 3..]);
    for (ms, xt) in [(50, XTiming::Three), (10, XTiming::Three), (200, XTiming::One), (1, XTiming::Three)] {
        s.configure(ms, xt).unwrap_or_else(|e| panic!("{e}"));
        let t = Instant::now();
        let c = s.read().unwrap_or_else(|e| panic!("{e}"));
        let mean = c.iter().map(|&v| v as f64).sum::<f64>() / PIXELS as f64;
        println!("{ms:>4} ms {xt:?}: read in {:?}, first {:?} last {:?} max {} mean {mean:.1}", t.elapsed(), &c[..4], &c[PIXELS - 4..], c.iter().max().unwrap());
    }
}
