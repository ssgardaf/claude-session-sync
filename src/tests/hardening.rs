//! What must hold when things are not as they should be: a daemon that hangs before it has
//! reported, a pass that is told to end, something that is not a file where a file is
//! expected, a move that finds its place taken, and (macOS, Linux) who may read what this
//! tool writes.

use std::cell::Cell;

use serde_json::json;

#[cfg(windows)]
use super::setup::{cli, Recorder};
#[cfg(unix)]
use super::setup_unix::{cli, Recorder};
use super::*;
use crate::consts::VERSION;
use crate::daemon::acquire_lock;
use crate::winfs::{copy_then_remove, move_file};

// ---------------------------------------------------------------------- the first sign of life

/// A daemon reports before its first pass, not after it: a first pass that hangs (or only
/// takes long) is then that of a daemon known to have started.
#[test]
fn first_heartbeat_comes_before_the_first_pass() {
    let b = Base::new();
    b.rec(A, "s1", r());
    let mut s = b.s();
    let seen: Rc<RefCell<Vec<Value>>> = Rc::new(RefCell::new(Vec::new()));
    let (slot, home) = (seen.clone(), b.home.clone());
    s.hooks.before_plan = Some(Box::new(move || {
        let report = fs::read(home.join("status.json"))
            .ok()
            .and_then(|d| serde_json::from_slice::<Value>(&d).ok());
        slot.borrow_mut().push(report.unwrap_or(Value::Null));
    }));
    let began = now();
    s.watch(3600.0, 0.0, 1.0, Some(1), &mut |_| {});
    let first = seen.borrow()[0].clone();
    assert_eq!(first["pid"], json!(std::process::id())); // there before the first plan was made
    assert_eq!(first["version"], json!(VERSION));
    assert!(first["heartbeat"].as_f64().unwrap() >= began.floor());
    assert!(first["started"].as_f64().unwrap() >= began.floor());
    assert_eq!(first["last_full"], json!(0)); // ...and saying that no pass has been made yet
    assert!(b.has(B, "s1")); // the pass itself went as ever
}

