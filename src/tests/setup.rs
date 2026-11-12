//! install / uninstall / pause / resume, and what `status` and `log` show. Nothing here
//! touches Task Scheduler, the registry or any process: the system's tools are replaced by a
//! recorder that keeps a little state of its own (is the task registered, is it disabled,
//! what is the user's PATH, is this process inside the Claude app's sandbox).
//!
//! These are the tests for Windows; `setup_unix` has those for macOS and Linux, and
//! `commands` what the command line does alike on all three.

use std::io;
use std::os::windows::fs::OpenOptionsExt;

use serde_json::json;

use super::*;
use crate::cli::{self, Console};
use crate::consts::VERSION;
use crate::daemon::{acquire_lock, Lock};
use crate::install::*;
use crate::winreg::Text;

type Start = Box<dyn FnMut()>;
type Answer = Box<dyn FnMut(&str)>;
type Held = Rc<RefCell<Option<Lock>>>;

fn task_definition(enabled: bool, arguments: &str) -> String {
    let definition = task_definition_without_action(enabled);
    if arguments.is_empty() {
        return definition;
    }
    let action = format!(
        "  <Actions Context=\"Author\">\r\n    <Exec>\r\n      <Command>C:\\x\\daemon.exe</Command>\r\n      \
         <Arguments>{}</Arguments>\r\n    </Exec>\r\n  </Actions>\r\n</Task>",
        arguments.replace('&', "&amp;").replace('"', "&quot;")
    );
    definition.replace("</Task>", &action)
}

fn task_definition_without_action(enabled: bool) -> String {
    // as `schtasks /Query /XML` prints it: the triggers have an Enabled of their own
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\r\n<Task>\r\n  <Triggers>\r\n    <LogonTrigger>\r\n      \
         <Enabled>true</Enabled>\r\n    </LogonTrigger>\r\n  </Triggers>\r\n  <Settings>\r\n    \
         <MultipleInstancesPolicy>Parallel</MultipleInstancesPolicy>\r\n    <Enabled>{}</Enabled>\r\n  </Settings>\r\n</Task>\r\n",
        enabled
    )
}

/// Records what would be run. `refuse` lists (program, first argument) pairs that fail.
pub(super) struct Recorder {
    pub(super) ran: Vec<String>,
    slept: f64,
    refuse: Vec<(&'static str, &'static str)>,
    /// What to do when the daemon is started: stands in for the daemon coming up.
    on_start: Option<Start>,
    /// The scheduled task, if it is registered: is it enabled.
    task: Option<bool>,
    /// The arguments of the daemon that task starts (empty: the definition shows none).
    arguments: String,
    run_entry: bool,
    ours: Vec<u32>,
    asked: Vec<(u32, Option<f64>)>,
    ended: Vec<u32>,
    path: Option<Text>,
    path_writes: Vec<Text>,
    /// Writing the PATH fails / writes something else than was asked.
    path_refused: bool,
    path_garbled: bool,
    announced: usize,
    /// As inside the Claude app: what is written to the user's registry is seen nowhere else.
    sandboxed: bool,
    /// The one-off task, if it is registered: the command it is to run.
    setup_task: Option<String>,
    /// What running the one-off task does (it is given the command).
    on_setup: Option<Answer>,
    setup_refused: bool,
    /// Is the desktop app running, as far as the command line is to know.
    pub(super) app: bool,
}

impl Recorder {
    pub(super) fn new() -> Recorder {
        Recorder {
            ran: Vec::new(),
            slept: 0.0,
            refuse: Vec::new(),
            on_start: None,
            task: None,
            arguments: String::new(),
            run_entry: false,
            ours: Vec::new(),
            asked: Vec::new(),
            ended: Vec::new(),
            path: None,
            path_writes: Vec::new(),
            path_refused: false,
            path_garbled: false,
            announced: 0,
            sandboxed: false,
            setup_task: None,
            on_setup: None,
            setup_refused: false,
            app: false,
        }
    }

    /// As in a terminal inside the Claude app.
    fn in_the_app() -> Recorder {
        Recorder {
            sandboxed: true,
            ..Recorder::new()
        }
    }

    /// As on a PC where this tool is installed.
    pub(super) fn installed() -> Recorder {
        Recorder {
            task: Some(true),
            ..Recorder::new()
        }
    }

    fn count(&self, what: &str) -> usize {
        self.ran.iter().filter(|c| c.contains(what)).count()
    }

    fn position(&self, what: &str) -> usize {
        self.ran
            .iter()
            .position(|c| c.starts_with(what))
            .unwrap_or_else(|| panic!("never ran: {}", what))
    }
}

impl System for Recorder {
    fn run_output(&mut self, program: &str, args: &[String]) -> io::Result<(i32, String)> {
        // a task definition is only there while schtasks runs: keep what it said
        let mut line = format!("{} {}", program, args.join(" "));
        if let Some(i) = args
            .iter()
            .position(|a| a == "/XML")
            .filter(|_| args[0] == "/Create")
        {
            let bytes = fs::read(&args[i + 1]).unwrap();
            assert_eq!(&bytes[..2], &[0xFF, 0xFE]);
            let units: Vec<u16> = bytes[2..]
                .chunks(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            line.push_str(&format!(" <<{}>>", String::from_utf16(&units).unwrap()));
        }
        self.ran.push(line);
        let verb = args.first().map(String::as_str).unwrap_or("");
        if self.refuse.iter().any(|(p, a)| *p == program && *a == verb) {
            return Ok((1, String::new()));
        }
        let name = args
            .iter()
            .position(|a| a == "/TN")
            .and_then(|i| args.get(i + 1));
        if program == "schtasks" && name.is_some_and(|n| n == SETUP_TASK_NAME) {
            let code = match verb {
                "/Create" if !self.setup_refused => {
                    // (the line just recorded, with the definition it was given)
                    self.setup_task = self.ran.last().cloned();
                    0
                }
                "/Run" => match (self.setup_task.clone(), self.on_setup.as_mut()) {
                    (Some(command), Some(answer)) => {
                        answer(&command);
                        0
                    }
                    (Some(_), None) => 0,
                    _ => 1,
                },
                "/Delete" => i32::from(self.setup_task.take().is_none()),
                _ => 1,
            };
            return Ok((code, String::new()));
        }
        let mut out = String::new();
        let code = match (program, verb) {
            ("schtasks", "/Create") => {
                self.task = Some(true);
                0
            }
            ("schtasks", "/Query") => match self.task {
                Some(enabled) => {
                    out = task_definition(enabled, &self.arguments);
                    0
                }
                None => 1,
            },
            ("schtasks", "/Delete") => i32::from(self.task.take().is_none()),
            ("schtasks", "/Change") if self.task.is_some() => {
                self.task = Some(args.last().is_some_and(|a| a == "/ENABLE"));
                0
            }
            ("schtasks", "/Run") if self.task == Some(true) => {
                if let Some(start) = self.on_start.as_mut() {
                    start();
                }
                0
            }
            ("schtasks", "/End") if self.task.is_some() => 0,
            ("schtasks", _) => 1,
            ("reg", "query") => i32::from(!self.run_entry),
            ("reg", "add") => {
                self.run_entry = true;
                0
            }
            ("reg", "delete") => i32::from(!std::mem::take(&mut self.run_entry)),
            _ => 0,
        };
        Ok((code, out))
    }

    fn spawn_detached(&mut self, program: &Path, args: &[String]) -> io::Result<()> {
        self.ran
            .push(format!("spawn {} {}", program.display(), args.join(" ")));
        if let Some(start) = self.on_start.as_mut() {
            start();
        }
        Ok(())
    }

    fn sleep(&mut self, seconds: f64) {
        self.slept += seconds;
    }

    fn is_ours(&mut self, pid: u32, started: Option<f64>) -> bool {
        self.asked.push((pid, started));
        self.ours.contains(&pid)
    }

    fn terminate(&mut self, pid: u32) -> bool {
        self.ended.push(pid);
        true
    }

    fn user_path(&mut self) -> io::Result<Option<Text>> {
        Ok(self.path.clone())
    }

    fn set_user_path(&mut self, path: &Text) -> io::Result<()> {
        if self.path_refused {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "access is denied",
            ));
        }
        self.path_writes.push(path.clone());
        self.path = Some(if self.path_garbled && self.path_writes.len() == 1 {
            Text {
                value: String::from("garbled"),
                expand: path.expand,
            }
        } else {
            path.clone()
        });
        Ok(())
    }

    fn environment_changed(&mut self) {
        self.announced += 1;
    }

    fn sandboxed(&mut self, _localappdata: Option<&Path>) -> bool {
        self.sandboxed
    }

    fn app_running_override(&mut self) -> Option<bool> {
        Some(self.app)
    }
}

/// Have the app's log say it has been in A and in B and is signed out now, so that what the
/// command line sees (it uses the real defaults) does not depend on a desktop app running here.
pub(super) fn signed_out(b: &Base) {
    use super::homes::at;
    b.write_log(&[
        &at("09:00:00", Some(A)),
        &at("10:00:00", None),
        &at("10:00:05", Some(B)),
        &at("11:00:00", None),
        &at("11:00:05", Some(C)),
        &at("12:00:00", None),
    ]);
}

/// A folder with stand-ins for the two programs, as a download would have them.
fn programs(b: &Base) -> PathBuf {
    let dir = b.tmp.join("download");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(CONSOLE_EXE), b"console program").unwrap();
    fs::write(dir.join(WINDOWLESS_EXE), b"windowless program").unwrap();
    dir.join(CONSOLE_EXE)
}

