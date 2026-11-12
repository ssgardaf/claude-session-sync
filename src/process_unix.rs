//! The process list on macOS (libproc) and Linux (`/proc`).
//!
//! Whether the desktop app is running decides whether the records in the folder it has
//! loaded may be replaced, so the question is answered cautiously: where there is doubt, the
//! app is running. It is taken to be running if any of these holds:
//!
//! * a process of this user runs the app's own program (macOS: a program called `Claude` in
//!   the `Contents/MacOS` folder of any application bundle; Linux: a program called
//!   `claude-desktop` or `claude` with the app's bundle beside it, or any program, such as
//!   `electron`, that is given the app's bundle to run: a path to an `app.asar` with
//!   "claude" in it);
//! * the lock Chromium keeps in the app's data folder while it runs (`SingletonLock`, a link
//!   whose target is this computer's name and the process's number) names a process of
//!   this user that is there;
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

/// What the system tells anyone about any process (`PROC_PIDT_SHORTBSDINFO`; the fuller
/// `PROC_PIDTBSDINFO` is refused for other users' processes, which are the very ones whose
/// owner is asked for).
#[cfg(target_os = "macos")]
#[repr(C)]
struct ShortInfo {
    pid: u32,
    parent: u32,
    group: u32,
    status: u32,
    name: [u8; 16],
    flags: u32,
    uid: u32,
    gid: u32,
    real_uid: u32,
    real_gid: u32,
    saved_uid: u32,
    saved_gid: u32,
    reserved: u32,
}

/// The user a process runs as. None if that cannot be told.
#[cfg(target_os = "macos")]
pub fn owner(pid: u32) -> Option<u32> {
    const PROC_PIDT_SHORTBSDINFO: libc::c_int = 13;
    // SAFETY: ShortInfo is plain data laid out as the system's own structure, and the call
    // is told its size.
    unsafe {
        let mut info: ShortInfo = std::mem::zeroed();
        let size = std::mem::size_of::<ShortInfo>() as libc::c_int;
        let got = libc::proc_pidinfo(
            pid as libc::c_int,
            PROC_PIDT_SHORTBSDINFO,
            0,
            (&mut info as *mut ShortInfo).cast(),
            size,
        );
        (got == size && info.pid == pid).then_some(info.uid)
    }
}

/// The user a process runs as. None if that cannot be told.
#[cfg(not(target_os = "macos"))]
pub fn owner(pid: u32) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(format!("/proc/{}", pid))
        .ok()
        .map(|md| md.uid())
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
            owner: owner(pid),
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

