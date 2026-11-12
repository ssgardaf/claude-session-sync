use serde_json::json;

use super::*;
use crate::consts::VANISH_GRACE_SEC;
use crate::sync::{Homes, Signed};

#[test]
fn test_marker_blocks_adding_back_by_any_id() {
    let b = Base::new();
    b.rec(
        A,
        "s1",
        r().cli("c1").extra(json!({"priorCliSessionIds": ["old1"]})),
    );
    b.rec(A, "s2", r().cli("c2"));
    b.rec(A, "s3", r().cli("c3"));
    b.tomb(B, "s1", ms(DAY)); // by file id
    b.tomb(C, "old1", ms(DAY)); // by an earlier conversation id
    b.tomb(B, "c2", ms(DAY)); // by conversation id
    b.s().run();
    assert!(!b.has(B, "s1"));
    assert!(!b.has(C, "s1"));
    assert!(!b.has(B, "s2"));
    assert!(b.has(C, "s2"));
    assert!(b.has(B, "s3"));
}

#[test]
fn test_deletion_while_running_is_carried_over_reversibly() {
    let b = Base::new();
    b.install(2.0 * HOUR, 0.0);
    b.rec(B, "s1", r().cli("c1").last(HOUR));
    b.rec(C, "s1", r().cli("c1").last(HOUR));
    b.delete(A, "s1", 600.0, Some("c1"));
    let ts = fs::read(b.dir(A).join("deleted_s1")).unwrap();
    let done = b.s().run();
    assert_eq!(done.quarantine, 2);
    for (leaf, leaf_name) in [(B, lb()), (C, lc())] {
        assert!(!b.has(leaf, "s1"));
        let kept = listdir(&b.home.join("quarantine").join(leaf_name.replace('/', "_")));
        assert_eq!(kept.len(), 1);
        assert!(kept[0].starts_with("local_s1.json."));
        for ident in ["s1", "c1"] {
            assert_eq!(
                fs::read(b.dir(leaf).join(format!("deleted_{}", ident))).unwrap(),
                ts
            );
        }
    }
    assert_eq!(b.s().run().sum(), 0);
}

#[test]
fn test_deleting_one_session_does_not_remove_another_that_shares_a_conversation_id() {
    let b = Base::new();
    b.install(2.0 * HOUR, 0.0);
    b.rec(B, "y", r().cli("c0").last(HOUR)); // a different session...
    b.delete(A, "x", 600.0, Some("cx")); // ...from the one deleted in A,
    b.tomb(A, "c0", ms(600.0)); // whose history included conversation c0
    let done = b.s().run();
    assert_eq!(done.quarantine, 0);
    assert!(b.has(B, "y"));
    assert!(b.has(C, "y"));
    assert!(!b.has(A, "y")); // not added where that conversation was deleted
}

#[test]
fn test_session_used_after_the_deletion_is_kept() {
    let b = Base::new();
    b.install(2.0 * HOUR, 0.0);
    b.rec(B, "s1", r().cli("c1").last(60.0));
    b.delete(A, "s1", 600.0, Some("c1"));
    let done = b.s().run();
    assert_eq!(done.quarantine, 0);
    assert!(b.has(B, "s1"));
    assert!(!b.has(A, "s1")); // still not added back where it was deleted
    assert!(b.has(C, "s1"));
}

#[test]
fn test_deletion_from_before_install_is_neither_carried_over_nor_spread() {
    let b = Base::new();
    b.install(HOUR, 0.0);
    b.rec(B, "s1", r().cli("c1").last(5.0 * HOUR));
    b.delete(A, "s1", 3.0 * HOUR, Some("c1")); // deleted in A after its last use, before this tool existed
    let mut s = b.s();
    let notes = s.plan().notes;
    assert_eq!(
        notes.deleted_elsewhere,
        vec![(lc(), String::from("sesión"))]
    );
    let done = s.run();
    assert_eq!(done.quarantine, 0);
    assert!(b.has(B, "s1")); // stays where it is
    assert!(!b.has(C, "s1")); // but is not handed to an account that never had it
    assert!(!b.has(A, "s1"));
}

#[test]
fn test_deletions_are_not_carried_over_unless_asked_for() {
    let b = Base::new();
    b.install(2.0 * HOUR, 0.0);
    b.rec(B, "s1", r().cli("c1").last(HOUR));
    b.delete(A, "s1", 600.0, Some("c1"));
    b.rec(B, "s2", r().cli("c2").last(60.0));
    b.delete(A, "s2", 600.0, Some("c2")); // this one was used again after A deleted it
    let mut plain = b.plain();
    plain.app_running_fn = Some(Box::new(|| false));
    plain.signed_in_fn = Some(Box::new(|| Signed::Unknown));
    plain.homes_fn = Some(Box::new(|| Homes::All));
    assert!(!plain.delete_sync); // the tool's own default
    let done = plain.run();
    assert_eq!(done.quarantine, 0);
    assert!(b.has(B, "s1")); // the account that has it keeps it
    assert!(!b.has(C, "s1")); // an account that never had it is not given a deleted session
    assert!(!(b.has(A, "s1") || b.has(A, "s2"))); // deleted there: never handed back
    assert!(b.has(C, "s2")); // still in use elsewhere: a session like any other
    assert!(!b.home.join("quarantine").exists());
}

