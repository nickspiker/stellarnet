//! patchscan — measure a Verichrome target on the StellarNet.
//!
//! A fluor window: the target's layout on the left with the current patch highlighted and every captured patch filled with its measured colour; on the right the step, what to do, the live colour under the fibre beside the captured one, and the spectral readout (reflectance once the dark and the flat exist, raw counts before). Enter captures and moves on; the arrows, the buttons, or a click on the target move between steps. Nothing is written until Save, and Save writes only a complete session: `<encryptcal>/pnx/<serial>.vsf`, one file per target, always replaced, never a second copy (see `session`). Closing is a killswitch: the window goes at once, and whatever was not saved goes with it.
//!
//! Nothing here reads an existing calibration: it is made for new targets.
//!
//! Exposure is a slider in stops — log₂ of the integration in milliseconds, 3 (8 ms) to 12 (4096 ms), in quarter stops — setting the short integration and the live view; the long one follows 2½ stops above it. It locks with the first capture, because the dark and the flats only correct captures made at the same exposure.
//!
//! Usage: patchscan --encryptcal DIR --rs50 FILE [--short MS] [--short-scans N] [--long-scans N]
//! `--encryptcal` names the calibration tree sessions save into, `--rs50` the RS50 standard's certified reflectance (CSV: nm, reflectance). Without them it still measures, but cannot save, or show reflectance.

mod art;
mod device;
mod session;

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};

use fluor::canvas::{Canvas, PixelRect};
use fluor::coord::Coord;
use fluor::event::{CursorIcon, ElementState, Event, Key, KeyEvent, MouseButton, NamedKey};
use fluor::geom::Viewport;
use fluor::host::WakeSender;
use fluor::host::app::{Context, EventResponse, FluorApp};
use fluor::host::chrome::{self, HIT_NONE, HitId, ResizeEdge};
use fluor::host::chrome_widget::DefaultChrome;
use fluor::host::widget::{self as widget, Container, Widget};
use fluor::paint::{fill_rect, pack_argb, stroke_rect};
use fluor::text::TextStyle;
use fluor::widgets::{Button, Slider, Textbox};
use std::time::{Duration, Instant};

use device::{Cmd, Exposure, Msg};
use session::{Role, Step};

const FG: (u8, u8, u8) = (224, 224, 224);
const DIM: (u8, u8, u8) = (128, 128, 128);
const FAINT: (u8, u8, u8) = (56, 60, 68);
const PANEL: (u8, u8, u8) = (18, 20, 26);
const ACCENT: (u8, u8, u8) = (255, 176, 32);
const WARN: (u8, u8, u8) = (255, 96, 96);
const GOOD: (u8, u8, u8) = (96, 200, 120);

fn c((r, g, b): (u8, u8, u8)) -> u32 {
    pack_argb(r, g, b, 255)
}

fn rgb(v: [u8; 3]) -> u32 {
    pack_argb(v[0], v[1], v[2], 255)
}

/// The readout's wavelength axis: the StellarNet's whole useful range.
const NM_LO: f32 = 250.;
const NM_HI: f32 = 1100.;

/// The exposure slider's range and step, in stops (log₂ ms).
const STOP_LO: f32 = 3.;
const STOP_HI: f32 = 12.;
const STOP_STEP: f32 = 0.25;
/// How far above the short integration the long one sits, in stops: the long one is for the UV and IR ends, where the light is weak.
const LONG_STOPS: f32 = 2.5;

const BACK: usize = 0;
const CAPTURE: usize = 1;
const SKIP: usize = 2;
const SAVE: usize = 3;
const LABELS: [&str; 4] = ["◀ Back", "Capture ⏎", "Skip ▶", "Save"];

struct Settings {
    /// The calibration tree sessions save into (`<encryptcal>/pnx/<serial>.vsf`); no default, so nothing is written until one is named.
    encryptcal: Option<String>,
    /// The RS50 standard's certified reflectance (CSV: nm, reflectance); no default, so reflectance waits until it is named.
    rs50: Option<String>,
    short_ms: u32,
    short_scans: u32,
    long_scans: u32,
}

fn settings() -> Settings {
    let mut s = Settings { encryptcal: None, rs50: None, short_ms: 64, short_scans: 32, long_scans: 8 };
    let v: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i + 1 < v.len() {
        let n = &v[i + 1];
        match v[i].as_str() {
            "--encryptcal" => s.encryptcal = Some(n.clone()),
            "--rs50" => s.rs50 = Some(n.clone()),
            "--short" => s.short_ms = n.parse().unwrap_or(s.short_ms),
            "--short-scans" => s.short_scans = n.parse().unwrap_or(s.short_scans),
            "--long-scans" => s.long_scans = n.parse().unwrap_or(s.long_scans),
            _ => {}
        }
        i += 2;
    }
    s
}

fn snap_stops(s: f32) -> f32 {
    ((s.clamp(STOP_LO, STOP_HI) / STOP_STEP).round()) * STOP_STEP
}

/// The target map's geometry this frame: origin and cell size in pixels.
#[derive(Clone, Copy, Default)]
struct MapGeom {
    x: Coord,
    y: Coord,
    cell: Coord,
}

struct App {
    title: String,
    chrome: DefaultChrome,
    hit_counter: HitId,
    buttons: Vec<Button>,
    modifiers: fluor::event::ModifiersState,
    set: Settings,

