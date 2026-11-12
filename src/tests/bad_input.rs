use std::collections::BTreeSet;
#[cfg(windows)]
use std::process::Command;

use serde_json::json;

use super::*;

fn names(s: &crate::sync::Sync) -> Vec<String> {
    s.leaves().into_iter().map(|(n, _)| n).collect()
}

#[test]
fn test_unreadable_source_is_ignored() {
    let b = Base::new();
    b.rec(A, "s1", r().raw(b"{\"sessionId\":\"local_s1\",\"cliSess"));
    assert_eq!(b.s().run().sum(), 0);
    assert!(!b.has(B, "s1"));
}

#[test]
fn test_broken_copy_is_repaired_and_the_broken_file_kept() {
    let b = Base::new();
    b.rec(A, "s1", r());
    b.rec(
        B,
        "s1",
        r().raw(b"{\"sessionId\":\"local_s1\",\"cli").mtime(HOUR),
    );
    let done = b.s().run();
    assert_eq!(done.repair, 1);
    assert_eq!(b.data(B, "s1"), b.data(A, "s1"));
    let kept = b.kept();
    assert_eq!(kept.len(), 1);
    assert_eq!(
        fs::read(&kept[0]).unwrap(),
        b"{\"sessionId\":\"local_s1\",\"cli"
    );
}

#[test]
fn test_files_we_do_not_understand_are_never_replaced() {
    let b = Base::new();
    b.rec(A, "s1", r());
    b.rec(A, "s2", r());
    b.rec(A, "s3", r());
    let odd = json!({"sessionId": "local_s1", "format": 2, "lastActivityAt": ms(1.0)})
        .to_string()
        .into_bytes();
    let null = json!({"sessionId": "local_s2", "cliSessionId": null, "lastActivityAt": ms(1.0)})
        .to_string()
        .into_bytes();
    b.rec(B, "s1", r().raw(&odd).mtime(HOUR)); // a newer format without the field we need
    b.rec(B, "s2", r().raw(&null).mtime(HOUR));
    b.rec(
        B,
        "s3",
        r().last(1.0)
            .mtime(HOUR)
            .extra(json!({"pad": "x".repeat(4000)})),
    );
    let big_data = b.data(B, "s3");
    let mut s = b.s();
    s.max_record_bytes = 2000; // B's s3 is now "too large to read"
    assert_eq!(s.plan().notes.unreadable_there, 3);
    s.run();
    assert_eq!(
        (b.data(B, "s1"), b.data(B, "s2"), b.data(B, "s3")),
        (odd, null, big_data)
    );
    assert!(b.has(C, "s1") && b.has(C, "s2") && b.has(C, "s3"));
    assert!(b.kept().is_empty());
}

#[test]
fn test_file_locked_by_another_process_is_not_treated_as_missing() {
    let b = Base::new();
    b.rec(A, "warmup", r());
    b.s().run(); // today's backup exists already
    b.rec(A, "s1", r().last(2.0 * HOUR));
    let locked = b.rec(
        B,
        "s1",
        r().last(60.0).mtime(HOUR).title("newer but locked"),
    );
    let mine = b.data(B, "s1");
    let mut s = b.s();
    s.hooks.read_fault = Some(Box::new(move |p| p == locked));
    let notes = s.plan().notes;
    let done = s.run();
    assert_eq!(notes.unreadable_there, 1);
    assert_eq!(b.data(B, "s1"), mine);
    assert_eq!((done.repair, done.update, done.add), (0, 0, 1)); // only C, which had none
    assert_eq!(b.s().run().update, 2); // readable again: B's newer version wins
}

#[test]
fn test_record_whose_id_does_not_match_its_name_is_ignored() {
    let b = Base::new();
    b.rec(A, "s1", r().extra(json!({"sessionId": "local_other"})));
    assert_eq!(b.s().run().sum(), 0);
}

#[test]
fn test_odd_field_types_never_stop_the_pass() {
    let b = Base::new();
    b.rec(
        A,
        "a",
        r().extra(json!({"lastActivityAt": "yesterday", "createdAt": ms(HOUR)})),
    );
    b.rec(
        A,
        "b",
        r().extra(json!({"title": 12345, "priorCliSessionIds": [{"x": 1}, 7, null, "ok-2"]})),
    );
    b.rec(
        A,
        "c",
        r().raw(b"{\"sessionId\":\"local_c\",\"cliSessionId\":\"cc\",\"lastActivityAt\":NaN,\"completedTurns\":\"many\"}").cli("cc"),
    );
    b.rec(A, "d", r().raw(b"[1,2,3]"));
    b.rec(
        A,
        "e",
        r().extra(json!({"isArchived": "yes", "unarchivedCliSessionId": 5})),
    );
    b.rec(A, "good", r());
    let done = b.s().run();
    assert_eq!(done.failed, 0);
    for fid in ["a", "b", "e", "good"] {
        assert!(b.has(B, fid), "{}", fid);
    }
    assert!(!b.has(B, "d"));
    // Here this version differs from the Python one on purpose: to Python a record with NaN in it
    // was a record like any other (and "c" was copied); the app cannot read such a file, so it is
    // now left where it is and not handed to other accounts.
    assert!(!b.has(B, "c") && !b.has(C, "c"));
}

