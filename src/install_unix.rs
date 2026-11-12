//! Autostart and upkeep on macOS and Linux.
//!
//! macOS: a launch agent in `~/Library/LaunchAgents`. launchd starts the daemon at login,
//! starts it again at once if it ends with an error, and within five minutes if it ended
//! without one (`stop`).
//!
//! Linux: a systemd user unit, in the folder the user's systemd reads its units from
//! (`~/.config/systemd/user` unless that systemd was started with another
//! `XDG_CONFIG_HOME`). systemd starts the daemon at login and starts it again a minute after
//! it ends, however it ended. Where there is no systemd user session, nothing can start the
//! daemon by itself: `install` puts the program in place, says so, and leaves a note for
//! `status` to say so too.
//!
//! Either way the program is copied to the `bin` folder of this tool's own folder and linked
//! as `~/.local/bin/claude-session-sync`; no shell profile is touched. The variables that
//! say where things are (`CLAUDE_SESSION_SYNC_HOME`, `CLAUDE_SESSION_SYNC_STORE`,
//! `CLAUDE_CONFIG_DIR`, and on Linux `XDG_CONFIG_HOME`) are written into the agent or the
//! unit if they are set when `install` runs, so that the daemon looks where the command
//! line looked.
//!
//! What is said is what was found, not what was meant: whether the daemon is loaded,
//! enabled or running is asked of launchd or systemd itself, whatever files are or are not
//! there, and "stopped" is only said once they agree.
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
    Autostart, Given, Settings, Setup, System, CONSOLE_EXE, OWN_DIRS, OWN_FILES,
};
use crate::consts::VERSION;
use crate::paths::{Env, Paths, HOME_VAR};
use crate::winfs::{self, Expect};