/// `status` goes by who holds the lock now. The daemon marks the lock when it takes it and
/// repeats the mark in every report; a report with another mark was left by an earlier
/// daemon. The one that holds the lock has then not reported, which is "starting" for a
/// short while and "stalled" after that, and never "OK".
#[test]
fn status_does_not_say_ok_for_a_daemon_that_has_not_reported() {
    let b = Base::new();
    b.rec(A, "s1", r());
    let mut sys = Recorder::new();
    let held = acquire_lock(&b.home).unwrap().unwrap();
    let lock = b.home.join("daemon.lock");
    let mark = |lock: &Path| crate::daemon::lock_mark(&fs::metadata(lock).unwrap()).unwrap();
    // the lock was just taken, nothing has been reported yet
    let (code, out) = cli(&b, &mut sys, &["status"]);
    assert_eq!(code, 0);
    assert!(
        out[0].starts_with("STARTING  a daemon has taken the lock and has not reported yet | "),
        "{}",
        out[0]
    );
    assert!(out
        .iter()
        .any(|l| l.starts_with("daemon: holds the lock") && l.ends_with("no report from it yet")));
    // ...nor does a report left by the daemon before it count, however fresh it looks: not
    // its version, not its errors, not its settings
    let old = json!({"pid": 1, "version": "0.0.1", "started": (now() - 9000.0) as i64,
                     "heartbeat": now() as i64, "failed": 3, "last_error": "an old error",
                     "delete_sync": true, "days": 7, "lock_stamp": mark(&lock) - 1});
    fs::write(b.home.join("status.json"), old.to_string()).unwrap();
    let (code, out) = cli(&b, &mut sys, &["status"]);
    assert_eq!(code, 0);
    assert!(out[0].starts_with("STARTING  "), "{}", out[0]);
    assert!(out[0].ends_with("| 0 errors"), "{}", out[0]);
    assert!(!out.iter().any(|l| l.contains("0.0.1")));
    assert!(out.iter().any(|l| l.contains("older than 30 days"))); // (not the 7 of that report)
                                                                   // a few minutes later it still has not reported: it is stuck, and that is said
    set_mtime(&lock, now() - 200.0);
    let (code, out) = cli(&b, &mut sys, &["status"]);
    assert_eq!(code, 1);
    assert!(
        out[0].starts_with("STALLED  the daemon has not reported since it started 2"),
        "{}",
        out[0]
    );
    // it reports, with the mark that is on the lock: all is well
    let report = |heartbeat_ago: f64, stamp: i64| {
        let st = json!({"pid": 4242, "version": VERSION, "started": (now() - 200.0) as i64,
                        "heartbeat": (now() - heartbeat_ago) as i64, "lock_stamp": stamp});
        fs::write(b.home.join("status.json"), st.to_string()).unwrap();
    };
    report(0.0, mark(&lock));
    let (code, out) = cli(&b, &mut sys, &["status"]);
    assert_eq!(code, 0);
    assert!(out[0].starts_with(&format!("OK  daemon {} running", VERSION)));

    // The clock is set while the daemon runs. Which report is whose does not hang on what
    // time it is: the mark on the lock is a number to be found again, not a time to be
    // earlier or later than.
    // set back by an hour: the lock's mark is "in the future", the reports go on as ever
    set_mtime(&lock, now() + 3600.0);
    report(0.0, mark(&lock));
    let (code, out) = cli(&b, &mut sys, &["status"]);
    assert_eq!(code, 0, "{}", out[0]);
    assert!(out[0].starts_with("OK  "), "{}", out[0]);
    // set forward by three days: the lock's mark is long ago, the reports go on as ever
    set_mtime(&lock, now() - 3.0 * DAY);
    report(0.0, mark(&lock));
    let (code, out) = cli(&b, &mut sys, &["status"]);
    assert_eq!(code, 0, "{}", out[0]);
    assert!(out[0].starts_with("OK  "), "{}", out[0]);
    // ...and neither way round does an earlier daemon's report pass for this one's: a lock
    // that was taken "before" that report (the clock went back in between), no report yet
    report(0.0, mark(&lock) + 7);
    set_mtime(&lock, now() - 30.0);
    let (code, out) = cli(&b, &mut sys, &["status"]);
    assert_eq!(code, 0);
    assert!(out[0].starts_with("STARTING  "), "{}", out[0]);
    // a daemon that has not reported, seen after the clock went back: still starting, not stalled
    set_mtime(&lock, now() + 3600.0);
    let (code, out) = cli(&b, &mut sys, &["status"]);
    assert_eq!(code, 0);
    assert!(out[0].starts_with("STARTING  "), "{}", out[0]);
    // its own report that has gone quiet is what "stalled" is, as it always was
    report(2000.0, mark(&lock));
    let (code, out) = cli(&b, &mut sys, &["status"]);
    assert_eq!(code, 1);
    assert!(
        out[0].starts_with(&format!(
            "STALLED  daemon {} has not reported for 20",
            VERSION
        )),
        "{}",
        out[0]
    );
    // a report without a mark is that of a version before there were marks: taken as it is
    let st = json!({"pid": 4242, "version": "0.1.2", "heartbeat": now() as i64});
    fs::write(b.home.join("status.json"), st.to_string()).unwrap();
    let (code, out) = cli(&b, &mut sys, &["status"]);
    assert_eq!(code, 0);
    assert!(out[0].starts_with("OK  daemon 0.1.2 running"), "{}", out[0]);
    drop(held);

    // and the daemon itself marks the lock when it takes it, and reports at once with that mark
    let b = Base::new();
    b.rec(A, "s1", r());
    fs::create_dir_all(&b.home).unwrap();
    fs::write(b.home.join("daemon.lock"), b"").unwrap();
    set_mtime(&b.home.join("daemon.lock"), now() - 5000.0);
    let mut s = b.s();
    let lock = crate::daemon::lock_for_daemon(&mut s, &|_| {})
        .unwrap()
        .unwrap();
    assert!(winfs::stat(&b.home.join("daemon.lock")).unwrap().mtime() > now() - 60.0);
    s.watch(3600.0, 0.0, 1.0, Some(1), &mut |_| {});
    assert_eq!(
        b.status()["lock_stamp"].as_i64(),
        Some(mark(&b.home.join("daemon.lock")))
    );
    let (code, out) = cli(&b, &mut sys, &["status"]);
    assert_eq!(code, 0);
    assert!(out[0].starts_with("OK  "), "{}", out[0]);
    drop(lock);
}

