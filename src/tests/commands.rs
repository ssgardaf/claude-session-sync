//! What the command line does and says that is the same on every system: the settings it
//! looks with, what `status` and `resolve` report, what `undo` asks for. The system's tools
//! are the recorder of this system's setup tests; nothing real is asked or changed.

use serde_json::json;

#[cfg(windows)]
use super::setup::{cli, signed_out, Recorder};
#[cfg(unix)]
use super::setup_unix::{cli, signed_out, Recorder};
use super::*;
use crate::consts::VERSION;
use crate::daemon::acquire_lock;
use crate::install::{save_settings, Settings};

#[test]
fn command_line_looks_with_the_settings_the_daemon_was_installed_with() {
    let b = Base::new();
    signed_out(&b);
    b.install(2.0 * HOUR, 0.0);
    b.rec(B, "s1", r().cli("c1").last(HOUR));
    b.rec(C, "own", r());
    b.delete(A, "s1", 600.0, Some("c1")); // with deletions carried over, s1 is to leave B
    let mut sys = Recorder::new();
    // nothing installed: the defaults
    let (_, out) = cli(&b, &mut sys, &["preview"]);
    assert!(out.iter().any(|l| l.contains("older than 30 days")));
    assert!(!out.iter().any(|l| l.ends_with("remove 1")));
    // installed with 90 days and deletions carried over: preview and status see it so
    save_settings(
        &b.home,
        &Settings {
            days: 90,
            delete_sync: true,
            ..Settings::default()
        },
    )
    .unwrap();
    for what in ["preview", "status"] {
        let (_, out) = cli(&b, &mut sys, &[what]);
        assert!(
            out.iter().any(|l| l.contains("older than 90 days")),
            "{}",
            what
        );
        assert!(out.iter().any(|l| l.ends_with("remove 1")), "{}", what);
    }
    // what is given on the command line counts for that command
    let (_, out) = cli(
        &b,
        &mut sys,
        &["preview", "--days", "5", "--no-delete-sync"],
    );
    assert!(out.iter().any(|l| l.contains("older than 5 days")));
    assert!(!out.iter().any(|l| l.ends_with("remove 1")));
    // a daemon at work is believed over the record: it reports what it runs with
    let held = acquire_lock(&b.home).unwrap().unwrap();
    let st = json!({"pid": 4242, "version": VERSION, "heartbeat": now() as i64, "delete_sync": false, "days": 7});
    fs::write(b.home.join("status.json"), st.to_string()).unwrap();
    let (_, out) = cli(&b, &mut sys, &["status"]);
    assert!(out.iter().any(|l| l.contains("older than 7 days")));
    assert!(!out.iter().any(|l| l.ends_with("remove 1")));
    drop(held);
    // and sync does what the daemon would
    let (code, out) = cli(&b, &mut sys, &["sync"]);
    assert_eq!(code, 0);
    assert!(out.iter().any(|l| l.contains("removed 1")), "{:?}", out);
    assert!(!b.has(B, "s1"));
}

// ---------------------------------------------------------------------- what status and resolve say

#[test]
fn refreshes_of_sessions_in_use_do_not_read_as_changes_in_waiting() {
    let b = Base::new();
    b.rec(A, "s1", r().last(600.0).mtime(300.0));
    b.s().run(); // B and C hold copies this tool placed
    b.rec(
        A,
        "s1",
        r().last(200.0).mtime(100.0).title("in use, saved again"),
    );
    let mut sys = Recorder::installed();
    // no daemon: the two copies are waiting for someone to bring them up to date
    let (_, out) = cli(&b, &mut sys, &["status"]);
    assert!(out.iter().any(|l| l == "2 change(s) pending"), "{:?}", out);
    let (_, out) = cli(&b, &mut sys, &["preview"]);
    assert!(out[0].ends_with("pending changes: 2"));
    // a daemon is at work: it refreshes such copies once a minute, and nothing is amiss
    let held = acquire_lock(&b.home).unwrap().unwrap();
    let st = json!({"pid": 4242, "version": VERSION, "heartbeat": now() as i64});
    fs::write(b.home.join("status.json"), st.to_string()).unwrap();
    let (code, out) = cli(&b, &mut sys, &["status"]);
    assert_eq!(code, 0);
    assert!(out[0].starts_with("OK  "));
    assert!(
        out.iter()
            .any(|l| l == "in sync (2 refreshes of sessions in use are due within a minute)"),
        "{:?}",
        out
    );
    let (_, out) = cli(&b, &mut sys, &["preview"]);
    assert_eq!(
        out[0],
        "account folders: 3   sessions: 1   pending changes: 0   refreshes of sessions in use, due within a minute: 2"
    );
    // a change that is really waiting is still counted as one
    b.rec(A, "s2", r());
    let (_, out) = cli(&b, &mut sys, &["status"]);
    assert!(
        out.iter().any(|l| l
            == "2 change(s) pending (and 2 refreshes of sessions in use are due within a minute)"),
        "{:?}",
        out
    );
    drop(held);
    assert_eq!(
        listdir(&b.home)
            .iter()
            .filter(|n| n.as_str() == "daemon.lock")
            .count(),
        1
    );
    // looking does not make a lock file where there is none
    let b = Base::new();
    b.rec(A, "s1", r());
    cli(&b, &mut sys, &["preview"]);
    assert!(!b.home.exists());
}

