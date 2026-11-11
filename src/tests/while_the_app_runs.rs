use serde_json::json;

use super::*;
use crate::sync::Signed;

fn held(names: &[String]) -> Vec<(String, String)> {
    names
        .iter()
        .map(|n| (n.clone(), String::from("sesión")))
        .collect()
}

/// A has moved on; B still holds our earlier copy, which the app in B has re-saved.
fn stale_copy_in_b(b: &Base) {
    b.rec(A, "s1", r().last(2.0 * HOUR).mtime(2.0 * HOUR));
    b.s().run();
    let t = b.last_activity(A, "s1");
    b.rec(
        A,
        "s1",
        r().last(HOUR).mtime(50.0 * 60.0).title("newer, from A"),
    );
    b.rec(
        B,
        "s1",
        r().mtime(40.0 * 60.0)
            .extra(json!({"lastActivityAt": t, "lastFocusedAt": ms(40.0 * 60.0)})),
    );
}

#[test]
fn test_existing_records_in_the_account_in_use_are_not_replaced_but_new_ones_are_added() {
    let b = Base::new();
    b.rec(A, "s1", r().last(2.0 * HOUR).mtime(2.0 * HOUR));
    b.s().run(); // B and C hold our copies
    let t = b.last_activity(A, "s1");
    b.rec(A, "s1", r().last(600.0).mtime(500.0).title("newer, from A"));
    // the user switched to B before that reached it: the app loaded our old copy and has just re-saved it
    b.rec(
        B,
        "s1",
        r().mtime(300.0)
            .extra(json!({"lastActivityAt": t, "lastFocusedAt": ms(300.0)})),
    );
    b.rec(A, "s2", r());
    let mut s = b.s_app(true, Signed::Unknown);
    assert_eq!(s.plan().notes.held, held(&[lb()]));
    let done = s.run();
    assert_eq!(b.title(B, "s1"), "sesión"); // what the app has in memory stays on disk too
    assert!(b.has(B, "s2"));
    assert_eq!(b.title(C, "s1"), "newer, from A");
    assert_eq!(done.deferred, 0);
    // once the app is closed (or uses another account) the held update goes through
    assert_eq!(b.s_app(false, Signed::Unknown).run().update, 1);
    assert_eq!(b.title(B, "s1"), "newer, from A");
}

#[test]
fn test_deletion_is_not_carried_into_the_account_in_use() {
    let b = Base::new();
    b.install(2.0 * HOUR, 0.0);
    b.rec(B, "s1", r().last(HOUR).mtime(30.0 * 60.0));
    b.rec(C, "s1", r().last(HOUR).mtime(HOUR));
    b.rec(B, "other", r().last(60.0).mtime(200.0)); // B wrote last: in use
    b.delete(A, "s1", 20.0 * 60.0, None);
    set_mtime(&b.dir(A).join("deleted_s1"), now() - 20.0 * 60.0);
    let done = b.s_app(true, Signed::Unknown).run();
    assert_eq!(done.quarantine, 1);
    assert!(b.has(B, "s1"));
    assert!(!b.has(C, "s1"));
}

#[test]
fn test_our_own_writes_do_not_make_a_folder_look_in_use() {
    let b = Base::new();
    b.rec(A, "s1", r().mtime(300.0));
    b.s_app(true, Signed::Unknown).run(); // B and C receive copies
    b.rec(A, "s1", r().last(250.0).mtime(200.0)); // A moves on
    let done = b.s_app(true, Signed::Unknown).run();
    assert_eq!(done.update, 2); // neither B nor C counts as "in use"
}

#[test]
fn test_the_apps_own_log_says_which_folder_it_has_loaded() {
    let b = Base::new();
    stale_copy_in_b(&b);
    b.rec(A, "s2", r().mtime(60.0)); // A is where the last write happened...
    let mut s = b.s_app(true, Signed::In(lb())); // ...but the app signed in to B after that
    assert_eq!(s.plan().notes.held, held(&[lb()]));
    assert_eq!(s.run().update, 1); // C only
    assert_eq!(b.title(B, "s1"), "sesión");
    assert!(b.has(B, "s2"));
}

#[test]
fn test_signed_out_is_the_moment_to_bring_every_folder_up_to_date() {
    let b = Base::new();
    stale_copy_in_b(&b);
    let mut s = b.s_app(true, Signed::Out);
    assert!(s.plan().notes.held.is_empty());
    assert_eq!(s.run().update, 2);
    assert_eq!(b.title(B, "s1"), "newer, from A");
}

#[test]
fn test_when_the_folder_in_use_cannot_be_told_no_existing_record_is_touched() {
    let b = Base::new();
    stale_copy_in_b(&b); // the last app write was 40 minutes ago: no telling
    b.rec(A, "s2", r().mtime(2.0 * HOUR));
    let mut s = b.s_app(true, Signed::Unknown);
    assert_eq!(s.plan().notes.held.len(), 2); // B's and C's copies of s1
    let done = s.run();
    assert_eq!((done.update, done.add), (0, 2)); // new records are still handed out
    assert_eq!(b.s_app(false, Signed::Unknown).run().update, 2);
}

#[test]
fn test_sign_out_starts_a_pass_at_once() {
    let b = Base::new();
    stale_copy_in_b(&b);
    let state = Rc::new(RefCell::new(Signed::In(lb())));
    let mut s = b.plain();
    s.app_running_fn = Some(Box::new(|| true));
    let reader = state.clone();
    s.signed_in_fn = Some(Box::new(move || reader.borrow().clone()));
    let mut seen: Vec<String> = Vec::new();
    let tree = b.clone();
    let mut nap = |_: f64| {
        // while the daemon sleeps after its first pass...
        if seen.is_empty() {
            seen.push(tree.title(B, "s1"));
        }
        *state.borrow_mut() = Signed::Out; // ...the user signs out; nothing on disk changes
    };
    s.watch(3600.0, 1.0, crate::consts::POLL_SEC, Some(2), &mut nap);
    assert_eq!(seen, vec![String::from("sesión")]); // held while B was loaded
    assert_eq!(b.title(B, "s1"), "newer, from A"); // brought up to date the moment it was not
}