/// A request to settle a conflict that a pass did not get to, because the daemon was told
/// to end in the middle of it, stays on record: the next daemon carries it out. (It used to
/// be closed with the pass, as if it had been looked at and found settled.)
#[test]
fn a_request_the_pass_did_not_get_to_stays_on_record() {
    let b = Base::new();
    fs::remove_dir(b.dir(C)).unwrap();
    for n in 0..6 {
        b.rec(A, &format!("s{}", n), r());
    }
    // a conflict: A's copy is further along, B's was un-archived later
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
    let mut s = b.s();
    assert_eq!(s.plan().notes.conflicts.len(), 1);
    assert_eq!(s.resolve("abcdef").unwrap(), Some(String::from("abcdef-1")));
    let asked = b.state()["resolve"].clone();
    assert_eq!(asked.as_object().unwrap().len(), 1);
    // where in the pass the request's turn is, and the pass is cut just before it
    let plan = s.plan();
    let turn = plan.acts.iter().position(|a| a.fid == "abcdef-1").unwrap();
    let before_it = if turn == 0 {
        // (it would be first: have the pass cut before anything is done at all)
        fs::write(b.home.join("cut-at-once"), b"").unwrap();
        0
    } else {
        turn - 1
    };
    let flag = b.home.join("stop.flag");
    let (at, at_once) = (flag.clone(), turn == 0);
    s.hooks.before_copy = Some(Box::new(move |copies| {
        if copies == before_it && !at_once {
            fs::write(&at, b"").unwrap(); // `stop`, during the change before the request's
        }
    }));
    if at_once {
        fs::remove_file(b.home.join("cut-at-once")).unwrap();
        s.hooks.before_plan = Some(Box::new({
            let at = flag.clone();
            move || fs::write(&at, b"").unwrap()
        }));
    }
    s.watch(3600.0, 1.0, 1.0, Some(5), &mut |_| {});
    assert!(s.cut_short);
    assert_eq!(b.title(B, "abcdef-1"), "older, un-archived later"); // not carried out...
    assert_eq!(b.state()["resolve"], asked); // ...and still asked for
                                             // the log says what happened: what was done, and that the pass was stopped, not "in sync"
    assert_eq!(b.said("in sync"), 0);
    assert_eq!(b.said("pass stopped before it was done: "), 1);
    assert_eq!(b.said("synced: added "), usize::from(turn > 0));
    assert_eq!(b.said("full pass: "), 0);
    // the next daemon carries it out, and only then is the request done with
    let mut s = b.s();
    s.watch(3600.0, 0.0, 1.0, Some(1), &mut |_| {});
    assert_eq!(b.title(B, "abcdef-1"), "further along");
    assert_eq!(b.state()["resolve"], json!({}));
    assert_eq!(s.plan().notes.conflicts.len(), 0);
    // one that was carried out before the cut is closed by that very pass, as ever
    let b = Base::new();
    fs::remove_dir(b.dir(C)).unwrap();
    b.rec(
        A,
        "abcdef-1",
        r().last(3.0 * DAY).mtime(2.0 * DAY).archived(),
    );
    b.rec(B, "abcdef-1", r().last(5.0 * DAY).mtime(DAY));
    let mut s = b.s();
    s.resolve("abcdef").unwrap();
    let flag = b.home.join("stop.flag");
    s.hooks.before_copy = Some(Box::new(move |_| fs::write(&flag, b"").unwrap()));
    for n in 0..3 {
        b.rec(A, &format!("s{}", n), r());
    }
    s.watch(3600.0, 1.0, 1.0, Some(5), &mut |_| {});
    if b.data(B, "abcdef-1") == b.data(A, "abcdef-1") {
        assert_eq!(b.state()["resolve"], json!({}));
    } else {
        assert_eq!(b.state()["resolve"].as_object().unwrap().len(), 1);
    }
}

