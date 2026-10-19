//! A Direct3D 9 graphics hook.
//!
//! This library is injected into a game and runs inside it. It patches the
//! function the game calls to hand a finished frame to the display, copies the
//! back buffer out through shared memory, and then lets the original call
//! proceed.
//!
//! The point of doing it this way rather than capturing the screen: the frame is
//! taken from the game's own swap chain, *before* the desktop compositor. The
//! machine therefore needs no display output at all, which is the one thing no
//! screen-capture API can work around.
//!
//! Two rules govern everything here, because this code runs inside somebody
//! else's render thread:
//!
//! 1. **Never panic across the boundary.** Unwinding out of an `extern "system"`
//!    function is undefined behaviour, so the detour catches everything.
//! 2. **Never block.** A contended lock or a slow copy shows up as a stutter in
//!    the game. If this cannot get a frame out instantly it skips that frame and
//!    counts it.

#![cfg(windows)]

mod shmem;

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use multicap_ipc::{FrameInfo, HookError};
use retour::RawDetour;
use windows::core::Interface;
use windows::Win32::Foundation::{BOOL, HINSTANCE, HWND, RECT, TRUE};
use windows::Win32::Graphics::Direct3D9::*;
use windows::Win32::System::Performance::QueryPerformanceCounter;
use windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::GetDesktopWindow;

use shmem::Shared;

mod dump;
mod status;

/// `IDirect3DDevice9::Present` is entry 17 of the COM vtable. The index is
/// fixed by the interface's ABI — it cannot change without breaking every
/// Direct3D 9 application ever shipped — so it is safe to rely on, while the
/// *address* is read at run time from a real vtable rather than hardcoded.
const VTBL_PRESENT: usize = 17;

/// `IDirect3DSwapChain9::Present` is entry 3 of its own interface. A game that
/// renders through an explicit swap chain never touches the device's Present,
/// so both are hooked and whichever fires is the one that matters.
const VTBL_SWAPCHAIN_PRESENT: usize = 3;

type PresentFn = unsafe extern "system" fn(
    *mut core::ffi::c_void,
    *const RECT,
    *const RECT,
    HWND,
    *const core::ffi::c_void,
) -> windows::core::HRESULT;

type SwapPresentFn = unsafe extern "system" fn(
    *mut core::ffi::c_void,
    *const RECT,
    *const RECT,
    HWND,
    *const core::ffi::c_void,
    u32,
) -> windows::core::HRESULT;

static SHARED: OnceLock<Shared> = OnceLock::new();
static DETOUR: OnceLock<RawDetour> = OnceLock::new();
static TRAMPOLINE: AtomicUsize = AtomicUsize::new(0);
static SWAP_DETOUR: OnceLock<RawDetour> = OnceLock::new();
static SWAP_TRAMPOLINE: AtomicUsize = AtomicUsize::new(0);

/// Surfaces kept between frames rather than rebuilt sixty times a second.
///
/// Two of them, because the game renders with multisampling on (WoW defaults to
/// 8x here) and a multisampled surface cannot be read back directly — that call
/// fails with D3DERR_INVALIDCALL. The frame is first resolved into a plain
/// render target of the same size, and that is what goes to system memory.
struct Readback {
    /// Plain, non-multisampled copy of the back buffer. `None` when the back
    /// buffer is not multisampled and can be read directly.
    resolve: Option<IDirect3DSurface9>,
    sysmem: IDirect3DSurface9,
    width: u32,
    height: u32,
    format: D3DFORMAT,
}
unsafe impl Send for Readback {}

static READBACK: Mutex<Option<Readback>> = Mutex::new(None);

#[no_mangle]
#[allow(non_snake_case)]
pub extern "system" fn DllMain(_dll: HINSTANCE, reason: u32, _reserved: *mut ()) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        // Almost nothing is legal inside DllMain (the loader lock is held), so
        // all real work happens on a thread of our own.
        std::thread::spawn(|| {
            let _ = catch_unwind(install);
        });
    }
    TRUE
}

