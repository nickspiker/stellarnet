//! Raw counts of a collection's spectra at a few wavelengths: the dark, the reference, the sample, and what Ratio makes of them. `cargo run --example inspect -- <file.vsf>`

use stellarnet_view::collection;

fn main() {
    let path = std::env::args().nth(1).expect("a collection");
    for sp in collection::load(std::path::Path::new(&path)).unwrap() {
        println!("== \"{}\"  sample {} ms x{}  dark {:?}  ref {:?}", sp.name, sp.sample.ms, sp.sample.scans, sp.dark.as_ref().map(|d| (d.ms, d.scans)), sp.reference.as_ref().map(|r| (r.ms, r.scans)));
        let (Some(d), Some(r)) = (&sp.dark, &sp.reference) else { continue };
        println!("   nm      dark     ref  sample  ref-dk  smp-dk   ratio");
        for l in std::env::args().skip(2).map(|a| a.parse::<f32>().unwrap()).collect::<Vec<_>>() {
            let k = sp.nm.partition_point(|&x| x < l);
            let (dv, rv, sv) = (d.counts[k], r.counts[k], sp.sample.counts[k]);
            println!("{:6.1} {:8.1} {:7.1} {:7.1} {:7.1} {:7.1} {:7.3}", sp.nm[k], dv, rv, sv, rv - dv, sv - dv, (sv - dv) / (rv - dv));
        }
    }
}
