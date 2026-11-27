//! StellarNet — the live spectrum from the StellarNet spectrometer, in a fluor window.
//!
//! The plot takes most of the window: the spectrum over wavelength, filled beneath with the colour each wavelength is (a dim neutral in the UV and IR), pixels that clipped flagged red along the top, the colour of every wavelength in a strip under the axis. The wheel zooms about the cursor, a drag pans, Home or a right click shows the instrument's whole range; hovering reads the value under the cursor.
//!
//! The panel at the right: the colour of what is measured, exposure (quarter stops, 1 ms to 16 s) and the frames averaged into each update (1 to 256), the quantity shown, the scale, the dark and reference captures, hold, auto exposure, and where the light peaks and centres, by power and by photon count.
//!
//! Quantities: Counts is raw (less the dark, once one is taken at this exposure); Ratio is (sample − dark) ÷ (reference − dark), transmittance or reflectance against whatever the reference saw; Emission is that ratio times the StellarNet SL1 lamp's spectrum, the light's relative spectral power when the reference was the SL1, normalized to its peak. A dark or reference applies only at the exposure it was taken at.
//!
//! Saving: name a collection and a sample, then Save (or Enter in the sample's box, or Ctrl+S). Each save adds the spectrum on screen (the held one while holding), with the dark and reference that apply at its exposure, to `<dir>/<collection>.vsf` (see `collection`), `<dir>` the folder the tool was started from unless `--dir DIR` names another. `stellarnet-csv` turns a collection into CSV.
//!
//! The wavelength range: typed into the panel's Range boxes (Enter on the maximum, or clicking away, keeps it), kept in `<dir>/.stellarnet.vsf` (see `settings`), the range the plot opens on and Home or a right click returns to; the instrument's whole range until one is kept.
//!
//! Keys: Space hold, D dark, R reference, A auto exposure, L linear/log, 1 2 3 the quantity, Home the kept range, Shift+Home the instrument's whole range, Ctrl+S save. While a box is being typed in, keys are its: Tab between the two of a pair, Escape out (the range's typing undone).

mod device;
mod plot;

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use fluor::canvas::{Canvas, PixelRect};
use fluor::coord::Coord;
use fluor::event::{CursorIcon, ElementState, Event, Key, KeyEvent, MouseButton, MouseScrollDelta, NamedKey};
use fluor::geom::Viewport;
use fluor::host::WakeSender;
use fluor::host::app::{Context, EventResponse, FluorApp};
use fluor::host::chrome::{self, HIT_NONE, HitId, ResizeEdge};
use fluor::host::chrome_widget::DefaultChrome;
use fluor::host::widget::{self as widget, Container, Widget};
use fluor::paint::{fill_rect, pack_argb};
use fluor::text::TextStyle;
use fluor::widgets::{Button, Choice, Slider, Textbox};

use device::{Cmd, Msg, Progress};
use plot::Plot;
use stellarnet::{FULL_SCALE, MASKED};
use stellarnet_view::collection::{self, Spectrum};
use stellarnet_view::quantity::{REF_FLOOR, compute};
use stellarnet_view::settings::{self, Settings};
use stellarnet_view::{Frame, Quantity, colour};

const FG: (u8, u8, u8) = (226, 228, 232);
const DIM: (u8, u8, u8) = (134, 138, 146);
const GRID: (u8, u8, u8) = (44, 48, 58);
const GRID_MINOR: (u8, u8, u8) = (24, 27, 33);
const PANEL: (u8, u8, u8) = (12, 14, 18);
const BG: (u8, u8, u8) = (7, 8, 11);
const ACCENT: (u8, u8, u8) = (255, 176, 32);
const WARN: (u8, u8, u8) = (255, 96, 96);
const GOOD: (u8, u8, u8) = (96, 200, 120);
const LINE: [u8; 3] = [246, 246, 242];

fn c((r, g, b): (u8, u8, u8)) -> u32 {
    pack_argb(r, g, b, 255)
}

/// Exposure slider: log₂ ms, 0 (1 ms) to 14 (16384 ms), in quarter stops.
const STOPS: f32 = 14.;
const STOP_STEP: f32 = 0.25;
/// The caret's blink, each way.
const BLINK: Duration = Duration::from_millis(1 << 9);
/// Averaging slider: log₂ frames, 0 (1) to 8 (256).
const AVG_STOPS: f32 = 8.;
/// Auto exposure aims the brightest pixel here.
const AUTO_TARGET: f32 = (FULL_SCALE as f32) * 0.75;
const SCALES: [&str; 2] = ["Linear", "Log"];

#[derive(Clone, Copy, PartialEq)]
enum Target {
    Dark,
    Reference,
}

/// Where `layout` put the panel's rows, for `render` to draw at.
#[derive(Clone, Copy, Default)]
struct Panel {
    swatch_y: Coord,
    swatch_h: Coord,
    caption_y: Coord,
    exposure_y: Coord,
    row: Coord,
    readout_y: Coord,
    range_y: Coord,
    /// Width of each range box: the dash and "nm" sit by it.
    range_box_w: Coord,
    collection_y: Coord,
    sample_y: Coord,
}

/// The boxes that take typing: the two names for a save, and the ends of the saved wavelength range.
#[derive(Clone, Copy, PartialEq)]
enum Field {
    Collection,
    Sample,
    Min,
    Max,
}

enum Drag {
    None,
    Exposure,
    Average,
    /// Panning the plot: where the drag started and the view then.
    Pan { x: f32, view: (f32, f32) },
}

/// The y axis: linear in the value, or in its log₂, between `lo` and `hi` (in those units), with the gridlines to label.
struct Axis {
    log: bool,
    lo: f32,
    hi: f32,
    ticks: Vec<(f32, String)>,
    /// Fainter lines between the labelled ones.
    minor: Vec<f32>,
}

impl Axis {
    /// Labelled in percent: Ratio and Emission as their own percent, Counts as percent of full scale (the counts themselves are in the hover).
    fn new(q: Quantity, log: bool) -> Axis {
        // A log tick 2^e, as a percent of 1 (Ratio, Emission) or of 2^16 (Counts).
        let pow = |e: i32, of: i32| pct(((e - of) as f32).exp2());
        let stops = |lo: i32, hi: i32| (lo..=hi).map(|k| k as f32).collect::<Vec<f32>>();
        match (q, log) {
            (Quantity::Counts, false) => Axis { log, lo: 0., hi: FULL_SCALE as f32 + 1., ticks: (0..=4).map(|k| ((k << 14) as f32, pct(k as f32 * 0.25))).collect(), minor: (0..=16).map(|k| (k << 12) as f32).collect() },
            (Quantity::Counts, true) => Axis { log, lo: 4., hi: 16., ticks: (2..=8).map(|k| ((2 * k) as f32, pow(2 * k, 16))).collect(), minor: stops(4, 16) },
            (_, false) => Axis { log, lo: 0., hi: 1.25, ticks: (0..=12).map(|k| (k as f32 * 0.1, pct(k as f32 * 0.1))).collect(), minor: Vec::new() },
            (Quantity::Ratio, true) => Axis { log, lo: -8., hi: 1., ticks: (-4..=0).map(|k| ((2 * k) as f32, pow(2 * k, 0))).collect(), minor: stops(-8, 1) },
            (Quantity::Emission, true) => Axis { log, lo: -12., hi: 1., ticks: (-6..=0).map(|k| ((2 * k) as f32, pow(2 * k, 0))).collect(), minor: stops(-12, 1) },
        }
    }