#[test]
fn test_remembered_stretches_of_downtime_never_lapse() {
    let b = Base::new();
    fs::create_dir_all(&b.home).unwrap();
    // sixty nights asleep
    let gaps: Vec<[i64; 2]> = (0..60)
        .map(|i| {
            [
                ms((60 - i) as f64 * DAY),
                ms((60 - i) as f64 * DAY - 3.0 * HOUR),
            ]
        })
        .collect();
    let st = json!({"installed_at": ms(100.0 * DAY), "propagate_since": ms(100.0 * DAY), "blackouts": gaps,
                    "alive_until": ms(0.0)});
    fs::write(b.home.join("state.json"), st.to_string()).unwrap();
    b.rec(B, "s1", r().last(61.0 * DAY));
    b.delete(A, "s1", 60.0 * DAY - HOUR, None); // deleted during the first of those nights
    for _ in 0..3 {
        let mut s = b.s();
        assert_eq!(s.run().quarantine, 0);
        s.touch_alive(true).unwrap();
    }
    assert!(b.has(B, "s1"));
    let st = b.state();
    assert!(st["blackouts"].as_array().unwrap().len() <= 50);
    // moved past the stretch that was let go
    assert!(st["propagate_since"].as_f64().unwrap() > ms(60.0 * DAY - HOUR) as f64);
}

#[test]
fn test_deletions_made_while_the_tool_was_not_running_are_not_carried_over() {
    // installed 10 h ago, last alive 5 h ago (uninstalled, or the PC was off), started again now
    let b = Base::new();
    b.install(10.0 * HOUR, 5.0 * HOUR);
    b.rec(B, "during", r().last(6.0 * HOUR));
    b.delete(A, "during", 3.0 * HOUR, None); // deleted while the tool was off
    b.rec(B, "before", r().last(8.0 * HOUR));
    b.delete(A, "before", 7.0 * HOUR, None); // deleted while it was running (it had not caught up)
    let mut s = b.s();
    let done = s.run();
    assert!(b.has(B, "during"));
    assert!(!b.has(B, "before"));
    assert_eq!(done.quarantine, 1);
    assert_eq!(b.state()["blackouts"].as_array().unwrap().len(), 1); // remembered, so a later run agrees
    assert_eq!(b.s().run().quarantine, 0);
    assert!(b.has(B, "during"));
}

#[test]
fn test_marker_next_to_a_live_record_does_not_remove_other_copies() {
    let b = Base::new();
    b.install(2.0 * HOUR, 0.0);
    b.rec(A, "s1", r().cli("c1").last(HOUR));
    b.tomb(A, "s1", ms(600.0)); // marker and record together: not a clean deletion
    b.rec(B, "s1", r().cli("c1").last(HOUR));
    assert_eq!(b.s().run().quarantine, 0);
    assert!(b.has(A, "s1") && b.has(B, "s1"));
}

#[test]
fn test_record_that_just_vanished_without_a_marker_is_not_put_back_at_once() {
    let b = Base::new();
    b.install(2.0 * HOUR, 0.0);
    b.rec(A, "s1", r());
    let mut s = b.s();
    s.run();
    fs::remove_file(b.path(B, "s1")).unwrap(); // the app is half-way through deleting it in B
    assert_eq!(s.run().add, 0);
    assert!(!b.has(B, "s1"));
    b.delete(B, "s1", 0.0, None); // ...and now the markers are there
    let done = s.run();
    assert_eq!(done.quarantine, 2); // a real deletion: carried to A and C
    assert!(!(b.has(A, "s1") || b.has(C, "s1")));
}

#[test]
fn test_record_that_vanished_and_stays_unmarked_comes_back_after_the_grace_period() {
    let b = Base::new();
    b.rec(A, "s1", r());
    let mut s = b.s();
    s.run();
    fs::remove_file(b.path(B, "s1")).unwrap();
    assert_eq!(s.run().add, 0);
    s.clock = Rc::new(|| now() + VANISH_GRACE_SEC + 5.0);
    assert_eq!(s.run().add, 1);
}