fn bin(b: &Base) -> PathBuf {
    b.home.join("bin")
}

/// What a daemon that has just come up leaves behind: the lock, and its report.
fn daemon_comes_up(b: &Base, version: &'static str, started_ago: f64) -> (Start, Held) {
    let held: Held = Rc::new(RefCell::new(None));
    let (slot, home) = (held.clone(), b.home.clone());
    let start = move || {
        *slot.borrow_mut() = acquire_lock(&home).unwrap();
        let at = (now() - started_ago) as i64;
        let st = json!({"pid": 4242, "version": version, "started": at, "heartbeat": now() as i64});
        fs::write(home.join("status.json"), st.to_string()).unwrap();
    };
    (Box::new(start), held)
}

/// Run one of the four with the recorder. What it said, and how it ended.
fn act(
    b: &Base,
    sys: &mut Recorder,
    exe: PathBuf,
    what: &str,
) -> (Result<(), String>, Vec<String>) {
    let (env, paths) = (b.env(), b.paths());
    let mut said = Vec::new();
    let mut say = |l: &str| said.push(l.to_string());
    let mut setup = Setup {
        env: &env,
        paths: &paths,
        current_exe: exe,
        sys,
        say: &mut say,
    };
    let result = match what {
        "install" => setup.install(&Given::default()),
        "install --delete-sync" => setup.install(&Given {
            delete_sync: Some(true),
            ..Given::default()
        }),
        "install --no-delete-sync" => setup.install(&Given {
            delete_sync: Some(false),
            ..Given::default()
        }),
        "install --interval 600 --quick 0 --days 90 --delete-sync" => setup.install(&Given {
            interval: Some(600),
            quick: Some(0),
            days: Some(90),
            delete_sync: Some(true),
        }),
        "uninstall" => setup.uninstall(false),
        "uninstall --purge" => setup.uninstall(true),
        "pause" => setup.pause(),
        "resume" => setup.resume(),
        other => panic!("{}", other),
    };
    (result, said)
}

fn run_install(b: &Base, sys: &mut Recorder) -> (Result<(), String>, Vec<String>) {
    let exe = programs(b);
    act(b, sys, exe, "install")
}

fn says(said: &[String], what: &str) -> bool {
    said.iter().any(|l| l.contains(what))
}

fn text(value: &str, expand: bool) -> Text {
    Text {
        value: value.to_string(),
        expand,
    }
}

pub(super) fn cli(b: &Base, sys: &mut Recorder, args: &[&str]) -> (i32, Vec<String>) {
    let (con, shown) = Console::capture();
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let code = cli::run_with(&args, &b.env(), &con, sys);
    let lines = String::from_utf8_lossy(&shown.borrow())
        .lines()
        .map(String::from)
        .collect();
    (code, lines)
}

// ---------------------------------------------------------------------- the task

#[test]
fn daemon_arguments_follow_the_settings() {
    assert_eq!(
        daemon_arguments(&Settings::default()),
        "daemon --interval 3600 --quick 10"
    );
    let s = Settings {
        interval: 600,
        quick: 0,
        delete_sync: true,
        days: 90,
    };
    assert_eq!(
        daemon_arguments(&s),
        "daemon --interval 600 --quick 0 --delete-sync --days 90"
    );
    // ...and they are arguments the program itself understands
    let parsed = cli::parse_args(
        &daemon_arguments(&s)
            .split(' ')
            .map(String::from)
            .collect::<Vec<_>>(),
    )
    .unwrap();
    assert_eq!(
        parsed.command,
        cli::Command::Daemon {
            interval: 600,
            quick: 0
        }
    );
    assert_eq!((parsed.days, parsed.delete_sync), (Some(90), Some(true)));
}

#[test]
fn task_definition_has_the_settings_the_daemon_relies_on() {
    let exe = Path::new(
        r"C:\Users\someone\AppData\Local\claude-session-sync\bin\claude-session-syncw.exe",
    );
    let xml = task_xml(
        exe,
        "daemon --interval 3600 --quick 10",
        exe.parent().unwrap(),
        r"PC\someone",
        "2026-11-11T12:00:00",
        true,
    );
    for part in [
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>",
        "<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">",
        // at logon, for this user
        "<LogonTrigger>\r\n      <Enabled>true</Enabled>\r\n      <UserId>PC\\someone</UserId>\r\n    </LogonTrigger>",
        // the watchdog: every five minutes, for ever
        "<Repetition>\r\n        <Interval>PT5M</Interval>\r\n        <StopAtDurationEnd>false</StopAtDurationEnd>\r\n      </Repetition>",
        "<StartBoundary>2026-11-11T12:00:00</StartBoundary>",
        // only while the user is logged on, without elevation
        "<LogonType>InteractiveToken</LogonType>",
        "<RunLevel>LeastPrivilege</RunLevel>",
        // a second copy may start (it leaves, or takes over a stalled one)
        "<MultipleInstancesPolicy>Parallel</MultipleInstancesPolicy>",
        "<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>",
        "<StartWhenAvailable>true</StartWhenAvailable>",
        "<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>",
        "<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>",
        "<RestartOnFailure>\r\n      <Interval>PT1M</Interval>\r\n      <Count>3</Count>\r\n    </RestartOnFailure>",
        "<Command>C:\\Users\\someone\\AppData\\Local\\claude-session-sync\\bin\\claude-session-syncw.exe</Command>",
        "<Arguments>daemon --interval 3600 --quick 10</Arguments>",
        "<WorkingDirectory>C:\\Users\\someone\\AppData\\Local\\claude-session-sync\\bin</WorkingDirectory>",
    ] {
        assert!(xml.contains(part), "missing: {}", part);
    }
    assert!(!xml.contains("<Duration>")); // no end to the repetition
    assert_eq!(xml.matches("<TimeTrigger>").count(), 1);
    // every element that is opened is closed
    for tag in [
        "Task",
        "RegistrationInfo",
        "Triggers",
        "Principals",
        "Principal",
        "Settings",
        "IdleSettings",
        "Actions",
        "Exec",
    ] {
        assert!(
            xml.contains(&format!("<{}", tag)) && xml.contains(&format!("</{}>", tag)),
            "{}",
            tag
        );
    }
    assert!(!task_is_disabled(&xml)); // as registered, it is enabled
}

#[test]
fn task_definition_without_a_logon_trigger_keeps_the_watchdog() {
    let exe = Path::new(r"C:\bin\claude-session-syncw.exe");
    let xml = task_xml(
        exe,
        "daemon",
        exe.parent().unwrap(),
        r"PC\someone",
        "2026-11-11T12:00:00",
        false,
    );
    assert!(!xml.contains("LogonTrigger"));
    assert!(xml.contains("<Interval>PT5M</Interval>"));
}

#[test]
fn task_definition_escapes_what_xml_would_misread() {
    let exe = Path::new(r"C:\Users\R&D <team>\bin\claude-session-syncw.exe");
    let xml = task_xml(
        exe,
        "daemon",
        exe.parent().unwrap(),
        r"PC\O'Neil & co",
        "2026-11-11T12:00:00",
        true,
    );
    assert!(xml.contains(
        r"<Command>C:\Users\R&amp;D &lt;team&gt;\bin\claude-session-syncw.exe</Command>"
    ));
    assert!(xml.contains(r"<UserId>PC\O&apos;Neil &amp; co</UserId>"));
    assert!(!xml.contains("R&D"));
}

#[test]
fn task_file_is_utf16_with_a_byte_order_mark() {
    assert_eq!(utf16_file("aé"), vec![0xFF, 0xFE, b'a', 0x00, 0xE9, 0x00]);
}

#[test]
fn start_up_entry_quotes_the_program() {
    let exe = Path::new(r"C:\Program Files\x\claude-session-syncw.exe");
    assert_eq!(
        run_entry_command(exe, "daemon --quick 10"),
        "\"C:\\Program Files\\x\\claude-session-syncw.exe\" daemon --quick 10"
    );
}

#[test]
fn task_runs_as_the_current_user() {
    let b = Base::new();
    assert_eq!(task_user(&b.env()), Some(String::from(r"PC\someone")));
    assert_eq!(
        task_user(&Env {
            username: Some("solo".into()),
            ..Env::default()
        }),
        Some(String::from("solo"))
    );
    assert_eq!(task_user(&Env::default()), None);
}

#[test]
fn a_disabled_task_is_told_from_a_disabled_trigger() {
    assert!(!task_is_disabled(&task_definition(true, "")));
    assert!(task_is_disabled(&task_definition(false, "")));
    let trigger_off = task_definition(true, "").replacen(
        "<Enabled>true</Enabled>",
        "<Enabled>false</Enabled>",
        1,
    );
    assert!(!task_is_disabled(&trigger_off));
    assert!(!task_is_disabled("not a task definition"));
}

#[test]
fn how_the_daemon_is_started_is_asked_of_the_system() {
    assert_eq!(
        autostart(&mut Recorder::new(), None),
        Autostart::NotInstalled
    );
    assert_eq!(
        autostart(&mut Recorder::installed(), None),
        Autostart::Task { paused: false }
    );
    assert_eq!(
        autostart(
            &mut Recorder {
                task: Some(false),
                ..Recorder::new()
            },
            None
        ),
        Autostart::Task { paused: true }
    );
    assert_eq!(
        autostart(
            &mut Recorder {
                run_entry: true,
                ..Recorder::new()
            },
            None
        ),
        Autostart::StartupEntry
    );
    let mut sys = Recorder::installed();
    autostart(&mut sys, None);
    assert_eq!(
        sys.ran,
        vec!["schtasks /Query /TN Claude Session Sync /XML"]
    ); // a question, nothing more
}

