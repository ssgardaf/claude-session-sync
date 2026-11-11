use std::io::{Read, Write};
use std::os::windows::fs::OpenOptionsExt;

use serde_json::{json, Value};

use super::*;
use crate::consts::{KEEP_BACKUPS, MANIFEST_EVERY};

fn zips(b: &Base) -> Vec<String> {
    listdir(&b.home.join("backups"))
        .into_iter()
        .filter(|n| n.ends_with(".zip"))
        .collect()
}

fn day(seconds_ago: f64) -> String {
    winfs::local_format(now() - seconds_ago, "%Y%m%d")
}

#[test]
fn test_backup_is_taken_before_the_first_write_once_a_day() {
    let b = Base::new();
    b.rec(A, "s1", r());
    b.s().run();
    let found = zips(&b);
    assert_eq!(found.len(), 1);
    let mut z =
        zip::ZipArchive::new(fs::File::open(b.home.join("backups").join(&found[0])).unwrap())
            .unwrap();
    let names: Vec<String> = z.file_names().map(String::from).collect();
    let mut first = Vec::new();
    z.by_index(0).unwrap().read_to_end(&mut first).unwrap();
    assert_eq!(first, b.data(A, "s1"));
    assert_eq!(
        names
            .iter()
            .filter(|n| n.ends_with("local_s1.json"))
            .count(),
        1
    ); // the state before writing
    assert_eq!(names[0], format!("{}/{}/local_s1.json", A.0, A.1));
    b.rec(A, "s2", r());
    b.s().run();
    assert_eq!(listdir(&b.home.join("backups")).len(), 1);
}

#[test]
fn test_first_backup_is_kept_for_good() {
    let b = Base::new();
    let bdir = b.home.join("backups");
    fs::create_dir_all(&bdir).unwrap();
    for d in 1..21 {
        fs::write(bdir.join(format!("200001{:02}-000000.zip", d)), b"").unwrap();
    }
    b.rec(A, "s1", r());
    b.s().run();
    let left = listdir(&bdir);
    assert_eq!(left.len(), 1 + KEEP_BACKUPS);
    assert_eq!(left[0], "20000101-000000.zip");
}

#[test]
fn test_files_with_impossible_dates_do_not_break_the_backup() {
    let b = Base::new();
    b.rec(A, "s1", r());
    for (name, when) in [
        ("deleted_old", 157_766_400.0),
        ("deleted_far", 7_258_118_400.0),
    ] {
        // 1975 and 2200
        let p = b.dir(A).join(name);
        fs::write(&p, b"1").unwrap();
        set_mtime(&p, when);
    }
    let done = b.s().run();
    assert_eq!((done.add, done.failed), (2, 0));
}

#[test]
fn test_no_write_when_the_backup_cannot_be_made() {
    let b = Base::new();
    b.rec(A, "s1", r());
    fs::create_dir_all(&b.home).unwrap();
    fs::write(b.home.join("backups"), b"").unwrap(); // a file where the folder should be
    let mut s = b.s();
    let done = s.run();
    assert_eq!(done.add, 0);
    assert_eq!(done.deferred, 2);
    assert!(!b.has(B, "s1"));
    assert!(b.said("aborted") > 0);
    assert!(s
        .last_error
        .as_deref()
        .unwrap_or("")
        .contains("nothing written"));
}

#[test]
fn test_no_write_when_one_file_cannot_be_backed_up() {
    let b = Base::new();
    b.rec(A, "s1", r());
    let blocker = b.dir(A).join("scheduled-tasks.json");
    fs::write(&blocker, b"").unwrap();
    let mut s = b.s();
    s.hooks.read_fault = Some(Box::new(move |p| p == blocker));
    s.hooks.sleep = Some(Box::new(|_| {}));
    let done = s.run();
    assert_eq!((done.add, done.deferred), (0, 2));
    assert!(!b.has(B, "s1"));
    assert!(zips(&b).is_empty());
    assert_eq!(b.s().run().add, 2); // and it recovers on the next pass
}

