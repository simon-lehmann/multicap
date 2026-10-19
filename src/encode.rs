//! Hardware video encoding through Media Foundation.
//!
//! The Sink Writer is given a DXGI device manager bound to the same D3D11 device
//! the capture runs on, so a captured texture is handed to the GPU's encoder
//! without ever entering system memory. Media Foundation picks whatever hardware
//! encoder the machine has (AMD AMF, NVIDIA NVENC, Intel QSV) behind one API,
//! which is why this is vendor-neutral where binding AMF directly would not be.
//!
//! Audio is deliberately not muxed here; see `audio`.

use windows::core::{Interface, GUID, HSTRING, VARIANT};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::*;

use crate::d3d::Device;
use crate::error::{Error, Result};
use crate::timing::TICKS_PER_SEC;

/// Which codec the GPU should encode to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    /// Default. Roughly half the bits of H.264 for the same picture.
    Hevc,
    /// For players and NLEs that still choke on HEVC.
    H264,
}

impl Codec {
    fn subtype(self) -> GUID {
        match self {
            Codec::Hevc => MFVideoFormat_HEVC,
            Codec::H264 => MFVideoFormat_H264,
        }
    }
}

#[derive(Debug, Clone)]
pub struct VideoConfig {
    pub width: u32,
    pub height: u32,
    pub codec: Codec,
    /// Nominal rate written into the file. The real cadence comes from the
    /// per-frame timestamps; this only has to be close.
    pub fps: u32,
    /// Constant-quality target, 0 (best) to 51, in the usual quantiser scale.
    /// Taken only if the installed encoder exposes quality rate control.
    pub quality: u32,
    /// Fallback when it does not. Deliberately generous: this is a master.
    pub bitrate: u32,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            codec: Codec::Hevc,
            fps: 60,
            quality: 20,
            bitrate: 60_000_000,
        }
    }
}

/// Media Foundation must be started once per process before any of this works.
pub fn startup() -> Result<()> {
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL)? };
    Ok(())
}

pub fn shutdown() -> Result<()> {
    unsafe { MFShutdown()? };
    Ok(())
}

pub struct Encoder {
    writer: IMFSinkWriter,
    stream: u32,
    cfg: VideoConfig,
    /// Whether constant-quality rate control was actually accepted. Reported in
    /// the sidecar: a take encoded at a fixed bitrate is not the same master.
    pub constant_quality: bool,
    frames: u64,
}

impl Encoder {
    /// `path` may be a UNC path (`\\\\spark\\rec\\take.mp4`), which is how a take
    /// is written straight to the other machine instead of being copied after.
    pub fn new(dev: &Device, path: &str, cfg: VideoConfig) -> Result<Self> {
        let mut token = 0u32;
        let mut mgr: Option<IMFDXGIDeviceManager> = None;
        unsafe { MFCreateDXGIDeviceManager(&mut token, &mut mgr)? };
        let mgr = mgr.ok_or_else(|| Error::msg("no DXGI device manager"))?;
        unsafe { mgr.ResetDevice(&dev.device, token)? };

        let mut attrs: Option<IMFAttributes> = None;
        unsafe { MFCreateAttributes(&mut attrs, 4)? };
        let attrs = attrs.ok_or_else(|| Error::msg("no sink writer attributes"))?;
        unsafe {
            // Hand the encoder the GPU device: this is what keeps it zero-copy.
            attrs.SetUnknown(&MF_SINK_WRITER_D3D_MANAGER, &mgr)?;
            attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
            // We feed frames as fast as they are captured; do not rate-limit us.
            attrs.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
            attrs.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;
        }

        let writer = unsafe { MFCreateSinkWriterFromURL(&HSTRING::from(path), None, &attrs)? };

        let size = ((cfg.width as u64) << 32) | cfg.height as u64;
        let rate = ((cfg.fps as u64) << 32) | 1u64;

        let out = unsafe { MFCreateMediaType()? };
        unsafe {
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out.SetGUID(&MF_MT_SUBTYPE, &cfg.codec.subtype())?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, cfg.bitrate)?;
            out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            out.SetUINT64(&MF_MT_FRAME_SIZE, size)?;
            out.SetUINT64(&MF_MT_FRAME_RATE, rate)?;
            out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
            // Say what the picture actually is, at the source. Getting this wrong
            // is why captures come out washed out or crushed: encoders write
            // limited-range pixels and then label the file full-range.
            out.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
            out.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
            out.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
            out.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        }
        let stream = unsafe { writer.AddStream(&out)? };

        // What we feed in: exactly what Graphics Capture produces. The Sink
        // Writer inserts the GPU video processor to reach the encoder's NV12.
        let inp = unsafe { MFCreateMediaType()? };
        unsafe {
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_ARGB32)?;
            inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            inp.SetUINT64(&MF_MT_FRAME_SIZE, size)?;
            inp.SetUINT64(&MF_MT_FRAME_RATE, rate)?;
            inp.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
            writer.SetInputMediaType(stream, &inp, None)?;
        }

        let constant_quality = set_constant_quality(&writer, stream, cfg.quality).unwrap_or(false);
        unsafe { writer.BeginWriting()? };

        Ok(Self {
            writer,
            stream,
            cfg,
            constant_quality,
            frames: 0,
        })
    }

    /// Hand one captured texture to the encoder.
    pub fn write(&mut self, texture: &ID3D11Texture2D, time_100ns: i64) -> Result<()> {
        let buffer =
            unsafe { MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)? };
        // A DXGI buffer comes back with length 0; the writer wants it set.
        let len = unsafe { buffer.GetMaxLength()? };
        unsafe { buffer.SetCurrentLength(len)? };

        let sample = unsafe { MFCreateSample()? };
        unsafe {
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(time_100ns)?;
            sample.SetSampleDuration(TICKS_PER_SEC / self.cfg.fps.max(1) as i64)?;
            self.writer.WriteSample(self.stream, &sample)?;
        }
        self.frames += 1;
        Ok(())
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    pub fn finish(self) -> Result<u64> {
        unsafe { self.writer.Finalize()? };
        Ok(self.frames)
    }
}

/// Ask the installed encoder for constant quality instead of constant bitrate.
///
/// Support varies by vendor and driver, so this is best effort: `Ok(false)` means
/// the encoder kept its bitrate mode and the take is still usable, just not at a
/// fixed quantiser. The caller records which it got.
fn set_constant_quality(writer: &IMFSinkWriter, stream: u32, quality: u32) -> Result<bool> {
    let mut codec_api: Option<ICodecAPI> = None;
    unsafe {
        let hr = writer.GetServiceForStream(
            stream,
            &GUID::zeroed(),
            &ICodecAPI::IID,
            &mut codec_api as *mut _ as *mut _,
        );
        if hr.is_err() {
            return Ok(false);
        }
    }
    let Some(api) = codec_api else {
        return Ok(false);
    };

    unsafe {
        // eAVEncCommonRateControlMode_Quality == 3
        if api
            .SetValue(&CODECAPI_AVEncCommonRateControlMode, &VARIANT::from(3u32))
            .is_err()
        {
            return Ok(false);
        }
        if api
            .SetValue(&CODECAPI_AVEncCommonQuality, &VARIANT::from(quality))
            .is_err()
        {
            return Ok(false);
        }
    }
    Ok(true)
}
