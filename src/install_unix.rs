//! Autostart and upkeep on macOS and Linux.
//!
//! macOS: a launch agent in `~/Library/LaunchAgents`. launchd starts the daemon at login,
//! starts it again at once if it ends with an error, and within five minutes if it ended
//! without one (`stop`).
//!
//! Linux: a systemd user unit in `~/.config/systemd/user`. systemd starts the daemon at
//! login and starts it again a minute after it ends, however it ended. Where there is no
//! systemd user session, nothing can start the daemon by itself: `install` says so and puts
//! only the program in place.
//!
//! Either way the program is copied to the `bin` folder of this tool's own folder and linked
//! as `~/.local/bin/claude-session-sync`; no shell profile is touched. The three variables
//! that say where things are (`CLAUDE_SESSION_SYNC_HOME`, `CLAUDE_SESSION_SYNC_STORE`,
//! `CLAUDE_CONFIG_DIR`) are written into the agent or the unit if they are set when
//! `install` runs, so that the daemon looks where the command line looked.
//!
//! As on Windows, a daemon is only ever ended after it was asked to stop, and what starts it
//! is taken away before it is stopped, so that nothing brings it back meanwhile.
//!
//! Neither launchd nor systemd starts a second copy beside a daemon that is still there, so
//! nothing from outside replaces one that hangs, as the scheduled task does on Windows. The
//! daemon ends itself then (see `watchdog`), with an exit code that has both start a fresh one.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{
    daemon_arguments, daemon_running, load_settings, save_settings, status_file, strs, xml_escape,
    Autostart, Given, Settings, Setup, System, CONSOLE_EXE,
};
use crate::consts::VERSION;
use crate::paths::{Env, HOME_VAR};
use crate::winfs::{self, Expect};

/// The name launchd knows the daemon by.
pub const AGENT_LABEL: &str = "io.github.ssgardaf.claude-session-sync";
/// The name systemd knows the daemon by.
pub const UNIT_NAME: &str = "claude-session-sync.service";

/// What starts the daemon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Manager {
    Launchd,
    Systemd,
}

impl Manager {
    /// The one this system has.
    pub fn native() -> Manager {
        if cfg!(target_os = "macos") {
            Manager::Launchd
        } else {
            Manager::Systemd
        }
    }

    /// What the daemon is to it, by name.
    pub fn what(self) -> String {
        match self {
            Manager::Launchd => format!("launch agent {}", AGENT_LABEL),
            Manager::Systemd => format!("systemd user unit {}", UNIT_NAME),
        }
    }

    /// The program it is asked through.
    fn tool(self) -> &'static str {
        match self {
            Manager::Launchd => "launchctl",
            Manager::Systemd => "systemctl",
        }
    }
}

/// The file that tells the manager about the daemon.
pub fn service_file(manager: Manager, env: &Env) -> Option<PathBuf> {
    match manager {
        Manager::Launchd => env.userprofile.as_ref().map(|home| {
            home.join("Library")
                .join("LaunchAgents")
                .join(format!("{}.plist", AGENT_LABEL))
        }),
        Manager::Systemd => env
            .appdata
            .as_ref()
            .map(|config| config.join("systemd").join("user").join(UNIT_NAME)),
    }
}

/// Where the program is linked so that it can be called by its name.
pub fn link_file(env: &Env) -> Option<PathBuf> {
    env.userprofile
        .as_ref()
        .map(|home| home.join(".local").join("bin").join(CONSOLE_EXE))
}

/// The variables that say where things are, as far as they are set, with full paths.
pub fn overrides(env: &Env) -> Vec<(&'static str, PathBuf)> {
    let full = |p: &PathBuf| std::path::absolute(p).unwrap_or_else(|_| p.clone());
    [
        ("CLAUDE_SESSION_SYNC_HOME", &env.home_override),
        ("CLAUDE_SESSION_SYNC_STORE", &env.store_override),
        ("CLAUDE_CONFIG_DIR", &env.claude_config_dir),
    ]
    .into_iter()
    .filter_map(|(name, value)| value.as_ref().map(|v| (name, full(v))))
    .collect()
}

