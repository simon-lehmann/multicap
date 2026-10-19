//! Load the graphics hook into a running game.
//!
//!   multicap-inject list                    32-bit processes that could be hooked
//!   multicap-inject <pid> <path-to-dll>     load the hook into one
//!
//! The classic sequence: write the DLL's path into the target's address space,
//! then start a thread there whose entry point is `LoadLibraryW` and whose
//! argument is that path. Windows does the rest.
//!
//! This is built 32-bit because the game is. A DLL has to match the
//! architecture of the process it lives in, and `CreateRemoteThread` from a
//! 64-bit process into a 32-bit one has to go the long way round — which is why
//! OBS ships `inject-helper32.exe` and `inject-helper64.exe` rather than one.

use std::ffi::c_void;

use windows::core::{s, HSTRING, PCSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
use windows::Win32::System::Memory::{
    VirtualAllocEx, VirtualFreeEx, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
};
use windows::Win32::System::Threading::{
    CreateRemoteThread, GetExitCodeThread, OpenProcess, WaitForSingleObject, INFINITE,
    LPTHREAD_START_ROUTINE, PROCESS_CREATE_THREAD, PROCESS_QUERY_INFORMATION, PROCESS_VM_OPERATION,
    PROCESS_VM_READ, PROCESS_VM_WRITE,
};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let as_str: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match as_str.as_slice() {
        ["list"] => list(),
        ["status", pid] => parse_pid(pid).and_then(status),
        [pid, dll] => parse_pid(pid).and_then(|p| inject(p, dll)),
        _ => {
            eprintln!(
                "usage: multicap-inject list\n       multicap-inject <pid> <dll>\n       multicap-inject status <pid>"
            );
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        println!("{{\"error\": {:?}}}", e);
        std::process::exit(1);
    }
}

fn parse_pid(s: &str) -> Result<u32, String> {
    s.parse()
        .map_err(|_| format!("pid must be a number, got {s:?}"))
}

fn list() -> Result<(), String> {
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
        .map_err(|e| format!("snapshot: {e}"))?;
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut out: Vec<String> = Vec::new();
    unsafe {
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let n = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
                let name = String::from_utf16_lossy(&entry.szExeFile[..n]);
                out.push(format!(
                    "{{\"pid\": {}, \"name\": {:?}}}",
                    entry.th32ProcessID, name
                ));
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    println!("[{}]", out.join(", "));
    Ok(())
}

fn inject(pid: u32, dll: &str) -> Result<(), String> {
    let full = std::fs::canonicalize(dll)
        .map_err(|e| format!("cannot find {dll}: {e}"))?
        .to_string_lossy()
        // canonicalize gives a \\?\ prefix, which LoadLibraryW does not want.
        .trim_start_matches(r"\\?\")
        .to_string();

    let wide: Vec<u16> = HSTRING::from(full.as_str()).as_wide().to_vec();
    let bytes = (wide.len() + 1) * 2;

    unsafe {
        let proc: HANDLE = OpenProcess(
            PROCESS_CREATE_THREAD
                | PROCESS_QUERY_INFORMATION
                | PROCESS_VM_OPERATION
                | PROCESS_VM_READ
                | PROCESS_VM_WRITE,
            false,
            pid,
        )
        .map_err(|e| format!("OpenProcess({pid}): {e} (run as the same user, or elevated)"))?;

        let remote = VirtualAllocEx(proc, None, bytes, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
        if remote.is_null() {
            let _ = CloseHandle(proc);
            return Err("VirtualAllocEx failed".into());
        }

        let mut written = 0usize;
        windows::Win32::System::Diagnostics::Debug::WriteProcessMemory(
            proc,
            remote,
            wide.as_ptr() as *const c_void,
            bytes,
            Some(&mut written),
        )
        .map_err(|e| format!("WriteProcessMemory: {e}"))?;

        // kernel32 sits at the same address in every process of the same
        // architecture for the life of a boot, so our LoadLibraryW is theirs.
        let kernel32: HMODULE =
            GetModuleHandleA(s!("kernel32.dll")).map_err(|e| format!("kernel32: {e}"))?;
        let load = GetProcAddress(kernel32, s!("LoadLibraryW"))
            .ok_or("LoadLibraryW not found".to_string())?;
        let start: LPTHREAD_START_ROUTINE = Some(std::mem::transmute(load));

        let thread = CreateRemoteThread(proc, None, 0, start, Some(remote), 0, None)
            .map_err(|e| format!("CreateRemoteThread: {e}"))?;
        WaitForSingleObject(thread, INFINITE);

        let mut module = 0u32;
        let _ = GetExitCodeThread(thread, &mut module);
        let _ = CloseHandle(thread);
        let _ = VirtualFreeEx(proc, remote, 0, MEM_RELEASE);
        let _ = CloseHandle(proc);

        if module == 0 {
            return Err(format!(
                "LoadLibraryW returned 0 in pid {pid}: the DLL did not load (wrong architecture, \
or a dependency is missing)"
            ));
        }
        println!("{{\"injected\": {pid}, \"module\": {module}}}");
    }
    Ok(())
}

#[allow(dead_code)]
fn unused(_: PCSTR) {}

/// Read what the hook inside `pid` is reporting about itself.
///
/// There is no way to attach a debugger to a hook living in someone's game, so
/// this is how we find out whether it is working and, if not, why.
fn status(pid: u32) -> Result<(), String> {
    use multicap_ipc::{mapping_name, Header, HookError, Namespace, MAGIC};
    use windows::Win32::System::Memory::{MapViewOfFile, OpenFileMappingW, FILE_MAP_READ};

    unsafe {
        // The hook puts its memory in whichever namespace it was allowed to use,
        // so look in both. Reading from another Windows session only works if it
        // managed Global.
        let mut found = None;
        for ns in Namespace::candidates() {
            if let Ok(m) = OpenFileMappingW(
                FILE_MAP_READ.0,
                false,
                &HSTRING::from(mapping_name(ns, pid)),
            ) {
                found = Some(m);
                break;
            }
        }
        let mapping = found.ok_or_else(|| {
            format!(
                "no hook found in pid {pid} from this session (it may be running in another \
Windows session, where only a Global mapping is visible)"
            )
        })?;
        let view = MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, Header::size());
        if view.Value.is_null() {
            return Err("could not map the hook's shared memory".into());
        }
        let h = &*(view.Value as *const Header);
        if h.magic != MAGIC {
            return Err(format!("shared memory is not ours (magic {:#x})", h.magic));
        }
        let info = h.info[if h.ready_slot == u32::MAX {
            0
        } else {
            h.ready_slot as usize
        }];
        println!(
            "{{\"pid\": {}, \"active\": {}, \"captured\": {}, \"dropped\": {}, \
\"error\": {:?}, \"hresult\": \"{:#010x}\", \"width\": {}, \"height\": {}, \"pitch\": {}, \"format\": {}}}",
            pid,
            h.active == 1,
            h.captured,
            h.dropped,
            HookError::describe(h.last_error),
            h.last_hresult,
            info.width,
            info.height,
            info.pitch,
            info.format
        );
    }
    Ok(())
}
