//! The StellarNet USB spectrometer (BLACK-Comet CXR-SR-X here), spoken to directly over usbfs. Replaces StellarNet's stellarnet_driver3, a Cython module that only exists for Python ≤ 3.9; every request below was traced off that driver's own pyusb calls against this instrument (2026-11-26), not guessed.
//!
//! The wire, all vendor control requests on the default pipe plus one bulk endpoint:
//! - `0xA0` OUT (bare FX2 only): write firmware RAM at wValue; `0xE600` is CPUCS, 1 holds the 8051 in reset, 0 lets it run and re-enumerate as `0BD7:A012`.
//! - `0xB4` OUT `[count hi, count lo, xtiming, clock]`: integration and pixel clock (see [`config_packet`]).
//! - `0xB2` OUT, no data: start an exposure.
//! - `0xB3` IN 2 bytes `[0xB3, ready]`: 1 once the exposure has been read out.
//! - `0xB6` OUT `[0, address, 0]` then `0xB5` IN 33 bytes `[0xB5, 32 bytes]`: one 32-byte EEPROM block.
//! - `0xB7` OUT `[register, value, 0, 0]`: the detector's gain registers.
//! - bulk IN `0x88`, 4096 bytes: 2048 pixels, u16 little-endian, raw counts (the vendor driver hands them on untouched when smoothing is off). The vendor also re-reads EEPROM block 0x20 before every bulk read; that changes nothing (256/256 frames intact with and without) and is dropped here.

use std::fmt;
use std::thread::sleep;
use std::time::{Duration, Instant};

use nusb::transfer::{Bulk, ControlIn, ControlOut, ControlType, In, Recipient, TransferError};
use nusb::{Device, DeviceInfo, Endpoint, Interface, MaybeFuture};

mod firmware;

/// Pixels in one readout.
pub const PIXELS: usize = 2048;
/// What a saturated pixel reads.
pub const FULL_SCALE: u16 = u16::MAX;
/// Pixels at the head of every readout that are not spectral, still read out and returned; a spectrum starts after them. Measured 2026-11-26: pixels 0..26 read a masked level well under the ~2000-count bias (~200 counts one morning, ~370 later, at every integration and under room light alike), and pixels 26..30 climb from it to the bias over four pixels (1080, 1650, 1870, 1970), the readout settling rather than light at 229–231 nm. Thirty-two leaves the settling behind with a margin; it costs 228–233 nm, where the detector sees next to nothing.
pub const MASKED: usize = 1 << 5;
/// The masked pixels that always read low, for telling an intact readout from one that starts mid-scan: the first sixteen sit far below the bias in every frame.
pub const MASK_MARK: usize = 1 << 4;
/// Integration the firmware accepts, in ms; the vendor driver refuses 0 and anything from 498000 up.
pub const INTEGRATION_MS: std::ops::RangeInclusive<u32> = 1..=497_999;

/// The bare Cypress FX2 before it has firmware. Note this is Cypress's generic development ID, which any blank FX2 board also wears; like the vendor driver, `open` will load spectrometer firmware into whichever one it finds when no StellarNet is already up.
const FX2_BARE: (u16, u16) = (0x04b4, 0x8613);
const STELLARNET: (u16, u16) = (0x0bd7, 0xa012);

const FX2_LOAD: u8 = 0xa0;
const FX2_CPUCS: u16 = 0xe600;
const SET_CONFIG: u8 = 0xb4;
const START: u8 = 0xb2;
const READY: u8 = 0xb3;
const EEPROM_READ: u8 = 0xb5;
const EEPROM_ADDRESS: u8 = 0xb6;
const GAIN: u8 = 0xb7;
const SPECTRUM_EP: u8 = 0x88;

const EEPROM_MODEL: u8 = 0x20;
const EEPROM_HOT_PIXELS: [u8; 2] = [0x40, 0x60];
const EEPROM_COEFFS: [u8; 4] = [0x80, 0xa0, 0xc0, 0xe0];
const EEPROM_GAIN_TABLE: u8 = 0xff;

