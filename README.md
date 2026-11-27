# StellarNet

A Rust driver and a live spectral tool for StellarNet USB spectrometers. No Python, no libusb: the driver talks to the instrument over the kernel's USB interface through [nusb](https://crates.io/crates/nusb).

Tested with a BLACK-Comet CXR-SR-X (detector type 1, 2048 pixels) on Linux. Other StellarNet models share the protocol, but the driver refuses EEPROM contents it has not been checked against rather than guess.

## What's here

- **`driver/`**: the `stellarnet` crate. It uploads the spectrometer's firmware when the device first appears, reads the wavelength calibration from its EEPROM, and reads frames at any integration from 1 ms to 497,999 ms. A long exposure can be abandoned the moment you change a setting. The protocol was traced from StellarNet's own driver's USB traffic.
- **`src/`**: `stellarnet`, a live spectrum in a [fluor](https://github.com/nickspiker/fluor) window, and `stellarnet-csv`, which turns saved spectra into CSV.
- **`patchscan/`**: measures a printed colour target patch by patch. It needs the `chameleon` crate, which is not public, so it won't build outside its author's machine.

## Building

`fluor` and [`vsf`](https://github.com/nickspiker/vsf) are path dependencies, so clone them beside this repository:

```
fluor/
vsf/
StellarNet/
```

Then, in `StellarNet/`:

```
cargo build --release
```

### USB access on Linux

The spectrometer first appears as a blank Cypress FX2 (`04b4:8613`). After the driver loads its firmware it comes back as StellarNet's own device (`0bd7:a012`). To use it without root, add a udev rule such as `/etc/udev/rules.d/60-stellarnet.rules`:

```
SUBSYSTEM=="usb", ATTR{idVendor}=="04b4", ATTR{idProduct}=="8613", MODE="0666"
SUBSYSTEM=="usb", ATTR{idVendor}=="0bd7", MODE="0666"
```

Note that `04b4:8613` is Cypress's generic development ID, so the driver will load spectrometer firmware into any blank FX2 board it finds if no StellarNet is already running.

## The spectral tool

```
./target/release/stellarnet [--dir FOLDER]
```

- **Plot:** the spectrum over wavelength. Wheel zooms, drag pans, and Home or a right click returns to your saved range (Shift+Home shows the whole instrument range).
- **Quantities:**
  - **Counts:** raw, or less the dark once one is taken.
  - **Ratio:** % of a reference (transmittance or reflectance).
  - **Emission:** a ratio against StellarNet's SL1 lamp, times that lamp's spectrum.
- **Dark and Reference** keep the frame on screen. Hover either button to see what was kept.
- **Baseline drift:** each frame records the level of the detector's masked pixels, and the drift since the dark is taken off every dark-subtracted value.
- **Saving:** spectra are saved as VSF collections in the folder the tool was started from, or `--dir`. Each spectrum keeps its raw frame, dark and reference, so any quantity can be recomputed later.
- **Export:** `stellarnet-csv <collection.vsf> [--quantity counts|ratio|emission] [--nm LO:HI:STEP]` writes CSV to stdout.

## License

Licensed under either of MIT ([LICENSE-MIT](LICENSE-MIT)) or Apache-2.0 ([LICENSE-APACHE](LICENSE-APACHE)), at your option.

The exception is `driver/src/firmware.rs`: that is StellarNet's spectrometer firmware, Copyright 2024 StellarNet, Inc., redistributed under the Apache License 2.0 that StellarNet's driver carries. See [NOTICE](NOTICE).
