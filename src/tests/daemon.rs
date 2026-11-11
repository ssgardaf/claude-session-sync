//! The daemon tests. The two that need a second real process
//! (`test_stalled_daemon_is_replaced_but_a_live_one_is_left_alone` and
//! `test_lock_is_honoured_between_real_processes`) are in `tests/processes.rs`.

use std::cell::Cell;

use serde_json::{json, Value};

use super::homes::at;
use super::*;
use crate::consts::POLL_SEC;
use crate::daemon::{acquire_lock, release_lock};

fn status(b: &Base) -> Value {
    serde_json::from_slice(&fs::read(b.home.join("status.json")).unwrap()).unwrap()
}

fn main(b: &Base, args: &[&str]) -> i32 {
    command_line(b, args).0
}

#[test]
fn test_single_instance_lock() {
    let b = Base::new();
    fs::create_dir_all(&b.home).unwrap();
    let a = acquire_lock(&b.home).unwrap();
    assert!(a.is_some());
    assert!(acquire_lock(&b.home).unwrap().is_none());
    release_lock(a.unwrap());
    let again = acquire_lock(&b.home).unwrap();
    assert!(again.is_some());
    release_lock(again.unwrap());
}

#[test]
fn test_fingerprint_tracks_records_and_markers_only() {
    let b = Base::new();
    let s = b.s();
    let f0 = s.fingerprint();
    b.rec(A, "s1", r());
    let f1 = s.fingerprint();
    assert_ne!(f0, f1);
    b.tomb(B, "x", ms(0.0));
    let f2 = s.fingerprint();
    assert_ne!(f1, f2);
    fs::write(b.dir(A).join("scheduled-tasks.json"), b"").unwrap();
    assert_eq!(f2, s.fingerprint());
}

#[test]
fn test_watch_syncs_on_change_writes_status_and_stops_on_request() {
    let b = Base::new();
    let mut s = b.s();
    b.rec(A, "s1", r());
    s.watch(3600.0, 1.0, POLL_SEC, Some(1), &mut |_| {});
    assert!(b.has(B, "s1"));
    let st = status(&b);
    assert_eq!(st["pid"], json!(std::process::id()));
    assert!(st["last_full"].as_i64().unwrap() > 0);
    assert_eq!(
        (
            &st["folders"],
            &st["sessions"],
            &st["failed"],
            &st["last_error"]
        ),
        (&json!(3), &json!(1), &json!(0), &Value::Null)
    );

    let mut calls = 0;
    let tree = b.clone();
    let mut nap = |_: f64| {
        calls += 1;
        if calls == 1 {
            tree.rec(A, "s2", r()); // a new session appears while the daemon sleeps
        } else if calls == 9 {
            fs::write(tree.home.join("stop.flag"), b"").unwrap();
        }
        std::thread::sleep(std::time::Duration::from_millis(400));
    };
    let mut s2 = b.s();
    s2.watch(3600.0, 1.0, 1.0, None, &mut nap); // returns only because of the stop flag
    assert!(b.has(C, "s2"));
    assert!(!b.home.join("stop.flag").exists());
    assert!(b.said("stopped on request") > 0);
}

#[test]
fn test_change_that_lands_during_a_pass_is_picked_up_by_the_next_check() {
    let b = Base::new();
    let mut s = b.s();
    s.watch(3600.0, 1.0, POLL_SEC, Some(1), &mut |_| {}); // first full pass: nothing to do
    let (tree, runs) = (b.clone(), Cell::new(0));
    s.hooks.after_run = Some(Box::new(move || {
        runs.set(runs.get() + 1);
        if runs.get() == 1 {
            tree.rec(A, "late", r()); // the app saves a new session while this pass is finishing
        }
    }));
    b.tomb(C, "unrelated", ms(0.0)); // something changes, so a pass starts
    FakeClock::at(&mut s, now() + 10.0);
    s.watch(3600.0, 1.0, POLL_SEC, Some(1), &mut |_| {}); // the pass during which "late" lands
    assert!(!b.has(B, "late"));
    s.watch(3600.0, 1.0, POLL_SEC, Some(1), &mut |_| {}); // a fresh watch always does a full pass
    assert!(b.has(B, "late"));
}

