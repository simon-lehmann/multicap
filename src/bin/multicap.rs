//! multicap: record many Windows application windows at once.
//!
//!   multicap check                 is this machine able to capture at all
//!   multicap windows               capturable windows, with handles and pids
//!   multicap record ...            one take, then exit
//!   multicap serve --port 7788     take orders over a loopback socket
//!
//! Everything prints JSON on stdout, so a pipeline never has to parse prose.

#[cfg(not(windows))]
fn main() {
    eprintln!("multicap records Windows applications and only builds for Windows.");
    std::process::exit(2);
}

#[cfg(windows)]
fn main() {
    if let Err(e) = windows_main::run() {
        eprintln!("{}", serde_json::json!({ "error": e.to_string() }));
        std::process::exit(1);
    }
}

#[cfg(windows)]
mod windows_main {
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use multicap::d3d::Device;
    use multicap::encode;
    use multicap::error::{Error, Result};
    use multicap::preflight;
    use multicap::session::{Take, TakeConfig, TakeReport};

    use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT, TRUE};
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClientRect, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
    };

    pub fn run() -> Result<()> {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let cmd = args.first().map(String::as_str).unwrap_or("help");

        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };

        match cmd {
            "check" => {
                let d = preflight::displays()?;
                println!("{}", serde_json::to_string_pretty(&d).unwrap());
                if !d.ok {
                    std::process::exit(3);
                }
                Ok(())
            }
            "windows" => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&list_windows()?).unwrap()
                );
                Ok(())
            }
            "record" => {
                encode::startup()?;
                let cfg = read_config(&args[1..])?;
                let seconds: f64 = flag(&args[1..], "--seconds")
                    .ok_or_else(|| Error::msg("record needs --seconds"))?
                    .parse()
                    .map_err(|_| Error::msg("--seconds must be a number"))?;
                let report = record_for(cfg, Duration::from_secs_f64(seconds))?;
                println!("{}", serde_json::to_string_pretty(&report).unwrap());
                encode::shutdown()
            }
            // Diagnostic: stand up the GPU pieces one at a time and hold them,
            // to find which of them disturbs a Direct3D 9 game on a machine with
            // no display output.
            "probe" => {
                let stage = flag(&args[1..], "--stage").unwrap_or_else(|| "d3d11".into());
                let hold: u64 = flag(&args[1..], "--seconds")
                    .unwrap_or_else(|| "8".into())
                    .parse()
                    .unwrap_or(8);
                println!("{}", serde_json::json!({ "stage": stage, "holding": hold }));
                let _dev = (stage != "none").then(Device::new).transpose()?;
                if stage == "encoder" {
                    encode::startup()?;
                    let d = _dev.as_ref().expect("device for encoder probe");
                    let out = flag(&args[1..], "--out")
                        .unwrap_or_else(|| "D:\\wow-sandbox\\probe.mp4".into());
                    let e = multicap::encode::Encoder::new(d, &out, Default::default())?;
                    println!(
                        "{}",
                        serde_json::json!({ "encoder": "created",
                        "constant_quality": e.constant_quality })
                    );
                    std::thread::sleep(Duration::from_secs(hold));
                    drop(e);
                    return encode::shutdown();
                }
                std::thread::sleep(Duration::from_secs(hold));
                Ok(())
            }
            "hook" => {
                encode::startup()?;
                let pids: Vec<u32> = flag(&args[1..], "--pid")
                    .ok_or_else(|| Error::msg("hook needs --pid N  (or --pid N,M,...)"))?
                    .split(',')
                    .map(|p| p.trim().parse::<u32>())
                    .collect::<std::result::Result<_, _>>()
                    .map_err(|_| Error::msg("--pid takes numbers separated by commas"))?;
                let seconds: f64 = flag(&args[1..], "--seconds")
                    .ok_or_else(|| Error::msg("hook needs --seconds"))?
                    .parse()
                    .map_err(|_| Error::msg("--seconds must be a number"))?;
                let out = flag(&args[1..], "--out");
                let dir = flag(&args[1..], "--dir");
                // A take wants sound, so it is recorded unless refused.
                let audio = !args[1..].iter().any(|a| a == "--no-audio");
                if pids.len() > 1 && out.is_some() {
                    return Err(Error::msg(
                        "several pids write several files: use --dir, not --out",
                    ));
                }
                let reports = record_hooks(
                    &pids,
                    out.as_deref(),
                    dir.as_deref(),
                    Duration::from_secs_f64(seconds),
                    audio,
                )?;
                println!("{}", serde_json::to_string_pretty(&reports).unwrap());
                encode::shutdown()
            }
            // Relay one request to a daemon on this machine and print its
            // answer. The daemon listens on loopback inside the desktop session;
            // a caller over SSH lands in another session and cannot reach the
            // games, but it can run this.
            "call" => {
                let port: u16 = flag(&args[1..], "--port")
                    .unwrap_or_else(|| "7788".into())
                    .parse()
                    .map_err(|_| Error::msg("--port must be a number"))?;
                let body = match args.get(1).map(String::as_str) {
                    Some("-") | None => {
                        let mut buf = String::new();
                        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
                        buf
                    }
                    Some(j) => j.to_string(),
                };
                let mut stream = TcpStream::connect(("127.0.0.1", port))
                    .map_err(|e| Error::msg(format!("no daemon on port {port}: {e}")))?;
                stream.write_all(body.trim().as_bytes())?;
                stream.write_all(b"\n")?;
                stream.flush()?;
                let mut reply = String::new();
                BufReader::new(&stream).read_line(&mut reply)?;
                print!("{reply}");
                Ok(())
            }
            "serve" => {
                encode::startup()?;
                let port: u16 = flag(&args[1..], "--port")
                    .unwrap_or_else(|| "7788".into())
                    .parse()
                    .map_err(|_| Error::msg("--port must be a number"))?;
                serve(port)
            }
            _ => {
                eprintln!("{}", include_str!("../../USAGE.txt"));
                Ok(())
            }
        }
    }

    fn flag(args: &[String], name: &str) -> Option<String> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    }

    /// A take is described by JSON, from `--config FILE` or `--json '{...}'`.
    /// Describing eight windows on a command line is worse for everyone.
    fn read_config(args: &[String]) -> Result<TakeConfig> {
        let text = if let Some(p) = flag(args, "--config") {
            std::fs::read_to_string(p)?
        } else if let Some(j) = flag(args, "--json") {
            j
        } else {
            return Err(Error::msg("record needs --config FILE or --json '{...}'"));
        };
        serde_json::from_str(&text).map_err(|e| Error::msg(format!("bad take config: {e}")))
    }

    #[derive(serde::Serialize)]
    struct WindowInfo {
        hwnd: isize,
        pid: u32,
        title: String,
        width: i32,
        height: i32,
    }

    fn list_windows() -> Result<Vec<WindowInfo>> {
        let mut found: Vec<WindowInfo> = Vec::new();
        unsafe {
            let _ = EnumWindows(Some(enum_proc), LPARAM(&mut found as *mut _ as isize));
        }
        found.retain(|w| w.width > 0 && w.height > 0 && !w.title.is_empty());
        Ok(found)
    }

    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let out = &mut *(lparam.0 as *mut Vec<WindowInfo>);
        if !IsWindowVisible(hwnd).as_bool() {
            return TRUE;
        }
        let mut buf = [0u16; 512];
        let n = GetWindowTextW(hwnd, &mut buf);
        let title = String::from_utf16_lossy(&buf[..n as usize]);
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let mut r = RECT::default();
        let _ = GetClientRect(hwnd, &mut r);
        out.push(WindowInfo {
            hwnd: hwnd.0 as isize,
            pid,
            title,
            width: r.right - r.left,
            height: r.bottom - r.top,
        });
        TRUE
    }

    #[derive(serde::Serialize)]
    struct HookReport {
        #[serde(skip_serializing_if = "Option::is_none")]
        take: Option<String>,
        pid: u32,
        file: String,
        namespace: String,
        frames: u64,
        /// What the hook itself saw, which is the number to compare against:
        /// frames it captured but we never collected show up as the difference.
        hook_captured: u64,
        hook_dropped: u64,
        width: u32,
        height: u32,
        constant_quality: bool,
        cadence: Option<multicap::timing::Cadence>,
        reads: multicap::hooksrc::ReadStats,
        audio: Option<multicap::audio::AudioResult>,
        /// Seconds to shift the sound by to line it up with the picture.
        /// Computed from two timestamps off the same performance counter: the
        /// hook stamps each frame, the audio engine stamps each buffer.
        audio_offset_seconds: Option<f64>,
        /// Wall-clock time of the first frame, seconds since the epoch. The
        /// shoot runner dates pointer and typed-text cues against this to turn
        /// them into seconds within the file.
        first_picture: Option<f64>,
        ffmpeg_hint: Option<String>,
    }

    /// Record several hooked games at once, each to its own file.
    ///
    /// One thread per game, and one D3D11 device and encoder per thread: COM
    /// objects belong to the apartment that made them, and keeping them apart
    /// means one game's encoder stalling cannot hold up another's frames. This
    /// is the case the whole tool exists for, so it is the default shape rather
    /// than a loop around a single recorder.
    fn record_hooks(
        pids: &[u32],
        out: Option<&str>,
        dir: Option<&str>,
        how_long: Duration,
        audio: bool,
    ) -> Result<Vec<HookReport>> {
        let never = Arc::new(AtomicBool::new(false));
        record_hooks_until(pids, out, dir, how_long, audio, never)
    }

    /// The same, but stoppable: the daemon does not know a take's length in
    /// advance, because a run sheet ends a take when the action ends.
    fn record_hooks_until(
        pids: &[u32],
        out: Option<&str>,
        dir: Option<&str>,
        how_long: Duration,
        audio: bool,
        stop: Arc<AtomicBool>,
    ) -> Result<Vec<HookReport>> {
        let files: Vec<(u32, String)> = pids
            .iter()
            .map(|&pid| (pid, default_name(pid, out, dir)))
            .collect();
        record_hooks_to(&files, how_long, audio, stop)
    }

    /// Where a take's file goes when the caller did not say.
    fn default_name(pid: u32, out: Option<&str>, dir: Option<&str>) -> String {
        match (out, dir) {
            (Some(o), _) => o.to_string(),
            (None, Some(d)) => {
                let d = d.trim_end_matches(['/', '\\']);
                // Keep whatever separator the caller used, rather than
                // producing D:\dir/file.mp4.
                let sep = if d.contains('\\') { '\\' } else { '/' };
                format!("{d}{sep}hook-{pid}.mp4")
            }
            (None, None) => format!("hook-{pid}.mp4"),
        }
    }

    /// The real fan-out: each game with the exact file it should be written to.
    /// Naming belongs to whoever asked for the take — a shoot runner has its own
    /// convention and the tools downstream of it expect that convention, not
    /// ours.
    fn record_hooks_to(
        files: &[(u32, String)],
        how_long: Duration,
        audio: bool,
        stop: Arc<AtomicBool>,
    ) -> Result<Vec<HookReport>> {
        let mut threads = Vec::new();
        let pids: Vec<u32> = files.iter().map(|(p, _)| *p).collect();
        for (pid, file) in files.iter().cloned() {
            let stop = stop.clone();
            threads.push(std::thread::spawn(move || {
                unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }
                    .map_err(|e| e.to_string())?;
                record_from_hook(pid, &file, how_long, audio, stop).map_err(|e| e.to_string())
            }));
        }

        let mut reports = Vec::new();
        let mut failures = Vec::new();
        for (pid, t) in pids.iter().zip(threads) {
            match t.join() {
                Ok(Ok(r)) => reports.push(r),
                Ok(Err(e)) => failures.push(format!("pid {pid}: {e}")),
                Err(_) => failures.push(format!("pid {pid}: recorder thread panicked")),
            }
        }
        // One game failing should not throw away the takes that worked.
        if reports.is_empty() && !failures.is_empty() {
            return Err(Error::msg(failures.join("; ")));
        }
        if !failures.is_empty() {
            eprintln!("{}", serde_json::json!({ "failed": failures }));
        }
        Ok(reports)
    }

    /// Record from a graphics hook rather than from the desktop. This is the
    /// path that needs no display output at all.
    fn record_from_hook(
        pid: u32,
        out: &str,
        how_long: Duration,
        want_audio: bool,
        stop: Arc<AtomicBool>,
    ) -> Result<HookReport> {
        let mut src = multicap::hooksrc::HookSource::open(pid)?;
        let dev = Device::new()?;

        // Sound first: it has to be running before the first frame, or the take
        // opens on a picture with nothing heard over it.
        let audio_path = format!("{}.wav", out.trim_end_matches(".mp4"));
        let audio = want_audio
            .then(|| multicap::audio::AudioTake::start(pid, &audio_path))
            .transpose()?;

        let mut encoder: Option<multicap::encode::Encoder> = None;
        let mut upload: Option<multicap::hooksrc::Upload> = None;
        let mut times: Vec<i64> = Vec::new();
        let (mut w, mut h) = (0u32, 0u32);
        let mut constant_quality = false;

        let until = Instant::now() + how_long;
        while Instant::now() < until && !stop.load(Ordering::Relaxed) {
            let Some((info, pixels, time_100ns)) = src.next_frame(100)? else {
                continue;
            };
            if upload
                .as_ref()
                .is_none_or(|u| !u.matches(info.width, info.height))
            {
                upload = Some(multicap::hooksrc::Upload::new(
                    &dev,
                    info.width,
                    info.height,
                )?);
            }
            let up = upload.as_ref().expect("upload texture just created");
            up.write(&dev, pixels, info.pitch)?;

            if encoder.is_none() {
                let mut cfg = multicap::encode::VideoConfig {
                    width: info.width,
                    height: info.height,
                    ..Default::default()
                };
                cfg.fps = 60;
                let e = multicap::encode::Encoder::new(&dev, out, cfg)?;
                constant_quality = e.constant_quality;
                (w, h) = (info.width, info.height);
                encoder = Some(e);
            }
            encoder
                .as_mut()
                .expect("encoder created with the first frame")
                .write(&up.texture, time_100ns)?;
            times.push(time_100ns);
        }

        // Stop the sound only once the picture is done, so it covers the whole
        // take rather than ending a frame early.
        let audio = match audio {
            Some(a) => Some(a.finish()?),
            None => None,
        };
        let audio_offset_seconds = match (&audio, times.first()) {
            (Some(a), Some(&first_frame)) if a.first_qpc_100ns != 0 => Some(
                (a.first_qpc_100ns - first_frame) as f64 / multicap::timing::TICKS_PER_SEC as f64,
            ),
            _ => None,
        };

        let header = src.header();
        let (hook_captured, hook_dropped) = (header.captured, header.dropped);
        let namespace = format!("{:?}", src.namespace);
        let reads = src.stats;
        let frames = match encoder {
            Some(e) => e.finish()?,
            None => 0,
        };

        Ok(HookReport {
            take: None,
            pid,
            file: out.to_string(),
            namespace,
            frames,
            hook_captured,
            hook_dropped,
            width: w,
            height: h,
            constant_quality,
            cadence: (!times.is_empty()).then(|| multicap::timing::cadence(&times, 0)),
            first_picture: times.first().map(|t| src.unix_of(*t)),
            reads,
            ffmpeg_hint: audio_offset_seconds.map(|o| {
                format!(
                    "ffmpeg -i {} -itsoffset {:.4} -i {} -c:v copy -c:a flac -shortest out.mkv",
                    out, o, audio_path
                )
            }),
            audio,
            audio_offset_seconds,
        })
    }

    fn record_for(cfg: TakeConfig, how_long: Duration) -> Result<TakeReport> {
        guard_display()?;
        // See the note on `TexturePool::new`: the device is multithread-protected.
        #[allow(clippy::arc_with_non_send_sync)]
        let dev = Arc::new(Device::new()?);
        let mut take = Take::start(dev, cfg)?;
        let until = Instant::now() + how_long;
        while Instant::now() < until {
            take.pump()?;
            std::thread::sleep(Duration::from_millis(4));
        }
        take.stop()
    }

    /// Never start a take that is certain to produce nothing.
    fn guard_display() -> Result<()> {
        let d = preflight::displays()?;
        if !d.ok {
            return Err(Error::msg(d.note));
        }
        Ok(())
    }

    // ---- the daemon ------------------------------------------------------

    struct Running {
        stop: Arc<AtomicBool>,
        progress: Arc<Mutex<Vec<(String, u64, u64)>>>,
        thread: JoinHandle<std::result::Result<TakeReport, String>>,
    }

    /// A take recorded through injected graphics hooks rather than window
    /// capture. Started and stopped by the caller, because a run sheet ends a
    /// take when the action ends, not after a number of seconds agreed up front.
    struct HookRunning {
        pids: Vec<u32>,
        stop: Arc<AtomicBool>,
        thread: JoinHandle<std::result::Result<Vec<HookReport>, String>>,
    }

    /// Each take owns a thread, and owns it completely: the D3D device, the
    /// capture sessions and the encoders are COM objects with apartment rules,
    /// so they are created on that thread and never cross to another. Only plain
    /// data (the config in, the report out) passes the boundary.
    ///
    /// The thread signals once it is either recording or has failed, so the
    /// client that asked for the take learns about a bad window handle straight
    /// away instead of at `stop`.
    fn start_take(cfg: TakeConfig) -> Result<Running> {
        guard_display()?;
        let stop = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new(Vec::new()));
        let (s, p) = (stop.clone(), progress.clone());
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<std::result::Result<(), String>>();

        let thread = std::thread::spawn(move || {
            unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }
                .map_err(|e| e.to_string())?;
            let dev = match Device::new() {
                // See the note on `TexturePool::new`.
                #[allow(clippy::arc_with_non_send_sync)]
                Ok(d) => Arc::new(d),
                Err(e) => {
                    let _ = ready_tx.send(Err(e.to_string()));
                    return Err(e.to_string());
                }
            };
            let mut take = match Take::start(dev, cfg) {
                Ok(t) => t,
                Err(e) => {
                    let _ = ready_tx.send(Err(e.to_string()));
                    return Err(e.to_string());
                }
            };
            let _ = ready_tx.send(Ok(()));

            while !s.load(Ordering::Relaxed) {
                take.pump().map_err(|e| e.to_string())?;
                *p.lock().unwrap() = take.progress();
                std::thread::sleep(Duration::from_millis(4));
            }
            take.stop().map_err(|e| e.to_string())
        });

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Running {
                stop,
                progress,
                thread,
            }),
            Ok(Err(e)) => Err(Error::msg(e)),
            Err(_) => Err(Error::msg("take thread died before it started recording")),
        }
    }

    fn serve(port: u16) -> Result<()> {
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        println!("{}", serde_json::json!({ "listening": port }));
        std::io::stdout().flush()?;

        let takes: Arc<Mutex<HashMap<String, Running>>> = Arc::new(Mutex::new(HashMap::new()));
        let hooks: Arc<Mutex<HashMap<String, HookRunning>>> = Arc::new(Mutex::new(HashMap::new()));
        for stream in listener.incoming() {
            let stream = stream?;
            if handle(stream, &takes, &hooks)? {
                break;
            }
        }
        encode::shutdown()
    }

    /// Returns true when the client asked the daemon to quit.
    fn handle(
        stream: TcpStream,
        takes: &Arc<Mutex<HashMap<String, Running>>>,
        hooks: &Arc<Mutex<HashMap<String, HookRunning>>>,
    ) -> Result<bool> {
        let mut out = stream.try_clone()?;
        let reader = BufReader::new(stream);
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let req: serde_json::Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => {
                    reply(
                        &mut out,
                        serde_json::json!({ "error": format!("bad json: {e}") }),
                    )?;
                    continue;
                }
            };
            let cmd = req.get("cmd").and_then(|c| c.as_str()).unwrap_or("");
            let answer = match cmd {
                "check" => preflight::displays()
                    .map(|d| serde_json::to_value(d).unwrap())
                    .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() })),
                "windows" => list_windows()
                    .map(|w| serde_json::to_value(w).unwrap())
                    .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() })),
                "start" => match serde_json::from_value::<TakeConfig>(req.clone()) {
                    Ok(cfg) => {
                        let name = cfg.name.clone();
                        let mut map = takes.lock().unwrap();
                        if map.contains_key(&name) {
                            serde_json::json!({ "error": format!("take already running: {name}") })
                        } else {
                            match start_take(cfg) {
                                Ok(r) => {
                                    map.insert(name.clone(), r);
                                    serde_json::json!({ "started": name })
                                }
                                Err(e) => serde_json::json!({ "error": e.to_string() }),
                            }
                        }
                    }
                    Err(e) => serde_json::json!({ "error": format!("bad take config: {e}") }),
                },
                "stop" => {
                    let name = req
                        .get("take")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string();
                    let running = takes.lock().unwrap().remove(&name);
                    match running {
                        None => serde_json::json!({ "error": format!("no such take: {name}") }),
                        Some(r) => {
                            r.stop.store(true, Ordering::Relaxed);
                            match r.thread.join() {
                                Ok(Ok(report)) => serde_json::to_value(report).unwrap(),
                                Ok(Err(e)) => serde_json::json!({ "error": e }),
                                Err(_) => serde_json::json!({ "error": "take thread panicked" }),
                            }
                        }
                    }
                }
                // {"cmd":"hook-start","take":"s04-t1","pids":[123,456],
                //  "dir":"D:\\wow-sandbox\\rec","audio":true,"max_seconds":600}
                "hook-start" => {
                    let take = req
                        .get("take")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string();
                    let pids: Vec<u32> = req
                        .get("pids")
                        .and_then(|p| p.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_u64().map(|n| n as u32))
                                .collect()
                        })
                        .unwrap_or_default();
                    let dir = req
                        .get("dir")
                        .and_then(|d| d.as_str())
                        .unwrap_or("D:\\wow-sandbox\\rec")
                        .to_string();
                    let audio = req.get("audio").and_then(|a| a.as_bool()).unwrap_or(true);
                    // {"outputs": {"<pid>": "D:\\...\\take-i1.mp4"}} lets the
                    // caller name each file; without it they are named by pid.
                    let outputs: HashMap<u32, String> = req
                        .get("outputs")
                        .and_then(|o| o.as_object())
                        .map(|o| {
                            o.iter()
                                .filter_map(|(k, v)| {
                                    Some((k.parse().ok()?, v.as_str()?.to_string()))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    // An upper bound so a take nobody stops cannot fill the disk.
                    let cap = req
                        .get("max_seconds")
                        .and_then(|m| m.as_f64())
                        .unwrap_or(900.0);

                    if take.is_empty() || pids.is_empty() {
                        serde_json::json!({ "error": "hook-start needs take and pids" })
                    } else {
                        let mut map = hooks.lock().unwrap();
                        let busy: Vec<u32> = map
                            .values()
                            .flat_map(|h| h.pids.iter().copied())
                            .filter(|p| pids.contains(p))
                            .collect();
                        if map.contains_key(&take) {
                            serde_json::json!({ "error": format!("take already running: {take}") })
                        } else if !busy.is_empty() {
                            // One game cannot be in two takes: the second encoder
                            // would get every other frame.
                            serde_json::json!({ "error": "already recording", "pids": busy })
                        } else {
                            let stop = Arc::new(AtomicBool::new(false));
                            let files: Vec<(u32, String)> = pids
                                .iter()
                                .map(|&p| {
                                    let f = outputs
                                        .get(&p)
                                        .cloned()
                                        .unwrap_or_else(|| default_name(p, None, Some(&dir)));
                                    (p, f)
                                })
                                .collect();
                            let (s2, t) = (stop.clone(), take.clone());
                            let thread = std::thread::spawn(move || {
                                record_hooks_to(&files, Duration::from_secs_f64(cap), audio, s2)
                                    .map(|mut v| {
                                        for r in &mut v {
                                            r.take = Some(t.clone());
                                        }
                                        v
                                    })
                                    .map_err(|e| e.to_string())
                            });
                            map.insert(
                                take.clone(),
                                HookRunning {
                                    pids: pids.clone(),
                                    stop,
                                    thread,
                                },
                            );
                            serde_json::json!({ "started": take, "pids": pids })
                        }
                    }
                }
                "hook-stop" => {
                    let take = req
                        .get("take")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string();
                    let running = hooks.lock().unwrap().remove(&take);
                    match running {
                        None => serde_json::json!({ "error": format!("no such take: {take}") }),
                        Some(h) => {
                            h.stop.store(true, Ordering::Relaxed);
                            match h.thread.join() {
                                Ok(Ok(reports)) => serde_json::to_value(reports).unwrap(),
                                Ok(Err(e)) => serde_json::json!({ "error": e }),
                                Err(_) => serde_json::json!({ "error": "take thread panicked" }),
                            }
                        }
                    }
                }
                "status" => {
                    let map = takes.lock().unwrap();
                    let running: Vec<_> = map
                        .iter()
                        .map(|(name, r)| {
                            serde_json::json!({
                                "take": name,
                                "targets": r.progress.lock().unwrap().iter()
                                    .map(|(l, kept, dropped)| serde_json::json!({
                                        "label": l, "frames": kept, "dropped": dropped }))
                                    .collect::<Vec<_>>(),
                            })
                        })
                        .collect();
                    let hook_running: Vec<_> = hooks
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|(name, h)| serde_json::json!({ "take": name, "pids": h.pids }))
                        .collect();
                    serde_json::json!({ "running": running, "hooks": hook_running })
                }
                "quit" => {
                    let mut names: Vec<String> = takes.lock().unwrap().keys().cloned().collect();
                    names.extend(hooks.lock().unwrap().keys().cloned());
                    if !names.is_empty() {
                        // Refuse rather than silently truncate someone's take.
                        reply(
                            &mut out,
                            serde_json::json!({
                                "error": "takes still running; stop them first",
                                "running": names
                            }),
                        )?;
                        continue;
                    }
                    reply(&mut out, serde_json::json!({ "quit": true }))?;
                    return Ok(true);
                }
                other => serde_json::json!({ "error": format!("unknown command: {other}") }),
            };
            reply(&mut out, answer)?;
        }
        Ok(false)
    }

    fn reply(out: &mut TcpStream, v: serde_json::Value) -> Result<()> {
        out.write_all(serde_json::to_string(&v).unwrap().as_bytes())?;
        out.write_all(b"\n")?;
        out.flush()?;
        Ok(())
    }
}