/// The name launchd knows the daemon by.
pub const AGENT_LABEL: &str = "io.github.ssgardaf.claude-session-sync";
/// The name systemd knows the daemon by.
pub const UNIT_NAME: &str = "claude-session-sync.service";
/// A note in this tool's folder: it was installed where nothing can start the daemon by
/// itself (Linux without a systemd user session).
pub const NO_AUTOSTART_FILE: &str = "no-autostart";

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

    /// What the daemon is to it, in two words.
    fn kind(self) -> &'static str {
        match self {
            Manager::Launchd => "launch agent",
            Manager::Systemd => "systemd user unit",
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

/// The file that tells the manager about the daemon, where it is when nothing says
/// otherwise: the agent's file, or the unit in `~/.config/systemd/user`. (Where the user's
/// systemd really reads its units from is asked of it: see `located`.)
pub fn service_file(manager: Manager, env: &Env) -> Option<PathBuf> {
    let home = env.userprofile.as_ref()?;
    Some(match manager {
        Manager::Launchd => home
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{}.plist", AGENT_LABEL)),
        Manager::Systemd => home
            .join(".config")
            .join("systemd")
            .join("user")
            .join(UNIT_NAME),
    })
}

/// The folder a systemd user manager reads this user's own units from, out of its list of
/// such folders (`systemctl --user show -p UnitPath --value`): the first that is called
/// `.../systemd/user` and is neither the system's nor a passing one under `/run`.
pub fn unit_dir_in(unit_path: &str) -> Option<PathBuf> {
    unit_path
        .split_whitespace()
        .find(|dir| {
            dir.ends_with("/systemd/user")
                && !["/run/", "/etc/", "/usr/", "/lib/"]
                    .iter()
                    .any(|system| dir.starts_with(system))
        })
        .map(PathBuf::from)
}

/// What the user's systemd says of where it reads units from. None if it does not answer
/// (there is no systemd user session).
fn unit_path(sys: &mut dyn System) -> Option<String> {
    match sys.run_output(
        "systemctl",
        &strs(&["--user", "show", "-p", "UnitPath", "--value"]),
    ) {
        Ok((0, listing)) if !listing.trim().is_empty() => Some(listing),
        _ => None,
    }
}

/// The unit's file, given what systemd said of its folders (or did not say).
fn unit_file(listing: Option<&str>, env: &Env) -> Option<PathBuf> {
    listing
        .and_then(unit_dir_in)
        .map(|dir| dir.join(UNIT_NAME))
        .or_else(|| service_file(Manager::Systemd, env))
}

/// The link systemd makes for an enabled unit, beside the unit's file.
fn wants_link(unit: &Path) -> Option<PathBuf> {
    Some(unit.parent()?.join("default.target.wants").join(UNIT_NAME))
}

/// Where the program is linked so that it can be called by its name.
pub fn link_file(env: &Env) -> Option<PathBuf> {
    env.userprofile
        .as_ref()
        .map(|home| home.join(".local").join("bin").join(CONSOLE_EXE))
}

/// The variables that say where things are, as far as they are set, with full paths. On
/// Linux that includes `XDG_CONFIG_HOME`, which says where the app's own folder is: a
/// daemon started by systemd does not see what was set in the shell `install` ran in.
pub fn overrides(env: &Env, manager: Manager) -> Vec<(&'static str, PathBuf)> {
    let full = |p: &PathBuf| std::path::absolute(p).unwrap_or_else(|_| p.clone());
    let none = None;
    [
        ("CLAUDE_SESSION_SYNC_HOME", &env.home_override),
        ("CLAUDE_SESSION_SYNC_STORE", &env.store_override),
        ("CLAUDE_CONFIG_DIR", &env.claude_config_dir),
        (
            "XDG_CONFIG_HOME",
            match manager {
                Manager::Systemd => &env.xdg_config_home,
                Manager::Launchd => &none,
            },
        ),
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

/// Why systemd cannot be given this program to start, if it cannot. It takes the program's
/// path as it is written, between double quotes, with `%` doubled and nothing else changed
/// (a `$` stands for itself there); a path with a quote of either kind, a backslash or a
/// control character in it is refused by systemd however it is written ("Executable name
/// contains special characters").
pub fn unit_refusal(program: &Path) -> Option<String> {
    let text = program.to_str()?;
    let odd = |c: char| matches!(c, '"' | '\'' | '\\' | '\x7f') || c.is_control();
    text.chars().any(odd).then(|| {
        format!(
            "systemd cannot start a program whose path has a quote, a backslash or a control character in it:\n  {}\n  \
             Nothing was installed. Keep this tool's files in a folder without such characters (name it in \
             CLAUDE_SESSION_SYNC_HOME) and run install again.",
            text
        )
    })
}

/// The program's path as `ExecStart=` takes it (see `unit_refusal` for what it cannot take).
fn unit_program(program: &Path) -> String {
    format!("\"{}\"", program.to_string_lossy().replace('%', "%%"))
}

/// A setting for `Environment=`, in quotes, so that systemd reads it back as it is: there a
/// backslash and a double quote are written with a backslash before them, `%` is doubled,
/// and `$` stands for itself.
fn unit_setting(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
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
        format!("ExecStart={} {}", unit_program(program), arguments),
    ];
    for (name, value) in environment {
        lines.push(format!(
            "Environment={}",
            unit_setting(&format!("{}={}", name, value.to_string_lossy()))
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

/// Does launchd have the agent loaded? None if it could not be asked.
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

/// Have launchd let go of the agent (which ends its daemon), and wait until it has: it
/// says so a moment before it is done. True if the agent is not loaded afterwards.
fn agent_out(sys: &mut dyn System) -> bool {
    let target = agent_target(sys);
    let _ = sys.run("launchctl", &strs(&["bootout", &target]));
    for _ in 0..10 {
        if agent_loaded(sys) != Some(true) {
            return true;
        }
        sys.sleep(0.5);
    }
    false
}

/// One word from systemd about the unit (`is-enabled`, `is-active`); empty if it said
/// nothing.
fn unit_says(sys: &mut dyn System, question: &str) -> String {
    sys.run_output("systemctl", &strs(&["--user", question, UNIT_NAME]))
        .map(|(_, said)| said.trim().to_string())
        .unwrap_or_default()
}

/// Is systemd running the unit, or about to?
fn unit_active(sys: &mut dyn System) -> bool {
    matches!(
        unit_says(sys, "is-active").as_str(),
        "active" | "activating" | "reloading"
    )
}

/// Ask the system how the daemon is started. launchd and systemd are asked whatever files
/// are there: a daemon they run without its file is reported as that, not as "not installed".
pub fn autostart(sys: &mut dyn System, env: &Env) -> Autostart {
    match sys.manager() {
        Manager::Launchd => {
            let file = service_file(Manager::Launchd, env).is_some_and(|f| f.is_file());
            match agent_loaded(sys) {
                None => Autostart::Unknown,
                Some(true) if file => Autostart::Task { paused: false },
                Some(true) => Autostart::NoFile,
                Some(false) if !file => Autostart::NotInstalled,
                Some(false) if agent_disabled(sys) => Autostart::Task { paused: true },
                Some(false) => Autostart::NotLoaded,
            }
        }
        Manager::Systemd => {
            let listing = unit_path(sys);
            let file = unit_file(listing.as_deref(), env).is_some_and(|f| f.is_file());
            if listing.is_none() {
                // no user session of systemd: nothing starts the daemon here
                let noted =
                    Paths::discover(env).is_ok_and(|p| p.home.join(NO_AUTOSTART_FILE).exists());
                return match file || noted {
                    true => Autostart::NoManager,
                    false => Autostart::NotInstalled,
                };
            }
            let enabled = unit_says(sys, "is-enabled");
            let active = unit_active(sys);
            match (file, enabled.as_str(), active) {
                (false, _, false) => Autostart::NotInstalled,
                (false, _, true) => Autostart::NoFile,
                (true, "enabled" | "enabled-runtime", _) => Autostart::Task { paused: false },
                (true, "disabled", false) => Autostart::Task { paused: true },
                // disabled by hand while it runs, masked, or something else not as installed
                (true, _, _) => Autostart::NotLoaded,
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
            "autostart: installed, but the {} is not in effect as it was installed (run install again)",
            what
        ),
        Autostart::NoFile => format!(
            "autostart: the {} is active, but its file is missing (run install again)",
            what
        ),
        Autostart::NoManager => String::from(
            "autostart: not available here (no systemd user session); the daemon runs only while you run it: claude-session-sync daemon",
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

/// What is said of the unit after an install that did not work took it out again: exactly
/// the state it is left in, which is the state `status` reports afterwards.
fn unit_left(had_one_before: bool) -> &'static str {
    match had_one_before {
        false => "The unit was removed again, so nothing that does not start is left enabled: this tool is not installed to start by itself now.",
        true => "The unit that was installed before was put back and is left disabled, so nothing that does not start is left enabled: `status` shows it as paused, and `claude-session-sync resume` enables it again.",
    }
}

/// A path that can be written into an agent or a unit: text without line breaks.
fn writable(path: &Path) -> Result<(), String> {
    match path.to_str() {
        Some(text) if !text.chars().any(char::is_control) => Ok(()),
        _ => Err(format!(
            "{} cannot be written into the daemon's start-up definition (it is not plain text). Nothing was installed.",
            path.display()
        )),
    }
}

/// Everything below a folder of this tool's own becomes the user's alone. Links are neither
/// followed nor changed.
fn tighten_tree(dir: &Path) {
    let _ = winfs::make_private(dir);
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => tighten_tree(&entry.path()),
            Ok(kind) if kind.is_file() => {
                let _ = winfs::make_private(&entry.path());
            }
            _ => {}
        }
    }
}

/// Make this tool's own folder, and what this tool keeps in it, the user's alone: folders
/// 0700, files 0600 (the program stays a program). An install made by an earlier version
/// left them readable by everyone; this is run when the daemon starts and at every install.
/// Only what is this tool's own is touched, should the folder hold more.
pub fn tighten(home: &Path) {
    if !fs::symlink_metadata(home).is_ok_and(|found| found.is_dir()) {
        return;
    }
    let _ = winfs::make_private(home);
    for name in OWN_FILES {
        let _ = winfs::make_private(&home.join(name));
    }
    for name in OWN_DIRS.iter().chain(&["bin"]) {
        let dir = home.join(name);
        if fs::symlink_metadata(&dir).is_ok_and(|found| found.is_dir()) {
            tighten_tree(&dir);
        }
    }
    if let Ok(entries) = fs::read_dir(home) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(".sync-") && name.ends_with(".tmp") {
                let _ = winfs::make_private(&entry.path());
            }
        }
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

    fn systemctl(&mut self, args: &[&str]) -> i32 {
        let mut all = vec!["--user"];
        all.extend_from_slice(args);
        self.sys.run("systemctl", &strs(&all)).unwrap_or(-1)
    }

    /// Take the unit away from systemd, whatever state it is in: stopped (so that systemd
    /// does not start the daemon again), disabled, its file removed or put back as it was
    /// before (`restore`), and the link systemd made for it removed if that is left over.
    /// True if systemd is not running it afterwards and nothing is left enabled.
    fn unit_out(&mut self, unit: &Path, restore: Option<&[u8]>) -> bool {
        let _ = self.systemctl(&["stop", UNIT_NAME]);
        let _ = self.systemctl(&["disable", UNIT_NAME]);
        match restore {
            Some(before) => {
                let _ = fs::write(unit, before);
            }
            None => {
                let _ = fs::remove_file(unit);
            }
        }
        if let Some(link) = wants_link(unit) {
            // (left dangling when the unit's file was removed before it was disabled)
            if fs::symlink_metadata(&link).is_ok_and(|found| found.file_type().is_symlink()) {
                let _ = fs::remove_file(&link);
            }
        }
        let _ = self.systemctl(&["daemon-reload"]);
        let _ = self.systemctl(&["reset-failed", UNIT_NAME]);
        !unit_active(self.sys)
            && !wants_link(unit).is_some_and(|l| fs::symlink_metadata(l).is_ok())
            && (restore.is_some() || !unit.exists())
    }

    /// The pid of the daemon on record, for telling the user what to end.
    fn pid_on_record(&self) -> String {
        status_file(&self.paths.home)
            .and_then(|st| st.get("pid").and_then(Value::as_u64))
            .map_or_else(|| String::from("<its pid>"), |pid| pid.to_string())
    }

    /// Where to look when the daemon did not come up.
    fn where_to_look(&self, manager: Manager) -> String {
        let log = self.paths.home.join("sync.log");
        let mut places = Vec::new();
        if manager == Manager::Systemd {
            places.push(format!(
                "systemctl --user status {}",
                UNIT_NAME.trim_end_matches(".service")
            ));
        }
        if log.exists() {
            places.push(log.display().to_string());
        }
        if places.is_empty() {
            places.push(format!("{} (not written yet)", log.display()));
        }
        places.join("\n            ")
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
        if !self.current_exe.is_file() {
            return Err(format!(
                "cannot tell where this program is ({} is not a file)",
                self.current_exe.display()
            ));
        }
        // What cannot be written into the agent or the unit is said before anything is changed.
        let environment = overrides(self.env, manager);
        writable(&program)?;
        for (_, value) in &environment {
            writable(value)?;
        }
        if manager == Manager::Systemd {
            if let Some(why) = unit_refusal(&program) {
                return Err(why);
            }
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

        // Where the definition goes. Without a user session of systemd nothing here can start
        // the daemon by itself, and there is nowhere for a unit to go.
        let listing = match manager {
            Manager::Systemd => unit_path(self.sys),
            Manager::Launchd => None,
        };
        let managed = manager == Manager::Launchd || listing.is_some();
        let service = match manager {
            Manager::Launchd => service_file(manager, self.env),
            Manager::Systemd => unit_file(listing.as_deref(), self.env),
        }
        .ok_or(format!("{} is not set", HOME_VAR))?;

        // A daemon that is already running keeps the old program and settings. Ask it to finish
        // what it is doing and exit; then take it out of the manager's hands, which also ends
        // one that did not answer, and keeps it from being started again meanwhile. The manager
        // is asked whether it has the daemon: its file may be gone while it runs.
        let stopped = self.stop().map_err(|e| e.to_string())?;
        match manager {
            Manager::Launchd => {
                if agent_loaded(self.sys) == Some(true) {
                    agent_out(self.sys);
                }
            }
            Manager::Systemd if managed => {
                let _ = self.systemctl(&["stop", UNIT_NAME]);
            }
            Manager::Systemd => {}
        }
        if !stopped {
            self.sys.sleep(2.0);
        }

        // The program: written beside the old one and put in its place in one step, so that
        // a copy that is still running is not written into.
        winfs::make_dirs(&home).map_err(|e| format!("cannot create {}: {}", home.display(), e))?;
        if !same_file(&self.current_exe, &program) {
            winfs::make_dirs(&bin)
                .map_err(|e| format!("cannot create {}: {}", bin.display(), e))?;
            let staged = bin.join(format!(".{}.new", CONSOLE_EXE));
            let copied = fs::read(&self.current_exe)
                .and_then(|bytes| winfs::write_file(&staged, &bytes))
                .and_then(|()| fs::set_permissions(&staged, fs::Permissions::from_mode(0o700)))
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
        // (what an earlier version left readable by everyone is the user's alone from now on)
        tighten(&home);

        let mut not_started: Option<String> = None;
        let before = fs::read(&service).ok();
        if managed {
            let text = match manager {
                Manager::Launchd => agent_plist(&program, &arguments, &environment),
                Manager::Systemd => unit_text(&program, &arguments, &environment),
            };
            if let Some(dir) = service.parent() {
                fs::create_dir_all(dir)
                    .map_err(|e| format!("cannot create {}: {}", dir.display(), e))?;
            }
            // (launchd and systemd want their files readable; nothing in them is private)
            winfs::atomic_write(&service, text.as_bytes(), None, true, Expect::Any)
                .map_err(|e| format!("cannot write {}: {}", service.display(), e))?;
            fs::set_permissions(&service, fs::Permissions::from_mode(0o644))
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
                    let _ = self.systemctl(&["daemon-reload"]);
                    let code = self.systemctl(&["enable", "--now", UNIT_NAME]);
                    if code != 0 {
                        // nothing that does not start is left enabled
                        self.unit_out(&service, before.as_deref());
                        not_started = Some(format!(
                            "systemd did not take the unit (systemctl --user enable --now exit code {}).\n  \
                             {}\n  Look at:  systemctl --user status {}",
                            code,
                            unit_left(before.is_some()),
                            UNIT_NAME.trim_end_matches(".service")
                        ));
                    }
                }
            }
        }

        let saved = save_settings(&home, settings);
        let linked = self.link(&program);
        let link = link_file(self.env).unwrap_or_default();
        let callable = linked.is_ok()
            && link
                .parent()
                .is_some_and(|dir| on_path(self.env.path.as_deref(), dir));
        let name = self.program(callable);
        let path_lines = |made: &Result<bool, String>| -> Vec<String> {
            match made {
                Ok(made) if callable => vec![format!(
                    "  PATH       {} {}",
                    if *made {
                        "linked as"
                    } else {
                        "already linked as"
                    },
                    link.display()
                )],
                Ok(_) => vec![
                    format!("  PATH       linked as {}", link.display()),
                    String::from(
                        "             (that folder is not on your PATH: add it, or call the program by its full path, as below)",
                    ),
                ],
                Err(why) => vec![format!(
                    "  PATH       not linked ({}): call the program by its full path, as below",
                    why
                )],
            }
        };

        let note = home.join(NO_AUTOSTART_FILE);
        if !managed {
            // The program is in place, and that is all that can be done here: said plainly,
            // and put on record for `status` to say so too.
            let _ = winfs::write_file(
                &note,
                b"installed where nothing can start the daemon by itself: no systemd user session\n",
            );
            self.tell(
                "Installed, but NOT to start by itself: there is no systemd user session here",
            );
            self.tell(
                "(systemctl --user did not answer), and nothing else this tool knows of starts a",
            );
            self.tell("program at login.");
            self.tell("");
            self.tell(&format!("  Program    {}", program.display()));
            self.tell(&format!("  Data       {}", home.display()));
            for line in path_lines(&linked) {
                self.tell(&line);
            }
            self.tell("");
            self.tell(
                "To sync, run the daemon yourself, for example from your desktop's autostart:",
            );
            self.tell(&format!("  {} {}", name, arguments));
            self.tell("or make a single pass whenever you like:");
            self.tell(&format!("  {} sync", name));
            self.tell("");
            self.tell("Run install again once a systemd user session is there.");
            return Ok(());
        }
        let _ = fs::remove_file(&note);
        if let Some(why) = not_started {
            return Err(format!(
                "the program is in place, but the daemon was not started: {}",
                why
            ));
        }

        // Do not report success unless THIS version really came up (a crash at start leaves
        // nothing else to see, and an old daemon that would not stop must not pass for the new one).
        let Some(pid) = self.wait_for_daemon(began, true) else {
            let look = self.where_to_look(manager);
            let undone = match manager {
                Manager::Systemd => {
                    // (systemd would go on trying once a minute for ever)
                    self.unit_out(&service, before.as_deref());
                    format!("\n  {}", unit_left(before.is_some()))
                }
                Manager::Launchd => String::new(),
            };
            return Err(format!(
                "the program was copied and the autostart was set up, but the new daemon did not come up.{}\n  \
                 Look at:  {}\n  Then:     {} status",
                undone, look, name
            ));
        };

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
        for line in path_lines(&linked) {
            self.tell(&line);
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
        let label = |what: &str| format!("  {:<52}  ", what);
        let mut name = manager.what();
        name[..1].make_ascii_uppercase();

        // Unregister first, so that nothing brings the daemon back while it is stopping. What
        // launchd or systemd has is asked of them, whatever files are there or are not: a
        // daemon whose file was deleted while it runs is still theirs to start again.
        let (was_there, gone, seen) = match manager {
            Manager::Launchd => {
                let file = service_file(manager, self.env).filter(|f| f.is_file());
                let loaded = agent_loaded(self.sys) == Some(true);
                let out = !loaded || agent_out(self.sys);
                // A pause is kept in launchd's own records, by name: without this, an agent
                // installed later under the same name would stay disabled.
                if (loaded || file.is_some()) && agent_disabled(self.sys) {
                    let target = agent_target(self.sys);
                    let _ = self.sys.run("launchctl", &strs(&["enable", &target]));
                }
                let removed = file.as_ref().is_none_or(|f| fs::remove_file(f).is_ok());
                (loaded || file.is_some(), out && removed, file)
            }
            Manager::Systemd => {
                let listing = unit_path(self.sys);
                let unit = unit_file(listing.as_deref(), self.env);
                let file = unit.clone().filter(|f| f.is_file());
                let enabled_link = unit
                    .as_deref()
                    .and_then(wants_link)
                    .is_some_and(|l| fs::symlink_metadata(l).is_ok());
                let active = listing.is_some() && unit_active(self.sys);
                let was_there = file.is_some() || enabled_link || active;
                let gone = match (&unit, listing.is_some()) {
                    (Some(unit), true) if was_there => self.unit_out(unit, None),
                    // (where no systemd answers, nothing runs under it: only the file can be there)
                    (Some(unit), false) => !unit.exists() || fs::remove_file(unit).is_ok(),
                    _ => true,
                };
                (was_there, gone, file)
            }
        };
        let _ = fs::remove_file(home.join(NO_AUTOSTART_FILE));
        if !was_there {
            self.tell(&format!("{}was not installed", label(&name)));
        } else if gone {
            self.tell(&format!("{}removed", label(&name)));
        } else {
            let look = match (manager, &seen) {
                (Manager::Systemd, _) => format!(
                    "systemctl --user status {}",
                    UNIT_NAME.trim_end_matches(".service")
                ),
                (Manager::Launchd, Some(file)) => file.display().to_string(),
                (Manager::Launchd, None) => {
                    format!("launchctl print {}", agent_target(self.sys))
                }
            };
            self.tell(&format!(
                "{}could NOT be removed: see  {}",
                label(&name),
                look
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
        if stopped && was_there && !gone {
            // (what could not be taken away from launchd or systemd may be started by it again)
            self.tell(&format!(
                "{}stopped for now, but may be started again (see above)",
                label("Daemon")
            ));
        } else if stopped {
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
    /// starts the daemon again, and then the daemon is stopped. That it is stopped is only
    /// said once launchd or systemd says so too.
    pub fn pause(&mut self) -> Result<(), String> {
        let manager = self.sys.manager();
        let what = manager.what();
        let found = autostart(self.sys, self.env);
        match found {
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
                    manager.kind()
                ))
            }
            Autostart::Task { paused: true } => {
                self.tell(&format!("The {} is paused already.", what))
            }
            Autostart::Task { paused: false } | Autostart::NotLoaded | Autostart::NoFile => {}
        }
        // Whatever was found, the manager is told to let go of the daemon: one that was
        // disabled by hand may still be running under it, and would be started again.
        let let_go = match manager {
            Manager::Launchd => {
                let target = agent_target(self.sys);
                let disabled = found == (Autostart::Task { paused: true })
                    || self
                        .sys
                        .run("launchctl", &strs(&["disable", &target]))
                        .is_ok_and(|c| c == 0);
                // (this ends the daemon, by asking it to)
                disabled && (agent_loaded(self.sys) != Some(true) || agent_out(self.sys))
            }
            Manager::Systemd => {
                let disabled = self.systemctl(&["disable", "--now", UNIT_NAME]) == 0
                    || !matches!(
                        unit_says(self.sys, "is-enabled").as_str(),
                        "enabled" | "enabled-runtime"
                    );
                // (without its file the unit cannot be disabled, but it can be stopped)
                disabled && {
                    let _ = self.systemctl(&["stop", UNIT_NAME]);
                    !unit_active(self.sys)
                }
            }
        };
        if !let_go {
            return Err(format!(
                "the {} could not be disabled; nothing else was changed",
                what
            ));
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
            Autostart::NoFile => {
                return Err(format!(
                    "the {}'s file is missing, so it cannot be started again.\n  \
                     To put it back:  claude-session-sync install",
                    manager.kind()
                ))
            }
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
                    manager.kind()
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
            Manager::Systemd => self.systemctl(&["enable", "--now", UNIT_NAME]),
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
                self.where_to_look(manager)
            )),
        }
    }
}
