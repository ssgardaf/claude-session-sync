use serde_json::Value;

use super::*;
use crate::consts::REFRESH_SEC;
use crate::sync::{Homes, Signed};

#[test]
fn test_copies_of_a_busy_session_are_refreshed_once_a_minute_not_on_every_save() {
    let b = Base::new();
    let mut s = b.s();
    b.rec(A, "s1", r().last(600.0).mtime(300.0));
    s.run();
    s.urgent = false; // as in the daemon's routine checks
    b.rec(A, "s1", r().last(200.0).mtime(100.0).title("saved again"));
    let notes = s.plan().notes;
    assert_eq!(notes.waiting_turn, 2);
    assert!(!s.something_due());
    assert_eq!(s.run().update, 0);
    FakeClock::at(&mut s, now() + REFRESH_SEC + 1.0);
    assert!(s.something_due()); // a minute on, the daemon is told it is time
    assert_eq!(s.run().update, 2);
}

#[test]
fn test_a_pass_that_matters_refreshes_at_once() {
    let b = Base::new();
    let mut s = b.s();
    b.rec(A, "s1", r().last(600.0).mtime(300.0));
    s.run();
    b.rec(A, "s1", r().last(200.0).mtime(100.0).title("saved again"));
    s.urgent = true; // sign-out, app start or quit, the hourly pass, a one-off run
    assert_eq!(s.run().update, 2);
}

#[test]
fn test_new_sessions_are_never_made_to_wait() {
    let b = Base::new();
    let mut s = b.s();
    b.rec(A, "s1", r());
    s.run();
    s.urgent = false;
    b.rec(A, "s2", r());
    assert_eq!(s.run().add, 2);
}

#[test]
fn test_session_that_never_goes_quiet_is_still_followed() {
    let b = Base::new();
    let mut s = b.s();
    b.rec(A, "s1", r().last(600.0).mtime(300.0));
    s.run();
    let clock = FakeClock::at(&mut s, now() + 3.0 * REFRESH_SEC);
    let p = b.rec(
        A,
        "s1",
        r().last(5.0).mtime(0.0).title("saved a second ago"),
    );
    set_mtime(&p, clock.t() - 1.0);
    assert_eq!(s.run().update, 2); // long overdue: one second of quiet is enough
    let p = b.rec(A, "s1", r().last(4.0).mtime(0.0).title("and again"));
    set_mtime(&p, clock.t() - 0.3);
    assert_eq!(s.run().deferred, 2); // just refreshed: a save is given its second to finish
}

#[test]
fn test_folder_the_app_loads_between_planning_and_writing_is_left_alone() {
    let b = Base::new();
    b.rec(A, "s1", r().last(2.0 * HOUR).mtime(2.0 * HOUR));
    b.s().run();
    b.rec(A, "s1", r().last(HOUR).mtime(50.0 * 60.0).title("newer"));
    let state = Rc::new(RefCell::new(Signed::Out));
    let mut s = b.plain();
    s.app_running_fn = Some(Box::new(|| true));
    let reader = state.clone();
    s.signed_in_fn = Some(Box::new(move || reader.borrow().clone()));
    s.homes_fn = Some(Box::new(|| Homes::All));
    let plan = s.plan();
    assert_eq!(plan.acts.len(), 2);
    *state.borrow_mut() = Signed::In(lb()); // the user signs in to B while the backup is being made
    FakeClock::at(&mut s, now() + 2.0);
    let done = s.apply(&plan.acts).unwrap();
    assert_eq!((done.update, done.deferred), (1, 1));
    assert_ne!(b.title(B, "s1"), "newer");
}

#[test]
fn test_failures_of_something_no_longer_wanted_are_forgotten() {
    let b = Base::new();
    b.rec(A, "s1", r());
    let in_the_way = b.block(B, "s1");
    let mut s = b.s();
    assert_eq!(s.run().failed, 1);
    assert_eq!(s.retry.len(), 1);
    fs::remove_dir(in_the_way).unwrap();
    b.delete(B, "s1", 0.0, None); // B does not want it any more
    s.run();
    assert!(s.retry.is_empty());
    FakeClock::at(&mut s, now() + 3600.0);
    assert!(!s.something_due());
}

#[test]
fn test_count_of_sessions_that_are_everywhere_is_not_flattered() {
    let b = Base::new();
    b.rec(A, "everywhere", r());
    b.rec(A, "not-in-b", r());
    b.delete(B, "not-in-b", 5.0 * HOUR, None); // ...yet it can still go to C
    let mut s = b.s();
    assert_eq!(s.plan().notes.everywhere, 1);
    s.run();
    assert!(b.has(C, "not-in-b"));
    assert_eq!(s.plan().notes.everywhere, 1);
}

#[test]
fn test_undo_also_puts_back_a_file_time_that_was_only_aligned() {
    let b = Base::new();
    b.rec(A, "s1", r().mtime(2.0 * HOUR));
    fs::copy(b.path(A, "s1"), b.path(B, "s1")).unwrap();
    set_mtime(&b.path(B, "s1"), now() - 5.0 * HOUR);
    let before = b.snapshot(&b.root);
    let start = now() - 1.0;
    assert_eq!(b.s_homes(&[la(), lb()]).run().aligned, 1);
    assert_ne!(before, b.snapshot(&b.root));
    b.s().undo(start).unwrap();
    assert_eq!(before, b.snapshot(&b.root));
}

#[test]
fn test_what_is_about_to_be_written_is_on_record_first() {
    let b = Base::new();
    b.rec(A, "s1", r());
    let mut s = b.s_homes(&[la(), lb()]);
    let seen: Rc<RefCell<Vec<Vec<String>>>> = Rc::new(RefCell::new(Vec::new()));
    let (log, journal) = (seen.clone(), b.home.join("journal.jsonl"));
    s.hooks.before_write = Some(Box::new(move |path| {
        if path.to_string_lossy().ends_with("local_s1.json") {
            let text = fs::read_to_string(&journal).unwrap();
            let on_record = text
                .lines()
                .map(|l| {
                    serde_json::from_str::<Value>(l).unwrap()["dst"]
                        .as_str()
                        .unwrap()
                        .to_string()
                })
                .collect();
            log.borrow_mut().push(on_record);
        }
    }));
    s.run();
    assert_eq!(
        *seen.borrow(),
        vec![vec![b.path(B, "s1").to_string_lossy().into_owned()]]
    );
}
