//! The process list: is the desktop app running, and is a given pid one of our daemons.
//!
//! What is asked of the system is here for Windows and in `process_unix.rs` for macOS and
//! Linux, under the same names.

#[cfg(windows)]
use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE};
#[cfg(windows)]
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, TerminateProcess,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
};

use crate::consts::START_SLACK_SEC;

#[cfg(unix)]
#[path = "process_unix.rs"]
mod unix;
#[cfg(unix)]
pub use unix::{
    command_names_the_app, counts_as_ours, desktop_app_running, is_desktop_app, is_the_app,
    lock_holder, processes, singleton_lock_live, start_time, terminate, this_user, Process,
};

/// `(pid, full path or None)` of the running processes whose file name is in `names`
/// (lower case). None if the list could not be read.
#[cfg(windows)]
pub fn processes(names: &[&str]) -> Option<Vec<(u32, Option<String>)>> {
    // SAFETY: plain Win32 calls on handles owned by this function; every buffer passed is
    // valid for the size given.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap.is_null() || snap == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut pids = Vec::new();
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut more = Process32FirstW(snap, &mut entry);
        while more != 0 {
            let len = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..len]).to_lowercase();
            if names.contains(&name.as_str()) {
                pids.push(entry.th32ProcessID);
            }
            more = Process32NextW(snap, &mut entry);
        }
        CloseHandle(snap);
        let mut out = Vec::new();
        for pid in pids {
            let mut path = None;
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if !h.is_null() {
                let mut buf = vec![0u16; 2048];
                let mut size = buf.len() as u32;
                if QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut size) != 0 {
                    path = Some(String::from_utf16_lossy(&buf[..size as usize]));
                }
                CloseHandle(h);
            }
            out.push((pid, path));
        }
        Some(out)
    }
}

/// True if the Claude desktop app has a process. When that cannot be told, true (the
/// cautious answer).
#[cfg(windows)]
pub fn desktop_app_running() -> bool {
    let Some(found) = processes(&["claude.exe"]) else {
        return true;
    };
    for (_, path) in found {
        // the command-line tool is also called claude.exe: tell them apart by path
        let Some(path) = path else {
            return true;
        };
        let p = path.to_lowercase();
        if p.contains("\\windowsapps\\claude_") || p.contains("\\anthropicclaude\\") {
            return true;
        }
    }
    false
}

/// When a process was started, in seconds since 1970. None if that cannot be told.
#[cfg(windows)]
pub fn start_time(pid: u32) -> Option<f64> {
    // SAFETY: the handle is checked and closed here; the four out pointers are valid.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let zero = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
        let ok = GetProcessTimes(h, &mut created, &mut exited, &mut kernel, &mut user) != 0;
        CloseHandle(h);
        if !ok {
            return None;
        }
        let ticks = ((created.dwHighDateTime as i64) << 32) | created.dwLowDateTime as i64;
        Some((ticks - 116_444_736_000_000_000) as f64 / 1e7)
    }
}

/// Is the process `pid` the daemon that reported, in status.json, that it started at
/// `started`? Its program must be one of `names`, and it must have been running by then
/// (give or take START_SLACK_SEC): a process that was given the pid of a daemon that has
/// since died was started later. Without a start time to go by, the answer is no.
pub fn is_daemon(pid: u32, names: &[&str], started: Option<f64>) -> bool {
    let Some(started) = started else {
        return false;
    };
    let named = processes(names).is_some_and(|list| list.iter().any(|(p, _)| *p == pid));
    named && start_time(pid).is_some_and(|created| created <= started + START_SLACK_SEC)
}

/// End a process. False if it could not be done.
#[cfg(windows)]
pub fn terminate(pid: u32) -> bool {
    // SAFETY: the handle is checked and closed here.
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if h.is_null() {
            return false;
        }
        let ok = TerminateProcess(h, 15) != 0;
        CloseHandle(h);
        ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::winfs::real_now;

    #[test]
    fn a_daemon_is_known_by_its_program_and_by_when_it_started() {
        let me = std::process::id();
        let exe = std::env::current_exe().unwrap();
        let name = exe.file_name().unwrap().to_string_lossy().to_lowercase();
        let created = start_time(me).unwrap();
        assert!(created <= real_now() && created > real_now() - 3600.0);
        assert!(is_daemon(me, &[name.as_str()], Some(real_now()))); // reported after it was started
        assert!(is_daemon(me, &[name.as_str()], Some(created - 2.0))); // clocks and rounding: a little slack
        assert!(!is_daemon(me, &[name.as_str()], Some(created - 60.0))); // reported by an earlier owner of the pid
        assert!(!is_daemon(me, &[name.as_str()], None)); // nothing to go by
        assert!(!is_daemon(
            me,
            &["some-other-program.exe"],
            Some(real_now())
        ));
        assert!(!is_daemon(
            me,
            &["python.exe", "pythonw.exe", "python", "python3"],
            Some(real_now())
        ));
    }
}