    /// Plot row (0 at the top, `h` at the bottom) of a value. Zero on a log axis lands at +∞ and a negative one at NaN: no fill and a gap in the line, which is what they are.
    fn row(&self, v: f32, h: usize) -> f32 {
        let x = if self.log { v.log2() } else { v };
        (1. - (x - self.lo) / (self.hi - self.lo)) * h as f32
    }

    /// Row of a tick, given in the axis's own units.
    fn tick_row(&self, t: f32, h: usize) -> f32 {
        (1. - (t - self.lo) / (self.hi - self.lo)) * h as f32
    }
}

/// Where the light sits: peak and centroid by power, and by photon count (power × λ).
struct Stats {
    peak_power: f32,
    peak_quanta: f32,
    centre_power: Option<f32>,
    centre_quanta: Option<f32>,
}

fn stats(nm: &[f32], v: &[f32], view: (f32, f32)) -> Option<Stats> {
    let (mut pp, mut pq) = ((f32::NEG_INFINITY, 0.), (f32::NEG_INFINITY, 0.));
    let (mut sp, mut sq, mut lp, mut lq) = (0f64, 0f64, 0f64, 0f64);
    for (&l, &x) in nm.iter().zip(v) {
        if !(x.is_finite() && (view.0..=view.1).contains(&l)) {
            continue;
        }
        let q = x * l;
        if x > pp.0 {
            pp = (x, l);
        }
        if q > pq.0 {
            pq = (q, l);
        }
        // Less the dark, an unlit pixel reads noise either side of zero. A centroid is a mean over a non-negative weight: signed noise in the sums can carry it anywhere, outside the view (1425 nm over 500–720 in a test). Light below zero is no light, so it weighs nothing.
        let w = x.max(0.);
        sp += w as f64;
        lp += (l * w) as f64;
        sq += (w * l) as f64;
        lq += (l * w * l) as f64;
    }
    if pp.0 == f32::NEG_INFINITY {
        return None;
    }
    Some(Stats { peak_power: pp.1, peak_quanta: pq.1, centre_power: (sp > 0.).then(|| (lp / sp) as f32), centre_quanta: (sq > 0.).then(|| (lq / sq) as f32) })
}

struct App {
    title: String,
    chrome: DefaultChrome,
    hit_counter: HitId,
    modifiers: fluor::event::ModifiersState,

    proxy: Option<Arc<dyn WakeSender<()>>>,
    cmd_tx: Option<Sender<Cmd>>,
    msg_rx: Option<Receiver<Msg>>,

    /// Wavelength of every spectral pixel (the masked ones dropped), once the instrument is open.
    nm: Vec<f32>,
    instrument: String,
    live: Option<Frame>,
    /// The frame Hold froze, while it is held.
    held: Option<Frame>,
    dark: Option<Frame>,
    reference: Option<Frame>,
    /// The widget under the cursor: hovering Dark or Reference shows that capture on the plot.
    hovered: HitId,
    progress: Option<Progress>,
    failed: bool,

    exposure: Slider,
    average: Slider,
    quantity: Choice,
    scale: Choice,
    dark_btn: Button,
    ref_btn: Button,
    auto_btn: Button,
    hold_btn: Button,
    stops: f32,
    avg_stops: u32,

    /// Saving: where collections live, the collection and sample names as typed, and which box is being typed in.
    dir: PathBuf,
    model: String,
    serial: String,
    collection_box: Textbox,
    sample_box: Textbox,
    save_btn: Button,
    editing: Option<Field>,
    last_blink: Instant,
    /// What the last save (or a name that cannot be one) said, and in what colour.
    note: Option<(String, (u8, u8, u8))>,
    /// The wavelength range the plot opens on and Home returns to, kept in `settings` beside the collections; None until one is set (the instrument's whole range then).
    range: Option<Settings>,
    min_box: Textbox,
    max_box: Textbox,
    /// The range boxes want their text set from `range` (or the instrument's) at the next render, which holds the text renderer.
    refill_range: bool,
    /// What the range boxes were last filled with: unchanged, leaving them keeps nothing.
    range_shown: (String, String),

    drag: Drag,
    /// The wavelength span the plot shows.
    view: (f32, f32),
    plot: Plot,
    panel: Panel,
    cursor: Option<(f32, f32)>,
}

impl App {
    fn new() -> Self {
        let viewport = Viewport::new(1400, 820);
        let mut hit_counter: HitId = HIT_NONE;
        let chrome = DefaultChrome::new(viewport, "StellarNet".to_string(), None, Some("Opening the spectrometer…".to_string()), &mut hit_counter);
        let stops = 6.;
        let avg_stops = 2;
        let exposure = Slider::new(&mut hit_counter, 0., 0., 1., 1., stops / STOPS);
        let average = Slider::new(&mut hit_counter, 0., 0., 1., 1., avg_stops as f32 / AVG_STOPS);
        let quantity = Choice::new(&mut hit_counter, &Quantity::ALL.map(Quantity::name), 0, 12.);
        let scale = Choice::new(&mut hit_counter, &SCALES, 0, 12.);
        let button = |h: &mut HitId, l: &str| Button::new(h, 0., 0., 1., 1., 12., l);
        let dark_btn = button(&mut hit_counter, "Dark");
        let ref_btn = button(&mut hit_counter, "Reference");
        let auto_btn = button(&mut hit_counter, "Auto exposure");
        let hold_btn = button(&mut hit_counter, "Hold");
        let save_btn = button(&mut hit_counter, "Save");
        let collection_box = Textbox::new(&mut hit_counter, 0., 0., 1., 1., 12.);
        let sample_box = Textbox::new(&mut hit_counter, 0., 0., 1., 1., 12.);
        let args: Vec<String> = std::env::args().collect();
        // Where collections and the kept range live: the folder the tool was started from, or `--dir DIR`.
        let dir = args.iter().position(|a| a == "--dir").and_then(|i| args.get(i + 1)).map_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")), PathBuf::from);
        let min_box = Textbox::new(&mut hit_counter, 0., 0., 1., 1., 12.);
        let max_box = Textbox::new(&mut hit_counter, 0., 0., 1., 1., 12.);
        let (range, note) = match settings::load(&dir) {
            Ok(r) => (r, None),
            Err(e) => (None, Some((e, WARN))),
        };
        let view = range.map_or((200., 1100.), |r| (r.nm_min, r.nm_max));
        Self {
            title: "StellarNet".into(),
            chrome,
            hit_counter,
            modifiers: Default::default(),
            proxy: None,
            cmd_tx: None,
            msg_rx: None,
            nm: Vec::new(),
            instrument: String::new(),
            live: None,
            held: None,
            dark: None,
            reference: None,
            hovered: HIT_NONE,
            progress: None,
            failed: false,
            exposure,
            average,
            quantity,
            scale,
            dark_btn,
            ref_btn,
            auto_btn,
            hold_btn,
            stops,
            avg_stops,
            dir,
            model: String::new(),
            serial: String::new(),
            collection_box,
            sample_box,
            save_btn,
            editing: None,
            last_blink: Instant::now(),
            note,
            range,
            min_box,
            max_box,
            refill_range: true,
            range_shown: (String::new(), String::new()),
            drag: Drag::None,
            view,
            plot: Plot { x: 0, y: 0, w: 1, h: 1, nm_lo: 200., nm_hi: 1100. },
            panel: Panel::default(),
            cursor: None,
        }
    }

    fn ms(&self) -> u32 {
        self.stops.exp2().round() as u32
    }

    fn scans(&self) -> u32 {
        1 << self.avg_stops
    }

