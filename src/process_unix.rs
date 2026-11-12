//! The process list on macOS (libproc) and Linux (`/proc`).
//!
//! Whether the desktop app is running decides whether the records in the folder it has
//! loaded may be replaced, so the question is answered cautiously: where there is doubt, the
//! app is running. It is taken to be running if any of these holds:
//!
//! * a process of this user runs the app's own program (macOS: a program called `Claude` in
//!   the `Contents/MacOS` folder of any application bundle; Linux: a program called
//!   `claude-desktop` or `claude` with the app's bundle beside it, or any program, such as
//!   `electron`, whose command line names an `app.asar` with "claude" in its path);
//! * the lock Chromium keeps in the app's data folder while it runs (`SingletonLock`, a link
//!   whose target ends in the process's number) names a process that is there;
//! * the process list cannot be read.
//!
//! Processes of other users do not count: their app has its own store. A process whose
//! owner cannot be told does.

use std::path::Path;

/// The file name of a program, lower case.
fn program_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

/// A running process, as far as it can be told.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    /// The full path of its program; empty if that cannot be told.
    pub path: String,
    /// The user it runs as, if that can be told.
    pub owner: Option<u32>,
    /// Its command line (Linux; empty elsewhere and where it cannot be read).
    pub command: Vec<String>,
}

/// The user this process runs as.
pub fn this_user() -> u32 {
    // SAFETY: a call without arguments that cannot fail.
    unsafe { libc::getuid() }
}

/// Every running process whose program can be told. None if the list could not be read.
#[cfg(target_os = "macos")]
fn all_processes() -> Option<Vec<Process>> {
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
                out.push(Process {
                    pid: pid as u32,
                    path: String::from_utf8_lossy(&buf[..n as usize]).into_owned(),
                    owner: owner(pid as u32),
                    command: Vec::new(),
                });
            }
        }
        Some(out)
    }
}

/// The user a process runs as. None if that cannot be told.
#[cfg(target_os = "macos")]
fn owner(pid: u32) -> Option<u32> {
    bsd_info(pid).map(|info| info.pbi_uid)
}

#[cfg(target_os = "macos")]
fn bsd_info(pid: u32) -> Option<libc::proc_bsdinfo> {
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
        (got == size).then_some(info)
    }
}

#[cfg(not(target_os = "macos"))]
fn all_processes() -> Option<Vec<Process>> {
    use std::os::unix::fs::MetadataExt;
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let dir = entry.path();
        // (the program of another user's process is not to be read, nor that of one that has
        // just ended; a program replaced on disk while it runs is listed with this ending)
        let path = std::fs::read_link(dir.join("exe"))
            .map(|exe| {
                let text = exe.to_string_lossy().into_owned();
                text.strip_suffix(" (deleted)").unwrap_or(&text).to_string()
            })
            .unwrap_or_default();
        let command = std::fs::read(dir.join("cmdline"))
            .map(|raw| {
                raw.split(|b| *b == 0)
                    .filter(|word| !word.is_empty())
                    .map(|word| String::from_utf8_lossy(word).into_owned())
                    .collect()
            })
            .unwrap_or_default();
        out.push(Process {
            pid,
            path,
            owner: std::fs::metadata(&dir).ok().map(|md| md.uid()),
            command,
        });
    }
    Some(out)
}

/// `(pid, full path)` of the running processes whose program's file name is in `names`
/// (lower case). None if the list could not be read.
pub fn processes(names: &[&str]) -> Option<Vec<(u32, Option<String>)>> {
    Some(
        all_processes()?
            .into_iter()
            .filter(|p| !p.path.is_empty() && names.contains(&program_name(&p.path).as_str()))
            .map(|p| (p.pid, Some(p.path)))
            .collect(),
    )
}