// ---------------------------------------------------------------------- told to end

/// A daemon that is told to end in the middle of a pass finishes the change it is making,
/// begins no other, and leaves. What was not done is done by the next daemon.
#[test]
fn a_pass_is_ended_between_two_changes_when_the_daemon_is_told_to_end() {
    let b = Base::new();
    fs::remove_dir(b.dir(C)).unwrap();
    b.rec(B, "own", r()); // so that B is a folder in use
    for n in 0..8 {
        b.rec(A, &format!("s{}", n), r());
    }
    let mut s = b.s();
    let flag = b.home.join("stop.flag");
    let told = Rc::new(Cell::new(false));
    let (slot, at) = (told.clone(), flag.clone());
    s.hooks.before_copy = Some(Box::new(move |copies| {
        if copies == 3 {
            fs::write(&at, b"").unwrap(); // `stop`, while the fourth change is being made
            slot.set(true);
        }
    }));
    s.watch(3600.0, 1.0, 1.0, Some(5), &mut |_| {});
    assert!(told.get());
    assert!(s.cut_short);
    let mirrored = |b: &Base| {
        listdir(&b.dir(B))
            .iter()
            .filter(|n| n.starts_with("local_s"))
            .count()
            + usize::from(b.has(A, "own"))
    };
    assert_eq!(mirrored(&b), 4); // the three before it and the one under way; no fifth
    assert!(b.said("stopped on request") == 1);
    assert!(!flag.exists());
    // nothing half-done: no temp file, every copy whole
    for leaf in [A, B] {
        assert!(!listdir(&b.dir(leaf))
            .iter()
            .any(|n| n.starts_with(".sync-")));
    }
    for name in listdir(&b.dir(B)) {
        if name != "local_own.json" {
            assert_eq!(
                fs::read(b.dir(B).join(&name)).unwrap(),
                fs::read(b.dir(A).join(&name)).unwrap()
            );
        }
    }
    // the next daemon does the rest
    let mut s = b.s();
    s.watch(3600.0, 0.0, 1.0, Some(1), &mut |_| {});
    assert_eq!(mirrored(&b), 9);
    // a single pass (`sync`) is not a daemon's: it is not cut short by a flag left lying about
    let b = Base::new();
    for n in 0..4 {
        b.rec(A, &format!("s{}", n), r());
    }
    fs::create_dir_all(&b.home).unwrap();
    fs::write(b.home.join("stop.flag"), b"").unwrap();
    let mut s = b.s();
    assert_eq!(s.run().add, 8);
    assert!(!s.cut_short);
}

// ---------------------------------------------------------------------- not a file

/// A folder under a record's name is not a record, torn or otherwise: it is left alone, and
/// nothing is tried on it (so nothing fails either).
#[test]
fn folder_under_a_records_name_is_left_alone() {
    let b = Base::new();
    b.rec(A, "s1", r());
    fs::create_dir_all(b.path(B, "s1").join("inside")).unwrap();
    let mut s = b.s();
    let plan = s.plan();
    assert_eq!(plan.notes.unreadable_there, 1);
    assert_eq!(plan.acts.len(), 1); // only C gets it
    let done = s.run();
    assert_eq!((done.add, done.repair, done.failed), (1, 0, 0));
    assert!(b.path(B, "s1").join("inside").is_dir());
    assert!(s.retry.is_empty());
    // ...and it does not keep the daily backup from being made
    assert_eq!(listdir(&b.home.join("backups")).len(), 1);
}

