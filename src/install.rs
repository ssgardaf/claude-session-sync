//! Autostart and upkeep: a per-user scheduled task that starts the daemon at logon and starts
//! it again every five minutes (a second copy leaves at once when the first is at work, and
//! replaces it when it has been silent for 15 minutes); the programs' folder on the user's
//! PATH; pausing, resuming and removing all of that.
//!
//! Everything that reaches outside this tool's own folders (Task Scheduler, the registry,
//! other processes) goes through the `System` trait, so that tests can record what would be
//! done instead of doing it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::consts::{OUR_PROCESS_NAMES, VERSION};
use crate::daemon::acquire_lock;
use crate::paths::{default_home, Env, Paths};
use crate::process;
use crate::sandbox;
use crate::winfs::{self, Expect};
use crate::winreg::{self, Text};

pub const TASK_NAME: &str = "Claude Session Sync";
pub const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
pub const CONSOLE_EXE: &str = "claude-session-sync.exe";
pub const WINDOWLESS_EXE: &str = "claude-session-syncw.exe";
/// The one-off task through which the PATH is changed from inside the app's sandbox.
pub const SETUP_TASK_NAME: &str = "Claude Session Sync (setup)";
/// Where that task leaves its answer: in the folder that holds `bin`.
pub const PATH_RESULT: &str = "path-result.txt";

/// This tool's own files in its state folder: what `uninstall --purge` deletes there.
const OWN_FILES: [&str; 13] = [
    "settings.json",
    "setup-task.xml",
    "sync.log",
    "sync.log.1",
    "journal.jsonl",
    "journal.jsonl.1",
    "manifest.json",
    "manifest.log",
    "state.json",
    "status.json",
    "stop.flag",
    "daemon.lock",
    "task.xml",
];
/// ...and its own folders there.
const OWN_DIRS: [&str; 4] = ["backups", "quarantine", "replaced", "undone"];

/// How the installed daemon is to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub interval: i64,
    pub quick: i64,
    pub delete_sync: bool,
    pub days: i64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            interval: 3600,
            quick: 10,
            delete_sync: false,
            days: 30,
        }
    }
}

/// What `install` was told on the command line. What is not given is kept from the previous
/// install, or is the default on a first one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Given {
    pub interval: Option<i64>,
    pub quick: Option<i64>,
    pub days: Option<i64>,
    pub delete_sync: Option<bool>,
}

/// The file in the state folder that keeps the settings the daemon was installed with.
pub const SETTINGS_FILE: &str = "settings.json";

/// The settings the daemon was last installed with, if that is on record.
pub fn load_settings(home: &Path) -> Option<Settings> {
    let saved: Value = serde_json::from_slice(&fs::read(home.join(SETTINGS_FILE)).ok()?).ok()?;
    let saved = saved.as_object()?;
    let default = Settings::default();
    let number = |key: &str, or: i64| saved.get(key).and_then(Value::as_i64).unwrap_or(or);
    Some(Settings {
        interval: number("interval", default.interval),
        quick: number("quick", default.quick),
        days: number("days", default.days),
        delete_sync: saved
            .get("delete_sync")
            .and_then(Value::as_bool)
            .unwrap_or(default.delete_sync),
    })
}

pub fn save_settings(home: &Path, s: &Settings) -> io::Result<()> {
    fs::create_dir_all(home)?;
    let text = serde_json::json!({
        "interval": s.interval,
        "quick": s.quick,
        "days": s.days,
        "delete_sync": s.delete_sync,
    })
    .to_string();
    match winfs::atomic_write(
        &home.join(SETTINGS_FILE),
        text.as_bytes(),
        None,
        true,
        Expect::Any,
    ) {
        Ok(()) => Ok(()),
        Err(winfs::WriteError::Io(e)) => Err(e),
        Err(winfs::WriteError::Changed) => Err(io::Error::other("settings.json changed")),
    }
}

/// The settings in a daemon's command line (this version's `daemon ...`, or the earlier
/// Python version's `... --watch ...`). None if it is not such a command line.
pub fn settings_from_arguments(arguments: &str) -> Option<Settings> {
    let tokens: Vec<&str> = arguments.split_whitespace().collect();
    if !tokens.iter().any(|t| *t == "daemon" || *t == "--watch") {
        return None;
    }
    let mut s = Settings::default();
    let mut i = 0;
    while i < tokens.len() {
        let (name, inline) = match tokens[i].split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (tokens[i], None),
        };
        if matches!(name, "--interval" | "--quick" | "--days") {
            let value = match inline {
                Some(v) => Some(v),
                None => {
                    i += 1;
                    tokens.get(i).copied()
                }
            };
            if let Some(n) = value.and_then(|v| v.parse::<i64>().ok()) {
                match name {
                    "--interval" => s.interval = n,
                    "--quick" => s.quick = n,
                    _ => s.days = n,
                }
            }
        } else if name == "--delete-sync" {
            s.delete_sync = true;
        } else if name == "--no-delete-sync" {
            s.delete_sync = false;
        }
        i += 1;
    }
    Some(s)
}

/// The settings of the daemon a registered task starts, read from the task's definition (as
/// `schtasks /Query /XML` prints it). This is how an install made before the settings were
/// kept in a file hands them on.
pub fn settings_from_task(xml: &str) -> Option<Settings> {
    let start = xml.find("<Arguments>")? + "<Arguments>".len();
    let end = start + xml[start..].find("</Arguments>")?;
    let arguments = xml[start..end]
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");
    settings_from_arguments(&arguments)
}