/// The launch agent's definition. The daemon is started when the agent is loaded (at login),
/// started again if it ends with an error, and started every five minutes if it is not
/// running (launchd never starts a second copy).
pub fn agent_plist(program: &Path, arguments: &str, environment: &[(&str, PathBuf)]) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut add = |line: &str| lines.push(line.to_string());
    add("<?xml version=\"1.0\" encoding=\"UTF-8\"?>");
    add("<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">");
    add("<plist version=\"1.0\">");
    add("<dict>");
    add("  <key>Label</key>");
    add(&format!("  <string>{}</string>", AGENT_LABEL));
    add("  <key>ProgramArguments</key>");
    add("  <array>");
    add(&format!(
        "    <string>{}</string>",
        xml_escape(&program.to_string_lossy())
    ));
    for argument in arguments.split_whitespace() {
        add(&format!("    <string>{}</string>", xml_escape(argument)));
    }
    add("  </array>");
    if !environment.is_empty() {
        add("  <key>EnvironmentVariables</key>");
        add("  <dict>");
        for (name, value) in environment {
            add(&format!("    <key>{}</key>", xml_escape(name)));
            add(&format!(
                "    <string>{}</string>",
                xml_escape(&value.to_string_lossy())
            ));
        }
        add("  </dict>");
    }
    add("  <key>RunAtLoad</key>");
    add("  <true/>");
    add("  <key>StartInterval</key>");
    add("  <integer>300</integer>");
    add("  <key>KeepAlive</key>");
    add("  <dict>");
    add("    <key>SuccessfulExit</key>");
    add("    <false/>");
    add("  </dict>");
    add("  <key>ProcessType</key>");
    add("  <string>Background</string>");
    add("</dict>");
    add("</plist>");
    lines.join("\n") + "\n"
}