// ---------------------------------------------------------------------- install

#[test]
fn install_copies_registers_starts_and_confirms() {
    let b = Base::new();
    let mut sys = Recorder::new();
    let (start, held) = daemon_comes_up(&b, VERSION, 0.0);
    sys.on_start = Some(start);
    let exe = programs(&b);
    let (result, said) = act(&b, &mut sys, exe, "install --delete-sync");
    assert_eq!(result, Ok(()));
    assert_eq!(
        fs::read(bin(&b).join(CONSOLE_EXE)).unwrap(),
        b"console program"
    );
    assert_eq!(
        fs::read(bin(&b).join(WINDOWLESS_EXE)).unwrap(),
        b"windowless program"
    );
    // one registration (the first form was accepted), then the start
    assert_eq!(
        sys.count("schtasks /Create /TN Claude Session Sync /XML"),
        1
    );
    let create = &sys.ran[sys.position("schtasks /Create")];
    assert!(create.contains(" /F "));
    assert!(create.contains("<LogonTrigger>"));
    assert!(create.contains(&format!(
        "<Command>{}</Command>",
        bin(&b).join(WINDOWLESS_EXE).display()
    )));
    assert!(
        create.contains("<Arguments>daemon --interval 3600 --quick 10 --delete-sync</Arguments>")
    );
    assert!(create.contains(r"<UserId>PC\someone</UserId>"));
    assert!(
        sys.position("schtasks /Create") < sys.position("schtasks /Run /TN Claude Session Sync")
    );
    assert_eq!(sys.count("reg add"), 0);
    assert!(!b.home.join("task.xml").exists()); // the definition file is not left behind
                                                // what the user is told: that it worked, what was set up, where things are, what to do next
    assert!(said[0].starts_with("Installed."));
    assert!(says(
        &said,
        &format!("version {} is running now (pid 4242)", VERSION)
    ));
    assert!(says(
        &said,
        "scheduled task \"Claude Session Sync\": at logon"
    ));
    assert!(says(
        &said,
        "full pass every 3600 s, change check every 10 s, archived sessions up to 30 days old"
    ));
    assert!(says(&said, "deletions are carried over"));
    assert!(says(&said, &format!("Programs   {}", bin(&b).display())));
    assert!(says(&said, &format!("Data       {}", b.home.display())));
    for next in [
        "claude-session-sync status",
        "claude-session-sync log",
        "claude-session-sync pause",
        "claude-session-sync uninstall",
    ] {
        assert!(says(&said, next), "{}", next);
    }
    assert!(says(&said, "after you restart the app or sign in again"));
    drop(held);
}

#[test]
fn install_says_so_when_the_new_daemon_does_not_come_up() {
    let b = Base::new();
    let mut sys = Recorder::new();
    let (result, said) = run_install(&b, &mut sys);
    let why = result.unwrap_err();
    assert!(why.contains("did not come up"));
    assert!(why.contains("sync.log") && why.contains("status")); // ...and where to look
    assert!(!says(&said, "Installed."));
    assert!(sys.slept >= 20.0); // it waited before giving up
}

#[test]
fn install_does_not_take_an_old_daemon_for_the_new_one() {
    let b = Base::new();
    let mut sys = Recorder::new();
    let (start, held) = daemon_comes_up(&b, "8.1", 0.0); // some other version holds the lock and reports
    sys.on_start = Some(start);
    assert!(run_install(&b, &mut sys)
        .0
        .unwrap_err()
        .contains("did not come up"));
    drop(held);
    // ...nor one of this version that was started before the install began
    let b = Base::new();
    let mut sys = Recorder::new();
    let (start, held) = daemon_comes_up(&b, VERSION, 3600.0);
    sys.on_start = Some(start);
    assert!(run_install(&b, &mut sys)
        .0
        .unwrap_err()
        .contains("did not come up"));
    drop(held);
}

#[test]
fn install_falls_back_to_the_watchdog_alone_and_then_to_a_start_up_entry() {
    // the logon trigger is refused once, the plain task is accepted
    struct Once(Recorder, bool);
    impl System for Once {
        fn run_output(&mut self, program: &str, args: &[String]) -> io::Result<(i32, String)> {
            if program == "schtasks"
                && args[0] == "/Create"
                && !std::mem::replace(&mut self.1, true)
            {
                self.0.refuse.push(("schtasks", "/Create"));
                let refused = self.0.run_output(program, args);
                self.0.refuse.clear();
                return refused;
            }
            self.0.run_output(program, args)
        }
        fn spawn_detached(&mut self, program: &Path, args: &[String]) -> io::Result<()> {
            self.0.spawn_detached(program, args)
        }
        fn sleep(&mut self, seconds: f64) {
            self.0.sleep(seconds)
        }
        fn is_ours(&mut self, pid: u32, started: Option<f64>) -> bool {
            self.0.is_ours(pid, started)
        }
        fn terminate(&mut self, pid: u32) -> bool {
            self.0.terminate(pid)
        }
        fn user_path(&mut self) -> io::Result<Option<Text>> {
            self.0.user_path()
        }
        fn set_user_path(&mut self, path: &Text) -> io::Result<()> {
            self.0.set_user_path(path)
        }
        fn environment_changed(&mut self) {
            self.0.environment_changed()
        }
        fn sandboxed(&mut self, localappdata: Option<&Path>) -> bool {
            self.0.sandboxed(localappdata)
        }
    }
    let b = Base::new();
    let mut once = Once(Recorder::new(), false);
    let (start, held) = daemon_comes_up(&b, VERSION, 0.0);
    once.0.on_start = Some(start);
    let (env, paths) = (b.env(), b.paths());
    let mut said = Vec::new();
    let mut say = |l: &str| said.push(l.to_string());
    let result = Setup {
        env: &env,
        paths: &paths,
        current_exe: programs(&b),
        sys: &mut once,
        say: &mut say,
    }
    .install(&Given::default());
    assert_eq!(result, Ok(()));
    let creates: Vec<&String> = once
        .0
        .ran
        .iter()
        .filter(|c| c.starts_with("schtasks /Create"))
        .collect();
    assert_eq!(creates.len(), 2);
    assert!(creates[0].contains("<LogonTrigger>") && !creates[1].contains("<LogonTrigger>"));
    assert!(says(
        &said,
        "within 5 minutes of logon (a logon trigger was refused)"
    ));
    drop(held);

    // Task Scheduler refuses altogether: a start-up entry, and the daemon is started directly
    let b = Base::new();
    let mut sys = Recorder::new();
    sys.refuse.push(("schtasks", "/Create"));
    let (start, held) = daemon_comes_up(&b, VERSION, 0.0);
    sys.on_start = Some(start);
    let (result, said) = run_install(&b, &mut sys);
    assert_eq!(result, Ok(()));
    let exe = bin(&b).join(WINDOWLESS_EXE);
    let add = &sys.ran[sys.position("reg add")];
    assert!(add.starts_with(&format!(
        "reg add {} /v Claude Session Sync /t REG_SZ /d \"{}\" daemon",
        RUN_KEY,
        exe.display()
    )));
    assert_eq!(
        sys.count(&format!(
            "spawn {} daemon --interval 3600 --quick 10",
            exe.display()
        )),
        1
    );
    assert_eq!(sys.count("schtasks /Run"), 0);
    assert!(says(&said, "start-up entry only") && says(&said, "is not restarted if it stops"));
    drop(held);

    // ...and if that is refused too, it is an error, not a silent success
    let b = Base::new();
    let mut sys = Recorder::new();
    sys.refuse.push(("schtasks", "/Create"));
    sys.refuse.push(("reg", "add"));
    let (result, _) = run_install(&b, &mut sys);
    assert!(result.unwrap_err().contains("Task Scheduler refused"));
}

#[test]
fn install_stops_a_running_daemon_first_and_does_not_copy_onto_itself() {
    let b = Base::new();
    // already installed: the program that runs is the one in the bin folder
    fs::create_dir_all(bin(&b)).unwrap();
    fs::write(bin(&b).join(CONSOLE_EXE), b"installed console").unwrap();
    fs::write(bin(&b).join(WINDOWLESS_EXE), b"installed windowless").unwrap();
    // a daemon that will not answer holds the lock
    let stuck = acquire_lock(&b.home).unwrap().unwrap();
    let mut sys = Recorder::installed();
    let (result, _) = act(&b, &mut sys, bin(&b).join(CONSOLE_EXE), "install");
    assert!(result.is_err()); // nothing new came up: the old one still holds the lock
    assert!(b.home.join("stop.flag").exists()); // it was asked to stop
    assert_eq!(sys.count("schtasks /End /TN Claude Session Sync"), 1); // ...and, being a task, ended
    assert!(sys.position("schtasks /End") < sys.position("schtasks /Create"));
    assert_eq!(
        fs::read(bin(&b).join(CONSOLE_EXE)).unwrap(),
        b"installed console"
    );
    drop(stuck);
}

#[test]
fn install_needs_both_programs() {
    let b = Base::new();
    let exe = programs(&b);
    fs::remove_file(exe.with_file_name(WINDOWLESS_EXE)).unwrap();
    let mut sys = Recorder::new();
    let (result, _) = act(&b, &mut sys, exe, "install");
    assert!(result.unwrap_err().contains(WINDOWLESS_EXE));
    assert!(sys.ran.is_empty() && sys.path_writes.is_empty()); // nothing was touched
}

// ---------------------------------------------------------------------- the PATH

