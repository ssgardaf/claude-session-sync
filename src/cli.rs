//! The command line.

use std::cell::RefCell;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::rc::Rc;

use serde_json::Value;

use crate::consts::VERSION;
use crate::daemon::{acquire_lock, lock_for_daemon};
use crate::history::journal_entries;
use crate::install::{self, Autostart, Given, RealSystem, Setup, System, TASK_NAME};
use crate::paths::{Env, Paths};
use crate::plan::{Kind, Plan};
use crate::records::copy_in;
use crate::sync::{num, short, Sync};
use crate::winfs;

/// What `--help` prints, after a first line with the version.
pub const HELP: &str = "\
Keeps the Claude desktop app's Code-tab session list the same under every
account you sign in to on this PC.

The app keeps a separate session list for each account. This tool copies each
session's record into every account's folder, so a session started under one
account is there when you sign in with another. It never touches transcripts
or credentials, keeps every version it replaces, and can undo what it did.

USAGE
    claude-session-sync <command> [options]

TO BEGIN WITH
    preview       Show what would be copied where; writes nothing (the default)
    sync          Copy now, once, and exit
    install       Sync automatically from now on (background daemon at logon)

DAY TO DAY
    status        One-line summary, then what is pending and any conflicts
    log           Show the last lines of the log (-n N: how many; default 30)
    pause         Stop syncing until you run resume
    resume        Start syncing again after pause
    uninstall     Remove the daemon and its autostart (--purge: its files too)

WHEN SOMETHING IS WRONG
    resolve ID    Settle a conflict: the most advanced copy wins (--all: all)
    restore ID    Bring back a session that --delete-sync took out
    undo          Take back what this tool did since a given time (--since)

RARELY NEEDED
    stop          Ask the daemon to exit (it is back in 5 minutes; see pause)
    daemon        Run the daemon in this window (install does this for you)

    ID is the session id that status prints, or its first 6 or more characters.

OPTIONS
    --verbose        preview: list every change, one per line
    --days N         Archived sessions are copied only if they were used in the
                     last N days (default 30). Open sessions always are.
    --delete-sync    When a session is deleted in one account, take it out of
                     the others too. It is moved to quarantine, not erased:
                     status lists it and restore brings it back.
    --no-delete-sync Turn that off again (the default is off)
    --interval N     daemon, install: seconds between full passes (default
                     3600, at least 60)
    --quick N        daemon, install: seconds between quick checks for changes
                     (default 10; 0 = full passes only)
    --since WHEN     undo: a local time, \"YYYY-MM-DD HH:MM\"
    --force          undo: go ahead although the Claude app is running (it may
                     write its old list back over what undo puts right)
    -n N             log: how many lines to show
    --purge          uninstall: also delete this tool's own files (programs,
                     log, backups, quarantine, replaced versions)
    --all            resolve: settle every conflict there is, without an ID
    --version        Print the version
    --help           Print this help

    --days and --delete-sync go with preview, sync, daemon, status, resolve
    and install. Give install the options the daemon is to run with: they are
    remembered. Installing again without options (an update) keeps them, and
    an option given then changes that one only. preview, sync, status and
    resolve use the same settings unless told otherwise.

EXAMPLES
    claude-session-sync preview --verbose
        See exactly what would change. Nothing is written.
    claude-session-sync install
        Keep the lists in sync from now on, and put this program on your PATH.
        Run it again after an update: the settings are kept.
    claude-session-sync install --delete-sync
        The same, and from now on carry deletions over to the other accounts.
    claude-session-sync status
        Is it running, is anything waiting, are there conflicts.
    claude-session-sync resolve 3f9c2a
        Settle the conflict that status reported for session 3f9c2a...
        (resolve --all settles every conflict at once.)
    claude-session-sync undo --since \"2026-01-31 09:00\"
        Take back everything done since then. Uninstall (or pause) first, and
        close the Claude app completely (while it runs, undo wants --force).

GOOD TO KNOW
    A session copied into the account you are signed in to shows up in the app
    after you restart it or sign in again.
    status exits with 0 if the daemon is running and not stalled, 1 otherwise;
    sync exits with 1 if it could not make its pass or an item failed.
    This tool never uses the network. To update it, install the new version
    over the old one.