    fn send(&self, cmd: Cmd) {
        if let Some(tx) = &self.cmd_tx {
            let _ = tx.send(cmd);
        }
    }

    fn quantity_kind(&self) -> Quantity {
        Quantity::ALL[self.quantity.selected()]
    }

    /// The instrument's whole range.
    fn full_view(&self) -> (f32, f32) {
        if self.nm.is_empty() { (200., 1100.) } else { (self.nm[0], self.nm[self.nm.len() - 1]) }
    }

    /// Where the plot opens and Home returns: the saved range, else the instrument's whole one.
    fn home_view(&self) -> (f32, f32) {
        self.range.map_or_else(|| self.full_view(), |r| (r.nm_min, r.nm_max))
    }

    /// Put the kept range (the instrument's while none is kept) into the range boxes, and remember what went in, so leaving them untouched keeps nothing.
    fn fill_range(&mut self, text: &mut fluor::text::TextRenderer) {
        let (lo, hi) = self.home_view();
        // A tenth of a nm, without trailing zeros: 380, 380.5, 233.9.
        let show = |v: f32| format!("{}", (v * 10.).round() / 10.);
        self.range_shown = (show(lo), show(hi));
        self.min_box.set_text(&self.range_shown.0, text);
        self.max_box.set_text(&self.range_shown.1, text);
        self.refill_range = false;
    }

    /// The typed range, checked and kept: numbers, min below max. A range that is not one says so and the boxes go back to the kept range. Boxes left as they were filled keep nothing.
    fn commit_range(&mut self) {
        if (typed(&self.min_box), typed(&self.max_box)) == self.range_shown {
            return;
        }
        let parsed = (typed(&self.min_box).parse::<f32>(), typed(&self.max_box).parse::<f32>());
        let (Ok(nm_min), Ok(nm_max)) = parsed else {
            self.note = Some(("The range is two numbers, in nm".into(), WARN));
            self.refill_range = true;
            return;
        };
        if !(nm_min.is_finite() && nm_max.is_finite() && nm_min < nm_max) {
            self.note = Some(("The range's minimum must be below its maximum".into(), WARN));
            self.refill_range = true;
            return;
        }
        let r = Settings { nm_min, nm_max };
        self.refill_range = true;
        if self.range == Some(r) {
            return;
        }
        self.range = Some(r);
        self.view = (nm_min, nm_max);
        self.note = Some(match settings::save(&self.dir, &r) {
            Ok(()) => (format!("Range {nm_min}–{nm_max} nm saved"), GOOD),
            Err(e) => (e, WARN),
        });
    }

