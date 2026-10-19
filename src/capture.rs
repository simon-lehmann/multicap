//! Windows Graphics Capture on one window.
//!
//! Two things here are not obvious and both were paid for in a previous recorder:
//!
//! 1. **Never sample at a fixed rate.** A capture pinned to 60 fps against a game
//!    drawing ~60 and a monitor running 100 lands on ~50: one frame in six is a
//!    duplicate and one game frame in five is lost. We take every frame the
//!    compositor hands us, each with its own timestamp, and sort the cadence out
//!    afterwards (see `timing`).
//! 2. **Frame-pool surfaces are recycled.** The texture inside a
//!    `Direct3D11CaptureFrame` is reused the moment the frame is dropped, so it is
//!    copied into a texture we own before the encoder ever sees it. Those are
//!    recycled through `TexturePool` rather than reallocated 60 times a second.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use windows::core::{IInspectable, Interface};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess;
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

use crate::d3d::Device;
use crate::error::{Error, Result};

/// A GPU texture lent out by `TexturePool`, returned when dropped.
pub struct PooledTexture {
    texture: Option<ID3D11Texture2D>,
    pool: Arc<TexturePool>,
}

impl PooledTexture {
    pub fn texture(&self) -> &ID3D11Texture2D {
        self.texture.as_ref().expect("texture taken before drop")
    }
}

impl Drop for PooledTexture {
    fn drop(&mut self) {
        if let Some(t) = self.texture.take() {
            self.pool.put(t);
        }
    }
}

/// Fixed-size recycling pool of identical textures.
///
/// Capped: if the encoder falls behind, we would rather drop the oldest frame than
/// let allocation grow without limit and take the machine down with it.
pub struct TexturePool {
    free: Mutex<Vec<ID3D11Texture2D>>,
    device: ID3D11Device,
    desc: D3D11_TEXTURE2D_DESC,
    live: AtomicU64,
    cap: u64,
}

impl TexturePool {
    // Shared with the capture callback, which Windows runs on a thread pool
    // thread, so this really is used from two threads. The D3D11 device is
    // created with multithread protection on (see `d3d::Device`), which is
    // precisely what makes concurrent use of the device and its resources
    // defined; the binding simply does not mark COM types `Send`/`Sync`.
    #[allow(clippy::arc_with_non_send_sync)]
    pub fn new(device: ID3D11Device, desc: D3D11_TEXTURE2D_DESC, cap: u64) -> Arc<Self> {
        Arc::new(Self {
            free: Mutex::new(Vec::new()),
            device,
            desc,
            live: AtomicU64::new(0),
            cap,
        })
    }

    /// `None` when every texture is still out with the encoder: the caller drops
    /// the frame and counts it, which is the signal that encoding cannot keep up.
    fn take(self: &Arc<Self>) -> Result<Option<PooledTexture>> {
        if let Some(t) = self.free.lock().unwrap().pop() {
            return Ok(Some(PooledTexture {
                texture: Some(t),
                pool: self.clone(),
            }));
        }
        if self.live.load(Ordering::Relaxed) >= self.cap {
            return Ok(None);
        }
        let mut made: Option<ID3D11Texture2D> = None;
        unsafe {
            self.device
                .CreateTexture2D(&self.desc, None, Some(&mut made))?
        };
        self.live.fetch_add(1, Ordering::Relaxed);
        Ok(Some(PooledTexture {
            texture: Some(made.expect("CreateTexture2D returned no texture")),
            pool: self.clone(),
        }))
    }

    fn put(&self, t: ID3D11Texture2D) {
        self.free.lock().unwrap().push(t);
    }
}

/// One captured frame: a texture we own and the compositor's own timestamp for it.
pub struct Frame {
    pub texture: PooledTexture,
    /// `SystemRelativeTime`, in 100 ns ticks off the performance counter. This is
    /// the same unit Media Foundation stamps samples in, so it is carried straight
    /// through rather than being re-derived from a wall clock.
    pub time_100ns: i64,
}

pub struct WindowCapture {
    session: GraphicsCaptureSession,
    _pool: Direct3D11CaptureFramePool,
    _item: GraphicsCaptureItem,
    dropped: Arc<AtomicU64>,
    captured: Arc<AtomicU64>,
}

impl WindowCapture {
    /// Begin capturing `hwnd`. Frames land on `sink` from a capture thread.
    pub fn start(
        dev: &Device,
        hwnd: HWND,
        capture_cursor: bool,
        queue_depth: u64,
        sink: Sender<Frame>,
    ) -> Result<Self> {
        let interop: IGraphicsCaptureItemInterop =
            windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(hwnd)? };
        let size: SizeInt32 = item.Size()?;
        if size.Width <= 0 || size.Height <= 0 {
            return Err(Error::msg(format!(
                "window {:?} has no size ({}x{}); it is minimised or gone",
                hwnd.0, size.Width, size.Height
            )));
        }

        let desc = D3D11_TEXTURE2D_DESC {
            Width: size.Width as u32,
            Height: size.Height as u32,
            MipLevels: 1,
            ArraySize: 1,
            Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let textures = TexturePool::new(dev.device.clone(), desc, queue_depth);

        // Free-threaded: frames arrive on a pool thread, so no message loop is
        // needed and the daemon never has to pump one.
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &dev.winrt,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )?;

        let dropped = Arc::new(AtomicU64::new(0));
        let captured = Arc::new(AtomicU64::new(0));
        let ctx = dev.context.clone();
        let (d, c) = (dropped.clone(), captured.clone());

        pool.FrameArrived(
            &TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(move |pool, _| {
                let pool = pool.as_ref().expect("FrameArrived without a pool");
                let frame = pool.TryGetNextFrame()?;
                let time_100ns = frame.SystemRelativeTime()?.Duration;

                let surface = frame.Surface()?;
                let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
                let src: ID3D11Texture2D = unsafe { access.GetInterface()? };

                match textures.take() {
                    Ok(Some(dst)) => {
                        // GPU-to-GPU; the frame never enters system memory.
                        unsafe { ctx.CopyResource(dst.texture(), &src) };
                        c.fetch_add(1, Ordering::Relaxed);
                        // A closed channel just means the take has stopped.
                        let _ = sink.send(Frame {
                            texture: dst,
                            time_100ns,
                        });
                    }
                    // Encoder is behind: drop this frame rather than grow memory.
                    Ok(None) => {
                        d.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        eprintln!("multicap: texture pool: {e}");
                        d.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Ok(())
            }),
        )?;

        let session = pool.CreateCaptureSession(&item)?;
        session.SetIsCursorCaptureEnabled(capture_cursor)?;
        // Windows 11 only: suppress the yellow "being captured" border. Older
        // builds do not have the property and do not draw the border either.
        let _ = session.SetIsBorderRequired(false);
        session.StartCapture()?;

        Ok(Self {
            session,
            _pool: pool,
            _item: item,
            dropped,
            captured,
        })
    }

    /// Frames the compositor delivered and we kept.
    pub fn captured(&self) -> u64 {
        self.captured.load(Ordering::Relaxed)
    }

    /// Frames thrown away because the encoder could not keep up. Must be 0 in a
    /// take that goes into a film.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub fn stop(&self) -> Result<()> {
        self.session.Close()?;
        Ok(())
    }
}
