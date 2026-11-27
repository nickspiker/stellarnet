//! The spectral tool's maths and files, shared by the window (`stellarnet`, src/main.rs) and the CSV exporter (`stellarnet-csv`, src/bin/stellarnet-csv.rs), so both give the same numbers.

pub mod collection;
pub mod colour;
pub mod quantity;
pub mod settings;
pub mod sl1;

pub use quantity::{Frame, Quantity};
pub use vsf::EagleTime;
