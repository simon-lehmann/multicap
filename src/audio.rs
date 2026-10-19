//! One application's own sound, recorded without a virtual cable or a mixer.
//!
//! Windows' process loopback (the mechanism behind OBS's application audio)
//! gives us exactly one program's output, so eight game clients on one machine
//! produce eight separate tracks instead of one mixed desktop recording.
//!
//! Audio is written as plain WAV rather than muxed into the video file, on
//! purpose: it stays lossless, and every buffer carries the performance-counter
//! time the audio engine stamped it with. Video frames carry a timestamp from the
//! same counter, so the offset between picture and sound is *computed* from the
//! two, not measured once with a clap and hard-coded.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use windows::core::{implement, Interface, PROPVARIANT};
use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};

use crate::error::{Error, Result};

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u16 = 2;
/// 32-bit float: what the Windows mixer works in, so nothing is quantised on the
/// way out. ffmpeg and every NLE read it.
pub const BITS: u16 = 32;

/// `PROPVARIANT` holding a blob, which the binding does not expose a constructor
/// for. The layout is fixed by the ABI: 8 bytes of tag and padding, then the
/// union, in which a BLOB is a 4-byte size (padded to 8) and a pointer.
#[repr(C)]
struct PropVariantBlob {
    vt: u16,
    r1: u16,
    r2: u16,
    r3: u16,
    cb_size: u32,
    _pad: u32,
    p_blob: *mut u8,
}
const VT_BLOB: u16 = 65;

/// Signalled by Windows when the asynchronous activation has finished.
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivationDone(HANDLE);

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationDone_Impl {
    fn ActivateCompleted(
        &self,
        _op: Option<&IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        unsafe { SetEvent(self.0) }?;
        Ok(())
    }
}

/// What a finished audio track turned out to be.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AudioResult {
    pub path: String,
    pub frames: u64,
    pub seconds: f64,
    /// Performance-counter time of the first sample, in 100 ns ticks. Directly
    /// comparable with a video frame's timestamp: subtract to get the offset.
    pub first_qpc_100ns: i64,
    /// Buffers the audio engine reported as discontinuous.
    pub glitches: u64,
}

pub struct AudioTake {
    stop: Arc<AtomicBool>,
    first_qpc: Arc<AtomicI64>,
    thread: Option<JoinHandle<Result<AudioResult>>>,
}

impl AudioTake {
    /// Record everything `pid` and its children play, to `path`.
    pub fn start(pid: u32, path: &str) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let first_qpc = Arc::new(AtomicI64::new(0));
        let (s, q, p) = (stop.clone(), first_qpc.clone(), path.to_string());
        let thread = std::thread::spawn(move || record(pid, &p, s, q));
        Ok(Self {
            stop,
            first_qpc,
            thread: Some(thread),
        })
    }

    /// When the first sample was captured, or 0 if nothing has arrived yet.
    pub fn first_qpc_100ns(&self) -> i64 {
        self.first_qpc.load(Ordering::Relaxed)
    }

    pub fn finish(mut self) -> Result<AudioResult> {
        self.stop.store(true, Ordering::Relaxed);
        self.thread
            .take()
            .expect("audio thread already joined")
            .join()
            .map_err(|_| Error::msg("audio thread panicked"))?
    }
}

fn record(
    pid: u32,
    path: &str,
    stop: Arc<AtomicBool>,
    first_qpc: Arc<AtomicI64>,
) -> Result<AudioResult> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
    let out = (|| -> Result<AudioResult> {
        let client = activate_process_loopback(pid)?;

        let block_align = CHANNELS * BITS / 8;
        let format = WAVEFORMATEX {
            // WAVE_FORMAT_IEEE_FLOAT; the mixer's own format, so no conversion.
            wFormatTag: 3,
            nChannels: CHANNELS,
            nSamplesPerSec: SAMPLE_RATE,
            nAvgBytesPerSec: SAMPLE_RATE * block_align as u32,
            nBlockAlign: block_align,
            wBitsPerSample: BITS,
            cbSize: 0,
        };

        // 200 ms of slack, so a scheduling hiccup does not cost samples.
        let buffer_100ns = 2_000_000i64;
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                buffer_100ns,
                0,
                &format,
                None,
            )?;
        }

        let event = unsafe { CreateEventW(None, false, false, None)? };
        unsafe { client.SetEventHandle(event)? };
        let capture: IAudioCaptureClient = unsafe { client.GetService()? };
        unsafe { client.Start()? };

        let mut wav = WavWriter::create(path, SAMPLE_RATE, CHANNELS, BITS)?;
        let mut frames = 0u64;
        let mut glitches = 0u64;
        let mut first = 0i64;

        while !stop.load(Ordering::Relaxed) {
            // A silent process wakes us on the timeout rather than the event.
            if unsafe { WaitForSingleObject(event, 200) } != WAIT_OBJECT_0 {
                continue;
            }
            loop {
                let avail = unsafe { capture.GetNextPacketSize()? };
                if avail == 0 {
                    break;
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut count = 0u32;
                let mut flags = 0u32;
                let mut qpc = 0u64;
                unsafe {
                    capture.GetBuffer(&mut data, &mut count, &mut flags, None, Some(&mut qpc))?;
                }
                if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 {
                    glitches += 1;
                }
                if first == 0 && qpc != 0 {
                    first = qpc as i64;
                    first_qpc.store(first, Ordering::Relaxed);
                }
                let bytes = count as usize * block_align as usize;
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                    // The engine may hand us a stale buffer with the silent flag:
                    // write real zeroes so the track stays on the clock.
                    wav.write_silence(bytes)?;
                } else if !data.is_null() && bytes > 0 {
                    wav.write(unsafe { std::slice::from_raw_parts(data, bytes) })?;
                }
                frames += count as u64;
                unsafe { capture.ReleaseBuffer(count)? };
            }
        }

        unsafe { client.Stop()? };
        wav.finish()?;
        Ok(AudioResult {
            path: path.to_string(),
            frames,
            seconds: frames as f64 / SAMPLE_RATE as f64,
            first_qpc_100ns: first,
            glitches,
        })
    })();
    unsafe { CoUninitialize() };
    out
}