/// One control transfer; pyusb's default.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(1);
/// The bulk readout, which only starts once the firmware says the frame is ready.
const BULK_TIMEOUT: Duration = Duration::from_secs(1);
/// How long past the integration time a frame may take to become ready before the instrument is called hung.
const READY_SLACK: Duration = Duration::from_secs(1);
/// Readiness poll once the integration has elapsed. The vendor sleeps 0.9·t then polls every t/3, which at long integrations leaves minutes on the table; the readout itself takes a few ms, and polling through it does no harm (256/256 frames intact at 4, 5 and 50 ms).
const READY_POLL: Duration = Duration::from_micros(1 << 10);
/// Least time from one frame leaving the bulk pipe to the next START. Any sooner and the detector starts capturing mid-scan: the frame comes back rotated, its 27 masked pixels (0..27, ~200 counts dark) landing up to ~370 pixels in. Measured 2026-11-26, 256 frames each: no pause lost 12/256 at 50 ms and 69/256 at 4 ms; a 1 ms pause after ready still lost 30/256 at 5 ms, 2 ms and up lost none. Four is that with a factor of two, and resting it from the end of the bulk read lost none in 512 frames each at 1, 2, 3, 4, 5, 10 and 50 ms. The vendor driver sleeps t/3 after ready instead (and times out below 10 ms).
const FRAME_REST: Duration = Duration::from_millis(1 << 2);
/// How often [`Spectrometer::read_while`] asks whether to go on, through an integration.
pub const WAIT_SLICE: Duration = Duration::from_millis(1 << 4);
/// Re-enumeration after the firmware starts: polled every 256 ms, for up to 16 polls (the vendor gives it 2 s).
const ENUM_POLL: Duration = Duration::from_millis(1 << 8);
const ENUM_POLLS: u32 = 1 << 4;
/// The vendor's settle between the two gain registers.
const GAIN_SETTLE: Duration = Duration::from_millis(400);

/// The vendor's power-on configuration, which `open` sends before reading the EEPROM, as it does.
const BOOT_MS: u32 = 100;