/// The arguments the scheduled task passes to the windowless program.
pub fn daemon_arguments(s: &Settings) -> String {
    let mut a = format!("daemon --interval {} --quick {}", s.interval, s.quick);
    if s.delete_sync {
        a.push_str(" --delete-sync");
    }
    if s.days != 30 {
        a.push_str(&format!(" --days {}", s.days));
    }
    a
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// The task definition handed to `schtasks /Create /XML`.
///
/// The daemon never ends by itself: no time limit, keep running on battery. A second copy may
/// be started (Parallel): it finds the first one at work and leaves at once, or, if the first
/// has been silent for 15 minutes, ends it and takes over. `start` is the first time the
/// repeating trigger fires (local time, `YYYY-MM-DDTHH:MM:SS`).
pub fn task_xml(
    exe: &Path,
    arguments: &str,
    workdir: &Path,
    user: &str,
    start: &str,
    with_logon: bool,
) -> String {
    let user = xml_escape(user);
    let mut lines: Vec<String> = Vec::new();
    let mut add = |line: &str| lines.push(line.to_string());
    add("<?xml version=\"1.0\" encoding=\"UTF-16\"?>");
    add("<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">");
    add("  <RegistrationInfo>");
    add(&format!(
        "    <Description>Keeps the Claude desktop Code-tab session list the same under every account on this PC. Files: {}</Description>",
        xml_escape(&workdir.to_string_lossy())
    ));
    add("  </RegistrationInfo>");
    add("  <Triggers>");
    if with_logon {
        add("    <LogonTrigger>");
        add("      <Enabled>true</Enabled>");
        add(&format!("      <UserId>{}</UserId>", user));
        add("    </LogonTrigger>");
    }
    add("    <TimeTrigger>");
    add("      <Repetition>");
    add("        <Interval>PT5M</Interval>");
    add("        <StopAtDurationEnd>false</StopAtDurationEnd>");
    add("      </Repetition>");
    add(&format!(
        "      <StartBoundary>{}</StartBoundary>",
        xml_escape(start)
    ));
    add("      <Enabled>true</Enabled>");
    add("    </TimeTrigger>");
    add("  </Triggers>");
    add("  <Principals>");
    add("    <Principal id=\"Author\">");
    add(&format!("      <UserId>{}</UserId>", user));
    add("      <LogonType>InteractiveToken</LogonType>");
    add("      <RunLevel>LeastPrivilege</RunLevel>");
    add("    </Principal>");
    add("  </Principals>");
    add("  <Settings>");
    add("    <MultipleInstancesPolicy>Parallel</MultipleInstancesPolicy>");
    add("    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>");
    add("    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>");
    add("    <AllowHardTerminate>true</AllowHardTerminate>");
    add("    <StartWhenAvailable>true</StartWhenAvailable>");
    add("    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>");
    add("    <IdleSettings>");
    add("      <StopOnIdleEnd>true</StopOnIdleEnd>");
    add("      <RestartOnIdle>false</RestartOnIdle>");
    add("    </IdleSettings>");
    add("    <AllowStartOnDemand>true</AllowStartOnDemand>");
    add("    <Enabled>true</Enabled>");
    add("    <Hidden>false</Hidden>");
    add("    <RunOnlyIfIdle>false</RunOnlyIfIdle>");
    add("    <WakeToRun>false</WakeToRun>");
    add("    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>");
    add("    <Priority>7</Priority>");
    add("    <RestartOnFailure>");
    add("      <Interval>PT1M</Interval>");
    add("      <Count>3</Count>");
    add("    </RestartOnFailure>");
    add("  </Settings>");
    add("  <Actions Context=\"Author\">");
    add("    <Exec>");
    add(&format!(
        "      <Command>{}</Command>",
        xml_escape(&exe.to_string_lossy())
    ));
    add(&format!(
        "      <Arguments>{}</Arguments>",
        xml_escape(arguments)
    ));
    add(&format!(
        "      <WorkingDirectory>{}</WorkingDirectory>",
        xml_escape(&workdir.to_string_lossy())
    ));
    add("    </Exec>");
    add("  </Actions>");
    add("</Task>");
    lines.join("\r\n") + "\r\n"
}

/// The definition of a one-off task: the one that runs the hidden `__path` command outside
/// the app's sandbox, or the one that deletes the programs once they have ended. It has no
/// trigger: it is registered, started on demand and deleted again. Unlike a task made with
/// `schtasks /SC ONCE`, it also starts on battery; it is hidden, and may run for a minute at
/// most.
pub fn setup_task_xml(exe: &Path, arguments: &str, workdir: Option<&Path>, user: &str) -> String {
    let lines = [
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>".to_string(),
        "<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">"
            .to_string(),
        "  <RegistrationInfo>".to_string(),
        "    <Description>A one-off step of claude-session-sync's install or uninstall; deleted right after.</Description>"
            .to_string(),
        "  </RegistrationInfo>".to_string(),
        "  <Triggers />".to_string(),
        "  <Principals>".to_string(),
        "    <Principal id=\"Author\">".to_string(),
        format!("      <UserId>{}</UserId>", xml_escape(user)),
        "      <LogonType>InteractiveToken</LogonType>".to_string(),
        "      <RunLevel>LeastPrivilege</RunLevel>".to_string(),
        "    </Principal>".to_string(),
        "  </Principals>".to_string(),
        "  <Settings>".to_string(),
        "    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>".to_string(),
        "    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>".to_string(),
        "    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>".to_string(),
        "    <AllowHardTerminate>true</AllowHardTerminate>".to_string(),
        "    <StartWhenAvailable>false</StartWhenAvailable>".to_string(),
        "    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>".to_string(),
        "    <AllowStartOnDemand>true</AllowStartOnDemand>".to_string(),
        "    <Enabled>true</Enabled>".to_string(),
        "    <Hidden>true</Hidden>".to_string(),
        "    <RunOnlyIfIdle>false</RunOnlyIfIdle>".to_string(),
        "    <WakeToRun>false</WakeToRun>".to_string(),
        "    <ExecutionTimeLimit>PT1M</ExecutionTimeLimit>".to_string(),
        "    <Priority>7</Priority>".to_string(),
        "  </Settings>".to_string(),
        "  <Actions Context=\"Author\">".to_string(),
        "    <Exec>".to_string(),
        format!(
            "      <Command>{}</Command>",
            xml_escape(&exe.to_string_lossy())
        ),
        format!("      <Arguments>{}</Arguments>", xml_escape(arguments)),
        match workdir {
            Some(dir) => format!(
                "      <WorkingDirectory>{}</WorkingDirectory>",
                xml_escape(&dir.to_string_lossy())
            ),
            None => String::new(),
        },
        "    </Exec>".to_string(),
        "  </Actions>".to_string(),
        "</Task>".to_string(),
    ];
    let lines: Vec<String> = lines.into_iter().filter(|l| !l.is_empty()).collect();
    lines.join("\r\n") + "\r\n"
}

/// The bytes of a task definition file: UTF-16, little endian, with a byte-order mark.
pub fn utf16_file(text: &str) -> Vec<u8> {
    let mut out = vec![0xFF, 0xFE];
    for unit in text.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

/// The command line for the `Run` registry entry (the last resort, without a watchdog).
pub fn run_entry_command(exe: &Path, arguments: &str) -> String {
    format!("\"{}\" {}", exe.to_string_lossy(), arguments)
}

/// The account the task runs as: `DOMAIN\user`.
pub fn task_user(env: &Env) -> Option<String> {
    let user = env.username.as_ref()?;
    Some(match &env.userdomain {
        Some(domain) => format!("{}\\{}", domain, user),
        None => user.clone(),
    })
}

/// Does a task definition (as `schtasks /Query /XML` prints it) say the task is disabled?
/// (Triggers have an `Enabled` of their own; the task's is the one under `Settings`.)
pub fn task_is_disabled(xml: &str) -> bool {
    let Some(start) = xml.find("<Settings>") else {
        return false;
    };
    let settings = &xml[start..];
    let settings = settings
        .find("</Settings>")
        .map_or(settings, |end| &settings[..end]);
    settings.contains("<Enabled>false</Enabled>")
}

// ---------------------------------------------------------------------- the user's PATH

fn same_folder(entry: &str, dir: &str) -> bool {
    fn tidy(s: &str) -> String {
        s.trim()
            .trim_matches('"')
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_lowercase()
    }
    !tidy(entry).is_empty() && tidy(entry) == tidy(dir)
}

/// The PATH with a folder added at the end. None if the folder is in it already under any of
/// the given spellings. What is there stays exactly as it is.
pub fn path_with(current: Option<&str>, spellings: &[&str]) -> Option<String> {
    let dir = spellings.first()?;
    let current = current.unwrap_or("");
    if current
        .split(';')
        .any(|entry| spellings.iter().any(|s| same_folder(entry, s)))
    {
        return None;
    }
    Some(if current.is_empty() || current.ends_with(';') {
        format!("{}{}", current, dir)
    } else {
        format!("{};{}", current, dir)
    })
}

/// The PATH without a folder. None if the folder is not in it. Every other entry stays as it
/// is, in its place.
pub fn path_without(current: &str, spellings: &[&str]) -> Option<String> {
    let ours = |entry: &&str| spellings.iter().any(|s| same_folder(entry, s));
    if !current.split(';').any(|entry| ours(&entry)) {
        return None;
    }
    Some(
        current
            .split(';')
            .filter(|entry| !ours(entry))
            .collect::<Vec<_>>()
            .join(";"),
    )
}

// ---------------------------------------------------------------------- the outside world

/// The system's own tools. The real one starts processes and writes the registry; tests
/// record what would be done.
pub trait System {
    /// Run a program and wait: its exit code and what it printed.
    fn run_output(&mut self, program: &str, args: &[String]) -> io::Result<(i32, String)>;
    /// Run a program and wait: its exit code.
    fn run(&mut self, program: &str, args: &[String]) -> io::Result<i32> {
        self.run_output(program, args).map(|(code, _)| code)
    }
    /// Start a program and let it go on by itself.
    fn spawn_detached(&mut self, program: &Path, args: &[String]) -> io::Result<()>;
    fn sleep(&mut self, seconds: f64);
    /// Is `pid` the daemon that reported it started at `started`: one of our programs (or
    /// python, for the earlier version), running since then?
    fn is_ours(&mut self, pid: u32, started: Option<f64>) -> bool;
    fn terminate(&mut self, pid: u32) -> bool;
    /// The user's own PATH as the registry has it (not expanded). None if there is none.
    fn user_path(&mut self) -> io::Result<Option<Text>>;
    fn set_user_path(&mut self, path: &Text) -> io::Result<()>;
    /// Tell running programs that the environment has changed.
    fn environment_changed(&mut self);
    /// Is this process inside the Claude app's sandbox (see the `sandbox` module)? There,
    /// what is written to the user's registry is seen by no process outside the app.
    fn sandboxed(&mut self, localappdata: Option<&Path>) -> bool;
}

pub struct RealSystem;

impl System for RealSystem {
    fn run_output(&mut self, program: &str, args: &[String]) -> io::Result<(i32, String)> {
        use std::process::{Command, Stdio};
        let out = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()?;
        Ok((
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        ))
    }

    fn spawn_detached(&mut self, program: &Path, args: &[String]) -> io::Result<()> {
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
            .spawn()
            .map(|_| ())
    }

    fn sleep(&mut self, seconds: f64) {
        if seconds > 0.0 && seconds.is_finite() {
            std::thread::sleep(std::time::Duration::from_secs_f64(seconds));
        }
    }

    fn is_ours(&mut self, pid: u32, started: Option<f64>) -> bool {
        process::is_daemon(pid, &OUR_PROCESS_NAMES, started)
    }

    fn terminate(&mut self, pid: u32) -> bool {
        process::terminate(pid)
    }

    fn user_path(&mut self) -> io::Result<Option<Text>> {
        winreg::read_text(&winreg::user_environment(false)?, "Path")
    }

    fn set_user_path(&mut self, path: &Text) -> io::Result<()> {
        winreg::write_text(&winreg::user_environment(true)?, "Path", path)
    }

    fn environment_changed(&mut self) {
        winreg::announce_environment_change();
    }

    fn sandboxed(&mut self, localappdata: Option<&Path>) -> bool {
        sandbox::in_app_sandbox(localappdata)
    }
}

fn strs(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| a.to_string()).collect()
}

/// How the daemon is started by itself, if at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Autostart {
    /// The scheduled task is registered; `paused` if it is disabled.
    Task {
        paused: bool,
    },
    /// Only a start-up entry (Task Scheduler refused the task): no watchdog.
    StartupEntry,
    NotInstalled,
    /// There is no scheduled task, and this process is inside the app's sandbox, from where
    /// a start-up entry cannot be seen.
    NoTaskSeen,
    /// Task Scheduler could not be asked.
    Unknown,
}

/// Ask the system how the daemon is started.
pub fn autostart(sys: &mut dyn System, localappdata: Option<&Path>) -> Autostart {
    match sys.run_output("schtasks", &strs(&["/Query", "/TN", TASK_NAME, "/XML"])) {
        Ok((0, xml)) => Autostart::Task {
            paused: task_is_disabled(&xml),
        },
        // a start-up entry is in the user's registry: not to be seen from inside the app
        Ok(_) if sys.sandboxed(localappdata) => Autostart::NoTaskSeen,
        Ok(_) => match sys.run("reg", &strs(&["query", RUN_KEY, "/v", TASK_NAME])) {
            Ok(0) => Autostart::StartupEntry,
            _ => Autostart::NotInstalled,
        },
        Err(_) => Autostart::Unknown,
    }
}

/// The ways the programs' folder may be written in a PATH.
fn spellings(bin: &Path) -> [String; 2] {
    [
        bin.to_string_lossy().into_owned(),
        String::from(r"%USERPROFILE%\.claude-session-sync\bin"),
    ]
}

/// Put the programs' folder on the user's PATH (`add`), or take it off. False if there was
/// nothing to change. The value keeps its kind, and is read back to make sure it is what was
/// meant; if it is not, the old value is put back.
pub fn change_user_path(sys: &mut dyn System, bin: &Path, add: bool) -> Result<bool, String> {
    let spellings = spellings(bin);
    let spellings: Vec<&str> = spellings.iter().map(String::as_str).collect();
    let current = sys.user_path().map_err(|e| e.to_string())?;
    let value = match (&current, add) {
        (_, true) => path_with(current.as_ref().map(|c| c.value.as_str()), &spellings),
        (Some(c), false) => path_without(&c.value, &spellings),
        (None, false) => None,
    };
    let Some(value) = value else {
        return Ok(false);
    };
    if value.encode_utf16().count() > 30_000 {
        return Err(String::from("it would become too long"));
    }
    let new = Text {
        value,
        expand: current.as_ref().is_none_or(|c| c.expand),
    };
    sys.set_user_path(&new).map_err(|e| e.to_string())?;
    if sys.user_path().ok().flatten().as_ref() != Some(&new) {
        if let Some(old) = &current {
            let _ = sys.set_user_path(old);
        }
        return Err(String::from(
            "the change did not read back as written and was put back",
        ));
    }
    sys.environment_changed();
    Ok(true)
}

/// The hidden `__path add|remove` command. `install` and `uninstall` cannot change the PATH
/// themselves when they run inside the Claude app's sandbox, so they have Task Scheduler
/// start the installed program (`exe`) with this command, outside the app. It changes the
/// PATH and leaves one line saying how that went (`added`, `present`, `removed`, `absent` or
/// `error: ...`) in the folder that holds `bin`. The exit code: 0 unless there was an error.
pub fn path_command(sys: &mut dyn System, exe: &Path, add: bool) -> i32 {
    let Some(bin) = exe.parent() else {
        return 1;
    };
    let answer = match change_user_path(sys, bin, add) {
        Ok(true) => String::from(if add { "added" } else { "removed" }),
        Ok(false) => String::from(if add { "present" } else { "absent" }),
        Err(why) => format!("error: {}", why),
    };
    let failed = answer.starts_with("error");
    if let Some(base) = bin.parent() {
        let line = format!("{}\n", answer);
        let written = winfs::atomic_write(
            &base.join(PATH_RESULT),
            line.as_bytes(),
            None,
            true,
            Expect::Any,
        );
        if written.is_err() {
            return 1;
        }
    }
    i32::from(failed)
}

/// Ask a running daemon to exit (the stop flag) and wait until the lock is free.
/// True if no daemon holds the lock any more.
pub fn stop_daemon(home: &Path, tries: usize, sleep: &mut dyn FnMut(f64)) -> io::Result<bool> {
    fs::create_dir_all(home)?;
    let flag = home.join("stop.flag");
    fs::File::create(&flag)?;
    for _ in 0..tries {
        if let Some(lock) = acquire_lock(home)? {
            drop(lock);
            let _ = fs::remove_file(&flag);
            return Ok(true);
        }
        sleep(0.5);
    }
    Ok(false)
}

fn status_file(home: &Path) -> Option<Value> {
    serde_json::from_slice(&fs::read(home.join("status.json")).ok()?).ok()
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => winfs::norm(a) == winfs::norm(b),
    }
}

