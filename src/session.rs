//! One take: several windows filmed at once, each to its own file.
//!
//! Division of labour, decided deliberately. This program does what only it can
//! do on the recording machine: grab each window's own picture, feed the GPU's
//! encoder, and record each application's sound, all at a cost low enough to run
//! beside eight game clients. It does **not** mux, because muxing is free
//! anywhere and this machine's spare capacity is not. Every take therefore
//! produces an `.mp4` per filmed window, a `.wav` per recorded application and
//! one `.json` describing exactly how to join them — including the evened frame
//! timestamps and the measured picture-to-sound offset.
//!
//! Write `dir` to a UNC path and the files land on the other machine as they are
//! encoded, which is the whole of "move the recording after each shot".

use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;

use windows::Win32::Foundation::HWND;

use crate::audio::{AudioResult, AudioTake};
use crate::capture::{Frame, WindowCapture};
use crate::d3d::Device;
use crate::encode::{Encoder, VideoConfig};
use crate::error::{Error, Result};
use crate::timing::{cadence, even_out, Cadence, TICKS_PER_SEC};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Target {
    /// How this window is named in the take's files, e.g. `i3`.
    pub label: String,
    pub hwnd: isize,
    /// Process to record the sound of. 0 for no sound from this one.
    pub pid: u32,
    #[serde(default = "yes")]
    pub video: bool,
    #[serde(default = "yes")]
    pub audio: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TakeConfig {
    pub name: String,
    /// Output directory. May be a UNC path to another machine.
    pub dir: String,
    pub targets: Vec<Target>,
    #[serde(default)]
    pub capture_cursor: bool,
    /// How many frames may be in flight to the encoder before frames are
    /// dropped instead of memory growing.
    #[serde(default = "default_queue")]
    pub queue_depth: u64,
    pub video: VideoConfigSerde,
}

fn default_queue() -> u64 {
    8
}

/// `VideoConfig` is in the encode module and deliberately not serde-aware there;
/// this is its wire form.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VideoConfigSerde {
    #[serde(default = "d1920")]
    pub width: u32,
    #[serde(default = "d1080")]
    pub height: u32,
    #[serde(default)]
    pub codec: Option<crate::encode::Codec>,
    #[serde(default = "d60")]
    pub fps: u32,
    #[serde(default = "d20")]
    pub quality: u32,
    #[serde(default = "d60m")]
    pub bitrate: u32,
}

fn d1920() -> u32 {
    1920
}
fn d1080() -> u32 {
    1080
}
fn d60() -> u32 {
    60
}
fn d20() -> u32 {
    20
}
fn d60m() -> u32 {
    60_000_000
}

