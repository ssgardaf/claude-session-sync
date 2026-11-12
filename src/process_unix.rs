//! The process list on macOS (libproc) and Linux (`/proc`).

use std::path::Path;

/// The file name of a program, lower case.
fn program_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

/// `(pid, full path of its program)` of every process whose program can be told. Processes
/// of other users may be missing: their programs cannot always be asked for. None if the
/// list could not be read at all.
#[cfg(target_os = "macos")]
fn all_processes() -> Option<Vec<(u32, String)>> {
    // SAFETY: the buffers are valid for the sizes passed, and the calls write no more.
    unsafe {
        let wanted = libc::proc_listallpids(std::ptr::null_mut(), 0);
        if wanted <= 0 {
            return None;
        }
        // (room for the processes started in between)
        let mut pids = vec![0 as libc::pid_t; wanted as usize + 64];
        let bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        let got = libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes);
        if got <= 0 {
            return None;
        }
        pids.truncate(got as usize);
        let mut out = Vec::new();
        let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        for pid in pids {
            if pid <= 0 {
                continue;
            }
            let n = libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32);
            if n > 0 {
                let path = String::from_utf8_lossy(&buf[..n as usize]).into_owned();
                out.push((pid as u32, path));
            }
        }
        Some(out)
    }
}

#[cfg(not(target_os = "macos"))]
fn all_processes() -> Option<Vec<(u32, String)>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        // (not to be read for a process of another user, or one that has just ended)
        if let Ok(exe) = std::fs::read_link(entry.path().join("exe")) {
            let path = exe.to_string_lossy().into_owned();
            // a program that was replaced on disk while it runs is listed with this ending
            let path = path.strip_suffix(" (deleted)").unwrap_or(&path).to_string();
            out.push((pid, path));
        }
    }
    Some(out)
}

/// `(pid, full path)` of the running processes whose program's file name is in `names`
/// (lower case). None if the list could not be read.
pub fn processes(names: &[&str]) -> Option<Vec<(u32, Option<String>)>> {
    Some(
        all_processes()?
            .into_iter()
            .filter(|(_, path)| names.contains(&program_name(path).as_str()))
            .map(|(pid, path)| (pid, Some(path)))
            .collect(),
    )
}

/// Is this the desktop app's own program? Its helpers do not count (the browser's
/// native-messaging host stays behind when the app is closed), nor does the command-line
/// tool of the same name.
#[cfg(target_os = "macos")]
pub fn is_desktop_app(path: &str) -> bool {
    path.ends_with("/Claude.app/Contents/MacOS/Claude")
}

/// Is this the desktop app's own program? It is called `claude-desktop` (or `claude`) and,
/// unlike the command-line tool, has the app's bundle beside it, as every Electron app has.
#[cfg(not(target_os = "macos"))]
pub fn is_desktop_app(path: &str) -> bool {
    if !matches!(program_name(path).as_str(), "claude-desktop" | "claude") {
        return false;
    }
    Path::new(path).parent().is_some_and(|dir| {
        let resources = dir.join("resources");
        resources.join("app.asar").exists() || resources.join("app").is_dir()
    })
}

/// True if the Claude desktop app has a process. When that cannot be told, true (the
/// cautious answer).
pub fn desktop_app_running() -> bool {
    match all_processes() {
        Some(found) => found.iter().any(|(_, path)| is_desktop_app(path)),
        None => true,
    }
}

/// When a process was started, in seconds since 1970. None if that cannot be told.
#[cfg(target_os = "macos")]
pub fn start_time(pid: u32) -> Option<f64> {
    // SAFETY: proc_bsdinfo is plain data, and the call is told its size.
    unsafe {
        let mut info: libc::proc_bsdinfo = std::mem::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let got = libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        );
        if got != size {
            return None;
        }
        Some(info.pbi_start_tvsec as f64 + info.pbi_start_tvusec as f64 / 1e6)
    }
}

/// The time on one of the system's clocks, in seconds.
#[cfg(not(target_os = "macos"))]
fn clock(id: libc::clockid_t) -> Option<f64> {
    // SAFETY: timespec is plain data, owned by this function.
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    if unsafe { libc::clock_gettime(id, &mut ts) } != 0 {
        return None;
    }
    Some(ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9)
}

/// The start time in a line of `/proc/<pid>/stat`, in clock ticks since the system started.
/// It is the 22nd field; the second is the program's name in brackets, which may itself
/// hold spaces and brackets.
#[cfg(not(target_os = "macos"))]
fn start_ticks(stat: &str) -> Option<f64> {
    let after_name = &stat[stat.rfind(')')? + 1..];
    after_name.split_whitespace().nth(19)?.parse().ok()
}