/// Is a daemon holding the lock?
fn daemon_running(home: &Path) -> bool {
    matches!(acquire_lock(home), Ok(None))
}

pub struct Setup<'a> {
    pub env: &'a Env,
    pub paths: &'a Paths,
    /// The program that is running (its folder holds both programs).
    pub current_exe: PathBuf,
    pub sys: &'a mut dyn System,
    pub say: &'a mut dyn FnMut(&str),
}

impl Setup<'_> {
    /// Where the programs are installed: the `bin` folder of this tool's default folder.
    pub fn bin_dir(&self) -> Option<PathBuf> {
        self.env
            .userprofile
            .as_ref()
            .map(|user| default_home(user).join("bin"))
    }

    fn tell(&mut self, line: &str) {
        (self.say)(line);
    }

    fn stop(&mut self) -> io::Result<bool> {
        let home = self.paths.home.clone();
        let sys = &mut *self.sys;
        stop_daemon(&home, 40, &mut |s| sys.sleep(s))
    }

    fn task_exists(&mut self) -> bool {
        self.sys
            .run("schtasks", &strs(&["/Query", "/TN", TASK_NAME]))
            .is_ok_and(|code| code == 0)
    }

    /// Wait for a daemon that started after `began` to hold the lock and report itself (and,
    /// if asked, to be of this version). Its pid, as text.
    fn wait_for_daemon(&mut self, began: f64, this_version: bool) -> Option<String> {
        for _ in 0..30 {
            self.sys.sleep(0.75);
            if !daemon_running(&self.paths.home) {
                continue;
            }
            let Some(st) = status_file(&self.paths.home) else {
                continue;
            };
            let version = st.get("version").and_then(Value::as_str);
            let started = st.get("started").and_then(Value::as_f64).unwrap_or(0.0);
            if started >= began - 5.0 && (!this_version || version == Some(VERSION)) {
                return Some(
                    st.get("pid")
                        .map(|p| p.to_string())
                        .unwrap_or_else(|| String::from("?")),
                );
            }
        }
        None
    }

    /// Put the programs' folder on the user's PATH (`add`), or take it off. False if there
    /// was nothing to change.
    fn set_path(&mut self, bin: &Path, add: bool, sandboxed: bool) -> Result<bool, String> {
        if sandboxed {
            self.path_through_task(bin, add)
        } else {
            change_user_path(self.sys, bin, add)
        }
    }

    /// Inside the app's sandbox the user's registry is a private copy, so the PATH is changed
    /// by the installed program itself, started outside the app by a one-off scheduled task;
    /// its answer is waited for (15 seconds at most), and the task is deleted again.
    fn path_through_task(&mut self, bin: &Path, add: bool) -> Result<bool, String> {
        let base = bin.parent().ok_or("cannot tell where the programs are")?;
        let result = base.join(PATH_RESULT);
        let program = bin.join(WINDOWLESS_EXE);
        if !program.is_file() {
            return Err(format!("{} is not there to do it", program.display()));
        }
        let _ = fs::remove_file(&result);
        let user = task_user(self.env).ok_or("USERNAME is not set")?;
        let arguments = if add { "__path add" } else { "__path remove" };
        let xml = setup_task_xml(&program, arguments, Some(bin), &user);
        let xml_path = base.join("setup-task.xml");
        fs::write(&xml_path, utf16_file(&xml))
            .map_err(|e| format!("cannot write {}: {}", xml_path.display(), e))?;
        let created = self.sys.run(
            "schtasks",
            &[
                "/Create".to_string(),
                "/TN".to_string(),
                SETUP_TASK_NAME.to_string(),
                "/XML".to_string(),
                xml_path.to_string_lossy().into_owned(),
                "/F".to_string(),
            ],
        );
        let _ = fs::remove_file(&xml_path);
        if !created.is_ok_and(|c| c == 0) {
            return Err(String::from(
                "the one-off task for it could not be registered",
            ));
        }
        let started = self
            .sys
            .run("schtasks", &strs(&["/Run", "/TN", SETUP_TASK_NAME]))
            .is_ok_and(|c| c == 0);
        let mut answer: Option<String> = None;
        if started {
            for _ in 0..30 {
                self.sys.sleep(0.5);
                // (the line is complete once it ends in a line feed)
                if let Some(text) = fs::read_to_string(&result)
                    .ok()
                    .filter(|t| t.ends_with('\n'))
                {
                    answer = Some(text.trim().to_string());
                    break;
                }
            }
        }
        let _ = self.sys.run(
            "schtasks",
            &strs(&["/Delete", "/TN", SETUP_TASK_NAME, "/F"]),
        );
        let _ = fs::remove_file(&result);
        match answer.as_deref() {
            Some("added") | Some("removed") => Ok(true),
            Some("present") | Some("absent") => Ok(false),
            Some(other) => Err(other.strip_prefix("error: ").unwrap_or(other).to_string()),
            None if !started => Err(String::from("the one-off task for it could not be started")),
            None => Err(String::from(
                "the one-off task for it did not answer within 15 seconds",
            )),
        }
    }

    /// How to call the program in what is printed: by name once it is on the PATH.
    fn program(&self, on_path: bool) -> String {
        match self.bin_dir() {
            Some(bin) if !on_path => format!("\"{}\"", bin.join(CONSOLE_EXE).display()),
            _ => String::from("claude-session-sync"),
        }
    }

    /// Register the daemon so it starts by itself and is restarted if it ever stops, and put
    /// the programs on the PATH. Safe to run again after an update or to change the settings:
    /// it stops the running daemon first.
    pub fn install(&mut self, given: &Given) -> Result<(), String> {
        let began = winfs::real_now();
        let bin = self.bin_dir().ok_or("USERPROFILE is not set")?;
        let sandboxed = self.sys.sandboxed(self.env.localappdata.as_deref());
        let user = task_user(self.env).ok_or("USERNAME is not set")?;
        let from = self
            .current_exe
            .parent()
            .map(Path::to_path_buf)
            .ok_or("cannot tell where this program is")?;
        for name in [CONSOLE_EXE, WINDOWLESS_EXE] {
            if !from.join(name).is_file() {
                return Err(format!(
                    "{} was not found beside this program ({}). Keep the two programs together and run install again.",
                    name,
                    from.display()
                ));
            }
        }

        // The settings: what is given now, over what the previous install was given. That is on
        // record in the state folder; an install from before it was kept there is asked through
        // its scheduled task, while that is still the old one.
        let home = self.paths.home.clone();
        let previous = load_settings(&home).or_else(|| {
            let asked = self
                .sys
                .run_output("schtasks", &strs(&["/Query", "/TN", TASK_NAME, "/XML"]));
            match asked {
                Ok((0, xml)) => settings_from_task(&xml),
                _ => None,
            }
        });
        let base = previous.clone().unwrap_or_default();
        let settings = &Settings {
            interval: given.interval.unwrap_or(base.interval).max(60),
            quick: given.quick.unwrap_or(base.quick).max(0),
            days: given.days.unwrap_or(base.days),
            delete_sync: given.delete_sync.unwrap_or(base.delete_sync),
        };

        // A daemon that is already running keeps the old program and settings. Ask it to finish
        // what it is doing and exit; only if it does not, end the task by force.
        let stopped = self.stop().map_err(|e| e.to_string())?;
        if !stopped && self.task_exists() {
            let _ = self.sys.run("schtasks", &strs(&["/End", "/TN", TASK_NAME]));
            self.sys.sleep(2.0);
        }

        if !same_dir(&from, &bin) {
            fs::create_dir_all(&bin)
                .map_err(|e| format!("cannot create {}: {}", bin.display(), e))?;
            for name in [CONSOLE_EXE, WINDOWLESS_EXE] {
                fs::copy(from.join(name), bin.join(name)).map_err(|e| {
                    format!(
                        "cannot copy {} to {}: {} (is an old daemon still running?)",
                        name,
                        bin.display(),
                        e
                    )
                })?;
            }
        }

        let exe = bin.join(WINDOWLESS_EXE);
        let arguments = daemon_arguments(settings);
        let start = winfs::local_format(winfs::real_now() + 60.0, "%Y-%m-%dT%H:%M:%S");
        let xml_path = self.paths.home.join("task.xml");
        let mut mode = None;
        let mut code = -1;
        // A logon trigger can need administrator rights; the repeating trigger does not.
        for (with_logon, what) in [
            (true, format!("scheduled task \"{}\": at logon, and looked after every 5 minutes", TASK_NAME)),
            (
                false,
                format!(
                    "scheduled task \"{}\": every 5 minutes, so within 5 minutes of logon (a logon trigger was refused)",
                    TASK_NAME
                ),
            ),
        ] {
            let xml = task_xml(&exe, &arguments, &bin, &user, &start, with_logon);
            fs::write(&xml_path, utf16_file(&xml)).map_err(|e| format!("cannot write {}: {}", xml_path.display(), e))?;
            let args = vec![
                "/Create".to_string(),
                "/TN".to_string(),
                TASK_NAME.to_string(),
                "/XML".to_string(),
                xml_path.to_string_lossy().into_owned(),
                "/F".to_string(),
            ];
            code = self.sys.run("schtasks", &args).unwrap_or(-1);
            if code == 0 {
                mode = Some(what);
                break;
            }
        }
        let _ = fs::remove_file(&xml_path);

        let mut not_started = false;
        let mode = match mode {
            Some(mode) => {
                // a start-up entry left by an earlier fallback is not needed any more
                if !sandboxed {
                    let _ = self
                        .sys
                        .run("reg", &strs(&["delete", RUN_KEY, "/v", TASK_NAME, "/f"]));
                }
                let started = self.sys.run("schtasks", &strs(&["/Run", "/TN", TASK_NAME]));
                not_started = !started.is_ok_and(|c| c == 0);
                mode
            }
            // A start-up entry written from inside the app's sandbox lands in the app's private
            // registry, where Windows does not look at logon: no use pretending.
            None if sandboxed => {
                return Err(format!(
                    "Task Scheduler refused the task (schtasks exit code {}). Nothing was installed.\n  \
                     This terminal runs inside the Claude app, where the fallback (a start-up entry) would have \
                     no effect.\n  Run install again from an ordinary terminal (PowerShell or Command Prompt \
                     opened from the Start menu).",
                    code
                ));
            }
            None => {
                // Last resort when Task Scheduler refuses: a start-up entry (no watchdog).
                let command = run_entry_command(&exe, &arguments);
                let added = self.sys.run(
                    "reg",
                    &strs(&[
                        "add", RUN_KEY, "/v", TASK_NAME, "/t", "REG_SZ", "/d", &command, "/f",
                    ]),
                );
                if !added.is_ok_and(|c| c == 0) {
                    return Err(format!(
                        "Task Scheduler refused the task (schtasks exit code {}), and the start-up entry could not \
                         be written either. Nothing was installed.",
                        code
                    ));
                }
                let args: Vec<String> = arguments.split(' ').map(String::from).collect();
                self.sys
                    .spawn_detached(&exe, &args)
                    .map_err(|e| format!("cannot start {}: {}", exe.display(), e))?;
                format!(
                    "start-up entry only: Task Scheduler refused the task (schtasks exit code {}), so the daemon \
                     starts at logon but is not restarted if it stops",
                    code
                )
            }
        };

        let saved = save_settings(&home, settings);
        let on_path = self.set_path(&bin, true, sandboxed);

        // Do not report success unless THIS version really came up (a crash at start leaves
        // nothing else to see, and an old daemon that would not stop must not pass for the new one).
        let Some(pid) = self.wait_for_daemon(began, true) else {
            return Err(format!(
                "the programs were copied and the autostart was set up, but the new daemon did not come up{}.\n  \
                 Look at:  {}\n  Then:     {} status",
                if not_started { " (Task Scheduler would not start the task)" } else { "" },
                self.paths.home.join("sync.log").display(),
                self.program(false)
            ));
        };

        let program = self.program(on_path.is_ok());
        self.tell("Installed. From now on the session list is kept the same under every account.");
        self.tell("");
        self.tell(&format!(
            "  Daemon     version {} is running now (pid {})",
            VERSION, pid
        ));
        self.tell(&format!("  Starts by  {}", mode));
        self.tell(&format!(
            "  Settings   full pass every {} s, change check {}, archived sessions up to {} days old,",
            settings.interval,
            if settings.quick == 0 { String::from("off") } else { format!("every {} s", settings.quick) },
            settings.days
        ));
        self.tell(&format!(
            "             deletions {} carried over to other accounts",
            if settings.delete_sync {
                "are"
            } else {
                "are not"
            }
        ));
        if previous.is_some() {
            self.tell(if *given == Given::default() {
                "             (kept from the previous install)"
            } else {
                "             (as given now; what was not given is kept from the previous install)"
            });
        }
        if let Err(e) = &saved {
            self.tell(&format!(
                "             (they could not be put on record for the next install: {})",
                e
            ));
        }
        self.tell(&format!("  Programs   {}", bin.display()));
        self.tell(&format!(
            "  Data       {}   (what it does: sync.log)",
            self.paths.home.display()
        ));
        match &on_path {
            Ok(true) => self.tell("  PATH       the programs' folder was added to your PATH: open a NEW terminal to use it"),
            Ok(false) => self.tell("  PATH       the programs' folder is on your PATH already"),
            Err(why) => self.tell(&format!(
                "  PATH       could not be changed ({}): call the program by its full path, as below",
                why
            )),
        }
        if sandboxed {
            self.tell("");
            self.tell(if on_path.is_ok() {
                "Running inside the Claude app's sandbox: the PATH was set through a one-off task."
            } else {
                "Running inside the Claude app's sandbox: the PATH has to be set through a one-off task, and that failed."
            });
        }
        if self.env.home_override.is_some() || self.env.store_override.is_some() {
            self.tell("");
            self.tell("Note: CLAUDE_SESSION_SYNC_HOME / CLAUDE_SESSION_SYNC_STORE are set in this window. The daemon");
            self.tell("only sees them if they are set as user environment variables (Windows Settings), too.");
        }
        self.tell("");
        self.tell("What you can do now:");
        self.tell(&format!(
            "  {} status      is it running, is anything waiting, are there conflicts",
            program
        ));
        self.tell(&format!("  {} log         what it did lately", program));
        self.tell(&format!(
            "  {} pause       stop syncing for a while (resume starts it again)",
            program
        ));
        self.tell(&format!(
            "  {} uninstall   remove it (your data is kept)",
            program
        ));
        self.tell("");
        self.tell("A session that is copied into the account you are signed in to shows up in the Claude app");
        self.tell("after you restart the app or sign in again.");
        Ok(())
    }

    /// Delete this tool's own files: its state, and the installed programs. Only files and
    /// folders this tool made are deleted, never a folder as a whole that might hold more.
    /// What could not be deleted is returned.
    fn purge(&mut self) -> Vec<PathBuf> {
        let home = self.paths.home.clone();
        let mut targets: Vec<PathBuf> = OWN_FILES.iter().map(|f| home.join(f)).collect();
        if let Ok(entries) = fs::read_dir(&home) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with(".sync-") && name.ends_with(".tmp") {
                    targets.push(e.path());
                }
            }
        }
        for file in &targets {
            let _ = fs::remove_file(file);
        }
        for dir in OWN_DIRS {
            let dir = home.join(dir);
            let _ = fs::remove_dir_all(&dir);
            targets.push(dir);
        }
        if let Some(bin) = self.bin_dir() {
            for name in [CONSOLE_EXE, WINDOWLESS_EXE] {
                let _ = fs::remove_file(bin.join(name));
                targets.push(bin.join(name));
            }
            // the folders themselves go only if nothing else is in them
            let _ = fs::remove_dir(&bin);
            if let Some(base) = bin.parent() {
                if !same_dir(base, &home) {
                    let _ = fs::remove_dir(base);
                }
            }
        }
        let _ = fs::remove_dir(&home);
        targets.retain(|t| t.exists());
        targets
    }

    /// Remove the autostart and the PATH entry, then stop the daemon. Records already copied,
    /// backups, the quarantine and replaced folders, the journal and the log are left in
    /// place, unless `purge` is given: then this tool's own files are deleted as well.
    pub fn uninstall(&mut self, purge: bool) -> Result<(), String> {
        let bin = self.bin_dir();
        let sandboxed = self.sys.sandboxed(self.env.localappdata.as_deref());
        // Unregister first, so the watchdog trigger cannot bring the daemon back while it is stopping.
        if self.task_exists() {
            let removed = self
                .sys
                .run("schtasks", &strs(&["/Delete", "/TN", TASK_NAME, "/F"]));
            if removed.is_ok_and(|c| c == 0) {
                self.tell(&format!("  Scheduled task \"{}\"   removed", TASK_NAME));
            } else {
                self.tell(&format!(
                    "  Scheduled task \"{}\"   could NOT be removed: delete it in Task Scheduler",
                    TASK_NAME
                ));
            }
        } else {
            self.tell(&format!(
                "  Scheduled task \"{}\"   was not registered",
                TASK_NAME
            ));
        }
        // (a start-up entry is in the user's registry: not to be seen from inside the app)
        let has_entry = !sandboxed
            && self
                .sys
                .run("reg", &strs(&["query", RUN_KEY, "/v", TASK_NAME]))
                .is_ok_and(|c| c == 0);
        if has_entry {
            let removed = self
                .sys
                .run("reg", &strs(&["delete", RUN_KEY, "/v", TASK_NAME, "/f"]));
            if removed.is_ok_and(|c| c == 0) {
                self.tell("  Start-up entry                       removed");
            }
        }
        if let Some(bin) = &bin {
            match self.set_path(bin, false, sandboxed) {
                Ok(true) => self.tell("  PATH entry                           removed (other entries are untouched)"),
                Ok(false) => {}
                Err(why) => self.tell(&format!(
                    "  PATH entry                           could NOT be removed ({}): take {} out of your PATH",
                    why,
                    bin.display()
                )),
            }
        }
        let mut stopped = self.stop().map_err(|e| e.to_string())?;
        if stopped {
            self.tell("  Daemon                               not running any more");
        } else {
            // It did not answer the request (it may be stuck, and its watchdog is gone now): end
            // that one process, after making sure the pid on record really is that daemon.
            let st = status_file(&self.paths.home);
            let pid = st
                .as_ref()
                .and_then(|st| st.get("pid").and_then(Value::as_u64))
                .and_then(|p| u32::try_from(p).ok());
            let started = st
                .as_ref()
                .and_then(|st| st.get("started").and_then(Value::as_f64));
            match pid {
                Some(pid)
                    if pid != std::process::id() && self.sys.is_ours(pid, started) && self.sys.terminate(pid) =>
                {
                    self.tell(&format!("  Daemon                               did not stop when asked and was ended (pid {})", pid));
                    stopped = true;
                }
                _ => self.tell(
                    "  Daemon                               did NOT stop when asked: end claude-session-syncw.exe in Task Manager",
                ),
            }
        }
        self.tell("");
        let program = self.program(false);
        if !purge {
            self.tell("Uninstalled. Nothing of yours was deleted. Still in place:");
            self.tell(&format!(
                "  {}   log, journal, backups, quarantine, replaced versions",
                self.paths.home.display()
            ));
            if let Some(bin) = &bin {
                self.tell(&format!("  {}   the programs", bin.display()));
            }
            self.tell("  the session records already copied into each account's folder");
            self.tell("");
            self.tell(&format!(
                "To delete this tool's own files as well:   {} uninstall --purge",
                program
            ));
            self.tell(&format!(
                "To take the copied records back:           {} undo --since \"YYYY-MM-DD HH:MM\"",
                program
            ));
            self.tell("                                           (close the Claude app completely first)");
            self.tell(&format!(
                "To start syncing again:                    {} install",
                program
            ));
            return Ok(());
        }
        if !stopped {
            return Err(String::from(
                "nothing was purged: the daemon is still running. End it, then run uninstall --purge again.",
            ));
        }
        let mut left = self.purge();
        self.tell("Uninstalled and purged: this tool's log, journal, backups, quarantine, replaced versions");
        self.tell("and programs were deleted. The session records already copied into each account's folder");
        self.tell("are the app's own data and were not touched.");
        // A program cannot delete itself while it runs: what is left of the programs is handed
        // to a one-off task, which deletes it a few seconds from now.
        let programs: Vec<PathBuf> = bin
            .iter()
            .flat_map(|b| [b.join(CONSOLE_EXE), b.join(WINDOWLESS_EXE)])
            .collect();
        if left.iter().any(|p| programs.contains(p)) {
            if let Some(bin) = &bin {
                if self.finish_later(bin).is_ok() {
                    left.retain(|p| !programs.contains(p));
                    self.tell("");
                    self.tell("This program cannot delete itself while it runs: a one-off task deletes it, and its");
                    self.tell("folder if nothing else is in it, a few seconds from now.");
                }
            }
        }
        if !left.is_empty() {
            self.tell("");
            self.tell("Could NOT be deleted (in use):");
            for path in &left {
                self.tell(&format!("  {}", path.display()));
            }
            let itself = left.iter().any(|p| same_dir(p, &self.current_exe));
            if itself {
                self.tell("A program cannot delete itself while it runs. To finish, delete that folder yourself:");
                if let Some(bin) = &bin {
                    self.tell(&format!("  rmdir /s /q \"{}\"", bin.display()));
                }
            }
        }
        Ok(())
    }

    /// Have a one-off task delete the installed programs once this one has ended: the two
    /// programs by name, then their folder and the one above it, each only if it is empty by
    /// then; last, the task deletes itself.
    fn finish_later(&mut self, bin: &Path) -> Result<(), String> {
        let user = task_user(self.env).ok_or("USERNAME is not set")?;
        let base = bin.parent().ok_or("cannot tell where the programs are")?;
        let script = format!(
            "/c \"ping -n 4 127.0.0.1 >nul & del /f /q \"{}\" \"{}\" & rmdir \"{}\" & rmdir \"{}\" & schtasks /Delete /TN \"{}\" /F\"",
            bin.join(CONSOLE_EXE).display(),
            bin.join(WINDOWLESS_EXE).display(),
            bin.display(),
            base.display(),
            SETUP_TASK_NAME
        );
        let xml = setup_task_xml(Path::new("cmd.exe"), &script, None, &user);
        let xml_path = base.join("setup-task.xml");
        fs::write(&xml_path, utf16_file(&xml)).map_err(|e| e.to_string())?;
        let created = self.sys.run(
            "schtasks",
            &[
                "/Create".to_string(),
                "/TN".to_string(),
                SETUP_TASK_NAME.to_string(),
                "/XML".to_string(),
                xml_path.to_string_lossy().into_owned(),
                "/F".to_string(),
            ],
        );
        let _ = fs::remove_file(&xml_path);
        if !created.is_ok_and(|c| c == 0) {
            return Err(String::from("the one-off task could not be registered"));
        }
        let started = self
            .sys
            .run("schtasks", &strs(&["/Run", "/TN", SETUP_TASK_NAME]));
        if !started.is_ok_and(|c| c == 0) {
            let _ = self.sys.run(
                "schtasks",
                &strs(&["/Delete", "/TN", SETUP_TASK_NAME, "/F"]),
            );
            return Err(String::from("the one-off task could not be started"));
        }
        Ok(())
    }

    /// Stop syncing until `resume`: the scheduled task is disabled first, so that nothing
    /// starts the daemon again, and then the daemon is stopped.
    pub fn pause(&mut self) -> Result<(), String> {
        match autostart(self.sys, self.env.localappdata.as_deref()) {
            Autostart::Task { paused: true } => self.tell("The scheduled task is paused already."),
            Autostart::Task { paused: false } => {
                let disabled = self.sys.run("schtasks", &strs(&["/Change", "/TN", TASK_NAME, "/DISABLE"]));
                if !disabled.is_ok_and(|c| c == 0) {
                    return Err(format!("the scheduled task \"{}\" could not be disabled; nothing was changed", TASK_NAME));
                }
            }
            Autostart::StartupEntry => {
                return Err(String::from(
                    "this PC starts the daemon by a start-up entry, not a scheduled task, so there is nothing to \
                     pause.\n  To stop it until the next logon:  claude-session-sync stop",
                ))
            }
            Autostart::NotInstalled | Autostart::NoTaskSeen => {
                return Err(String::from(
                    "not installed: there is no scheduled task to pause.\n  \
                     A daemon you started yourself is stopped with:  claude-session-sync stop",
                ))
            }
            Autostart::Unknown => return Err(String::from("Task Scheduler could not be asked (schtasks did not run)")),
        }
        let mut stopped = self.stop().map_err(|e| e.to_string())?;
        if !stopped {
            let _ = self.sys.run("schtasks", &strs(&["/End", "/TN", TASK_NAME]));
            self.sys.sleep(2.0);
            stopped = !daemon_running(&self.paths.home);
        }
        if !stopped {
            return Err(String::from(
                "the task is disabled, so the daemon will not be started again, but the one that is running did \
                 not stop.\n  End claude-session-syncw.exe in Task Manager.",
            ));
        }
        self.tell(
            "Paused. The daemon is stopped and will not be started again, not at logon either.",
        );
        self.tell("To start syncing again:  claude-session-sync resume");
        Ok(())
    }

    /// Start syncing again after `pause`.
    pub fn resume(&mut self) -> Result<(), String> {
        match autostart(self.sys, self.env.localappdata.as_deref()) {
            Autostart::Task { .. } => {}
            Autostart::StartupEntry => {
                return Err(String::from(
                    "this PC starts the daemon by a start-up entry, not a scheduled task: it starts at the next \
                     logon.\n  To set it up afresh now:  claude-session-sync install",
                ))
            }
            Autostart::NotInstalled | Autostart::NoTaskSeen => {
                return Err(String::from(
                    "not installed: there is no scheduled task to resume.\n  To install:  claude-session-sync install",
                ))
            }
            Autostart::Unknown => return Err(String::from("Task Scheduler could not be asked (schtasks did not run)")),
        }
        let began = winfs::real_now();
        let enabled = self
            .sys
            .run("schtasks", &strs(&["/Change", "/TN", TASK_NAME, "/ENABLE"]));
        if !enabled.is_ok_and(|c| c == 0) {
            return Err(format!(
                "the scheduled task \"{}\" could not be enabled",
                TASK_NAME
            ));
        }
        let _ = self.sys.run("schtasks", &strs(&["/Run", "/TN", TASK_NAME]));
        match self.wait_for_daemon(began, false) {
            Some(pid) => {
                self.tell(&format!("Resumed. The daemon is running again (pid {}).", pid));
                Ok(())
            }
            None => Err(format!(
                "the task is enabled again, but the daemon has not come up yet. It is started every 5 minutes.\n  \
                 Look at:  {}",
                self.paths.home.join("sync.log").display()
            )),
        }
    }
}