#[test]
fn session_that_was_brought_back_is_no_longer_listed_as_removed() {
    let b = Base::new();
    b.install(2.0 * HOUR, 0.0);
    for fid in ["abcdef-1", "ghijkl-2"] {
        b.rec(B, fid, r().cli(&format!("c-{}", fid)).last(HOUR).title(fid));
        b.delete(A, fid, 600.0, Some(&format!("c-{}", fid)));
    }
    b.s().run();
    let mut sys = Recorder::new();
    let listed = |out: &[String]| -> Vec<String> {
        let at = out
            .iter()
            .position(|l| l.starts_with("removed from other accounts"));
        let mut ids: Vec<String> = match at {
            Some(at) => out[at + 1..]
                .iter()
                .take_while(|l| l.starts_with("  "))
                .map(|l| l.split_whitespace().nth(2).unwrap().to_string())
                .collect(),
            None => Vec::new(),
        };
        ids.sort();
        ids
    };
    let (_, out) = cli(&b, &mut sys, &["status", "--delete-sync"]);
    assert_eq!(listed(&out), vec!["abcdef-1", "ghijkl-2"]);
    assert_eq!(cli(&b, &mut sys, &["restore", "abcdef"]).0, 0);
    let (_, out) = cli(&b, &mut sys, &["status", "--delete-sync"]);
    assert_eq!(listed(&out), vec!["ghijkl-2"]);
    assert_eq!(cli(&b, &mut sys, &["restore", "ghijkl"]).0, 0);
    let (_, out) = cli(&b, &mut sys, &["status", "--delete-sync"]);
    assert!(listed(&out).is_empty() && !out.iter().any(|l| l.starts_with("removed from")));
}

#[test]
fn resolve_says_who_carries_it_out() {
    let b = Base::new();
    for fid in ["abcdef-1", "abcdef-2"] {
        b.rec(
            A,
            fid,
            r().last(3.0 * DAY)
                .mtime(2.0 * DAY)
                .archived()
                .title("further along"),
        );
        b.rec(
            B,
            fid,
            r().last(5.0 * DAY)
                .mtime(DAY)
                .title("older, un-archived later"),
        );
    }
    let mut sys = Recorder::new();
    // no daemon: it is for the user to run a pass
    let (code, out) = cli(&b, &mut sys, &["resolve", "abcdef-1"]);
    assert_eq!(code, 0);
    assert!(out
        .last()
        .unwrap()
        .contains("run `claude-session-sync sync` (or start the daemon) to carry it out"));
    // too little to go by
    let (code, out) = cli(&b, &mut sys, &["resolve", "abc"]);
    assert_eq!(code, 1);
    assert!(out
        .iter()
        .any(|l| l.contains("at least its first 6 characters")));
    assert!(!out.iter().any(|l| l.contains("carry it out")));
    // a daemon is running: it will
    let held = acquire_lock(&b.home).unwrap().unwrap();
    let (code, out) = cli(&b, &mut sys, &["resolve", "--all"]);
    assert_eq!(code, 0);
    // (the one asked for above is a conflict no longer: it is waiting to be carried out)
    assert!(
        out.iter().any(|l| l == "1 conflict to be settled"),
        "{:?}",
        out
    );
    assert!(out
        .last()
        .unwrap()
        .starts_with("the daemon carries it out on its next pass"));
    drop(held);
    assert_eq!(b.state()["resolve"].as_object().unwrap().len(), 2);
    // nothing in conflict
    let b = Base::new();
    b.rec(A, "s1", r());
    let (code, out) = cli(&b, &mut sys, &["resolve", "--all"]);
    assert_eq!(
        (code, out),
        (0, vec![String::from("there are no conflicts to settle")])
    );
}

// ---------------------------------------------------------------------- undo

