//! Dark drift: blocks of frames at one integration over a few minutes, the masked pixels' level beside the spectral pixels' (the UV end, the middle, the whole), to see whether the masked ones move with the baseline. Sources off: `cargo run --release --example drift -- <ms> <frames per block> <blocks>`

use std::time::Instant;

use stellarnet::{MASK_MARK, MASKED, PIXELS, Spectrometer};

fn main() {
    let a: Vec<u32> = std::env::args().skip(1).map(|v| v.parse().unwrap()).collect();
    let (ms, frames, blocks) = (a[0], a[1], a[2]);
    let mut s = Spectrometer::open().unwrap_or_else(|e| panic!("{e}"));
    s.set_integration(ms).unwrap();
    let nm = s.wavelengths();
    let uv = (MASKED..PIXELS).filter(|&p| nm[p] < 300.).collect::<Vec<_>>();
    let mid = (MASKED..PIXELS).filter(|&p| (500. ..700.).contains(&nm[p])).collect::<Vec<_>>();
    let t0 = Instant::now();
    println!("    t_s   masked    uv<300  500-700      all");
    for _ in 0..blocks {
        let mut sum = vec![0f64; PIXELS];
        for _ in 0..frames {
            for (s, c) in sum.iter_mut().zip(s.read().unwrap()) {
                *s += c as f64;
            }
        }
        let mean = |ps: &[usize]| ps.iter().map(|&p| sum[p]).sum::<f64>() / (ps.len() * frames as usize) as f64;
        let masked: Vec<usize> = (0..MASK_MARK).collect();
        let all: Vec<usize> = (MASKED..PIXELS).collect();
        println!("{:7.1} {:8.2} {:9.2} {:8.2} {:8.2}", t0.elapsed().as_secs_f64(), mean(&masked), mean(&uv), mean(&mid), mean(&all));
    }
}
