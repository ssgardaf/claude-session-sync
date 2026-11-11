use std::collections::BTreeSet;

use super::*;
use crate::applog::AppLog;
use crate::sync::Homes;

/// A sign-in to `leaf` (or a sign-out, for None) at that time on 2026-11-08.
pub fn at(hms: &str, leaf: Option<Leaf>) -> String {
    match leaf {
        None => format!(
            "2026-11-08 {} [info] [LocalSessionManager] Account logged out, marking for re-init on next login\r\n",
            hms
        ),
        Some(leaf) => format!(
            "2026-11-08 {} [info] [LocalSessionManager] Initialization succeeded — accountId={}, orgId={}, existingSessions=0\r\n",
            hms, leaf.0, leaf.1
        ),
    }
}

fn set(names: &[String]) -> BTreeSet<String> {
    names.iter().cloned().collect()
}

#[test]
fn test_folder_the_app_only_passes_through_while_switching_is_given_nothing() {
    let b = Base::new();
    b.rec(A, "s1", r());
    let mut s = b.s_homes(&[la(), lb()]); // C: signed in to for a second in the middle of a switch
    let notes = s.plan().notes;
    assert_eq!((notes.not_settled, notes.everywhere), (1, 1));
    s.run();
    assert!(b.has(B, "s1"));
    assert!(listdir(&b.dir(C)).is_empty());
}

#[test]
fn test_folder_that_holds_anything_of_the_apps_counts_as_used() {
    let b = Base::new();
    b.rec(A, "s1", r());
    b.rec(C, "own", r()); // the app has kept a session here: it is a real home
    b.tomb(B, "whatever", ms(0.0)); // ...and has deleted one here
    b.s_homes(&[]).run();
    assert!(b.has(C, "s1") && b.has(B, "s1") && b.has(A, "own"));
}

#[test]
fn test_log_history_tells_real_sign_ins_from_steps_in_a_switch() {
    let b = Base::new();
    let mixed: Leaf = (A.0, B.1); // old account + new organization: what the app does mid-switch
    b.write_log(&[
        &at("09:00:00", Some(A)),
        &at("09:00:00", Some(A)), // A, for a long while
        &at("16:14:41", None),    // signed out
        &at("16:14:53", Some(mixed)),
        &at("16:14:55", Some(B)), // the two-second step, then B
        &at("18:00:00", None),
        &at("18:00:05", Some(C)),
        &at("18:00:15", None), // C for ten seconds, then signed out: really there
    ]);
    let all = set(&[la(), lb(), lc()]);
    assert_eq!(AppLog::new(b.log_dirs()).homes(true), Some(all.clone()));
    fs::rename(&b.applog, b.applog.with_file_name("main1.log")).unwrap();
    b.write_log_mode(&["2026-11-09 00:00:00 [info] nothing yet\r\n"], true);
    assert_eq!(AppLog::new(b.log_dirs()).homes(true), Some(all)); // older log files are read too
    assert_eq!(
        AppLog::new(b.log_dirs()).homes(false),
        Some(BTreeSet::new())
    );
}

#[test]
fn test_default_sync_takes_homes_from_the_log_and_remembers_them() {
    let b = Base::new();
    b.write_log(&[
        &at("09:00:00", Some(A)),
        &at("10:00:00", None),
        &at("10:00:05", Some(B)),
        &at("11:00:00", None),
    ]);
    b.rec(A, "s1", r());
    let mut s = b.plain();
    s.app_running_fn = Some(Box::new(|| false));
    assert_eq!(s.homes(), Homes::Only(set(&[la(), lb()])));
    s.run();
    assert!(b.has(B, "s1"));
    assert!(!b.has(C, "s1"));
    fs::remove_file(&b.applog).unwrap(); // the log is rotated away...
    let mut later = b.plain();
    later.app_running_fn = Some(Box::new(|| false));
    assert_eq!(later.homes(), Homes::Only(set(&[la(), lb()]))); // ...but not forgotten
}

#[test]
fn test_without_any_log_only_folders_already_in_use_receive_records() {
    let b = Base::new();
    b.rec(A, "s1", r());
    b.rec(B, "own", r());
    let mut s = b.plain();
    s.app_running_fn = Some(Box::new(|| false));
    s.run();
    assert!(b.has(B, "s1") && b.has(A, "own"));
    assert!(listdir(&b.dir(C)).is_empty());
}
