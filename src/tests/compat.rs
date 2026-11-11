//! Taking over from the Python version in place: its files must keep working, and the
//! Windows calls that behave differently from Python's must behave the way the rules need.

use serde_json::{json, Value};

use super::*;
use crate::sync::Sync;
use crate::winfs::{atomic_write, rename_noreplace, Expect, WriteError};

/// The manifest key the Python version wrote for a path: absolute, lower case.
fn python_key(path: &Path) -> String {
    path.to_string_lossy().to_lowercase()
}

/// `sha256(repr((archived, title)))[:16]` is what Python stored; any 16 hex digits stand in for it.
const PYTHON_USER_KEY: &str = "0123456789abcdef";

fn write_manifest(b: &Base, entries: &[(&Path, Value)]) {
    fs::create_dir_all(&b.home).unwrap();
    let m: serde_json::Map<String, Value> = entries
        .iter()
        .map(|(p, v)| (python_key(p), v.clone()))
        .collect();
    // Python writes ", " and ": " between items, and non-ASCII characters as \u escapes
    let text = serde_json::to_string_pretty(&Value::Object(m)).unwrap();
    fs::write(b.home.join("manifest.json"), text).unwrap();
}

#[test]
fn create_never_replaces_an_existing_file() {
    let b = Base::new();
    let p = b.dir(B).join("local_x.json");
    fs::write(&p, b"the app's file").unwrap();
    set_mtime(&p, now() - HOUR);
    let before = winfs::stat(&p).unwrap();
    assert!(matches!(
        atomic_write(&p, b"ours", None, true, Expect::Absent),
        Err(WriteError::Changed)
    ));
    assert_eq!(fs::read(&p).unwrap(), b"the app's file"); // (std::fs::rename would have replaced it)
    assert_eq!(winfs::stat(&p).unwrap(), before);
    assert_eq!(listdir(&b.dir(B)), vec!["local_x.json"]); // and no temp file is left
                                                          // the same holds for the rename itself, and for a folder in the way
    let other = b.dir(B).join("other");
    fs::write(&other, b"new").unwrap();
    let refused = rename_noreplace(&other, &p).unwrap_err();
    assert_eq!(refused.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(fs::read(&p).unwrap(), b"the app's file");
    fs::remove_file(&other).unwrap();
    let folder = b.dir(B).join("local_y.json");
    fs::create_dir(&folder).unwrap();
    assert!(matches!(
        atomic_write(&folder, b"ours", None, true, Expect::Absent),
        Err(WriteError::Changed)
    ));
    assert!(folder.is_dir());
    // where nothing is in the way, the file is created, with the times asked for
    let fresh = b.dir(B).join("local_z.json");
    atomic_write(
        &fresh,
        b"ours",
        Some((1_700_000_000_123_456_700, 1_700_000_001_987_654_300)),
        true,
        Expect::Absent,
    )
    .unwrap();
    assert_eq!(fs::read(&fresh).unwrap(), b"ours");
    let st = winfs::stat(&fresh).unwrap();
    assert_eq!(
        (st.atime_ns, st.mtime_ns),
        (1_700_000_000_123_456_700, 1_700_000_001_987_654_300)
    ); // kept to 100 ns
}

#[test]
fn paths_longer_than_the_classic_limit_work() {
    let b = Base::new();
    let mut dir = b.tmp.join("deep");
    while dir.to_string_lossy().len() < 300 {
        dir = dir.join("a-folder-with-a-rather-long-name-0123456789");
    }
    fs::create_dir_all(&dir).unwrap();
    let p = dir.join("local_00000000-0000-4000-8000-000000000001.json");
    atomic_write(
        &p,
        b"one",
        Some((1_700_000_000_000_000_000, 1_700_000_000_000_000_000)),
        true,
        Expect::Absent,
    )
    .unwrap();
    assert!(matches!(
        atomic_write(&p, b"two", None, true, Expect::Absent),
        Err(WriteError::Changed)
    ));
    let st = winfs::stat(&p).unwrap();
    assert_eq!(st.mtime_ns, 1_700_000_000_000_000_000);
    atomic_write(&p, b"three", None, true, Expect::Is(st.mtime_ns, st.size)).unwrap();
    assert_eq!(winfs::read_shared(&p).unwrap(), b"three");
    let moved = dir.join("moved.json");
    winfs::move_file(&p, &moved).unwrap();
    assert!(!p.exists() && moved.exists());
    assert_eq!(listdir(&dir), vec!["moved.json"]);
    assert!(winfs::norm(&moved).ends_with("\\moved.json"));
}

#[test]
fn manifest_keys_are_what_python_wrote() {
    let b = Base::new();
    let p = b.path(B, "MiXed-Case");
    let key = winfs::norm(&p);
    assert_eq!(key, python_key(&p));
    assert!(!key.contains('/'));
    // relative and unnormalised spellings lead to the same key
    assert_eq!(
        winfs::norm(&b.dir(B).join("..").join(B.1).join("local_MiXed-Case.json")),
        key
    );
    assert_eq!(
        winfs::norm(Path::new(&p.to_string_lossy().replace('\\', "/"))),
        key
    );
    assert_eq!(
        winfs::norm(Path::new(&p.to_string_lossy().to_uppercase())),
        key
    );
    // Outside ASCII, Windows' own lower-casing is what counts, not Unicode's: this is what
    // Python 3.12's os.path.normcase(os.path.abspath(...)) gives for the same path.
    assert_eq!(
        winfs::norm(Path::new("C:\\Temp\\Dir \u{130}\u{c7}\u{1c5}\u{3a3}\u{1e9e}\u{d55c}\u{ae00} X\\Sub/..\\FILE.JSON")),
        "c:\\temp\\dir \u{130}\u{e7}\u{1c5}\u{3c3}\u{1e9e}\u{d55c}\u{ae00} x\\file.json"
    );
}

#[test]
fn manifest_written_by_python_is_understood() {
    let b = Base::new();
    let p2 = b.rec(B, "two", r()); // [mtime_ns, size]: the oldest form
    let p3 = b.rec(B, "three", r()); // + lastActivityAt
    let p4 = b.rec(B, "four", r()); // + sha
    let p5 = b.rec(B, "five", r()); // + user key
    let gone = b.path(B, "gone");
    let st = |p: &Path| winfs::stat(p).unwrap();
    let sha4 = crate::sync::sha256_hex(&fs::read(&p4).unwrap())[..32].to_string();
    write_manifest(
        &b,
        &[
            (&p2, json!([st(&p2).mtime_ns, st(&p2).size])),
            (
                &p3,
                json!([st(&p3).mtime_ns, st(&p3).size, 1794404914215i64]),
            ),
            // a float where Python had one, and a file time that has since changed
            (&p4, json!([1, st(&p4).size, 1794404914215.0, sha4])),
            (
                &p5,
                json!([
                    st(&p5).mtime_ns,
                    st(&p5).size,
                    1794404914215i64,
                    "00ff",
                    PYTHON_USER_KEY
                ]),
            ),
            (&gone, json!([5, 6, null, null, null])),
            (&b.path(C, "odd"), json!("not a list")),
            (&b.path(C, "short"), json!([1])),
        ],
    );
    // Python appends "\r\n"-terminated lines; a forgotten entry is [key, null]
    let log = format!(
        "{}\r\n{}\r\nnot json\r\n{}\r\n",
        json!([python_key(&gone), null]),
        json!([python_key(&b.path(A, "added")), [7, 8, null, null, null]]),
        json!(["three items", 1, 2]),
    );
    fs::write(b.home.join("manifest.log"), log).unwrap();
    let mut s = b.s();
    for p in [&p2, &p3, &p4, &p5] {
        assert!(s.is_ours(p, &st(p)), "{}", p.display());
    }
    assert_eq!(s.base(&p2).unwrap().len(), 2);
    assert_eq!(s.base(&p5).unwrap()[4], json!(PYTHON_USER_KEY));
    assert_eq!(s.base(&gone), None);
    assert_eq!(
        s.base(&b.path(A, "added")),
        Some(vec![
            json!(7),
            json!(8),
            Value::Null,
            Value::Null,
            Value::Null
        ])
    );
    assert_eq!(s.base(&b.path(C, "odd")), None);
    assert_eq!(s.base(&b.path(C, "short")), None);
    // a copy Python placed is still "ours": it gives way to progress without being kept
    b.rec(A, "two", r().last(60.0).mtime(50.0).title("moved on in A"));
    assert_eq!(s.run().update, 1);
    assert_eq!(b.title(B, "two"), "moved on in A");
    assert!(b.kept().is_empty());
}

/// B holds a copy this tool placed, which the app has saved again without the user deciding
/// anything; meanwhile A went on, and A's title differs. Returns a Sync whose manifest says
/// B's copy was placed with the given user key.
fn resaved_copy(b: &Base, key: Value) -> Sync {
    b.rec(A, "s1", r().last(2.0 * HOUR).title("as it was"));
    b.s().run();
    let t = b.last_activity(A, "s1");
    let placed = winfs::stat(&b.path(B, "s1")).unwrap();
    b.rec(
        A,
        "s1",
        r().last(20.0 * 60.0).mtime(19.0 * 60.0).title("A went on"),
    );
    b.rec(
        B,
        "s1",
        r().mtime(10.0 * 60.0)
            .title("as it was")
            .extra(json!({"lastActivityAt": t, "lastFocusedAt": ms(600.0)})),
    );
    write_manifest(
        b,
        &[(
            &b.path(B, "s1"),
            json!([placed.mtime_ns, placed.size, t, "00", key]),
        )],
    );
    fs::write(b.home.join("manifest.log"), b"").unwrap();
    b.s()
}

#[test]
fn user_key_in_pythons_form_is_never_taken_as_equal() {
    // with our own key the re-saved copy gives way...
    let b = Base::new();
    let mut s = resaved_copy(
        &b,
        json!(Sync::user_key(&(false, String::from("as it was")))),
    );
    assert!(s.plan().notes.conflicts.is_empty());
    // ...with a key in Python's form the same situation is a conflict: the safe direction
    let b = Base::new();
    let mut s = resaved_copy(&b, json!(PYTHON_USER_KEY));
    let found: Vec<String> = s
        .plan()
        .notes
        .conflicts
        .iter()
        .map(|c| c.0.clone())
        .collect();
    assert_eq!(found, vec![lb()]);
    let mine = b.data(B, "s1");
    s.run();
    assert_eq!(b.data(B, "s1"), mine);
    // and our keys are recognisably ours
    assert!(Sync::user_key(&(true, String::from("x"))).starts_with("r1:"));
    assert_ne!(
        Sync::user_key(&(true, String::from("x"))),
        Sync::user_key(&(false, String::from("x")))
    );
    assert_ne!(
        Sync::user_key(&(true, String::from("x"))),
        Sync::user_key(&(true, String::from("y")))
    );
}

#[test]
fn state_and_journal_written_by_python_are_understood() {
    let b = Base::new();
    fs::create_dir_all(&b.home).unwrap();
    // state.json as Python leaves it (floats, spaces, keys this version does not use)
    let since = ms(2.0 * HOUR) as f64 + 0.25;
    fs::write(
        b.home.join("state.json"),
        format!(
            "{{\"installed_at\": {}, \"propagate_since\": {}, \"blackouts\": [], \"alive_until\": {}, \"version\": \"8.1\", \"homes\": [\"{}\"], \"future\": {{\"x\": 1}}}}",
            ms(2.0 * HOUR), since, ms(0.0), la()
        ),
    )
    .unwrap();
    b.rec(B, "s1", r().cli("c1").last(HOUR));
    b.delete(A, "s1", 600.0, Some("c1"));
    let mut s = b.s();
    assert_eq!(s.run().quarantine, 1); // the deletion window carries over
    let st = b.state();
    assert_eq!(st["future"], json!({"x": 1})); // what we do not know is kept
    assert_eq!(st["propagate_since"].as_f64(), Some(since));
    assert_eq!(st["version"], json!(crate::consts::VERSION));
    // a journal with Python's line endings can be undone
    let text = fs::read_to_string(b.home.join("journal.jsonl"))
        .unwrap()
        .replace('\n', "\r\n");
    fs::write(b.home.join("journal.jsonl"), text).unwrap();
    let undone = b.s().undo(now() - 60.0).unwrap();
    assert_eq!(undone.put_back, 1);
    assert!(b.has(B, "s1"));
}
