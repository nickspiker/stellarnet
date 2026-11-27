//! What the instrument does when an exposure is walked away from: start a long one, leave it part way, reconfigure short, and read on. Prints how long each step takes and whether each frame's 27 masked pixels sit where they belong: `cargo run --example abort`.

use std::time::{Duration, Instant};

use stellarnet::{MASK_MARK, MASKED, Spectrometer, XTiming};

fn intact(c: &[u16]) -> bool {
    c[..MASK_MARK].iter().all(|&v| v < 1 << 10) && c[MASKED..].iter().all(|&v| v >= 1 << 10)
}

fn main() {
    let mut s = Spectrometer::open().unwrap_or_else(|e| panic!("{e}"));
    for (long, short) in [(4000, 50), (4000, 4), (1000, 50)] {
        s.configure(long, XTiming::Three).unwrap();
        let t = Instant::now();
        let left = s.read_while(|| t.elapsed() < Duration::from_millis(500)).unwrap();
        println!("{long} ms exposure walked away from after {:?} (frame: {})", t.elapsed(), left.is_some());
        let t = Instant::now();
        s.configure(short, XTiming::Three).unwrap();
        println!("  reconfigure to {short} ms, with its throwaway frame: {:?}", t.elapsed());
        let mut ok = 0;
        for _ in 0..1 << 5 {
            let t = Instant::now();
            let c = s.read().unwrap();
            if t.elapsed() > Duration::from_millis(short as u64 * 4 + 100) {
                println!("  slow {short} ms read: {:?}", t.elapsed());
            }
            ok += intact(&c) as u32;
        }
        println!("  {ok}/32 frames intact after");
    }
}