#[test]
fn test_version_the_app_wrote_is_kept_before_it_is_replaced_but_our_own_copies_are_not() {
    let b = Base::new();
    b.rec(A, "s1", r().last(600.0).mtime(900.0));
    b.rec(
        B,
        "s1",
        r().last(5.0 * HOUR)
            .mtime(4.0 * HOUR)
            .title("written by the app"),
    );
    let app_version = b.data(B, "s1");
    b.s().run();
    let kept = b.kept();
    assert_eq!(kept.len(), 1);
    assert_eq!(fs::read(&kept[0]).unwrap(), app_version);
    b.rec(A, "s1", r().last(300.0).mtime(200.0)); // the session moves on in A
    assert_eq!(b.s().run().update, 2); // a new process: relies on the saved manifest
    assert_eq!(b.kept().len(), 1); // B held our own copy: nothing new to keep
    b.rec(B, "s1", r().last(250.0).mtime(240.0).title("app again")); // the app writes B
    b.rec(A, "s1", r().last(100.0).mtime(90.0));
    b.s().run();
    assert_eq!(b.kept().len(), 2);
}

#[test]
fn test_everything_can_be_undone() {
    let b = Base::new();
    b.install(2.0 * HOUR, 0.0);
    b.rec(A, "s1", r().last(600.0).mtime(900.0));
    b.rec(
        B,
        "s1",
        r().last(5.0 * HOUR)
            .mtime(4.0 * HOUR)
            .title("B's own version"),
    );
    b.rec(A, "s2", r());
    b.rec(B, "s3", r().cli("c3").last(HOUR));
    b.rec(B, "s4", r().raw(b"").mtime(HOUR));
    b.rec(A, "s4", r());
    b.delete(A, "s3", 600.0, Some("c3"));
    let (before, before_times) = (b.contents(), b.snapshot(&b.root));
    let start = now() - 1.0;
    let done = b.s().run();
    assert_eq!(
        (done.add, done.update, done.repair, done.quarantine),
        (4, 1, 1, 1)
    );
    assert_ne!(before, b.contents());
    let result = b.s().undo(start).unwrap();
    assert_eq!(before, b.contents());
    assert_eq!(before_times, b.snapshot(&b.root));
    assert_eq!(result.left_changed_since, 0);
}

#[test]
fn test_undo_leaves_alone_what_the_app_changed_afterwards() {
    let b = Base::new();
    b.rec(A, "s1", r());
    let start = now() - 1.0;
    b.s().run();
    b.rec(
        B,
        "s1",
        r().last(1.0).mtime(1.0).title("the app took it over"),
    );
    let mine = b.data(B, "s1");
    let result = b.s().undo(start).unwrap();
    assert_eq!(b.data(B, "s1"), mine);
    assert!(!b.has(C, "s1"));
    assert_eq!((result.removed, result.left_changed_since), (1, 1));
}

#[test]
fn test_reading_does_not_block_the_app_from_saving_or_deleting() {
    let b = Base::new();
    let p = b.rec(A, "s1", r());
    {
        let mut fh = winfs::open_shared(&p).unwrap();
        {
            // the app's fallback when its rename is refused: write in place (it shares read and write only)
            let mut app = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .share_mode(0x1 | 0x2)
                .open(&p)
                .unwrap();
            app.write_all(b"X").unwrap();
        }
        let mut one = [0u8; 1];
        fh.read_exact(&mut one).unwrap();
        assert_eq!(&one, b"X");
        fs::remove_file(&p).unwrap(); // deleting is allowed too (an open without delete sharing would refuse it)
    }
    assert!(!p.exists());
}

