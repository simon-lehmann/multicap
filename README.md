# multicap

Record many Windows application windows at once — each to its own file, with each
application's own audio — on the GPU's hardware encoder, from one small process.

Built for filming several game clients side by side on one machine, where OBS is
too heavy to run many of and an `ffmpeg` per window spends CPU you would rather
give to the games.

```
multicap check                      # can this machine capture at all?
multicap windows                    # capturable windows: handle, pid, title, size
multicap record --config take.json --seconds 12
multicap serve --port 7788          # take orders as newline-delimited JSON
```

Everything prints JSON. No GUI, no scene graph, no config file format to learn.

## What it does that is worth having

**Each window's own picture, even when covered.** Windows Graphics Capture reads a
window's own buffer, not the screen in front of it. Eight clients can overlap on
one desktop and each file still shows its own game. Window layout stops being
something you have to manage.

**Each application's own sound.** Windows process loopback records exactly one
program's output — the mechanism behind OBS's application audio — so eight clients
give eight separate tracks, with no virtual cable and no mixer.

**The frame never leaves the GPU.** The captured texture is handed to Media
Foundation's hardware encoder on the same D3D11 device it was captured on. Nothing
is copied through system memory and nothing is encoded on the CPU. Media Foundation
picks up whatever encoder the machine has — AMD AMF, NVIDIA NVENC, Intel QSV —
which is why this is not tied to one vendor.

**Honest frame timing.** A capture pinned to 60 fps against a game drawing ~60 on a
100 Hz monitor lands on ~50: one frame in six a duplicate, one game frame in five
lost. So every frame the compositor offers is kept, with its own timestamp, and the
jitter is evened out afterwards between anchors a couple of seconds apart — long-run
drift preserved, per-frame wobble removed. (`src/timing.rs`, unit-tested.)

**Sound and picture lined up by arithmetic, not by calibration.** Video frames carry
`SystemRelativeTime`; audio buffers carry WASAPI's `qpcPosition`. Both come off the
same performance counter, so the offset between them is computed per take and
written into the report. Nothing has to be measured once with a clap and hard-coded.

**Writes where you tell it.** Point `dir` at a UNC path and takes are encoded
straight onto another machine, so there is no copy step after a shot.

**It refuses to record nothing.** See below.

## Headless: read this first

Graphics capture takes its picture from the desktop compositor, and the compositor
only composes when the machine has an **active display output**. With every monitor
switched off or unplugged, there is no output: capture starts cleanly, reports no
error, and writes zero frames. A monitor switched off at its power button drops off
the bus and counts as unplugged.

No capture API fixes this — not Graphics Capture, not Desktop Duplication, not GDI.
It is not a recorder problem. Give the machine an output:

- an **HDMI/DisplayPort dummy plug** in a spare port (hardware, no driver), or
- a **virtual display driver** (software, an install).

Either also ends the 1024x768 fallback desktop and lets you pick the resolution you
record at.

`multicap check` detects both conditions and exits 3, and every take refuses to
start rather than produce an empty file.

## What a take produces

One `.mp4` per filmed window, one `.wav` per recorded application, and one JSON
report:

```jsonc
{
  "name": "s04-t1",
  "tracks": [{
    "label": "i1",
    "video_file": "\\\\spark\\rec\\s04-t1-i1.mp4",
    "audio": { "path": "...wav", "seconds": 12.04, "glitches": 0 },
    "cadence": { "frames": 722, "seconds": 12.01, "fps": 60.1, "gaps": [], "dropped": 0 },
    "constant_quality": true,
    "audio_offset_seconds": -0.0184,
    "evened_seconds": [0.0, 0.0166, 0.0333, ...]
  }]
}
```

`gaps` are stretches where the application did not draw; `dropped` must be 0 —
anything else means the encoder could not keep up and the take is suspect. Read
those two numbers before you keep a take.

**Muxing is deliberately not done here.** Joining picture and sound is cheap on any
machine; the recording machine's spare capacity is not. The report carries
everything needed to do it wherever you like:

```
ffmpeg -i take-i1.mp4 -itsoffset <audio_offset_seconds> -i take-i1.wav \
       -c:v copy -c:a flac -shortest take-i1.mkv
```

Audio stays lossless 32-bit float WAV until you decide otherwise.

## Status

Written and compiling; **not yet run against real hardware**. Nothing below is
claimed to work until it has been measured on a machine.

| What | State |
|---|---|
| Builds for `x86_64-pc-windows-msvc` | yes, including cross-compiled from Linux |
| Frame cadence logic | unit-tested on every platform in CI |
| Capture, encode, audio, daemon | compile; unproven at runtime |
| Constant-quality rate control | best effort — support varies by vendor and driver; the report says which mode was actually used |
| Headless with a dummy plug or virtual display | untested |
| More than a handful of simultaneous windows | untested |
| 4K | untested |

## Building

Normally, on Windows:

```
cargo build --release
```

Cross-compiling from Linux, which needs no Microsoft toolchain — the `windows`
crate ships its own import libraries, `xwin` supplies the CRT ones and `rust-lld`
links:

```
rustup target add x86_64-pc-windows-msvc
cargo install xwin && xwin --accept-license splat --output ~/.xwin
cargo build --release --target x86_64-pc-windows-msvc
```

(`.cargo/config.toml` points at `~/.xwin`; change the paths if yours is elsewhere.)

## Requirements

Windows 10 2004 (build 19041) or newer, for process loopback audio. A GPU with a
hardware video encoder. An active display output — see above.

## Licence

MIT or Apache-2.0, at your option.