/// Does a command line name the Claude app's bundle? That is how the app looks when a
/// general Electron program runs it (`electron /usr/lib/claude-desktop/app.asar`), and how
/// its helper processes look (`--app-path=/opt/Claude/resources/app.asar`). One of the
/// words (or what follows the `=` of an option) has to be a path to a file called
/// `app.asar`, or to the unpacked form of it, a folder `resources/app`, with "claude"
/// somewhere in that path. A file that merely has such a name inside its own
/// (`claude/app.asar.txt`, `notes-on-claude-app.asar.md`) does not make its reader the app.
pub fn command_names_the_app(command: &[String]) -> bool {
    command.iter().any(|word| {
        let named = match word.split_once('=') {
            Some((option, value)) if option.starts_with('-') => value,
            _ => word.as_str(),
        };
        let named = named.to_lowercase();
        let path = Path::new(&named);
        let last = path.file_name().and_then(|n| n.to_str());
        let above = path
            .parent()
            .and_then(Path::file_name)
            .and_then(|n| n.to_str());
        let bundle =
            last == Some("app.asar") || (last == Some("app") && above == Some("resources"));
        bundle && named.contains("claude")
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

/// The computer and the process a `SingletonLock` link's target names (`<host>-<pid>`).
pub fn lock_holder(target: &str) -> Option<(&str, u32)> {
    let (host, pid) = target.rsplit_once('-')?;
    let pid: u32 = pid.parse().ok().filter(|pid| *pid > 0)?;
    Some((host, pid))
}

/// The name this computer goes by (what the app's engine writes into its lock).
pub fn this_host() -> Option<String> {
    let mut name = vec![0u8; 256];
    // SAFETY: the buffer is valid for the size passed.
    if unsafe { libc::gethostname(name.as_mut_ptr().cast(), name.len()) } != 0 {
        return None;
    }
    let end = name.iter().position(|b| *b == 0).unwrap_or(name.len());
    String::from_utf8(name[..end].to_vec())
        .ok()
        .filter(|n| !n.is_empty())
}

/// Does a lock that names this target say that this user's app is running here? Only if it
/// was taken on this computer (a home folder may be shared between several) by a process
/// that is still there and is this user's. A lock left behind by an app that was killed
/// names a process that is gone, or one that has since been given its number: system
/// processes and other users' do not count for it. Where this computer's name cannot be
/// told, or the process's owner, the lock counts (the cautious way round).
///
/// `look` says of a process number whether there is such a process (None: there is not)
/// and whose it is (Some(None): there is, and no telling whose).
pub fn lock_counts(
    target: &str,
    host: Option<&str>,
    me: u32,
    look: &dyn Fn(u32) -> Option<Option<u32>>,
) -> bool {
    let Some((their_host, pid)) = lock_holder(target) else {
        return false;
    };
    if host.is_some_and(|here| here != their_host) {
        return false;
    }
    look(pid).is_some_and(|whose| counts_as_ours(whose, me))
}

/// Is the lock Chromium keeps in the app's data folder while it runs (a link, not a file)
/// held by this user's app on this computer? See `lock_counts`.
pub fn singleton_lock_live(app_dir: &Path) -> bool {
    let Ok(target) = std::fs::read_link(app_dir.join("SingletonLock")) else {
        return false;
    };
    lock_counts(
        &target.to_string_lossy(),
        this_host().as_deref(),
        this_user(),
        &|pid| exists(pid).then(|| owner(pid)),
    )
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
        assert_eq!(owner(me), Some(this_user()));
        // Another user's process is told to be another user's (it is not "no telling whose",
        // which would count): the first process of the system belongs to the system. (On
        // macOS that is so everywhere; inside a container on Linux the first process may be
        // the user's own.)
        assert!(owner(1).is_some());
        #[cfg(target_os = "macos")]
        {
            assert_eq!(owner(1), Some(0));
            if this_user() != 0 {
                assert!(!counts_as_ours(owner(1), this_user()));
                // ...and so is every process on the list that is not this user's
                assert!(all
                    .iter()
                    .any(|p| p.owner.is_some_and(|uid| uid != this_user())));
                assert!(all.iter().all(|p| p.owner.is_some()));
            }
        }
        assert_eq!(owner(u32::MAX - 7), None); // no such process: nothing to tell
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
        // the unpacked form of the bundle
        assert!(command_names_the_app(&words(&[
            "electron",
            "/usr/lib/claude-desktop/resources/app"
        ])));
        assert!(command_names_the_app(&words(&[
            "electron",
            "/usr/lib/claude-desktop/resources/app/"
        ])));
        // a file that only has such a name inside its own is not the bundle
        for other in [
            "/home/someone/claude/app.asar.txt",
            "/home/someone/notes-on-claude-app.asar.md",
            "/home/someone/claude/app.asar.d/readme",
            "/home/someone/claude/myapp.asar",
            "/home/someone/claude/resources/application",
            "/home/someone/claude/app",
            "--title=claude app.asar",
        ] {
            assert!(!command_names_the_app(&words(&["vim", other])), "{}", other);
        }
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
    fn the_apps_own_lock_counts_only_for_this_users_app_on_this_computer() {
        assert_eq!(lock_holder("some-host-4242"), Some(("some-host", 4242)));
        assert_eq!(
            lock_holder("host.with-dashes.example-17"),
            Some(("host.with-dashes.example", 17))
        );
        assert_eq!(lock_holder("no number"), None);
        assert_eq!(lock_holder("host-"), None);
        assert_eq!(lock_holder("host-0"), None);
        assert_eq!(lock_holder("host--5"), Some(("host-", 5)));
        // who holds it: this user's process 4242, the system's process 1, another user's 77,
        // one nobody can place (88); there is no process 99
        let look = |pid: u32| -> Option<Option<u32>> {
            match pid {
                4242 => Some(Some(501)),
                1 => Some(Some(0)),
                77 => Some(Some(502)),
                88 => Some(None),
                _ => None,
            }
        };
        let here = Some("this-mac.local");
        assert!(lock_counts("this-mac.local-4242", here, 501, &look));
        // a lock left behind, whose number now belongs to the system or to someone else
        assert!(!lock_counts("this-mac.local-1", here, 501, &look));
        assert!(!lock_counts("this-mac.local-77", here, 501, &look));
        // ...or to nobody
        assert!(!lock_counts("this-mac.local-99", here, 501, &look));
        // taken on another computer (a shared home folder): whatever runs under that number here
        assert!(!lock_counts("another-host-4242", here, 501, &look));
        assert!(!lock_counts("this-mac-4242", here, 501, &look));
        // where there is no telling, it counts
        assert!(lock_counts("this-mac.local-88", here, 501, &look)); // whose process
        assert!(lock_counts("another-host-4242", None, 501, &look)); // what this computer is called
        assert!(!lock_counts("garbage", here, 501, &look));

        // the same through the real thing
        let host = this_host().unwrap();
        let dir = scratch("lock");
        let lock = dir.join("SingletonLock");
        let set = |target: String| {
            let _ = std::fs::remove_file(&lock);
            std::os::unix::fs::symlink(target, &lock).unwrap();
        };
        assert!(!singleton_lock_live(&dir)); // no lock: no app
        set(format!("{}-{}", host, std::process::id()));
        assert!(singleton_lock_live(&dir)); // held by a process of this user that is there
        if owner(1).is_some_and(|uid| uid != this_user()) {
            set(format!("{}-1", host));
            assert!(!singleton_lock_live(&dir)); // the system's first process is not the app
        }
        set(format!("not-{}-{}", host, std::process::id()));
        assert!(!singleton_lock_live(&dir)); // another computer's lock
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let gone = child.id();
        child.wait().unwrap();
        set(format!("{}-{}", host, gone));
        assert!(!singleton_lock_live(&dir)); // left behind by a process that is gone
                                             // an ordinary file of that name is not the lock
        std::fs::remove_file(&lock).unwrap();
        std::fs::write(&lock, format!("{}-{}", host, std::process::id())).unwrap();
        assert!(!singleton_lock_live(&dir));
        assert!(exists(1)); // (a process that may not be signalled is there all the same)
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