WHERE THINGS ARE
    %USERPROFILE%\\.claude-session-sync\\     this tool's own files:
        bin\\                                the programs, once installed
        sync.log                            what it did
        backups\\                            the session lists, zipped once a
                                            day before the first change
        replaced\\                           versions it replaced (14 days)
        quarantine\\                         sessions --delete-sync took out
        journal.jsonl                       what undo goes by
        settings.json                       what install was told
    %APPDATA%\\Claude\\claude-code-sessions\\  the app's session lists (synced)
    %USERPROFILE%\\.claude\\projects\\         transcripts (only looked at)

ENVIRONMENT
    CLAUDE_SESSION_SYNC_HOME     Keep this tool's own files in this folder (the
                                 programs stay in the folder named above)
    CLAUDE_SESSION_SYNC_STORE    The app's claude-code-sessions folder, if it
                                 is not found by itself (a Microsoft Store
                                 install is)
    CLAUDE_CONFIG_DIR            Claude Code's folder, if it is not
                                 %USERPROFILE%\\.claude
";

/// The whole help text.
pub fn help() -> String {
    format!("claude-session-sync {}\n{}", VERSION, HELP.trim_end())
}

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    Preview {
        verbose: bool,
    },
    Sync,
    Daemon {
        interval: i64,
        quick: i64,
    },
    Status,
    Log {
        lines: usize,
    },
    Stop,
    Pause,
    Resume,
    /// `id` is None for `--all`.
    Resolve {
        id: Option<String>,
    },
    Restore {
        id: String,
    },
    /// `force`: go ahead although the desktop app is running.
    Undo {
        since: f64,
        force: bool,
    },
    /// What is None is kept from the previous install.
    Install {
        interval: Option<i64>,
        quick: Option<i64>,
    },
    Uninstall {
        purge: bool,
    },
    /// `__path add|remove`: not for people. `install` and `uninstall` have Task Scheduler
    /// run it when they cannot change the PATH themselves (see `install::path_command`).
    PathSetup {
        add: bool,
    },
    Version,
    Help,
}

/// A command line as given. `days` and `delete_sync` are None when they were not given: the
/// settings the daemon was installed with then apply (30 days and off, if there are none).
#[derive(Clone, Debug, PartialEq)]
pub struct Args {
    pub command: Command,
    pub days: Option<i64>,
    pub delete_sync: Option<bool>,
}

fn only(command: Command) -> Args {
    Args {
        command,
        days: None,
        delete_sync: None,
    }
}

