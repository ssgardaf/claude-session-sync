//! The command line's arguments.

use crate::cli::{help, parse_args, Args, Command};
use crate::winfs;

fn parse(args: &[&str]) -> Result<Args, String> {
    parse_args(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>())
}

fn command(args: &[&str]) -> Command {
    parse(args).unwrap().command
}

#[test]
fn no_command_is_a_preview() {
    assert_eq!(
        parse(&[]).unwrap(),
        Args {
            command: Command::Preview { verbose: false },
            days: 30,
            delete_sync: false
        }
    );
    assert_eq!(command(&["--verbose"]), Command::Preview { verbose: true });
    assert_eq!(
        command(&["preview", "--verbose"]),
        Command::Preview { verbose: true }
    );
}

#[test]
fn every_command_is_recognised() {
    assert_eq!(command(&["sync"]), Command::Sync);
    assert_eq!(
        command(&["daemon"]),
        Command::Daemon {
            interval: 3600,
            quick: 10
        }
    );
    assert_eq!(
        command(&["daemon", "--interval", "60", "--quick", "2"]),
        Command::Daemon {
            interval: 60,
            quick: 2
        }
    );
    assert_eq!(
        command(&["daemon", "--interval=120", "--quick=0"]),
        Command::Daemon {
            interval: 120,
            quick: 0
        }
    );
    assert_eq!(command(&["status"]), Command::Status);
    assert_eq!(command(&["stop"]), Command::Stop);
    assert_eq!(
        command(&["resolve", "abcdef"]),
        Command::Resolve {
            id: "abcdef".into()
        }
    );
    assert_eq!(
        command(&["restore", "abcdef"]),
        Command::Restore {
            id: "abcdef".into()
        }
    );
    assert_eq!(
        command(&["install"]),
        Command::Install {
            interval: 3600,
            quick: 10
        }
    );
    assert_eq!(
        command(&["install", "--quick", "0"]),
        Command::Install {
            interval: 3600,
            quick: 0
        }
    );
    assert_eq!(command(&["uninstall"]), Command::Uninstall { purge: false });
    assert_eq!(
        command(&["uninstall", "--purge"]),
        Command::Uninstall { purge: true }
    );
    assert_eq!(command(&["pause"]), Command::Pause);
    assert_eq!(command(&["resume"]), Command::Resume);
    assert_eq!(command(&["log"]), Command::Log { lines: 30 });
    assert_eq!(command(&["log", "-n", "5"]), Command::Log { lines: 5 });
    assert_eq!(command(&["log", "-n", "-5"]), Command::Log { lines: 0 });
    assert_eq!(command(&["--version"]), Command::Version);
    assert_eq!(command(&["status", "--version"]), Command::Version);
    assert_eq!(command(&["--help"]), Command::Help);
    assert_eq!(command(&["daemon", "-h"]), Command::Help);
    assert_eq!(command(&["help"]), Command::Help);
}

#[test]
fn common_options() {
    for cmd in ["preview", "sync", "daemon", "status", "install"] {
        let a = parse(&[cmd, "--days", "60", "--delete-sync"]).unwrap();
        assert_eq!((a.days, a.delete_sync), (60, true), "{}", cmd);
    }
    let a = parse(&["--delete-sync", "--days=7"]).unwrap();
    assert_eq!(
        (a.command, a.days, a.delete_sync),
        (Command::Preview { verbose: false }, 7, true)
    );
    let a = parse(&["sync"]).unwrap();
    assert_eq!((a.days, a.delete_sync), (30, false));
}

