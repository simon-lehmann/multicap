# multicap

Record many Windows application windows at once — each to its own file, with each
application's own audio — on the GPU's hardware encoder, from one small process.

Built for filming several game clients side by side on one machine, where OBS is
too heavy to run many of and an `ffmpeg` per window spends CPU you would rather
give to the games. Recording four games at once measured at about a quarter of
one core — roughly an eighth of what the games themselves cost.

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

## Headless: two ways, and why there are two

Graphics capture takes its picture from the desktop compositor, and the compositor
only composes when the machine has an **active display output**. With every monitor
switched off or unplugged there is no output: capture starts cleanly, reports no
error, and writes zero frames. A monitor switched off at its power button drops off
the bus and counts as unplugged. No screen-capture API gets around this — not
Graphics Capture, not Desktop Duplication, not GDI.

`multicap check` detects it and exits 3, and a window take refuses to start rather
than write an empty file.

**If you can give the machine an output**, do: an HDMI/DisplayPort dummy plug, or a
virtual display driver. Window capture then works normally, the frame never leaves
the GPU, and you choose the resolution you record at.

**If you cannot**, use the graphics hook (`crates/hook`). It is loaded into the game
and patches the Direct3D call that hands a finished frame to the display, so the
frame is taken from the game's own swap chain before the compositor exists in the
story at all. No display output is needed, ever. This is how OBS's game capture
works, and it is why that records with a monitor switched off.

The hook costs something: plain Direct3D 9 has no shared textures, so the frame is
read back into system memory rather than staying on the GPU. Use window capture
when you can and the hook when you must.

### Resolution, and why the offset is cached

With no display output Windows falls back to a 1024x768 desktop, and a *windowed*
game renders at that size — the hook faithfully captures a small picture. Put the
game in **fullscreen** at the resolution you want and it renders at that size
regardless of what is plugged in.

That creates a second problem. Finding `Present` means reading a real vtable, which
means having a device, and a game in exclusive fullscreen owns the adapter: a second
device created inside it fails with `D3DERR_DEVICELOST`. So the hook records the
function's *offset* within d3d9.dll the first time it succeeds — while some client
is windowed — into `%TEMP%\multicap-d3d9-offsets.txt`, and afterwards adds that
offset to wherever d3d9.dll happens to be loaded. The offset is stable for a given
DLL; the base is not, because of address space randomisation, so only the offset is
stored. This is the same split OBS makes with its `get-graphics-offsets` helper.

In short: run one windowed client once to learn the offset, then fullscreen clients
capture at full resolution with no display attached.

### But fullscreen is a trap if you record more than one thing

Exclusive fullscreen gets you full resolution, and it is wrong for this tool anyway:

- A fullscreen Direct3D 9 device is **lost whenever the game stops being the
  foreground window**, and a machine with no display may never hand it the
  foreground back. It then sits there, alive and responding, presenting nothing.
- Only **one** process can own the adapter exclusively, so several games cannot all
  be fullscreen. Recording several at once is the entire point of this tool.

Windowed clients have neither problem: they keep presenting while unfocused and
while completely covered, which is what makes many-at-once work. But a windowed
game sizes its back buffer from the desktop when it creates its device, and
resizing its window afterwards does **not** make it render larger — tested, the
back buffer stays put.

So: for one game at full resolution, fullscreen plus a cached offset needs nothing
else. For **several** games at full resolution with no monitor, give Windows a
desktop of the size you want to record — a virtual display driver does that in
software, with every monitor still switched off.

```
multicap-inject list                 # processes that can be hooked
multicap-inject <pid> multicap_hook.dll
multicap-inject status <pid>         # what the hook inside that process reports
```

The hook also writes `%TEMP%\multicap-hook-<pid>.json` once a second, because
there is no way to attach a debugger to a hook living inside somebody's game, and
named objects in the `Local\` namespace cannot be read from another session.

## What a take produces

One `.mp4` per filmed window, one `.wav` per recorded application, and one JSON
report:

```jsonc
{
  "name": "s04-t1",
  "tracks": [{
    "label": "i1",
    "video_file": "\\\\nas\\takes\\s04-t1-i1.mp4",
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

| What | State |
|---|---|
| Builds for `x86_64-pc-windows-msvc` | yes, including cross-compiled from Linux |
| Frame cadence logic | unit-tested on every platform in CI |
| Shared memory layout identical 32-bit and 64-bit | asserted at compile time, both targets |
| **Graphics hook on a Direct3D 9 game, monitor off** | **proven** 19 Oct 2026: injected into WoW 3.3.5a (32-bit, D3D9) on a machine with no display output. 289 frames, 289 `Present` calls, 0 dropped, correct colour and orientation |
| **Full resolution with no display at all** | **proven** 19 Oct 2026: 1920x1080, 362 frames, 0 dropped, monitor off. Needs the game in fullscreen *and* a cached offset — see below |
| Reading the hook from another Windows session | not possible in practice: the hook could not create a `Global\` mapping (no SeCreateGlobalPrivilege for an ordinary process), so the recorder must run in the same session as the game |
| **Hook to finished file, end to end** | **proven** 19 Oct 2026: 600 frames in 10.01 s (59.81 fps), 0 timed out, 0 torn, 0 dropped. HEVC Main, bt709, limited range, labelled correctly at the source |
| **Constant-quality rate control** | **proven** on AMD (RDNA 4): the encoder accepted a fixed quantiser. Still best effort elsewhere, and the report says which mode was actually used |
| **Four games recorded at once** | **proven** 19 Oct 2026: four WoW clients, 2337 frames in 10 s (57.5–58.6 fps each), 0 dropped, 0 torn, 0 waits timed out, one file per client |
| **Cost** | **measured** 19 Oct 2026: 2.73 s of CPU for 10.2 s of wall clock recording four clients — about 0.27 of one core, against 2.23 cores for the four games themselves. The recorder costs roughly an eighth of what it records |
| Window capture, audio, daemon | compile; unproven at runtime |
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

Copy `.cargo/config.toml.example` to `.cargo/config.toml` and put your own home
directory in the three paths; cargo does not expand `~` or environment variables
there.

## Requirements

Windows 10 2004 (build 19041) or newer, for process loopback audio. A GPU with a
hardware video encoder. An active display output — see above.

## Licence

MIT or Apache-2.0, at your option.
