//! A status file beside the shared memory.
//!
//! The frame buffer lives in the `Local\\` object namespace, which is private to
//! one Windows session: a tool run over SSH lands in session 0 and cannot see a
//! hook living in a game in session 1. A file can be read from anywhere, by a
//! person as easily as by a program, which makes it the difference between
//! "the hook did not work" and knowing why.
//!
//! Written once at install and then once a second — never per frame.

use std::io::Write;
use std::sync::atomic::Ordering;

use multicap_ipc::HookError;

use crate::shmem::Shared;

pub fn path(pid: u32) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("multicap-hook-{pid}.json"))
}

/// Write the hook's state now, and keep writing it while the process lives.
pub fn publish(pid: u32, shared: &'static Shared) {
    write_once(pid, shared);
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
        write_once(pid, shared);
    });
}

fn write_once(pid: u32, shared: &Shared) {
    let h = shared.header();
    let (active, captured, dropped, err, hr, info, pc, spc) = unsafe {
        let slot = if (*h).ready_slot == u32::MAX {
            0
        } else {
            (*h).ready_slot as usize
        };
        (
            (*h).active,
            (*h).captured,
            (*h).dropped,
            (*h).last_error,
            (*h).last_hresult,
            (*h).info[slot],
            (*h).present_calls,
            (*h).swap_present_calls,
        )
    };
    let text = format!(
        concat!(
            "{{\"pid\": {}, \"active\": {}, \"captured\": {}, \"dropped\": {}, ",
            "\"error\": \"{}\", \"hresult\": \"{:#010x}\", \"width\": {}, \"height\": {}, ",
            "\"pitch\": {}, \"d3dformat\": {}, \"hooked\": {}, ",
            "\"present_calls\": {}, \"swapchain_present_calls\": {}}}\n"
        ),
        pid,
        active == 1,
        captured,
        dropped,
        HookError::describe(err),
        hr,
        info.width,
        info.height,
        info.pitch,
        info.format,
        crate::TRAMPOLINE.load(Ordering::SeqCst) != 0,
        pc,
        spc,
    );
    if let Ok(mut f) = std::fs::File::create(path(pid)) {
        let _ = f.write_all(text.as_bytes());
    }
}
