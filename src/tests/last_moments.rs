//! What the reviewers of version 7 found: the seconds around a sign-out, and a few edges.

use std::collections::BTreeSet;

use serde_json::json;

use super::*;
use crate::consts::{BEAT_SEC, HOME_SETTLE_SEC, POLL_SEC, REFRESH_SEC};
use crate::sync::{Homes, Signed};
use crate::winfs::{atomic_write, Expect, WriteError};

fn conflict(b: &Base, leaves: &[Leaf]) {
    b.rec(
        A,
        "abcdef-1",
        r().last(3.0 * DAY)
            .mtime(2.0 * DAY)
            .archived()
            .title("further along"),
    );
    for leaf in leaves {
        // an older copy the user un-archived
        b.rec(
            *leaf,
            "abcdef-1",
            r().last(5.0 * DAY)
                .mtime(DAY)
                .title("older, un-archived later"),
        );
    }
}

fn asked(b: &Base) -> serde_json::Value {
    b.state()["resolve"].clone()
}

#[test]
fn test_session_saved_just_before_sign_out_reaches_the_other_accounts_within_seconds() {
    let b = Base::new();
    b.rec(A, "s1", r().last(600.0).mtime(300.0));
    let state = Rc::new(RefCell::new(Signed::In(la())));
    let mut s = b.plain();
    s.app_running_fn = Some(Box::new(|| true));
    let reader = state.clone();
    s.signed_in_fn = Some(Box::new(move || reader.borrow().clone()));
    s.homes_fn = Some(Box::new(|| Homes::All));
    let clock = FakeClock::on(&mut s);
    s.watch(3600.0, 10.0, POLL_SEC, Some(1), &mut |_| {}); // B and C hold copies, placed a moment ago
    let mut went_out: Option<f64> = None;
    let mut done: Option<f64> = None;
    let tree = b.clone();
    let mut nap = |d: f64| {
        clock.add(d);
        if went_out.is_none() {
            // the session is saved once more, and 0.4 s later
            let p = tree.rec(A, "s1", r().last(1.0).mtime(0.0).title("the last save"));
            set_mtime(&p, clock.t() - 0.4);
            *state.borrow_mut() = Signed::Out; // the user signs out
            went_out = Some(clock.t());
        } else if done.is_none() && tree.title(B, "s1") == "the last save" {
            done = Some(clock.t());
        }
    };
    s.watch(3600.0, 10.0, POLL_SEC, Some(4), &mut nap);
    // not a minute later, when the next account is long signed in
    let (went_out, done) = (went_out.unwrap(), done.expect("the last save reached B"));
    assert!(done - went_out <= 6.0);
    assert_eq!(b.title(C, "s1"), "the last save");
}

#[test]
fn test_daemon_reports_it_is_alive_every_minute_even_with_hourly_passes_only() {
    let b = Base::new();
    let mut s = b.s();
    let clock = FakeClock::on(&mut s);
    let beats: Rc<RefCell<Vec<f64>>> = Rc::new(RefCell::new(Vec::new()));
    let (log, at) = (beats.clone(), clock.clone());
    s.hooks.on_status = Some(Box::new(move || log.borrow_mut().push(at.t())));
    s.watch(3600.0, 0.0, POLL_SEC, Some(2), &mut |d| clock.add(d));
    let beats = beats.borrow();
    let widest = beats.windows(2).map(|w| w[1] - w[0]).fold(0.0, f64::max);
    assert!(beats.len() > 55);
    assert!(widest <= BEAT_SEC + POLL_SEC);
}

#[test]
fn test_first_sign_in_to_a_new_account_is_recognised_within_a_minute_or_so() {
    let line = |when: f64, leaf: Leaf| {
        format!(
            "{} [info] [LocalSessionManager] Initialization succeeded — accountId={}, orgId={}, existingSessions=0\r\n",
            winfs::local_format(when, "%Y-%m-%d %H:%M:%S"),
            leaf.0,
            leaf.1
        )
    };
    let b = Base::new();
    b.write_log(&[&line(now() - DAY, A)]);
    let mut s = b.plain();
    s.app_running_fn = Some(Box::new(|| true));
    let clock = FakeClock::on(&mut s);
    let only_a = Homes::Only(BTreeSet::from([la()]));
    assert_eq!(s.homes(), only_a); // concluded before the new sign-in...
    b.write_log(&[&line(now() - HOME_SETTLE_SEC - 5.0, C)]);
    assert_eq!(s.homes(), only_a); // ...and not asked again within 20 s
    clock.add(21.0);
    assert_eq!(s.homes(), Homes::Only(BTreeSet::from([la(), lc()]))); // but then, not ten minutes later
}