impl From<&VideoConfigSerde> for VideoConfig {
    fn from(v: &VideoConfigSerde) -> Self {
        VideoConfig {
            width: v.width,
            height: v.height,
            codec: v.codec.unwrap_or(crate::encode::Codec::Hevc),
            fps: v.fps,
            quality: v.quality,
            bitrate: v.bitrate,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TrackReport {
    pub label: String,
    pub video_file: Option<String>,
    pub audio: Option<AudioResult>,
    pub cadence: Option<Cadence>,
    /// True if the encoder honoured constant quality; false means it fell back
    /// to a fixed bitrate and this is not the master you asked for.
    pub constant_quality: Option<bool>,
    /// Seconds to shift this track's sound by to line it up with its picture.
    /// Computed from two timestamps off the same counter, not calibrated.
    pub audio_offset_seconds: Option<f64>,
    /// Frame timestamps in seconds after jitter has been evened out. Apply these
    /// when muxing; see `ffmpeg_hint`.
    pub evened_seconds: Option<Vec<f64>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TakeReport {
    pub name: String,
    pub dir: String,
    pub tracks: Vec<TrackReport>,
    pub ffmpeg_hint: String,
}

struct Running {
    label: String,
    capture: Option<WindowCapture>,
    rx: Option<Receiver<Frame>>,
    encoder: Option<Encoder>,
    video_file: Option<String>,
    audio: Option<AudioTake>,
}

pub struct Take {
    cfg: TakeConfig,
    dev: Arc<Device>,
    running: Vec<Running>,
}

impl Take {
    pub fn start(dev: Arc<Device>, cfg: TakeConfig) -> Result<Self> {
        let mut running = Vec::new();
        for t in &cfg.targets {
            let mut r = Running {
                label: t.label.clone(),
                capture: None,
                rx: None,
                encoder: None,
                video_file: None,
                audio: None,
            };
            // Sound first: it must already be running when the first frame lands,
            // or the take begins with a picture nothing was heard over.
            if t.audio && t.pid != 0 {
                let path = format!("{}/{}-{}.wav", cfg.dir, cfg.name, t.label);
                r.audio = Some(AudioTake::start(t.pid, &path)?);
            }
            if t.video {
                let path = format!("{}/{}-{}.mp4", cfg.dir, cfg.name, t.label);
                let encoder = Encoder::new(&dev, &path, (&cfg.video).into())?;
                let (tx, rx) = channel();
                let capture = WindowCapture::start(
                    &dev,
                    HWND(t.hwnd as *mut _),
                    cfg.capture_cursor,
                    cfg.queue_depth,
                    tx,
                )?;
                r.capture = Some(capture);
                r.rx = Some(rx);
                r.encoder = Some(encoder);
                r.video_file = Some(path);
            }
            running.push(r);
        }
        Ok(Self { cfg, dev, running })
    }

    /// Drain whatever the capture threads have produced into the encoders.
    /// Called repeatedly while the take runs; `stop` drains the rest.
    pub fn pump(&mut self) -> Result<()> {
        for r in &mut self.running {
            let (Some(rx), Some(enc)) = (r.rx.as_ref(), r.encoder.as_mut()) else {
                continue;
            };
            while let Ok(frame) = rx.try_recv() {
                enc.write(frame.texture.texture(), frame.time_100ns)?;
            }
        }
        Ok(())
    }

    pub fn stop(mut self) -> Result<TakeReport> {
        // Stop every capture before draining, so the takes end together rather
        // than each one ending whenever its own drain finishes.
        for r in &self.running {
            if let Some(c) = &r.capture {
                c.stop()?;
            }
        }

        let mut tracks = Vec::new();
        for r in &mut self.running {
            let mut times: Vec<i64> = Vec::new();
            if let (Some(rx), Some(enc)) = (r.rx.take(), r.encoder.as_mut()) {
                for frame in rx.try_iter() {
                    times.push(frame.time_100ns);
                    enc.write(frame.texture.texture(), frame.time_100ns)?;
                }
            }
            let dropped = r.capture.as_ref().map(|c| c.dropped()).unwrap_or(0);
            let constant_quality = r.encoder.as_ref().map(|e| e.constant_quality);
            if let Some(enc) = r.encoder.take() {
                enc.finish()?;
            }

            let audio = match r.audio.take() {
                Some(a) => Some(a.finish()?),
                None => None,
            };
            let audio_offset_seconds = match (&audio, times.first()) {
                (Some(a), Some(&first_frame)) if a.first_qpc_100ns != 0 => {
                    Some((a.first_qpc_100ns - first_frame) as f64 / TICKS_PER_SEC as f64)
                }
                _ => None,
            };

            let (cad, evened) = if times.is_empty() {
                (None, None)
            } else {
                let e = even_out(&times, 2.0);
                let base = e[0];
                (
                    Some(cadence(&times, dropped)),
                    Some(
                        e.iter()
                            .map(|t| (t - base) as f64 / TICKS_PER_SEC as f64)
                            .collect::<Vec<f64>>(),
                    ),
                )
            };

            tracks.push(TrackReport {
                label: r.label.clone(),
                video_file: r.video_file.clone(),
                audio,
                cadence: cad,
                constant_quality,
                audio_offset_seconds,
                evened_seconds: evened,
            });
        }

        let _ = &self.dev; // kept alive until every encoder has finalised
        Ok(TakeReport {
            name: self.cfg.name.clone(),
            dir: self.cfg.dir.clone(),
            ffmpeg_hint: "join picture and sound on the receiving machine: \
ffmpeg -i <take>-<label>.mp4 -itsoffset <audio_offset_seconds> -i <take>-<label>.wav \
-c:v copy -c:a flac -shortest <take>-<label>.mkv"
                .to_string(),
            tracks,
        })
    }

    /// Frames kept and frames dropped so far, per target.
    pub fn progress(&self) -> Vec<(String, u64, u64)> {
        self.running
            .iter()
            .map(|r| {
                let c = r.capture.as_ref();
                (
                    r.label.clone(),
                    c.map(|c| c.captured()).unwrap_or(0),
                    c.map(|c| c.dropped()).unwrap_or(0),
                )
            })
            .collect()
    }
}

impl Drop for Take {
    fn drop(&mut self) {
        for r in &self.running {
            if let Some(c) = &r.capture {
                let _ = c.stop();
            }
        }
    }
}

#[allow(dead_code)]
fn unused(_: Error) {}
