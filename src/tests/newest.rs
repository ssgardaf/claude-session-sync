use std::collections::BTreeSet;

use serde_json::json;

use super::*;

#[test]
fn test_more_activity_beats_newer_file_time() {
    // the real-world trap: the app re-saved a stale copy, so its file time is newer
    let b = Base::new();
    b.rec(A, "s1", r().last(HOUR).mtime(5.0 * DAY).title("advanced"));
    b.rec(B, "s1", r().last(9.0 * HOUR).mtime(DAY).title("stale"));
    let a_before = b.data(A, "s1");
    let done = b.s().run();
    assert_eq!(done.update, 1);
    assert_eq!(b.data(A, "s1"), a_before);
    assert_eq!(b.data(B, "s1"), a_before);
}

#[test]
fn test_equal_activity_with_no_known_order_is_a_conflict_and_nothing_is_overwritten() {
    // A: archived and renamed by the user. B: an old copy the app merely re-saved later.
    let b = Base::new();
    let t = ms(HOUR);
    b.rec(
        A,
        "s1",
        r().mtime(3.0 * HOUR)
            .archived()
            .title("renamed")
            .extra(json!({"lastActivityAt": t})),
    );
    b.rec(
        B,
        "s1",
        r().mtime(2.0 * HOUR)
            .title("old name")
            .extra(json!({"lastActivityAt": t})),
    );
    let (a, bb) = (b.data(A, "s1"), b.data(B, "s1"));
    let mut s = b.s();
    assert_eq!(s.plan().notes.conflicts.len(), 1);
    s.run();
    assert_eq!((b.data(A, "s1"), b.data(B, "s1")), (a, bb.clone()));
    assert_eq!(b.data(C, "s1"), bb); // an account that had none still gets one
}

#[test]
fn test_change_without_activity_made_on_top_of_our_copy_is_carried_everywhere() {
    let b = Base::new();
    b.rec(A, "s1", r()); // A is where the session lives
    b.s().run(); // B and C now hold our copies of it
    let t = b.last_activity(A, "s1");
    // the user switches to B and archives it there: same activity, newer file, different content
    b.rec(
        B,
        "s1",
        r().mtime(600.0)
            .archived()
            .title("archived in B")
            .extra(json!({"lastActivityAt": t})),
    );
    let mut s = b.s();
    assert!(s.plan().notes.conflicts.is_empty());
    assert_eq!(s.run().update, 2);
    assert_eq!(b.title(A, "s1"), "archived in B"); // A held exactly the version B started from
    assert_eq!(b.title(C, "s1"), "archived in B");
    assert_eq!(b.kept().len(), 1); // A's own file was kept first
                                   // ...and back in A the same works the other way round
    b.rec(
        A,
        "s1",
        r().mtime(300.0)
            .title("renamed in A")
            .archived()
            .extra(json!({"lastActivityAt": t})),
    );
    assert_eq!(b.s().run().update, 2);
    assert_eq!(b.title(B, "s1"), "renamed in A");
    assert_eq!(b.s().run().sum(), 0);
}

#[test]
fn test_copy_opened_after_the_other_moved_on_is_a_conflict_until_the_session_is_used_again() {
    // what the real store looks like: A worked on it and archived it; later, under another account,
    // the user un-archived an older copy. Which state they want cannot be told, so neither is overwritten.
    let b = Base::new();
    b.rec(
        A,
        "s1",
        r().last(3.0 * DAY)
            .mtime(2.0 * DAY)
            .archived()
            .title("further along, archived"),
    );
    b.rec(
        B,
        "s1",
        r().last(5.0 * DAY)
            .mtime(DAY)
            .title("older, un-archived later"),
    );
    let (a, bb) = (b.data(A, "s1"), b.data(B, "s1"));
    let mut s = b.s();
    let found: Vec<(String, String, String)> = s
        .plan()
        .notes
        .conflicts
        .iter()
        .map(|c| (c.0.clone(), c.1.clone(), c.3.clone()))
        .collect();
    assert_eq!(
        found,
        vec![(
            lb(),
            String::from("older, un-archived later"),
            String::from("s1")
        )]
    );
    s.run();
    assert_eq!((b.data(A, "s1"), b.data(B, "s1")), (a, bb));
    assert!(b.kept().is_empty());
    assert_eq!(b.title(C, "s1"), "further along, archived");
    // the same is true when it is only the "last opened" stamp that is later
    b.rec(
        B,
        "s1",
        r().last(5.0 * DAY)
            .mtime(4.0 * DAY)
            .title("only looked at")
            .extra(json!({"lastFocusedAt": ms(DAY)})),
    );
    assert_eq!(b.s().plan().notes.conflicts.len(), 1);
    // using the session anywhere settles it
    b.rec(
        A,
        "s1",
        r().last(600.0).mtime(500.0).title("used again in A"),
    );
    assert_eq!(b.s().run().update, 2);
    assert_eq!(b.title(B, "s1"), "used again in A");
    assert_eq!(b.kept().len(), 1);
}