    proxy: Option<Arc<dyn WakeSender<()>>>,
    cmd_tx: Option<Sender<Cmd>>,
    msg_rx: Option<Receiver<Msg>>,

    nm: Vec<f32>,
    rs: Vec<(f32, f32)>,
    live: Option<Exposure>,
    steps: Vec<Step>,
    cur: usize,
    /// While a capture runs: (integration ms, scans done, scans to do).
    capturing: Option<(u32, u32, u32)>,
    /// The step captured most recently — what the captured swatch and curve show once the session has moved on.
    last_captured: Option<usize>,

    /// The target's serial number: typed here, committed with Enter, locked once the first capture is taken — a session belongs to one target.
    serial_box: Textbox,
    /// Exposure in stops; the slider's position is linear in stops.
    exposure: Slider,
    stops: f32,
    dragging_exposure: bool,
    serial_focused: bool,
    serial: Option<u64>,
    last_blink: Instant,
    started: i64,

    status: String,
    warnings: Vec<String>,
    map: MapGeom,
    art: Option<Art>,
}

/// The target's printed art at this frame's cell size, kept until the size or the serial changes.
struct Art {
    cell: usize,
    serial: Option<u64>,
    /// The roundcode at its frame size, and that size.
    roundcode: Option<(Vec<bool>, usize)>,
    bullseye: (Vec<Option<bool>>, usize),
    logo: (Vec<Option<(f32, f32, f32)>>, usize),
}

impl Art {
    fn new(cell: usize, serial: Option<u64>) -> Art {
        // In the printed cell, the roundcode's frame (code and corner doughnuts) spans 0.078 to 0.912 of the cell — measured off the target's photo.
        let rc = ((cell as f32) * 0.834) as usize;
        let be = ((cell as f32) * 0.9) as usize;
        Art { cell, serial, roundcode: serial.map(|s| (art::roundcode(1, s, rc), rc)), bullseye: (art::bullseye(be), be), logo: (art::logo(cell), cell) }
    }
}

/// Paint `n`×`n` pixels at (x0, y0) wherever `f` gives a colour and nothing in front has painted yet — fluor composites front to back.
fn blit(canvas: &mut Canvas, x0: Coord, y0: Coord, n: usize, f: impl Fn(usize) -> Option<u32>) {
    let (bw, bh) = (canvas.width, canvas.height);
    let (x0, y0) = (x0.max(0.) as usize, y0.max(0.) as usize);
    for h in 0..n {
        for w in 0..n {
            let (x, y) = (x0 + w, y0 + h);
            if x >= bw || y >= bh {
                continue;
            }
            if let Some(c) = f(h * n + w) {
                let p = &mut canvas.pixels[y * bw + x];
                if *p >> 24 == 0 {
                    *p = c;
                }
            }
        }
    }
}

impl App {
    fn new() -> Self {
        let viewport = Viewport::new(1200, 760);
        let mut hit_counter: HitId = HIT_NONE;
        let chrome = DefaultChrome::new(viewport, "patchscan".to_string(), None, Some("Verichrome target on the StellarNet".to_string()), &mut hit_counter);
        let buttons = LABELS.iter().map(|l| Button::new(&mut hit_counter, 0., 0., 1., 1., 12., *l)).collect();
        let serial_box = Textbox::new(&mut hit_counter, 0., 0., 1., 1., 12.);
        let set = settings();
        let stops = snap_stops((set.short_ms.max(1) as f32).log2());
        let exposure = Slider::new(&mut hit_counter, 0., 0., 1., 1., (stops - STOP_LO) / (STOP_HI - STOP_LO));
        let (rs, status) = match set.rs50.as_deref().map(session::rs50) {
            Some(Ok(rs)) => (rs, "Opening the StellarNet…".to_string()),
            Some(Err(e)) => (Vec::new(), format!("No RS50 curve: {e}")),
            None => (Vec::new(), "No RS50 curve: start with --rs50 <its reflectance CSV> for reflectance".to_string()),
        };
        Self {
            title: "patchscan".into(),
            chrome,
            hit_counter,
            buttons,
            modifiers: Default::default(),
            set,
            proxy: None,
            cmd_tx: None,
            msg_rx: None,
            nm: Vec::new(),
            rs,
            live: None,
            steps: session::steps(),
            cur: 0,
            capturing: None,
            last_captured: None,
            serial_box,
            exposure,
            stops,
            dragging_exposure: false,
            serial_focused: false,
            serial: None,
            last_blink: Instant::now(),
            started: vsf::eagle_time_oscillations(),
            status,
            warnings: Vec::new(),
            map: MapGeom::default(),
            art: None,
        }
    }

    fn short_ms(&self) -> u32 {
        (2f32.powf(self.stops)).round() as u32
    }

    fn long_ms(&self) -> u32 {
        (2f32.powf(self.stops + LONG_STOPS)).round().min(65535.) as u32
    }

    /// The slider moved: snap to the step, and run the live view at the new integration.
    fn exposure_changed(&mut self) {
        let stops = snap_stops(STOP_LO + self.exposure.value() * (STOP_HI - STOP_LO));
        if stops != self.stops {
            self.stops = stops;
            if let Some(tx) = &self.cmd_tx {
                let _ = tx.send(Cmd::LiveMs(self.short_ms()));
            }
        }
    }