    /// A capture taken at the exposure of `f`, if it was.
    fn at<'a>(&self, cap: &'a Option<Frame>, f: &Frame) -> Option<&'a Frame> {
        cap.as_ref().filter(|d| d.ms == f.ms)
    }

    /// The frame on screen: the held one while holding, else the latest.
    fn shown(&self) -> Option<&Frame> {
        self.held.as_ref().or(self.live.as_ref())
    }

    /// The quantity to plot from the frame on screen, or why there is none.
    fn values(&self) -> Result<Vec<f32>, String> {
        let f = self.shown().ok_or_else(|| "waiting for the first frame".to_string())?;
        let q = self.quantity_kind();
        let (dark, reference) = (self.at(&self.dark, f), self.at(&self.reference, f));
        let v = compute(q, &self.nm, f, dark, reference).ok_or_else(|| format!("{} needs a dark and a reference taken at {} ms.\nBlock the light into the fibre and press Dark;\nthen show it the reference (white standard or lamp) and press Reference.", q.name(), f.ms))?;
        // A ratio with nothing to divide by anywhere is an empty plot that says nothing: say why instead.
        if q != Quantity::Counts
            && !v.iter().any(|x| x.is_finite())
            && let (Some(d), Some(r)) = (dark, reference)
        {
            let lit = r.counts.iter().zip(&d.counts).map(|(r, d)| r - d).fold(f32::NEG_INFINITY, f32::max);
            return Err(format!(
                "The reference is only {lit:.0} counts over the dark at its brightest; a ratio needs {REF_FLOOR:.0}.\nLengthen the exposure or brighten the reference, then take the dark (light blocked) and the reference again."
            ));
        }
        Ok(v)
    }

    fn pump(&mut self) -> bool {
        let Some(rx) = &self.msg_rx else { return false };
        let msgs: Vec<Msg> = rx.try_iter().collect();
        let changed = !msgs.is_empty();
        for m in msgs {
            match m {
                Msg::Ready { nm, model, serial } => {
                    self.nm = nm[MASKED..].to_vec();
                    self.instrument = format!("{model} #{serial}");
                    (self.model, self.serial) = (model, serial);
                    self.chrome.set_title(format!("StellarNet · {}", self.instrument));
                    self.view = self.home_view();
                    // With no saved range the boxes show the instrument's, now that it is known.
                    self.refill_range = true;
                }
                Msg::Progress(p) => self.progress = Some(p),
                Msg::Frame(mut f) => {
                    f.counts.drain(..MASKED);
                    f.clipped.drain(..MASKED);
                    self.progress = None;
                    self.live = Some(f);
                }
                Msg::Failed(e) => {
                    self.failed = true;
                    self.chrome.set_status_text(Some(format!("Spectrometer: {e}")));
                }
            }
        }
        changed
    }

    /// Keep the frame on screen (the held one while holding) as the dark or the reference, as it is: what you saw is what calibrates, at once, as Save keeps it. Until 2026-11-27 a capture threw the block under way away and averaged a fresh one nobody had seen.
    fn capture(&mut self, t: Target) {
        let Some(f) = self.shown().cloned() else {
            self.note = Some(("Nothing on screen to keep yet".into(), WARN));
            return;
        };
        let what = if t == Target::Dark { "Dark" } else { "Reference" };
        self.note = Some((format!("{what}: the frame on screen, {} ms × {}, kept. Hover {what} to see it.", f.ms, f.scans), GOOD));
        match t {
            Target::Dark => self.dark = Some(f),
            Target::Reference => self.reference = Some(f),
        }
    }

    fn toggle_hold(&mut self) {
        self.held = match self.held {
            Some(_) => None,
            None => self.live.clone(),
        };
    }

    fn field_id(&self, f: Field) -> HitId {
        match f {
            Field::Collection => self.collection_box.hit_id(),
            Field::Sample => self.sample_box.hit_id(),
            Field::Min => self.min_box.hit_id(),
            Field::Max => self.max_box.hit_id(),
        }
    }

    fn field_box(&mut self, f: Field) -> &mut Textbox {
        match f {
            Field::Collection => &mut self.collection_box,
            Field::Sample => &mut self.sample_box,
            Field::Min => &mut self.min_box,
            Field::Max => &mut self.max_box,
        }
    }

    /// Move typing to `f`, or out of every box. Leaving the collection's box says what Save will add to; leaving the range for anything but its other end keeps what was typed (or says why not).
    fn edit(&mut self, f: Option<Field>) {
        if f == self.editing {
            return;
        }
        let (old, new) = (self.editing.map(|e| self.field_id(e)), f.map(|e| self.field_id(e)));
        widget::apply_focus_change(self as &mut dyn Container, old, new);
        let range = |e: Option<Field>| matches!(e, Some(Field::Min | Field::Max));
        if self.editing == Some(Field::Collection) {
            self.collection_named();
        }
        if range(self.editing) && !range(f) {
            self.commit_range();
        }
        self.editing = f;
        self.last_blink = Instant::now();
    }

    fn collection_named(&mut self) {
        let name = typed(&self.collection_box);
        if name.is_empty() {
            return;
        }
        self.note = Some(match collection::path_for(&self.dir, &name) {
            Err(e) => (e, WARN),
            Ok(p) if p.exists() => match collection::load(&p) {
                Ok(v) => (format!("{} holds {}; Save adds the next", file_name(&p), v.len()), DIM),
                Err(e) => (e, WARN),
            },
            Ok(p) => (format!("Save starts {} in {}", file_name(&p), self.dir.display()), DIM),
        });
    }

    /// Add the spectrum on screen (the held one while holding) to the collection, with the dark and reference that apply at its exposure.
    fn save(&mut self) {
        let Some(f) = self.shown().cloned() else {
            self.note = Some(("Nothing to save yet".into(), WARN));
            return;
        };
        let path = match collection::path_for(&self.dir, &typed(&self.collection_box)) {
            Ok(p) => p,
            Err(e) => {
                self.note = Some((e, WARN));
                self.edit(Some(Field::Collection));
                return;
            }
        };
        let name = typed(&self.sample_box);
        let sp = Spectrum {
            name: name.clone(),
            at: vsf::eagle_time_oscillations(),
            model: self.model.clone(),
            serial: self.serial.clone(),
            shown: self.quantity_kind(),
            nm: self.nm.clone(),
            dark: self.at(&self.dark, &f).cloned(),
            reference: self.at(&self.reference, &f).cloned(),
            sample: f,
        };
        self.note = Some(match collection::append(&path, sp) {
            Ok(n) => (format!("Saved “{name}”: {n} in {}", file_name(&path)), GOOD),
            Err(e) => (e, WARN),
        });
    }

    fn set_stops(&mut self, stops: f32) {
        let stops = (stops / STOP_STEP).round() * STOP_STEP;
        self.exposure.set_value(stops / STOPS);
        if stops != self.stops {
            self.stops = stops;
            self.send(Cmd::Integration(self.ms()));
        }
    }

    /// Scale the exposure so the brightest unclipped pixel lands at three quarters of full scale, measured over the dark at this exposure if there is one, else over the dimmest sixteenth of the spectrum. A clipped frame says nothing about how bright, so that one only comes down two stops.
    fn auto_expose(&mut self) {
        let Some(f) = &self.live else { return };
        let stops = if f.clipped.iter().any(|&x| x) {
            self.stops - 2.
        } else {
            let peak = f.counts.iter().copied().fold(0., f32::max);
            let base = match self.at(&self.dark, f) {
                Some(d) => d.counts.iter().sum::<f32>() / d.counts.len() as f32,
                None => {
                    let mut v = f.counts.clone();
                    v.sort_by(f32::total_cmp);
                    v[v.len() >> 4]
                }
            };
            // A frame with no light above its floor gives a ratio of ∞ or NaN: the longest exposure is the honest answer, and the slider's range is where it has to land.
            let gain = (AUTO_TARGET - base) / (peak - base);
            (self.stops + gain.log2()).clamp(0., STOPS)
        };
        self.set_stops(stops);
    }

    fn take_widgets(&mut self) {
        if self.dark_btn.take_click() {
            self.capture(Target::Dark);
        }
        if self.ref_btn.take_click() {
            self.capture(Target::Reference);
        }
        if self.auto_btn.take_click() {
            self.auto_expose();
        }
        if self.hold_btn.take_click() {
            self.toggle_hold();
        }
        if self.save_btn.take_click() {
            self.save();
        }
        self.quantity.poll();
        self.quantity.take_change();
        self.scale.poll();
        self.scale.take_change();
    }

    fn slider_moved(&mut self) {
        if self.exposure.take_change() {
            self.set_stops(self.exposure.value() * STOPS);
        }
        if self.average.take_change() {
            let s = (self.average.value() * AVG_STOPS).round() as u32;
            if s != self.avg_stops {
                self.avg_stops = s;
                self.send(Cmd::Average(self.scans()));
            }
        }
    }

    fn zoom(&mut self, x: f32, notches: f32) {
        let f = (-notches / 4.).exp2();
        let at = self.plot.nm_of(x);
        self.view = (at - (at - self.view.0) * f, at + (self.view.1 - at) * f);
    }

    fn key(&mut self, kev: &KeyEvent, ctx: &mut Context) -> EventResponse {
        if kev.state != ElementState::Pressed {
            return EventResponse::Pass;
        }
        if self.modifiers.control_key()
            && let Key::Character(ch) = &kev.logical_key
            && ch.eq_ignore_ascii_case("s")
        {
            self.save();
            ctx.window.request_redraw();
            return EventResponse::Handled;
        }
        // While a box is being typed in, keys are its: Enter on the collection moves to the sample, Enter on the sample saves (and selects it, ready for the next name); Enter on the range's minimum moves to its maximum, Enter on the maximum keeps the range; Tab goes between the two of a pair, Escape leaves (the range's typing undone).
        if let Some(f) = self.editing {
            match (&kev.logical_key, f) {
                (Key::Named(NamedKey::Enter), Field::Collection) | (Key::Named(NamedKey::Tab), Field::Collection) => self.edit(Some(Field::Sample)),
                (Key::Named(NamedKey::Tab), Field::Sample) => self.edit(Some(Field::Collection)),
                (Key::Named(NamedKey::Enter), Field::Sample) => {
                    self.save();
                    self.sample_box.select_all();
                }
                (Key::Named(NamedKey::Enter), Field::Min) | (Key::Named(NamedKey::Tab), Field::Min) => self.edit(Some(Field::Max)),
                (Key::Named(NamedKey::Tab), Field::Max) => self.edit(Some(Field::Min)),
                (Key::Named(NamedKey::Enter), Field::Max) => self.edit(None),
                (Key::Named(NamedKey::Escape), Field::Min | Field::Max) => {
                    self.fill_range(ctx.text);
                    self.edit(None);
                }
                (Key::Named(NamedKey::Escape), _) => self.edit(None),
                _ => {
                    let (id, mods) = (self.field_id(f), self.modifiers);
                    let r = widget::dispatch_key(self as &mut dyn Container, id, kev, mods, ctx.text);
                    self.last_blink = Instant::now();
                    ctx.window.request_redraw();
                    return r;
                }
            }
            ctx.window.request_redraw();
            return EventResponse::Handled;
        }
        match &kev.logical_key {
            Key::Named(NamedKey::Space) => self.toggle_hold(),
            Key::Named(NamedKey::Home) if self.modifiers.shift_key() => self.view = self.full_view(),
            Key::Named(NamedKey::Home) => self.view = self.home_view(),
            Key::Character(ch) => match ch.to_ascii_lowercase().as_str() {
                "d" => self.capture(Target::Dark),
                "r" => self.capture(Target::Reference),
                "a" => self.auto_expose(),
                "l" => self.scale.set_selected(1 - self.scale.selected()),
                "1" => self.quantity.set_selected(0),
                "2" => self.quantity.set_selected(1),
                "3" => self.quantity.set_selected(2),
                _ => return EventResponse::Pass,
            },
            _ => return EventResponse::Pass,
        }
        ctx.window.request_redraw();
        EventResponse::Handled
    }

    fn layout(&mut self, vp: Viewport, text: &mut fluor::text::TextRenderer) {
        let (w, h) = (vp.width_px as Coord, vp.height_px as Coord);
        let span = vp.effective_span();
        let u = span / 32.;
        let fs = span / 48.;
        let top = u * 2.5;
        let side_w = span * 0.375;
        let sx = w - side_w;
        let sw = side_w - u;
        let row = fs * 2.;
        // The panel's rows, from the top: the swatch and its two caption lines, the controls, the readouts, the save.
        let swatch_y = top + fs * 0.3;
        let swatch_h = u * 2.;
        let caption_y = swatch_y + swatch_h + fs * 0.8;
        let exposure_y = caption_y + fs * 2.9;
        let label_w = fs * 6.;
        let slider = |s: &mut Slider, y: Coord| s.set_rect(sx + label_w + (sw - label_w) * 0.5, y + fs * 0.45, sw - label_w, fs * 1.2);
        slider(&mut self.exposure, exposure_y);
        slider(&mut self.average, exposure_y + row);
        let mut y = exposure_y + row * 2. + fs * 0.4;
        for ch in [&mut self.quantity, &mut self.scale] {
            ch.set_rect(sx, y - fs * 0.8, sw, fs * 1.6);
            ch.set_font_size(fs * 0.85);
            y += row;
        }
        let gap = fs * 0.35;
        let half = (sw - gap) * 0.5;
        for (k, b) in [&mut self.dark_btn, &mut self.ref_btn, &mut self.auto_btn, &mut self.hold_btn].into_iter().enumerate() {
            let col = (k & 1) as Coord;
            b.set_rect(sx + half * 0.5 + col * (half + gap), y + (k >> 1) as Coord * row, half, fs * 1.6);
            b.set_font_size(fs * 0.85);
        }
        let readout_y = y + row * 1.9;
        let range_y = readout_y + fs * 5.6;
        let collection_y = range_y + row;
        let sample_y = collection_y + row;
        let box_x = sx + label_w;
        let save_w = fs * 4.5;
        // The range row: two boxes either side of a dash, then "nm".
        let (dash_w, unit_w) = (fs * 1.2, fs * 1.8);
        let range_w = (sw - label_w - dash_w - unit_w) * 0.5;
        self.min_box.set_rect(box_x + range_w * 0.5, range_y, range_w, fs * 1.6);
        self.max_box.set_rect(box_x + range_w * 1.5 + dash_w, range_y, range_w, fs * 1.6);
        for b in [&mut self.min_box, &mut self.max_box] {
            b.set_font_size(fs * 0.85, text);
        }
        self.collection_box.set_rect(box_x + (sw - label_w) * 0.5, collection_y, sw - label_w, fs * 1.6);
        self.sample_box.set_rect(box_x + (sw - label_w - save_w - gap) * 0.5, sample_y, sw - label_w - save_w - gap, fs * 1.6);
        for b in [&mut self.collection_box, &mut self.sample_box] {
            b.set_font_size(fs * 0.85, text);
        }
        self.save_btn.set_rect(sx + sw - save_w * 0.5, sample_y, save_w, fs * 1.6);
        self.save_btn.set_font_size(fs * 0.85);
        self.panel = Panel { swatch_y, swatch_h, caption_y, exposure_y, row, readout_y, range_y, range_box_w: range_w, collection_y, sample_y };
        // The plot: left margin for the value labels, room under it for the colour strip and the wavelength labels, clear of the status band.
        let px = u + fs * 4.;
        let py = top;
        let bottom = h - u * 0.5 - fs * 2.6 - u * 0.5;
        self.plot = Plot { x: px as usize, y: py as usize, w: (sx - u - px) as usize, h: (bottom - py) as usize, nm_lo: self.view.0, nm_hi: self.view.1 };
    }
}

