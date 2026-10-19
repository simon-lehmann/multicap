//! Where `Present` lives inside d3d9.dll, remembered between runs.
//!
//! Finding it means reading a real vtable, which means having a device. A game
//! in exclusive fullscreen owns the adapter, so a device created inside it fails
//! with D3DERR_DEVICELOST and the vtable cannot be read at all. The way round
//! that is to not need a device: record the function's *offset* within the
//! module once, while some client is windowed, and afterwards add it to
//! whatever base d3d9.dll is loaded at.
//!
//! An offset is stable for a given d3d9.dll; the base is not, because of address
//! space layout randomisation, which is why this stores the offset and resolves
//! the base fresh in every process.

use windows::core::s;
use windows::Win32::System::LibraryLoader::GetModuleHandleA;

use crate::{PresentFn, SwapPresentFn};

fn cache_path() -> std::path::PathBuf {
    std::env::temp_dir().join("multicap-d3d9-offsets.txt")
}

/// The loaded base of d3d9.dll in this process, or `None` if the game is not a
/// Direct3D 9 game at all.
fn d3d9_base() -> Option<usize> {
    unsafe { GetModuleHandleA(s!("d3d9.dll")).ok() }.map(|h| h.0 as usize)
}

/// Offsets saved by an earlier run, resolved against this process's d3d9.dll.
pub fn load() -> Option<(PresentFn, Option<SwapPresentFn>)> {
    let base = d3d9_base()?;
    let text = std::fs::read_to_string(cache_path()).ok()?;

    let mut present = None;
    let mut swap = None;
    for line in text.lines() {
        let (key, value) = line.split_once('=')?;
        let offset: usize = value.trim().parse().ok()?;
        match key.trim() {
            "present" => present = Some(offset),
            "swapchain_present" => swap = Some(offset),
            _ => {}
        }
    }

    let present = present?;
    unsafe {
        Some((
            std::mem::transmute::<usize, PresentFn>(base + present),
            swap.map(|o| std::mem::transmute::<usize, SwapPresentFn>(base + o)),
        ))
    }
}

/// Record where these functions sit relative to the module, for next time.
pub fn save(present: PresentFn, swap: Option<SwapPresentFn>) {
    let Some(base) = d3d9_base() else { return };
    let mut text = format!("present={}\n", present as usize - base);
    if let Some(sp) = swap {
        text.push_str(&format!("swapchain_present={}\n", sp as usize - base));
    }
    let _ = std::fs::write(cache_path(), text);
}