/// Parse the arguments (without the program name).
pub fn parse_args(args: &[String]) -> Result<Args, String> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        return Ok(only(Command::Help));
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        return Ok(only(Command::Version));
    }
    let mut rest: Vec<String> = args.to_vec();
    let name = match rest.first() {
        Some(first) if !first.starts_with('-') => rest.remove(0),
        _ => String::from("preview"),
    };
    const COMMON: [&str; 3] = ["--days", "--delete-sync", "--no-delete-sync"];
    let allowed: Vec<&str> = match name.as_str() {
        "preview" => [&["--verbose"][..], &COMMON].concat(),
        "sync" | "status" => COMMON.to_vec(),
        "resolve" => [&["--all"][..], &COMMON].concat(),
        "daemon" | "install" => [&["--interval", "--quick"][..], &COMMON].concat(),
        "undo" => vec!["--since", "--force"],
        "log" => vec!["-n"],
        "uninstall" => vec!["--purge"],
        "stop" | "pause" | "resume" | "restore" | "__path" => Vec::new(),
        "help" => return Ok(only(Command::Help)),
        other => return Err(format!("unknown command '{}'", other)),
    };
    let (mut interval, mut quick, mut days): (Option<i64>, Option<i64>, Option<i64>) =
        (None, None, None);
    let mut lines = 30i64;
    let mut delete_sync: Option<bool> = None;
    let (mut verbose, mut purge, mut all, mut force) = (false, false, false, false);
    let mut since: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut it = rest.into_iter();
    while let Some(arg) = it.next() {
        if !arg.starts_with("--") && arg != "-n" {
            positional.push(arg);
            continue;
        }
        let (opt, inline) = match arg.split_once('=') {
            Some((o, v)) => (o.to_string(), Some(v.to_string())),
            None => (arg.clone(), None),
        };
        if !allowed.contains(&opt.as_str()) {
            return Err(format!("unknown option '{}' for '{}'", opt, name));
        }
        if matches!(
            opt.as_str(),
            "--verbose" | "--delete-sync" | "--no-delete-sync" | "--purge" | "--all" | "--force"
        ) {
            if inline.is_some() {
                return Err(format!("'{}' takes no value", opt));
            }
            match opt.as_str() {
                "--verbose" => verbose = true,
                "--purge" => purge = true,
                "--force" => force = true,
                "--all" => all = true,
                "--delete-sync" => delete_sync = Some(true),
                _ => delete_sync = Some(false),
            }
            continue;
        }
        let value = match inline.or_else(|| it.next()) {
            Some(v) => v,
            None => return Err(format!("'{}' needs a value", opt)),
        };
        if opt == "--since" {
            since = Some(value);
            continue;
        }
        let n: i64 = value
            .trim()
            .parse()
            .map_err(|_| format!("'{}' needs a whole number, not '{}'", opt, value))?;
        match opt.as_str() {
            "--interval" => interval = Some(n),
            "--quick" => quick = Some(n),
            "-n" => lines = n,
            _ => days = Some(n),
        }
    }
    if name == "__path" {
        return match positional.as_slice() {
            [what] if what == "add" || what == "remove" => {
                Ok(only(Command::PathSetup { add: what == "add" }))
            }
            _ => Err(String::from("'__path' is for this tool's own use")),
        };
    }
    let wants_id = name == "restore" || (name == "resolve" && !all);
    if positional.len() != usize::from(wants_id) {
        return Err(if name == "resolve" && all {
            String::from("'resolve --all' takes no session id")
        } else if name == "resolve" {
            String::from("'resolve' needs one session id (or its first characters), or --all")
        } else if wants_id {
            format!("'{}' needs one session id (or its first characters)", name)
        } else {
            format!("unexpected argument '{}'", positional[0])
        });
    }
    let command = match name.as_str() {
        "preview" => Command::Preview { verbose },
        "sync" => Command::Sync,
        "daemon" => Command::Daemon {
            interval: interval.unwrap_or(3600),
            quick: quick.unwrap_or(10),
        },
        "status" => Command::Status,
        "log" => Command::Log {
            lines: lines.max(0) as usize,
        },
        "stop" => Command::Stop,
        "pause" => Command::Pause,
        "resume" => Command::Resume,
        "resolve" => Command::Resolve {
            id: positional.pop(),
        },
        "restore" => Command::Restore {
            id: positional.remove(0),
        },
        "undo" => {
            let text =
                since.ok_or_else(|| String::from("'undo' needs --since \"YYYY-MM-DD HH:MM\""))?;
            Command::Undo {
                since: winfs::parse_when(&text).map_err(|e| format!("bad --since: {}", e))?,
                force,
            }
        }
        "install" => Command::Install { interval, quick },
        _ => Command::Uninstall { purge },
    };
    Ok(Args {
        command,
        days,
        delete_sync,
    })
}

/// Where the command line writes.
pub type Sink = Rc<RefCell<dyn Write>>;

#[derive(Clone)]
pub struct Console {
    pub out: Sink,
    pub err: Sink,
    /// True for the program without a console window: what goes wrong before it has started
    /// is then written to the log, as there is nowhere else for it to be seen.
    pub windowless: bool,
}

impl Console {
    pub fn stdio() -> Console {
        Console {
            out: Rc::new(RefCell::new(io::stdout())),
            err: Rc::new(RefCell::new(io::stderr())),
            windowless: false,
        }
    }

    /// A console that keeps what is written (for tests).
    pub fn capture() -> (Console, Rc<RefCell<Vec<u8>>>) {
        let buf: Rc<RefCell<Vec<u8>>> = Rc::new(RefCell::new(Vec::new()));
        (
            Console {
                out: buf.clone(),
                err: buf.clone(),
                windowless: false,
            },
            buf,
        )
    }

    fn say(&self, line: &str) {
        let mut o = self.out.borrow_mut();
        let _ = writeln!(o, "{}", line);
        let _ = o.flush();
    }

    fn complain(&self, line: &str) {
        let mut e = self.err.borrow_mut();
        let _ = writeln!(e, "{}", line);
        let _ = e.flush();
    }
}

/// Append a line to sync.log without a `Sync` (for failures before one exists).
fn log_line(paths: &Paths, msg: &str) {
    let _ = (|| -> io::Result<()> {
        fs::create_dir_all(&paths.home)?;
        let mut fh = OpenOptions::new()
            .append(true)
            .create(true)
            .open(paths.home.join("sync.log"))?;
        let stamp = winfs::local_format(winfs::real_now(), "%Y-%m-%d %H:%M:%S");
        fh.write_all(format!("{} {}\n", stamp, msg).as_bytes())
    })();
}