    fn focus_serial(&mut self, on: bool) {
        let id = self.serial_box.hit_id();
        let on = on && self.serial_box.is_enabled();
        if on != self.serial_focused {
            let (old, new) = if on { (None, Some(id)) } else { (Some(id), None) };
            widget::apply_focus_change(self as &mut dyn Container, old, new);
            self.serial_focused = on;
        }
    }

    fn commit_serial(&mut self) {
        let typed: String = self.serial_box.chars.iter().collect();
        match typed.trim().parse::<u64>() {
            Ok(n) => {
                self.serial = Some(n);
                self.focus_serial(false);
                self.status = match self.path() {
                    Some(path) if std::path::Path::new(&path).exists() => format!("Target {n}. Save will replace {path}."),
                    Some(path) => format!("Target {n}. Save will write {path}."),
                    None => format!("Target {n}. To save, start with --encryptcal <calibration folder>."),
                };
            }
            Err(_) => self.status = "A serial number is a number.".into(),
        }
    }

    fn path(&self) -> Option<String> {
        Some(format!("{}/pnx/{}.vsf", self.set.encryptcal.as_ref()?, self.serial?))
    }

    fn dark(&self) -> Option<&Exposure> {
        self.steps.iter().find(|s| s.role == Role::Dark).and_then(|s| s.short.as_ref())
    }

    /// The flat to divide by for step `i`: the latest one captured before it, else any.
    fn flat_for(&self, i: usize) -> Option<&Exposure> {
        self.steps[..i].iter().rev().chain(self.steps[i..].iter()).find(|s| s.role == Role::Flat && s.short.is_some()).and_then(|s| s.short.as_ref())
    }

    fn reflectance_of(&self, e: &Exposure, i: usize) -> Option<Vec<f32>> {
        Some(session::reflectance(&self.nm, e, self.flat_for(i)?, self.dark()?, &self.rs))
    }

    fn save(&mut self) {
        let Some(path) = self.path() else {
            self.status = if self.set.encryptcal.is_none() { "Nowhere to save: start with --encryptcal <calibration folder>.".into() } else { "Type the target's serial number first.".into() };
            return;
        };
        if self.nm.is_empty() {
            return;
        }
        let missing: Vec<String> = self.steps.iter().filter(|s| !s.captured()).map(|s| s.label()).collect();
        if !missing.is_empty() {
            self.status = format!("Not saved — {} still to capture: {}", missing.len(), missing.join(", "));
            return;
        }
        match session::save(&path, self.serial.unwrap(), self.started, &self.nm, &self.rs, &self.steps) {
            Ok(()) => {
                self.status = format!("Saved all {} steps to {path}", self.steps.len());
            }
            Err(e) => self.status = format!("Could not save: {e}"),
        }
    }

    fn capture(&mut self) {
        if self.serial.is_none() {
            self.status = "Type the target's serial number first, then Enter.".into();
            return;
        }
        if self.capturing.is_some() || self.nm.is_empty() {
            return;
        }
        if let Some(tx) = &self.cmd_tx {
            let (short_ms, long_ms) = (self.short_ms(), self.long_ms());
            let s = &self.set;
            let _ = tx.send(Cmd::Capture { short_ms, short_scans: s.short_scans, long_ms, long_scans: s.long_scans });
            self.capturing = Some((short_ms, 0, s.short_scans));
            self.warnings.clear();
        }
    }

    fn captured(&mut self, short: Exposure, long: Exposure) {
        let i = self.cur;
        self.capturing = None;
        let mut warn = Vec::new();
        if short.saturated > 0 {
            warn.push(format!("Short exposure clipped on {} pixels.", short.saturated));
        }
        self.last_captured = Some(i);
        self.steps[i].short = Some(short);
        self.steps[i].long = Some(long);
        self.steps[i].at = vsf::eagle_time_oscillations();
        // The session now belongs to this target.
        self.focus_serial(false);
        self.serial_box.set_enabled(false);
        self.exposure.set_enabled(false);
        self.dragging_exposure = false;
        self.warnings = warn;
        self.status = match self.steps.iter().filter(|s| !s.captured()).count() {
            0 => "Every step captured — Save writes the file.".into(),
            n => format!("Captured {}. {n} to go; nothing is written until Save.", self.steps[i].label()),
        };
        if self.cur + 1 < self.steps.len() {
            self.cur += 1;
        }
    }

    fn pump(&mut self) -> bool {
        let Some(rx) = &self.msg_rx else { return false };
        let msgs: Vec<Msg> = rx.try_iter().collect();
        let changed = !msgs.is_empty();
        for m in msgs {
            match m {
                Msg::Ready(nm) => {
                    self.status = format!("StellarNet ready: {} pixels, {:.0}–{:.0} nm.", nm.len(), nm[0], nm[nm.len() - 1]);
                    self.nm = nm;
                }
                Msg::Live(e) => self.live = Some(e),
                Msg::Progress { ms, done, of } => self.capturing = Some((ms, done, of)),
                Msg::Captured(s, l) => self.captured(s, l),
                Msg::Failed(e) => {
                    self.capturing = None;
                    self.status = format!("Spectrometer: {e}");
                }
            }
        }
        changed
    }

    fn go(&mut self, i: usize) {
        if self.capturing.is_none() && i < self.steps.len() {
            self.cur = i;
            self.warnings.clear();
        }
    }