fn install() {
    let pid = unsafe { GetCurrentProcessId() };

    // Sized once, because a mapping cannot be resized while another process
    // holds it open. 1440p at 4 bytes a pixel covers every resolution we record
    // at with room to spare, and costs ~30 MB of the game's address space —
    // which matters, since a 32-bit process has little of it. A bigger frame is
    // reported as FrameTooLarge rather than silently truncated; raise this with
    // MULTICAP_SLOT_BYTES if you record larger.
    let slot_bytes: u32 = std::env::var("MULTICAP_SLOT_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2560 * 1440 * 4);
    let shared = match Shared::create(pid, slot_bytes) {
        Ok(s) => s,
        Err(_) => return,
    };
    let shared = SHARED.get_or_init(|| shared);

    let (present, swap_present) = match find_present() {
        Ok(p) => p,
        Err((e, hr)) => {
            shared.fail(e, hr);
            status::publish(pid, shared);
            return;
        }
    };

    let detour = match unsafe { RawDetour::new(present as *const (), present_detour as *const ()) }
    {
        Ok(d) => d,
        Err(_) => {
            shared.fail(HookError::HookFailed, 0);
            status::publish(pid, shared);
            return;
        }
    };
    if unsafe { detour.enable() }.is_err() {
        shared.fail(HookError::HookFailed, 0);
        status::publish(pid, shared);
        return;
    }
    TRAMPOLINE.store(detour.trampoline() as *const () as usize, Ordering::SeqCst);
    let _ = DETOUR.set(detour);

    // Best effort: if the game turns out to use the device's Present, this one
    // simply never fires.
    if let Some(sp) = swap_present {
        if let Ok(d) = unsafe { RawDetour::new(sp as *const (), swap_present_detour as *const ()) }
        {
            if unsafe { d.enable() }.is_ok() {
                SWAP_TRAMPOLINE.store(d.trampoline() as *const () as usize, Ordering::SeqCst);
                let _ = SWAP_DETOUR.set(d);
            }
        }
    }

    shared.set_active(true);
    status::publish(pid, shared);
}

/// Find the real address of `Present` by making a throwaway device and reading
/// its vtable. Every `IDirect3DDevice9` from the same `d3d9.dll` shares one
/// vtable, so the address read here is the one the game's own device calls —
/// which is why patching it captures the game without ever touching its device.
fn find_present() -> Result<(PresentFn, Option<SwapPresentFn>), (HookError, i32)> {
    unsafe {
        let d3d = Direct3DCreate9(D3D_SDK_VERSION).ok_or((HookError::NoD3d9, 0))?;

        let hwnd = GetDesktopWindow();
        let mut pp = D3DPRESENT_PARAMETERS {
            Windowed: TRUE,
            SwapEffect: D3DSWAPEFFECT_DISCARD,
            BackBufferFormat: D3DFMT_UNKNOWN,
            hDeviceWindow: hwnd,
            ..Default::default()
        };
        let mut device: Option<IDirect3DDevice9> = None;
        let hr = d3d.CreateDevice(
            D3DADAPTER_DEFAULT,
            D3DDEVTYPE_HAL,
            hwnd,
            D3DCREATE_SOFTWARE_VERTEXPROCESSING as u32,
            &mut pp,
            &mut device,
        );
        if let Err(e) = hr {
            return Err((HookError::NoDummyDevice, e.code().0));
        }
        let device = device.ok_or((HookError::NoDummyDevice, 0))?;

        let vtbl = *(device.as_raw() as *const *const usize);
        let addr = *vtbl.add(VTBL_PRESENT);

        // The device's own swap chain gives us the other candidate's vtable.
        let swap = device.GetSwapChain(0).ok().map(|sc| {
            let svtbl = *(sc.as_raw() as *const *const usize);
            std::mem::transmute::<usize, SwapPresentFn>(*svtbl.add(VTBL_SWAPCHAIN_PRESENT))
        });

        // The throwaway device goes away here; the vtables it pointed at live in
        // d3d9.dll and stay.
        Ok((std::mem::transmute::<usize, PresentFn>(addr), swap))
    }
}

unsafe extern "system" fn present_detour(
    device: *mut core::ffi::c_void,
    src: *const RECT,
    dst: *const RECT,
    hwnd: HWND,
    dirty: *const core::ffi::c_void,
) -> windows::core::HRESULT {
    // Anything that goes wrong in here is this frame's problem, never the
    // game's: swallow it, note it, and let the game present as normal.
    if let Some(shared) = SHARED.get() {
        // Counted first, before anything that could fail: this number alone
        // says whether the detour is on the function the game actually calls.
        unsafe { (*shared.header()).present_calls += 1 };
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if let (Some(shared), Some(dev)) =
            (SHARED.get(), IDirect3DDevice9::from_raw_borrowed(&device))
        {
            if let Err((e, hr)) = grab(shared, dev) {
                shared.fail(e, hr);
            }
        }
    }));

    let original: PresentFn = std::mem::transmute(TRAMPOLINE.load(Ordering::SeqCst));
    original(device, src, dst, hwnd, dirty)
}

