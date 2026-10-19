//! The channel between a hook living inside a game process and the recorder.
//!
//! Deliberately tiny and C-layout: one side of this is compiled 32-bit and
//! injected into the game, the other is the 64-bit recorder, and they share one
//! block of memory. Anything whose size differs between the two architectures —
//! a pointer, a `usize`, an enum without a fixed representation — would silently
//! disagree about where the fields are. Every field here is fixed width.
//!
//! Layout: a header, then two frame slots. The hook writes whichever slot the
//! recorder is not reading, then publishes its index. One writer, one reader,
//! no lock on the hot path: a game's `Present` must never block on us.

#![cfg_attr(not(windows), allow(dead_code))]

/// Bumped whenever the layout below changes. The recorder refuses a hook that
/// does not match, which is cheaper than debugging a field-offset mismatch.
pub const PROTOCOL_VERSION: u32 = 2;

pub const MAGIC: u32 = 0x4d43_4150; // "MCAP"

/// Two slots is enough: the hook fills one while the recorder drains the other.
pub const SLOTS: usize = 2;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameInfo {
    pub width: u32,
    pub height: u32,
    /// Bytes per row, which is not width * 4: the GPU pads rows.
    pub pitch: u32,
    /// D3DFMT value the surface was locked as.
    pub format: u32,
    /// QueryPerformanceCounter when the frame was taken, in counter ticks.
    pub qpc: i64,
    /// Monotonic counter, so the recorder can see frames it missed.
    pub sequence: u64,
}

#[repr(C)]
pub struct Header {
    pub magic: u32,
    pub protocol: u32,
    /// Set by the hook once it is actually capturing.
    pub active: u32,
    /// Slot the hook most recently finished writing, or u32::MAX for none yet.
    pub ready_slot: u32,
    /// Bytes reserved for each slot's pixels.
    pub slot_bytes: u32,
    /// Why the hook is not capturing, as a `HookError` value. There is no way
    /// to attach a debugger inside someone's game, so the hook says here what
    /// went wrong and the recorder prints it.
    pub last_error: u32,
    pub info: [FrameInfo; SLOTS],
    /// Frames the hook captured, and frames it had to throw away because the
    /// recorder had not taken the previous one. Both visible without IPC.
    pub captured: u64,
    pub dropped: u64,
    /// The raw HRESULT behind `last_error`, when there was one.
    pub last_hresult: i32,
    pub _pad: u32,
    /// Times our detour on `IDirect3DDevice9::Present` was entered, counted
    /// before anything else can go wrong. Distinguishes "the hook is on the
    /// wrong function" from "the hook fires but capture fails".
    pub present_calls: u64,
    /// The same for `IDirect3DSwapChain9::Present`, which some games use
    /// instead. Exactly one of these two should move.
    pub swap_present_calls: u64,
}

impl Header {
    pub const fn size() -> usize {
        core::mem::size_of::<Header>()
    }
}

// Checked when the crate is compiled, for every target it is compiled for. The
// 32-bit hook and the 64-bit recorder each build this file, so a layout that
// differed between them would fail the build rather than corrupt frames.
const _: () = {
    assert!(core::mem::size_of::<FrameInfo>() == 32);
    assert!(core::mem::align_of::<FrameInfo>() == 8);
    assert!(core::mem::size_of::<Header>() == 24 + 32 * SLOTS + 40);
    assert!(core::mem::align_of::<Header>() == 8);
};

/// What the hook reports in `Header::last_error`.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookError {
    None = 0,
    /// d3d9.dll is not loaded in the target: it is not a Direct3D 9 game.
    NoD3d9 = 1,
    /// Could not make a throwaway device to read the vtable from.
    NoDummyDevice = 2,
    /// The detour could not be placed on Present.
    HookFailed = 3,
    /// The back buffer could not be read; `last_hresult` says what the driver said.
    BackBufferFailed = 4,
    /// A frame arrived larger than the slot reserved for it.
    FrameTooLarge = 5,
    /// The multisampled back buffer could not be resolved to a plain one.
    ResolveFailed = 6,
    /// A read-back surface could not be created.
    CreateSurfaceFailed = 7,
    /// The resolved surface could not be copied to system memory.
    ReadBackFailed = 8,
    /// The system-memory surface could not be locked.
    LockFailed = 9,
}

impl HookError {
    pub fn describe(code: u32) -> &'static str {
        match code {
            0 => "no error",
            1 => "d3d9.dll is not loaded in the target process",
            2 => "could not create a throwaway D3D9 device to read the vtable",
            3 => "could not place the detour on IDirect3DDevice9::Present",
            4 => "could not read the back buffer (see last_hresult)",
            5 => "frame larger than the reserved slot",
            6 => "could not resolve the multisampled back buffer (StretchRect)",
            7 => "could not create a read-back surface",
            8 => "could not copy the frame to system memory (GetRenderTargetData)",
            9 => "could not lock the system-memory surface",
            _ => "unknown error",
        }
    }
}

/// Which Windows object namespace the hook's shared memory lives in.
///
/// `Local\` is private to one session. That is the right place for it when the
/// recorder runs beside the games, which is the normal case, but it makes the
/// hook invisible to anything in another session. `Global\` can be read from
/// any session; creating a name there needs SeCreateGlobalPrivilege, which an
/// ordinary interactive process does not have, so it is attempted and not
/// relied on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Namespace {
    Global,
    Local,
}

impl Namespace {
    pub fn prefix(self) -> &'static str {
        match self {
            Namespace::Global => "Global\\",
            Namespace::Local => "Local\\",
        }
    }

    /// Tried in this order: a reader in another session needs Global, and a
    /// reader beside the game works with either.
    pub fn candidates() -> [Namespace; 2] {
        [Namespace::Global, Namespace::Local]
    }
}

/// Names are per target process, so several games can be hooked at once.
pub fn mapping_name(ns: Namespace, pid: u32) -> String {
    format!("{}multicap_frames_{pid}", ns.prefix())
}

/// Signalled by the hook when a slot has been published.
pub fn frame_event_name(ns: Namespace, pid: u32) -> String {
    format!("{}multicap_frame_{pid}", ns.prefix())
}

/// Signalled by the recorder to ask the hook to stop and unload.
pub fn stop_event_name(ns: Namespace, pid: u32) -> String {
    format!("{}multicap_stop_{pid}", ns.prefix())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of this crate: 32-bit hook and 64-bit recorder must agree.
    /// These numbers are checked on whatever host runs the tests, and the same
    /// assertions compile into both builds.
    #[test]
    fn layout_is_architecture_independent() {
        assert_eq!(core::mem::size_of::<FrameInfo>(), 32);
        assert_eq!(core::mem::align_of::<FrameInfo>(), 8);
        assert_eq!(core::mem::size_of::<Header>(), 24 + 32 * SLOTS + 40);
        assert_eq!(core::mem::align_of::<Header>(), 8);
    }

    #[test]
    fn names_are_per_process_and_per_namespace() {
        assert_ne!(
            mapping_name(Namespace::Local, 100),
            mapping_name(Namespace::Local, 101)
        );
        assert!(mapping_name(Namespace::Local, 42).starts_with("Local\\"));
        assert!(mapping_name(Namespace::Global, 42).starts_with("Global\\"));
    }
}