/// What is in the store and is not an ordinary file is never read: not by the scan, not by
/// the backup. A pipe nobody writes to would hold the read for ever (a daemon that hangs in
/// its first pass), and a device has no end.
#[cfg(unix)]
#[test]
fn pipes_and_devices_in_the_store_never_hold_a_pass() {
    use std::os::unix::fs::{symlink, FileTypeExt};
    fn fifo(path: &Path) {
        let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        // SAFETY: a NUL-terminated string that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
    }
    let b = Base::new();
    b.rec(A, "s1", r());
    b.rec(A, "s2", r());
    b.rec(A, "s3", r());
    b.rec(B, "own", r());
    fs::remove_dir(b.dir(C)).unwrap();
    // in the folder records are read from, and in the one they go to:
    fifo(&b.dir(A).join("some.fifo")); // a pipe under any name
    fifo(&b.dir(B).join("local_s1.json")); // a pipe under a record's name
    fifo(&b.dir(B).join("deleted_s9")); // a pipe under a deletion marker's name
    symlink("/dev/zero", b.dir(B).join("local_s2.json")).unwrap(); // a link to a device, likewise
    symlink("/dev/zero", b.dir(A).join("endless")).unwrap();
    symlink(b.dir(A).join("some.fifo"), b.dir(B).join("link-to-pipe")).unwrap();
    fifo(&b.root.join("top-level.fifo"));
    let done = Rc::new(Cell::new(false));
    let mut s = b.s();
    // (were any of them opened as a file, this test would not come back)
    let counts = s.run();
    done.set(true);
    assert!(done.get());
    assert_eq!((counts.add, counts.repair, counts.failed), (2, 0, 0)); // s3 to B, own to A
    assert_eq!(s.plan().notes.unreadable_there, 2); // s1 and s2 in B: not files, left alone
    for (leaf, name) in [(B, "local_s1.json"), (B, "deleted_s9"), (A, "some.fifo")] {
        assert!(fs::symlink_metadata(b.dir(leaf).join(name))
            .unwrap()
            .file_type()
            .is_fifo());
    }
    assert!(fs::symlink_metadata(b.dir(B).join("local_s2.json"))
        .unwrap()
        .file_type()
        .is_symlink());
    // the backup was made, of the ordinary files only, and says what it left out
    let zips = listdir(&b.home.join("backups"));
    assert_eq!(zips.len(), 1);
    let mut archive =
        zip::ZipArchive::new(fs::File::open(b.home.join("backups").join(&zips[0])).unwrap())
            .unwrap();
    let mut names: Vec<String> = (0..archive.len())
        .map(|i| archive.by_index(i).unwrap().name().to_string())
        .collect();
    names.sort();
    let leaf = |l: Leaf, n: &str| format!("{}/{}/{}", l.0, l.1, n);
    assert_eq!(
        names,
        vec![
            leaf(A, "local_s1.json"),
            leaf(A, "local_s2.json"),
            leaf(A, "local_s3.json"),
            leaf(B, "local_own.json"),
        ]
    );
    assert_eq!(
        b.said("7 thing(s) in the store that are not ordinary files"),
        1
    );
    // the daemon's loop gets through as well, and reports
    let mut s = b.s();
    s.watch(3600.0, 1.0, 1.0, Some(2), &mut |_| {});
    assert!(b.home.join("status.json").exists());
    // the app's log, should it ever be a pipe, is not read either
    let b = Base::new();
    fs::create_dir_all(b.applog.parent().unwrap()).unwrap();
    fifo(&b.applog);
    b.rec(A, "s1", r());
    let mut s = b.plain();
    s.hooks.probe_app = Some(Box::new(|| false));
    s.run();
    assert_eq!(s.signed_in(), Signed::Unknown);
}

// ---------------------------------------------------------------------- a move never replaces