#[derive(Debug)]
pub enum Error {
    /// Neither a StellarNet nor a bare FX2 is on the bus.
    NotPlugged,
    /// Firmware went in, but the spectrometer never came back as `0BD7:A012`.
    NoReenumeration,
    Usb(nusb::Error),
    Transfer(TransferError),
    /// A reply that is not the shape the firmware sends.
    Reply { request: u8, got: Vec<u8> },
    /// A bulk readout shorter than 2048 pixels.
    ShortRead(usize),
    /// The frame never became ready.
    NeverReady,
    /// An integration time the firmware cannot take.
    Integration(u32),
    /// EEPROM content this driver does not understand; everything past here (wavelength mapping, gain baseline, the raw readout) was only verified on detector type 1 with no hot pixels mapped.
    Eeprom(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotPlugged => write!(f, "no StellarNet spectrometer on USB; is it plugged in, and is the StellarNet app closed?"),
            Error::NoReenumeration => write!(f, "firmware loaded, but the spectrometer never re-enumerated"),
            Error::Usb(e) => write!(f, "USB: {e}"),
            Error::Transfer(e) => write!(f, "USB transfer: {e}; the spectrometer may have dropped off USB"),
            Error::Reply { request, got } => write!(f, "request {request:#04x} answered {got:02x?}"),
            Error::ShortRead(n) => write!(f, "spectrum readout was {n} bytes, not {}", PIXELS * 2),
            Error::NeverReady => write!(f, "the spectrometer never finished the exposure"),
            Error::Integration(ms) => write!(f, "integration {ms} ms is outside {}..={} ms", INTEGRATION_MS.start(), INTEGRATION_MS.end()),
            Error::Eeprom(what) => write!(f, "EEPROM: {what}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<nusb::Error> for Error {
    fn from(e: nusb::Error) -> Self {
        Error::Usb(e)
    }
}

impl From<TransferError> for Error {
    fn from(e: TransferError) -> Self {
        Error::Transfer(e)
    }
}

/// The detector's pixel clock. The vendor driver takes 1, 2 or 3 and refuses 4.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum XTiming {
    One = 1,
    Two = 2,
    Three = 3,
}

impl XTiming {
    fn code(self) -> u8 {
        1 << (self as u8 + 1)
    }
}

/// The 0xB4 packet. Up to 65000 ms the count is milliseconds on clock 0x1F; past that it is eighths of the integration (truncated) on clock 0xFA; 1 ms goes as a count of 2 on the doubled clock 0x0F. Every branch matches the vendor driver byte for byte across 1..=497999 (sampled at each edge).
pub fn config_packet(ms: u32, xtiming: XTiming) -> Result<[u8; 4], Error> {
    if !INTEGRATION_MS.contains(&ms) {
        return Err(Error::Integration(ms));
    }
    let (count, clock) = match ms {
        1 => (2, 0x0f),
        2..=65000 => (ms, 0x1f),
        _ => (ms >> 3, 0xfa),
    };
    Ok([(count >> 8) as u8, count as u8, xtiming.code(), clock])
}

pub struct Spectrometer {
    interface: Interface,
    spectrum: Endpoint<Bulk, In>,
    /// "BLACK CXR-SR-X".
    pub model: String,
    /// "24032812".
    pub serial: String,
    /// C1..C4 from the EEPROM: λ = C3 + C1·x + C2·x² + C4·x³ at x = pixel / 2.
    pub coeffs: [f64; 4],
    integration_ms: u32,
    xtiming: XTiming,
    /// When the last frame came off the bulk pipe; the next START waits out FRAME_REST from here.
    last_frame: Instant,
    /// A setting changed since the last frame: the next read throws one away first.
    discard: bool,
}

impl Spectrometer {
    /// Finds the instrument, loading its firmware first if it has just been plugged in, then reads its calibration and sets the detector's gain baseline.
    pub fn open() -> Result<Spectrometer, Error> {
        let info = match find(STELLARNET)? {
            Some(info) => info,
            None => {
                let bare = find(FX2_BARE)?.ok_or(Error::NotPlugged)?;
                load_firmware(&bare.open().wait()?)?;
                wait_for(STELLARNET)?
            }
        };
        let device = info.open().wait()?;
        device.set_configuration(1).wait()?;
        let interface = device.claim_interface(0).wait()?;
        let spectrum = interface.endpoint::<Bulk, In>(SPECTRUM_EP)?;
        let mut s = Spectrometer { interface, spectrum, model: String::new(), serial: String::new(), coeffs: [0.; 4], integration_ms: BOOT_MS, xtiming: XTiming::Three, last_frame: Instant::now(), discard: true };
        s.control_out(SET_CONFIG, &config_packet(BOOT_MS, XTiming::Three)?)?;

        for address in EEPROM_HOT_PIXELS {
            let block = s.eeprom(address)?;
            if block.iter().any(|&b| b != b' ' && b != 0xff) {
                return Err(Error::Eeprom("hot pixels are mapped, and this driver does not correct them"));
            }
        }
        let mut coeff_blocks = [[0; 32]; 4];
        for (block, address) in coeff_blocks.iter_mut().zip(EEPROM_COEFFS) {
            *block = s.eeprom(address)?;
        }
        for (c, block) in s.coeffs.iter_mut().zip(&coeff_blocks) {
            *c = number(block)?;
        }
        // The detector type is the last character of the C1 block.
        if coeff_blocks[0][31] != b'1' {
            return Err(Error::Eeprom("detector type is not 1, the only one this driver has been checked against"));
        }
        let ident = s.eeprom(EEPROM_MODEL)?;
        let ident = std::str::from_utf8(&ident).map_err(|_| Error::Eeprom("model block is not text"))?;
        let (model, rest) = ident.split_once(" #").ok_or(Error::Eeprom("model block has no '#' serial"))?;
        s.model = model.trim().to_string();
        s.serial = rest.chars().take_while(char::is_ascii_digit).collect();

        s.gain_baseline()?;
        Ok(s)
    }

    /// The wavelength of every pixel, nm.
    pub fn wavelengths(&self) -> Vec<f64> {
        let [c1, c2, c3, c4] = self.coeffs;
        (0..PIXELS)
            .map(|p| {
                let x = p as f64 * 0.5;
                c3 + x * (c1 + x * (c2 + x * c4))
            })
            .collect()
    }

    pub fn integration_ms(&self) -> u32 {
        self.integration_ms
    }

    /// Sets the integration and pixel clock, at once. The first frame after it is thrown away inside the next read (the detector was already exposing at the old setting), so that wait can be walked away from like any other.
    pub fn configure(&mut self, ms: u32, xtiming: XTiming) -> Result<(), Error> {
        self.control_out(SET_CONFIG, &config_packet(ms, xtiming)?)?;
        self.integration_ms = ms;
        self.xtiming = xtiming;
        self.discard = true;
        Ok(())
    }