#[test]
fn test_file_that_appears_in_the_last_instant_is_not_replaced() {
    let b = Base::new();
    let p = b.path(B, "s1");
    b.rec(B, "s1", r().title("appeared"));
    // we meant to create it
    assert!(matches!(
        atomic_write(&p, b"ours", None, true, Expect::Absent),
        Err(WriteError::Changed)
    ));
    assert_eq!(b.title(B, "s1"), "appeared");
    let st = winfs::stat(&p).unwrap();
    b.rec(B, "s1", r().title("changed again").mtime(5.0));
    // we meant to replace the one we looked at
    assert!(matches!(
        atomic_write(&p, b"ours", None, true, Expect::Is(st.mtime_ns, st.size)),
        Err(WriteError::Changed)
    ));
    assert_eq!(b.title(B, "s1"), "changed again");
    assert!(!listdir(p.parent().unwrap())
        .iter()
        .any(|n| n.ends_with(".tmp")));
    let st = winfs::stat(&p).unwrap();
    atomic_write(&p, b"ours", None, true, Expect::Is(st.mtime_ns, st.size)).unwrap();
    assert_eq!(fs::read(&p).unwrap(), b"ours");
}

#[test]
fn test_resolve_also_finds_a_conflict_in_the_account_in_use_and_waits_for_its_turn() {
    let b = Base::new();
    conflict(&b, &[B]);
    let busy = || b.s_app(true, Signed::In(lb())); // the app has B loaded
    assert_eq!(
        busy().resolve("abcdef").unwrap(),
        Some(String::from("abcdef-1"))
    );
    let mut s = busy();
    s.clock = Rc::new(|| now() + 2.0 * 3600.0); // two hours on, still loaded
    assert_eq!(s.run().update, 0);
    assert_eq!(b.title(B, "abcdef-1"), "older, un-archived later");
    assert!(asked(&b).get("abcdef-1").is_some()); // the request has not lapsed
    assert_eq!(b.s_app(true, Signed::Out).run().update, 1);
    assert_eq!(b.title(B, "abcdef-1"), "further along");
    b.s().run();
    assert_eq!(asked(&b), json!({}));
}

#[test]
fn test_request_is_carried_out_in_full_even_if_one_copy_has_to_wait() {
    let b = Base::new();
    conflict(&b, &[B, C]); // two accounts hold an older copy the user un-archived
    b.s().resolve("abcdef").unwrap();
    let busy = || b.s_app(true, Signed::In(lb()));
    assert_eq!(busy().run().update, 1); // C now; B is in use
    busy().run();
    assert!(asked(&b).get("abcdef-1").is_some()); // not closed while B is still owed
    assert_eq!(b.s_app(true, Signed::Out).run().update, 1);
    assert_eq!(b.title(B, "abcdef-1"), "further along");
}

#[test]
fn test_request_that_comes_in_gets_a_pass_and_is_not_lost_to_the_one_in_progress() {
    let b = Base::new();
    conflict(&b, &[B]);
    let mut daemon = b.s();
    let plan = daemon.plan(); // the daemon is in the middle of a pass...
    assert!(!daemon.something_due());
    b.s().resolve("abcdef").unwrap(); // ...when the user files the request
    daemon.apply(&plan.acts).unwrap();
    daemon.close_requests().unwrap();
    assert!(asked(&b).get("abcdef-1").is_some());
    assert!(daemon.something_due()); // and it does not have to wait for a change on disk
    assert_eq!(daemon.run().update, 1);
    assert!(!daemon.something_due());
}

#[test]
fn test_only_a_save_in_progress_keeps_the_hurry_up() {
    let b = Base::new();
    b.rec(A, "s1", r().last(600.0).mtime(300.0));
    b.rec(B, "s1", r().last(HOUR).mtime(2.0)); // B's copy was written by the app 2 s ago: wait for it
    let mut s = b.s();
    assert_eq!(s.run().deferred, 1);
    assert_eq!(s.settling, 0); // put off, but not on account of a save still settling
    b.rec(A, "s2", r().mtime(0.0)); // this one is: its source was saved this instant
    assert!(s.run().deferred >= 2);
    assert!(s.settling > 0);
}

