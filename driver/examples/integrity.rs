//! Counts intact frames at each integration given: the first sixteen masked pixels low, and every pixel from the spectrum's start on at or above the bias. A frame that starts mid-scan carries its low run somewhere else, and this says where: `cargo run --example integrity -- 1 2 3`.

use stellarnet::{MASK_MARK, MASKED, Spectrometer};

fn main() {
    let mut s = Spectrometer::open().unwrap_or_else(|e| panic!("{e}"));
    for ms in std::env::args().skip(1).map(|a| a.parse::<u32>().expect("integration in ms")) {
        s.set_integration(ms).unwrap();
        let frames = std::env::var("FRAMES").map_or(1 << 6, |f| f.parse().unwrap());
        let mut intact = 0;
        let mut odd = Vec::new();
        for _ in 0..frames {
            let c = s.read().unwrap();
            if c[..MASK_MARK].iter().all(|&v| v < 1 << 10) && c[MASKED..].iter().all(|&v| v >= 1 << 10) {
                intact += 1;
            } else {
                let low: Vec<usize> = (0..c.len()).filter(|&p| c[p] < 1 << 10).collect();
                odd.push(match (low.first(), low.last()) {
                    (Some(a), Some(b)) => format!("{}@{a}..={b}", low.len()),
                    _ => "none".into(),
                });
            }
        }
        println!("{ms:>3} ms: {intact}/{frames} intact; others: {odd:?}");
    }
}