#[test]
fn folder_is_added_to_a_path_once_and_taken_out_alone() {
    let ours = [
        r"C:\Users\someone\AppData\Local\claude-session-sync\bin",
        r"%LOCALAPPDATA%\claude-session-sync\bin",
    ];
    // added at the end; what is there stays as it is, spelling and all
    assert_eq!(path_with(None, &ours), Some(ours[0].to_string()));
    assert_eq!(path_with(Some(""), &ours), Some(ours[0].to_string()));
    assert_eq!(
        path_with(Some(r"%USERPROFILE%\bin;C:\Tools"), &ours),
        Some(format!(r"%USERPROFILE%\bin;C:\Tools;{}", ours[0]))
    );
    assert_eq!(
        path_with(Some(r"C:\Tools;"), &ours),
        Some(format!(r"C:\Tools;{}", ours[0]))
    );
    // not twice, however it is written
    for there in [
        format!(r"C:\Tools;{}", ours[0]),
        format!(r"{}\;C:\Tools", ours[0]),
        format!(r"C:\Tools;{};D:\x", ours[0].to_uppercase()),
        format!("C:\\Tools; \"{}\" ", ours[0]),
        format!(r"C:\Tools;{}", ours[0].replace('\\', "/")),
        format!(r"C:\Tools;{}", ours[1]),
    ] {
        assert_eq!(path_with(Some(&there), &ours), None, "{}", there);
    }
    // a folder that merely begins the same is another folder
    let near = format!(r"{}2;C:\Tools", ours[0]);
    assert_eq!(
        path_with(Some(&near), &ours),
        Some(format!("{};{}", near, ours[0]))
    );
    // taken out: only that entry, the others in their places and as they were
    assert_eq!(
        path_without(
            &format!(r"%USERPROFILE%\bin;{};C:\Tools;;D:\Has Space\", ours[0]),
            &ours
        ),
        Some(String::from(r"%USERPROFILE%\bin;C:\Tools;;D:\Has Space\"))
    );
    assert_eq!(
        path_without(&format!(r"{}\;{}", ours[0].to_uppercase(), ours[1]), &ours),
        Some(String::new())
    );
    assert_eq!(path_without(ours[0], &ours), Some(String::new()));
    assert_eq!(path_without(&near, &ours), None);
    assert_eq!(path_without(r"C:\Tools;D:\x", &ours), None);
    assert_eq!(path_without("", &ours), None);
}

#[test]
fn install_puts_the_programs_on_the_users_path_once() {
    let b = Base::new();
    let mut sys = Recorder::new();
    sys.path = Some(text(r"%USERPROFILE%\bin;C:\Tools", true));
    let (start, held) = daemon_comes_up(&b, VERSION, 0.0);
    sys.on_start = Some(start);
    let (result, said) = run_install(&b, &mut sys);
    assert_eq!(result, Ok(()));
    // the old value is kept as it was (not expanded, same kind), ours is added at the end
    let want = text(
        &format!(r"%USERPROFILE%\bin;C:\Tools;{}", bin(&b).display()),
        true,
    );
    assert_eq!(sys.path, Some(want.clone()));
    assert_eq!(sys.path_writes, vec![want.clone()]);
    assert_eq!(sys.announced, 1); // running programs are told
    assert!(says(&said, "added to your PATH: open a NEW terminal"));
    assert!(says(&said, "  claude-session-sync status")); // ...so the plain name is what to type
    drop(held);
    // installing again (an update) leaves the PATH alone
    let (start, held) = daemon_comes_up(&b, VERSION, 0.0);
    sys.on_start = Some(start);
    let (result, said) = run_install(&b, &mut sys);
    assert_eq!(result, Ok(()));
    assert_eq!(sys.path, Some(want));
    assert_eq!((sys.path_writes.len(), sys.announced), (1, 1));
    assert!(says(&said, "on your PATH already"));
    drop(held);
}

#[test]
fn path_keeps_its_kind_and_is_made_if_there_is_none() {
    // a plain (REG_SZ) value stays plain
    let b = Base::new();
    let mut sys = Recorder::new();
    sys.path = Some(text(r"C:\Tools", false));
    let (start, held) = daemon_comes_up(&b, VERSION, 0.0);
    sys.on_start = Some(start);
    assert_eq!(run_install(&b, &mut sys).0, Ok(()));
    assert_eq!(
        sys.path,
        Some(text(&format!(r"C:\Tools;{}", bin(&b).display()), false))
    );
    drop(held);
    // no user PATH at all: one is made, of the usual kind
    let b = Base::new();
    let mut sys = Recorder::new();
    let (start, held) = daemon_comes_up(&b, VERSION, 0.0);
    sys.on_start = Some(start);
    assert_eq!(run_install(&b, &mut sys).0, Ok(()));
    assert_eq!(sys.path, Some(text(&bin(&b).display().to_string(), true)));
    drop(held);
}

#[test]
fn path_that_cannot_be_changed_does_not_fail_the_install() {
    let b = Base::new();
    let mut sys = Recorder::new();
    sys.path = Some(text(r"C:\Tools", true));
    sys.path_refused = true;
    let (start, held) = daemon_comes_up(&b, VERSION, 0.0);
    sys.on_start = Some(start);
    let (result, said) = run_install(&b, &mut sys);
    assert_eq!(result, Ok(()));
    assert_eq!(sys.path, Some(text(r"C:\Tools", true)));
    assert_eq!(sys.announced, 0);
    assert!(says(
        &said,
        "PATH       could not be changed (access is denied)"
    ));
    // the commands to type are then given with the full path
    assert!(says(
        &said,
        &format!("\"{}\" status", bin(&b).join(CONSOLE_EXE).display())
    ));
    drop(held);
    // a write that does not read back as written is put back
    let b = Base::new();
    let mut sys = Recorder::new();
    sys.path = Some(text(r"C:\Tools", true));
    sys.path_garbled = true;
    let (start, held) = daemon_comes_up(&b, VERSION, 0.0);
    sys.on_start = Some(start);
    let (result, said) = run_install(&b, &mut sys);
    assert_eq!(result, Ok(()));
    assert_eq!(sys.path, Some(text(r"C:\Tools", true)));
    assert!(says(&said, "was put back"));
    drop(held);
}

// ---------------------------------------------------------------------- uninstall

#[test]
fn uninstall_removes_the_autostart_before_stopping_the_daemon() {
    let b = Base::new();
    b.rec(A, "s1", r());
    b.s().run(); // there is data: it must survive
    let kept = b.snapshot(&b.home);
    let mut sys = Recorder {
        run_entry: true,
        ..Recorder::installed()
    };
    let ours = bin(&b).display().to_string();
    sys.path = Some(text(&format!(r"%USERPROFILE%\bin;{};C:\Tools", ours), true));
    let (result, said) = act(&b, &mut sys, programs(&b), "uninstall");
    assert_eq!(result, Ok(()));
    assert_eq!(
        sys.ran,
        vec![
            "schtasks /Query /TN Claude Session Sync".to_string(),
            "schtasks /Delete /TN Claude Session Sync /F".to_string(),
            format!("reg query {} /v Claude Session Sync", RUN_KEY),
            format!("reg delete {} /v Claude Session Sync /f", RUN_KEY),
        ]
    );
    // only our entry leaves the PATH
    assert_eq!(sys.path, Some(text(r"%USERPROFILE%\bin;C:\Tools", true)));
    assert_eq!(sys.announced, 1);
    assert!(said[0].contains("Scheduled task") && said[0].ends_with("removed"));
    assert!(said[1].contains("Start-up entry") && said[1].ends_with("removed"));
    assert!(said[2].contains("PATH entry") && said[2].contains("removed"));
    assert!(said[3].contains("Daemon") && said[3].contains("not running any more"));
    // what is left, where it is, and how to go on from here
    assert!(says(&said, "Uninstalled. Nothing of yours was deleted."));
    assert!(says(&said, &b.home.display().to_string()));
    assert!(
        says(&said, "uninstall --purge") && says(&said, "undo --since") && says(&said, " install")
    );
    assert!(sys.ended.is_empty());
    for (path, version) in kept {
        assert_eq!(
            winfs::stat(&path).map(|st| (st.mtime_ns, st.size)).ok(),
            Some(version),
            "{}",
            path.display()
        );
    }
    assert!(!b.home.join("stop.flag").exists());
}

#[test]
fn uninstall_of_what_is_not_installed_changes_nothing() {
    let b = Base::new();
    let mut sys = Recorder::new();
    sys.path = Some(text(r"C:\Tools", true));
    let (result, said) = act(&b, &mut sys, programs(&b), "uninstall");
    assert_eq!(result, Ok(()));
    assert_eq!(sys.count("/Delete") + sys.count("reg delete"), 0);
    assert!(sys.path_writes.is_empty() && sys.announced == 0);
    assert!(said[0].ends_with("was not registered"));
}

#[test]
fn uninstall_ends_a_daemon_that_will_not_stop_only_if_it_is_that_daemon() {
    let b = Base::new();
    let stuck = acquire_lock(&b.home).unwrap().unwrap();
    fs::write(
        b.home.join("status.json"),
        json!({"pid": 777_777, "started": 1_700_000_000, "heartbeat": 1}).to_string(),
    )
    .unwrap();
    // the pid on record belongs to something else now
    let mut sys = Recorder::new();
    let (result, said) = act(&b, &mut sys, programs(&b), "uninstall");
    assert_eq!(result, Ok(()));
    assert!(sys.ended.is_empty());
    assert_eq!(sys.asked, vec![(777_777, Some(1_700_000_000.0))]); // asked with the start time it reported
    assert!(says(
        &said,
        "did NOT stop when asked: end claude-session-syncw.exe in Task Manager"
    ));
    // it is that daemon: ended
    let mut sys = Recorder::new();
    sys.ours.push(777_777);
    let (_, said) = act(&b, &mut sys, programs(&b), "uninstall");
    assert_eq!(sys.ended, vec![777_777]);
    assert!(says(
        &said,
        "did not stop when asked and was ended (pid 777777)"
    ));
    drop(stuck);
}

#[test]
fn purge_deletes_this_tools_own_files_and_nothing_else() {
    let b = Base::new();
    b.install(2.0 * HOUR, 0.0);
    b.rec(B, "s1", r().cli("c1").last(HOUR));
    b.rec(
        C,
        "s1",
        r().cli("c1")
            .last(HOUR)
            .mtime(2.0 * HOUR)
            .title("another version"),
    );
    b.delete(A, "s1", 600.0, Some("c1"));
    b.rec(A, "s2", r());
    b.rec(
        B,
        "s2",
        r().last(5.0 * HOUR).mtime(4.0 * HOUR).title("older"),
    );
    b.s().run(); // backups, quarantine, replaced, journal, manifest, state, log
    fs::create_dir_all(bin(&b)).unwrap();
    fs::write(bin(&b).join(CONSOLE_EXE), b"x").unwrap();
    fs::write(bin(&b).join(WINDOWLESS_EXE), b"x").unwrap();
    for made in [
        "backups",
        "quarantine",
        "replaced",
        "journal.jsonl",
        "manifest.json",
        "state.json",
        "sync.log",
    ] {
        assert!(b.home.join(made).exists(), "{}", made);
    }
    let store = b.contents();
    let transcripts = b.snapshot(&b.tmp.join("user").join(".claude"));
    let mut sys = Recorder::installed();
    let (result, said) = act(&b, &mut sys, programs(&b), "uninstall --purge");
    assert_eq!(result, Ok(()));
    assert!(!b.home.exists()); // (the folder held nothing but ours, so it went too)
    assert!(says(&said, "Uninstalled and purged"));
    assert!(!says(&said, "Could NOT be deleted"));
    // the app's session lists and the transcripts are not this tool's files
    assert_eq!(store, b.contents());
    assert_eq!(transcripts, b.snapshot(&b.tmp.join("user").join(".claude")));
    assert!(b.tmp.join("download").join(CONSOLE_EXE).exists()); // nor is the copy that was run
}

/// A state folder with something in it that is not this tool's, and the programs installed,
/// one of them "running" (held open here without leave to delete, as a running program is).
fn purge_scene(b: &Base) -> (PathBuf, fs::File) {
    b.rec(A, "s1", r());
    b.s().run();
    fs::write(b.home.join("notes of my own.txt"), b"keep me").unwrap();
    fs::create_dir_all(bin(b)).unwrap();
    let exe = bin(b).join(CONSOLE_EXE);
    fs::write(&exe, b"x").unwrap();
    fs::write(bin(b).join(WINDOWLESS_EXE), b"x").unwrap();
    let in_use = fs::OpenOptions::new()
        .read(true)
        .share_mode(0x1)
        .open(&exe)
        .unwrap();
    (exe, in_use)
}

#[test]
fn purge_hands_the_running_program_to_a_one_off_task() {
    let b = Base::new();
    let (exe, in_use) = purge_scene(&b);
    let mut sys = Recorder::installed();
    let (result, said) = act(&b, &mut sys, exe.clone(), "uninstall --purge");
    assert_eq!(result, Ok(()));
    // what could be deleted here was; what is not this tool's is still there
    assert_eq!(listdir(&b.home), vec!["bin", "notes of my own.txt"]);
    assert_eq!(listdir(&bin(&b)), vec![CONSOLE_EXE]);
    // the rest is left to a task that waits a few seconds, deletes the two programs by name,
    // removes the folders only if they are empty by then, and then deletes itself
    let create = &sys.ran[sys.position("schtasks /Create /TN Claude Session Sync (setup) /XML")];
    assert!(create.contains("<Command>cmd.exe</Command>"));
    let script = format!(
        "<Arguments>/c &quot;ping -n 4 127.0.0.1 &gt;nul &amp; del /f /q &quot;{0}\\{1}&quot; &quot;{0}\\{2}&quot; &amp; rmdir &quot;{0}&quot; &amp; rmdir &quot;{3}&quot; &amp; schtasks /Delete /TN &quot;Claude Session Sync (setup)&quot; /F&quot;</Arguments>",
        bin(&b).display(),
        CONSOLE_EXE,
        WINDOWLESS_EXE,
        b.home.display()
    );
    assert!(create.contains(&script), "{}", create);
    assert!(!create.contains("rmdir /s")); // no folder is deleted with what may be in it
    assert!(!create.contains("WorkingDirectory")); // (it must not sit in the folder it removes)
    assert!(create.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"));
    assert_eq!(
        sys.ran.last().unwrap(),
        "schtasks /Run /TN Claude Session Sync (setup)"
    ); // started, and left to finish by itself
    assert!(!b.home.join("setup-task.xml").exists());
    assert!(says(&said, "a one-off task deletes it"));
    assert!(!says(&said, "Could NOT be deleted") && !says(&said, "rmdir /s /q"));
    drop(in_use);
}

#[test]
fn purge_says_what_it_could_not_delete_when_the_one_off_task_cannot_be_used() {
    let b = Base::new();
    let (exe, in_use) = purge_scene(&b);
    let mut sys = Recorder::installed();
    sys.setup_refused = true;
    let (result, said) = act(&b, &mut sys, exe.clone(), "uninstall --purge");
    assert_eq!(result, Ok(()));
    assert_eq!(listdir(&b.home), vec!["bin", "notes of my own.txt"]);
    assert_eq!(listdir(&bin(&b)), vec![CONSOLE_EXE]);
    let at = said
        .iter()
        .position(|l| l == "Could NOT be deleted (in use):")
        .unwrap();
    assert_eq!(said[at + 1].trim(), exe.display().to_string());
    assert!(says(&said, "A program cannot delete itself while it runs"));
    assert!(says(
        &said,
        &format!("rmdir /s /q \"{}\"", bin(&b).display())
    )); // that folder, not the one with the notes
    assert_eq!(
        sys.count("schtasks /Run /TN Claude Session Sync (setup)"),
        0
    );
    drop(in_use);
}

#[test]
fn purge_is_refused_while_the_daemon_runs() {
    let b = Base::new();
    b.rec(A, "s1", r());
    b.s().run();
    let stuck = acquire_lock(&b.home).unwrap().unwrap();
    let mut sys = Recorder::installed();
    let (result, _) = act(&b, &mut sys, programs(&b), "uninstall --purge");
    assert!(result.unwrap_err().contains("nothing was purged"));
    assert!(b.home.join("sync.log").exists() && b.home.join("backups").exists());
    drop(stuck);
}

// ---------------------------------------------------------------------- pause, resume

#[test]
fn pause_disables_the_task_before_it_stops_the_daemon() {
    let b = Base::new();
    let mut sys = Recorder::installed();
    let (result, said) = act(&b, &mut sys, programs(&b), "pause");
    assert_eq!(result, Ok(()));
    assert_eq!(
        sys.ran,
        vec![
            "schtasks /Query /TN Claude Session Sync /XML".to_string(),
            "schtasks /Change /TN Claude Session Sync /DISABLE".to_string(),
        ]
    );
    assert_eq!(sys.task, Some(false));
    assert!(said[0].starts_with("Paused."));
    assert!(says(&said, "claude-session-sync resume"));
    assert!(!b.home.join("stop.flag").exists()); // asked to stop, and (no daemon here) seen stopped
                                                 // pausing again is harmless
    let (result, said) = act(&b, &mut sys, programs(&b), "pause");
    assert_eq!(result, Ok(()));
    assert!(says(&said, "paused already"));
    assert_eq!(sys.count("/DISABLE"), 1);
    // a daemon that will not stop by itself is ended as a task
    let stuck = acquire_lock(&b.home).unwrap().unwrap();
    let mut sys = Recorder::installed();
    let (result, _) = act(&b, &mut sys, programs(&b), "pause");
    assert!(sys.position("schtasks /Change") < sys.position("schtasks /End"));
    assert!(result.unwrap_err().contains("did not stop")); // (the lock is still held here: said, not hidden)
    drop(stuck);
}

#[test]
fn pause_and_resume_say_so_when_there_is_no_task() {
    let b = Base::new();
    for what in ["pause", "resume"] {
        let mut sys = Recorder::new();
        let (result, said) = act(&b, &mut sys, programs(&b), what);
        assert!(result.unwrap_err().contains("not installed"), "{}", what);
        assert!(said.is_empty());
        assert_eq!(sys.count("/Change") + sys.count("/Run"), 0);
        let mut sys = Recorder {
            run_entry: true,
            ..Recorder::new()
        };
        let (result, _) = act(&b, &mut sys, programs(&b), what);
        assert!(result.unwrap_err().contains("start-up entry"), "{}", what);
    }
    let mut sys = Recorder::installed();
    sys.refuse.push(("schtasks", "/Change"));
    assert!(act(&b, &mut sys, programs(&b), "pause")
        .0
        .unwrap_err()
        .contains("could not be disabled"));
    assert!(!b.home.join("stop.flag").exists()); // nothing was changed: the daemon was not stopped either
}

#[test]
fn resume_enables_the_task_and_starts_the_daemon() {
    let b = Base::new();
    let mut sys = Recorder {
        task: Some(false),
        ..Recorder::new()
    };
    let (start, held) = daemon_comes_up(&b, VERSION, 0.0);
    sys.on_start = Some(start);
    let (result, said) = act(&b, &mut sys, programs(&b), "resume");
    assert_eq!(result, Ok(()));
    assert_eq!(
        sys.ran,
        vec![
            "schtasks /Query /TN Claude Session Sync /XML".to_string(),
            "schtasks /Change /TN Claude Session Sync /ENABLE".to_string(),
            "schtasks /Run /TN Claude Session Sync".to_string(),
        ]
    );
    assert_eq!(sys.task, Some(true));
    assert_eq!(
        said,
        vec!["Resumed. The daemon is running again (pid 4242)."]
    );
    drop(held);
    // enabled, but nothing came up: said so
    let b = Base::new();
    let mut sys = Recorder {
        task: Some(false),
        ..Recorder::new()
    };
    let (result, _) = act(&b, &mut sys, programs(&b), "resume");
    assert!(result.unwrap_err().contains("has not come up yet"));
    assert_eq!(sys.task, Some(true));
}

// ---------------------------------------------------------------------- status, log

#[test]
fn status_begins_with_a_one_line_summary() {
    let b = Base::new();
    let t = ms(HOUR);
    b.rec(A, "s1", r().extra(json!({"lastActivityAt": t})));
    fs::copy(b.path(A, "s1"), b.path(B, "s1")).unwrap(); // the same session, the same bytes
    b.rec(A, "s2", r().extra(json!({"lastActivityAt": t})));
    // same activity, different content: a conflict (both accounts have the session, though)
    b.rec(
        B,
        "s2",
        r().extra(json!({"lastActivityAt": t}))
            .title("another name"),
    );
    b.rec(A, "s3", r());
    b.tomb(B, "s3", ms(DAY)); // deleted in B: not everywhere
    fs::remove_dir(b.dir(C)).unwrap();
    // nothing installed, nothing running
    let mut sys = Recorder::new();
    let (code, out) = cli(&b, &mut sys, &["status"]);
    assert_eq!(code, 1);
    assert_eq!(
        out[0],
        "NOT RUNNING  no daemon | 2 of 3 sessions in every account | 1 conflict | 0 errors"
    );
    assert_eq!(
        out[1],
        "autostart: not installed (to install: claude-session-sync install)"
    );
    assert_eq!(out[2], "");
    assert_eq!(out[3], "daemon: NOT running"); // the details follow as before
    assert_eq!(sys.ran.len(), 2); // two questions were asked of the system, nothing was changed
                                  // installed and running
    let held = acquire_lock(&b.home).unwrap().unwrap();
    let report = |failed: u32, error: Option<&str>| {
        let st = json!({"pid": 4242, "version": VERSION, "started": now() as i64, "heartbeat": now() as i64,
                        "failed": failed, "last_error": error});
        fs::write(b.home.join("status.json"), st.to_string()).unwrap();
    };
    report(0, None);
    let (code, out) = cli(&b, &mut Recorder::installed(), &["status"]);
    assert_eq!(code, 0);
    assert_eq!(
        out[0],
        format!(
            "OK  daemon {} running | 2 of 3 sessions in every account | 1 conflict | 0 errors",
            VERSION
        )
    );
    assert_eq!(
        out[1],
        "autostart: installed (scheduled task \"Claude Session Sync\")"
    );
    assert!(out[3].starts_with("daemon: running (pid 4242"));
    report(2, Some("add 00000000/00000000 s9: access is denied"));
    assert!(
        cli(&b, &mut Recorder::installed(), &["status"]).1[0].ends_with("| 1 conflict | 2 errors")
    );
    // it has not reported for a long time (it took the lock before that, as a daemon does)
    let st = json!({"pid": 4242, "version": VERSION, "heartbeat": (now() - 2000.0) as i64});
    fs::write(b.home.join("status.json"), st.to_string()).unwrap();
    set_mtime(&b.home.join("daemon.lock"), now() - 3000.0);
    let (code, out) = cli(&b, &mut Recorder::installed(), &["status"]);
    assert_eq!(code, 1);
    assert!(
        out[0].starts_with(&format!(
            "STALLED  daemon {} has not reported for 20",
            VERSION
        )),
        "{}",
        out[0]
    );
    drop(held);
    // paused
    let (code, out) = cli(
        &b,
        &mut Recorder {
            task: Some(false),
            ..Recorder::new()
        },
        &["status"],
    );
    assert_eq!(code, 1);
    assert!(
        out[0].starts_with(
            "PAUSED  daemon stopped until you run: claude-session-sync resume | 2 of 3"
        ),
        "{}",
        out[0]
    );
    assert!(out[1].starts_with("autostart: installed, PAUSED"));
    // installed by a start-up entry only
    let (_, out) = cli(
        &b,
        &mut Recorder {
            run_entry: true,
            ..Recorder::new()
        },
        &["status"],
    );
    assert!(out[1].starts_with("autostart: installed (start-up entry only"));
}

#[test]
fn log_shows_the_last_lines() {
    let b = Base::new();
    let mut sys = Recorder::new();
    let (code, out) = cli(&b, &mut sys, &["log"]);
    assert_eq!(code, 0);
    assert_eq!(
        out,
        vec![format!(
            "no log yet ({})",
            b.home.join("sync.log").display()
        )]
    );
    assert!(!b.home.exists()); // looking for a log does not make a folder
    fs::create_dir_all(&b.home).unwrap();
    let lines = |from: usize, to: usize| {
        (from..=to)
            .map(|i| format!("2026-11-11 10:00:{:02} line {}", i % 60, i))
            .collect::<Vec<_>>()
    };
    fs::write(b.home.join("sync.log"), lines(1, 40).join("\r\n") + "\r\n").unwrap();
    assert_eq!(cli(&b, &mut sys, &["log"]).1, lines(11, 40)); // the last 30
    assert_eq!(cli(&b, &mut sys, &["log", "-n", "3"]).1, lines(38, 40));
    assert_eq!(cli(&b, &mut sys, &["log", "-n", "1000"]).1, lines(1, 40));
    assert!(cli(&b, &mut sys, &["log", "-n", "0"]).1.is_empty());
    // the rotated file is drawn on when the current one is short
    fs::write(b.home.join("sync.log.1"), lines(1, 40).join("\n") + "\n").unwrap();
    fs::write(b.home.join("sync.log"), lines(41, 45).join("\n") + "\n").unwrap();
    assert_eq!(cli(&b, &mut sys, &["log", "-n", "8"]).1, lines(38, 45));
    assert!(sys.ran.is_empty());
}

// ---------------------------------------------------------------------- inside the Claude app

/// What running the one-off task leaves behind: the answer of the `__path` command.
fn answers(b: &Base, line: &'static str) -> Answer {
    let result = b.home.join(PATH_RESULT);
    Box::new(move |_| fs::write(&result, line).unwrap())
}

/// The one-off task was registered from a definition (so that it starts on battery too),
/// run, and deleted again; the definition file is not left behind.
fn assert_setup_ran(b: &Base, ran: &[String], what: &str) {
    assert_eq!(ran.len(), 3, "{:?}", ran);
    let file = b.home.join("setup-task.xml");
    let create = &ran[0];
    assert!(
        create.starts_with(&format!(
            "schtasks /Create /TN {} /XML {} /F <<",
            SETUP_TASK_NAME,
            file.display()
        )),
        "{}",
        create
    );
    for part in [
        format!(
            "<Command>{}</Command>",
            bin(b).join(WINDOWLESS_EXE).display()
        ),
        format!("<Arguments>__path {}</Arguments>", what),
        String::from("<Triggers />"),
        String::from("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"),
        String::from("<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>"),
        String::from("<ExecutionTimeLimit>PT1M</ExecutionTimeLimit>"),
        String::from("<Hidden>true</Hidden>"),
        String::from(r"<UserId>PC\someone</UserId>"),
    ] {
        assert!(create.contains(&part), "missing {} in {}", part, create);
    }
    assert!(!create.contains("/SC") && !create.contains("/TR"));
    assert_eq!(ran[1], format!("schtasks /Run /TN {}", SETUP_TASK_NAME));
    assert_eq!(
        ran[2],
        format!("schtasks /Delete /TN {} /F", SETUP_TASK_NAME)
    );
    assert!(!file.exists());
}

#[test]
fn one_off_task_has_no_trigger_and_starts_on_battery() {
    let exe = Path::new(r"C:\Users\R&D\.claude-session-sync\bin\claude-session-syncw.exe");
    let xml = setup_task_xml(exe, "__path add", exe.parent(), r"PC\someone");
    for part in [
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>",
        "<Triggers />",
        "<LogonType>InteractiveToken</LogonType>",
        "<RunLevel>LeastPrivilege</RunLevel>",
        "<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>",
        "<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>",
        "<AllowStartOnDemand>true</AllowStartOnDemand>",
        "<Hidden>true</Hidden>",
        "<ExecutionTimeLimit>PT1M</ExecutionTimeLimit>",
        r"<Command>C:\Users\R&amp;D\.claude-session-sync\bin\claude-session-syncw.exe</Command>",
        "<Arguments>__path add</Arguments>",
    ] {
        assert!(xml.contains(part), "missing: {}", part);
    }
    assert!(!xml.contains("Trigger>") && !xml.contains("R&D"));
    assert!(!task_is_disabled(&xml));
}

#[test]
fn inside_the_app_the_path_is_set_through_a_one_off_task() {
    let b = Base::new();
    let mut sys = Recorder::in_the_app();
    sys.path = Some(text(r"C:\Tools", true)); // (what this process sees is the app's private copy)
    let (start, held) = daemon_comes_up(&b, VERSION, 0.0);
    sys.on_start = Some(start);
    sys.on_setup = Some(answers(&b, "added\n"));
    let (result, said) = run_install(&b, &mut sys);
    assert_eq!(result, Ok(()));
    // the registry is not written from here: it would change nothing outside the app
    assert!(sys.path_writes.is_empty() && sys.announced == 0);
    assert_eq!(sys.count("reg "), 0);
    // the daemon's task is registered and started as ever; then the one-off task is
    // registered, run and deleted again
    assert_eq!(sys.ran.len(), 6);
    assert_eq!(sys.ran[0], "schtasks /Query /TN Claude Session Sync /XML"); // (settings to keep?)
    assert!(sys.ran[1].starts_with("schtasks /Create /TN Claude Session Sync /XML"));
    assert_eq!(sys.ran[2], "schtasks /Run /TN Claude Session Sync");
    assert_setup_ran(&b, &sys.ran[3..], "add");
    assert!(sys.setup_task.is_none());
    assert!(!b.home.join(PATH_RESULT).exists()); // its answer was read and cleared away
    assert!(says(&said, "added to your PATH: open a NEW terminal"));
    assert!(says(
        &said,
        "Running inside the Claude app's sandbox: the PATH was set through a one-off task."
    ));
    assert!(says(&said, "  claude-session-sync status"));
    drop(held);
    // it was there already
    let (start, held) = daemon_comes_up(&b, VERSION, 0.0);
    sys.on_start = Some(start);
    sys.on_setup = Some(answers(&b, "present\n"));
    let (result, said) = run_install(&b, &mut sys);
    assert_eq!(result, Ok(()));
    assert!(says(&said, "on your PATH already"));
    drop(held);
}

#[test]
fn one_off_task_that_fails_or_does_not_answer_is_said_and_the_install_stands() {
    // (what the task leaves, or None if it leaves nothing; what the user is told)
    let cases: [(Option<&'static str>, &str); 4] = [
        (None, "did not answer within 15 seconds"),
        (Some("added"), "did not answer within 15 seconds"), // half a line is not an answer yet
        (
            Some("error: access is denied\n"),
            "could not be changed (access is denied)",
        ),
        (
            Some("something else\n"),
            "could not be changed (something else)",
        ),
    ];
    for (left, told) in cases {
        let b = Base::new();
        let mut sys = Recorder::in_the_app();
        let (start, held) = daemon_comes_up(&b, VERSION, 0.0);
        sys.on_start = Some(start);
        sys.on_setup = left.map(|line| answers(&b, line));
        let (result, said) = run_install(&b, &mut sys);
        assert_eq!(result, Ok(()), "{:?}", left); // installed all the same
        assert!(says(&said, "Installed."));
        assert!(says(&said, told), "{:?}: {:?}", left, said);
        assert!(says(
            &said,
            "the PATH has to be set through a one-off task, and that failed"
        ));
        // how to call it without the PATH
        assert!(says(
            &said,
            &format!("\"{}\" status", bin(&b).join(CONSOLE_EXE).display())
        ));
        // the one-off task is not left behind, nor its answer
        assert_setup_ran(&b, &sys.ran[3..], "add");
        assert!(sys.setup_task.is_none() && !b.home.join(PATH_RESULT).exists());
        if told.contains("15 seconds") {
            assert!(sys.slept >= 15.0);
        }
        drop(held);
    }
    // the one-off task cannot even be registered
    let b = Base::new();
    let mut sys = Recorder::in_the_app();
    sys.setup_refused = true;
    let (start, held) = daemon_comes_up(&b, VERSION, 0.0);
    sys.on_start = Some(start);
    let (result, said) = run_install(&b, &mut sys);
    assert_eq!(result, Ok(()));
    assert!(says(
        &said,
        "could not be changed (the one-off task for it could not be registered)"
    ));
    assert_eq!(
        sys.count("schtasks /Run /TN Claude Session Sync (setup)"),
        0
    );
    drop(held);
}

#[test]
fn inside_the_app_a_start_up_entry_is_no_fallback() {
    let b = Base::new();
    let mut sys = Recorder::in_the_app();
    sys.refuse.push(("schtasks", "/Create"));
    let (start, held) = daemon_comes_up(&b, VERSION, 0.0);
    sys.on_start = Some(start);
    let (result, said) = run_install(&b, &mut sys);
    let why = result.unwrap_err();
    assert!(
        why.contains("Task Scheduler refused the task") && why.contains("Nothing was installed")
    );
    assert!(why.contains("from an ordinary terminal"));
    assert!(said.is_empty());
    // no start-up entry, no daemon started by hand, no PATH: it would all be for nothing
    assert_eq!(
        sys.count("reg ") + sys.count("spawn ") + sys.count("__path"),
        0
    );
    assert!(!sys.run_entry && held.borrow().is_none());
    // outside the app the same refusal still falls back to the start-up entry
    let b = Base::new();
    let mut sys = Recorder::new();
    sys.refuse.push(("schtasks", "/Create"));
    let (start, held) = daemon_comes_up(&b, VERSION, 0.0);
    sys.on_start = Some(start);
    assert_eq!(run_install(&b, &mut sys).0, Ok(()));
    assert!(sys.run_entry);
    drop(held);
}

#[test]
fn inside_the_app_uninstall_takes_the_path_entry_out_through_a_one_off_task() {
    let b = Base::new();
    let mut sys = Recorder {
        task: Some(true),
        run_entry: true, // (there, but not to be seen from inside the app)
        ..Recorder::in_the_app()
    };
    sys.path = Some(text(&format!(r"C:\Tools;{}", bin(&b).display()), true)); // the private copy
    fs::create_dir_all(bin(&b)).unwrap(); // as installed
    fs::write(bin(&b).join(CONSOLE_EXE), b"x").unwrap();
    fs::write(bin(&b).join(WINDOWLESS_EXE), b"x").unwrap();
    sys.on_setup = Some(answers(&b, "removed\n"));
    let (result, said) = act(&b, &mut sys, programs(&b), "uninstall");
    assert_eq!(result, Ok(()));
    let want = [
        "schtasks /Query /TN Claude Session Sync",
        "schtasks /Delete /TN Claude Session Sync /F",
    ];
    assert_eq!(sys.ran[..2], want); // the registry is neither asked nor written
    assert_setup_ran(&b, &sys.ran[2..], "remove");
    assert!(sys.path_writes.is_empty() && sys.run_entry);
    assert!(said[1].contains("PATH entry") && said[1].contains("removed"));
    // nothing to take out
    sys.on_setup = Some(answers(&b, "absent\n"));
    let (_, said) = act(&b, &mut sys, programs(&b), "uninstall");
    assert!(!says(&said, "PATH entry"));
    // no answer: said, with what to do by hand
    sys.on_setup = None;
    let (result, said) = act(&b, &mut sys, programs(&b), "uninstall");
    assert_eq!(result, Ok(()));
    assert!(says(&said, "PATH entry") && says(&said, "could NOT be removed"));
    assert!(says(
        &said,
        &format!("take {} out of your PATH", bin(&b).display())
    ));
    // the installed program is gone, and so cannot be sent to do it: said as well
    fs::remove_file(bin(&b).join(WINDOWLESS_EXE)).unwrap();
    let before = sys.ran.len();
    let (_, said) = act(&b, &mut sys, programs(&b), "uninstall");
    assert!(says(&said, "is not there to do it"));
    assert_eq!(
        sys.ran[before..]
            .iter()
            .filter(|c| c.contains("(setup)"))
            .count(),
        0
    );
}

#[test]
fn inside_the_app_a_start_up_entry_is_not_looked_for() {
    let b = Base::new();
    b.rec(A, "s1", r());
    let mut sys = Recorder {
        run_entry: true,
        ..Recorder::in_the_app()
    };
    assert_eq!(autostart(&mut sys, None), Autostart::NoTaskSeen);
    let (code, out) = cli(&b, &mut sys, &["status"]);
    assert_eq!(code, 1);
    assert_eq!(
        out[1],
        "autostart: no scheduled task (a start-up entry, if any, cannot be seen from inside the Claude app)"
    );
    assert_eq!(sys.count("reg "), 0);
    // a task is seen from there as from anywhere
    sys.task = Some(false);
    assert!(cli(&b, &mut sys, &["status"]).1[1].starts_with("autostart: installed, PAUSED"));
    for what in ["pause", "resume"] {
        let (result, _) = act(&b, &mut Recorder::in_the_app(), programs(&b), what);
        assert!(result.unwrap_err().contains("not installed"), "{}", what);
    }
}

/// The user's PATH kept in a registry hive of our own (a file in the test tree).
struct Hive {
    key: crate::winreg::Key,
    announced: usize,
    read_only: bool,
}

impl System for Hive {
    fn run_output(&mut self, _program: &str, _args: &[String]) -> io::Result<(i32, String)> {
        panic!("the __path command runs nothing")
    }
    fn spawn_detached(&mut self, _program: &Path, _args: &[String]) -> io::Result<()> {
        panic!("the __path command starts nothing")
    }
    fn sleep(&mut self, _seconds: f64) {}
    fn is_ours(&mut self, _pid: u32, _started: Option<f64>) -> bool {
        false
    }
    fn terminate(&mut self, _pid: u32) -> bool {
        false
    }
    fn user_path(&mut self) -> io::Result<Option<Text>> {
        crate::winreg::read_text(&self.key, "Path")
    }
    fn set_user_path(&mut self, path: &Text) -> io::Result<()> {
        if self.read_only {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "access is denied",
            ));
        }
        crate::winreg::write_text(&self.key, "Path", path)
    }
    fn environment_changed(&mut self) {
        self.announced += 1;
    }
    fn sandboxed(&mut self, _localappdata: Option<&Path>) -> bool {
        false
    }
}

#[test]
fn hidden_path_command_changes_the_path_and_leaves_its_answer() {
    let b = Base::new();
    fs::create_dir_all(bin(&b)).unwrap();
    let exe = bin(&b).join(WINDOWLESS_EXE); // the installed program the one-off task starts
    let answer = || fs::read_to_string(b.home.join(PATH_RESULT)).unwrap();
    let mut sys = Hive {
        key: crate::winreg::private_hive(&b.tmp.join("hive.dat")).unwrap(),
        announced: 0,
        read_only: false,
    };
    let before = text(r"%USERPROFILE%\bin;C:\Tools", true);
    crate::winreg::write_text(&sys.key, "Path", &before).unwrap();
    // add: at the end, the rest untouched and still of the expanding kind
    assert_eq!(path_command(&mut sys, &exe, true), 0);
    let with = text(
        &format!(r"%USERPROFILE%\bin;C:\Tools;{}", bin(&b).display()),
        true,
    );
    assert_eq!(sys.user_path().unwrap(), Some(with.clone()));
    assert_eq!((answer().as_str(), sys.announced), ("added\n", 1));
    assert_eq!(path_command(&mut sys, &exe, true), 0);
    assert_eq!(sys.user_path().unwrap(), Some(with));
    assert_eq!((answer().as_str(), sys.announced), ("present\n", 1));
    // remove: only that entry
    assert_eq!(path_command(&mut sys, &exe, false), 0);
    assert_eq!(sys.user_path().unwrap(), Some(before.clone()));
    assert_eq!((answer().as_str(), sys.announced), ("removed\n", 2));
    assert_eq!(path_command(&mut sys, &exe, false), 0);
    assert_eq!((answer().as_str(), sys.announced), ("absent\n", 2));
    // it cannot be written: said in the answer, and by the exit code
    sys.read_only = true;
    assert_eq!(path_command(&mut sys, &exe, true), 1);
    assert_eq!(answer(), "error: access is denied\n");
    assert_eq!(sys.user_path().unwrap(), Some(before));
    assert_eq!(listdir(&b.home), vec!["bin", PATH_RESULT]); // nothing else is left there
}

#[test]
fn hidden_path_command_is_understood_and_not_advertised() {
    let parse =
        |args: &[&str]| cli::parse_args(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>());
    assert_eq!(
        parse(&["__path", "add"]).unwrap().command,
        cli::Command::PathSetup { add: true }
    );
    assert_eq!(
        parse(&["__path", "remove"]).unwrap().command,
        cli::Command::PathSetup { add: false }
    );
    for bad in [
        &["__path"][..],
        &["__path", "sideways"],
        &["__path", "add", "more"],
        &["__path", "add", "--days", "3"],
    ] {
        assert!(parse(bad).is_err(), "{:?}", bad);
    }
    assert!(!cli::help().contains("__path"));
}

// ---------------------------------------------------------------------- settings that are kept

fn arguments_registered(sys: &Recorder) -> String {
    let create = sys
        .ran
        .iter()
        .rev()
        .find(|c| c.starts_with("schtasks /Create /TN Claude Session Sync /XML"))
        .unwrap();
    let start = create.find("<Arguments>").unwrap() + "<Arguments>".len();
    let end = create.find("</Arguments>").unwrap();
    create[start..end].to_string()
}

fn install_again(b: &Base, sys: &mut Recorder, what: &str) -> Vec<String> {
    let (start, held) = daemon_comes_up(b, VERSION, 0.0);
    sys.on_start = Some(start);
    let exe = programs(b);
    let (result, said) = act(b, sys, exe, what);
    assert_eq!(result, Ok(()), "{}", what);
    sys.on_start = None;
    drop(held);
    said
}

#[test]
fn install_keeps_the_settings_of_the_install_before() {
    let b = Base::new();
    let mut sys = Recorder::new();
    let given = "install --interval 600 --quick 0 --days 90 --delete-sync";
    let said = install_again(&b, &mut sys, given);
    assert_eq!(
        arguments_registered(&sys),
        "daemon --interval 600 --quick 0 --delete-sync --days 90"
    );
    assert!(says(
        &said,
        "full pass every 600 s, change check off, archived sessions up to 90 days old"
    ));
    assert!(says(&said, "deletions are carried over"));
    assert!(!says(&said, "previous install")); // a first install: nothing to keep
    assert_eq!(
        load_settings(&b.home),
        Some(Settings {
            interval: 600,
            quick: 0,
            delete_sync: true,
            days: 90
        })
    );
    // an update: install again, saying nothing. Everything is as it was.
    let before = sys.ran.len();
    let said = install_again(&b, &mut sys, "install");
    assert_eq!(
        arguments_registered(&sys),
        "daemon --interval 600 --quick 0 --delete-sync --days 90"
    );
    assert!(says(&said, "deletions are carried over"));
    assert!(says(&said, "(kept from the previous install)"));
    assert!(!sys.ran[before..].iter().any(|c| c.contains("/Query"))); // the record is enough
                                                                      // one thing is changed: that one only
    let said = install_again(&b, &mut sys, "install --no-delete-sync");
    assert_eq!(
        arguments_registered(&sys),
        "daemon --interval 600 --quick 0 --days 90"
    );
    assert!(says(&said, "deletions are not carried over"));
    assert!(says(
        &said,
        "(as given now; what was not given is kept from the previous install)"
    ));
    assert_eq!(
        load_settings(&b.home).map(|s| (s.delete_sync, s.days)),
        Some((false, 90))
    );
}

#[test]
fn install_takes_over_the_settings_of_an_install_that_left_no_record() {
    // version 0.1.0 kept its settings in the scheduled task alone
    let b = Base::new();
    let mut sys = Recorder::installed();
    sys.arguments = String::from("daemon --interval 3600 --quick 10 --delete-sync");
    let said = install_again(&b, &mut sys, "install");
    assert_eq!(sys.ran[0], "schtasks /Query /TN Claude Session Sync /XML"); // asked before anything is changed
    assert_eq!(
        arguments_registered(&sys),
        "daemon --interval 3600 --quick 10 --delete-sync"
    );
    assert!(
        says(&said, "deletions are carried over")
            && says(&said, "(kept from the previous install)")
    );
    assert_eq!(load_settings(&b.home).map(|s| s.delete_sync), Some(true)); // on record from now on
                                                                           // the Python version's task, with its own way of writing the same
    let b = Base::new();
    let mut sys = Recorder::installed();
    sys.arguments = String::from(
        "-I \"C:\\Users\\R&D\\sync.py\" --apply --watch --interval 1800 --quick 5 --delete-sync",
    );
    install_again(&b, &mut sys, "install");
    assert_eq!(
        arguments_registered(&sys),
        "daemon --interval 1800 --quick 5 --delete-sync"
    );
    // a task that starts something else is no previous install
    let b = Base::new();
    let mut sys = Recorder::installed();
    sys.arguments = String::from("--frobnicate --interval 5");
    let said = install_again(&b, &mut sys, "install");
    assert_eq!(
        arguments_registered(&sys),
        "daemon --interval 3600 --quick 10"
    );
    assert!(!says(&said, "previous install"));
}

#[test]
fn settings_are_read_from_a_daemons_command_line() {
    let read =
        |a: &str| settings_from_arguments(a).map(|s| (s.interval, s.quick, s.days, s.delete_sync));
    assert_eq!(read("daemon"), Some((3600, 10, 30, false)));
    assert_eq!(
        read("daemon --interval 600 --quick 0 --delete-sync --days 90"),
        Some((600, 0, 90, true))
    );
    assert_eq!(
        read("daemon --interval=120 --days=7 --delete-sync --no-delete-sync"),
        Some((120, 10, 7, false))
    );
    assert_eq!(
        read("daemon --interval soon --quick"),
        Some((3600, 10, 30, false))
    );
    assert_eq!(read("sync --days 5"), None);
    assert_eq!(read(""), None);
    // whatever install registers can be read back
    for s in [
        Settings::default(),
        Settings {
            interval: 600,
            quick: 0,
            delete_sync: true,
            days: 90,
        },
    ] {
        assert_eq!(
            settings_from_arguments(&daemon_arguments(&s)),
            Some(s.clone())
        );
        let exe = Path::new(r"C:\bin\claude-session-syncw.exe");
        let xml = task_xml(
            exe,
            &daemon_arguments(&s),
            exe.parent().unwrap(),
            "PC\\someone",
            "2026-11-11T12:00:00",
            true,
        );
        assert_eq!(settings_from_task(&xml), Some(s));
    }
    assert_eq!(settings_from_task("<Task></Task>"), None);
    // a record that cannot be read is no record
    let b = Base::new();
    assert_eq!(load_settings(&b.home), None);
    fs::create_dir_all(&b.home).unwrap();
    fs::write(b.home.join(SETTINGS_FILE), b"not json").unwrap();
    assert_eq!(load_settings(&b.home), None);
    fs::write(b.home.join(SETTINGS_FILE), b"{\"days\": 45}").unwrap(); // what is missing is the default
    assert_eq!(
        load_settings(&b.home),
        Some(Settings {
            days: 45,
            ..Settings::default()
        })
    );
}
