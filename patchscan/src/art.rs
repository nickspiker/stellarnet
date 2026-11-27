//! The printed parts of the target that are not measured, drawn so the screen looks like the target in hand: the roundcode (the target's type and serial, to check the right target is under the fibre), the VERICHROME wordmark cell, and the four corner bullseyes.
//!
//! The roundcode is makeroundcode's, the program that made the printed codes: the same bytes, scramble, Reed–Solomon, ring layout, fixed inner ring and corner doughnuts. chameleon's `decode_roundcode` reads exactly this back.

use reed_solomon::Encoder;

fn next_rand(prevrand: u8, index: usize) -> u8 {
    let mut crand = prevrand as usize;
    crand = crand * 106189 + prevrand as usize + 487 + index * 2543;
    crand %= 379;
    while crand > 255 {
        crand *= 106189;
        crand %= 379;
    }
    crand as u8
}

/// The 126 cells of a target's roundcode: the fixed inner ring (cells 0, 1, 3 white), then the scrambled, Reed–Solomon-protected type and serial.
fn roundcode_cells(ttype: u8, serial: u64) -> [bool; 126] {
    let mut data = [false; 126];
    data[0] = true;
    data[1] = true;
    data[3] = true;
    let mut plain = [0u8; 11];
    plain[..8].copy_from_slice(&serial.to_le_bytes());
    plain[8] = ttype;
    plain[9] = 129;
    plain[10] = 83;
    let mut number = 37u8;
    for (i, b) in plain.iter_mut().enumerate() {
        *b ^= number;
        number = next_rand(*b, i);
    }
    let coded = Encoder::new(4).encode(&plain);
    for (i, byte) in coded.iter().enumerate() {
        for k in 0..8 {
            if 6 + i * 8 + k < 126 && (byte >> k) & 1 == 1 {
                data[6 + i * 8 + k] = true;
            }
        }
    }
    data
}

/// The roundcode as printed, `n`×`n`, row-major, true = white: six rings of 6, 12, … 36 cells, and a doughnut in each corner.
pub fn roundcode(ttype: u8, serial: u64, n: usize) -> Vec<bool> {
    let data = roundcode_cells(ttype, serial);
    let (sides, rings) = (6f32, 6f32);
    let mut img = vec![false; n * n];
    for h in 0..n {
        for w in 0..n {
            let re = ((h as f32 + 0.5) / n as f32 * 2. - 1.) * (rings + 0.5);
            let im = (1. - (w as f32 + 0.5) / n as f32 * 2.) * (rings + 0.5);
            let norm = (re * re + im * im).sqrt();
            if norm < rings + 0.5 && norm > 0.5 {
                let c = (norm - 0.5).floor();
                let c = c * (c + 1.) * sides / 2.;
                let mut a = im.atan2(re) + std::f32::consts::PI;
                a = a / (std::f32::consts::PI * 2.) - 0.000_000_03;
                a = (a * (norm + 0.5).floor() * sides).floor();
                let cell = (a + c).round() as usize;
                img[h * n + w] = cell < 126 && data[cell];
            }
        }
    }
    // Corner doughnuts: outer radius n/16, inner half that, centred n/16 in from each corner.
    let r6 = (n / 16) as isize;
    for (ch, cw) in [(r6, r6), (r6, n as isize - r6), (n as isize - r6, r6), (n as isize - r6, n as isize - r6)] {
        for h in (ch - r6).max(0)..(ch + r6).min(n as isize) {
            for w in (cw - r6).max(0)..(cw + r6).min(n as isize) {
                let d2 = (h - ch) * (h - ch) + (w - cw) * (w - cw);
                if d2 < r6 * r6 && d2 >= r6 * r6 / 4 {
                    img[h as usize * n + w as usize] = true;
                }
            }
        }
    }
    img
}

/// A corner bullseye, `n`×`n`: rings evenly spaced in log radius, as printed — measured off `Target/Product artwork/Bullseye.tif`, one black-to-white transition every 0.083 in ln r (cos(38·ln r), chameleon's spiral filter at 2 × its frequency 18), the outermost band white. None outside the disc, else true = white.
pub fn bullseye(n: usize) -> Vec<Option<bool>> {
    let mut img = vec![None; n * n];
    for h in 0..n {
        for w in 0..n {
            let (x, y) = ((w as f32 + 0.5) / n as f32 * 2. - 1., (h as f32 + 0.5) / n as f32 * 2. - 1.);
            let r = (x * x + y * y).sqrt();
            if r <= 1. {
                img[h * n + w] = Some(-(r.ln() * 38.).sin() > 0.);
            }
        }
    }
    img
}

/// The VERICHROME cell, `n`×`n`: chameleon's own wordmark coverage — the wordmark, and the two rails as the spectrum violet → red. Per pixel: None outside the rounded square, else (wordmark coverage, rail coverage, position along the rail 0..1).
pub fn logo(n: usize) -> Vec<Option<(f32, f32, f32)>> {
    let cov = chameleon::logo::coverage(n);
    (0..n * n)
        .map(|i| {
            let (t, rail, ru, m) = (cov[i * 4], cov[i * 4 + 1], cov[i * 4 + 2], cov[i * 4 + 3]);
            if m <= 0. { None } else { Some((t, rail, if rail > 0. { ru / rail } else { 0. })) }
        })
        .collect()
}