#[test]
fn a_move_never_replaces_what_is_there() {
    let b = Base::new();
    let (src, dst) = (b.tmp.join("moving.json"), b.tmp.join("there.json"));
    fs::write(&src, b"the one being moved").unwrap();
    fs::write(&dst, b"the one that is there").unwrap();
    set_mtime(&dst, now() - HOUR);
    let before = mtime_ns(&dst);
    for attempt in [move_file(&src, &dst), copy_then_remove(&src, &dst)] {
        assert_eq!(
            attempt.unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&dst).unwrap(), b"the one that is there");
        assert_eq!(mtime_ns(&dst), before);
        assert_eq!(fs::read(&src).unwrap(), b"the one being moved"); // nothing was lost either
    }
    // a folder in the way is in the way too
    let folder = b.tmp.join("a-folder");
    fs::create_dir(&folder).unwrap();
    assert!(move_file(&src, &folder).is_err());
    assert!(src.exists() && folder.is_dir());
    // an error that is not "another volume" is the error, not a reason to copy
    let nowhere = b.tmp.join("no-such-folder").join("x.json");
    assert_eq!(
        move_file(&src, &nowhere).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
    assert!(src.exists() && !nowhere.exists());
    // where nothing is in the way, the file is moved
    let free = b.tmp.join("free.json");
    set_mtime(&src, now() - 2.0 * HOUR);
    let was = mtime_ns(&src);
    move_file(&src, &free).unwrap();
    assert!(!src.exists());
    assert_eq!(fs::read(&free).unwrap(), b"the one being moved");
    assert_eq!(mtime_ns(&free), was);
}

/// Across volumes a move is a copy: made as a new file, with the times of the original,
/// which is removed only when the copy is whole.
#[test]
fn the_copy_of_a_move_across_volumes_keeps_the_files_times() {
    let b = Base::new();
    let (src, dst) = (b.tmp.join("moving.json"), b.tmp.join("moved.json"));
    fs::write(&src, b"{\"a\": 1}").unwrap();
    let (atime, mtime) = (1_700_000_000_123_456_700, 1_700_000_001_987_654_300);
    winfs::set_times(&src, atime, mtime).unwrap();
    copy_then_remove(&src, &dst).unwrap();
    assert!(!src.exists());
    let st = winfs::stat(&dst).unwrap(); // (looked at before it is read: reading may count as access)
    assert_eq!((st.atime_ns, st.mtime_ns), (atime, mtime));
    assert_eq!(fs::read(&dst).unwrap(), b"{\"a\": 1}");
    // a file that is not there to move: nothing is made in its place
    let made = b.tmp.join("made.json");
    assert!(copy_then_remove(&src, &made).is_err());
    assert!(!made.exists());
}

/// The same through the real thing where there is a second volume at hand (Linux: the
/// memory file system at /dev/shm is another one than the temp folder's, as a rule).
#[cfg(target_os = "linux")]
#[test]
fn a_move_to_another_volume_keeps_the_time_and_does_not_replace() {
    use std::os::unix::fs::MetadataExt;
    let b = Base::new();
    let other = PathBuf::from("/dev/shm");
    let same_volume = fs::metadata(&other)
        .and_then(|o| fs::metadata(&b.tmp).map(|t| o.dev() == t.dev()))
        .unwrap_or(true);
    if same_volume {
        eprintln!("skipped: no second volume at /dev/shm");
        return;
    }
    let far = other.join(format!("css-move-test-{}", std::process::id()));
    fs::create_dir_all(&far).unwrap();
    let src = b.tmp.join("moving.json");
    fs::write(&src, b"{\"a\": 1}").unwrap();
    let mtime = 1_700_000_001_987_654_321;
    winfs::set_times(&src, mtime, mtime).unwrap();
    // (the plain rename is refused here: that is what makes it the other case)
    assert_eq!(
        winfs::rename_noreplace(&src, &far.join("x.json"))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::CrossesDevices
    );
    let dst = far.join("moved.json");
    fs::write(&dst, b"there").unwrap();
    assert_eq!(
        move_file(&src, &dst).unwrap_err().kind(),
        std::io::ErrorKind::AlreadyExists
    );
    assert_eq!(fs::read(&dst).unwrap(), b"there");
    fs::remove_file(&dst).unwrap();
    move_file(&src, &dst).unwrap();
    assert!(!src.exists());
    assert_eq!(mtime_ns(&dst), mtime);
    // ...and back
    move_file(&dst, &src).unwrap();
    assert_eq!(mtime_ns(&src), mtime);
    fs::remove_dir_all(&far).unwrap();
}