    /// Sets the integration at the current pixel clock.
    pub fn set_integration(&mut self, ms: u32) -> Result<(), Error> {
        self.configure(ms, self.xtiming)
    }

    /// Whether the next read first throws a frame away, after a change of setting: it then takes two integrations.
    pub fn discard_pending(&self) -> bool {
        self.discard
    }

    /// One exposure at the current integration, raw counts.
    pub fn read(&mut self) -> Result<[u16; PIXELS], Error> {
        Ok(self.read_while(|| true)?.expect("an exposure nothing walks away from comes back"))
    }

    /// One exposure (after the frame a change of setting throws away), walked away from (None) the moment `go_on` says no; it is asked every [`WAIT_SLICE`] through each integration. Walking away is clean (measured 2026-11-26): a reconfigure takes effect at once, a fresh START is a full new exposure, and every frame after is intact.
    pub fn read_while(&mut self, mut go_on: impl FnMut() -> bool) -> Result<Option<[u16; PIXELS]>, Error> {
        if self.discard {
            if self.expose(&mut go_on)?.is_none() {
                return Ok(None);
            }
            self.discard = false;
        }
        self.expose(&mut go_on)
    }

    fn expose(&mut self, go_on: &mut impl FnMut() -> bool) -> Result<Option<[u16; PIXELS]>, Error> {
        // checked_sub: `last_frame.elapsed()` is wall time the caller spent since the last frame, and it normally exceeds FRAME_REST (any caller doing work between reads); plain Duration subtraction panics on that, so None here means "already rested", not a bug.
        if let Some(rest) = FRAME_REST.checked_sub(self.last_frame.elapsed()) {
            sleep(rest);
        }
        let integration = Duration::from_millis(self.integration_ms as u64);
        self.control_out(START, &[])?;
        let started = Instant::now();
        while let Some(left) = integration.checked_sub(started.elapsed()) {
            if !go_on() {
                return Ok(None);
            }
            // The shorter of a slice and what is left, so the integration is not overslept. checked_sub: past the integration there is nothing left, which ends the wait (None), not a bug.
            sleep(left.min(WAIT_SLICE));
        }
        loop {
            if self.control_in(READY, 2)?[1] == 1 {
                break;
            }
            if started.elapsed() > integration + READY_SLACK {
                return Err(Error::NeverReady);
            }
            sleep(READY_POLL);
        }
        let buffer = self.spectrum.allocate(PIXELS * 2);
        let done = self.spectrum.transfer_blocking(buffer, BULK_TIMEOUT);
        self.last_frame = Instant::now();
        done.status?;
        if done.buffer.len() != PIXELS * 2 {
            return Err(Error::ShortRead(done.buffer.len()));
        }
        let mut counts = [0; PIXELS];
        for (c, b) in counts.iter_mut().zip(done.buffer.chunks_exact(2)) {
            *c = u16::from_le_bytes([b[0], b[1]]);
        }
        Ok(Some(counts))
    }

    /// The vendor's `gain_baseline`: two fixed registers, then the first entry of each of the EEPROM's two gain tables into registers 0 and 1.
    fn gain_baseline(&mut self) -> Result<(), Error> {
        self.control_out(GAIN, &[3, 0x80, 0, 0])?;
        self.control_out(GAIN, &[2, 0xf8, 0, 0])?;
        let table = self.eeprom(EEPROM_GAIN_TABLE)?;
        // "4 20 54 94134174 65123155205216 ": one character, then ten three-wide fields, two tables of five.
        let field = |at: usize| -> Result<u8, Error> {
            std::str::from_utf8(&table[at..at + 3]).ok().and_then(|f| f.trim().parse().ok()).ok_or(Error::Eeprom("gain table is not three-wide numbers"))
        };
        let (g0, g1) = (field(1)?, field(16)?);
        self.control_out(GAIN, &[0, g0, 0, 0])?;
        sleep(GAIN_SETTLE);
        self.control_out(GAIN, &[1, g1, 0, 0])?;
        Ok(())
    }

    fn eeprom(&self, address: u8) -> Result<[u8; 32], Error> {
        self.control_out(EEPROM_ADDRESS, &[0, address, 0])?;
        let r = self.control_in(EEPROM_READ, 33)?;
        if r[0] != EEPROM_READ {
            return Err(Error::Reply { request: EEPROM_READ, got: r });
        }
        let mut block = [0; 32];
        block.copy_from_slice(&r[1..]);
        Ok(block)
    }