#[test]
fn test_first_sign_in_gets_its_pass_when_it_has_settled_even_if_nothing_changes() {
    let b = Base::new();
    b.rec(A, "s1", r());
    let mut s = b.plain();
    s.app_running_fn = Some(Box::new(|| true));
    s.signed_in_fn = Some(Box::new(|| Signed::In(lc())));
    let clock = FakeClock::on(&mut s);
    s.homes_cache = Some((clock.t(), BTreeSet::from([la(), lb()]))); // what was known before the user signed in to C
    s.run();
    assert!(!b.has(C, "s1"));
    assert!(!s.something_due());
    clock.add(HOME_SETTLE_SEC + 30.0);
    assert!(s.something_due());
}

#[test]
fn test_request_with_nothing_left_to_settle_is_dropped() {
    let b = Base::new();
    conflict(&b, &[B]);
    b.s().resolve("abcdef").unwrap();
    b.rec(
        B,
        "abcdef-1",
        r().last(60.0).mtime(50.0).title("used again in B"),
    ); // the conflict ends by itself
    b.s().run();
    b.s().run();
    assert_eq!(asked(&b), json!({}));
}

#[test]
fn test_new_session_that_never_goes_quiet_is_added_all_the_same() {
    let b = Base::new();
    let mut s = b.s();
    s.urgent = false;
    let clock = FakeClock::on(&mut s);
    let p = b.rec(A, "s1", r().mtime(0.0));
    assert_eq!(s.run().deferred, 2);
    clock.add(3.0 * REFRESH_SEC);
    set_mtime(&p, clock.t() - 1.0); // still being saved every second or two
    s.urgent = false;
    assert_eq!(s.run().add, 2);
}

#[test]
fn test_app_whose_program_folder_is_not_recognised_still_counts_as_running_while_it_logs() {
    let b = Base::new();
    let mut s = b.plain();
    s.hooks.probe_app = Some(Box::new(|| false)); // as if the install path had changed
    assert!(!s.app_running()); // no log either: not running
    b.write_log(&["2026-11-11 09:00:00 [info] anything\r\n"]);
    s.app_seen = (0.0, false);
    assert!(s.app_running()); // the log was written just now
}

#[test]
fn resolve_all_settles_every_conflict_there_is() {
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
    b.rec(A, "plain", r());
    assert_eq!(b.s().plan().notes.conflicts.len(), 2);
    let asked = b.s().resolve_all().unwrap();
    assert_eq!(
        asked,
        vec![String::from("abcdef-1"), String::from("abcdef-2")]
    );
    assert_eq!(b.s().run().update, 2); // the next pass
    for fid in ["abcdef-1", "abcdef-2"] {
        assert_eq!(b.title(B, fid), "further along");
    }
    assert_eq!(b.kept().len(), 2); // the versions that gave way are kept
    b.s().run();
    assert_eq!(asked_now(&b), json!({}));
    // with nothing in conflict there is nothing to ask for, and nothing is written
    let b = Base::new();
    b.rec(A, "plain", r());
    assert!(b.s().resolve_all().unwrap().is_empty());
    assert!(!b.home.exists());
}

fn asked_now(b: &Base) -> serde_json::Value {
    b.state()["resolve"].clone()
}

#[test]
fn too_short_a_prefix_is_said_to_be_too_short() {
    let b = Base::new();
    conflict(&b, &[B]);
    assert_eq!(b.s().resolve("abc").unwrap(), None);
    assert_eq!(b.said("at least its first 6 characters"), 1);
    assert_eq!(b.s().resolve("zzzzzzzz").unwrap(), None); // long enough, but no such session
    assert_eq!(b.said("at least its first 6 characters"), 1);
    assert_eq!(b.s().restore("abc").unwrap(), None);
    assert_eq!(b.said("at least its first 6 characters"), 2);
    // the whole id is always enough, however short
    let b = Base::new();
    b.rec(
        A,
        "s1",
        r().last(3.0 * DAY)
            .mtime(2.0 * DAY)
            .archived()
            .title("further along"),
    );
    b.rec(
        B,
        "s1",
        r().last(5.0 * DAY)
            .mtime(DAY)
            .title("older, un-archived later"),
    );
    assert_eq!(b.s().resolve("s1").unwrap(), Some(String::from("s1")));
}