#[test]
fn undo_takes_a_local_time() {
    let Command::Undo { since } = command(&["undo", "--since", "2026-11-11 10:30"]) else {
        panic!("not an undo");
    };
    assert_eq!(
        winfs::local_format(since, "%Y-%m-%d %H:%M:%S"),
        "2026-11-11 10:30:00"
    );
    let Command::Undo { since } = command(&["undo", "--since=2026-11-11 10:30:15"]) else {
        panic!("not an undo");
    };
    assert_eq!(
        winfs::local_format(since, "%Y-%m-%d %H:%M:%S"),
        "2026-11-11 10:30:15"
    );
    let Command::Undo { since } = command(&["undo", "--since", "2026-11-11"]) else {
        panic!("not an undo");
    };
    assert_eq!(
        winfs::local_format(since, "%Y-%m-%d %H:%M:%S"),
        "2026-11-11 00:00:00"
    );
    assert!(parse(&["undo", "--since", "last tuesday"])
        .unwrap_err()
        .contains("bad --since"));
    assert!(parse(&["undo"]).unwrap_err().contains("--since"));
}

#[test]
fn what_is_refused() {
    for bad in [
        &["frobnicate"][..],
        &["--no-such-option"],
        &["sync", "--verbose"],
        &["stop", "--days", "3"],
        &["daemon", "--interval"],
        &["daemon", "--interval", "soon"],
        &["daemon", "extra"],
        &["resolve"],
        &["restore"],
        &["resolve", "a", "b"],
        &["preview", "--delete-sync=yes"],
        &["uninstall", "--delete-sync"],
        &["install", "--purge"],
        &["uninstall", "--purge=yes"],
        &["log", "-n"],
        &["log", "-n", "many"],
        &["log", "30"],
        &["status", "-n", "5"],
        &["pause", "--days", "3"],
        &["resume", "now"],
        &["undo", "--since", "2026-11-11 10:30", "--days", "3"],
    ] {
        assert!(parse(bad).is_err(), "{:?}", bad);
    }
}

#[test]
fn help_names_every_command() {
    let text = help();
    assert!(text.starts_with(&format!("claude-session-sync {}\n", crate::consts::VERSION)));
    // every command has a line of its own that says what it does
    for name in [
        "preview",
        "sync",
        "install",
        "status",
        "log",
        "pause",
        "resume",
        "uninstall",
        "resolve ID",
        "restore ID",
        "undo",
        "stop",
        "daemon",
    ] {
        let line = text
            .lines()
            .find(|l| l.starts_with(&format!("    {} ", name)));
        assert!(line.is_some_and(|l| l.len() > 30), "{}", name);
        // ...and it is a command
        let word = name.split(' ').next().unwrap();
        assert!(
            !parse_args(&[word.to_string(), "x".into(), "--since=2026-01-01".into()])
                .is_err_and(|e| e.contains("unknown command"))
        );
    }
    for word in [
        "--interval",
        "--quick",
        "--days",
        "--delete-sync",
        "--since",
        "--verbose",
        "--purge",
        "-n N",
        "--version",
        "--help",
        "EXAMPLES",
        "WHERE THINGS ARE",
        "%USERPROFILE%\\.claude-session-sync\\",
        "sync.log",
        "backups\\",
        "quarantine\\",
        "replaced\\",
        "CLAUDE_SESSION_SYNC_HOME",
        "CLAUDE_SESSION_SYNC_STORE",
        "CLAUDE_CONFIG_DIR",
    ] {
        assert!(text.contains(word), "{}", word);
    }
    // the examples are commands that are understood as written
    let examples: Vec<&str> = text
        .lines()
        .filter(|l| l.starts_with("    claude-session-sync ") && !l.contains('<'))
        .collect();
    assert!(examples.len() >= 4);
    for line in examples {
        let args: Vec<String> = line
            .trim()
            .split(' ')
            .skip(1)
            .map(|a| a.replace('"', ""))
            .collect();
        let mut joined: Vec<String> = Vec::new();
        for a in args {
            // (the one quoted argument, a date and a time, was split at its space)
            match joined.last_mut() {
                Some(last) if last.starts_with("20") && a.contains(':') => {
                    *last = format!("{} {}", last, a)
                }
                _ => joined.push(a),
            }
        }
        assert!(parse_args(&joined).is_ok(), "{}", line);
    }
    // it fits a terminal
    assert!(
        text.lines().all(|l| l.chars().count() <= 80),
        "{:?}",
        text.lines().find(|l| l.chars().count() > 80)
    );
}