    fn key(&mut self, kev: &KeyEvent, ctx: &mut Context) -> EventResponse {
        if kev.state != ElementState::Pressed {
            return EventResponse::Pass;
        }
        if self.serial_focused {
            match &kev.logical_key {
                // One Enter: the serial is set and the step's capture starts.
                Key::Named(NamedKey::Enter) => {
                    self.commit_serial();
                    if self.serial.is_some() && !self.serial_focused {
                        self.capture();
                    }
                }
                Key::Named(NamedKey::Escape) => self.focus_serial(false),
                _ => {
                    let (id, mods) = (self.serial_box.hit_id(), self.modifiers);
                    let r = widget::dispatch_key(self as &mut dyn Container, id, kev, mods, ctx.text);
                    ctx.window.request_redraw();
                    return r;
                }
            }
            ctx.window.request_redraw();
            return EventResponse::Handled;
        }
        match &kev.logical_key {
            Key::Named(NamedKey::Enter) if self.serial.is_none() => self.focus_serial(true),
            Key::Named(NamedKey::Enter) => self.capture(),
            Key::Named(NamedKey::ArrowLeft) => self.go(self.cur.saturating_sub(1)),
            Key::Named(NamedKey::ArrowRight) => self.go(self.cur + 1),
            _ => return EventResponse::Pass,
        }
        ctx.window.request_redraw();
        EventResponse::Handled
    }

    fn take_buttons(&mut self) -> bool {
        let mut any = false;
        for b in 0..self.buttons.len() {
            if self.buttons[b].take_click() {
                any = true;
                match b {
                    BACK => self.go(self.cur.saturating_sub(1)),
                    CAPTURE => self.capture(),
                    SKIP => self.go(self.cur + 1),
                    SAVE => self.save(),
                    _ => {}
                }
            }
        }
        any
    }

    /// The patch (or dark / flat chip) under a click on the target map, as a step.
    fn step_at(&self, x: Coord, y: Coord) -> Option<usize> {
        let m = self.map;
        if m.cell <= 0. {
            return None;
        }
        let (col, row) = (((x - m.x) / m.cell).floor(), ((y - m.y) / m.cell).floor());
        if (0. ..7.).contains(&col) && (0. ..6.).contains(&row) {
            let at = (row as usize, col as usize);
            return session::layout().iter().position(|&p| p == at).map(|p| p + 2);
        }
        // The chips under the map: dark, flat before, flat after.
        let chip_y = m.y + m.cell * 6.25;
        if y >= chip_y && y <= chip_y + m.cell * 0.6 {
            let k = ((x - m.x) / (m.cell * 7. / 3.)).floor();
            return match k as i32 {
                0 => Some(0),
                1 => Some(1),
                2 => Some(self.steps.len() - 1),
                _ => None,
            };
        }
        None
    }

    fn layout_widgets(&mut self, vp: Viewport, text: &mut fluor::text::TextRenderer) {
        let (w, h) = (vp.width_px as Coord, vp.height_px as Coord);
        let span = vp.effective_span();
        let fs = (span / 40.).max(10.);
        let top = h * 0.09;
        let box_w = w * 0.18;
        self.serial_box.set_rect(w * 0.47 + fs * 7. + box_w * 0.5, top + fs * 3.3, box_w, fs * 1.6);
        self.serial_box.set_font_size(fs, text);
        self.exposure.set_rect(w * 0.47 + fs * 7. + box_w * 0.5, top + fs * 5., box_w, fs * 1.1);
        let bw = span / 7.;
        let bh = span / 18.;
        let y = h - bh * 1.1;
        let x0 = w * 0.47 + bw * 0.5;
        for (k, b) in self.buttons.iter_mut().enumerate() {
            b.set_rect(x0 + k as Coord * bw * 1.12, y, bw, bh);
            b.set_font_size(fs);
        }
    }
}

impl Container for App {
    fn visit(&mut self, f: &mut dyn FnMut(&mut dyn Widget)) {
        for b in &mut self.buttons {
            f(b);
        }
        f(&mut self.serial_box);
        f(&mut self.exposure);
        self.chrome.visit(f);
    }
}

/// Draw a polyline of (x, y) pixel points, `t` pixels thick, colour per segment. Each segment is filled column by column, which is all a spectrum needs (its samples are denser than the screen's columns).
fn polyline(target: &mut [u32], w: usize, h: usize, damage: &mut fluor::canvas::Damage, pts: &[(Coord, Coord, u32)], t: Coord) {
    let mut canvas = Canvas::new(target, w, h, damage);
    for p in pts.windows(2) {
        let (x0, y0, col) = p[0];
        let (x1, y1, _) = p[1];
        if !(y0.is_finite() && y1.is_finite()) {
            continue;
        }
        let (xa, xb) = (x0.min(x1), x0.max(x1));
        let (ya, yb) = (y0.min(y1), y0.max(y1));
        fill_rect(&mut canvas, (xa - t * 0.5) as isize, (ya - t * 0.5) as isize, ((xb - xa) + t).max(1.) as isize, ((yb - ya) + t).max(1.) as isize, col, None, None);
    }
}

impl FluorApp for App {
    type UserEvent = ();

    fn title(&self) -> &str {
        &self.title
    }

    fn set_event_proxy(&mut self, proxy: Arc<dyn WakeSender<()>>) {
        self.proxy = Some(proxy);
    }

    fn initial_size(&self, monitor: (u32, u32)) -> (u32, u32) {
        (monitor.0 * 3 / 4, monitor.1 * 3 / 4)
    }