/// A word for a unit file, in quotes, so that systemd reads it back as it is.
fn unit_word(text: &str, dollar: bool) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            '$' if dollar => out.push_str("$$"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// The systemd user unit. The daemon is started with the user's session and started again a
/// minute after it ends, however it ended (a daemon that was stopped through systemd stays
/// stopped). What it prints is in its own log already.
pub fn unit_text(program: &Path, arguments: &str, environment: &[(&str, PathBuf)]) -> String {
    let mut lines: Vec<String> = vec![
        String::from("[Unit]"),
        String::from(
            "Description=claude-session-sync: keeps the Claude desktop app's session list the same under every account",
        ),
        String::from("Documentation=https://github.com/ssgardaf/claude-session-sync"),
        String::new(),
        String::from("[Service]"),
        String::from("Type=simple"),
        format!(
            "ExecStart={} {}",
            unit_word(&program.to_string_lossy(), true),
            arguments
        ),
    ];
    for (name, value) in environment {
        lines.push(format!(
            "Environment={}",
            unit_word(&format!("{}={}", name, value.to_string_lossy()), false)
        ));
    }
    lines.extend(
        [
            "Restart=always",
            "RestartSec=60",
            "StandardOutput=null",
            "",
            "[Install]",
            "WantedBy=default.target",
        ]
        .map(String::from),
    );
    lines.join("\n") + "\n"
}

/// Does `launchctl print-disabled` list the agent as disabled? (Older systems write `true`
/// where newer ones write `disabled`.)
pub fn agent_is_disabled(listing: &str) -> bool {
    let name = format!("\"{}\"", AGENT_LABEL);
    listing.lines().any(|line| {
        let Some((label, state)) = line.split_once("=>") else {
            return false;
        };
        label.trim() == name && matches!(state.trim(), "disabled" | "true")
    })
}

fn agent_domain(sys: &mut dyn System) -> String {
    format!("gui/{}", sys.user_id())
}

fn agent_target(sys: &mut dyn System) -> String {
    format!("gui/{}/{}", sys.user_id(), AGENT_LABEL)
}

fn agent_loaded(sys: &mut dyn System) -> Option<bool> {
    let target = agent_target(sys);
    sys.run("launchctl", &strs(&["print", &target]))
        .ok()
        .map(|code| code == 0)
}

fn agent_disabled(sys: &mut dyn System) -> bool {
    let domain = agent_domain(sys);
    sys.run_output("launchctl", &strs(&["print-disabled", &domain]))
        .is_ok_and(|(_, listing)| agent_is_disabled(&listing))
}

/// Ask the system how the daemon is started. Where the agent's or the unit's file is not
/// there, nothing is asked: it is not installed.
pub fn autostart(sys: &mut dyn System, env: &Env) -> Autostart {
    let manager = sys.manager();
    if !service_file(manager, env).is_some_and(|f| f.is_file()) {
        return Autostart::NotInstalled;
    }
    match manager {
        Manager::Launchd => match agent_loaded(sys) {
            Some(true) => Autostart::Task { paused: false },
            Some(false) if agent_disabled(sys) => Autostart::Task { paused: true },
            Some(false) => Autostart::NotLoaded,
            None => Autostart::Unknown,
        },
        Manager::Systemd => {
            match sys.run_output("systemctl", &strs(&["--user", "is-enabled", UNIT_NAME])) {
                Ok((0, _)) => Autostart::Task { paused: false },
                Ok((_, said)) => match said.trim() {
                    "disabled" => Autostart::Task { paused: true },
                    // nothing was said on standard output: the user's systemd did not answer
                    "" => Autostart::NoManager,
                    _ => Autostart::NotLoaded,
                },
                Err(_) => Autostart::NoManager,
            }
        }
    }
}

/// Ask the system how the daemon is started.
pub fn autostart_for(sys: &mut dyn System, env: &Env) -> Autostart {
    autostart(sys, env)
}

/// The second line of `status`.
pub fn autostart_line(found: Autostart, manager: Manager) -> String {
    let what = manager.what();
    match found {
        Autostart::Task { paused: false } => format!("autostart: installed ({})", what),
        Autostart::Task { paused: true } => format!(
            "autostart: installed, PAUSED (the {} is disabled; resume enables it)",
            what
        ),
        Autostart::NotLoaded => format!(
            "autostart: installed, but the {} is not active (run install again)",
            what
        ),
        Autostart::NoManager => String::from(
            "autostart: none (no systemd user session here; the daemon runs only if you start it: claude-session-sync daemon)",
        ),
        Autostart::Unknown => format!("autostart: unknown ({} could not be asked)", manager.tool()),
        Autostart::NotInstalled | Autostart::StartupEntry | Autostart::NoTaskSeen => {
            String::from("autostart: not installed (to install: claude-session-sync install)")
        }
    }
}

/// Is a folder named in a PATH?
pub fn on_path(path: Option<&str>, dir: &Path) -> bool {
    path.is_some_and(|path| {
        path.split(':')
            .any(|entry| !entry.is_empty() && Path::new(entry.trim_end_matches('/')) == dir)
    })
}

fn same_file(a: &Path, b: &Path) -> bool {
    matches!((fs::canonicalize(a), fs::canonicalize(b)), (Ok(x), Ok(y)) if x == y)
}

/// A path that can be written into an agent or a unit: text without line breaks.
fn writable(path: &Path) -> Result<(), String> {
    match path.to_str() {
        Some(text) if !text.chars().any(char::is_control) => Ok(()),
        _ => Err(format!(
            "{} cannot be written into the daemon's start-up definition (it is not plain text)",
            path.display()
        )),
    }
}

impl Setup<'_> {
    /// Link the program into `~/.local/bin`. True if the link was made, false if it was
    /// there already. Whatever else has that name is left as it is.
    fn link(&mut self, program: &Path) -> Result<bool, String> {
        let link = link_file(self.env).ok_or(format!("{} is not set", HOME_VAR))?;
        match fs::symlink_metadata(&link) {
            Ok(found) if found.file_type().is_symlink() => {
                if fs::read_link(&link).is_ok_and(|to| to == program) || same_file(&link, program) {
                    Ok(false)
                } else {
                    Err(format!(
                        "{} is a link to something else and was left as it is",
                        link.display()
                    ))
                }
            }
            Ok(_) => Err(format!(
                "{} is there already, is not a link, and was left as it is",
                link.display()
            )),
            Err(_) => {
                if let Some(dir) = link.parent() {
                    fs::create_dir_all(dir)
                        .map_err(|e| format!("cannot create {}: {}", dir.display(), e))?;
                }
                std::os::unix::fs::symlink(program, &link)
                    .map_err(|e| format!("cannot create {}: {}", link.display(), e))?;
                Ok(true)
            }
        }
    }

    /// Take the link out of `~/.local/bin` again, if it is ours: a link to the installed
    /// program. True if it was removed, false if there was none of ours.
    fn unlink(&mut self, program: &Path) -> Result<bool, String> {
        let Some(link) = link_file(self.env) else {
            return Ok(false);
        };
        let ours = fs::symlink_metadata(&link).is_ok_and(|f| f.file_type().is_symlink())
            && (fs::read_link(&link).is_ok_and(|to| to == program) || same_file(&link, program));
        if !ours {
            return Ok(false);
        }
        fs::remove_file(&link)
            .map(|()| true)
            .map_err(|e| format!("{}: {}", link.display(), e))
    }

    /// Have launchd load the agent. It may still be busy letting go of the one before, so
    /// it is asked a few times. The exit code of the last try.
    fn load_agent(&mut self, file: &Path) -> i32 {
        let domain = agent_domain(self.sys);
        let args = [
            String::from("bootstrap"),
            domain,
            file.to_string_lossy().into_owned(),
        ];
        let mut code = -1;
        for attempt in 0..8 {
            if attempt > 0 {
                self.sys.sleep(0.5);
            }
            code = self.sys.run("launchctl", &args).unwrap_or(-1);
            if code == 0 {
                break;
            }
        }
        code
    }

    /// Is there a user session of systemd to hand the unit to?
    fn systemd_answers(&mut self) -> bool {
        self.sys
            .run("systemctl", &strs(&["--user", "show-environment"]))
            .is_ok_and(|code| code == 0)
    }

    /// The pid of the daemon on record, for telling the user what to end.
    fn pid_on_record(&self) -> String {
        status_file(&self.paths.home)
            .and_then(|st| st.get("pid").and_then(Value::as_u64))
            .map_or_else(|| String::from("<its pid>"), |pid| pid.to_string())
    }

    /// Register the daemon so it starts by itself and is started again if it ever stops, and
    /// link the program into `~/.local/bin`. Safe to run again after an update or to change
    /// the settings: it stops the running daemon first.
    pub fn install(&mut self, given: &Given) -> Result<(), String> {
        let began = winfs::real_now();
        let manager = self.sys.manager();
        let home = self.paths.home.clone();
        let bin = home.join("bin");
        let program = bin.join(CONSOLE_EXE);
        let service = service_file(manager, self.env).ok_or(format!("{} is not set", HOME_VAR))?;
        if !self.current_exe.is_file() {
            return Err(format!(
                "cannot tell where this program is ({} is not a file)",
                self.current_exe.display()
            ));
        }
        let environment = overrides(self.env);
        writable(&program)?;
        for (_, value) in &environment {
            writable(value)?;
        }

        // The settings: what is given now, over what the previous install was given.
        let previous = load_settings(&home);
        let base = previous.clone().unwrap_or_default();
        let settings = &Settings {
            interval: given.interval.unwrap_or(base.interval).max(60),
            quick: given.quick.unwrap_or(base.quick).max(0),
            days: given.days.unwrap_or(base.days),
            delete_sync: given.delete_sync.unwrap_or(base.delete_sync),
        };
        let arguments = daemon_arguments(settings);

        // Without a user session of systemd nothing here can start the daemon by itself.
        let managed = manager == Manager::Launchd || self.systemd_answers();

        // A daemon that is already running keeps the old program and settings. Ask it to finish
        // what it is doing and exit; then take it out of the manager's hands, which also ends
        // one that did not answer, and keeps it from being started again meanwhile.
        let stopped = self.stop().map_err(|e| e.to_string())?;
        if service.is_file() {
            match manager {
                Manager::Launchd => {
                    let target = agent_target(self.sys);
                    let _ = self.sys.run("launchctl", &strs(&["bootout", &target]));
                }
                Manager::Systemd if managed => {
                    let _ = self
                        .sys
                        .run("systemctl", &strs(&["--user", "stop", UNIT_NAME]));
                }
                Manager::Systemd => {}
            }
            if !stopped {
                self.sys.sleep(2.0);
            }
        }

        // The program: written beside the old one and put in its place in one step, so that
        // a copy that is still running is not written into.
        if !same_file(&self.current_exe, &program) {
            fs::create_dir_all(&bin)
                .map_err(|e| format!("cannot create {}: {}", bin.display(), e))?;
            let staged = bin.join(format!(".{}.new", CONSOLE_EXE));
            let copied = fs::read(&self.current_exe)
                .and_then(|bytes| fs::write(&staged, bytes))
                .and_then(|()| fs::set_permissions(&staged, fs::Permissions::from_mode(0o755)))
                .and_then(|()| fs::rename(&staged, &program));
            if let Err(e) = copied {
                let _ = fs::remove_file(&staged);
                return Err(format!(
                    "cannot copy the program to {}: {}",
                    bin.display(),
                    e
                ));
            }
        }

        let mut not_started: Option<String> = None;
        if managed {
            let text = match manager {
                Manager::Launchd => agent_plist(&program, &arguments, &environment),
                Manager::Systemd => unit_text(&program, &arguments, &environment),
            };
            if let Some(dir) = service.parent() {
                fs::create_dir_all(dir)
                    .map_err(|e| format!("cannot create {}: {}", dir.display(), e))?;
            }
            winfs::atomic_write(&service, text.as_bytes(), None, true, Expect::Any)
                .map_err(|e| format!("cannot write {}: {}", service.display(), e))?;
            match manager {
                Manager::Launchd => {
                    // An agent that was paused stays disabled until it is enabled again. (Asked
                    // first: launchd keeps a note of every name it was told to enable or disable,
                    // and there is no need to leave one where nothing was paused.)
                    if agent_disabled(self.sys) {
                        let target = agent_target(self.sys);
                        let _ = self.sys.run("launchctl", &strs(&["enable", &target]));
                    }
                    let code = self.load_agent(&service);
                    if code != 0 {
                        not_started = Some(format!(
                            "launchd did not take the launch agent (launchctl bootstrap exit code {}).\n  \
                             It is in place and is loaded at your next login. To try again now, run install again.",
                            code
                        ));
                    }
                }
                Manager::Systemd => {
                    let _ = self
                        .sys
                        .run("systemctl", &strs(&["--user", "daemon-reload"]));
                    let enabled = self.sys.run(
                        "systemctl",
                        &strs(&["--user", "enable", "--now", UNIT_NAME]),
                    );
                    if !enabled.as_ref().is_ok_and(|c| *c == 0) {
                        not_started = Some(format!(
                            "systemd did not take the unit (systemctl --user enable --now exit code {}).\n  \
                             Look at:  systemctl --user status {}",
                            enabled.unwrap_or(-1),
                            UNIT_NAME
                        ));
                    }
                }
            }
        }

        let saved = save_settings(&home, settings);
        let linked = self.link(&program);
        let link_dir = link_file(self.env).and_then(|l| l.parent().map(Path::to_path_buf));
        let callable = linked.is_ok()
            && link_dir
                .as_ref()
                .is_some_and(|dir| on_path(self.env.path.as_deref(), dir));

        if !managed {
            return Err(format!(
                "the program is in place, but nothing here can start the daemon by itself: there is no systemd \
                 user session (systemctl --user did not answer).\n  \
                 Program:  {}\n  \
                 To sync, start the daemon yourself, for example from your desktop's autostart:\n      \
                 {} {}\n  \
                 A single pass:  {} sync",
                program.display(),
                self.program(callable),
                arguments,
                self.program(callable)
            ));
        }
        if let Some(why) = not_started {
            return Err(format!(
                "the program is in place, but the daemon was not started: {}",
                why
            ));
        }

        // Do not report success unless THIS version really came up (a crash at start leaves
        // nothing else to see, and an old daemon that would not stop must not pass for the new one).
        let Some(pid) = self.wait_for_daemon(began, true) else {
            return Err(format!(
                "the program was copied and the autostart was set up, but the new daemon did not come up.\n  \
                 Look at:  {}\n  Then:     {} status",
                home.join("sync.log").display(),
                self.program(callable)
            ));
        };

        let name = self.program(callable);
        self.tell("Installed. From now on the session list is kept the same under every account.");
        self.tell("");
        self.tell(&format!(
            "  Daemon     version {} is running now (pid {})",
            VERSION, pid
        ));
        self.tell(&format!(
            "  Starts by  {}: at login, and started again {} if it stops",
            manager.what(),
            match manager {
                Manager::Launchd => "within 5 minutes",
                Manager::Systemd => "within a minute",
            }
        ));
        self.tell(&format!(
            "  Settings   full pass every {} s, change check {}, archived sessions up to {} days old,",
            settings.interval,
            if settings.quick == 0 {
                String::from("off")
            } else {
                format!("every {} s", settings.quick)
            },
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
        self.tell(&format!("  Program    {}", program.display()));
        self.tell(&format!(
            "  Data       {}   (what it does: sync.log)",
            home.display()
        ));
        let link = link_file(self.env).unwrap_or_default();
        match &linked {
            Ok(made) if callable => self.tell(&format!(
                "  PATH       {} {}",
                if *made {
                    "linked as"
                } else {
                    "already linked as"
                },
                link.display()
            )),
            Ok(_) => {
                self.tell(&format!("  PATH       linked as {}", link.display()));
                self.tell("             (that folder is not on your PATH: add it, or call the program by its full path, as below)");
            }
            Err(why) => self.tell(&format!(
                "  PATH       not linked ({}): call the program by its full path, as below",
                why
            )),
        }
        if !environment.is_empty() {
            let names: Vec<&str> = environment.iter().map(|(name, _)| *name).collect();
            self.tell("");
            self.tell(&format!(
                "Note: {} {} set in this shell.",
                names.join(", "),
                if names.len() == 1 { "is" } else { "are" }
            ));
            self.tell(&format!(
                "What {} was written into the {}, so the daemon looks in the same",
                if names.len() == 1 {
                    "it says"
                } else {
                    "they say"
                },
                manager.what()
            ));
            self.tell(
                "places. Commands you run later look there only if the same is set then, too.",
            );
        }
        self.tell("");
        self.tell("What you can do now:");
        self.tell(&format!(
            "  {} status      is it running, is anything waiting, are there conflicts",
            name
        ));
        self.tell(&format!("  {} log         what it did lately", name));
        self.tell(&format!(
            "  {} pause       stop syncing for a while (resume starts it again)",
            name
        ));
        self.tell(&format!(
            "  {} uninstall   remove it (your data is kept)",
            name
        ));
        self.tell("");
        self.tell("A session that is copied into the account you are signed in to shows up in the Claude app");
        self.tell("after you restart the app or sign in again.");
        Ok(())
    }

    /// Remove the autostart and the link, then stop the daemon. Records already copied,
    /// backups, the quarantine and replaced folders, the journal and the log are left in
    /// place, unless `purge` is given: then this tool's own files are deleted as well.
    pub fn uninstall(&mut self, purge: bool) -> Result<(), String> {
        let manager = self.sys.manager();
        let home = self.paths.home.clone();
        let bin = home.join("bin");
        let program = bin.join(CONSOLE_EXE);
        let service = service_file(manager, self.env);
        let label = |what: &str| format!("  {:<52}  ", what);
        let mut name = manager.what();
        name[..1].make_ascii_uppercase();

        // Unregister first, so that nothing brings the daemon back while it is stopping.
        let had_file = service.as_ref().is_some_and(|f| f.is_file());
        let mut gone = true;
        let mut was_registered = had_file;
        match manager {
            Manager::Launchd => {
                let target = agent_target(self.sys);
                if agent_loaded(self.sys) == Some(true) {
                    was_registered = true;
                    let _ = self.sys.run("launchctl", &strs(&["bootout", &target]));
                    // (it lets go of the agent a moment after it says so)
                    for _ in 0..10 {
                        if agent_loaded(self.sys) != Some(true) {
                            break;
                        }
                        self.sys.sleep(0.5);
                    }
                    gone = agent_loaded(self.sys) != Some(true);
                }
                // A pause is kept in launchd's own records, by name: without this, an agent
                // installed later under the same name would stay disabled.
                if was_registered && agent_disabled(self.sys) {
                    let _ = self.sys.run("launchctl", &strs(&["enable", &target]));
                }
            }
            Manager::Systemd => {
                if had_file {
                    let disabled = self.sys.run(
                        "systemctl",
                        &strs(&["--user", "disable", "--now", UNIT_NAME]),
                    );
                    // (where no systemd answers, nothing is running under it either)
                    gone = disabled.is_ok_and(|c| c == 0) || !self.systemd_answers();
                }
            }
        }
        if let Some(file) = service.as_ref().filter(|_| had_file) {
            gone = fs::remove_file(file).is_ok() && gone;
            if manager == Manager::Systemd {
                let _ = self
                    .sys
                    .run("systemctl", &strs(&["--user", "daemon-reload"]));
            }
        }
        if !was_registered {
            self.tell(&format!("{}was not installed", label(&name)));
        } else if gone {
            self.tell(&format!("{}removed", label(&name)));
        } else {
            self.tell(&format!(
                "{}could NOT be removed{}",
                label(&name),
                service
                    .as_ref()
                    .map_or_else(String::new, |f| format!(": see {}", f.display()))
            ));
        }

        let link = link_file(self.env).unwrap_or_default();
        match self.unlink(&program) {
            Ok(true) => self.tell(&format!(
                "{}removed",
                label(&format!("Link {}", link.display()))
            )),
            Ok(false) => {}
            Err(why) => self.tell(&format!(
                "{}could NOT be removed ({})",
                label(&format!("Link {}", link.display())),
                why
            )),
        }

        let mut stopped = self.stop().map_err(|e| e.to_string())?;
        if stopped {
            self.tell(&format!("{}not running any more", label("Daemon")));
        } else {
            // It did not answer the request (it may be stuck, and nothing looks after it now):
            // end that one process, after making sure the pid on record really is that daemon.
            let st = status_file(&home);
            let pid = st
                .as_ref()
                .and_then(|st| st.get("pid").and_then(Value::as_u64))
                .and_then(|p| u32::try_from(p).ok());
            let started = st
                .as_ref()
                .and_then(|st| st.get("started").and_then(Value::as_f64));
            match pid {
                Some(pid)
                    if pid != std::process::id()
                        && self.sys.is_ours(pid, started)
                        && self.sys.terminate(pid) =>
                {
                    self.tell(&format!(
                        "{}did not stop when asked and was ended (pid {})",
                        label("Daemon"),
                        pid
                    ));
                    stopped = true;
                }
                _ => self.tell(&format!(
                    "{}did NOT stop when asked: end it yourself (kill {})",
                    label("Daemon"),
                    self.pid_on_record()
                )),
            }
        }
        self.tell("");
        let call = self.program(false);
        if !purge {
            self.tell("Uninstalled. Nothing of yours was deleted. Still in place:");
            self.tell(&format!(
                "  {}   log, journal, backups, quarantine, replaced versions",
                home.display()
            ));
            self.tell(&format!("  {}   the program", bin.display()));
            self.tell("  the session records already copied into each account's folder");
            self.tell("");
            self.tell(&format!(
                "To delete this tool's own files as well:   {} uninstall --purge",
                call
            ));
            self.tell(&format!(
                "To take the copied records back:           {} undo --since \"YYYY-MM-DD HH:MM\"",
                call
            ));
            self.tell(
                "                                           (quit the Claude app completely first)",
            );
            self.tell(&format!(
                "To start syncing again:                    {} install",
                call
            ));
            return Ok(());
        }
        if !stopped {
            return Err(String::from(
                "nothing was purged: the daemon is still running. End it, then run uninstall --purge again.",
            ));
        }
        let left = self.purge();
        self.tell("Uninstalled and purged: this tool's log, journal, backups, quarantine, replaced versions");
        self.tell("and program were deleted. The session records already copied into each account's folder");
        self.tell("are the app's own data and were not touched.");
        if !left.is_empty() {
            self.tell("");
            self.tell("Could NOT be deleted:");
            for path in &left {
                self.tell(&format!("  {}", path.display()));
            }
        }
        Ok(())
    }

    /// Stop syncing until `resume`: the agent or the unit is disabled first, so that nothing
    /// starts the daemon again, and then the daemon is stopped.
    pub fn pause(&mut self) -> Result<(), String> {
        let manager = self.sys.manager();
        let what = manager.what();
        match autostart(self.sys, self.env) {
            Autostart::Task { paused: true } => self.tell(&format!("The {} is paused already.", what)),
            Autostart::Task { paused: false } | Autostart::NotLoaded => {
                let disabled = match manager {
                    Manager::Launchd => {
                        let target = agent_target(self.sys);
                        let disabled = self.sys.run("launchctl", &strs(&["disable", &target]));
                        if disabled.as_ref().is_ok_and(|c| *c == 0) {
                            // (this ends the daemon, by asking it to; nothing to unload is no error)
                            let _ = self.sys.run("launchctl", &strs(&["bootout", &target]));
                        }
                        disabled
                    }
                    Manager::Systemd => self.sys.run(
                        "systemctl",
                        &strs(&["--user", "disable", "--now", UNIT_NAME]),
                    ),
                };
                if !disabled.is_ok_and(|c| c == 0) {
                    return Err(format!(
                        "the {} could not be disabled; nothing was changed",
                        what
                    ));
                }
            }
            Autostart::NoManager => {
                return Err(String::from(
                    "there is no systemd user session here, so nothing starts the daemon by itself and there is \
                     nothing to pause.\n  A daemon you started yourself is stopped with:  claude-session-sync stop",
                ))
            }
            Autostart::Unknown => return Err(format!("{} could not be run", manager.tool())),
            Autostart::NotInstalled | Autostart::StartupEntry | Autostart::NoTaskSeen => {
                return Err(format!(
                    "not installed: there is no {} to pause.\n  \
                     A daemon you started yourself is stopped with:  claude-session-sync stop",
                    match manager {
                        Manager::Launchd => "launch agent",
                        Manager::Systemd => "systemd user unit",
                    }
                ))
            }
        }
        let mut stopped = self.stop().map_err(|e| e.to_string())?;
        if !stopped {
            self.sys.sleep(2.0);
            stopped = !daemon_running(&self.paths.home);
        }
        if !stopped {
            return Err(format!(
                "the {} is disabled, so the daemon will not be started again, but the one that is running did \
                 not stop.\n  End it yourself:  kill {}",
                what,
                self.pid_on_record()
            ));
        }
        self.tell(
            "Paused. The daemon is stopped and will not be started again, not at login either.",
        );
        self.tell("To start syncing again:  claude-session-sync resume");
        Ok(())
    }

    /// Start syncing again after `pause`.
    pub fn resume(&mut self) -> Result<(), String> {
        let manager = self.sys.manager();
        let what = manager.what();
        let found = autostart(self.sys, self.env);
        match found {
            Autostart::Task { .. } | Autostart::NotLoaded => {}
            Autostart::NoManager => {
                return Err(String::from(
                    "there is no systemd user session here, so nothing starts the daemon by itself.\n  \
                     To run it yourself:  claude-session-sync daemon",
                ))
            }
            Autostart::Unknown => return Err(format!("{} could not be run", manager.tool())),
            Autostart::NotInstalled | Autostart::StartupEntry | Autostart::NoTaskSeen => {
                return Err(format!(
                    "not installed: there is no {} to resume.\n  To install:  claude-session-sync install",
                    match manager {
                        Manager::Launchd => "launch agent",
                        Manager::Systemd => "systemd user unit",
                    }
                ))
            }
        }
        if found == (Autostart::Task { paused: false }) && daemon_running(&self.paths.home) {
            self.tell("Not paused: the daemon is running.");
            return Ok(());
        }
        let began = winfs::real_now();
        let started = match manager {
            Manager::Launchd => {
                let target = agent_target(self.sys);
                let enabled = self.sys.run("launchctl", &strs(&["enable", &target]));
                if !enabled.is_ok_and(|c| c == 0) {
                    return Err(format!("the {} could not be enabled", what));
                }
                if agent_loaded(self.sys) == Some(true) {
                    // loaded, and its daemon not running: start it now, not at the next interval
                    self.sys
                        .run("launchctl", &strs(&["kickstart", &target]))
                        .unwrap_or(-1)
                } else {
                    let file = service_file(manager, self.env).unwrap_or_default();
                    self.load_agent(&file)
                }
            }
            Manager::Systemd => self
                .sys
                .run(
                    "systemctl",
                    &strs(&["--user", "enable", "--now", UNIT_NAME]),
                )
                .unwrap_or(-1),
        };
        if started != 0 {
            return Err(format!(
                "the {} could not be started again ({} exit code {})",
                what,
                manager.tool(),
                started
            ));
        }
        match self.wait_for_daemon(began, false) {
            Some(pid) => {
                self.tell(&format!(
                    "Resumed. The daemon is running again (pid {}).",
                    pid
                ));
                Ok(())
            }
            None => Err(format!(
                "the {} is enabled again, but the daemon has not come up yet.\n  Look at:  {}",
                what,
                self.paths.home.join("sync.log").display()
            )),
        }
    }
}
