//! install / uninstall / pause / resume on macOS and Linux, and what `status` says of them.
//! Nothing here touches launchd, systemd or any process: the system's tools are replaced by
//! a recorder that keeps a little state of its own (is the agent loaded, is it disabled, is
//! the unit enabled and running, does a systemd user session answer). Both ways of starting
//! the daemon are tested on either system: which one is used is the recorder's to say.

use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use serde_json::json;

use super::*;
use crate::cli::{self, Console};
use crate::consts::VERSION;
use crate::daemon::{acquire_lock, Lock};
use crate::install::*;

type Start = Box<dyn FnMut()>;
type Held = Rc<RefCell<Option<Lock>>>;

const BOTH: [Manager; 2] = [Manager::Launchd, Manager::Systemd];
const TARGET: &str = "gui/501/io.github.ssgardaf.claude-session-sync";
const UNIT: &str = "claude-session-sync.service";

/// Records what would be run. `refuse` lists beginnings of command lines that fail.
pub(super) struct Recorder {
    pub(super) ran: Vec<String>,
    slept: f64,
    refuse: Vec<&'static str>,
    manager: Manager,
    /// What to do when the daemon is started: stands in for the daemon coming up.
    on_start: Option<Start>,
    /// What to do when the manager ends the daemon.
    on_stop: Option<Start>,
    /// launchd has the agent loaded.
    loaded: bool,
    /// launchd has the agent on record as disabled (a pause).
    disabled: bool,
    /// A user session of systemd answers.
    session: bool,
    /// What systemd says of the unit: None = it knows of no such unit file.
    enabled: Option<bool>,
    /// systemd is running the unit.
    active: bool,
    /// What systemd says of the folders it reads units from.
    unit_path: String,
    ours: Vec<u32>,
    asked: Vec<(u32, Option<f64>)>,
    ended: Vec<u32>,
    /// Is the desktop app running, as far as the command line is to know.
    pub(super) app: bool,
}

impl Recorder {
    fn with(manager: Manager) -> Recorder {
        Recorder {
            ran: Vec::new(),
            slept: 0.0,
            refuse: Vec::new(),
            manager,
            on_start: None,
            on_stop: None,
            loaded: false,
            disabled: false,
            session: true,
            enabled: None,
            active: false,
            // (nothing of the user's own in it: the unit goes to ~/.config/systemd/user)
            unit_path: String::from(
                "/run/user/501/systemd/transient /etc/systemd/user /usr/lib/systemd/user",
            ),
            ours: Vec::new(),
            asked: Vec::new(),
            ended: Vec::new(),
            app: false,
        }
    }

    pub(super) fn new() -> Recorder {
        Recorder::with(Manager::native())
    }

    /// As on a computer where this tool is installed, as far as launchd or systemd say
    /// (the file that goes with it is `registered`'s to make: see below).
    pub(super) fn installed() -> Recorder {
        Recorder {
            loaded: true,
            enabled: Some(true),
            active: true,
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
            .unwrap_or_else(|| panic!("never ran: {}\n{:#?}", what, self.ran))
    }

    /// Everything that was run only asked a question.
    fn only_asked(&self) -> bool {
        self.ran.iter().all(|c| {
            c.starts_with("launchctl print")
                || c.starts_with("systemctl --user show ")
                || c.starts_with("systemctl --user is-")
        })
    }

    fn started(&mut self) {
        if let Some(start) = self.on_start.as_mut() {
            start();
        }
    }

    fn stopped(&mut self) {
        if let Some(stop) = self.on_stop.as_mut() {
            stop();
        }
    }
}

impl System for Recorder {
    fn run_output(&mut self, program: &str, args: &[String]) -> io::Result<(i32, String)> {
        let line = format!("{} {}", program, args.join(" "));
        self.ran.push(line.clone());
        if self.refuse.iter().any(|r| line.starts_with(r)) {
            return Ok((1, String::new()));
        }
        let words: Vec<&str> = args.iter().map(String::as_str).collect();
        let mut out = String::new();
        let code = match (program, words.as_slice()) {
            ("launchctl", ["print", _]) => {
                if self.loaded {
                    0
                } else {
                    113
                }
            }
            ("launchctl", ["print-disabled", _]) => {
                out = format!(
                    "disabled services = {{\n\t\"com.example.other\" => disabled\n\t\"{}\" => {}\n}}\n",
                    AGENT_LABEL,
                    if self.disabled { "disabled" } else { "enabled" }
                );
                0
            }
            ("launchctl", ["bootstrap", _, file]) => {
                assert!(Path::new(file).is_file(), "{}", file);
                if self.disabled || self.loaded {
                    5 // launchd's "Input/output error" for both
                } else {
                    self.loaded = true;
                    self.started();
                    0
                }
            }
            ("launchctl", ["bootout", _]) => {
                if std::mem::take(&mut self.loaded) {
                    self.stopped();
                    0
                } else {
                    3
                }
            }
            ("launchctl", ["enable", _]) => {
                self.disabled = false;
                0
            }
            ("launchctl", ["disable", _]) => {
                self.disabled = true;
                0
            }
            ("launchctl", ["kickstart", _]) if self.loaded => {
                self.started();
                0
            }
            ("launchctl", _) => 1,
            ("systemctl", ["--user", ..]) if !self.session => 1, // "Failed to connect to bus"
            ("systemctl", ["--user", "show", "-p", "UnitPath", "--value"]) => {
                out = format!("{}\n", self.unit_path);
                0
            }
            ("systemctl", ["--user", "daemon-reload" | "reset-failed", ..]) => 0,
            ("systemctl", ["--user", "is-enabled", _]) => match self.enabled {
                Some(true) => {
                    out = String::from("enabled\n");
                    0
                }
                Some(false) => {
                    out = String::from("disabled\n");
                    1
                }
                None => {
                    out = String::from("not-found\n");
                    4
                }
            },
            ("systemctl", ["--user", "is-active", _]) => {
                if self.active {
                    out = String::from("active\n");
                    0
                } else {
                    out = String::from("inactive\n");
                    3
                }
            }
            ("systemctl", ["--user", "enable", "--now", _]) => {
                self.enabled = Some(true);
                self.active = true;
                self.started();
                0
            }
            ("systemctl", ["--user", "disable", rest @ ..]) => {
                // (a unit without its file cannot be disabled; with --now it is not stopped either)
                if self.enabled.is_none() {
                    1
                } else {
                    self.enabled = Some(false);
                    if rest.first() == Some(&"--now") && std::mem::take(&mut self.active) {
                        self.stopped();
                    }
                    0
                }
            }
            ("systemctl", ["--user", "stop", _]) => {
                if std::mem::take(&mut self.active) {
                    self.stopped();
                }
                0
            }
            ("systemctl", _) => 1,
            _ => 0,
        };
        Ok((code, out))
    }