#[test]
fn test_our_copy_with_only_non_activity_edits_gives_way_to_real_progress() {
    let b = Base::new();
    b.rec(A, "s1", r().last(2.0 * HOUR));
    b.s().run(); // B and C hold our copies
    let t = b.last_activity(A, "s1");
    b.rec(
        A,
        "s1",
        r().last(20.0 * 60.0).mtime(19.0 * 60.0).title("A went on"),
    );
    // ...but the switch to B came before that reached B: the app loaded our old copy and saved it
    // again (a new "last opened" stamp, nothing the user decided)
    b.rec(
        B,
        "s1",
        r().mtime(10.0 * 60.0)
            .extra(json!({"lastActivityAt": t, "lastFocusedAt": ms(10.0 * 60.0)})),
    );
    let mut s = b.s();
    assert!(s.plan().notes.conflicts.is_empty());
    assert_eq!(s.run().update, 2);
    assert_eq!(b.title(B, "s1"), "A went on");
    assert_eq!(b.kept().len(), 1); // B's re-saved file is kept, not lost
}

#[test]
fn test_decision_the_user_made_on_top_of_our_older_copy_is_not_overwritten() {
    let b = Base::new();
    b.rec(A, "s1", r().last(2.0 * HOUR));
    b.s().run(); // B holds our copy
    let t = b.last_activity(A, "s1");
    b.rec(
        A,
        "s1",
        r().last(20.0 * 60.0).mtime(19.0 * 60.0).title("A went on"),
    );
    // before that reached B, the user switched to B and archived the session there
    b.rec(
        B,
        "s1",
        r().mtime(10.0 * 60.0)
            .archived()
            .extra(json!({"lastActivityAt": t})),
    );
    let mine = b.data(B, "s1");
    let mut s = b.s();
    let found: Vec<(String, String)> = s
        .plan()
        .notes
        .conflicts
        .iter()
        .map(|c| (c.0.clone(), c.3.clone()))
        .collect();
    assert_eq!(found, vec![(lb(), String::from("s1"))]);
    assert_eq!(s.run().update, 1); // C, which holds our untouched copy
    assert_eq!(b.data(B, "s1"), mine);
    assert!(b.kept().is_empty());
}

#[test]
fn test_copy_that_differs_only_in_progress_gives_way_without_being_asked() {
    // both archived, same title: the older copy was merely re-saved by the app when that account loaded it
    let b = Base::new();
    b.rec(
        A,
        "s1",
        r().last(3.0 * DAY)
            .mtime(2.0 * DAY)
            .archived()
            .title("bug hunt"),
    );
    b.rec(
        B,
        "s1",
        r().last(5.0 * DAY).mtime(DAY).archived().title("bug hunt"),
    );
    let bb = b.data(B, "s1");
    let mut s = b.s();
    assert!(s.plan().notes.conflicts.is_empty());
    assert_eq!(s.run().update, 1);
    assert_eq!(b.data(B, "s1"), b.data(A, "s1"));
    assert_eq!(fs::read(&b.kept()[0]).unwrap(), bb);
}

#[test]
fn test_resolve_settles_a_conflict_on_request() {
    let b = Base::new();
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
    assert_eq!(b.s().run().update, 0);
    assert_eq!(b.s().resolve("zzzzzz").unwrap(), None);
    assert_eq!(
        b.s().resolve("abcdef").unwrap(),
        Some(String::from("abcdef-1"))
    );
    assert_eq!(b.s().run().update, 1); // the daemon's next pass
    assert_eq!(b.title(B, "abcdef-1"), "further along");
    assert_eq!(b.kept().len(), 1);
    assert_eq!(b.s().run().sum(), 0); // the pass after that finds nothing left to settle
    assert_eq!(b.state()["resolve"], json!({})); // asked once, done once
}

