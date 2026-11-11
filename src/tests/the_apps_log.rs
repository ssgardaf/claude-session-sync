use super::*;
use crate::applog::AppLog;
use crate::process;
use crate::sync::Signed;

#[test]
fn test_last_sign_in_wins_and_sign_out_is_seen() {
    let b = Base::new();
    let mut log = AppLog::new(b.log_dirs());
    assert_eq!(log.state(), Signed::Unknown); // no log at all
    b.write_log(&[
        "2026-11-11 09:00:00 [info] something else\r\n",
        &Base::login(A, 3),
        &Base::login(A, 64),
    ]);
    assert_eq!(log.state(), Signed::In(la()));
    b.write_log(&[
        "2026-11-11 09:01:00 [info] [LocalAgentModeSessionManager] Loaded 1 persisted sessions\r\n",
    ]);
    assert_eq!(log.state(), Signed::In(la())); // unrelated lines change nothing
    b.write_log(&[Base::LOGOUT]);
    assert_eq!(log.state(), Signed::Out);
    b.write_log(&[&Base::login(B, 3)]);
    assert_eq!(log.state(), Signed::In(lb()));
    assert_eq!(AppLog::new(b.log_dirs()).state(), Signed::In(lb())); // a fresh reader (daemon restart) agrees
}

#[test]
fn test_line_still_being_written_is_read_when_it_is_complete() {
    let b = Base::new();
    let mut log = AppLog::new(b.log_dirs());
    b.write_log(&[&Base::login(A, 3)]);
    let whole = Base::login(B, 3);
    b.write_log(&[&whole[..70]]);
    assert_eq!(log.state(), Signed::In(la()));
    b.write_log(&[&whole[70..]]);
    assert_eq!(log.state(), Signed::In(lb()));
}

#[test]
fn test_rotated_log_is_read_from_the_start_and_the_previous_file_is_consulted() {
    let b = Base::new();
    let mut log = AppLog::new(b.log_dirs());
    b.write_log(&[&Base::login(A, 3), &("x".repeat(5000) + "\r\n")]);
    assert_eq!(log.state(), Signed::In(la()));
    fs::rename(&b.applog, b.applog.with_file_name("main1.log")).unwrap();
    b.write_log_mode(
        &["2026-11-12 00:00:00 [info] new file, nothing about accounts yet\r\n"],
        true,
    );
    assert_eq!(log.state(), Signed::In(la()));
    assert_eq!(AppLog::new(b.log_dirs()).state(), Signed::In(la())); // a fresh reader finds it in main1.log
    b.write_log(&[&Base::login(C, 3)]);
    assert_eq!(log.state(), Signed::In(lc()));
}

#[test]
fn test_default_sync_uses_the_log() {
    let b = Base::new();
    b.write_log(&[&Base::login(B, 3)]);
    let mut s = b.plain();
    s.app_running_fn = Some(Box::new(|| true));
    assert_eq!(s.signed_in(), Signed::In(lb()));
}

#[test]
fn test_detecting_the_desktop_app_gives_a_plain_answer() {
    let _answer: bool = process::desktop_app_running();
}