    fn spawn_detached(&mut self, program: &Path, args: &[String]) -> io::Result<()> {
        self.ran
            .push(format!("spawn {} {}", program.display(), args.join(" ")));
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

    fn user_id(&mut self) -> u32 {
        501
    }

    fn manager(&mut self) -> Manager {
        self.manager
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

/// A stand-in for the program, as a download would have it.
fn program(b: &Base) -> PathBuf {
    let dir = b.tmp.join("download");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(CONSOLE_EXE), b"the program").unwrap();
    dir.join(CONSOLE_EXE)
}

fn bin(b: &Base) -> PathBuf {
    b.home.join("bin")
}

fn link(b: &Base) -> PathBuf {
    b.tmp
        .join("user")
        .join(".local")
        .join("bin")
        .join(CONSOLE_EXE)
}

fn service(b: &Base, manager: Manager) -> PathBuf {
    service_file(manager, &b.env()).unwrap()
}

fn wants(b: &Base) -> PathBuf {
    service(b, Manager::Systemd)
        .parent()
        .unwrap()
        .join("default.target.wants")
        .join(UNIT)
}

/// The file an install leaves for the manager, and the manager having taken it up, without
/// an install.
fn registered(b: &Base, manager: Manager) -> Recorder {
    let file = service(b, manager);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, b"as an install left it").unwrap();
    if manager == Manager::Systemd {
        fs::create_dir_all(wants(b).parent().unwrap()).unwrap();
        let _ = fs::remove_file(wants(b));
        std::os::unix::fs::symlink(&file, wants(b)).unwrap();
    }
    Recorder {
        loaded: true,
        enabled: Some(true),
        active: true,
        ..Recorder::with(manager)
    }
}

/// As `registered`, paused.
fn paused(b: &Base, manager: Manager) -> Recorder {
    let _ = registered(b, manager);
    let _ = fs::remove_file(wants(b));
    Recorder {
        disabled: true,
        enabled: Some(false),
        ..Recorder::with(manager)
    }
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
fn act_in(
    env: &Env,
    sys: &mut Recorder,
    exe: PathBuf,
    what: &str,
) -> (Result<(), String>, Vec<String>) {
    let paths = Paths::discover(env).unwrap();
    let mut said = Vec::new();
    let mut say = |l: &str| said.push(l.to_string());
    let mut setup = Setup {
        env,
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

fn act(
    b: &Base,
    sys: &mut Recorder,
    exe: PathBuf,
    what: &str,
) -> (Result<(), String>, Vec<String>) {
    act_in(&b.env(), sys, exe, what)
}

fn says(said: &[String], what: &str) -> bool {
    said.iter().any(|l| l.contains(what))
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().mode() & 0o7777
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

// ---------------------------------------------------------------------- the definitions

#[test]
fn launch_agent_has_what_the_daemon_relies_on() {
    let text = agent_plist(
        Path::new("/Users/someone/.claude-session-sync/bin/claude-session-sync"),
        "daemon --interval 3600 --quick 10 --delete-sync",
        &[],
    );
    assert!(text.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist "));
    assert!(text.ends_with("</dict>\n</plist>\n"));
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join("");
    for needed in [
        "<key>Label</key><string>io.github.ssgardaf.claude-session-sync</string>",
        // the program and each argument by itself: no shell reads this
        "<key>ProgramArguments</key><array><string>/Users/someone/.claude-session-sync/bin/claude-session-sync</string>\
         <string>daemon</string><string>--interval</string><string>3600</string><string>--quick</string>\
         <string>10</string><string>--delete-sync</string></array>",
        "<key>RunAtLoad</key><true/>", // started at login
        "<key>StartInterval</key><integer>300</integer>", // ...and again within five minutes of a stop
        "<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>", // ...at once after a crash
    ] {
        assert!(flat.contains(needed), "{}\n{}", needed, text);
    }
    assert!(!text.contains("EnvironmentVariables")); // nothing is set: nothing is written
    assert!(!text.contains('\r'));
}

#[test]
fn launch_agent_carries_the_overrides_and_escapes_what_xml_would_misread() {
    let text = agent_plist(
        Path::new("/Users/R&D <dev>/bin/claude-session-sync"),
        "daemon --interval 60 --quick 0",
        &[
            (
                "CLAUDE_SESSION_SYNC_HOME",
                PathBuf::from("/Volumes/My \"Disk\"/state"),
            ),
            ("CLAUDE_CONFIG_DIR", PathBuf::from("/Users/someone/it's")),
        ],
    );
    assert!(text.contains("<string>/Users/R&amp;D &lt;dev&gt;/bin/claude-session-sync</string>"));
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join("");
    assert!(
        flat.contains(
            "<key>EnvironmentVariables</key><dict><key>CLAUDE_SESSION_SYNC_HOME</key>\
             <string>/Volumes/My&quot;Disk&quot;/state</string><key>CLAUDE_CONFIG_DIR</key>\
             <string>/Users/someone/it&apos;s</string></dict>"
        ),
        "{}",
        text
    );
}

#[test]
fn systemd_unit_has_what_the_daemon_relies_on() {
    let text = unit_text(
        Path::new("/home/someone/.claude-session-sync/bin/claude-session-sync"),
        "daemon --interval 3600 --quick 10",
        &[],
    );
    let lines: Vec<&str> = text.lines().collect();
    for needed in [
        "[Unit]",
        "[Service]",
        "Type=simple",
        "ExecStart=\"/home/someone/.claude-session-sync/bin/claude-session-sync\" daemon --interval 3600 --quick 10",
        "Restart=always", // started again however it ended (a stop through systemd stays a stop)
        "RestartSec=60",
        "StandardOutput=null", // what it prints is in sync.log already
        "[Install]",
        "WantedBy=default.target", // started with the user's session
    ] {
        assert!(lines.contains(&needed), "{}\n{}", needed, text);
    }
    assert!(!text.contains("Environment="));
    assert!(text.ends_with("WantedBy=default.target\n"));
}

/// What systemd 255 was seen to do with each of these, in a unit that was really started:
/// in the program's path `$` stands for itself (`$$` is not turned back into `$` there) and
/// `%` has to be doubled; in a setting a quote and a backslash take a backslash, `%` is
/// doubled and `$` stands for itself.
#[test]
fn systemd_unit_is_written_so_that_systemd_reads_the_paths_back_as_they_are() {
    let text = unit_text(
        Path::new("/home/some one/100% mine/dollar$x/two$$y/semi;colon/claude-session-sync"),
        "daemon --interval 60 --quick 0",
        &[
            (
                "CLAUDE_SESSION_SYNC_STORE",
                PathBuf::from("/mnt/50% off/$x \"y\" it's back\\slash"),
            ),
            ("XDG_CONFIG_HOME", PathBuf::from("/home/some one/cfg")),
        ],
    );
    assert!(text.contains(
        "ExecStart=\"/home/some one/100%% mine/dollar$x/two$$y/semi;colon/claude-session-sync\" daemon --interval 60 --quick 0\n"
    ));
    assert!(text.contains(
        "Environment=\"CLAUDE_SESSION_SYNC_STORE=/mnt/50%% off/$x \\\"y\\\" it's back\\\\slash\"\n"
    ));
    assert!(text.contains("Environment=\"XDG_CONFIG_HOME=/home/some one/cfg\"\n"));
    // what systemd takes for a program's path, and what it does not, however it is written
    for fine in [
        "/home/someone/bin/claude-session-sync",
        "/home/some one/bin/claude-session-sync",
        "/home/dollar$x/100%/semi;colon/claude-session-sync",
        "/home/\u{e9}t\u{e9}/\u{d55c}/claude-session-sync",
    ] {
        assert_eq!(unit_refusal(Path::new(fine)), None, "{}", fine);
    }
    for odd in [
        "/home/quo\"te/claude-session-sync",
        "/home/apo'strophe/claude-session-sync",
        "/home/back\\slash/claude-session-sync",
        "/home/new\nline/claude-session-sync",
        "/home/tab\there/claude-session-sync",
    ] {
        let why = unit_refusal(Path::new(odd)).unwrap();
        assert!(
            why.contains("Nothing was installed") && why.contains("CLAUDE_SESSION_SYNC_HOME"),
            "{}",
            why
        );
    }
}

#[test]
fn only_the_variables_that_are_set_are_carried_over_with_full_paths() {
    let b = Base::new();
    for manager in BOTH {
        assert!(overrides(&b.env(), manager).is_empty());
    }
    let env = Env {
        home_override: Some(b.tmp.join("state")),
        claude_config_dir: Some(PathBuf::from("relative/claude")),
        xdg_config_home: Some(b.tmp.join("xdg")),
        ..b.env()
    };
    let found = overrides(&env, Manager::Launchd);
    assert_eq!(found.len(), 2); // (where the app's folder is does not hang on XDG_CONFIG_HOME on a Mac)
    assert_eq!(found[0], ("CLAUDE_SESSION_SYNC_HOME", b.tmp.join("state")));
    assert_eq!(found[1].0, "CLAUDE_CONFIG_DIR");
    assert!(found[1].1.is_absolute() && found[1].1.ends_with("relative/claude"));
    // under systemd the daemon would not see the shell's XDG_CONFIG_HOME: it is handed on
    let found = overrides(&env, Manager::Systemd);
    assert_eq!(found.len(), 3);
    assert_eq!(found[2], ("XDG_CONFIG_HOME", b.tmp.join("xdg")));
}

#[test]
fn a_paused_agent_is_told_from_launchds_listing() {
    let listing = |state: &str| {
        format!(
            "disabled services = {{\n\t\"com.example.other\" => disabled\n\t\"{}\" => {}\n}}\n",
            AGENT_LABEL, state
        )
    };
    assert!(agent_is_disabled(&listing("disabled")));
    assert!(agent_is_disabled(&listing("true"))); // as older systems write it
    assert!(!agent_is_disabled(&listing("enabled")));
    assert!(!agent_is_disabled(&listing("false")));
    assert!(!agent_is_disabled(
        "disabled services = {\n\t\"com.example.other\" => disabled\n}\n"
    ));
    // a longer name that ends in ours is another agent
    assert!(!agent_is_disabled(&format!(
        "\t\"x.{}\" => disabled\n",
        AGENT_LABEL
    )));
    assert!(!agent_is_disabled(""));
}

#[test]
fn the_unit_goes_where_the_users_systemd_reads_its_units_from() {
    // as `systemctl --user show -p UnitPath --value` answers
    let usual = "/home/someone/.config/systemd/user.control /run/user/1000/systemd/user.control \
                 /run/user/1000/systemd/transient /run/user/1000/systemd/generator.early \
                 /home/someone/.config/systemd/user /etc/xdg/systemd/user /etc/systemd/user \
                 /run/user/1000/systemd/user /run/systemd/user /home/someone/.local/share/systemd/user \
                 /usr/local/share/systemd/user /usr/share/systemd/user /usr/lib/systemd/user\n";
    assert_eq!(
        unit_dir_in(usual),
        Some(PathBuf::from("/home/someone/.config/systemd/user"))
    );
    // a systemd that was started with another XDG_CONFIG_HOME reads from there
    assert_eq!(
        unit_dir_in(&usual.replace("/home/someone/.config", "/elsewhere/cfg")),
        Some(PathBuf::from("/elsewhere/cfg/systemd/user"))
    );
    assert_eq!(
        unit_dir_in("/run/user/1000/systemd/user /etc/systemd/user"),
        None
    );
    assert_eq!(unit_dir_in(""), None);
}

#[test]
fn how_the_daemon_is_started_is_asked_of_the_system() {
    let b = Base::new();
    let env = b.env();
    for manager in BOTH {
        // nothing installed: asked all the same (the file is not all there is to it)
        let mut sys = Recorder::with(manager);
        assert_eq!(autostart(&mut sys, &env), Autostart::NotInstalled);
        assert!(!sys.ran.is_empty() && sys.only_asked());
        // installed
        let mut sys = registered(&b, manager);
        assert_eq!(autostart(&mut sys, &env), Autostart::Task { paused: false });
        // paused
        let mut sys = paused(&b, manager);
        assert_eq!(autostart(&mut sys, &env), Autostart::Task { paused: true });
        // the file was deleted while the daemon runs: not "not installed"
        let mut sys = registered(&b, manager);
        fs::remove_file(service(&b, manager)).unwrap();
        if manager == Manager::Systemd {
            sys.enabled = None;
        }
        assert_eq!(autostart(&mut sys, &env), Autostart::NoFile);
        assert!(sys.only_asked());
        let _ = fs::remove_file(wants(&b));
    }
    // the agent's file is there, but launchd does not have it and it is not disabled
    let mut sys = registered(&b, Manager::Launchd);
    sys.loaded = false;
    assert_eq!(autostart(&mut sys, &env), Autostart::NotLoaded);
    // the unit was disabled by hand while it runs: not as installed, and not "paused" either
    let mut sys = registered(&b, Manager::Systemd);
    sys.enabled = Some(false);
    assert_eq!(autostart(&mut sys, &env), Autostart::NotLoaded);
    // the unit's file is there, but no systemd answers
    let mut sys = registered(&b, Manager::Systemd);
    sys.session = false;
    assert_eq!(autostart(&mut sys, &env), Autostart::NoManager);
    assert_eq!(sys.ran.len(), 1);
    // no systemd answers and nothing was ever installed
    fs::remove_file(service(&b, Manager::Systemd)).unwrap();
    let mut sys = Recorder::with(Manager::Systemd);
    sys.session = false;
    assert_eq!(autostart(&mut sys, &env), Autostart::NotInstalled);
    // every answer has a line in `status`
    for (found, word) in [
        (Autostart::Task { paused: false }, "installed ("),
        (Autostart::Task { paused: true }, "PAUSED"),
        (Autostart::NotInstalled, "not installed"),
        (Autostart::NotLoaded, "not in effect"),
        (Autostart::NoFile, "its file is missing"),
        (Autostart::NoManager, "no systemd user session"),
        (Autostart::Unknown, "unknown"),
    ] {
        for manager in BOTH {
            let line = autostart_line(found, manager);
            assert!(
                line.starts_with("autostart: ") && line.contains(word),
                "{}",
                line
            );
        }
    }
    assert!(
        autostart_line(Autostart::Task { paused: false }, Manager::Launchd)
            .contains("launch agent io.github.ssgardaf.claude-session-sync")
    );
    assert!(
        autostart_line(Autostart::Task { paused: false }, Manager::Systemd)
            .contains("systemd user unit claude-session-sync.service")
    );
}

// ---------------------------------------------------------------------- install

#[test]
fn install_copies_registers_starts_and_confirms() {
    for manager in BOTH {
        let b = Base::new();
        let mut sys = Recorder::with(manager);
        let (start, held) = daemon_comes_up(&b, VERSION, 0.0);
        sys.on_start = Some(start);
        let env = Env {
            path: Some(format!(
                "/usr/bin:{}/:/bin",
                link(&b).parent().unwrap().display()
            )),
            ..b.env()
        };
        let exe = program(&b);
        let (result, said) = act_in(&env, &mut sys, exe, "install --delete-sync");
        assert_eq!(result, Ok(()), "{:?}", manager);
        // the program, as a program, and the user's alone like everything in that folder
        let installed = bin(&b).join(CONSOLE_EXE);
        assert_eq!(fs::read(&installed).unwrap(), b"the program");
        assert_eq!(mode(&installed), 0o700);
        assert_eq!((mode(&b.home), mode(&bin(&b))), (0o700, 0o700));
        assert_eq!(listdir(&bin(&b)), vec![CONSOLE_EXE]); // nothing is left beside it
                                                          // the definition, where the manager looks for it, readable as the manager wants it
        let definition = fs::read_to_string(service(&b, manager)).unwrap();
        assert_eq!(mode(&service(&b, manager)), 0o644);
        assert!(definition.contains(&installed.display().to_string()));
        match manager {
            Manager::Launchd => {
                assert!(definition
                    .contains("<string>--quick</string>\n    <string>10</string>\n    <string>--delete-sync</string>"));
                assert_eq!(sys.count("launchctl enable"), 0); // nothing was paused: nothing to lift
                assert_eq!(
                    sys.ran[sys.position("launchctl bootstrap")],
                    format!(
                        "launchctl bootstrap gui/501 {}",
                        service(&b, manager).display()
                    )
                );
                assert_eq!(sys.count("bootout"), 0); // there was nothing to unload
            }
            Manager::Systemd => {
                assert!(definition.contains(" daemon --interval 3600 --quick 10 --delete-sync\n"));
                assert!(
                    sys.position("systemctl --user daemon-reload")
                        < sys.position("systemctl --user enable --now claude-session-sync.service")
                );
            }
        }
        assert!(!definition.contains("CLAUDE_") && !definition.contains("XDG_")); // no variable was set
                                                                                  // the link, and the settings for the next install
        assert_eq!(fs::read_link(link(&b)).unwrap(), installed);
        assert_eq!(
            load_settings(&b.home),
            Some(Settings {
                delete_sync: true,
                ..Settings::default()
            })
        );
        assert_eq!(mode(&b.home.join("settings.json")), 0o600);
        // what the user is told: that it worked, what was set up, where things are, what to do next
        assert!(said[0].starts_with("Installed."));
        assert!(says(
            &said,
            &format!("version {} is running now (pid 4242)", VERSION)
        ));
        assert!(says(
            &said,
            &format!("Starts by  {}: at login", manager.what())
        ));
        assert!(says(
            &said,
            "full pass every 3600 s, change check every 10 s, archived sessions up to 30 days old"
        ));
        assert!(says(&said, "deletions are carried over"));
        assert!(says(&said, &format!("Program    {}", installed.display())));
        assert!(says(&said, &format!("Data       {}", b.home.display())));
        assert!(says(
            &said,
            &format!("PATH       linked as {}", link(&b).display())
        ));
        assert!(!says(&said, "not on your PATH") && !says(&said, "Note:"));
        for next in [
            "  claude-session-sync status",
            "  claude-session-sync log",
            "  claude-session-sync pause",
            "  claude-session-sync uninstall",
        ] {
            assert!(says(&said, next), "{}", next);
        }
        assert!(says(&said, "after you restart the app or sign in again"));
        // nothing of Windows' is asked for here
        assert_eq!(sys.count("schtasks") + sys.count("reg "), 0);
        drop(held);
    }
}

#[test]
fn install_writes_the_variables_that_are_set_into_the_definition() {
    for manager in BOTH {
        let b = Base::new();
        let mut sys = Recorder::with(manager);
        let (start, held) = daemon_comes_up(&b, VERSION, 0.0);
        sys.on_start = Some(start);
        // the tool's own folder is named by the variable: the program goes into its bin folder
        let env = Env {
            home_override: Some(b.home.clone()),
            store_override: Some(b.root.clone()),
            userprofile: Some(b.tmp.join("elsewhere")),
            ..b.env()
        };
        let (result, said) = act_in(&env, &mut sys, program(&b), "install");
        assert_eq!(result, Ok(()), "{:?}", manager);
        assert!(bin(&b).join(CONSOLE_EXE).is_file());
        let definition = fs::read_to_string(service_file(manager, &env).unwrap()).unwrap();
        match manager {
            Manager::Launchd => {
                assert!(definition.contains(&format!(
                    "<key>CLAUDE_SESSION_SYNC_HOME</key>\n    <string>{}</string>",
                    b.home.display()
                )));
                assert!(definition.contains(&format!(
                    "<key>CLAUDE_SESSION_SYNC_STORE</key>\n    <string>{}</string>",
                    b.root.display()
                )));
            }
            Manager::Systemd => {
                assert!(definition.contains(&format!(
                    "Environment=\"CLAUDE_SESSION_SYNC_HOME={}\"\n",
                    b.home.display()
                )));
                assert!(definition.contains(&format!(
                    "Environment=\"CLAUDE_SESSION_SYNC_STORE={}\"\n",
                    b.root.display()
                )));
            }
        }
        assert!(!definition.contains("CLAUDE_CONFIG_DIR"));
        // ...and the user is told, and told how to call a program that is not on the PATH
        assert!(says(
            &said,
            "Note: CLAUDE_SESSION_SYNC_HOME, CLAUDE_SESSION_SYNC_STORE are set in this shell"
        ));
        assert!(says(&said, "that folder is not on your PATH"));
        assert!(says(
            &said,
            &format!("  \"{}\" status", bin(&b).join(CONSOLE_EXE).display())
        ));
        drop(held);
    }
}

/// `XDG_CONFIG_HOME` set in the shell says where the app's folder is, not where the user's
/// systemd reads units from: the unit goes where systemd says, and the variable goes into
/// the unit, so that the daemon looks for the store where the command line looked.
#[test]
fn unit_goes_where_systemd_looks_and_carries_the_shells_xdg_config_home() {
    let b = Base::new();
    let shells = b.tmp.join("shell-xdg");
    let env = Env {
        appdata: Some(shells.clone()),
        xdg_config_home: Some(shells.clone()),
        ..b.env()
    };
    let install = |sys: &mut Recorder| {
        let (start, held) = daemon_comes_up(&b, VERSION, 0.0);
        sys.on_start = Some(start);
        let (result, said) = act_in(&env, sys, program(&b), "install");
        assert_eq!(result, Ok(()));
        drop(held);
        said
    };
    // systemd reads from ~/.config (it was not started with the shell's variable)
    let mut sys = Recorder::with(Manager::Systemd);
    let said = install(&mut sys);
    let usual = service(&b, Manager::Systemd);
    let text = fs::read_to_string(&usual).unwrap();
    assert!(text.contains(&format!(
        "Environment=\"XDG_CONFIG_HOME={}\"\n",
        shells.display()
    )));
    assert!(!shells.join("systemd").exists()); // nothing where systemd would never look
    assert!(says(&said, "Note: XDG_CONFIG_HOME is set in this shell."));
    // uninstall finds it there, and says what is true
    let mut sys = registered(&b, Manager::Systemd);
    let (result, said) = act_in(&env, &mut sys, program(&b), "uninstall");
    assert_eq!(result, Ok(()));
    assert!(said[0].ends_with("removed") && !says(&said, "could NOT"));
    assert!(!usual.exists());
    // a systemd that was started with another folder says so, and gets its unit there
    let theirs = b.tmp.join("managers-xdg").join("systemd").join("user");
    let mut sys = Recorder::with(Manager::Systemd);
    sys.unit_path = format!(
        "/run/user/501/systemd/transient {} /etc/systemd/user",
        theirs.display()
    );
    install(&mut sys);
    assert!(theirs.join(UNIT).is_file());
    assert!(!usual.exists());
    assert_eq!(autostart(&mut sys, &env), Autostart::Task { paused: false });
}

#[test]
fn install_says_so_when_the_new_daemon_does_not_come_up() {
    for manager in BOTH {
        let b = Base::new();
        let mut sys = Recorder::with(manager);
        let (result, said) = act(&b, &mut sys, program(&b), "install");
        let why = result.unwrap_err();
        assert!(why.contains("did not come up"));
        assert!(why.contains("status")); // ...and where to look
        assert!(!says(&said, "Installed."));
        assert!(sys.slept >= 20.0); // it waited before giving up
        match manager {
            // the agent stays: launchd tries again at the next login
            Manager::Launchd => assert!(why.contains("sync.log (not written yet)")),
            // a unit that does not start is not left enabled, to be tried once a minute for ever
            Manager::Systemd => {
                assert!(why.contains("systemctl --user status claude-session-sync"));
                assert!(!why.contains("sync.log")); // (there is none to point to)
                assert!(why.contains("taken out again"));
                assert!(!service(&b, manager).exists());
                assert_eq!((sys.enabled, sys.active), (Some(false), false));
                assert!(
                    sys.position("systemctl --user enable --now")
                        < sys.position("systemctl --user disable")
                );
            }
        }
    }
    // an update that does not come up: the unit that was there is put back, not enabled
    let b = Base::new();
    let mut sys = registered(&b, Manager::Systemd);
    assert!(act(&b, &mut sys, program(&b), "install").0.is_err());
    assert_eq!(
        fs::read(service(&b, Manager::Systemd)).unwrap(),
        b"as an install left it"
    );
    assert_eq!((sys.enabled, sys.active), (Some(false), false));
}

#[test]
fn install_does_not_take_an_old_daemon_for_the_new_one() {
    for manager in BOTH {
        let b = Base::new();
        let mut sys = Recorder::with(manager);
        let (start, held) = daemon_comes_up(&b, "0.1.0", 0.0); // some other version holds the lock and reports
        sys.on_start = Some(start);
        assert!(act(&b, &mut sys, program(&b), "install")
            .0
            .unwrap_err()
            .contains("did not come up"));
        drop(held);
        // ...nor one of this version that was started before the install began
        let b = Base::new();
        let mut sys = Recorder::with(manager);
        let (start, held) = daemon_comes_up(&b, VERSION, 3600.0);
        sys.on_start = Some(start);
        assert!(act(&b, &mut sys, program(&b), "install")
            .0
            .unwrap_err()
            .contains("did not come up"));
        drop(held);
    }
}

#[test]
fn install_stops_a_running_daemon_first_and_does_not_copy_onto_itself() {
    for manager in BOTH {
        let b = Base::new();
        // already installed: the program that runs is the one in the bin folder
        fs::create_dir_all(bin(&b)).unwrap();
        let installed = bin(&b).join(CONSOLE_EXE);
        fs::write(&installed, b"installed program").unwrap();
        // the daemon holds the lock and does not answer the request to stop: the manager ends it
        let running: Held = Rc::new(RefCell::new(acquire_lock(&b.home).unwrap()));
        let mut sys = registered(&b, manager);
        let slot = running.clone();
        sys.on_stop = Some(Box::new(move || drop(slot.borrow_mut().take())));
        let (start, held) = daemon_comes_up(&b, VERSION, 0.0);
        sys.on_start = Some(start);
        let (result, _) = act(&b, &mut sys, installed.clone(), "install");
        assert_eq!(result, Ok(()), "{:?}", manager);
        assert!(running.borrow().is_none());
        match manager {
            Manager::Launchd => {
                assert_eq!(sys.count(&format!("launchctl bootout {}", TARGET)), 1);
                assert!(sys.position("launchctl bootout") < sys.position("launchctl bootstrap"));
            }
            Manager::Systemd => {
                assert!(
                    sys.position("systemctl --user stop claude-session-sync.service")
                        < sys.position("systemctl --user enable --now")
                );
            }
        }
        assert!(sys.slept >= 20.0); // it was asked first, and given time
        assert_eq!(fs::read(&installed).unwrap(), b"installed program");
        assert_eq!(listdir(&bin(&b)), vec![CONSOLE_EXE]);
        // the definition is written afresh
        assert_ne!(
            fs::read(service(&b, manager)).unwrap(),
            b"as an install left it"
        );
        drop(held);
    }
}

/// The agent's file was deleted while launchd has the agent loaded: launchd is asked, not
/// the file, so the old agent is let go of before the new one is handed over (handing it
/// over on top of the old one is refused), and the first install works.
#[test]
fn install_lets_go_of_an_agent_that_is_loaded_without_its_file() {
    let b = Base::new();
    let mut sys = Recorder::with(Manager::Launchd);
    sys.loaded = true; // ...and no file
    let (start, held) = daemon_comes_up(&b, VERSION, 0.0);
    sys.on_start = Some(start);
    let (result, said) = act(&b, &mut sys, program(&b), "install");
    assert_eq!(result, Ok(()));
    assert!(sys.position("launchctl bootout") < sys.position("launchctl bootstrap"));
    assert_eq!(sys.count("launchctl bootstrap"), 1); // taken at the first try
    assert!(said[0].starts_with("Installed."));
    drop(held);
    // meanwhile `status` did not call it "not installed", and `pause` did not refuse
    let b = Base::new();
    let mut sys = Recorder::with(Manager::Launchd);
    sys.loaded = true;
    let (_, out) = cli(&b, &mut sys, &["status"]);
    assert!(
        out[1].contains("is active, but its file is missing"),
        "{}",
        out[1]
    );
    let (result, said) = act(&b, &mut sys, program(&b), "pause");
    assert_eq!(result, Ok(()));
    assert!(said[0].starts_with("Paused."));
    assert!(sys.disabled && !sys.loaded);
    // ...but it cannot be resumed without its file, and that is said
    let why = act(&b, &mut sys, program(&b), "resume").0.unwrap_err();
    assert!(why.contains("not installed"), "{}", why);
}

#[test]
fn install_lifts_a_pause_that_was_left_behind() {
    let b = Base::new();
    let mut sys = paused(&b, Manager::Launchd);
    let (start, held) = daemon_comes_up(&b, VERSION, 0.0);
    sys.on_start = Some(start);
    assert_eq!(act(&b, &mut sys, program(&b), "install").0, Ok(()));
    assert!(sys.position("launchctl enable") < sys.position("launchctl bootstrap"));
    assert!(!sys.disabled && sys.loaded);
    drop(held);
}

#[test]
fn install_replaces_the_program_in_one_step() {
    let b = Base::new();
    fs::create_dir_all(bin(&b)).unwrap();
    let installed = bin(&b).join(CONSOLE_EXE);
    fs::write(&installed, b"the version before").unwrap();
    // (a program that is running: its file is open, and stays what it was for whoever has it)
    let mut running = fs::File::open(&installed).unwrap();
    let mut sys = Recorder::with(Manager::Launchd);
    let (start, held) = daemon_comes_up(&b, VERSION, 0.0);
    sys.on_start = Some(start);
    assert_eq!(act(&b, &mut sys, program(&b), "install").0, Ok(()));
    assert_eq!(fs::read(&installed).unwrap(), b"the program");
    let mut still = Vec::new();
    io::Read::read_to_end(&mut running, &mut still).unwrap();
    assert_eq!(still, b"the version before");
    drop(held);
}

/// Where nothing can start the daemon by itself the program is put in place all the same:
/// that is a success with a warning, not a failure, and it is remembered for `status`.
#[test]
fn install_where_no_systemd_answers_puts_the_program_in_place_and_says_how_to_run_it() {
    let b = Base::new();
    let mut sys = Recorder::with(Manager::Systemd);
    sys.session = false;
    let (result, said) = act(&b, &mut sys, program(&b), "install --delete-sync");
    assert_eq!(result, Ok(()));
    assert!(said[0].starts_with("Installed, but NOT to start by itself"));
    assert!(says(&said, "no systemd user session"));
    assert!(says(
        &said,
        " daemon --interval 3600 --quick 10 --delete-sync"
    ));
    assert!(says(&said, " sync"));
    assert!(!says(&said, "is running now"));
    // the program, its link and the settings are there; a unit nobody would read is not
    assert!(bin(&b).join(CONSOLE_EXE).is_file());
    assert_eq!(fs::read_link(link(&b)).unwrap(), bin(&b).join(CONSOLE_EXE));
    assert!(load_settings(&b.home).is_some_and(|s| s.delete_sync));
    assert!(!service(&b, Manager::Systemd).exists());
    assert_eq!(sys.ran, vec!["systemctl --user show -p UnitPath --value"]);
    assert_eq!(sys.slept, 0.0); // nothing was waited for
                                // `status` says so too, and how to run it; `pause` and `resume` have nothing to act on
    let mut sys = Recorder::with(Manager::Systemd);
    sys.session = false;
    let (code, out) = cli(&b, &mut sys, &["status"]);
    assert_eq!(code, 1);
    assert!(out[0].starts_with("NOT RUNNING"));
    assert_eq!(
        out[1],
        "autostart: not available here (no systemd user session); the daemon runs only while you run it: claude-session-sync daemon"
    );
    for what in ["pause", "resume"] {
        let why = act(&b, &mut sys, program(&b), what).0.unwrap_err();
        assert!(why.contains("no systemd user session"), "{}", why);
    }
    // once a session is there, an install is an ordinary one, and the note is gone
    let mut sys = Recorder::with(Manager::Systemd);
    let (start, held) = daemon_comes_up(&b, VERSION, 0.0);
    sys.on_start = Some(start);
    assert_eq!(act(&b, &mut sys, program(&b), "install").0, Ok(()));
    assert!(!b.home.join(NO_AUTOSTART_FILE).exists());
    drop(held);
    // and an uninstall where none is leaves no note behind either
    let b = Base::new();
    let mut sys = Recorder::with(Manager::Systemd);
    sys.session = false;
    assert_eq!(act(&b, &mut sys, program(&b), "install").0, Ok(()));
    assert!(b.home.join(NO_AUTOSTART_FILE).exists());
    assert_eq!(act(&b, &mut sys, program(&b), "uninstall").0, Ok(()));
    assert!(!b.home.join(NO_AUTOSTART_FILE).exists());
    let (_, out) = cli(&b, &mut sys, &["status"]);
    assert!(out[1].starts_with("autostart: not installed"));
}

#[test]
fn install_says_so_when_the_manager_does_not_take_the_definition() {
    let b = Base::new();
    let mut sys = Recorder::with(Manager::Launchd);
    sys.refuse.push("launchctl bootstrap");
    let why = act(&b, &mut sys, program(&b), "install").0.unwrap_err();
    assert!(why.contains("launchctl bootstrap exit code 1"), "{}", why);
    assert!(why.contains("loaded at your next login"));
    assert_eq!(sys.count("launchctl bootstrap"), 8); // (launchd may be busy with the agent before: asked again)
    assert!(service(&b, Manager::Launchd).is_file());
    // systemd: nothing that was refused is left enabled, or left at all
    let b = Base::new();
    let mut sys = Recorder::with(Manager::Systemd);
    sys.refuse.push("systemctl --user enable");
    let why = act(&b, &mut sys, program(&b), "install").0.unwrap_err();
    assert!(
        why.contains("systemctl --user enable --now exit code 1"),
        "{}",
        why
    );
    assert!(why.contains("systemctl --user status claude-session-sync"));
    assert!(why.contains("taken out again"));
    assert!(!service(&b, Manager::Systemd).exists());
    assert_eq!(sys.slept, 0.0); // (no waiting for a daemon that was never started)
}

/// A folder systemd cannot start a program from is said before anything is written: no
/// program, no unit, no link, nothing asked of systemd but where it reads its units.
#[test]
fn install_refuses_up_front_a_path_systemd_cannot_start() {
    for odd in ["quo\"te", "apo'strophe", "back\\slash"] {
        let b = Base::new();
        let state = b.tmp.join(odd).join("state");
        let env = Env {
            home_override: Some(state.clone()),
            ..b.env()
        };
        let before = b.snapshot(&b.tmp);
        let mut sys = Recorder::with(Manager::Systemd);
        let (result, said) = act_in(&env, &mut sys, program(&b), "install");
        let why = result.unwrap_err();
        assert!(why.contains("systemd cannot start a program whose path has"));
        assert!(why.contains("Nothing was installed") && why.contains(odd));
        assert!(said.is_empty() && sys.ran.is_empty());
        let mut after = b.snapshot(&b.tmp);
        after.retain(|path, _| !path.starts_with(b.tmp.join("download")));
        assert_eq!(before, after);
        assert!(!state.exists());
        // launchd takes such a path as it is
        let mut sys = Recorder::with(Manager::Launchd);
        let held: Held = Rc::new(RefCell::new(None));
        let (slot, home) = (held.clone(), state.clone());
        sys.on_start = Some(Box::new(move || {
            *slot.borrow_mut() = acquire_lock(&home).unwrap();
            let st = json!({"pid": 4242, "version": VERSION, "started": now() as i64, "heartbeat": now() as i64});
            fs::write(home.join("status.json"), st.to_string()).unwrap();
        }));
        assert_eq!(
            act_in(&env, &mut sys, program(&b), "install").0,
            Ok(()),
            "{}",
            odd
        );
        drop(held);
    }
    // what systemd does take is written as it takes it
    let b = Base::new();
    let state = b.tmp.join("dollar$x 100%").join("state");
    let env = Env {
        home_override: Some(state.clone()),
        ..b.env()
    };
    let mut sys = Recorder::with(Manager::Systemd);
    let held: Held = Rc::new(RefCell::new(None));
    let (slot, home) = (held.clone(), state.clone());
    sys.on_start = Some(Box::new(move || {
        *slot.borrow_mut() = acquire_lock(&home).unwrap();
        let st = json!({"pid": 4242, "version": VERSION, "started": now() as i64, "heartbeat": now() as i64});
        fs::write(home.join("status.json"), st.to_string()).unwrap();
    }));
    assert_eq!(act_in(&env, &mut sys, program(&b), "install").0, Ok(()));
    let text = fs::read_to_string(service(&b, Manager::Systemd)).unwrap();
    assert!(
        text.contains(&format!(
            "ExecStart=\"{}/dollar$x 100%%/state/bin/claude-session-sync\" daemon",
            b.tmp.display()
        )),
        "{}",
        text
    );
    drop(held);
}

#[test]
fn link_is_made_once_and_nothing_else_is_replaced() {
    let b = Base::new();
    let installed = bin(&b).join(CONSOLE_EXE);
    let install = |b: &Base| {
        let mut sys = Recorder::with(Manager::Launchd);
        let (start, held) = daemon_comes_up(b, VERSION, 0.0);
        sys.on_start = Some(start);
        let (result, said) = act(b, &mut sys, program(b), "install");
        assert_eq!(result, Ok(()));
        drop(held);
        said
    };
    let said = install(&b);
    assert_eq!(fs::read_link(link(&b)).unwrap(), installed);
    assert!(says(&said, "that folder is not on your PATH")); // (no PATH was given here)
                                                             // again: it is there already
    let said = install(&b);
    assert!(says(&said, "linked as"));
    assert_eq!(fs::read_link(link(&b)).unwrap(), installed);
    // a program of that name that is not ours is left alone, and the install stands
    fs::remove_file(link(&b)).unwrap();
    fs::write(link(&b), b"someone else's").unwrap();
    let said = install(&b);
    assert!(says(&said, "PATH       not linked ("));
    assert!(says(&said, "is not a link, and was left as it is"));
    assert_eq!(fs::read(link(&b)).unwrap(), b"someone else's");
    // so is a link that leads elsewhere
    fs::remove_file(link(&b)).unwrap();
    std::os::unix::fs::symlink(b.tmp.join("another-program"), link(&b)).unwrap();
    let said = install(&b);
    assert!(says(
        &said,
        "is a link to something else and was left as it is"
    ));
    assert_eq!(
        fs::read_link(link(&b)).unwrap(),
        b.tmp.join("another-program")
    );
    // ...and uninstall takes away only a link that is ours
    let mut sys = registered(&b, Manager::Launchd);
    let (result, said) = act(&b, &mut sys, program(&b), "uninstall");
    assert_eq!(result, Ok(()));
    assert!(!says(&said, "Link "));
    assert_eq!(
        fs::read_link(link(&b)).unwrap(),
        b.tmp.join("another-program")
    );
}

#[test]
fn folder_on_the_path_is_told_by_its_spelling() {
    let dir = Path::new("/home/someone/.local/bin");
    assert!(on_path(Some("/usr/bin:/home/someone/.local/bin:/bin"), dir));
    assert!(on_path(Some("/home/someone/.local/bin/"), dir));
    assert!(!on_path(Some("/usr/bin:/home/someone/.local/bin2"), dir));
    assert!(!on_path(Some("/usr/bin::/bin"), dir));
    assert!(!on_path(Some(""), dir));
    assert!(!on_path(None, dir));
}

#[test]
fn install_keeps_the_settings_of_the_install_before() {
    for manager in BOTH {
        let b = Base::new();
        let install = |what: &str| {
            let mut sys = Recorder::with(manager);
            let (start, held) = daemon_comes_up(&b, VERSION, 0.0);
            sys.on_start = Some(start);
            let (result, said) = act(&b, &mut sys, program(&b), what);
            assert_eq!(result, Ok(()));
            drop(held);
            (said, fs::read_to_string(service(&b, manager)).unwrap())
        };
        let (said, _) = install("install --interval 600 --quick 0 --days 90 --delete-sync");
        assert!(says(&said, "change check off"));
        assert!(!says(&said, "previous install"));
        // an update: installed again without a word
        let (said, definition) = install("install");
        assert!(says(
            &said,
            "full pass every 600 s, change check off, archived sessions up to 90 days old"
        ));
        assert!(says(&said, "deletions are carried over"));
        assert!(says(&said, "(kept from the previous install)"));
        let arguments = match manager {
            Manager::Launchd => "<string>--interval</string>\n    <string>600</string>\n    <string>--quick</string>\n    <string>0</string>\n    <string>--delete-sync</string>\n    <string>--days</string>\n    <string>90</string>",
            Manager::Systemd => " daemon --interval 600 --quick 0 --delete-sync --days 90\n",
        };
        assert!(definition.contains(arguments), "{}", definition);
        assert_eq!(
            load_settings(&b.home),
            Some(Settings {
                interval: 600,
                quick: 0,
                days: 90,
                delete_sync: true
            })
        );
    }
}

/// What an earlier version left readable by everyone becomes the user's alone at the next
/// install (and when the daemon starts: the same call); what is not this tool's is left.
#[test]
fn an_install_made_by_an_earlier_version_is_tightened() {
    let b = Base::new();
    let open = |path: &Path, as_dir: bool| {
        if as_dir {
            fs::create_dir_all(path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        } else {
            fs::write(path, b"x").unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
        }
    };
    open(&b.home, true);
    let kept = b.home.join("replaced").join("20260101").join("a_b");
    let dirs = [
        b.home.join("backups"),
        b.home.join("quarantine").join("a_b"),
        kept.clone(),
        b.home.join("undone"),
        bin(&b),
    ];
    for dir in &dirs {
        open(dir, true);
    }
    let files = [
        b.home.join("sync.log"),
        b.home.join("journal.jsonl"),
        b.home.join("manifest.json"),
        b.home.join("state.json"),
        b.home.join("status.json"),
        b.home.join("settings.json"),
        b.home.join("backups").join("20260101-000000.zip"),
        b.home.join("quarantine").join("a_b").join("local_x.json.1"),
        kept.join("local_x.json.1"),
    ];
    for file in &files {
        open(file, false);
    }
    let installed = bin(&b).join(CONSOLE_EXE);
    fs::write(&installed, b"the program").unwrap();
    fs::set_permissions(&installed, fs::Permissions::from_mode(0o755)).unwrap();
    // not this tool's: a file of the user's own, and a link out of the folder
    let own = b.home.join("notes of my own.txt");
    open(&own, false);
    let outside = b.tmp.join("outside.txt");
    open(&outside, false);
    std::os::unix::fs::symlink(&outside, b.home.join("backups").join("a link")).unwrap();
    tighten(&b.home);
    assert_eq!(mode(&b.home), 0o700);
    for dir in &dirs {
        assert_eq!(mode(dir), 0o700, "{}", dir.display());
    }
    assert_eq!(mode(&b.home.join("replaced")), 0o700);
    assert_eq!(mode(&b.home.join("replaced").join("20260101")), 0o700);
    for file in &files {
        assert_eq!(mode(file), 0o600, "{}", file.display());
    }
    assert_eq!(mode(&installed), 0o700); // still a program
    assert_eq!((mode(&own), mode(&outside)), (0o644, 0o644));
    // a folder that is not there, or is a link to one, is left be
    tighten(&b.tmp.join("no such folder"));
    let linked = b.tmp.join("linked-home");
    std::os::unix::fs::symlink(b.tmp.join("download"), &linked).unwrap();
    fs::create_dir_all(b.tmp.join("download")).unwrap();
    fs::set_permissions(b.tmp.join("download"), fs::Permissions::from_mode(0o755)).unwrap();
    tighten(&linked);
    assert_eq!(mode(&b.tmp.join("download")), 0o755);
}

// ---------------------------------------------------------------------- uninstall

#[test]
fn uninstall_removes_the_autostart_before_stopping_the_daemon() {
    for manager in BOTH {
        let b = Base::new();
        b.rec(A, "s1", r());
        b.s().run(); // there is data: it must survive
        let kept = b.snapshot(&b.home);
        let mut sys = registered(&b, manager);
        fs::create_dir_all(bin(&b)).unwrap();
        fs::write(bin(&b).join(CONSOLE_EXE), b"x").unwrap();
        fs::create_dir_all(link(&b).parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(bin(&b).join(CONSOLE_EXE), link(&b)).unwrap();
        fs::write(link(&b).with_file_name("something-else"), b"x").unwrap();
        let (result, said) = act(&b, &mut sys, program(&b), "uninstall");
        assert_eq!(result, Ok(()), "{:?}", manager);
        match manager {
            Manager::Launchd => assert_eq!(
                sys.ran,
                vec![
                    format!("launchctl print {}", TARGET),
                    format!("launchctl bootout {}", TARGET),
                    format!("launchctl print {}", TARGET),
                    String::from("launchctl print-disabled gui/501"),
                ]
            ),
            Manager::Systemd => {
                assert!(
                    sys.position("systemctl --user stop claude-session-sync.service")
                        < sys.position("systemctl --user disable claude-session-sync.service")
                );
                assert_eq!(sys.count("systemctl --user daemon-reload"), 1);
                // ...and systemd was asked afterwards whether it is really stopped
                assert_eq!(
                    sys.ran.last().unwrap(),
                    "systemctl --user is-active claude-session-sync.service"
                );
                assert!(fs::symlink_metadata(wants(&b)).is_err());
            }
        }
        match manager {
            Manager::Launchd => assert!(!sys.loaded),
            Manager::Systemd => assert!(!sys.active && sys.enabled == Some(false)),
        }
        assert!(!service(&b, manager).exists());
        // only our link leaves that folder
        assert_eq!(listdir(link(&b).parent().unwrap()), vec!["something-else"]);
        assert!(said[0].contains(&manager.what()[1..]) && said[0].ends_with("removed"));
        assert!(said[1].contains("Link ") && said[1].ends_with("removed"));
        assert!(said[2].contains("Daemon") && said[2].ends_with("not running any more"));
        // what is left, where it is, and how to go on from here
        assert!(says(&said, "Uninstalled. Nothing of yours was deleted."));
        assert!(says(&said, &b.home.display().to_string()));
        assert!(
            says(&said, "uninstall --purge")
                && says(&said, "undo --since")
                && says(&said, " install")
        );
        assert!(says(&said, "quit the Claude app completely first"));
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
        assert!(bin(&b).join(CONSOLE_EXE).exists()); // the program stays, unless purged
    }
}

/// The unit's file was deleted while systemd runs the unit. systemd still has it, would
/// start the daemon again a minute after it was stopped, and its link for the enabled unit
/// is left pointing at nothing: none of that is "was not installed".
#[test]
fn uninstall_takes_a_daemon_away_from_the_manager_whatever_files_are_left() {
    let b = Base::new();
    let mut sys = registered(&b, Manager::Systemd);
    fs::remove_file(service(&b, Manager::Systemd)).unwrap();
    sys.enabled = None;
    let running: Held = Rc::new(RefCell::new(acquire_lock(&b.home).unwrap()));
    let slot = running.clone();
    sys.on_stop = Some(Box::new(move || drop(slot.borrow_mut().take())));
    let (result, said) = act(&b, &mut sys, program(&b), "uninstall");
    assert_eq!(result, Ok(()));
    assert_eq!(
        sys.count("systemctl --user stop claude-session-sync.service"),
        1
    );
    assert!(!sys.active && running.borrow().is_none());
    assert!(fs::symlink_metadata(wants(&b)).is_err()); // the dangling link is gone too
    assert!(said[0].ends_with("removed"), "{}", said[0]);
    assert!(says(&said, "not running any more"));
    // systemd would not let go: that is said, and where to look
    let b = Base::new();
    let mut sys = registered(&b, Manager::Systemd);
    sys.refuse.push("systemctl --user stop");
    let (result, said) = act(&b, &mut sys, program(&b), "uninstall");
    assert_eq!(result, Ok(()));
    assert!(sys.active);
    assert!(
        said[0].contains("could NOT be removed")
            && said[0].contains("systemctl --user status claude-session-sync"),
        "{}",
        said[0]
    );
    // the same for an agent that is loaded without its file
    let b = Base::new();
    let mut sys = Recorder::with(Manager::Launchd);
    sys.loaded = true;
    let (result, said) = act(&b, &mut sys, program(&b), "uninstall");
    assert_eq!(result, Ok(()));
    assert!(!sys.loaded);
    assert!(said[0].ends_with("removed"), "{}", said[0]);
}

#[test]
fn uninstall_of_a_paused_agent_does_not_leave_the_pause_behind() {
    let b = Base::new();
    let mut sys = paused(&b, Manager::Launchd);
    let (result, said) = act(&b, &mut sys, program(&b), "uninstall");
    assert_eq!(result, Ok(()));
    assert!(!sys.disabled); // launchd keeps a pause by name: an agent installed later would stay off
    assert_eq!(sys.count("bootout"), 0);
    assert!(said[0].ends_with("removed"));
    assert!(!service(&b, Manager::Launchd).exists());
}

#[test]
fn uninstall_of_what_is_not_installed_changes_nothing() {
    for manager in BOTH {
        let b = Base::new();
        let mut sys = Recorder::with(manager);
        let before = b.snapshot(&b.tmp);
        let (result, said) = act(&b, &mut sys, program(&b), "uninstall");
        assert_eq!(result, Ok(()));
        assert!(sys.only_asked(), "{:?}", sys.ran);
        assert!(said[0].ends_with("was not installed"), "{}", said[0]);
        // (the folder for the stop flag, and the stand-in for the download, are all that is new)
        let mut after = b.snapshot(&b.tmp);
        after.retain(|path, _| {
            !path.starts_with(b.tmp.join("download")) && !path.starts_with(&b.home)
        });
        assert_eq!(before, after);
    }
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
    let (result, said) = act(&b, &mut sys, program(&b), "uninstall");
    assert_eq!(result, Ok(()));
    assert!(sys.ended.is_empty());
    assert_eq!(sys.asked, vec![(777_777, Some(1_700_000_000.0))]); // asked with the start time it reported
    assert!(says(
        &said,
        "did NOT stop when asked: end it yourself (kill 777777)"
    ));
    // it is that daemon: ended
    let mut sys = Recorder::new();
    sys.ours.push(777_777);
    let (_, said) = act(&b, &mut sys, program(&b), "uninstall");
    assert_eq!(sys.ended, vec![777_777]);
    assert!(says(
        &said,
        "did not stop when asked and was ended (pid 777777)"
    ));
    drop(stuck);
}

#[test]
fn purge_deletes_this_tools_own_files_and_nothing_else() {
    for manager in BOTH {
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
        fs::write(bin(&b).join(format!(".{}.new", CONSOLE_EXE)), b"x").unwrap(); // an install cut short
        fs::write(b.home.join(NO_AUTOSTART_FILE), b"x").unwrap();
        fs::create_dir_all(link(&b).parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(bin(&b).join(CONSOLE_EXE), link(&b)).unwrap();
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
        let mut sys = registered(&b, manager);
        // (the program that is run is the installed one: here it can delete itself)
        let (result, said) = act(&b, &mut sys, bin(&b).join(CONSOLE_EXE), "uninstall --purge");
        assert_eq!(result, Ok(()));
        assert!(!b.home.exists()); // (the folder held nothing but ours, so it went too)
        assert!(!service(&b, manager).exists());
        assert!(fs::symlink_metadata(link(&b)).is_err());
        assert!(says(&said, "Uninstalled and purged"));
        assert!(!says(&said, "Could NOT be deleted"));
        // the app's session lists and the transcripts are not this tool's files
        assert_eq!(store, b.contents());
        assert_eq!(transcripts, b.snapshot(&b.tmp.join("user").join(".claude")));
    }
}

#[test]
fn purge_leaves_what_is_not_this_tools_and_is_refused_while_the_daemon_runs() {
    let b = Base::new();
    b.rec(A, "s1", r());
    b.s().run();
    fs::write(b.home.join("notes of my own.txt"), b"keep me").unwrap();
    fs::create_dir_all(bin(&b)).unwrap();
    fs::write(bin(&b).join(CONSOLE_EXE), b"x").unwrap();
    // a daemon that will not stop: nothing is purged
    let stuck = acquire_lock(&b.home).unwrap().unwrap();
    let mut sys = registered(&b, Manager::Systemd);
    let (result, _) = act(&b, &mut sys, program(&b), "uninstall --purge");
    assert!(result.unwrap_err().contains("nothing was purged"));
    assert!(b.home.join("sync.log").exists() && b.home.join("backups").exists());
    drop(stuck);
    // stopped: everything of ours goes, the folder stays for what is not ours
    let mut sys = Recorder::with(Manager::Systemd);
    let (result, said) = act(&b, &mut sys, program(&b), "uninstall --purge");
    assert_eq!(result, Ok(()));
    assert_eq!(listdir(&b.home), vec!["notes of my own.txt"]);
    assert!(!says(&said, "Could NOT be deleted"));
}

// ---------------------------------------------------------------------- pause, resume

#[test]
fn pause_disables_the_autostart_before_it_stops_the_daemon() {
    for manager in BOTH {
        let b = Base::new();
        let mut sys = registered(&b, manager);
        let (result, said) = act(&b, &mut sys, program(&b), "pause");
        assert_eq!(result, Ok(()), "{:?}", manager);
        match manager {
            Manager::Launchd => {
                assert!(sys.position("launchctl disable") < sys.position("launchctl bootout"));
                assert!(sys.disabled && !sys.loaded);
            }
            Manager::Systemd => {
                assert_eq!(
                    sys.count("systemctl --user disable --now claude-session-sync.service"),
                    1
                );
                assert_eq!((sys.enabled, sys.active), (Some(false), false));
                // ...and systemd was asked afterwards whether it is really stopped
                assert_eq!(
                    sys.ran.last().unwrap(),
                    "systemctl --user is-active claude-session-sync.service"
                );
            }
        }
        assert!(service(&b, manager).is_file()); // the definition stays: resume needs it
        assert!(said[0].starts_with("Paused."));
        assert!(says(&said, "claude-session-sync resume"));
        assert!(!b.home.join("stop.flag").exists()); // asked to stop, and (no daemon here) seen stopped
                                                     // pausing again is harmless
        let (result, said) = act(&b, &mut sys, program(&b), "pause");
        assert_eq!(result, Ok(()));
        assert!(says(&said, "paused already"));
        assert_eq!(
            sys.count("launchctl disable"),
            usize::from(manager == Manager::Launchd)
        );
        // a daemon that will not stop is said, not hidden
        let stuck = acquire_lock(&b.home).unwrap().unwrap();
        fs::write(
            b.home.join("status.json"),
            json!({"pid": 4242, "heartbeat": 1}).to_string(),
        )
        .unwrap();
        let mut sys = registered(&b, manager);
        let (result, _) = act(&b, &mut sys, program(&b), "pause");
        let why = result.unwrap_err();
        assert!(
            why.contains("did not stop") && why.contains("kill 4242"),
            "{}",
            why
        );
        drop(stuck);
    }
}

/// The unit was disabled by hand while systemd runs it. It reads as disabled, but systemd
/// would start the daemon again a minute after it was stopped: "paused already" would be a
/// lie. The unit is stopped through systemd, and only then is it said to be paused.
#[test]
fn pause_does_not_take_a_unit_that_is_disabled_but_running_for_paused() {
    let b = Base::new();
    let mut sys = registered(&b, Manager::Systemd);
    sys.enabled = Some(false);
    let running: Held = Rc::new(RefCell::new(acquire_lock(&b.home).unwrap()));
    let slot = running.clone();
    sys.on_stop = Some(Box::new(move || drop(slot.borrow_mut().take())));
    let (result, said) = act(&b, &mut sys, program(&b), "pause");
    assert_eq!(result, Ok(()));
    assert!(!says(&said, "paused already"));
    assert!(said[0].starts_with("Paused."));
    assert!(!sys.active && running.borrow().is_none());
    // systemd would not stop it: nothing is said to be paused
    let b = Base::new();
    let mut sys = registered(&b, Manager::Systemd);
    sys.enabled = Some(false);
    sys.refuse.push("systemctl --user disable");
    sys.refuse.push("systemctl --user stop");
    let (result, said) = act(&b, &mut sys, program(&b), "pause");
    assert!(result.unwrap_err().contains("could not be disabled"));
    assert!(said.is_empty() && sys.active);
    // a unit that runs without its file cannot be disabled, but is stopped all the same
    let b = Base::new();
    let mut sys = registered(&b, Manager::Systemd);
    fs::remove_file(service(&b, Manager::Systemd)).unwrap();
    sys.enabled = None;
    let (result, said) = act(&b, &mut sys, program(&b), "pause");
    assert_eq!(result, Ok(()));
    assert!(said[0].starts_with("Paused.") && !sys.active);
}

#[test]
fn pause_and_resume_say_so_when_nothing_is_installed() {
    for manager in BOTH {
        let b = Base::new();
        for what in ["pause", "resume"] {
            let mut sys = Recorder::with(manager);
            let (result, said) = act(&b, &mut sys, program(&b), what);
            assert!(result.unwrap_err().contains("not installed"), "{}", what);
            assert!(said.is_empty() && sys.only_asked());
        }
        let mut sys = registered(&b, manager);
        sys.refuse.push(match manager {
            Manager::Launchd => "launchctl disable",
            Manager::Systemd => "systemctl --user disable",
        });
        assert!(act(&b, &mut sys, program(&b), "pause")
            .0
            .unwrap_err()
            .contains("could not be disabled"));
        assert!(!b.home.join("stop.flag").exists()); // nothing else was changed: the daemon was not stopped either
        assert_eq!(sys.count("bootout"), 0);
    }
    // where no systemd answers there is nothing to pause or resume
    let b = Base::new();
    for what in ["pause", "resume"] {
        let mut sys = registered(&b, Manager::Systemd);
        sys.session = false;
        let why = act(&b, &mut sys, program(&b), what).0.unwrap_err();
        assert!(why.contains("no systemd user session"), "{}", why);
    }
}

#[test]
fn resume_enables_the_autostart_and_starts_the_daemon() {
    for manager in BOTH {
        let b = Base::new();
        let mut sys = paused(&b, manager);
        let (start, held) = daemon_comes_up(&b, VERSION, 0.0);
        sys.on_start = Some(start);
        let (result, said) = act(&b, &mut sys, program(&b), "resume");
        assert_eq!(result, Ok(()), "{:?}", manager);
        match manager {
            Manager::Launchd => {
                assert!(sys.position("launchctl enable") < sys.position("launchctl bootstrap"));
                assert!(!sys.disabled && sys.loaded);
            }
            Manager::Systemd => {
                assert_eq!(
                    sys.ran.last().unwrap(),
                    "systemctl --user enable --now claude-session-sync.service"
                );
                assert_eq!((sys.enabled, sys.active), (Some(true), true));
            }
        }
        assert_eq!(
            said,
            vec!["Resumed. The daemon is running again (pid 4242)."]
        );
        // resumed already, and running: said, and nothing more is done
        let (result, said) = act(&b, &mut sys, program(&b), "resume");
        assert_eq!(result, Ok(()));
        assert_eq!(said, vec!["Not paused: the daemon is running."]);
        assert_eq!(sys.count(" enable "), 1);
        drop(held);
        // enabled, but nothing came up: said so
        let b = Base::new();
        let mut sys = paused(&b, manager);
        let (result, _) = act(&b, &mut sys, program(&b), "resume");
        assert!(result.unwrap_err().contains("has not come up yet"));
        match manager {
            Manager::Launchd => assert!(!sys.disabled),
            Manager::Systemd => assert_eq!(sys.enabled, Some(true)),
        }
    }
}

#[test]
fn resume_starts_a_daemon_that_was_only_stopped() {
    // loaded and not paused, but the daemon is not running (`stop`): it is started now, not at
    // launchd's next interval
    let b = Base::new();
    let mut sys = registered(&b, Manager::Launchd);
    let (start, held) = daemon_comes_up(&b, VERSION, 0.0);
    sys.on_start = Some(start);
    let (result, said) = act(&b, &mut sys, program(&b), "resume");
    assert_eq!(result, Ok(()));
    assert_eq!(
        sys.ran.last().unwrap(),
        &format!("launchctl kickstart {}", TARGET)
    );
    assert!(said[0].starts_with("Resumed."));
    drop(held);
}

// ---------------------------------------------------------------------- status, log, the rest

#[test]
fn status_begins_with_a_one_line_summary() {
    for manager in BOTH {
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
        let mut sys = Recorder::with(manager);
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
        assert!(sys.only_asked()); // questions were asked of the system, nothing was changed
                                   // installed and running
        let held = acquire_lock(&b.home).unwrap().unwrap();
        let st = json!({"pid": 4242, "version": VERSION, "started": now() as i64, "heartbeat": now() as i64});
        fs::write(b.home.join("status.json"), st.to_string()).unwrap();
        let mut sys = registered(&b, manager);
        let (code, out) = cli(&b, &mut sys, &["status"]);
        assert_eq!(code, 0);
        assert_eq!(
            out[0],
            format!(
                "OK  daemon {} running | 2 of 3 sessions in every account | 1 conflict | 0 errors",
                VERSION
            )
        );
        assert_eq!(out[1], format!("autostart: installed ({})", manager.what()));
        assert!(out[3].starts_with("daemon: running (pid 4242"));
        assert!(sys.only_asked());
        drop(held);
        // paused
        let mut sys = paused(&b, manager);
        let (code, out) = cli(&b, &mut sys, &["status"]);
        assert_eq!(code, 1);
        assert!(
            out[0].starts_with(
                "PAUSED  daemon stopped until you run: claude-session-sync resume | 2 of 3"
            ),
            "{}",
            out[0]
        );
        assert!(out[1].starts_with("autostart: installed, PAUSED"));
        assert!(sys.only_asked());
    }
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
    fs::write(b.home.join("sync.log"), lines(1, 40).join("\n") + "\n").unwrap();
    assert_eq!(cli(&b, &mut sys, &["log"]).1, lines(11, 40)); // the last 30
    assert_eq!(cli(&b, &mut sys, &["log", "-n", "3"]).1, lines(38, 40));
    assert!(cli(&b, &mut sys, &["log", "-n", "0"]).1.is_empty());
    assert!(sys.ran.is_empty());
}

#[test]
fn what_is_printed_speaks_of_this_system() {
    let text = cli::help();
    for windows_word in [
        "%USERPROFILE%",
        "%APPDATA%",
        "this PC",
        "logon",
        "Microsoft",
        "\\",
        "tray",
        ".exe",
    ] {
        assert!(!text.contains(windows_word), "{}", windows_word);
    }
    for word in [
        "~/.claude-session-sync/",
        "~/.local/bin/claude-session-sync",
        "~/.claude/projects/",
        "claude-code-sessions/",
        "at login",
    ] {
        assert!(text.contains(word), "{}", word);
    }
    if cfg!(target_os = "macos") {
        assert!(text.contains("~/Library/Application Support/Claude/claude-code-sessions/"));
        assert!(
            text.contains("~/Library/LaunchAgents/io.github.ssgardaf.claude-session-sync.plist")
        );
    } else {
        assert!(text.contains("~/.config/Claude/claude-code-sessions/"));
        assert!(text.contains("~/.config/systemd/user/claude-session-sync.service"));
    }
    // while the app runs, undo says how to close it here
    let b = Base::new();
    signed_out(&b);
    b.rec(A, "s1", r());
    b.rec(B, "own", r());
    let mut sys = Recorder::new();
    assert_eq!(cli(&b, &mut sys, &["sync"]).0, 0);
    sys.app = true;
    let (code, out) = cli(&b, &mut sys, &["undo", "--since", "2000-01-01"]);
    assert_eq!(code, 1);
    assert!(out.iter().any(|l| l.contains("Quit the app completely")));
    assert!(!out.iter().any(|l| l.contains("tray")));
    // where the store is not found, the places named are this system's
    let b = Base::new();
    fs::remove_dir_all(&b.root).unwrap();
    let (_, out) = cli(&b, &mut sys, &["preview"]);
    assert!(out[0].starts_with("The Claude app's session store was not found"));
    assert!(!out.iter().any(|l| l.contains("this PC")));
}

#[test]
fn hidden_path_command_is_not_for_these_systems() {
    let b = Base::new();
    let mut sys = Recorder::new();
    assert_eq!(cli(&b, &mut sys, &["__path", "add"]).0, 1);
    assert!(sys.ran.is_empty());
    assert!(!b.home.exists());
}

#[test]
fn where_things_are_follows_the_systems_own_variables() {
    let home = Path::new("/home/someone");
    if cfg!(target_os = "macos") {
        assert_eq!(
            Env::app_folders(Some(home), Some(PathBuf::from("/elsewhere"))),
            Some(home.join("Library").join("Application Support"))
        );
        assert_eq!(Env::app_folders(None, None), None);
    } else {
        assert_eq!(
            Env::app_folders(Some(home), None),
            Some(home.join(".config"))
        );
        assert_eq!(
            Env::app_folders(Some(home), Some(PathBuf::from("/elsewhere/config"))),
            Some(PathBuf::from("/elsewhere/config"))
        );
        // (a relative XDG_CONFIG_HOME is not valid, and is passed over)
        assert_eq!(
            Env::app_folders(Some(home), Some(PathBuf::from("relative"))),
            Some(home.join(".config"))
        );
        assert_eq!(Env::app_folders(None, None), None);
    }
    // the app's log on macOS: ~/Library/Logs/Claude/main.log, beside the places looked in everywhere
    let b = Base::new();
    let env = Env {
        library_logs: Some(b.tmp.join("user").join("Library").join("Logs")),
        ..b.env()
    };
    let mut log = crate::applog::AppLog::new(Paths::discover(&env).unwrap().logs);
    let mac = b
        .tmp
        .join("user")
        .join("Library")
        .join("Logs")
        .join("Claude")
        .join("main.log");
    assert_eq!(log.candidates().last(), Some(&mac));
    fs::create_dir_all(mac.parent().unwrap()).unwrap();
    // (as the app writes it there: local time, line feeds only)
    fs::write(
        &mac,
        format!(
            "2026-11-11 09:00:00 [info] [LocalSessionManager] Initialization succeeded \u{2014} accountId={}, orgId={}, existingSessions=3\n\
             2026-11-11 09:00:01 [info] something else\n",
            A.0, A.1
        ),
    )
    .unwrap();
    assert_eq!(log.current().unwrap().0, mac);
    assert_eq!(log.state(), Signed::In(la()));
}