impl Container for App {
    fn visit(&mut self, f: &mut dyn FnMut(&mut dyn Widget)) {
        f(&mut self.exposure);
        f(&mut self.average);
        self.quantity.visit(f);
        self.scale.visit(f);
        f(&mut self.dark_btn);
        f(&mut self.ref_btn);
        f(&mut self.auto_btn);
        f(&mut self.hold_btn);
        f(&mut self.min_box);
        f(&mut self.max_box);
        f(&mut self.collection_box);
        f(&mut self.sample_box);
        f(&mut self.save_btn);
        self.chrome.visit(f);
    }
}

#[derive(Clone, Copy)]
enum Align {
    Left,
    Centre,
    Right,
}

fn text(target: &mut [u32], ctx: &mut Context, s: &str, x: Coord, y: Coord, size: Coord, col: (u8, u8, u8), align: Align) {
    let (bw, bh) = (ctx.viewport.width_px as usize, ctx.viewport.height_px as usize);
    let mut canvas = Canvas::new(target, bw, bh, ctx.damage);
    let style = TextStyle::new(size, c(col));
    match align {
        Align::Left => ctx.text.draw_text_left(&mut canvas, s, x, y, &style, None, None),
        Align::Centre => ctx.text.draw_text_center(&mut canvas, s, x, y, &style, None, None),
        Align::Right => ctx.text.draw_text_right(&mut canvas, s, x, y, &style, None, None),
    };
}

/// A collection's file, by its own name: the full path would run off the panel.
fn file_name(p: &std::path::Path) -> String {
    p.file_name().map_or_else(|| p.display().to_string(), |f| f.to_string_lossy().into_owned())
}

/// A fraction as a percent a person reads at a glance: whole percents from 10 % up, two significant figures below (6.3 %, 0.39 %, 0.024 %), 0 as 0 %.
fn pct(x: f32) -> String {
    let p = x * 100.;
    if p.abs() >= 10. || p == 0. {
        return format!("{p:.0}%");
    }
    // Two significant figures: one decimal from 1 % up, and one more for each tenfold below. Precision cannot be negative, and a negative i32 cast to usize wraps: from 10 % up the branch above has already answered.
    let decimals = (1 - p.abs().log10().floor() as i32) as usize;
    format!("{p:.decimals$}%")
}

/// What a box holds, without the spaces at its ends.
fn typed(b: &Textbox) -> String {
    b.chars.iter().collect::<String>().trim().to_string()
}

/// Four significant digits, fixed-point: what an LMS reading needs across the 10⁻³ to 10⁴ it spans between modes.
fn sig(x: f32) -> String {
    if x == 0. || !x.is_finite() {
        return format!("{x}");
    }
    // Precision cannot be negative, and a negative i32 cast to usize wraps: four digits of a number above 10⁴ is all of its integer part.
    let decimals = (3 - x.abs().log10().floor() as i32).max(0) as usize;
    format!("{x:.decimals$}")
}

