use serde_json::{json, Value};

use super::*;
use crate::sync::short;

#[test]
fn test_added_everywhere_byte_identical_and_idempotent() {
    let b = Base::new();
    let p = b.rec(A, "s1", r());
    let done = b.s().run();
    assert_eq!(done.add, 2);
    for leaf in [B, C] {
        assert_eq!(b.data(leaf, "s1"), b.data(A, "s1"));
        assert_eq!(mtime_ns(&b.path(leaf, "s1")), mtime_ns(&p));
    }
    let again = b.s().run();
    assert_eq!(again.sum(), 0);
}

#[test]
fn test_archived_session_is_mirrored_as_archived() {
    let b = Base::new();
    b.rec(A, "s1", r().archived());
    b.s().run();
    let j: Value = serde_json::from_slice(&b.data(B, "s1")).unwrap();
    assert_eq!(j["isArchived"], json!(true));
}

#[test]
fn test_age_limit_applies_to_the_archive_only() {
    let b = Base::new();
    b.rec(A, "live", r().last(40.0 * DAY));
    b.rec(A, "arch", r().last(40.0 * DAY).archived());
    b.s().run();
    assert!(b.has(B, "live") && b.has(C, "live")); // an open session is mirrored however old
    assert!(!b.has(B, "arch"));
    let mut s = b.s();
    s.days = 60.0;
    assert_eq!(s.run().add, 2);
}

#[test]
fn test_session_deleted_here_between_planning_and_writing_is_not_put_back() {
    let b = Base::new();
    b.rec(A, "s1", r());
    let mut s = b.s();
    let plan = s.plan();
    b.delete(B, "s1", 0.0, None); // the user deletes it in B right now
    let done = s.apply(&plan.acts).unwrap();
    assert!(!b.has(B, "s1"));
    assert_eq!((done.add, done.deferred), (1, 1));
}

#[test]
fn test_missing_transcript_not_added() {
    let b = Base::new();
    b.rec(A, "s1", r().no_transcript());
    assert_eq!(b.s().run().add, 0);
}

#[test]
fn test_session_started_after_the_transcript_listing_was_cached_is_still_added() {
    let b = Base::new();
    let mut s = b.s();
    s.run(); // caches an empty transcript listing
    b.rec(A, "s1", r());
    assert_eq!(s.run().add, 2);
}

#[test]
fn test_bom_and_non_ascii_survive() {
    let b = Base::new();
    let mut raw = b"\xef\xbb\xbf".to_vec();
    raw.extend_from_slice(
        json!({"sessionId": "local_s1", "cliSessionId": "c1", "title": "Título de sesión — 日本語", "lastActivityAt": ms(HOUR)})
            .to_string()
            .as_bytes(),
    );
    b.rec(A, "s1", r().cli("c1").raw(&raw));
    b.s().run();
    assert_eq!(b.data(B, "s1"), raw);
}

#[test]
fn test_same_conversation_under_another_id_not_duplicated() {
    let b = Base::new();
    b.rec(B, "other", r().cli("shared").last(3.0 * HOUR));
    b.rec(A, "s1", r().cli("shared"));
    b.s().run();
    assert!(!b.has(B, "s1"));
    assert!(!b.has(A, "other"));
    // the third account gets one record for that conversation, the more advanced one
    assert!(b.has(C, "s1"));
    assert!(!b.has(C, "other"));
}

#[test]
fn test_preview_writes_nothing() {
    let b = Base::new();
    b.rec(A, "s1", r());
    b.rec(B, "s1", r().last(5.0 * HOUR));
    b.tomb(C, "zzz", ms(0.0));
    let before = b.snapshot(&b.tmp);
    let plan = b.s().plan();
    assert!(!plan.acts.is_empty());
    assert_eq!(command_line(&b, &[]).0, 0);
    assert_eq!(command_line(&b, &["--verbose"]).0, 0);
    let (code, shown) = command_line(&b, &["preview", "--verbose"]);
    assert_eq!(code, 0);
    assert!(shown.contains("account folders: 3   sessions: 1"));
    // looking at the log, or asking for help, writes nothing either
    assert_eq!(command_line(&b, &["log"]).0, 0);
    assert_eq!(command_line(&b, &["--help"]).0, 0);
    assert_eq!(before, b.snapshot(&b.tmp));
    assert!(!b.home.exists());
}

#[test]
fn test_file_appearing_where_an_add_was_planned_is_not_replaced() {
    let b = Base::new();
    b.rec(A, "s1", r());
    let mut s = b.s();
    let plan = s.plan();
    b.rec(
        B,
        "s1",
        r().last(1.0).mtime(900.0).title("appeared meanwhile"),
    );
    let mine = b.data(B, "s1");
    let done = s.apply(&plan.acts).unwrap();
    assert_eq!((done.add, done.deferred), (1, 1));
    assert_eq!(b.data(B, "s1"), mine);
}

#[test]
fn test_folder_that_cannot_be_listed_is_left_out_of_the_pass() {
    let b = Base::new();
    b.rec(A, "s1", r());
    b.rec(A, "s2", r());
    b.delete(B, "s2", 5.0 * HOUR, None); // B deleted s2 long ago...
    b.rec(B, "s1", r().last(60.0).mtime(600.0).title("B is ahead"));
    let b_dir = winfs::norm(&b.dir(B));
    let mut s = b.s();
    s.hooks.list_fault = Some(Box::new(move |p| winfs::norm(p) == b_dir));
    let plan = s.plan();
    let done = s.run();
    assert_eq!(plan.notes.unlisted, vec![short(&lb())]);
    assert!(!plan.acts.iter().any(|a| a.leaf == lb()));
    assert!(!b.has(B, "s2")); // ...and it is not handed back while B is unreadable
    assert_eq!(b.title(B, "s1"), "B is ahead");
    assert_eq!(done.failed, 0);
}
