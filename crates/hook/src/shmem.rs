//! The hook's side of the shared frame buffer.

use std::ffi::c_void;

use multicap_ipc::{mapping_name, FrameInfo, Header, HookError, MAGIC, PROTOCOL_VERSION, SLOTS};
use windows::core::HSTRING;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
    MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};
use windows::Win32::System::Threading::{CreateEventW, SetEvent};

pub struct Shared {
    pid: u32,
    _mapping: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    pub frame_event: HANDLE,
    slot_bytes: u32,
}

// The hook thread creates this and the render thread uses it. Both are inside
// the game process and the buffer is plain memory with a single writer.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

impl Shared {
    /// Reserve room for `slot_bytes` of pixels per slot. Sized for the largest
    /// frame we expect rather than grown later: resizing a mapping that another
    /// process has open is not worth the complexity.
    pub fn create(pid: u32, slot_bytes: u32) -> windows::core::Result<Self> {
        let total = Header::size() + slot_bytes as usize * SLOTS;
        // Widened on purpose: this file is also compiled 32-bit, where `usize`
        // is 32 bits and shifting one by 32 is an overflow, not a zero.
        let total64 = total as u64;
        let mapping = unsafe {
            CreateFileMappingW(
                windows::Win32::Foundation::INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE,
                (total64 >> 32) as u32,
                total64 as u32,
                &HSTRING::from(mapping_name(pid)),
            )?
        };
        let view = unsafe { MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, total) };
        if view.Value.is_null() {
            unsafe { CloseHandle(mapping)? };
            return Err(windows::core::Error::from_win32());
        }

        let frame_event = unsafe {
            CreateEventW(
                None,
                false,
                false,
                &HSTRING::from(multicap_ipc::frame_event_name(pid)),
            )?
        };

        let s = Self {
            pid,
            _mapping: mapping,
            view,
            frame_event,
            slot_bytes,
        };
        // Zero the header before anyone can read a half-written one.
        unsafe {
            std::ptr::write_bytes(s.view.Value as *mut u8, 0, Header::size());
            let h = s.header();
            (*h).magic = MAGIC;
            (*h).protocol = PROTOCOL_VERSION;
            (*h).slot_bytes = slot_bytes;
            (*h).ready_slot = u32::MAX;
        }
        Ok(s)
    }

    pub fn header(&self) -> *mut Header {
        self.view.Value as *mut Header
    }

    fn slot(&self, i: usize) -> *mut u8 {
        unsafe { (self.view.Value as *mut u8).add(Header::size() + self.slot_bytes as usize * i) }
    }

    pub fn set_active(&self, active: bool) {
        unsafe { (*self.header()).active = active as u32 }
    }

    pub fn fail(&self, err: HookError, hr: i32) {
        unsafe {
            (*self.header()).last_error = err as u32;
            (*self.header()).last_hresult = hr;
        }
    }

    /// Copy one frame in and publish it. Returns false if it did not fit.
    ///
    /// Called from the game's render thread, so it does the least it can: one
    /// memcpy and two stores. No allocation, no locking, nothing that could
    /// make a game stutter.
    pub fn publish(&self, info: FrameInfo, pixels: *const u8, bytes: usize) -> bool {
        crate::dump::first_frame(self.pid, pixels, info.width, info.height, info.pitch);
        if bytes > self.slot_bytes as usize {
            self.fail(HookError::FrameTooLarge, 0);
            return false;
        }
        let h = self.header();
        unsafe {
            // Write to whichever slot was not published last.
            let last = (*h).ready_slot;
            let next = if last == 0 { 1usize } else { 0usize };
            std::ptr::copy_nonoverlapping(pixels, self.slot(next), bytes);
            (*h).info[next] = info;
            (*h).ready_slot = next as u32;
            (*h).captured += 1;
            let _ = SetEvent(self.frame_event);
        }
        true
    }

    pub fn note_dropped(&self) {
        unsafe { (*self.header()).dropped += 1 }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(self.view);
            let _ = CloseHandle(self.frame_event);
            let _ = CloseHandle(self._mapping);
        }
    }
}

#[allow(dead_code)]
fn unused(_: *mut c_void) {}