/// What a plan leaves to be done: changes in waiting, and routine refreshes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pending {
    /// Changes that are waiting to be made.
    pub changes: usize,
    /// Copies of sessions in use that the running daemon brings up to date once a minute:
    /// nothing is wrong or waiting when these are due.
    pub refreshes: usize,
}

impl Pending {
    /// `in sync`, or how many changes are pending.
    pub fn line(&self) -> String {
        let refreshes = match self.refreshes {
            0 => String::new(),
            1 => String::from("1 refresh of a session in use is due within a minute"),
            n => format!("{} refreshes of sessions in use are due within a minute", n),
        };
        match (self.changes, self.refreshes) {
            (0, 0) => String::from("in sync"),
            (0, _) => format!("in sync ({})", refreshes),
            (n, 0) => format!("{} change(s) pending", n),
            (n, _) => format!("{} change(s) pending (and {})", n, refreshes),
        }
    }
}

/// Print what a pass would do. `daemon_running` says whether a daemon is at work: then the
/// refreshes it makes once a minute are not counted as changes in waiting.
pub fn preview(
    s: &mut Sync,
    con: &Console,
    verbose: bool,
    conflicts: bool,
    daemon_running: bool,
) -> Pending {
    let plan = s.plan();
    print_plan(s.days, &plan, con, verbose, conflicts, daemon_running)
}

fn print_plan(
    days: f64,
    plan: &Plan,
    con: &Console,
    verbose: bool,
    conflicts: bool,
    daemon_running: bool,
) -> Pending {
    let (scan, acts, notes) = (&plan.scan, &plan.acts, &plan.notes);
    let refreshes = if daemon_running {
        acts.iter().filter(|a| a.refresh).count()
    } else {
        0
    };
    let pending = Pending {
        changes: acts.len() - refreshes,
        refreshes,
    };
    con.say(&format!(
        "account folders: {}   sessions: {}   pending changes: {}{}",
        scan.lv.len(),
        scan.records.len(),
        pending.changes,
        match refreshes {
            0 => String::new(),
            n => format!(
                "   refreshes of sessions in use, due within a minute: {}",
                n
            ),
        }
    ));
    for (name, _) in &scan.lv {
        let count = |k: Kind| {
            acts.iter()
                .filter(|a| &a.leaf == name && a.kind == k)
                .count()
        };
        let have = scan
            .records
            .values()
            .filter(|copies| copy_in(copies, name).is_some())
            .count();
        con.say(&format!(
            "  {}  has {} | add {}, update {}, repair {}, remove {}",
            short(name),
            have,
            count(Kind::Add),
            count(Kind::Update),
            count(Kind::Repair),
            count(Kind::Quarantine)
        ));
    }
    con.say(&format!(
        "in every account folder the app really uses{}: {} of {} sessions",
        if acts.is_empty() {
            ""
        } else {
            " once the pending changes are made"
        },
        notes.everywhere,
        scan.records.len()
    ));
    con.say(&format!(
        "left out of some folder: archived and older than {} days {}, transcript missing {}, deleted in that account {}, \
         deleted elsewhere after last use {}, same conversation already there {}, unreadable file there {}, \
         just removed {}, retrying later {}",
        days,
        notes.old,
        notes.no_transcript,
        notes.deleted_there.len(),
        notes.deleted_elsewhere.len(),
        notes.same_conversation,
        notes.unreadable_there,
        notes.just_removed,
        notes.retry_later
    ));
    con.say(&format!(
        "not changed for now: held while the app uses that account {}, conflicts (order in doubt, both kept) {}{}",
        notes.held.len(),
        notes.conflicts.len(),
        if notes.unlisted.is_empty() {
            String::new()
        } else {
            format!(", folders that could not be listed: {}", notes.unlisted.join(", "))
        }
    ));
    if verbose || conflicts {
        for (name, title, why, fid) in &notes.conflicts {
            con.say(&format!(
                "  conflict   -> {}  {}  {}  ({})",
                short(name),
                fid.chars().take(8).collect::<String>(),
                title,
                why
            ));
        }
        if !notes.conflicts.is_empty() {
            con.say("  (a conflict ends when the session is used again, or with: resolve <id>, resolve --all)");
        }
        let mut titles: Vec<&String> = notes.deleted_elsewhere.iter().map(|d| &d.1).collect();
        titles.sort();
        titles.dedup();
        for title in titles {
            con.say(&format!(
                "  deleted in another account after its last use, not handed to further accounts:  {}",
                title
            ));
        }
    }
    if verbose {
        for a in acts {
            let r = a.rec();
            con.say(&format!(
                "  {:<10} -> {}  {}{}",
                a.kind.name(),
                short(&a.leaf),
                r.title,
                if r.archived { "  [archived]" } else { "" }
            ));
        }
        for (name, title) in &notes.held {
            con.say(&format!("  held       -> {}  {}", short(name), title));
        }
        let mut there: Vec<&(String, String)> = notes.deleted_there.iter().collect();
        there.sort();
        there.dedup();
        for (name, title) in there {
            con.say(&format!(
                "  deleted in {}, not added there  {}",
                short(name),
                title
            ));
        }
    }
    pending
}