#[test]
fn test_older_state_the_app_wrote_back_over_our_newer_copy_is_not_a_conflict() {
    let b = Base::new();
    b.rec(A, "s1", r().last(2.0 * HOUR).mtime(2.0 * HOUR).title("v1"));
    b.s().run(); // B holds our copy of v1
    b.rec(A, "s1", r().last(HOUR).mtime(50.0 * 60.0).title("v2"));
    b.s().run(); // ...and now of v2
                 // the app in B still had v1 in memory and saves it again, un-archived and all
    let t1 = ms(2.0 * HOUR);
    b.rec(
        B,
        "s1",
        r().mtime(10.0 * 60.0)
            .title("v1 from memory")
            .extra(json!({"lastActivityAt": t1})),
    );
    let mut s = b.s();
    assert!(s.plan().notes.conflicts.is_empty());
    assert_eq!(s.run().update, 1);
    assert_eq!(b.title(B, "s1"), "v2");
}

#[test]
fn test_origin_saved_again_unchanged_does_not_block_a_change_made_elsewhere() {
    let b = Base::new();
    b.rec(A, "s1", r());
    b.s().run();
    let t = b.last_activity(A, "s1");
    let same = b.data(A, "s1");
    fs::write(b.path(A, "s1"), &same).unwrap(); // A's app writes the same bytes again (new file time)
    set_mtime(&b.path(A, "s1"), now() - 20.0 * 60.0);
    b.rec(
        B,
        "s1",
        r().mtime(600.0)
            .archived()
            .title("archived in B")
            .extra(json!({"lastActivityAt": t})),
    );
    let mut s = b.s();
    assert!(s.plan().notes.conflicts.is_empty());
    s.run();
    assert_eq!(b.title(A, "s1"), "archived in B");
}

#[test]
fn test_our_copy_never_outranks_the_apps_file_for_the_same_activity() {
    let b = Base::new();
    let t = ms(HOUR);
    b.rec(
        A,
        "s1",
        r().mtime(30.0 * 60.0)
            .title("the app's later save")
            .extra(json!({"lastActivityAt": t, "completedTurns": 7})),
    );
    b.rec(
        B,
        "s1",
        r().mtime(20.0 * 60.0)
            .title("a copy we placed")
            .extra(json!({"lastActivityAt": t, "completedTurns": 9})),
    );
    let mut s = b.s();
    let st = winfs::stat(&b.path(B, "s1")).unwrap();
    s.touch_alive(true).unwrap();
    s.mark_ours(
        &b.path(B, "s1"),
        st.mtime_ns,
        st.size,
        Some(t as f64),
        None,
        None,
    );
    assert!(s.plan().notes.conflicts.is_empty());
    s.run();
    assert_eq!(b.title(B, "s1"), "the app's later save");
    assert_eq!(b.title(A, "s1"), "the app's later save");
}

#[test]
fn test_three_versions_converge_and_stay_put() {
    let b = Base::new();
    b.rec(A, "s1", r().last(3.0 * HOUR).mtime(4.0 * HOUR).title("a"));
    b.rec(B, "s1", r().last(HOUR).mtime(4.0 * HOUR).title("b"));
    b.rec(C, "s1", r().last(2.0 * HOUR).mtime(4.0 * HOUR).title("c"));
    b.s().run();
    let all: BTreeSet<Vec<u8>> = [A, B, C].iter().map(|x| b.data(*x, "s1")).collect();
    assert_eq!(all, BTreeSet::from([b.data(B, "s1")]));
    assert_eq!(b.title(A, "s1"), "b");
    assert_eq!(b.s().run().sum(), 0);
}

#[test]
fn test_identical_content_only_aligns_time() {
    let b = Base::new();
    b.rec(A, "s1", r().mtime(2.0 * HOUR));
    fs::copy(b.path(A, "s1"), b.path(B, "s1")).unwrap();
    set_mtime(&b.path(B, "s1"), now() - 5.0 * HOUR);
    let done = b.s().run();
    assert_eq!(done.aligned, 1);
    assert_eq!(mtime_ns(&b.path(B, "s1")), mtime_ns(&b.path(A, "s1")));
    assert!(b.kept().is_empty());
    assert_eq!(b.s().run().sum(), 0);
}