#[test]
fn record_with_numbers_the_app_cannot_read_is_neither_copied_nor_replaced() {
    for odd in ["NaN", "Infinity", "-Infinity"] {
        let b = Base::new();
        let raw = format!(
            "{{\"sessionId\":\"local_s1\",\"cliSessionId\":\"c1\",\"title\":\"odd\",\"lastActivityAt\":{},\"cost\":{}}}",
            ms(9.0 * HOUR),
            odd
        );
        // not as a source...
        b.rec(A, "s1", r().cli("c1").raw(raw.as_bytes()));
        assert_eq!(b.s().run().sum(), 0, "{}", odd);
        assert!(!b.has(B, "s1"));
        // ...and not as a destination: a readable, more advanced copy elsewhere does not replace it
        b.rec(
            B,
            "s1",
            r().cli("c1").last(60.0).mtime(600.0).title("readable"),
        );
        let mut s = b.s();
        assert_eq!(s.plan().notes.unreadable_there, 1, "{}", odd);
        let done = s.run();
        assert_eq!(
            (done.add, done.update, done.repair, done.failed),
            (1, 0, 0, 0)
        ); // C, which had none
        assert_eq!(b.data(A, "s1"), raw.as_bytes());
        assert_eq!(b.title(C, "s1"), "readable");
        assert!(b.kept().is_empty());
    }
}

#[test]
fn id_followed_by_a_line_feed_is_not_an_id() {
    let b = Base::new();
    b.install(2.0 * HOUR, 0.0);
    // among the earlier conversation ids of a session: left out, the rest are used
    b.rec(
        B,
        "s1",
        r().cli("c1")
            .last(HOUR)
            .extra(json!({"priorCliSessionIds": ["ok-1", "bad\n", "worse\r\n"]})),
    );
    b.delete(A, "s1", 600.0, Some("c1"));
    let done = b.s().run();
    assert_eq!((done.quarantine, done.failed), (1, 0)); // (in the Python version this item failed on every pass)
    let markers: Vec<String> = listdir(&b.dir(B))
        .into_iter()
        .filter(|n| n.starts_with("deleted_"))
        .collect();
    assert_eq!(markers, vec!["deleted_c1", "deleted_ok-1", "deleted_s1"]);
    assert_eq!(b.s().run().sum(), 0);
    // as the conversation a record points at: such a record is not one we understand
    b.rec(A, "s2", r().cli("c2\n").no_transcript());
    b.rec(
        A,
        "s3",
        r().extra(json!({"unarchivedCliSessionId": "u3\n"})),
    );
    let mut s = b.s();
    let plan = s.plan();
    assert!(!plan.scan.records.contains_key("s2"));
    assert_eq!(
        plan.scan.records["s3"][0].1.ids,
        std::collections::BTreeSet::from([String::from("cli-s3")])
    );
    s.run();
    assert!(!b.has(B, "s2") && b.has(B, "s3"));
}

#[test]
fn test_ids_that_are_not_plain_are_never_used_as_file_names() {
    let b = Base::new();
    b.install(2.0 * HOUR, 0.0);
    let evil = "..\\..\\..\\evil";
    b.rec(
        B,
        "s1",
        r().cli("c1")
            .last(HOUR)
            .extra(json!({"priorCliSessionIds": [evil, "ok-1"]})),
    );
    b.rec(A, "bad", r().cli("..\\x").last(HOUR).no_transcript()); // unusable conversation id: record ignored
    b.delete(A, "s1", 600.0, Some("c1"));
    let around = |b: &Base| -> BTreeSet<String> {
        listdir(&b.tmp)
            .into_iter()
            .chain(listdir(&b.tmp.join("appdata")))
            .collect()
    };
    let before = around(&b);
    let done = b.s().run();
    assert_eq!(done.quarantine, 1);
    let markers: Vec<String> = listdir(&b.dir(B))
        .into_iter()
        .filter(|n| n.starts_with("deleted_"))
        .collect();
    assert_eq!(markers, vec!["deleted_c1", "deleted_ok-1", "deleted_s1"]);
    assert!(!b.has(C, "bad"));
    assert_eq!(before, around(&b));
}

#[test]
fn test_folders_not_named_like_uuids_are_ignored() {
    let b = Base::new();
    fs::create_dir_all(b.root.join("backup-copy").join(uid(0xd1))).unwrap();
    fs::create_dir_all(b.root.join(A.0).join("notes")).unwrap();
    b.rec(A, "s1", r());
    let mut s = b.s();
    assert_eq!(names(&s), vec![la(), lb(), lc()]);
    s.run();
    assert!(listdir(&b.root.join("backup-copy").join(uid(0xd1))).is_empty());
    assert!(listdir(&b.root.join(A.0).join("notes")).is_empty());
}

#[test]
fn test_linked_account_folder_is_skipped() {
    let b = Base::new();
    let target = b.tmp.join("elsewhere");
    fs::create_dir_all(&target).unwrap();
    let link = b.root.join(uid(0xf0));
    #[cfg(windows)]
    {
        let made = Command::new("cmd")
            .arg("/c")
            .arg("mklink")
            .arg("/J")
            .arg(&link)
            .arg(&target)
            .output();
        if !made.is_ok_and(|o| o.status.success()) {
            eprintln!("skipped: cannot create a junction here");
            return;
        }
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &link).unwrap();
    fs::create_dir_all(target.join(uid(0xf1))).unwrap();
    b.rec(A, "s1", r());
    let mut s = b.s();
    assert_eq!(names(&s), vec![la(), lb(), lc()]);
    s.run();
    assert!(listdir(&target.join(uid(0xf1))).is_empty());
    // the link itself, not what it points at
    #[cfg(windows)]
    let _ = fs::remove_dir(&link);
    #[cfg(unix)]
    let _ = fs::remove_file(&link);
}
