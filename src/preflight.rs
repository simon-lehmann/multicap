//! Refuse to record nothing.
//!
//! Windows Graphics Capture takes its picture from the desktop compositor, and
//! the compositor only composes when the machine has an active display output.
//! With every monitor off or unplugged there is no output: capture starts
//! cleanly, reports no error, and delivers zero frames. On a headless box that
//! is the single most expensive failure available, because you find out after
//! the take.
//!
//! So we check first and say plainly what is wrong and how to fix it.

use windows::core::Interface;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1};
use windows::Win32::Graphics::Gdi::{
    GetDC, GetDeviceCaps, ReleaseDC, DESKTOPHORZRES, DESKTOPVERTRES,
};

use crate::error::Result;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DisplayState {
    /// Display outputs across all adapters. Zero means nothing can be captured.
    pub outputs: usize,
    pub width: i32,
    pub height: i32,
    pub ok: bool,
    pub note: String,
}

pub fn displays() -> Result<DisplayState> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let mut outputs = 0usize;
    let mut i = 0u32;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(i) } {
        let adapter: IDXGIAdapter1 = adapter.cast()?;
        let mut j = 0u32;
        while unsafe { adapter.EnumOutputs(j) }.is_ok() {
            outputs += 1;
            j += 1;
        }
        i += 1;
    }

    let (width, height) = unsafe {
        let dc = GetDC(None);
        let w = GetDeviceCaps(dc, DESKTOPHORZRES);
        let h = GetDeviceCaps(dc, DESKTOPVERTRES);
        ReleaseDC(None, dc);
        (w, h)
    };

    let (ok, note) = if outputs == 0 {
        (
            false,
            "No display output is active, so Windows is not composing a \
desktop and screen capture will record zero frames without reporting an error. \
Plug in an HDMI/DisplayPort dummy plug, or install a virtual display driver. \
Note that a monitor switched off at the power button drops off the bus and \
counts as unplugged."
                .to_string(),
        )
    } else if width <= 1024 && height <= 768 {
        (
            false,
            format!(
                "The desktop is {width}x{height}, which is the fallback Windows uses \
when no real display mode is available. Captures will be this size. Give the \
machine a dummy plug or a virtual display and set the mode you want to record."
            ),
        )
    } else {
        (
            true,
            format!("{outputs} display output(s), desktop {width}x{height}"),
        )
    };

    Ok(DisplayState {
        outputs,
        width,
        height,
        ok,
        note,
    })
}
