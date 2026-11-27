//! The instrument, on its own thread: a continuous live preview, and full captures on request. The spectrometer is driven directly over USB by the `stellarnet` crate; this thread is the only thing that talks to it.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};

use fluor::host::WakeSender;
use stellarnet::{FULL_SCALE, PIXELS, Spectrometer};

#[derive(Clone)]
pub struct Exposure {
    pub ms: u32,
    pub scans: u32,
    pub counts: Vec<f32>,
    pub std: Vec<f32>,
    pub saturated: u32,
}

/// What the UI asks for.
pub enum Cmd {
    /// A full capture: the short integration (the middle of the spectrum) then the long one (the UV and IR ends).
    Capture { short_ms: u32, short_scans: u32, long_ms: u32, long_scans: u32 },
    /// The integration the live preview runs at.
    LiveMs(u32),
}

/// What the thread reports.
pub enum Msg {
    /// The wavelength of every pixel, once, when the instrument opens.
    Ready(Vec<f32>),
    Live(Exposure),
    Progress { ms: u32, done: u32, of: u32 },
    Captured(Exposure, Exposure),
    Failed(String),
}

/// `scans` frames at `ms`: the mean and the (population) standard deviation of every pixel, and how many pixels hit full scale in any frame.
fn capture(s: &mut Spectrometer, ms: u32, scans: u32, mut progress: impl FnMut(u32)) -> Result<Exposure, stellarnet::Error> {
    if ms != s.integration_ms() {
        s.set_integration(ms)?;
        // set_integration already throws one frame away; the Python bridge found the next could still hold the previous integration, so that one goes too.
        s.read()?;
    }
    // Exact integer sums: 2048 pixels of u16 fit u64 for any scan count below 2^32, and n·Σv² − (Σv)² is then exact and never negative (Cauchy–Schwarz), so the standard deviation needs no float cancellation argument.
    let mut sum = vec![0u64; PIXELS];
    let mut sum_sq = vec![0u64; PIXELS];
    let mut peak = vec![0u16; PIXELS];
    for k in 0..scans {
        let frame = s.read()?;
        for p in 0..PIXELS {
            let v = frame[p] as u64;
            sum[p] += v;
            sum_sq[p] += v * v;
            peak[p] = peak[p].max(frame[p]);
        }
        progress(k + 1);
    }
    let n = scans as u64;
    let counts: Vec<f32> = sum.iter().map(|&t| (t as f64 / n as f64) as f32).collect();
    let std = sum.iter().zip(&sum_sq).map(|(&t, &q)| (((n as u128 * q as u128 - t as u128 * t as u128) as f64).sqrt() / n as f64) as f32);
    Ok(Exposure { ms, scans, counts, std: std.collect(), saturated: peak.iter().filter(|&&v| v == FULL_SCALE).count() as u32 })
}

/// The thread body. Every message wakes the UI.
pub fn run(cmds: Receiver<Cmd>, out: Sender<Msg>, wake: Arc<dyn WakeSender<()>>) {
    let send = |m: Msg| {
        let _ = out.send(m);
        let _ = wake.send(());
    };
    let mut s = match Spectrometer::open() {
        Ok(s) => s,
        Err(e) => return send(Msg::Failed(e.to_string())),
    };
    send(Msg::Ready(s.wavelengths().iter().map(|&nm| nm as f32).collect()));
    let mut live_ms = 70;
    loop {
        match cmds.try_recv() {
            Ok(Cmd::LiveMs(ms)) => live_ms = ms,
            Ok(Cmd::Capture { short_ms, short_scans, long_ms, long_scans }) => {
                let mut steps = |ms: u32, n: u32| capture(&mut s, ms, n, |k| send(Msg::Progress { ms, done: k, of: n }));
                let short = steps(short_ms, short_scans);
                let long = short.as_ref().ok().map(|_| steps(long_ms, long_scans));
                match (short, long) {
                    (Ok(s), Some(Ok(l))) => send(Msg::Captured(s, l)),
                    (Err(e), _) | (_, Some(Err(e))) => return send(Msg::Failed(e.to_string())),
                    _ => {}
                }
                continue;
            }
            Err(TryRecvError::Disconnected) => return,
            Err(TryRecvError::Empty) => {}
        }
        match capture(&mut s, live_ms, 1, |_| {}) {
            Ok(e) => send(Msg::Live(e)),
            Err(e) => return send(Msg::Failed(e.to_string())),
        }
    }
}