#[test]
fn test_half_written_file_is_never_copied() {
    let b = Base::new();
    let whole = json!({"sessionId": "local_s1", "cliSessionId": "c1", "lastActivityAt": ms(HOUR)})
        .to_string()
        .into_bytes();
    b.rec(A, "s1", r().cli("c1").raw(&whole[..whole.len() / 2])); // caught in the middle of an in-place write
    assert_eq!(b.s().run().sum(), 0);
    b.rec(A, "s1", r().cli("c1").raw(&whole)); // the write finished
    assert_eq!(b.s().run().add, 2);
}

#[test]
fn test_destination_something_else_just_wrote_is_left_alone() {
    let b = Base::new();
    b.rec(A, "s1", r().last(1.0).mtime(600.0));
    b.rec(B, "s1", r().last(HOUR).mtime(5.0)); // stale content, written 5 s ago
    let stale = b.data(B, "s1");
    let done = b.s().run();
    assert_eq!(done.deferred, 1);
    assert_eq!(b.data(B, "s1"), stale);
    set_mtime(&b.path(B, "s1"), now() - 900.0);
    assert_eq!(b.s().run().update, 1);
}

#[test]
fn test_file_dated_in_the_future_is_not_waited_on_forever() {
    let b = Base::new();
    b.rec(A, "s1", r().last(60.0).mtime(600.0));
    b.rec(B, "s1", r().last(HOUR).mtime(-3.0 * HOUR)); // stale content, clock was wrong when it was written
    let done = b.s().run(); // same title, same archived state: only progress differs
    assert_eq!((done.update, done.deferred, done.failed), (1, 0, 0));
    assert_eq!(b.kept().len(), 1);
    b.rec(
        B,
        "s2",
        r().last(HOUR).mtime(-3.0 * HOUR).title("renamed here"),
    );
    b.rec(A, "s2", r().last(60.0).mtime(600.0));
    let mut s = b.s();
    assert_eq!(s.plan().notes.conflicts.len(), 1); // a user decision is at stake: kept, and not retried
    let done = s.run();
    assert_eq!((done.update, done.deferred, done.failed), (0, 0, 0));
}

#[test]
fn test_undo_also_takes_back_copies_the_app_merely_saved_again() {
    let b = Base::new();
    b.rec(A, "s1", r());
    let before = b.contents();
    let start = now() - 1.0;
    b.s().run();
    let same = b.data(B, "s1");
    fs::write(b.path(B, "s1"), &same).unwrap(); // at sign-in the app writes the same bytes back
    let result = b.s().undo(start).unwrap();
    assert_eq!((result.removed, result.left_changed_since), (2, 0));
    assert_eq!(before, b.contents());
}

#[test]
fn test_every_write_of_ours_is_on_record_at_once() {
    let b = Base::new();
    let mut s = b.s();
    s.touch_alive(true).unwrap();
    let p = b.path(B, "s1");
    s.mark_ours(&p, 111, 22, Some(3.0), Some("abc"), None); // no end-of-pass save: as if killed right here
    assert_eq!(
        b.s().base(&p),
        Some(vec![
            json!(111),
            json!(22),
            json!(3),
            json!("abc"),
            Value::Null
        ])
    );
    s.forget(&p);
    assert_eq!(b.s().base(&p), None);
    s.mark_ours(&p, 5, 6, None, None, None);
    s.save_manifest().unwrap(); // folded into manifest.json, the running list emptied
    assert_eq!(fs::metadata(b.home.join("manifest.log")).unwrap().len(), 0);
    assert_eq!(
        b.s().base(&p),
        Some(vec![
            json!(5),
            json!(6),
            Value::Null,
            Value::Null,
            Value::Null
        ])
    );
}

