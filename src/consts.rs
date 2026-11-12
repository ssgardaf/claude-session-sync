//! Timings and limits. Every value mirrors a rule that exists for a reason; see the notes.

/// The version this program reports (`--version`, `status.json`, `state.json`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The app itself skips records above 10 MB.
pub const MAX_RECORD_BYTES: u64 = 10 * 1024 * 1024;
/// Nothing this tool reads as a whole is larger than this (the app's log is the largest
/// thing it reads): a file that is, is not read at all.
pub const MAX_READ_BYTES: u64 = 64 * 1024 * 1024;
/// A file in the store larger than this is left out of the daily backup (the records the
/// app itself reads are a hundredth of it at most).
pub const MAX_BACKUP_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Leave alone a destination something else wrote this recently.
pub const APP_FRESH_SEC: f64 = 120.0;
/// The app debounces saves by 1 s; let a write settle.
pub const SRC_SETTLE_SEC: f64 = 3.0;
/// A record that just disappeared without a marker may be mid-deletion.
pub const VANISH_GRACE_SEC: f64 = 300.0;
/// Not running this long = deletions made meanwhile are not carried over.
pub const DOWNTIME_GAP_SEC: f64 = 2.0 * 3600.0;
/// The app's log was written this recently: the app is taken to be running, whatever the
/// process list says. On Windows and macOS the app's program is known, and this only covers
/// the moment a log line is written by a program not yet recognised. On Linux everything
/// known about the app's program is a guess, so the log counts for much longer there.
#[cfg(not(target_os = "linux"))]
pub const APP_LOG_FRESH_SEC: f64 = 20.0;
#[cfg(target_os = "linux")]
pub const APP_LOG_FRESH_SEC: f64 = 600.0;
/// Without the app's log: a folder written this recently is the one in use.
pub const IN_USE_SEC: f64 = 600.0;
/// A copy we already placed follows a busy session at most this often.
pub const REFRESH_SEC: f64 = 60.0;
/// A sign-in replaced this soon by one to another account/org was only a step in a switch.
pub const TRANSIENT_SEC: f64 = 10.0;
/// The current sign-in counts as settled after this long.
pub const HOME_SETTLE_SEC: f64 = 60.0;
/// In a pass that must not wait (sign-out...), a save this old is taken as finished.
pub const URGENT_SETTLE_SEC: f64 = 1.0;
/// The daemon reports that it is alive at least this often, whatever its settings.
pub const BEAT_SEC: f64 = 60.0;
/// A daemon that has taken the lock reports at once; `status` gives it this long to do so
/// before it calls it stuck.
pub const START_GRACE_SEC: f64 = 60.0;
/// A daemon whose heartbeat is this old is taken to be stuck.
pub const STALL_SEC: f64 = 900.0;
/// How often the app's sign-in state is looked at between checks.
pub const POLL_SEC: f64 = 2.0;
pub const KEEP_BACKUPS: usize = 14;
pub const KEEP_REPLACED_DAYS: f64 = 14.0;
pub const MAX_REPLACED_BYTES: u64 = 200 * 1024 * 1024;
/// Fold the running list of our own writes into manifest.json this often.
pub const MANIFEST_EVERY: usize = 50;
pub const MIN_FREE_BYTES: u64 = 200 * 1024 * 1024;

/// The programs a daemon of this tool runs as. Only these are ever ended to take over from
/// a daemon that has stalled.
#[cfg(windows)]
pub const DAEMON_PROCESS_NAMES: [&str; 2] = ["claude-session-sync.exe", "claude-session-syncw.exe"];
#[cfg(not(windows))]
pub const DAEMON_PROCESS_NAMES: [&str; 1] = ["claude-session-sync"];

/// A daemon reports when it started; its process must not be younger than that by more than
/// this, or it is some other process that was given the same pid later.
pub const START_SLACK_SEC: f64 = 5.0;

/// Process names a daemon of this tool can have: this program, or its Python predecessor
/// (which `uninstall` may have to end).
#[cfg(windows)]
pub const OUR_PROCESS_NAMES: [&str; 4] = [
    "claude-session-sync.exe",
    "claude-session-syncw.exe",
    "pythonw.exe",
    "python.exe",
];
/// (There was no Python version for macOS or Linux.)
#[cfg(not(windows))]
pub const OUR_PROCESS_NAMES: [&str; 1] = ["claude-session-sync"];