    fn init(&mut self, ctx: &mut Context) {
        self.chrome.resize(ctx.viewport);
        self.layout_widgets(ctx.viewport, ctx.text);
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (msg_tx, msg_rx) = mpsc::channel();
        if let Some(proxy) = self.proxy.clone() {
            std::thread::spawn(move || device::run(cmd_rx, msg_tx, proxy));
        }
        let _ = cmd_tx.send(Cmd::LiveMs(self.short_ms()));
        self.cmd_tx = Some(cmd_tx);
        self.msg_rx = Some(msg_rx);
        self.status = "Type the target's serial number, then Enter.".into();
        self.focus_serial(true);
    }

    fn on_resize(&mut self, _w: u32, _h: u32, ctx: &mut Context) {
        self.chrome.resize(ctx.viewport);
        self.chrome.set_full_edge(ctx.is_maximized);
        self.layout_widgets(ctx.viewport, ctx.text);
    }

    fn tick(&mut self, _ctx: &mut Context) -> bool {
        if self.serial_focused && self.last_blink.elapsed() >= Duration::from_millis(512) {
            self.last_blink = Instant::now();
            return self.serial_box.flip_blinkey();
        }
        false
    }

    fn wake_at(&self) -> Option<Instant> {
        self.serial_focused.then(|| self.last_blink + Duration::from_millis(512))
    }

    fn on_user_event(&mut self, _event: (), ctx: &mut Context) -> EventResponse {
        if self.pump() {
            ctx.window.request_redraw();
        }
        EventResponse::Handled
    }

    fn on_event(&mut self, event: &Event, ctx: &mut Context) -> EventResponse {
        match event {
            Event::ModifiersChanged(m) => {
                self.modifiers = *m;
                EventResponse::Pass
            }
            Event::CursorMoved { .. } if self.dragging_exposure => {
                self.exposure.set_value_from_x(ctx.cursor_x);
                if self.exposure.take_change() {
                    self.exposure_changed();
                    ctx.window.request_redraw();
                }
                EventResponse::Handled
            }
            Event::MouseInput { state: ElementState::Released, .. } if self.dragging_exposure => {
                self.dragging_exposure = false;
                EventResponse::Handled
            }
            Event::CursorMoved { .. } => {
                let hit = self.chrome.hit_at(ctx.cursor_x, ctx.cursor_y);
                if self.chrome.set_hover(hit) {
                    widget::apply_hover(self as &mut dyn Container, hit);
                    ctx.window.request_redraw();
                }
                EventResponse::Pass
            }
            Event::CursorLeft => {
                if self.chrome.set_hover(HIT_NONE) {
                    widget::apply_hover(self as &mut dyn Container, HIT_NONE);
                    ctx.window.request_redraw();
                }
                EventResponse::Pass
            }
            Event::MouseInput { state: ElementState::Pressed, button: MouseButton::Left } => {
                let (x, y) = (ctx.cursor_x, ctx.cursor_y);
                let hit = self.chrome.hit_at(x, y);
                if hit != HIT_NONE {
                    let mods = self.modifiers;
                    let r = widget::dispatch_click(self as &mut dyn Container, hit, x, y, mods);
                    self.take_buttons();
                    if hit == self.exposure.hit_id() && self.exposure.is_enabled() {
                        self.dragging_exposure = true;
                        self.exposure.set_value_from_x(x);
                    }
                    if self.exposure.take_change() {
                        self.exposure_changed();
                    }
                    self.focus_serial(hit == self.serial_box.hit_id());
                    ctx.window.request_redraw();
                    return r;
                }
                self.focus_serial(false);
                if let Some(i) = self.step_at(x, y) {
                    self.go(i);
                    ctx.window.request_redraw();
                    return EventResponse::Handled;
                }
                let edge = chrome::get_resize_edge(ctx.viewport, x, y);
                if edge != ResizeEdge::None {
                    return EventResponse::StartResize(edge);
                }
                EventResponse::StartWindowDrag
            }
            Event::KeyboardInput { event: kev } => self.key(kev, ctx),
            Event::Focused(f) => {
                if self.chrome.set_focused(*f) {
                    ctx.window.request_redraw();
                }
                EventResponse::Pass
            }
            _ => EventResponse::Pass,
        }
    }

    fn damage_rect(&mut self, viewport: Viewport) -> Option<PixelRect> {
        Some(PixelRect::new(0, 0, viewport.width_px as usize, viewport.height_px as usize))
    }

    fn hit_test_map(&mut self) -> Option<(&mut [HitId], usize, usize)> {
        Some(self.chrome.hit_map_mut())
    }

    fn overlay_deltas(&mut self) -> Vec<u32> {
        let count = self.hit_counter as usize + 1;
        widget::build_overlay_deltas(self, count)
    }

