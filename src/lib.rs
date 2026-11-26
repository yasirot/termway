//! termway: show headless Wayland apps inside a terminal via the Kitty graphics protocol.

#[macro_use]
pub mod log;

pub mod app;
pub mod base64;
pub mod cli;
pub mod compositor;
pub mod frame;
pub mod geom;
pub mod input;
pub mod keymap;
pub mod kitty;
pub mod lru;
pub mod par;
pub mod png;
pub mod render;
pub mod scroll;
pub mod signals;
pub mod sim;
pub mod stats;
pub mod term;
pub mod vt;
pub mod wl;
pub mod writer;
pub mod zlib;