    fn control_out(&self, request: u8, data: &[u8]) -> Result<(), Error> {
        let packet = ControlOut { control_type: ControlType::Vendor, recipient: Recipient::Device, request, value: 0, index: 0, data };
        Ok(self.interface.control_out(packet, CONTROL_TIMEOUT).wait()?)
    }

    /// A vendor IN request, held to exactly the length asked for.
    fn control_in(&self, request: u8, length: u16) -> Result<Vec<u8>, Error> {
        let packet = ControlIn { control_type: ControlType::Vendor, recipient: Recipient::Device, request, value: 0, index: 0, length };
        let r = self.interface.control_in(packet, CONTROL_TIMEOUT).wait()?;
        if r.len() != length as usize {
            return Err(Error::Reply { request, got: r });
        }
        Ok(r)
    }
}

fn find((vendor, product): (u16, u16)) -> Result<Option<DeviceInfo>, Error> {
    Ok(nusb::list_devices().wait()?.find(|d| d.vendor_id() == vendor && d.product_id() == product))
}

fn wait_for(id: (u16, u16)) -> Result<DeviceInfo, Error> {
    for _ in 0..ENUM_POLLS {
        sleep(ENUM_POLL);
        if let Some(info) = find(id)? {
            return Ok(info);
        }
    }
    Err(Error::NoReenumeration)
}

/// Holds the 8051 in reset, writes every record into its RAM, and lets it go; it drops off the bus and comes back as the spectrometer.
fn load_firmware(fx2: &Device) -> Result<(), Error> {
    fx2.set_configuration(1).wait()?;
    let load = |value: u16, data: &[u8]| fx2.control_out(ControlOut { control_type: ControlType::Vendor, recipient: Recipient::Device, request: FX2_LOAD, value, index: 0, data }, CONTROL_TIMEOUT).wait();
    load(FX2_CPUCS, &[1])?;
    for &(address, bytes) in firmware::RECORDS {
        load(address, bytes)?;
    }
    load(FX2_CPUCS, &[0])?;
    Ok(())
}

/// A calibration coefficient: the EEPROM stores each as decimal text at the head of its block.
fn number(block: &[u8; 32]) -> Result<f64, Error> {
    let text = std::str::from_utf8(block).map_err(|_| Error::Eeprom("coefficient block is not text"))?;
    text.split_whitespace().next().and_then(|t| t.parse().ok()).ok_or(Error::Eeprom("coefficient is not a number"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every packet the vendor driver sent for these integrations, as traced.
    #[test]
    fn config_matches_vendor() {
        let traced: &[(u32, XTiming, [u8; 4])] = &[
            (1, XTiming::Three, [0x00, 0x02, 0x10, 0x0f]),
            (2, XTiming::Three, [0x00, 0x02, 0x10, 0x1f]),
            (10, XTiming::Three, [0x00, 0x0a, 0x10, 0x1f]),
            (20, XTiming::Two, [0x00, 0x14, 0x08, 0x1f]),
            (200, XTiming::One, [0x00, 0xc8, 0x04, 0x1f]),
            (65000, XTiming::Three, [0xfd, 0xe8, 0x10, 0x1f]),
            (65001, XTiming::Three, [0x1f, 0xbd, 0x10, 0xfa]),
            (65535, XTiming::Three, [0x1f, 0xff, 0x10, 0xfa]),
            (65544, XTiming::Three, [0x20, 0x01, 0x10, 0xfa]),
            (65836, XTiming::Three, [0x20, 0x25, 0x10, 0xfa]),
            (262144, XTiming::Three, [0x80, 0x00, 0x10, 0xfa]),
            (497999, XTiming::Three, [0xf3, 0x29, 0x10, 0xfa]),
        ];
        for &(ms, xt, packet) in traced {
            assert_eq!(config_packet(ms, xt).unwrap(), packet, "{ms} ms");
        }
        assert!(config_packet(0, XTiming::Three).is_err());
        assert!(config_packet(498000, XTiming::Three).is_err());
    }

    #[test]
    fn firmware_size() {
        assert_eq!(firmware::RECORDS.len(), 324);
        assert_eq!(firmware::RECORDS.iter().map(|(_, b)| b.len()).sum::<usize>(), 3971);
    }
}