#[test]
fn test_copy_the_app_saved_again_unchanged_still_counts_as_ours() {
    let b = Base::new();
    b.rec(A, "s1", r().last(2.0 * HOUR).mtime(2.0 * HOUR));
    b.s().run();
    let same = b.data(B, "s1");
    fs::write(b.path(B, "s1"), &same).unwrap();
    set_mtime(&b.path(B, "s1"), now() - 600.0);
    b.rec(
        A,
        "s1",
        r().last(HOUR).mtime(50.0 * 60.0).title("A went on"),
    );
    assert_eq!(b.s().run().update, 2);
    assert!(b.kept().is_empty()); // nothing unique was in B's file
}

#[test]
fn test_broken_file_is_mended_whatever_the_sessions_age() {
    let b = Base::new();
    b.rec(A, "s1", r().last(90.0 * DAY).archived());
    b.rec(B, "s1", r().raw(b"").mtime(HOUR));
    let done = b.s().run();
    assert_eq!(done.repair, 1);
    assert!(!b.has(C, "s1")); // too old an archived session to hand out anew
}

#[test]
fn test_preview_says_how_many_sessions_are_everywhere() {
    let b = Base::new();
    b.rec(A, "s1", r());
    b.rec(A, "old", r().last(90.0 * DAY).archived());
    let mut s = b.s();
    assert_eq!(s.plan().notes.everywhere, 1);
    s.run();
    assert_eq!(s.plan().notes.everywhere, 1);
}

#[test]
fn test_undo_works_even_if_the_list_of_our_writes_was_lost() {
    let b = Base::new();
    b.rec(A, "s1", r());
    let before = b.contents();
    let start = now() - 1.0;
    b.s().run();
    fs::remove_file(b.home.join("manifest.json")).unwrap(); // as after a kill in the middle of a pass
    let result = b.s().undo(start).unwrap();
    assert_eq!(result.removed, 2);
    assert_eq!(before, b.contents());
}

#[test]
fn test_list_of_our_writes_is_saved_during_a_long_pass() {
    let b = Base::new();
    for i in 0..MANIFEST_EVERY + 5 {
        b.rec(A, &format!("s{}", i), r());
    }
    let mut s = b.s();
    let plan = s.plan();
    let seen: Rc<RefCell<Vec<Option<usize>>>> = Rc::new(RefCell::new(Vec::new()));
    let (log, manifest) = (seen.clone(), b.home.join("manifest.json"));
    s.hooks.before_copy = Some(Box::new(move |made| {
        if made == MANIFEST_EVERY + 2 {
            // what a kill right here would leave behind
            let saved: Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
            log.borrow_mut()
                .push(Some(saved.as_object().unwrap().len()));
        } else {
            log.borrow_mut().push(None);
        }
    }));
    s.apply(&plan.acts).unwrap();
    assert_eq!(seen.borrow()[MANIFEST_EVERY + 2], Some(MANIFEST_EVERY));
}

