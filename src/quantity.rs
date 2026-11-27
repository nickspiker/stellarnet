//! What a frame of counts becomes on screen and in an export: Counts (less the dark, when there is one), Ratio to a reference, or Emission against the SL1 lamp. One function, so the window and the CSV exporter give the same numbers.

use crate::sl1;

/// One averaged block of frames from the instrument, spectral pixels only (the masked head of the readout dropped).
#[derive(Clone)]
pub struct Frame {
    /// Integration per frame, ms.
    pub ms: u32,
    /// Frames averaged.
    pub scans: u32,
    /// Mean counts per pixel.
    pub counts: Vec<f32>,
    /// Pixels that hit full scale in any frame of the block.
    pub clipped: Vec<bool>,
    /// The detector's baseline in this block: the mean of the masked pixels at the head of the readout (`stellarnet::MASK_MARK` of them), which see no light. None for frames saved before it was kept (2026-11-27).
    pub black: Option<f32>,
}

impl Frame {
    /// How far the baseline moved from `dark`'s to this frame's, to take off everything this frame read less the dark. Measured 2026-11-27, sources off, 609 ms: the spectral pixels moved 1.05 to 1.07 counts per count the masked ones did (r 0.89), so a 1:1 shift, and that halved the dark scatter. Zero when either frame has no baseline (saved before it was kept).
    pub fn drift_from(&self, dark: &Frame) -> f32 {
        match (self.black, dark.black) {
            (Some(s), Some(d)) => s - d,
            _ => 0.,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Quantity {
    Counts,
    Ratio,
    Emission,
}

impl Quantity {
    pub const ALL: [Quantity; 3] = [Quantity::Counts, Quantity::Ratio, Quantity::Emission];

    pub fn name(self) -> &'static str {
        match self {
            Quantity::Counts => "Counts",
            Quantity::Ratio => "Ratio",
            Quantity::Emission => "Emission",
        }
    }

    /// The code a file stores it as.
    pub fn code(self) -> u64 {
        self as u64
    }

    pub fn from_code(code: u64) -> Option<Quantity> {
        Quantity::ALL.into_iter().find(|q| q.code() == code)
    }
}

/// A reference pixel with less light than this over its dark is too dim to divide by: its ratio is NaN, not noise blown up.
pub const REF_FLOOR: f32 = (1 << 7) as f32;

/// The SL1 lamp's relative power at `l`, linearly between its table's points; zero outside them.
pub fn sl1_at(l: f32) -> f32 {
    let (x, y) = (&sl1::SL1_EMISSION_WAVELENGTHS, &sl1::SL1_EMISSION);
    let j = x.partition_point(|&v| v <= l);
    if j == 0 || j == x.len() {
        return 0.;
    }
    let t = (l - x[j - 1]) / (x[j] - x[j - 1]);
    y[j - 1] * (1. - t) + y[j] * t
}

/// `q` of `sample`, given the dark and reference taken at its exposure (the caller's to match). None when Ratio or Emission lacks either.
/// Counts: sample − dark, or the sample as read with no dark. Ratio: (sample − dark) ÷ (reference − dark), NaN where either clipped or the reference is under [`REF_FLOOR`]. Emission: that ratio × the SL1's spectrum, normalized to its peak.
pub fn compute(q: Quantity, nm: &[f32], sample: &Frame, dark: Option<&Frame>, reference: Option<&Frame>) -> Option<Vec<f32>> {
    // Less the dark means less the dark's baseline too: what the baseline moved between the dark and each frame comes off that frame (`Frame::drift_from`). Uncorrected, a drift of 100-odd counts over an hour sat on a UV reference of a few hundred and read as half the light (2026-11-27, black tape at 300 nm).
    if q == Quantity::Counts {
        return Some(match dark {
            Some(d) => {
                let drift = sample.drift_from(d);
                sample.counts.iter().zip(&d.counts).map(|(s, d)| s - d - drift).collect()
            }
            None => sample.counts.clone(),
        });
    }
    let (d, r) = (dark?, reference?);
    let (s_drift, r_drift) = (sample.drift_from(d), r.drift_from(d));
    let mut v: Vec<f32> = (0..sample.counts.len())
        .map(|k| {
            let lit = r.counts[k] - d.counts[k] - r_drift;
            if sample.clipped[k] || r.clipped[k] || lit < REF_FLOOR { f32::NAN } else { (sample.counts[k] - d.counts[k] - s_drift) / lit }
        })
        .collect();
    if q == Quantity::Emission {
        v.iter_mut().zip(nm).for_each(|(x, &l)| *x *= sl1_at(l));
        let peak = v.iter().copied().filter(|x| x.is_finite()).fold(0., f32::max);
        if peak > 0. {
            v.iter_mut().for_each(|x| *x /= peak);
        }
    }
    Some(v)
}