fn grab(shared: &Shared, device: &IDirect3DDevice9) -> Result<(), (HookError, i32)> {
    unsafe {
        let back = device
            .GetBackBuffer(0, 0, D3DBACKBUFFER_TYPE_MONO)
            .map_err(|e| (HookError::BackBufferFailed, e.code().0))?;
        let mut desc = D3DSURFACE_DESC::default();
        back.GetDesc(&mut desc)
            .map_err(|e| (HookError::BackBufferFailed, e.code().0))?;

        // Skipping a frame beats blocking a game's render thread on our lock.
        let Ok(mut guard) = READBACK.try_lock() else {
            shared.note_dropped();
            return Ok(());
        };

        let stale = guard.as_ref().is_none_or(|r| {
            r.width != desc.Width || r.height != desc.Height || r.format != desc.Format
        });
        if stale {
            *guard = Some(make_readback(device, &desc)?);
        }
        let readback = guard.as_ref().expect("read-back surfaces just created");

        // Multisampled frames go via the plain intermediate; others go straight.
        let source = match &readback.resolve {
            Some(resolve) => {
                device
                    .StretchRect(
                        &back,
                        std::ptr::null(),
                        resolve,
                        std::ptr::null(),
                        D3DTEXF_NONE,
                    )
                    .map_err(|e| (HookError::ResolveFailed, e.code().0))?;
                resolve
            }
            None => &back,
        };

        // The one unavoidable cost of the Direct3D 9 path: the frame comes back
        // across the bus into system memory. Plain D3D9 has no shared textures,
        // so there is nowhere else to put it.
        device
            .GetRenderTargetData(source, &readback.sysmem)
            .map_err(|e| (HookError::ReadBackFailed, e.code().0))?;

        let mut locked = D3DLOCKED_RECT::default();
        readback
            .sysmem
            .LockRect(&mut locked, std::ptr::null(), D3DLOCK_READONLY as u32)
            .map_err(|e| (HookError::LockFailed, e.code().0))?;

        let mut qpc = 0i64;
        let _ = QueryPerformanceCounter(&mut qpc);
        let bytes = locked.Pitch as usize * desc.Height as usize;
        shared.publish(
            FrameInfo {
                width: desc.Width,
                height: desc.Height,
                pitch: locked.Pitch as u32,
                format: desc.Format.0 as u32,
                qpc,
                sequence: 0,
            },
            locked.pBits as *const u8,
            bytes,
        );

        let _ = readback.sysmem.UnlockRect();
        Ok(())
    }
}

/// Build the surfaces a frame is copied through, for one back-buffer shape.
unsafe fn make_readback(
    device: &IDirect3DDevice9,
    desc: &D3DSURFACE_DESC,
) -> Result<Readback, (HookError, i32)> {
    let multisampled = desc.MultiSampleType != D3DMULTISAMPLE_NONE;
    let resolve = if multisampled {
        let mut s: Option<IDirect3DSurface9> = None;
        device
            .CreateRenderTarget(
                desc.Width,
                desc.Height,
                desc.Format,
                D3DMULTISAMPLE_NONE,
                0,
                false,
                &mut s,
                std::ptr::null_mut(),
            )
            .map_err(|e| (HookError::CreateSurfaceFailed, e.code().0))?;
        Some(s.ok_or((HookError::CreateSurfaceFailed, 0))?)
    } else {
        None
    };

    let mut sysmem: Option<IDirect3DSurface9> = None;
    device
        .CreateOffscreenPlainSurface(
            desc.Width,
            desc.Height,
            desc.Format,
            D3DPOOL_SYSTEMMEM,
            &mut sysmem,
            std::ptr::null_mut(),
        )
        .map_err(|e| (HookError::CreateSurfaceFailed, e.code().0))?;

    Ok(Readback {
        resolve,
        sysmem: sysmem.ok_or((HookError::CreateSurfaceFailed, 0))?,
        width: desc.Width,
        height: desc.Height,
        format: desc.Format,
    })
}

/// The swap-chain route. Captures through the device that owns the chain, so
/// the frame path is identical once we are here.
unsafe extern "system" fn swap_present_detour(
    chain: *mut core::ffi::c_void,
    src: *const RECT,
    dst: *const RECT,
    hwnd: HWND,
    dirty: *const core::ffi::c_void,
    flags: u32,
) -> windows::core::HRESULT {
    if let Some(shared) = SHARED.get() {
        unsafe { (*shared.header()).swap_present_calls += 1 };
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if let (Some(shared), Some(sc)) =
            (SHARED.get(), IDirect3DSwapChain9::from_raw_borrowed(&chain))
        {
            if let Ok(dev) = sc.GetDevice() {
                if let Err((e, hr)) = grab(shared, &dev) {
                    shared.fail(e, hr);
                }
            }
        }
    }));

    let original: SwapPresentFn = std::mem::transmute(SWAP_TRAMPOLINE.load(Ordering::SeqCst));
    original(chain, src, dst, hwnd, dirty, flags)
}