#[test]
fn list_of_our_writes_that_cannot_be_saved_midway_does_not_count_against_the_item() {
    let b = Base::new();
    for i in 0..MANIFEST_EVERY + 5 {
        b.rec(A, &format!("s{}", i), r());
    }
    let mut s = b.s();
    let plan = s.plan();
    assert_eq!(plan.acts.len(), 2 * (MANIFEST_EVERY + 5));
    // while the 50th item is done, something is in the way of manifest.json; it is gone again
    // before the save after the 100th
    let manifest = b.home.join("manifest.json");
    s.hooks.before_copy = Some(Box::new(move |made| {
        if made == MANIFEST_EVERY - 1 {
            fs::create_dir(&manifest).unwrap();
        } else if made == MANIFEST_EVERY {
            fs::remove_dir(&manifest).unwrap();
        }
    }));
    let done = s.apply(&plan.acts).unwrap();
    // (in the Python version the 50th item, although written, was counted as failed and put on back-off)
    assert_eq!((done.add, done.failed), (2 * (MANIFEST_EVERY + 5), 0));
    assert!(s.retry.is_empty());
    assert!(s.last_error.is_none());
    assert_eq!(b.said("the list of our writes could not be saved yet"), 1);
    // nothing was lost: the list is complete at the end of the pass
    let saved: Value =
        serde_json::from_slice(&fs::read(b.home.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(saved.as_object().unwrap().len(), 2 * (MANIFEST_EVERY + 5));
    assert_eq!(b.s().run().sum(), 0);
}

#[test]
fn test_kept_versions_are_capped_in_size_but_todays_stay() {
    let b = Base::new();
    let days = [day(2.0 * DAY), day(DAY), day(0.0)];
    for d in &days {
        let dir = b.home.join("replaced").join(d);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("x"), b"0123456789abcdef").unwrap();
    }
    let mut s = b.s();
    s.max_replaced_bytes = 10;
    s.sweep();
    assert_eq!(listdir(&b.home.join("replaced")), vec![days[2].clone()]);
}

#[test]
fn test_source_still_being_written_waits() {
    let b = Base::new();
    b.rec(A, "s1", r().mtime(0.0));
    let done = b.s().run();
    assert_eq!(done.add, 0);
    assert_eq!(done.deferred, 2);
}

#[test]
fn test_change_between_planning_and_writing_is_not_overwritten() {
    let b = Base::new();
    b.rec(A, "s1", r().last(60.0).mtime(600.0));
    b.rec(B, "s1", r().last(HOUR).mtime(HOUR));
    let mut s = b.s();
    let plan = s.plan();
    b.rec(
        B,
        "s1",
        r().last(1.0).mtime(900.0).title("app wrote this meanwhile"),
    );
    let fresh = b.data(B, "s1");
    let done = s.apply(&plan.acts).unwrap();
    assert_eq!(done.deferred, 1);
    assert_eq!(b.data(B, "s1"), fresh);
}

#[test]
fn test_own_copies_do_not_block_the_next_update_even_after_a_restart() {
    let b = Base::new();
    b.rec(A, "s1", r().last(600.0).mtime(30.0)); // active session, saved 30 s ago
    assert_eq!(b.s().run().add, 2);
    b.rec(A, "s1", r().last(20.0).mtime(10.0)); // ...and again 10 s ago
    assert_eq!(b.s().run().update, 2); // a fresh process, as after a daemon restart
    assert_eq!(b.data(B, "s1"), b.data(A, "s1"));
}

#[test]
fn test_no_temp_files_left_and_stale_ones_swept() {
    let b = Base::new();
    b.rec(A, "s1", r());
    let mut s = b.s();
    s.run();
    for leaf in [A, B, C] {
        assert!(!listdir(&b.dir(leaf)).iter().any(|n| n.ends_with(".tmp")));
    }
    let old = b.dir(B).join(".sync-dead.tmp");
    let new = b.dir(B).join(".sync-live.tmp");
    for (p, ago) in [(&old, 3600.0), (&new, 5.0)] {
        fs::write(p, b"").unwrap();
        set_mtime(p, now() - ago);
    }
    s.sweep();
    assert!(!old.exists());
    assert!(new.exists());
}

#[test]
fn test_kept_versions_expire_after_their_retention() {
    let b = Base::new();
    for d in [String::from("20200101"), day(0.0)] {
        let dir = b.home.join("replaced").join(d);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("x"), b"").unwrap();
    }
    b.s().sweep();
    assert_eq!(listdir(&b.home.join("replaced")), vec![day(0.0)]);
}

#[test]
fn test_other_files_in_the_folder_are_untouched() {
    let b = Base::new();
    b.rec(A, "s1", r());
    let mut other = Vec::new();
    for leaf in [A, B] {
        for n in ["scheduled-tasks.json", "archived-sessions.idx"] {
            let p = b.dir(leaf).join(n);
            fs::write(&p, b"{\"v\":1,\"archived\":[]}").unwrap();
            other.push((p.clone(), mtime_ns(&p)));
        }
    }
    b.s().run();
    for (p, m) in other {
        assert_eq!(mtime_ns(&p), m);
    }
}