/// When a process was started, in seconds since 1970. None if that cannot be told.
#[cfg(not(target_os = "macos"))]
pub fn start_time(pid: u32) -> Option<f64> {
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    let ticks = start_ticks(&stat)?;
    // SAFETY: a call without pointers.
    let per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if per_second <= 0 {
        return None;
    }
    // how long the system has been up, on the clock that start times are counted on
    let up = clock(libc::CLOCK_BOOTTIME)?;
    let now = clock(libc::CLOCK_REALTIME)?;
    Some(now - (up - ticks / per_second as f64))
}

fn alive(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 sends nothing; it only asks whether the process can be signalled.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// End a process: it is asked to (SIGTERM), and if it is still there three seconds later it
/// is made to (SIGKILL). False if it could not be done.
pub fn terminate(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 1 {
        return false; // (0 and negative numbers mean whole groups of processes)
    }
    // SAFETY: plain calls; the pid is a single process.
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        return false;
    }
    for _ in 0..30 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if !alive(pid) {
            return true;
        }
    }
    unsafe { libc::kill(pid, libc::SIGKILL) == 0 || !alive(pid) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_is_on_the_list_under_its_programs_name() {
        let me = std::process::id();
        let exe = std::env::current_exe().unwrap();
        let name = exe.file_name().unwrap().to_string_lossy().to_lowercase();
        let found = processes(&[name.as_str()]).unwrap();
        let mine = found.iter().find(|(pid, _)| *pid == me).unwrap();
        assert!(mine
            .1
            .as_ref()
            .is_some_and(|p| p.ends_with(exe.file_name().unwrap().to_string_lossy().as_ref())));
        assert!(processes(&["no-such-program-anywhere"]).unwrap().is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn only_the_apps_own_program_counts_as_the_app() {
        assert!(is_desktop_app(
            "/Applications/Claude.app/Contents/MacOS/Claude"
        ));
        assert!(is_desktop_app(
            "/Users/someone/Applications/Claude.app/Contents/MacOS/Claude"
        ));
        // the browser's helper stays behind when the app is closed
        assert!(!is_desktop_app(
            "/Applications/Claude.app/Contents/Helpers/chrome-native-host"
        ));
        assert!(!is_desktop_app(
            "/Applications/Claude.app/Contents/Frameworks/Claude Helper.app/Contents/MacOS/Claude Helper"
        ));
        // the command-line tool, and other apps
        assert!(!is_desktop_app("/Users/someone/.local/bin/claude"));
        assert!(!is_desktop_app("/opt/homebrew/bin/claude"));
        assert!(!is_desktop_app(
            "/Applications/NotClaude.app/Contents/MacOS/NotClaude"
        ));
        assert!(!is_desktop_app(
            "/Applications/Claude.app/Contents/MacOS/Claude Helper"
        ));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn the_app_is_told_from_the_command_line_tool_by_what_is_beside_it() {
        let dir = std::env::temp_dir().join(format!(
            "css-proc-unit-{}-{}",
            std::process::id(),
            (crate::winfs::real_now() * 1e6) as u64
        ));
        let app = dir.join("opt").join("Claude");
        std::fs::create_dir_all(app.join("resources")).unwrap();
        std::fs::write(app.join("resources").join("app.asar"), b"").unwrap();
        let cli = dir.join("bin");
        std::fs::create_dir_all(&cli).unwrap();
        let text = |p: std::path::PathBuf| p.to_string_lossy().into_owned();
        assert!(is_desktop_app(&text(app.join("claude-desktop"))));
        assert!(is_desktop_app(&text(app.join("claude"))));
        assert!(!is_desktop_app(&text(app.join("chrome-native-host"))));
        assert!(!is_desktop_app(&text(cli.join("claude")))); // the command-line tool
        assert!(!is_desktop_app(&text(cli.join("claude-desktop"))));
        assert!(!is_desktop_app("claude"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn start_time_is_read_past_a_name_with_spaces_and_brackets() {
        let line = "4242 (a b) c) d) S 1 4242 4242 0 -1 4194304 100 0 0 0 1 2 0 0 20 0 1 0 987654 1000 10 18446744073709551615 0 0";
        assert_eq!(start_ticks(line), Some(987654.0));
        assert_eq!(start_ticks("4242 (x) S 1 2"), None);
        assert_eq!(start_ticks("nothing of the kind"), None);
    }

    #[test]
    fn a_process_that_is_asked_to_end_ends() {
        let mut child = std::process::Command::new("sleep")
            .arg("300")
            .spawn()
            .unwrap();
        let pid = child.id();
        assert!(start_time(pid).is_some());
        assert!(terminate(pid));
        let status = child.wait().unwrap();
        assert!(!status.success());
        assert!(!terminate(0));
        assert!(!terminate(1));
    }
}
