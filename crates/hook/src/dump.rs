//! Write one captured frame to a bitmap, once, for proof and for debugging.
//!
//! A frame counter only tells you frames were taken, not that they contain the
//! game. This writes the first frame of a session next to the status file, so
//! the picture can be looked at from another machine.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

static DUMPED: AtomicBool = AtomicBool::new(false);

pub fn first_frame(pid: u32, pixels: *const u8, width: u32, height: u32, pitch: u32) {
    if DUMPED.swap(true, Ordering::SeqCst) {
        return;
    }
    let path = std::env::temp_dir().join(format!("multicap-hook-{pid}.bmp"));
    let Ok(mut f) = std::fs::File::create(path) else {
        return;
    };

    let row = width as usize * 4;
    let size = 54 + row * height as usize;

    let mut header = Vec::with_capacity(54);
    header.extend_from_slice(b"BM");
    header.extend_from_slice(&(size as u32).to_le_bytes());
    header.extend_from_slice(&0u32.to_le_bytes());
    header.extend_from_slice(&54u32.to_le_bytes());
    header.extend_from_slice(&40u32.to_le_bytes()); // BITMAPINFOHEADER
    header.extend_from_slice(&(width as i32).to_le_bytes());
    header.extend_from_slice(&(height as i32).to_le_bytes());
    header.extend_from_slice(&1u16.to_le_bytes());
    header.extend_from_slice(&32u16.to_le_bytes());
    header.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    header.extend_from_slice(&((row * height as usize) as u32).to_le_bytes());
    header.extend_from_slice(&[0u8; 16]);
    if f.write_all(&header).is_err() {
        return;
    }

    // D3DFMT_X8R8G8B8 is B,G,R,X in memory, which is what a 32-bit BMP wants.
    // BMP rows run bottom to top, so the frame is written in reverse.
    for y in (0..height as usize).rev() {
        let line = unsafe { std::slice::from_raw_parts(pixels.add(y * pitch as usize), row) };
        if f.write_all(line).is_err() {
            return;
        }
    }
}
