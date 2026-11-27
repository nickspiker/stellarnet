//! A second START at the same integration after walking away from the first: does the frame take a full integration from the new START (a fresh exposure) or arrive when the old one would have ended? `cargo run --example restart`.

use std::time::{Duration, Instant};

use stellarnet::{MASK_MARK, MASKED, Spectrometer, XTiming};

fn main() {
    let mut s = Spectrometer::open().unwrap_or_else(|e| panic!("{e}"));
    s.configure(2000, XTiming::Three).unwrap();
    for _ in 0..1 << 1 {
        let t = Instant::now();
        let left = s.read_while(|| t.elapsed() < Duration::from_millis(1000)).unwrap();
        println!("walked away after {:?} (frame: {})", t.elapsed(), left.is_some());
        let t = Instant::now();
        let c = s.read().unwrap();
        let ok = c[..MASK_MARK].iter().all(|&v| v < 1 << 10) && c[MASKED..].iter().all(|&v| v >= 1 << 10);
        println!("  next 2000 ms read took {:?}, intact {ok}", t.elapsed());
    }
}