/// Is this the desktop app's own program? Any application bundle's main program called
/// `Claude` is (the app may have been renamed or installed twice: `Claude 2.app`, `Claude
/// Beta.app`). Its helpers are not (the browser's native-messaging host stays behind when
/// the app is closed), nor is the command-line tool, which is no bundle's main program.
#[cfg(target_os = "macos")]
pub fn is_desktop_app(path: &str) -> bool {
    path.ends_with(".app/Contents/MacOS/Claude")
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

/// Does a command line name the Claude app's bundle: an `app.asar` with "claude" somewhere
/// in its path? That is how the app looks when a general Electron program runs it
/// (`electron /usr/lib/claude-desktop/app.asar`), and how its helper processes look.
pub fn command_names_the_app(command: &[String]) -> bool {
    command.iter().any(|word| {
        let word = word.to_lowercase();
        word.contains("app.asar") && word.contains("claude")
    })
}

/// Does a process count for the question whether this user's app is running? One of
/// another user does not; one whose owner cannot be told does.
pub fn counts_as_ours(owner: Option<u32>, me: u32) -> bool {
    owner.is_none_or(|uid| uid == me)
}

/// Is this process the desktop app (of this user)?
pub fn is_the_app(process: &Process, me: u32) -> bool {
    counts_as_ours(process.owner, me)
        && (is_desktop_app(&process.path) || command_names_the_app(&process.command))
}

/// True if the Claude desktop app has a process. When that cannot be told, true (the
/// cautious answer).
pub fn desktop_app_running() -> bool {
    let me = this_user();
    match all_processes() {
        Some(found) => found.iter().any(|p| is_the_app(p, me)),
        None => true,
    }
}

/// The process number at the end of a `SingletonLock` link's target (`<host>-<pid>`).
pub fn lock_holder(target: &str) -> Option<u32> {
    let (_, pid) = target.rsplit_once('-')?;
    pid.parse().ok().filter(|pid| *pid > 0)
}

/// Is the lock Chromium keeps in the app's data folder while it runs held by a process that
/// is there? (The lock is a link, not a file; a link left by an app that was killed names a
/// process that is gone, or by now another one: that is taken for the app, the cautious way
/// round.)
pub fn singleton_lock_live(app_dir: &Path) -> bool {
    let Ok(target) = std::fs::read_link(app_dir.join("SingletonLock")) else {
        return false;
    };
    lock_holder(&target.to_string_lossy()).is_some_and(exists)
}

/// Is there a process of this number (whoever it belongs to)?
fn exists(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 sends nothing; it only asks whether the process is there. (A process
    // that may not be signalled is there all the same.)
    let asked = unsafe { libc::kill(pid, 0) };
    asked == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// When a process was started, in seconds since 1970. None if that cannot be told.
#[cfg(target_os = "macos")]
pub fn start_time(pid: u32) -> Option<f64> {
    bsd_info(pid).map(|info| info.pbi_start_tvsec as f64 + info.pbi_start_tvusec as f64 / 1e6)
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

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "css-proc-unit-{}-{}-{}",
            name,
            std::process::id(),
            (crate::winfs::real_now() * 1e6) as u64
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn this_process_is_on_the_list_under_its_programs_name_and_as_this_users() {
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
        let all = all_processes().unwrap();
        let this = all.iter().find(|p| p.pid == me).unwrap();
        assert_eq!(this.owner, Some(this_user()));
        #[cfg(not(target_os = "macos"))]
        assert!(!this.command.is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn any_bundles_main_program_called_claude_counts_as_the_app() {
        assert!(is_desktop_app(
            "/Applications/Claude.app/Contents/MacOS/Claude"
        ));
        assert!(is_desktop_app(
            "/Users/someone/Applications/Claude.app/Contents/MacOS/Claude"
        ));
        // installed twice, or renamed: when in doubt, it is the app
        assert!(is_desktop_app(
            "/Applications/Claude 2.app/Contents/MacOS/Claude"
        ));
        assert!(is_desktop_app(
            "/Applications/Claude Beta.app/Contents/MacOS/Claude"
        ));
        assert!(is_desktop_app(
            "/Volumes/Somewhere/Anything.app/Contents/MacOS/Claude"
        ));
        // the browser's helper stays behind when the app is closed
        assert!(!is_desktop_app(
            "/Applications/Claude.app/Contents/Helpers/chrome-native-host"
        ));
        assert!(!is_desktop_app(
            "/Applications/Claude.app/Contents/Frameworks/Claude Helper.app/Contents/MacOS/Claude Helper"
        ));
        assert!(!is_desktop_app(
            "/Applications/Claude.app/Contents/Frameworks/Claude Helper (Renderer).app/Contents/MacOS/Claude Helper (Renderer)"
        ));
        assert!(!is_desktop_app(
            "/Applications/Claude.app/Contents/MacOS/Claude Helper"
        ));
        // the command-line tool, and other apps
        assert!(!is_desktop_app("/Users/someone/.local/bin/claude"));
        assert!(!is_desktop_app("/opt/homebrew/bin/claude"));
        assert!(!is_desktop_app("/usr/local/Contents/MacOS/Claude"));
        assert!(!is_desktop_app(
            "/Applications/NotClaude.app/Contents/MacOS/NotClaude"
        ));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn the_app_is_told_from_the_command_line_tool_by_what_is_beside_it() {
        let dir = scratch("layout");
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

    #[test]
    fn a_command_line_that_names_the_apps_bundle_is_the_app() {
        let words = |text: &[&str]| -> Vec<String> { text.iter().map(|w| w.to_string()).collect() };
        assert!(command_names_the_app(&words(&[
            "/usr/bin/electron",
            "/usr/lib/claude-desktop/app.asar"
        ])));
        assert!(command_names_the_app(&words(&[
            "electron33",
            "--no-sandbox",
            "/opt/Claude/resources/app.asar",
            "--some-flag"
        ])));
        // a helper of the app, as Electron starts it
        assert!(command_names_the_app(&words(&[
            "/proc/self/exe",
            "--type=renderer",
            "--app-path=/opt/CLAUDE/resources/app.asar"
        ])));
        // other Electron apps, the command-line tool, nothing at all
        assert!(!command_names_the_app(&words(&[
            "/usr/bin/electron",
            "/usr/lib/some-editor/app.asar"
        ])));
        assert!(!command_names_the_app(&words(&["claude", "--resume"])));
        assert!(!command_names_the_app(&words(&[
            "node",
            "/home/someone/.claude/local/cli.js"
        ])));
        assert!(!command_names_the_app(&[]));
    }

    #[test]
    fn only_this_users_processes_count_and_those_nobody_can_place() {
        assert!(counts_as_ours(Some(501), 501));
        assert!(!counts_as_ours(Some(502), 501)); // another user's app has its own store
        assert!(!counts_as_ours(Some(0), 501));
        assert!(counts_as_ours(None, 501)); // no telling whose: it counts
        let app = |owner| Process {
            pid: 4242,
            path: String::from("/somewhere/else"),
            owner,
            command: vec![
                String::from("electron"),
                String::from("/usr/lib/claude-desktop/app.asar"),
            ],
        };
        assert!(is_the_app(&app(Some(501)), 501));
        assert!(is_the_app(&app(None), 501));
        assert!(!is_the_app(&app(Some(502)), 501));
        let other = Process {
            command: vec![String::from("vim")],
            ..app(Some(501))
        };
        assert!(!is_the_app(&other, 501));
    }

    #[test]
    fn the_apps_own_lock_says_it_is_running_while_its_process_is_there() {
        assert_eq!(lock_holder("some-host-4242"), Some(4242));
        assert_eq!(lock_holder("host.with-dashes.example-17"), Some(17));
        assert_eq!(lock_holder("no number"), None);
        assert_eq!(lock_holder("host-"), None);
        assert_eq!(lock_holder("host-0"), None);
        assert_eq!(lock_holder("host--5"), Some(5));
        let dir = scratch("lock");
        let lock = dir.join("SingletonLock");
        assert!(!singleton_lock_live(&dir)); // no lock: no app
                                             // held by a process that is there (this one)
        std::os::unix::fs::symlink(format!("some-host-{}", std::process::id()), &lock).unwrap();
        assert!(singleton_lock_live(&dir));
        // left behind by a process that is gone
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let gone = child.id();
        child.wait().unwrap();
        std::fs::remove_file(&lock).unwrap();
        std::os::unix::fs::symlink(format!("some-host-{}", gone), &lock).unwrap();
        assert!(!singleton_lock_live(&dir));
        // an ordinary file of that name is not the lock
        std::fs::remove_file(&lock).unwrap();
        std::fs::write(&lock, format!("some-host-{}", std::process::id())).unwrap();
        assert!(!singleton_lock_live(&dir));
        // a process that is there but not this user's to signal is there all the same
        assert!(exists(1));
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