/// Two undos within the same second keep both of what they took out.
#[test]
fn undo_twice_in_a_second_replaces_nothing_it_took_out_before() {
    let b = Base::new();
    b.rec(A, "s1", r());
    fs::remove_dir(b.dir(C)).unwrap();
    b.rec(B, "own", r());
    for _ in 0..2 {
        assert_eq!(b.s().run().add, 2);
        let mut s = b.s();
        FakeClock::at(&mut s, 1_800_000_000.0); // the same moment both times
        assert_eq!(s.undo(0.0).unwrap().removed, 2);
        assert!(!b.has(B, "s1"));
    }
    let mut kept = Vec::new();
    fn walk(top: &Path, out: &mut Vec<PathBuf>) {
        for e in fs::read_dir(top).unwrap().flatten() {
            if e.path().is_dir() {
                walk(&e.path(), out);
            } else {
                out.push(e.path());
            }
        }
    }
    walk(&b.home.join("undone"), &mut kept);
    assert_eq!(kept.len(), 4, "{:?}", kept);
}

// ---------------------------------------------------------------------- who may read it

/// Everything this tool writes is the user's alone: its own folder and every folder in it
/// 0700, every file 0600, and the records it puts in the store 0600 too, whatever mode the
/// record it replaces had. (The umask is not something a test can change for itself while
/// others run; the programs are run under several in tests/processes.rs.)
#[cfg(unix)]
#[test]
fn what_this_tool_writes_is_the_users_alone() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().mode() & 0o7777
    }
    fn open_to_all(path: &Path) {
        fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
    }
    let b = Base::new();
    winfs::make_dirs(&b.home).unwrap(); // (the folder is this tool's to make, as at a first run)
    b.install(2.0 * HOUR, 0.0);
    // as an older app (or another umask) left them: readable by all
    for (leaf, fid, record) in [
        (A, "plain", r()),
        (A, "newer", r().last(600.0).mtime(500.0).title("newer")),
        (
            B,
            "newer",
            r().last(5.0 * HOUR).mtime(4.0 * HOUR).title("older"),
        ),
        (B, "gone", r().cli("c-gone").last(HOUR)),
        (C, "gone", r().cli("c-gone").last(HOUR).mtime(2.0 * HOUR)),
    ] {
        open_to_all(&b.rec(leaf, fid, record));
    }
    b.delete(A, "gone", 600.0, Some("c-gone"));
    let source = b.path(A, "plain");
    let mut s = b.s();
    let done = s.run();
    assert!(
        done.add >= 2 && done.update >= 1 && done.quarantine >= 2,
        "{:?}",
        (done.add, done.update, done.quarantine)
    );
    s.resolve_all().unwrap();
    s.watch(3600.0, 0.0, 1.0, Some(1), &mut |_| {}); // status.json, the lock's folder
    drop(acquire_lock(&b.home).unwrap());
    crate::install::save_settings(&b.home, &crate::install::Settings::default()).unwrap();
    assert!(crate::install::stop_daemon(&b.home, 1, &mut |_| {}).unwrap());
    b.s().undo(now() - 1.0).unwrap(); // the undone folder
    b.s().run();
    // this tool's own folder: everything in it
    assert_eq!(mode(&b.home), 0o700);
    let mut seen = 0;
    fn check(dir: &Path, seen: &mut usize) {
        for e in fs::read_dir(dir).unwrap().flatten() {
            let path = e.path();
            if e.file_type().unwrap().is_dir() {
                assert_eq!(mode(&path), 0o700, "{}", path.display());
                check(&path, seen);
            } else if path.file_name().unwrap() != "stop.flag" {
                assert_eq!(mode(&path), 0o600, "{}", path.display());
                *seen += 1;
            }
        }
    }
    check(&b.home, &mut seen);
    for made in [
        "backups",
        "replaced",
        "quarantine",
        "undone",
        "journal.jsonl",
        "sync.log",
        "manifest.json",
        "state.json",
        "status.json",
        "settings.json",
        "daemon.lock",
    ] {
        assert!(b.home.join(made).exists(), "{}", made);
    }
    assert!(seen >= 12, "{}", seen);
    // the store: what this tool put there is the user's alone...
    assert_eq!(mode(&b.path(B, "plain")), 0o600); // a copy of a record that is readable by all
    assert_eq!(mode(&b.path(C, "plain")), 0o600);
    assert_eq!(mode(&b.path(B, "newer")), 0o600); // a record that was readable by all, replaced
    for name in listdir(&b.dir(B)) {
        if name.starts_with("deleted_") {
            assert_eq!(mode(&b.dir(B).join(&name)), 0o600, "{}", name); // a marker it wrote
        }
    }
    // ...and what it only read is as it was
    assert_eq!(mode(&source), 0o644);
    assert_eq!(
        mode(&b.dir(B)),
        fs::metadata(b.dir(A)).unwrap().mode() & 0o7777
    );
}