/// Wavelength gridline spacing: the finest of 1, 2, 5 × 10ⁿ nm that leaves `gap` pixels between labels.
fn nm_step(p: &Plot, gap: f32) -> f32 {
    let per_px = (p.nm_hi - p.nm_lo) / p.w as f32;
    [1., 2., 5., 10., 20., 50., 100., 200., 500.].into_iter().find(|s| s / per_px >= gap).unwrap_or(1000.)
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
        self.layout(ctx.viewport, ctx.text);
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (msg_tx, msg_rx) = mpsc::channel();
        if let Some(proxy) = self.proxy.clone() {
            let (ms, scans) = (self.ms(), self.scans());
            std::thread::spawn(move || device::run(cmd_rx, msg_tx, proxy, ms, scans));
        }
        self.cmd_tx = Some(cmd_tx);
        self.msg_rx = Some(msg_rx);
    }

    fn on_resize(&mut self, _w: u32, _h: u32, ctx: &mut Context) {
        self.chrome.resize(ctx.viewport);
        self.chrome.set_full_edge(ctx.is_maximized);
        self.layout(ctx.viewport, ctx.text);
    }

    fn tick(&mut self, _ctx: &mut Context) -> bool {
        let Some(f) = self.editing else { return false };
        if self.last_blink.elapsed() < BLINK {
            return false;
        }
        self.last_blink = Instant::now();
        self.field_box(f).flip_blinkey()
    }

    fn wake_at(&self) -> Option<Instant> {
        self.editing.map(|_| self.last_blink + BLINK)
    }

    fn on_user_event(&mut self, _event: (), ctx: &mut Context) -> EventResponse {
        if self.pump() {
            ctx.window.request_redraw();
        }
        EventResponse::Handled
    }

    fn on_event(&mut self, event: &Event, ctx: &mut Context) -> EventResponse {
        let (x, y) = (ctx.cursor_x, ctx.cursor_y);
        match event {
            Event::ModifiersChanged(m) => {
                self.modifiers = *m;
                EventResponse::Pass
            }
            Event::CursorMoved { .. } => {
                match self.drag {
                    Drag::Exposure => self.exposure.set_value_from_x(x),
                    Drag::Average => self.average.set_value_from_x(x),
                    Drag::Pan { x: x0, view } => {
                        let d = (x - x0) / self.plot.w as f32 * (view.1 - view.0);
                        self.view = (view.0 - d, view.1 - d);
                    }
                    Drag::None => {
                        let hit = self.chrome.hit_at(x, y);
                        if self.chrome.set_hover(hit) {
                            widget::apply_hover(self as &mut dyn Container, hit);
                        }
                        self.hovered = hit;
                    }
                }
                self.slider_moved();
                self.cursor = self.plot.contains(x, y).then_some((x, y));
                ctx.window.request_redraw();
                EventResponse::Pass
            }
            Event::CursorLeft => {
                if self.chrome.set_hover(HIT_NONE) {
                    widget::apply_hover(self as &mut dyn Container, HIT_NONE);
                }
                self.hovered = HIT_NONE;
                self.cursor = None;
                ctx.window.request_redraw();
                EventResponse::Pass
            }
            Event::MouseWheel { delta } if self.plot.contains(x, y) => {
                let notches = match delta {
                    MouseScrollDelta::Lines(_, dy) => *dy,
                    MouseScrollDelta::Pixels(_, dy) => *dy / (ctx.viewport.effective_span() / 16.),
                };
                self.zoom(x, notches);
                ctx.window.request_redraw();
                EventResponse::Handled
            }
            Event::MouseInput { state: ElementState::Released, .. } if !matches!(self.drag, Drag::None) => {
                self.drag = Drag::None;
                EventResponse::Handled
            }
            Event::MouseInput { state: ElementState::Pressed, button: MouseButton::Right } if self.plot.contains(x, y) => {
                self.view = self.home_view();
                ctx.window.request_redraw();
                EventResponse::Handled
            }
            Event::MouseInput { state: ElementState::Pressed, button: MouseButton::Left } => {
                let hit = self.chrome.hit_at(x, y);
                if hit != HIT_NONE {
                    let mods = self.modifiers;
                    let r = widget::dispatch_click(self as &mut dyn Container, hit, x, y, mods);
                    let field = [Field::Collection, Field::Sample, Field::Min, Field::Max].into_iter().find(|&f| self.field_id(f) == hit);
                    self.edit(field);
                    self.take_widgets();
                    if hit == self.exposure.hit_id() {
                        self.drag = Drag::Exposure;
                        self.exposure.set_value_from_x(x);
                    } else if hit == self.average.hit_id() {
                        self.drag = Drag::Average;
                        self.average.set_value_from_x(x);
                    }
                    self.slider_moved();
                    ctx.window.request_redraw();
                    return r;
                }
                self.edit(None);
                if self.plot.contains(x, y) {
                    self.drag = Drag::Pan { x, view: self.view };
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
        self.layout(ctx.viewport, ctx.text);
        // The range boxes show the kept range (refilled here, where the text renderer is), unless they are being typed in.
        if self.refill_range && !matches!(self.editing, Some(Field::Min | Field::Max)) {
            self.fill_range(ctx.text);
        }
        let (bw, bh) = (ctx.viewport.width_px as usize, ctx.viewport.height_px as usize);
        // The frame, front to back for pixels and hits alike (fluor's contract, chrome_widget.rs): the host has cleared both over the damage, the window controls are frontmost so they go in first, and the background goes in last, at the end of render.
        widget::apply_pressed(self as &mut dyn Container, ctx.pressed_hit);
        self.chrome.rasterize_bg(ctx.damage, |canvas| canvas.pixels.fill(c(BG)));
        self.chrome.rasterize_perimeter(target, bw, bh, ctx.clip_mask);
        self.chrome.rasterize_chrome(ctx.damage, ctx.text, ctx.clip_mask);
        self.chrome.flatten_chrome_into(target, bw, bh, None);
        let w = bw as Coord;
        let span = ctx.viewport.effective_span();
        let u = span / 32.;
        let fs = span / 48.;
        let top = u * 2.5;
        let sx = w - span * 0.375;
        let sw = span * 0.375 - u;
        let p = self.plot;
        // Hovering Dark or Reference shows that capture in place of the live curve, in raw counts: the calibration as it was kept.
        let preview: Option<(&str, Frame)> = if self.hovered == self.dark_btn.hit_id() {
            self.dark.clone().map(|f| ("Dark", f))
        } else if self.hovered == self.ref_btn.hit_id() {
            self.reference.clone().map(|f| ("Reference", f))
        } else {
            None
        };
        let q = if preview.is_some() { Quantity::Counts } else { self.quantity_kind() };
        let axis = Axis::new(q, self.scale.selected() == 1);
        let values = match &preview {
            Some((_, f)) => Ok(f.counts.clone()),
            None => self.values(),
        };
        let frame_owned = preview.as_ref().map(|(_, f)| f.clone()).or_else(|| self.shown().cloned());
        let frame = frame_owned.as_ref();
        let (ms, scans) = (self.ms(), self.scans());

        // ---- status band ----
        if !self.failed {
            // A person counts the frame under way from one.
            let under_way = |p: &Progress| {
                let secs = if p.read >= Duration::from_secs(1) { format!(" · {:.1} of {:.1} s", p.elapsed.as_secs_f32(), p.read.as_secs_f32()) } else { String::new() };
                format!("frame {} of {}{secs}", p.done + 1, p.of)
            };
            let state = match (&self.nm.is_empty(), self.progress, &self.held) {
                (true, ..) => "Opening the spectrometer…".to_string(),
                (_, Some(p), _) => format!("Exposing: {}", under_way(&p)),
                (_, _, Some(_)) => "Held. Space or Hold to go live.".into(),
                _ => match &values {
                    Ok(_) => "Live.".into(),
                    Err(_) => "Live; the plot says what it needs.".into(),
                },
            };
            // How far the detector's baseline has moved since the dark at this exposure, which every dark-subtracted value has taken off: the drift made visible.
            let drift = frame.and_then(|f| self.at(&self.dark, f).map(|d| f.drift_from(d)));
            let state = match drift {
                Some(d) => format!("{state}  ·  baseline {d:+.1} counts since the dark (taken off)"),
                None => state,
            };
            self.chrome.set_status_text(Some(state));
        }

        // Front to back: the hover readout, then text, then widgets, then the curve and its fill, the grid, the strip, the swatch, and the panels last.

        // ---- hover ----
        if let (Some((cx, cy)), Ok(v)) = (self.cursor, &values)
            && !matches!(self.drag, Drag::Pan { .. })
        {
            let l = p.nm_of(cx);
            let j = self.nm.partition_point(|&x| x <= l);
            let at = (j > 0 && j < self.nm.len()).then(|| {
                let t = (l - self.nm[j - 1]) / (self.nm[j] - self.nm[j - 1]);
                v[j - 1] * (1. - t) + v[j] * t
            });
            let reading = match at {
                Some(x) if x.is_finite() && q == Quantity::Counts => format!("{l:.1} nm   {x:.0} counts  ·  {:.2}% of full scale", x / FULL_SCALE as f32 * 100.),
                Some(x) if x.is_finite() => format!("{l:.1} nm   {:.2}%", x * 100.),
                _ => format!("{l:.1} nm"),
            };
            let right = cx > (p.x + p.w / 2) as f32;
            let tx = if right { cx - fs * 0.6 } else { cx + fs * 0.6 };
            text(target, ctx, &reading, tx, cy - fs, fs * 0.9, FG, if right { Align::Right } else { Align::Left });
            let mut canvas = Canvas::new(target, bw, bh, ctx.damage);
            fill_rect(&mut canvas, cx as isize, p.y as isize, 1, p.h as isize, pack_argb(FG.0, FG.1, FG.2, 96), None, None);
        }

        // ---- plot labels ----
        let title = match (&preview, q) {
            (Some((what, f)), _) => format!("{what} as kept: {} ms × {}, raw counts, % of full scale", f.ms, f.scans),
            (_, Quantity::Counts) if frame.is_some_and(|f| self.at(&self.dark, f).is_some()) => "Counts less the dark, % of full scale".into(),
            (_, Quantity::Counts) => "Counts, % of full scale".into(),
            (_, Quantity::Ratio) => "% of the reference".into(),
            (_, Quantity::Emission) => "Emission, % of its peak (SL1 reference)".into(),
        };
        text(target, ctx, &title, p.x as f32, top - fs * 0.9, fs * 0.85, if preview.is_some() { ACCENT } else { DIM }, Align::Left);
        if let Err(e) = &values {
            for (k, line) in e.lines().enumerate() {
                text(target, ctx, line, (p.x + p.w / 2) as f32, (p.y + p.h / 2) as f32 + k as f32 * fs * 1.4, fs * 0.85, DIM, Align::Centre);
            }
        }
        let step = nm_step(&p, fs * 4.);
        let first = (p.nm_lo / step).ceil() as i64;
        let last = (p.nm_hi / step).floor() as i64;
        let label_y = (p.y + p.h) as f32 + u * 0.5 + fs * 1.1;
        for k in first..=last {
            let l = k as f32 * step;
            text(target, ctx, &format!("{l}"), p.x_of(l), label_y, fs * 0.75, DIM, Align::Centre);
        }
        text(target, ctx, "nm", (p.x + p.w) as f32, label_y + fs * 1.1, fs * 0.7, DIM, Align::Right);
        for (t, s) in &axis.ticks {
            text(target, ctx, s, p.x as f32 - fs * 0.4, p.y as f32 + axis.tick_row(*t, p.h), fs * 0.75, DIM, Align::Right);
        }

        // ---- panel text ----
        let pn = self.panel;
        let (swatch_y, swatch_h) = (pn.swatch_y, pn.swatch_h);
        let ty = pn.caption_y;
        // Raw counts sit on a ~2000-count bias the same at every wavelength; its colour would be the bias's, white whatever the light. Counts give a colour only less the dark.
        // A capture on show is raw counts, the dark not taken off: no colour for it.
        let has_dark = preview.is_none() && frame.is_some_and(|f| self.at(&self.dark, f).is_some());
        let lms = values.as_ref().ok().filter(|_| q != Quantity::Counts || has_dark).map(|v| colour::spectrum_lms(&self.nm, v));
        if lms.is_none() && values.is_ok() {
            text(target, ctx, "Block the light and press Dark to see the colour", sx + sw * 0.5, swatch_y + swatch_h * 0.5, fs * 0.75, DIM, Align::Centre);
        }
        let (sw_rgb, out_of_gamut) = match lms {
            Some(lms) => {
                let (rgb, out) = colour::swatch(lms, q == Quantity::Ratio);
                (Some(rgb), out)
            }
            None => (None, false),
        };
        if let Some(lms) = lms {
            text(target, ctx, &format!("L {}   M {}   S {}", sig(lms[0]), sig(lms[1]), sig(lms[2])), sx, ty, fs * 0.85, FG, Align::Left);
            let note = match q {
                Quantity::Counts => "uncorrected for the instrument's response",
                Quantity::Ratio => "under equal-energy light",
                Quantity::Emission => "the light itself",
            };
            text(target, ctx, &format!("{note}{}", if out_of_gamut { " · beyond the screen's gamut" } else { "" }), sx, ty + fs * 1.2, fs * 0.7, DIM, Align::Left);
        }
        let ty = pn.exposure_y;
        let exp_label = if ms < 1000 { format!("{ms} ms") } else { format!("{:.2} s", ms as f32 / 1000.) };
        text(target, ctx, "Exposure", sx, ty, fs * 0.85, FG, Align::Left);
        text(target, ctx, &exp_label, sx, ty + fs * 0.95, fs * 0.7, ACCENT, Align::Left);
        let row = pn.row;
        text(target, ctx, "Average", sx, ty + row, fs * 0.85, FG, Align::Left);
        text(target, ctx, &format!("{scans} frame{}", if scans == 1 { "" } else { "s" }), sx, ty + row + fs * 0.95, fs * 0.7, ACCENT, Align::Left);

        // Capture state on the buttons themselves.
        let cap_label = |name: &str, cap: &Option<Frame>| match (cap, frame) {
            (Some(d), Some(f)) if d.ms == f.ms => format!("{name} ✓"),
            (Some(d), _) => format!("{name} ({} ms)", d.ms),
            (None, _) => name.to_string(),
        };
        self.dark_btn.set_label(cap_label("Dark", &self.dark));
        self.ref_btn.set_label(cap_label("Reference", &self.reference));
        self.hold_btn.set_label(if self.held.is_some() { "Go live" } else { "Hold" });

        // Readouts under the buttons: peak and centroid, by power and by photons, over what is in view.
        let mut ry = pn.readout_y;
        let line_gap = fs * 1.3;
        let st = values.as_ref().ok().and_then(|v| stats(&self.nm, v, (p.nm_lo, p.nm_hi)));
        let nm_text = |x: Option<f32>| x.map_or("—".to_string(), |l| format!("{l:.1} nm"));
        let mid = sx + sw * 0.62;
        text(target, ctx, "in view", sx, ry, fs * 0.7, DIM, Align::Left);
        text(target, ctx, "power", mid, ry, fs * 0.7, DIM, Align::Right);
        text(target, ctx, "photons", sx + sw, ry, fs * 0.7, DIM, Align::Right);
        ry += line_gap;
        let rows: [(&str, String, String); 2] = [
            ("Peak", nm_text(st.as_ref().map(|s| s.peak_power)), nm_text(st.as_ref().map(|s| s.peak_quanta))),
            ("Centroid", nm_text(st.as_ref().and_then(|s| s.centre_power)), nm_text(st.as_ref().and_then(|s| s.centre_quanta))),
        ];
        for (label, power, photons) in &rows {
            text(target, ctx, label, sx, ry, fs * 0.8, DIM, Align::Left);
            text(target, ctx, power, mid, ry, fs * 0.8, FG, Align::Right);
            text(target, ctx, photons, sx + sw, ry, fs * 0.8, FG, Align::Right);
            ry += line_gap;
        }
        if let Some(f) = frame {
            let clipped = f.clipped.iter().filter(|&&x| x).count();
            let peak = f.counts.iter().copied().fold(0., f32::max);
            let (msg, col) = if clipped > 0 { (format!("{clipped} pixels clipped: shorten the exposure"), WARN) } else { (format!("brightest pixel at {:.0}% of full scale", peak / FULL_SCALE as f32 * 100.), DIM) };
            text(target, ctx, &msg, sx, ry, fs * 0.75, col, Align::Left);
        }

        // The kept wavelength range: two boxes either side of a dash.
        let range_x = sx + fs * 6.;
        text(target, ctx, "Range", sx, pn.range_y, fs * 0.85, FG, Align::Left);
        text(target, ctx, "–", range_x + pn.range_box_w + fs * 0.6, pn.range_y, fs * 0.85, DIM, Align::Centre);
        text(target, ctx, "nm", range_x + pn.range_box_w * 2. + fs * 1.5, pn.range_y, fs * 0.8, DIM, Align::Left);

        // Saving: the collection and the sample's name, and what the last save said.
        text(target, ctx, "Collection", sx, pn.collection_y, fs * 0.85, FG, Align::Left);
        text(target, ctx, "Sample", sx, pn.sample_y, fs * 0.85, FG, Align::Left);
        if let Some((msg, col)) = &self.note {
            text(target, ctx, msg, sx, pn.sample_y + fs * 1.5, fs * 0.7, *col, Align::Left);
        }
        // Where the collections go, always on show: the folder the tool was started from, unless `--dir` said otherwise.
        text(target, ctx, &format!("Saves to {}", self.dir.display()), sx, pn.sample_y + fs * 2.6, fs * 0.65, DIM, Align::Left);

        // ---- widgets ----
        {
            let mut canvas = Canvas::new(target, bw, bh, ctx.damage);
            for s in [&mut self.exposure, &mut self.average] {
                let id = s.hit_id();
                s.render_content_into(&mut canvas, Some(&mut self.chrome.hit_test_map), id);
            }
            for ch in [&mut self.quantity, &mut self.scale] {
                ch.render_content_into(&mut canvas, ctx.text, None, Some(&mut self.chrome.hit_test_map));
            }
            for b in [&mut self.dark_btn, &mut self.ref_btn, &mut self.auto_btn, &mut self.hold_btn, &mut self.save_btn] {
                let id = b.hit_id();
                b.render_content_into(&mut canvas, 0., 0., ctx.text, None, Some(&mut self.chrome.hit_test_map), id);
            }
            for b in [&mut self.min_box, &mut self.max_box, &mut self.collection_box, &mut self.sample_box] {
                let id = b.hit_id();
                b.render_content_into(&mut canvas, 0., 0., ctx.text, None, None, Some(&mut self.chrome.hit_test_map), id);
            }
        }

        // ---- the curve ----
        let tints: Vec<[u8; 3]> = (0..p.w).map(|col| colour::encode(colour::tint(p.nm_of(p.x as f32 + col as f32 + 0.5)))).collect();
        {
            let mut canvas = Canvas::new(target, bw, bh, ctx.damage);
            if let Some(f) = frame {
                // Clipped pixels: a red band along the top of the plot over each one.
                let band = (u * 0.25) as isize;
                for (k, &cl) in f.clipped.iter().enumerate() {
                    if cl && (p.nm_lo..p.nm_hi).contains(&self.nm[k]) {
                        let x0 = p.x_of(if k > 0 { (self.nm[k - 1] + self.nm[k]) * 0.5 } else { self.nm[k] });
                        let x1 = p.x_of(if k + 1 < self.nm.len() { (self.nm[k] + self.nm[k + 1]) * 0.5 } else { self.nm[k] });
                        fill_rect(&mut canvas, x0 as isize, p.y as isize, (x1 - x0).ceil().max(1.) as isize, band, c(WARN), None, None);
                    }
                }
            }
            if let Ok(v) = &values {
                let rows: Vec<f32> = v.iter().map(|&x| axis.row(x, p.h)).collect();
                let ys = plot::trace(&p, &self.nm, &rows);
                let t = span / (1 << 9) as f32 + 1.;
                plot::line(&mut canvas, &p, &plot::vertices(&p, &self.nm, &rows), t, LINE, 1.);
                plot::fill(&mut canvas, &p, &ys, 0.85, |col| tints[col]);
            }
            // In Counts, the reference at this exposure rides behind as a faint line, less the same dark as the live curve.
            if q == Quantity::Counts
                && let Some(f) = frame
                && let Some(r) = self.at(&self.reference, f)
            {
                let dark = self.at(&self.dark, f);
                let rows: Vec<f32> = r.counts.iter().enumerate().map(|(k, &x)| axis.row(x - dark.map_or(0., |d| d.counts[k]), p.h)).collect();
                plot::line(&mut canvas, &p, &plot::vertices(&p, &self.nm, &rows), span / (1 << 10) as f32 + 1., [255, 200, 120], 0.45);
            }
        }

        // ---- grid, strip, swatch, panels ----
        {
            let mut canvas = Canvas::new(target, bw, bh, ctx.damage);
            for k in first..=last {
                let x = p.x_of(k as f32 * step);
                fill_rect(&mut canvas, x as isize, p.y as isize, 1, p.h as isize, c(GRID), None, None);
            }
            for (t, _) in &axis.ticks {
                fill_rect(&mut canvas, p.x as isize, (p.y as f32 + axis.tick_row(*t, p.h)) as isize, p.w as isize, 1, c(GRID), None, None);
            }
            // Minor lines behind the labelled ones: a tenth of the wavelength step (a fifth of a 5 × 10ⁿ one, so they stay on round numbers), and the axis's own.
            let lead = step / 10f32.powf(step.log10().floor());
            let minor = step / if lead.round() == 5. { 5. } else { 10. };
            for k in (p.nm_lo / minor).ceil() as i64..=(p.nm_hi / minor).floor() as i64 {
                fill_rect(&mut canvas, p.x_of(k as f32 * minor) as isize, p.y as isize, 1, p.h as isize, c(GRID_MINOR), None, None);
            }
            for t in &axis.minor {
                fill_rect(&mut canvas, p.x as isize, (p.y as f32 + axis.tick_row(*t, p.h)) as isize, p.w as isize, 1, c(GRID_MINOR), None, None);
            }
            // The colour of every wavelength in view, under the axis.
            let strip_y = (p.y + p.h) as isize + (u * 0.15) as isize;
            for (col, rgb) in tints.iter().enumerate() {
                fill_rect(&mut canvas, (p.x + col) as isize, strip_y, 1, (u * 0.3) as isize, pack_argb(rgb[0], rgb[1], rgb[2], 255), None, None);
            }
            match sw_rgb {
                Some(rgb) => fill_rect(&mut canvas, sx as isize, swatch_y as isize, sw as isize, swatch_h as isize, pack_argb(rgb[0], rgb[1], rgb[2], 255), None, None),
                None => fill_rect(&mut canvas, sx as isize, swatch_y as isize, sw as isize, swatch_h as isize, c(PANEL), None, None),
            }
            fill_rect(&mut canvas, p.x as isize, p.y as isize, p.w as isize, p.h as isize, c(PANEL), None, None);
        }

        // ---- the window's background, behind everything ----
        self.chrome.flatten_bg_into(target, bw, bh, None);
    }

    fn cursor_for(&self, x: Coord, y: Coord, ctx: &Context) -> CursorIcon {
        let hit = self.chrome.hit_at(x, y);
        if hit != HIT_NONE {
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