/// Open an audio client bound to one process rather than to a device.
fn activate_process_loopback(pid: u32) -> Result<IAudioClient> {
    let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                // The game may play through a child process.
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    let pv = PropVariantBlob {
        vt: VT_BLOB,
        r1: 0,
        r2: 0,
        r3: 0,
        cb_size: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
        _pad: 0,
        p_blob: &mut params as *mut _ as *mut u8,
    };

    let event = unsafe { CreateEventW(None, false, false, None)? };
    let handler: IActivateAudioInterfaceCompletionHandler = ActivationDone(event).into();
    let op = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&pv as *const PropVariantBlob as *const PROPVARIANT),
            &handler,
        )?
    };
    if unsafe { WaitForSingleObject(event, 3000) } != WAIT_OBJECT_0 {
        return Err(Error::msg(format!(
            "audio activation for pid {pid} timed out"
        )));
    }

    let mut hr = windows::core::HRESULT(0);
    let mut unknown: Option<windows::core::IUnknown> = None;
    unsafe { op.GetActivateResult(&mut hr, &mut unknown)? };
    hr.ok()?;
    unknown
        .ok_or_else(|| Error::msg(format!("no audio client for pid {pid}")))?
        .cast::<IAudioClient>()
        .map_err(Into::into)
}

/// Minimal WAV writer: header first with the sizes left blank, patched on close.
struct WavWriter {
    file: BufWriter<File>,
    bytes: u64,
}

impl WavWriter {
    fn create(path: &str, rate: u32, channels: u16, bits: u16) -> Result<Self> {
        let mut file = BufWriter::new(File::create(path)?);
        let block_align = channels * bits / 8;
        let fmt_tag: u16 = if bits == 32 { 3 } else { 1 }; // 3 = IEEE float
        file.write_all(b"RIFF")?;
        file.write_all(&0u32.to_le_bytes())?; // patched in finish()
        file.write_all(b"WAVEfmt ")?;
        file.write_all(&16u32.to_le_bytes())?;
        file.write_all(&fmt_tag.to_le_bytes())?;
        file.write_all(&channels.to_le_bytes())?;
        file.write_all(&rate.to_le_bytes())?;
        file.write_all(&(rate * block_align as u32).to_le_bytes())?;
        file.write_all(&block_align.to_le_bytes())?;
        file.write_all(&bits.to_le_bytes())?;
        file.write_all(b"data")?;
        file.write_all(&0u32.to_le_bytes())?; // patched in finish()
        Ok(Self { file, bytes: 0 })
    }

    fn write(&mut self, data: &[u8]) -> Result<()> {
        self.file.write_all(data)?;
        self.bytes += data.len() as u64;
        Ok(())
    }

    fn write_silence(&mut self, bytes: usize) -> Result<()> {
        const ZEROS: [u8; 4096] = [0u8; 4096];
        let mut left = bytes;
        while left > 0 {
            let n = left.min(ZEROS.len());
            self.file.write_all(&ZEROS[..n])?;
            left -= n;
        }
        self.bytes += bytes as u64;
        Ok(())
    }

    fn finish(mut self) -> Result<()> {
        self.file.flush()?;
        let mut f = self
            .file
            .into_inner()
            .map_err(|e| Error::msg(e.to_string()))?;
        f.seek(SeekFrom::Start(4))?;
        f.write_all(&((36 + self.bytes) as u32).to_le_bytes())?;
        f.seek(SeekFrom::Start(40))?;
        f.write_all(&(self.bytes as u32).to_le_bytes())?;
        f.flush()?;
        Ok(())
    }
}