    fn render(&mut self, target: &mut [u32], ctx: &mut Context) {
        self.pump();
        let (bw, bh) = (ctx.viewport.width_px as usize, ctx.viewport.height_px as usize);
        // The frame, front to back for pixels and hits alike (fluor's contract, chrome_widget.rs): the host has cleared both over the damage, the window controls are frontmost so they go in first, and the background goes in last, at the end of render.
        widget::apply_pressed(self as &mut dyn Container, ctx.pressed_hit);
        self.chrome.rasterize_bg(ctx.damage, |canvas| canvas.pixels.fill(pack_argb(10, 12, 16, 255)));
        self.chrome.rasterize_perimeter(target, bw, bh, ctx.clip_mask);
        self.chrome.rasterize_chrome(ctx.damage, ctx.text, ctx.clip_mask);
        self.chrome.flatten_chrome_into(target, bw, bh, None);
        let (w, h) = (bw as Coord, bh as Coord);
        let span = ctx.viewport.effective_span();
        let fs = (span / 40.).max(10.);
        let pad = span / 32.;
        let top = h * 0.09;

        // fluor composites front to back: text and curves first, then the shapes under them, then the panels, then the window's own background.
        let text = |t: &mut [u32], ctx: &mut Context, s: &str, x: Coord, y: Coord, size: Coord, col: (u8, u8, u8)| {
            let mut canvas = Canvas::new(t, bw, bh, ctx.damage);
            ctx.text.draw_text_left(&mut canvas, s, x, y, &TextStyle::new(size, c(col)), None, None);
        };

        // ---- right column: step, instruction, serial, status ----
        let rx = w * 0.47;
        let step = &self.steps[self.cur];
        let heading = format!("Step {} of {}: {}", self.cur, self.steps.len() - 1, step.label());
        text(target, ctx, &heading, rx, top, fs * 1.4, FG);
        text(target, ctx, &step.instruction(), rx, top + fs * 1.8, fs, DIM);
        text(target, ctx, "Target serial", rx, top + fs * 3.3, fs, FG);
        let note = if !self.serial_box.is_enabled() { "locked for this session" } else if self.serial.is_some() && !self.serial_focused { "set — click to change" } else { "type it, then Enter" };
        text(target, ctx, note, rx + fs * 7. + w * 0.18 + fs * 0.8, top + fs * 3.3, fs * 0.8, if self.serial_box.is_enabled() { ACCENT } else { DIM });
        {
            let mut canvas = Canvas::new(target, bw, bh, ctx.damage);
            let id = self.serial_box.hit_id();
            self.serial_box.render_content_into(&mut canvas, 0., 0., ctx.text, None, None, Some(&mut self.chrome.hit_test_map), id);
        }
        text(target, ctx, "Exposure", rx, top + fs * 5., fs, FG);
        let frac = self.stops - self.stops.floor();
        let quarter = ["", "¼", "½", "¾"][(frac / STOP_STEP).round() as usize % 4];
        let exposure_note = format!("{}{quarter} stops · {} ms, long {} ms{}", self.stops.floor() as i32, self.short_ms(), self.long_ms(), if self.exposure.is_enabled() { "" } else { " · locked" });
        text(target, ctx, &exposure_note, rx + fs * 7. + w * 0.18 + fs * 0.8, top + fs * 5., fs * 0.8, if self.exposure.is_enabled() { ACCENT } else { DIM });
        {
            let mut canvas = Canvas::new(target, bw, bh, ctx.damage);
            let id = self.exposure.hit_id();
            self.exposure.render_content_into(&mut canvas, Some(&mut self.chrome.hit_test_map), id);
        }
        let status_y = h - span / 18. * 2.3;
        text(target, ctx, &self.status, rx, status_y, fs * 0.85, DIM);
        for (k, wmsg) in self.warnings.iter().enumerate() {
            text(target, ctx, wmsg, rx, status_y - fs * 1.2 * (k + 1) as Coord, fs * 0.85, WARN);
        }
        if let Some((ms, done, of)) = self.capturing {
            text(target, ctx, &format!("Capturing {ms} ms: {done} of {of}"), rx, top + fs * 6.5, fs, ACCENT);
        }

        // ---- swatches: live and captured ----
        let sw_y = top + fs * 7.3;
        let sw_h = fs * 3.;
        let sw_w = w * 0.16;
        let live_refl = self.live.as_ref().and_then(|e| self.reflectance_of(e, self.cur));
        // The step on screen if it has been captured, else the latest capture.
        let shown = if step.captured() { Some(self.cur) } else { self.last_captured };
        let shown_short = shown.and_then(|i| self.steps[i].short.as_ref());
        let cap_refl = shown.zip(shown_short).and_then(|(i, e)| self.reflectance_of(e, i));
        text(target, ctx, "live", rx, sw_y + sw_h + fs * 0.7, fs * 0.8, DIM);
        let cap_label = shown.map(|i| format!("captured: {}", self.steps[i].label())).unwrap_or_else(|| "captured".into());
        text(target, ctx, &cap_label, rx + sw_w * 1.15, sw_y + sw_h + fs * 0.7, fs * 0.8, DIM);
        {
            let mut canvas = Canvas::new(target, bw, bh, ctx.damage);
            let live_col = live_refl.as_ref().map(|r| rgb(session::swatch(&self.nm, r)));
            let cap_col = cap_refl.as_ref().map(|r| rgb(session::swatch(&self.nm, r)));
            for (k, col) in [live_col, cap_col].into_iter().enumerate() {
                let x = (rx + k as Coord * sw_w * 1.15) as isize;
                match col {
                    Some(col) => fill_rect(&mut canvas, x, sw_y as isize, sw_w as isize, sw_h as isize, col, None, None),
                    None => stroke_rect(&mut canvas, x, sw_y as isize, sw_w as isize, sw_h as isize, 1, c(FAINT), None, None),
                }
            }
        }

        // ---- spectral readout ----
        let px = rx;
        let py = sw_y + sw_h + fs * 3.;
        let pw = w - pad - px;
        let ph = (status_y - fs * 1.2 * (self.warnings.len() + 1) as Coord) - py - fs * 1.5;
        let reflect_mode = self.dark().is_some() && self.flat_for(self.cur).is_some() && step.role == Role::Patch;
        let ymax = if reflect_mode { 1.2 } else { 65535. };
        let xof = |l: f32| px + (l - NM_LO) / (NM_HI - NM_LO) * pw;
        let yof = |v: f32| py + ph - (v / ymax).clamp(-0.02, 1.02) * ph;
        let curve = |vals: &[f32], tint: Option<u32>| -> Vec<(Coord, Coord, u32)> {
            self.nm.iter().zip(vals).filter(|(l, _)| (NM_LO..=NM_HI).contains(*l)).map(|(&l, &v)| (xof(l), if v.is_finite() { yof(v) } else { f32::NAN }, tint.unwrap_or_else(|| session::spectral_colour(l).map(rgb).unwrap_or(c(DIM))))).collect()
        };
        let live_vals: Option<Vec<f32>> = if reflect_mode { live_refl.clone() } else { self.live.as_ref().map(|e| e.counts.clone()) };
        let cap_vals: Option<Vec<f32>> = if reflect_mode { cap_refl.clone() } else { shown_short.map(|e| e.counts.clone()) };
        if let Some(v) = &cap_vals {
            polyline(target, bw, bh, ctx.damage, &curve(v, Some(c(FG))), (span / 400.).max(1.));
        }
        if let Some(v) = &live_vals {
            polyline(target, bw, bh, ctx.damage, &curve(v, None), (span / 300.).max(1.5));
        }
        // Axis labels and grid.
        for l in (300..=1100).step_by(100) {
            let x = xof(l as f32);
            let mut canvas = Canvas::new(target, bw, bh, ctx.damage);
            ctx.text.draw_text_center(&mut canvas, &l.to_string(), x, py + ph + fs * 0.8, &TextStyle::new(fs * 0.7, c(DIM)), None, None);
        }
        let ylabels: Vec<(f32, String)> = if reflect_mode { (0..=4).map(|k| (k as f32 * 0.25, format!("{:.2}", k as f32 * 0.25))).collect() } else { (0..=4).map(|k| (k as f32 * 16384., format!("{}", k * 16384))).collect() };
        for (v, s) in &ylabels {
            let mut canvas = Canvas::new(target, bw, bh, ctx.damage);
            ctx.text.draw_text_right(&mut canvas, s, px - fs * 0.3, yof(*v), &TextStyle::new(fs * 0.7, c(DIM)), None, None);
        }
        text(target, ctx, if reflect_mode { "reflectance (nm)" } else { "raw counts (nm)" }, px, py - fs * 0.6, fs * 0.75, DIM);
        {
            let mut canvas = Canvas::new(target, bw, bh, ctx.damage);
            for l in (300..=1100).step_by(100) {
                fill_rect(&mut canvas, xof(l as f32) as isize, py as isize, 1, ph as isize, c(FAINT), None, None);
            }
            for (v, _) in &ylabels {
                fill_rect(&mut canvas, px as isize, yof(*v) as isize, pw as isize, 1, c(FAINT), None, None);
            }
            fill_rect(&mut canvas, px as isize, py as isize, pw as isize, ph as isize, c(PANEL), None, None);
        }

        // ---- buttons ----
        for b in 0..self.buttons.len() {
            let mut canvas = Canvas::new(target, bw, bh, ctx.damage);
            let id = self.buttons[b].hit_id();
            self.buttons[b].render_content_into(&mut canvas, 0., 0., ctx.text, None, Some(&mut self.chrome.hit_test_map), id);
        }

        // ---- target map ----
        let mw = w * 0.42 - pad;
        let mh = h - top - pad - span / 10.;
        let cell = (mw / 7.).min(mh / 6.8);
        let (mx, my) = (pad, top);
        self.map = MapGeom { x: mx, y: my, cell };
        if self.art.as_ref().map(|a| (a.cell, a.serial)) != Some((cell as usize, self.serial)) {
            self.art = Some(Art::new(cell as usize, self.serial));
        }
        let layout = session::layout();
        let inset = cell * 0.08;
        let colour_of = |i: usize| -> Option<u32> { let s = &self.steps[i]; s.short.as_ref().and_then(|e| self.reflectance_of(e, i)).map(|r| rgb(session::swatch(&self.nm, &r))) };
        // Labels first (front), then highlight, then fills and outlines.
        for (p, &(row, col)) in layout.iter().enumerate() {
            let (cx, cy) = (mx + (col as Coord + 0.5) * cell, my + (row as Coord + 0.5) * cell);
            let name = if p >= 21 { session::INDICATORS[p - 21].to_string() } else { p.to_string() };
            let size = if p >= 21 { fs * 0.55 } else { fs * 0.8 };
            let mut canvas = Canvas::new(target, bw, bh, ctx.damage);
            ctx.text.draw_text_center(&mut canvas, &name, cx, cy, &TextStyle::new(size, c(if self.steps[p + 2].captured() { FG } else { DIM })), None, None);
        }
        let chips = [(0usize, "dark"), (1, "flat before"), (self.steps.len() - 1, "flat after")];
        let chip_y = my + cell * 6.25;
        let chip_w = cell * 7. / 3.;
        for (k, (i, name)) in chips.iter().enumerate() {
            let mut canvas = Canvas::new(target, bw, bh, ctx.damage);
            ctx.text.draw_text_center(&mut canvas, name, mx + (k as Coord + 0.5) * chip_w, chip_y + cell * 0.3, &TextStyle::new(fs * 0.7, c(if self.steps[*i].captured() { FG } else { DIM })), None, None);
        }
        {
            let mut canvas = Canvas::new(target, bw, bh, ctx.damage);
            let thick = (cell / 14.).max(2.) as isize;
            // The current step, ringed.
            let ring = |canvas: &mut Canvas, x: Coord, y: Coord, sw: Coord, sh: Coord| stroke_rect(canvas, (x - thick as Coord) as isize, (y - thick as Coord) as isize, (sw + 2. * thick as Coord) as isize, (sh + 2. * thick as Coord) as isize, thick, c(ACCENT), None, None);
            if self.cur >= 2 && self.cur < self.steps.len() - 1 {
                let (row, col) = layout[self.cur - 2];
                ring(&mut canvas, mx + col as Coord * cell + inset, my + row as Coord * cell + inset, cell - 2. * inset, cell - 2. * inset);
            } else if let Some(k) = chips.iter().position(|(i, _)| *i == self.cur) {
                ring(&mut canvas, mx + k as Coord * chip_w + inset, chip_y, chip_w - 2. * inset, cell * 0.6);
            }
            // Patches: measured colour once captured, an outline before.
            for (p, &(row, col)) in layout.iter().enumerate() {
                let (x, y, s) = ((mx + col as Coord * cell + inset) as isize, (my + row as Coord * cell + inset) as isize, (cell - 2. * inset) as isize);
                match colour_of(p + 2) {
                    Some(col) => fill_rect(&mut canvas, x, y, s, s, col, None, None),
                    None => stroke_rect(&mut canvas, x, y, s, s, 1, c(DIM), None, None),
                }
            }
            for (k, (i, _)) in chips.iter().enumerate() {
                let (x, y) = ((mx + k as Coord * chip_w + inset) as isize, chip_y as isize);
                if self.steps[*i].captured() {
                    fill_rect(&mut canvas, x, y, (chip_w - 2. * inset) as isize, (cell * 0.6) as isize, c((40, 70, 50)), None, None);
                }
                stroke_rect(&mut canvas, x, y, (chip_w - 2. * inset) as isize, (cell * 0.6) as isize, 1, c(if self.steps[*i].captured() { GOOD } else { DIM }), None, None);
            }
            // The cells that are not measured — reference squares, the dot code, the bottom greys — faintly, and the four bullseyes.
            for row in 0..6 {
                for col in 0..7 {
                    if layout.contains(&(row, col)) {
                        continue;
                    }
                    let (x, y) = (mx + col as Coord * cell, my + row as Coord * cell);
                    let corner = (row == 0 || row == 5) && (col == 0 || col == 6);
                    let (ink, paper) = (pack_argb(232, 232, 232, 255), pack_argb(6, 6, 6, 255));
                    let art = self.art.as_ref().unwrap();
                    if corner {
                        let (img, n) = &art.bullseye;
                        let off = (cell - *n as Coord) * 0.5;
                        blit(&mut canvas, x + off, y + off, *n, |i| img[i].map(|white| if white { ink } else { paper }));
                    } else if (row, col) == (2, 0) {
                        let (img, n) = &art.logo;
                        blit(&mut canvas, x, y, *n, |i| {
                            img[i].map(|(text, rail, u)| {
                                let spec = session::spectral_colour(390. + u * 340.).unwrap_or([128, 128, 128]);
                                let ch = |k: usize| (text * 232. + rail * spec[k] as f32).min(255.) as u8;
                                pack_argb(ch(0), ch(1), ch(2), 255)
                            })
                        });
                    } else if let ((3, 5), Some((img, n))) = ((row, col), &art.roundcode) {
                        let off = cell * 0.078;
                        blit(&mut canvas, x + off, y + off, *n, |i| Some(if img[i] { ink } else { paper }));
                        fill_rect(&mut canvas, x as isize, y as isize, cell as isize, cell as isize, paper, None, None);
                    } else {
                        stroke_rect(&mut canvas, (x + inset) as isize, (y + inset) as isize, (cell - 2. * inset) as isize, (cell - 2. * inset) as isize, 1, c(FAINT), None, None);
                    }
                }
            }
            fill_rect(&mut canvas, mx as isize, my as isize, (cell * 7.) as isize, (cell * 6.) as isize, c(PANEL), None, None);
        }

        // ---- the window's background, behind everything ----
        self.chrome.flatten_bg_into(target, bw, bh, None);
    }

    fn cursor_for(&self, x: Coord, y: Coord, ctx: &Context) -> CursorIcon {
        let hit = self.chrome.hit_at(x, y);
        if hit != HIT_NONE || self.step_at(x, y).is_some() {
            return CursorIcon::Pointer;
        }
        match chrome::get_resize_edge(ctx.viewport, x, y) {
            ResizeEdge::Top | ResizeEdge::Bottom => CursorIcon::NsResize,
            ResizeEdge::Left | ResizeEdge::Right => CursorIcon::EwResize,
            ResizeEdge::TopLeft | ResizeEdge::BottomRight => CursorIcon::NwseResize,
            ResizeEdge::TopRight | ResizeEdge::BottomLeft => CursorIcon::NeswResize,
            ResizeEdge::None => CursorIcon::Default,
        }
    }
}

fn main() {
    fluor::host::app::run_app(App::new()).expect("event loop");
}