#[test]
fn test_restore_brings_a_removed_session_back_everywhere() {
    let b = Base::new();
    b.install(2.0 * HOUR, 0.0);
    b.rec(B, "abcdef-1", r().cli("c1").last(HOUR));
    b.rec(C, "abcdef-1", r().cli("c1").last(HOUR));
    b.delete(A, "abcdef-1", 600.0, Some("c1"));
    b.s().run();
    assert!(!b.has(B, "abcdef-1"));
    assert_eq!(b.s().restore("nothing-like-it").unwrap(), None);
    let found = b.s().restore("abcdef").unwrap();
    assert_eq!(found, Some((String::from("abcdef-1"), 6, 2)));
    assert!(b.has(B, "abcdef-1") && b.has(C, "abcdef-1"));
    let done = b.s().run(); // the daemon's next pass
    assert_eq!((done.quarantine, done.add), (0, 1));
    assert!(b.has(A, "abcdef-1")); // back even where it was deleted
}

#[test]
fn deletion_that_was_carried_over_and_undone_is_not_carried_over_again() {
    let b = Base::new();
    b.install(2.0 * HOUR, 0.0);
    b.rec(B, "s1", r().cli("c1").last(HOUR));
    // the same version in both (not every system's copy keeps the file's time: it is given
    // the very same one)
    fs::copy(b.path(B, "s1"), b.path(C, "s1")).unwrap();
    let as_in_b = winfs::stat(&b.path(B, "s1")).unwrap().mtime_ns;
    winfs::set_times(&b.path(C, "s1"), as_in_b, as_in_b).unwrap();
    b.delete(A, "s1", 600.0, Some("c1"));
    let start = now() - 1.0;
    assert_eq!(b.s().run().quarantine, 2);
    assert_eq!(b.s().undo(start).unwrap().put_back, 2);
    assert!(b.has(B, "s1") && b.has(C, "s1"));
    // A's own marker is still there, as the app left it; it does not take the session out again
    assert!(b.dir(A).join("deleted_s1").exists());
    for _ in 0..2 {
        let mut s = b.s();
        assert!(s.plan().acts.is_empty());
        assert_eq!(s.run().sum(), 0);
    }
    assert!(b.has(B, "s1") && b.has(C, "s1"));
    assert!(!b.has(A, "s1")); // (nor is it put back where it was deleted)
    assert!(b.state()["exempt"]["s1"].as_f64().is_some());
    // a deletion made after that is a new decision, and is carried over
    b.rec(A, "s1", r().cli("c1").last(HOUR));
    fs::remove_file(b.dir(A).join("deleted_s1")).unwrap();
    fs::remove_file(b.dir(A).join("deleted_c1")).unwrap();
    b.delete(A, "s1", 30.0, Some("c1"));
    assert_eq!(b.s().run().quarantine, 2);
    assert!(!b.has(B, "s1") && !b.has(C, "s1"));
}

#[test]
fn record_that_vanished_is_not_put_back_at_once_by_a_new_process_either() {
    let b = Base::new();
    b.rec(A, "s1", r());
    b.s().run(); // B and C hold copies this tool placed
    fs::remove_file(b.path(B, "s1")).unwrap(); // the app is half-way through deleting it in B
                                               // a new process (the daemon restarted, or a one-off sync): it cannot have seen the record
                                               // go, but it knows it had placed it there, and that there is no marker
    let mut s = b.s();
    assert_eq!(s.plan().notes.just_removed, 1);
    assert_eq!(s.run().add, 0);
    assert!(!b.has(B, "s1"));
    // and another one a little later: the five minutes run from the first notice, not anew
    let mut s = b.s();
    s.clock = Rc::new(|| now() + VANISH_GRACE_SEC - 60.0);
    assert_eq!(s.run().add, 0);
    let mut s = b.s();
    s.clock = Rc::new(|| now() + VANISH_GRACE_SEC + 5.0);
    assert_eq!(s.run().add, 1); // no marker ever came: it comes back
    assert!(b.has(B, "s1"));
    assert_eq!(b.s().run().sum(), 0);
    assert!(b.state().get("vanished").is_none()); // nothing is kept on record longer than needed
}

#[test]
fn record_that_vanished_and_then_got_its_marker_is_a_deletion_for_a_new_process_too() {
    let b = Base::new();
    b.install(2.0 * HOUR, 0.0);
    b.rec(A, "s1", r());
    b.s().run();
    fs::remove_file(b.path(B, "s1")).unwrap();
    assert_eq!(b.s().run().add, 0); // noticed by one process...
    b.delete(B, "s1", 0.0, None); // ...the markers follow...
    let done = b.s().run(); // ...and the next process carries the deletion over
    assert_eq!((done.quarantine, done.add), (2, 0));
    // a file that was never one of ours and is simply not there is a session to hand out
    let b = Base::new();
    b.rec(A, "s1", r());
    assert_eq!(b.s().plan().notes.just_removed, 0);
    assert_eq!(b.s().run().add, 2);
}
