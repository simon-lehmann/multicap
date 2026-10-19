//! Reading frames from a graphics hook living inside a game.
//!
//! This is the recorder's half of `crates/ipc`. The hook writes a frame into one
//! of two slots and publishes its index; we take the published one. Both sides
//! stamp a sequence number, so a slot overwritten while we were copying it is
//! detected and the frame dropped rather than delivered half-torn.
//!
//! Frames arrive as system memory here — that is the price of Direct3D 9, which
//! has no shared textures — so they are uploaded to the GPU before encoding.

use multicap_ipc::{frame_event_name, mapping_name, FrameInfo, Header, Namespace, MAGIC, SLOTS};
use windows::core::HSTRING;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::System::Memory::OpenFileMappingW;
use windows::Win32::System::Memory::{
    MapViewOfFile, UnmapViewOfFile, FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS,
};
use windows::Win32::System::Performance::QueryPerformanceFrequency;
use windows::Win32::System::Threading::{
    OpenEventW, WaitForSingleObject, SYNCHRONIZATION_ACCESS_RIGHTS,
};

use crate::d3d::Device;
use crate::error::{Error, Result};
use crate::timing::TICKS_PER_SEC;

pub struct HookSource {
    mapping: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    frame_event: HANDLE,
    slot_bytes: u32,
    qpc_hz: i64,
    pub namespace: Namespace,
    /// Last sequence handed out, so the same frame is not encoded twice.
    last_sequence: u64,
    /// Scratch for one frame, so the hot loop does not allocate.
    scratch: Vec<u8>,
    pub stats: ReadStats,
}

/// Why frames did not arrive. Guessing at this from a frame count alone wastes
/// far more time than counting it.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct ReadStats {
    /// The wait returned without the hook signalling a frame.
    pub timeouts: u64,
    /// Woke up, but the published slot held a frame already delivered.
    pub stale: u64,
    /// The hook overwrote the slot while it was being copied.
    pub torn: u64,
    pub delivered: u64,
}

impl HookSource {
    /// Attach to the hook inside `pid`.
    ///
    /// A hook that only managed a `Local\` mapping is invisible from another
    /// Windows session, which is the usual reason this fails.
    pub fn open(pid: u32) -> Result<Self> {
        let mut opened = None;
        for ns in Namespace::candidates() {
            let m = unsafe {
                OpenFileMappingW(
                    FILE_MAP_READ.0,
                    false,
                    &HSTRING::from(mapping_name(ns, pid)),
                )
            };
            if let Ok(m) = m {
                opened = Some((ns, m));
                break;
            }
        }
        let (namespace, mapping) = opened.ok_or_else(|| {
            Error::msg(format!(
                "no multicap hook found in pid {pid}. Either nothing is injected, or the hook is \
in another Windows session and could only create a Local mapping, which cannot be read from here."
            ))
        })?;

        let view = unsafe { MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0) };
        if view.Value.is_null() {
            unsafe { CloseHandle(mapping)? };
            return Err(Error::msg("could not map the hook's shared memory"));
        }

        let header = unsafe { &*(view.Value as *const Header) };
        if header.magic != MAGIC {
            return Err(Error::msg(format!(
                "shared memory is not multicap's (magic {:#x})",
                header.magic
            )));
        }
        if header.protocol != multicap_ipc::PROTOCOL_VERSION {
            return Err(Error::msg(format!(
                "hook speaks protocol {} and this recorder speaks {}: rebuild them together",
                header.protocol,
                multicap_ipc::PROTOCOL_VERSION
            )));
        }

        let frame_event = unsafe {
            OpenEventW(
                SYNCHRONIZATION_ACCESS_RIGHTS(0x0010_0000), // SYNCHRONIZE
                false,
                &HSTRING::from(frame_event_name(namespace, pid)),
            )?
        };

        let mut hz = 0i64;
        unsafe { QueryPerformanceFrequency(&mut hz)? };

        Ok(Self {
            mapping,
            view,
            frame_event,
            slot_bytes: header.slot_bytes,
            qpc_hz: hz,
            namespace,
            last_sequence: 0,
            scratch: Vec::new(),
            stats: ReadStats::default(),
        })
    }

    pub fn header(&self) -> &Header {
        unsafe { &*(self.view.Value as *const Header) }
    }

    fn slot(&self, i: usize) -> *const u8 {
        unsafe { (self.view.Value as *const u8).add(Header::size() + self.slot_bytes as usize * i) }
    }

    /// Wait for the next frame the hook publishes.
    ///
    /// `Ok(None)` means nothing arrived within the timeout, which is normal for
    /// a game that is not drawing. The returned time is in 100 ns ticks, the
    /// same unit Media Foundation and the rest of this crate use.
    pub fn next_frame(&mut self, timeout_ms: u32) -> Result<Option<(FrameInfo, &[u8], i64)>> {
        if unsafe { WaitForSingleObject(self.frame_event, timeout_ms) } != WAIT_OBJECT_0 {
            self.stats.timeouts += 1;
            return Ok(None);
        }
        let header = self.header();
        let slot = header.ready_slot;
        if slot as usize >= SLOTS {
            self.stats.stale += 1;
            return Ok(None);
        }
        let info = header.info[slot as usize];
        if info.sequence == self.last_sequence || info.width == 0 {
            self.stats.stale += 1;
            return Ok(None);
        }

        let bytes = info.pitch as usize * info.height as usize;
        if bytes > self.slot_bytes as usize {
            return Err(Error::msg("hook reported a frame larger than its own slot"));
        }
        self.scratch.resize(bytes, 0);
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.slot(slot as usize),
                self.scratch.as_mut_ptr(),
                bytes,
            )
        };

        // If the hook published again while we copied, this frame is torn.
        if self.header().info[slot as usize].sequence != info.sequence {
            self.stats.torn += 1;
            return Ok(None);
        }
        self.last_sequence = info.sequence;
        self.stats.delivered += 1;

        // The hook stamps raw performance-counter ticks; everything downstream
        // works in 100 ns units.
        let time_100ns = (info.qpc as i128 * TICKS_PER_SEC as i128 / self.qpc_hz as i128) as i64;
        Ok(Some((info, &self.scratch[..bytes], time_100ns)))
    }
}

impl Drop for HookSource {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(self.view);
            let _ = CloseHandle(self.frame_event);
            let _ = CloseHandle(self.mapping);
        }
    }
}

/// A GPU texture that frames from the hook are uploaded into before encoding.
pub struct Upload {
    pub texture: ID3D11Texture2D,
    width: u32,
    height: u32,
}

impl Upload {
    pub fn new(dev: &Device, width: u32, height: u32) -> Result<Self> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture: Option<ID3D11Texture2D> = None;
        unsafe {
            dev.device
                .CreateTexture2D(&desc, None, Some(&mut texture))?
        };
        Ok(Self {
            texture: texture.ok_or_else(|| Error::msg("no upload texture"))?,
            width,
            height,
        })
    }

    pub fn matches(&self, width: u32, height: u32) -> bool {
        self.width == width && self.height == height
    }

    /// Copy one frame's pixels onto the GPU. `pitch` is the source's row stride,
    /// which is not width * 4: the driver pads rows.
    pub fn write(&self, dev: &Device, pixels: &[u8], pitch: u32) -> Result<()> {
        unsafe {
            dev.context.UpdateSubresource(
                &self.texture,
                0,
                None,
                pixels.as_ptr() as *const core::ffi::c_void,
                pitch,
                0,
            );
        }
        Ok(())
    }
}
