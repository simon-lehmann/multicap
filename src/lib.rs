//! Record many Windows application windows at once, each to its own file, with
//! each application's own audio, on the GPU's hardware encoder.
//!
//! See the README for what this is for and what it cannot do. The short version:
//! Windows Graphics Capture takes each window's own picture even when the window
//! is covered by another, Media Foundation hands the texture to the GPU's encoder
//! without it ever entering system memory, and WASAPI process loopback records
//! each application's sound separately. One process, no ffmpeg, no OBS.

pub mod error;
pub mod timing;

#[cfg(windows)]
pub mod audio;
#[cfg(windows)]
pub mod capture;
#[cfg(windows)]
pub mod d3d;
#[cfg(windows)]
pub mod encode;
#[cfg(windows)]
pub mod hooksrc;
#[cfg(windows)]
pub mod preflight;
#[cfg(windows)]
pub mod session;

pub use error::{Error, Result};