// ---------------------------------------------------------------------- is the app running

/// Besides the process list: the lock the app's own engine keeps in its data folder (the
/// store's parent) while it runs, and, on Linux, a log that was written not long ago.
#[cfg(unix)]
#[test]
fn the_apps_lock_and_its_log_say_it_is_running_when_the_process_list_does_not() {
    use std::os::unix::fs::symlink;
    let app_dir = |b: &Base| b.root.parent().unwrap().to_path_buf();
    let sync = |b: &Base| {
        let mut s = b.plain();
        s.hooks.probe_app = Some(Box::new(|| false)); // the process list knows of no app
        s
    };
    let b = Base::new();
    assert!(!sync(&b).app_running());
    // the app's lock, held by a process that is there
    let lock = app_dir(&b).join("SingletonLock");
    let host = crate::process::this_host().unwrap();
    symlink(format!("{}-{}", host, std::process::id()), &lock).unwrap();
    assert!(sync(&b).app_running());
    // ...so the folder it has loaded is not written into
    b.write_log(&[&Base::login(A, 1)]);
    set_mtime(&b.applog, now() - 3.0 * HOUR);
    b.rec(B, "s1", r().last(600.0).mtime(500.0).title("newer"));
    b.rec(
        A,
        "s1",
        r().last(5.0 * HOUR).mtime(4.0 * HOUR).title("loaded"),
    );
    let mut s = sync(&b);
    s.homes_fn = Some(Box::new(|| Homes::All));
    assert_eq!(s.run().update, 0);
    assert_eq!(b.title(A, "s1"), "loaded");
    // the lock of an app that is gone (its process is not there) says nothing
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let gone = child.id();
    child.wait().unwrap();
    fs::remove_file(&lock).unwrap();
    symlink(format!("{}-{}", host, gone), &lock).unwrap();
    assert!(!sync(&b).app_running());
    let mut s = sync(&b);
    s.homes_fn = Some(Box::new(|| Homes::All));
    assert_eq!(s.run().update, 1);
    assert_eq!(b.title(A, "s1"), "newer");
    // the log: written a moment ago, the app is taken to be running everywhere
    let b = Base::new();
    b.write_log(&[&Base::login(A, 1)]);
    assert!(sync(&b).app_running());
    // written five minutes ago: only where everything about the app's program is a guess
    set_mtime(&b.applog, now() - 300.0);
    assert_eq!(sync(&b).app_running(), cfg!(target_os = "linux"));
    set_mtime(&b.applog, now() - 700.0);
    assert!(!sync(&b).app_running());
}