#[test]
fn test_baseline_is_what_was_on_disk_before_the_pass_looked() {
    let b = Base::new();
    let mut s = b.s();
    let (tree, seen) = (b.clone(), Cell::new(false));
    s.hooks.after_run = Some(Box::new(move || {
        if !seen.get() {
            tree.rec(A, "late", r());
            seen.set(true);
        }
    }));
    let mut naps = 0;
    let home = b.home.clone();
    let mut nap = |_: f64| {
        naps += 1;
        if naps >= 4 {
            fs::write(home.join("stop.flag"), b"").unwrap();
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    };
    s.watch(3600.0, 1.0, 1.0, None, &mut nap); // one process, several checks
    assert!(b.has(B, "late"));
}

#[test]
fn test_stop_flag_left_over_from_before_does_not_end_a_new_daemon() {
    let b = Base::new();
    fs::create_dir_all(&b.home).unwrap();
    fs::write(b.home.join("stop.flag"), b"").unwrap();
    b.rec(A, "s1", r());
    b.s().watch(3600.0, 1.0, POLL_SEC, Some(1), &mut |_| {});
    assert!(b.has(B, "s1"));
    assert_eq!(b.said("stopped on request"), 0);
}

#[test]
fn test_full_pass_only_mode_still_syncs_on_the_full_pass() {
    let b = Base::new();
    let mut s = b.s();
    s.watch(3600.0, 0.0, POLL_SEC, Some(1), &mut |_| {});
    b.rec(A, "s1", r());
    s.watch(3600.0, 0.0, POLL_SEC, Some(1), &mut |_| {}); // a fresh watch starts with a full pass
    assert!(b.has(B, "s1"));
}

#[test]
fn test_a_failing_pass_does_not_kill_the_daemon_and_shows_in_the_status() {
    let b = Base::new();
    let mut s = b.s();
    s.hooks.before_plan = Some(Box::new(|| panic!("boom")));
    s.watch(3600.0, 1.0, POLL_SEC, Some(2), &mut |_| {});
    assert_eq!(b.said("pass failed"), 1); // said once, not every time
    assert!(status(&b)["last_error"].as_str().unwrap().contains("boom"));
}

#[test]
fn test_cli_smoke() {
    // the command line uses the real defaults: the app's log says where it has been, and that it is signed out
    let b = Base::new();
    b.write_log(&[
        &at("09:00:00", Some(A)),
        &at("10:00:00", None),
        &at("10:00:05", Some(B)),
        &at("11:00:00", None),
    ]);
    b.rec(A, "s1", r());
    assert_eq!(main(&b, &["--version"]), 0);
    assert_eq!(main(&b, &["sync"]), 0);
    assert!(b.has(B, "s1"));
    assert!(!b.has(C, "s1")); // never signed in to
    assert_eq!(main(&b, &["status"]), 1); // no daemon in the test
    assert_eq!(main(&b, &["stop"]), 0);
    assert_eq!(main(&b, &["restore", "nothing"]), 1);
    assert_eq!(main(&b, &["resolve", "nothing"]), 1);
    assert_eq!(main(&b, &["undo", "--since", "2000-01-01 00:00"]), 0);
    assert!(!b.has(B, "s1"));
    assert_eq!(main(&b, &["undo", "--since", "last tuesday"]), 2);
}

#[test]
fn test_bad_arguments_without_a_console_are_logged_not_lost() {
    let b = Base::new();
    assert_eq!(main(&b, &["--no-such-option"]), 2);
    assert!(fs::read_to_string(b.home.join("sync.log"))
        .unwrap()
        .contains("bad arguments"));
}

#[test]
fn test_new_empty_account_folder_counts_as_a_change() {
    let b = Base::new();
    let s = b.s();
    let before = s.fingerprint();
    fs::create_dir_all(b.root.join(uid(0xd0)).join(uid(0xd1))).unwrap();
    assert_ne!(before, s.fingerprint());
}

#[test]
fn test_item_that_fails_backs_off_alone_and_the_rest_carry_on() {
    let b = Base::new();
    b.rec(A, "s1", r());
    fs::create_dir_all(b.path(B, "s1")).unwrap(); // a folder where B's record should be: cannot be mended
    set_mtime(&b.path(B, "s1"), now() - HOUR);
    let mut s = b.s();
    let done = s.run();
    assert_eq!((done.failed, done.add), (1, 1));
    assert!(b.has(C, "s1"));
    b.rec(A, "s2", r());
    assert_eq!(s.plan().notes.retry_later, 1); // not tried again yet...
    let done = s.run();
    assert_eq!((done.failed, done.add), (0, 2)); // ...and the new session is not held up by it
    assert!(!s.something_due());
    FakeClock::at(&mut s, now() + 700.0);
    assert!(s.something_due()); // after its own wait it is tried again
    assert_eq!(s.run().failed, 1);
    // said once
    let failed = b
        .lines
        .borrow()
        .iter()
        .filter(|l| {
            l.splitn(3, ' ')
                .nth(2)
                .is_some_and(|m| m.starts_with("failed"))
        })
        .count();
    assert_eq!(failed, 1);
}

#[test]
fn test_items_put_off_because_a_file_just_changed_do_not_slow_the_checks_down() {
    let b = Base::new();
    let src = b.rec(A, "busy", r().mtime(0.0));
    let mut s = b.s();
    let clock = FakeClock::on(&mut s);
    let naps: RefCell<Vec<f64>> = RefCell::new(Vec::new());
    let mut nap = |d: f64| {
        naps.borrow_mut().push(d);
        clock.add(d);
        set_mtime(&src, clock.t()); // the app keeps saving it
    };
    s.watch(3600.0, 10.0, POLL_SEC, Some(4), &mut nap);
    assert!(!b.has(B, "busy"));
    // three short waits (until the file has settled), never longer ones
    assert!((naps.borrow().iter().sum::<f64>() - 6.0).abs() <= 1.0);
    set_mtime(&src, clock.t() - 60.0); // it goes quiet
    s.watch(3600.0, 10.0, POLL_SEC, Some(1), &mut nap);
    assert!(b.has(B, "busy"));
}

#[test]
fn test_pass_that_cannot_start_does_slow_the_checks_down() {
    let b = Base::new();
    b.rec(A, "s1", r());
    fs::create_dir_all(&b.home).unwrap();
    fs::write(b.home.join("backups"), b"").unwrap(); // no backup can be made: nothing is written
    let mut s = b.s();
    let clock = FakeClock::on(&mut s);
    let mut total = 0.0;
    let mut nap = |d: f64| {
        total += d;
        clock.add(d);
    };
    s.watch(3600.0, 10.0, POLL_SEC, Some(4), &mut nap);
    assert!(total > 100.0); // 20 + 40 + 80 s
}

#[test]
fn test_one_off_apply_steps_aside_for_a_running_daemon() {
    let b = Base::new();
    fs::create_dir_all(&b.home).unwrap();
    let held = acquire_lock(&b.home).unwrap().unwrap();
    b.rec(A, "s1", r());
    assert_eq!(main(&b, &["sync"]), 0);
    assert!(!b.has(B, "s1"));
    assert_eq!(main(&b, &["undo", "--since", "2000-01-01"]), 1);
    release_lock(held);
}