#[test]
fn undo_wants_force_while_the_app_is_running() {
    let b = Base::new();
    signed_out(&b);
    b.rec(A, "s1", r());
    b.rec(B, "own", r());
    let mut sys = Recorder::new();
    assert_eq!(cli(&b, &mut sys, &["sync"]).0, 0);
    assert!(b.has(B, "s1"));
    // the app is running: nothing is undone, and it is said why and what to do
    sys.app = true;
    let (code, out) = cli(&b, &mut sys, &["undo", "--since", "2000-01-01"]);
    assert_eq!(code, 1);
    assert!(out[0].starts_with("The Claude app is running"));
    assert!(
        out.iter().any(|l| l.contains("Nothing was undone"))
            && out.iter().any(|l| l.contains("--force"))
    );
    assert!(b.has(B, "s1"));
    // told to go ahead: it does, with the warning
    let (code, out) = cli(&b, &mut sys, &["undo", "--since", "2000-01-01", "--force"]);
    assert_eq!(code, 0);
    assert!(out[0].starts_with("The Claude app is running") && out[1].contains("--force"));
    assert!(out.iter().any(|l| l.contains("undo since")));
    assert!(!b.has(B, "s1"));
    // the app is not running: no warning, no need to force
    assert_eq!(cli(&b, &mut sys, &["sync"]).0, 0);
    sys.app = false;
    let (code, out) = cli(&b, &mut sys, &["undo", "--since", "2000-01-01"]);
    assert_eq!(code, 0);
    assert!(!out.iter().any(|l| l.contains("Claude app is running")));
    assert!(!b.has(B, "s1"));
    // a daemon at work comes first, as before
    let held = acquire_lock(&b.home).unwrap().unwrap();
    sys.app = true;
    let (code, out) = cli(&b, &mut sys, &["undo", "--since", "2000-01-01", "--force"]);
    assert_eq!(code, 1);
    assert!(out[0].starts_with("the daemon is running"));
    drop(held);
}

#[test]
fn deletion_that_undo_took_back_is_no_longer_listed_as_removed() {
    let b = Base::new();
    signed_out(&b);
    b.install(2.0 * HOUR, 0.0);
    b.rec(B, "abcdef-1", r().cli("c1").last(HOUR));
    b.delete(A, "abcdef-1", 600.0, Some("c1"));
    let mut sys = Recorder::new();
    let start = winfs::local_format(now() - 60.0, "%Y-%m-%d %H:%M:%S");
    assert_eq!(cli(&b, &mut sys, &["sync", "--delete-sync"]).0, 0);
    assert!(!b.has(B, "abcdef-1"));
    let (_, out) = cli(&b, &mut sys, &["status", "--delete-sync"]);
    assert!(out
        .iter()
        .any(|l| l.starts_with("removed from other accounts")));
    assert_eq!(cli(&b, &mut sys, &["undo", "--since", &start]).0, 0);
    assert!(b.has(B, "abcdef-1"));
    let (_, out) = cli(&b, &mut sys, &["status", "--delete-sync"]);
    assert!(
        !out.iter()
            .any(|l| l.starts_with("removed from other accounts")),
        "{:?}",
        out
    );
    assert!(out.iter().any(|l| l == "in sync")); // ...and it is not taken out again
}

#[test]
fn conflict_settled_by_a_one_off_sync_and_undone_is_a_conflict_again() {
    let b = Base::new();
    signed_out(&b);
    fs::remove_dir(b.dir(C)).unwrap(); // two accounts only: nothing but the conflict is in play
    b.rec(
        A,
        "abcdef-1",
        r().last(3.0 * DAY)
            .mtime(2.0 * DAY)
            .archived()
            .title("further along"),
    );
    b.rec(
        B,
        "abcdef-1",
        r().last(5.0 * DAY)
            .mtime(DAY)
            .title("older, un-archived later"),
    );
    let before = b.contents();
    let mut sys = Recorder::new();
    let start = winfs::local_format(now() - 60.0, "%Y-%m-%d %H:%M:%S");
    assert_eq!(cli(&b, &mut sys, &["resolve", "--all"]).0, 0);
    assert_eq!(cli(&b, &mut sys, &["sync"]).0, 0);
    assert_eq!(b.title(B, "abcdef-1"), "further along");
    assert_eq!(b.state()["resolve"], json!({})); // carried out, and closed in that very pass
    assert_eq!(cli(&b, &mut sys, &["undo", "--since", &start]).0, 0);
    assert_eq!(b.title(B, "abcdef-1"), "older, un-archived later");
    // it is a conflict again, nothing is pending, and another pass leaves it alone
    let (_, out) = cli(&b, &mut sys, &["status"]);
    assert!(out[0].contains("| 1 conflict |"), "{}", out[0]);
    assert!(out.iter().any(|l| l == "in sync"), "{:?}", out);
    assert_eq!(cli(&b, &mut sys, &["sync"]).0, 0);
    assert_eq!(b.title(B, "abcdef-1"), "older, un-archived later");
    assert_eq!(
        before.get(
            Path::new(B.0)
                .join(B.1)
                .join("local_abcdef-1.json")
                .as_path()
        ),
        b.contents().get(
            Path::new(B.0)
                .join(B.1)
                .join("local_abcdef-1.json")
                .as_path()
        )
    );
}
