//! The spectrometer, on its own thread: frames at the integration the UI asks for, averaged in blocks of the count it asks for, each block sent as it completes. This thread is the only thing that talks to the instrument.
//!
//! No wait outlasts a command: an exposure under way is walked away from the moment the UI asks for anything (the driver's `read_while`), so a 16 s exposure turned down to 64 ms answers at once rather than when the 16 s run out.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use fluor::host::WakeSender;
use stellarnet::{FULL_SCALE, MASK_MARK, PIXELS, Spectrometer};
use stellarnet_view::Frame;

/// What the UI asks for.
pub enum Cmd {
    /// Integration per frame, ms.
    Integration(u32),
    /// Frames averaged into each block.
    Average(u32),
}

/// Where a block that is long enough to wait on has got to.
#[derive(Clone, Copy)]
pub struct Progress {
    /// Frames of the block done, and to do.
    pub done: u32,
    pub of: u32,
    /// Into the read under way, and how long it takes (two integrations when a change of setting throws a frame away first).
    pub elapsed: Duration,
    pub read: Duration,
}

/// What the thread reports.
pub enum Msg {
    /// Once, when the instrument opens: the wavelength of every pixel, and who it is.
    Ready { nm: Vec<f32>, model: String, serial: String },
    Progress(Progress),
    Frame(Frame),
    Failed(String),
}

/// A block at least this long reports its progress.
const LONG_BLOCK_MS: u64 = 1 << 9;
/// And this often.
const PROGRESS_EVERY: Duration = Duration::from_millis(1 << 7);

pub fn run(cmds: Receiver<Cmd>, out: Sender<Msg>, wake: Arc<dyn WakeSender<()>>, mut ms: u32, mut scans: u32) {
    let send = |m: Msg| {
        let _ = out.send(m);
        let _ = wake.send(());
    };
    let mut s = match Spectrometer::open() {
        Ok(s) => s,
        Err(e) => return send(Msg::Failed(e.to_string())),
    };
    send(Msg::Ready { nm: s.wavelengths().iter().map(|&nm| nm as f32).collect(), model: s.model.clone(), serial: s.serial.clone() });
    let mut sum = vec![0u64; PIXELS];
    let mut clipped = vec![false; PIXELS];
    let mut done = 0;
    // A command that arrived during an exposure, which it ended.
    let mut arrived: Option<Cmd> = None;
    let mut gone = false;
    loop {
        // Anything the UI changed starts the block over at the new setting.
        loop {
            let cmd = match arrived.take() {
                Some(c) => c,
                None => match cmds.try_recv() {
                    Ok(c) => c,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return,
                },
            };
            match cmd {
                Cmd::Integration(v) => ms = v,
                Cmd::Average(v) => scans = v,
            }
            done = 0;
        }
        if gone {
            return;
        }
        if ms != s.integration_ms()
            && let Err(e) = s.set_integration(ms)
        {
            return send(Msg::Failed(e.to_string()));
        }
        if done == 0 {
            sum.fill(0);
            clipped.fill(false);
        }
        let long = ms as u64 * scans as u64 >= LONG_BLOCK_MS;
        let read = Duration::from_millis(ms as u64) * if s.discard_pending() { 2 } else { 1 };
        let started = Instant::now();
        let mut told = started;
        let go_on = || {
            match cmds.try_recv() {
                Ok(c) => {
                    arrived = Some(c);
                    return false;
                }
                Err(TryRecvError::Disconnected) => {
                    gone = true;
                    return false;
                }
                Err(TryRecvError::Empty) => {}
            }
            if long && told.elapsed() >= PROGRESS_EVERY {
                told = Instant::now();
                send(Msg::Progress(Progress { done, of: scans, elapsed: started.elapsed(), read }));
            }
            true
        };
        let frame = match s.read_while(go_on) {
            Ok(Some(f)) => f,
            Ok(None) => continue,
            Err(e) => return send(Msg::Failed(e.to_string())),
        };
        for p in 0..PIXELS {
            sum[p] += frame[p] as u64;
            clipped[p] |= frame[p] == FULL_SCALE;
        }
        done += 1;
        if done == scans {
            let counts = sum.iter().map(|&t| (t as f64 / scans as f64) as f32).collect();
            // The baseline, from the masked pixels at the head of the readout, before the window drops them.
            let black = (sum[..MASK_MARK].iter().sum::<u64>() as f64 / (MASK_MARK as u64 * scans as u64) as f64) as f32;
            send(Msg::Frame(Frame { ms, scans, counts, clipped: clipped.clone(), black: Some(black) }));
            done = 0;
        } else if long {
            send(Msg::Progress(Progress { done, of: scans, elapsed: Duration::ZERO, read }));
        }
    }
}