fn when(v: Option<&Value>) -> String {
    match num(v) {
        Some(t) if t != 0.0 => winfs::local_format(t, "%Y-%m-%d %H:%M:%S"),
        _ => String::from("-"),
    }
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn shown(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::from("None"),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

fn count(n: usize, one: &str) -> String {
    format!("{} {}{}", n, one, if n == 1 { "" } else { "s" })
}

/// What to say when the app's session store is not where it was looked for. Nothing if it
/// is there.
pub fn store_notice(env: &Env, paths: &Paths) -> Vec<String> {
    if paths.root.is_dir() {
        return Vec::new();
    }
    let mut lines = vec![String::from(
        "The Claude app's session store was not found. Looked for:",
    )];
    let looked = match &env.store_override {
        Some(named) => vec![named.clone()],
        None => crate::paths::store_candidates(env),
    };
    for place in looked {
        lines.push(format!("  {}", place.display()));
    }
    lines.push(String::from(
        "If the app keeps its claude-code-sessions folder somewhere else, name that folder in the",
    ));
    lines.push(String::from(
        "environment variable CLAUDE_SESSION_SYNC_STORE. (If the Code tab of the Claude desktop app",
    ));
    lines.push(String::from(
        "has never been used on this PC, there is nothing to sync yet.)",
    ));
    lines
}

/// Is the daemon alive, what is pending. 0 = running and not stalled.
pub fn status(s: &mut Sync, con: &Console, sys: &mut dyn System, env: &Env) -> io::Result<i32> {
    let home = s.p.home.clone();
    let running = match acquire_lock(&home)? {
        Some(lock) => {
            drop(lock);
            false
        }
        None => true,
    };
    let st = match fs::read(home.join("status.json"))
        .ok()
        .and_then(|d| serde_json::from_slice::<Value>(&d).ok())
    {
        Some(Value::Object(m)) => m,
        _ => serde_json::Map::new(),
    };
    let autostart = install::autostart(sys, env.localappdata.as_deref());
    let paused = autostart == (Autostart::Task { paused: true });
    let version = st
        .get("version")
        .map_or_else(|| String::from("?"), |v| shown(Some(v)));
    let beat = num(st.get("heartbeat")).filter(|b| *b != 0.0);
    let age = beat.map(|b| winfs::real_now() - b);
    // it reports every minute, whatever its settings
    let stalled = running && age.is_some_and(|a| a > 660.0);
    if running {
        // look with the eyes of the daemon that is at work
        s.delete_sync = truthy(st.get("delete_sync"));
        if let Some(days) = num(st.get("days")) {
            s.days = days;
        }
    }
    let plan = s.plan();

    // the summary: one line that says whether all is well
    let (word, daemon) = if stalled {
        (
            "STALLED",
            format!(
                "daemon {} has not reported for {} s",
                version,
                age.unwrap_or(0.0) as i64
            ),
        )
    } else if running {
        ("OK", format!("daemon {} running", version))
    } else if paused {
        (
            "PAUSED",
            String::from("daemon stopped until you run: claude-session-sync resume"),
        )
    } else {
        ("NOT RUNNING", String::from("no daemon"))
    };
    let failed = if running {
        num(st.get("failed")).unwrap_or(0.0) as usize
    } else {
        0
    };
    let errors = failed.max(usize::from(running && truthy(st.get("last_error"))));
    con.say(&format!(
        "{}  {} | {} of {} sessions in every account | {} | {}",
        word,
        daemon,
        plan.notes.everywhere,
        plan.scan.records.len(),
        count(plan.notes.conflicts.len(), "conflict"),
        count(errors, "error")
    ));
    con.say(&match autostart {
        Autostart::Task { paused: false } => format!("autostart: installed (scheduled task \"{}\")", TASK_NAME),
        Autostart::Task { paused: true } => {
            format!("autostart: installed, PAUSED (scheduled task \"{}\" is disabled; resume enables it)", TASK_NAME)
        }
        Autostart::StartupEntry => String::from("autostart: installed (start-up entry only: not restarted if it stops)"),
        Autostart::NotInstalled => String::from("autostart: not installed (to install: claude-session-sync install)"),
        Autostart::NoTaskSeen => String::from(
            "autostart: no scheduled task (a start-up entry, if any, cannot be seen from inside the Claude app)",
        ),
        Autostart::Unknown => String::from("autostart: unknown (Task Scheduler could not be asked)"),
    });
    con.say("");
    for line in store_notice(env, &s.p) {
        con.say(&line);
    }

    if running {
        let pid = st
            .get("pid")
            .map_or_else(|| String::from("?"), |v| shown(Some(v)));
        con.say(&format!(
            "daemon: running (pid {}, version {}{})",
            pid,
            version,
            if truthy(st.get("delete_sync")) {
                ", deletions carried over"
            } else {
                ""
            }
        ));
    } else {
        con.say("daemon: NOT running");
    }
    con.say(&format!(
        "last heartbeat: {}{}   last full pass: {}   last change: {}",
        when(st.get("heartbeat")),
        age.map_or_else(String::new, |a| format!(
            " ({} s ago{})",
            a as i64,
            if stalled { ", STALLED?" } else { "" }
        )),
        when(st.get("last_full")),
        when(st.get("last_change"))
    ));
    if running
        && ["waiting", "failed", "retrying", "last_error"]
            .iter()
            .any(|k| truthy(st.get(*k)))
    {
        con.say(&format!(
            "daemon's last pass: waiting {}, failed {}, retrying later {}, last error: {}",
            shown(st.get("waiting")),
            shown(st.get("failed")),
            shown(st.get("retrying")),
            shown(st.get("last_error"))
        ));
    }
    let pending = print_plan(s.days, &plan, con, false, true, running && !stalled);
    con.say(&pending.line());
    // What --delete-sync took out of the other accounts lately, and has not been brought back.
    let week_ago = winfs::real_now() - 7.0 * 86400.0;
    let journal = journal_entries(&home);
    let brought_back = |fid: &str, after: f64| {
        journal.iter().any(|e| {
            e.get("kind").and_then(Value::as_str) == Some("restore")
                && e.get("fid").and_then(Value::as_str) == Some(fid)
                && num(e.get("t")).unwrap_or(0.0) >= after
        })
    };
    let gone: Vec<_> = journal
        .iter()
        .filter(|e| {
            let t = num(e.get("t")).unwrap_or(0.0);
            e.get("kind").and_then(Value::as_str) == Some("quarantine")
                && t > week_ago
                && !brought_back(e.get("fid").and_then(Value::as_str).unwrap_or(""), t)
                // (nor one that `undo` has put back: its file is in quarantine no longer)
                && e.get("q").and_then(Value::as_str).is_some_and(|q| std::path::Path::new(q).exists())
        })
        .collect();
    if !gone.is_empty() {
        con.say("removed from other accounts in the last 7 days (bring back with: restore <id>):");
        for e in &gone[gone.len().saturating_sub(15)..] {
            con.say(&format!(
                "  {}  {}  {}  {}",
                when(e.get("t")),
                shown(e.get("fid")).chars().take(8).collect::<String>(),
                short(&shown(e.get("leaf"))),
                e.get("title").and_then(Value::as_str).unwrap_or("")
            ));
        }
    }
    let tail = log_tail(&home, 5);
    if !tail.is_empty() {
        con.say("last log lines (more with: claude-session-sync log):");
        for line in tail {
            con.say(&format!("  {}", line));
        }
    }
    Ok(if running && !stalled { 0 } else { 1 })
}

/// The last `n` lines of the log (the rotated file is read too if the current one is short).
pub fn log_tail(home: &std::path::Path, n: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for name in ["sync.log.1", "sync.log"] {
        if let Ok(data) = fs::read(home.join(name)) {
            lines.extend(
                String::from_utf8_lossy(&data)
                    .lines()
                    .map(|l| l.trim_end().to_string()),
            );
        }
    }
    lines.split_off(lines.len().saturating_sub(n))
}

fn real_sleep(seconds: f64) {
    if seconds > 0.0 && seconds.is_finite() {
        std::thread::sleep(std::time::Duration::from_secs_f64(seconds));
    }
}

/// Run one command line against the given environment. Returns the exit code.
pub fn run(args: &[String], env: &Env, con: &Console) -> i32 {
    run_with(args, env, con, &mut RealSystem)
}

/// As `run`, with the system's tools (Task Scheduler, the registry) given by the caller.
pub fn run_with(args: &[String], env: &Env, con: &Console, sys: &mut dyn System) -> i32 {
    let parsed = match parse_args(args) {
        Ok(p) => p,
        Err(e) => {
            // the program without a console has nowhere to say it: leave a trace in the log
            if con.windowless {
                if let Ok(paths) = Paths::discover(env) {
                    log_line(&paths, &format!("bad arguments, not started: {:?}", args));
                }
            }
            con.complain(&format!("claude-session-sync: {}", e));
            con.complain("For the commands and options:  claude-session-sync --help");
            return 2;
        }
    };
    match parsed.command {
        Command::Help => {
            con.say(&help());
            return 0;
        }
        Command::Version => {
            con.say(VERSION);
            return 0;
        }
        Command::PathSetup { add } => {
            return match std::env::current_exe() {
                Ok(exe) => install::path_command(sys, &exe, add),
                Err(_) => 1,
            };
        }
        _ => {}
    }
    let paths = match Paths::discover(env) {
        Ok(p) => p,
        Err(e) => {
            con.complain(&format!("claude-session-sync: {}", e));
            return 1;
        }
    };
    match execute(&parsed, env, paths, con, sys) {
        Ok(code) => code,
        Err(e) => {
            con.complain(&format!("claude-session-sync: {}", e));
            1
        }
    }
}

fn execute(
    parsed: &Args,
    env: &Env,
    paths: Paths,
    con: &Console,
    sys: &mut dyn System,
) -> Result<i32, String> {
    let io_err = |e: io::Error| e.to_string();
    let home = paths.home.clone();
    if let Command::Log { lines } = &parsed.command {
        // (before a `Sync` is made: looking at the log reads nothing else)
        let tail = log_tail(&home, *lines);
        if tail.is_empty() && !home.join("sync.log").exists() {
            con.say(&format!("no log yet ({})", home.join("sync.log").display()));
        }
        for line in tail {
            con.say(&line);
        }
        return Ok(0);
    }
    let mut s = Sync::new(paths.clone());
    // What is not given is what the daemon was installed with, so that a look from the command
    // line sees what the daemon sees. (The daemon itself goes by its own arguments alone.)
    let installed = match parsed.command {
        Command::Daemon { .. } => None,
        _ => install::load_settings(&home),
    };
    s.days = parsed
        .days
        .or(installed.as_ref().map(|i| i.days))
        .unwrap_or(30) as f64;
    s.delete_sync = parsed
        .delete_sync
        .or(installed.as_ref().map(|i| i.delete_sync))
        .unwrap_or(false);
    let sink = con.clone();
    s.out = Box::new(move |line: &str| sink.say(line));
    match &parsed.command {
        Command::Help | Command::Version | Command::Log { .. } | Command::PathSetup { .. } => Ok(0),
        Command::Preview { verbose } => {
            for line in store_notice(env, &paths) {
                con.say(&line);
            }
            let running = crate::daemon::is_running(&home);
            preview(&mut s, con, *verbose, false, running);
            Ok(0)
        }
        Command::Status => status(&mut s, con, sys, env).map_err(io_err),
        Command::Stop => {
            if install::stop_daemon(&home, 40, &mut real_sleep).map_err(io_err)? {
                con.say("stopped");
                Ok(0)
            } else {
                con.say("still running; it will stop within a few seconds");
                Ok(1)
            }
        }
        Command::Resolve { id } => {
            let (asked, code) = match id {
                Some(id) => {
                    let found = s.resolve(id).map_err(io_err)?;
                    let code = if found.is_some() { 0 } else { 1 };
                    (found.into_iter().count(), code)
                }
                None => {
                    let found = s.resolve_all().map_err(io_err)?;
                    con.say(&match found.len() {
                        0 => String::from("there are no conflicts to settle"),
                        n => format!("{} to be settled", count(n, "conflict")),
                    });
                    (found.len(), 0)
                }
            };
            if asked > 0 {
                con.say(if crate::daemon::is_running(&home) {
                    "the daemon carries it out on its next pass (while the app has that account open, when it leaves it)"
                } else {
                    "no daemon is running: run `claude-session-sync sync` (or start the daemon) to carry it out"
                });
            }
            Ok(code)
        }
        Command::Restore { id } => Ok(if s.restore(id).map_err(io_err)?.is_some() {
            0
        } else {
            1
        }),
        Command::Sync | Command::Daemon { .. } | Command::Undo { .. } => {
            let lock = match &parsed.command {
                Command::Daemon { .. } => lock_for_daemon(&mut s, &real_sleep),
                _ => acquire_lock(&home),
            }
            .map_err(io_err)?;
            let Some(lock) = lock else {
                con.say(
                    "the daemon is running: it will pick changes up by itself. To undo, run \"claude-session-sync uninstall\" first.",
                );
                return Ok(if matches!(parsed.command, Command::Undo { .. }) {
                    1
                } else {
                    0
                });
            };
            let result = match &parsed.command {
                Command::Undo { since, force } => {
                    // The app keeps its session list in memory and writes it back: with it
                    // running, what undo puts right may not stay. Not without being told to.
                    let app = match sys.app_running_override() {
                        Some(known) => known,
                        None => s.app_running(),
                    };
                    if app && !*force {
                        con.say("The Claude app is running: it may write its own list back over what undo puts right.");
                        con.say("Nothing was undone. Close the app completely (tray icon too) and run this again,");
                        con.say("or add --force to go ahead all the same.");
                        Ok(1)
                    } else {
                        if app {
                            con.say("The Claude app is running: it may write its own list back over what undo puts right.");
                            con.say("Going ahead all the same (--force).");
                        }
                        s.undo(*since).map(|_| 0).map_err(io_err)
                    }
                }
                Command::Daemon { interval, quick } => {
                    s.watch(
                        (*interval).max(60) as f64,
                        (*quick).max(0) as f64,
                        crate::consts::POLL_SEC,
                        None,
                        &mut real_sleep,
                    );
                    Ok(0)
                }
                _ => {
                    let done = s.run();
                    s.log(&format!(
                        "one-off: added {}, updated {}, repaired {}, aligned {}, removed {}, waiting {}, failed {}",
                        done.add, done.update, done.repair, done.aligned, done.quarantine, done.deferred, done.failed
                    ));
                    // a pass that could not be made, or that left items failed, is not a success
                    Ok(if s.aborted || done.failed > 0 { 1 } else { 0 })
                }
            };
            drop(lock);
            result
        }
        Command::Install { .. } | Command::Uninstall { .. } | Command::Pause | Command::Resume => {
            let current_exe = std::env::current_exe().map_err(io_err)?;
            let sink = con.clone();
            let mut say = move |line: &str| sink.say(line);
            let mut setup = Setup {
                env,
                paths: &paths,
                current_exe,
                sys,
                say: &mut say,
            };
            match &parsed.command {
                Command::Install { interval, quick } => setup.install(&Given {
                    interval: *interval,
                    quick: *quick,
                    days: parsed.days,
                    delete_sync: parsed.delete_sync,
                })?,
                Command::Uninstall { purge } => setup.uninstall(*purge)?,
                Command::Pause => setup.pause()?,
                _ => setup.resume()?,
            }
            Ok(0)
        }
    }
}

/// The whole program: both executables call this. `windowless` is true for the one without
/// a console window.
pub fn process_main(windowless: bool) -> ! {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let env = Env::from_process();
    let console = Console {
        windowless,
        ..Console::stdio()
    };
    let outcome = catch_unwind(AssertUnwindSafe(|| run(&args, &env, &console)));
    let code = match outcome {
        Ok(code) => code,
        Err(payload) => {
            // without a console a crash would otherwise leave no trace at all
            let what = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| String::from("unknown error"));
            if let Ok(paths) = Paths::discover(&env) {
                log_line(&paths, &format!("stopped by an error: {}", what));
            }
            1
        }
    };
    std::process::exit(code)
}
