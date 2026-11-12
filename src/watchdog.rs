//! A daemon that has stopped coming round is ended, so that a fresh one is started.
//!
//! On Windows that is done from outside: the scheduled task starts a second copy every five
//! minutes, which ends a daemon whose heartbeat is `STALL_SEC` old and takes its place (see
//! `daemon::lock_for_daemon`). launchd and systemd never start a second copy beside a daemon
//! that is still there, so on macOS and Linux the daemon looks after itself: a second thread
//! watches the main loop's heartbeat (the same event, with the same limit) and, when it has
//! been silent for that long, ends the process with an exit code that is not 0. launchd
//! (`KeepAlive`, `SuccessfulExit` false) and systemd (`Restart=always`) then start a fresh
//! daemon. The process is ended on the spot, as the Windows takeover ends it: that is safe
//! at any point (every change is journaled first and put in place in one step), and the
//! single-instance lock goes with the process.
//!
//! Time is measured with `std::time::Instant`. On Linux that is `CLOCK_MONOTONIC` and on
//! macOS `CLOCK_UPTIME_RAW` (checked in the standard library's source, `sys/time/unix.rs`,
//! where the choice is made in one constant for each): neither goes on while the computer
//! sleeps. So a daemon that slept with the computer has not been silent for the time the
//! computer was asleep, and waking up cannot look like a stall.
//!
//! Only the `daemon` command of the real program starts the watchdog; a single pass
//! (`sync`) has none. On Windows nothing here does anything.

#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(unix)]
use std::sync::OnceLock;
#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(unix)]
use crate::consts::STALL_SEC;

/// What the process ends with when the watchdog ends it (the conventional code for "failed
/// for now, try again").
pub const STALL_EXIT_CODE: i32 = 75;

/// Is a heartbeat of this age (seconds) that of a daemon that is stuck? One that has not
/// beaten at all yet (it is still waiting for its turn to start) is not.
pub fn stalled(beat_age: Option<f64>, limit: f64) -> bool {
    beat_age.is_some_and(|age| age >= limit)
}

/// How patient the watchdog is, in seconds.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pace {
    /// A heartbeat this old is that of a daemon that is stuck.
    pub limit: f64,
    /// How often the heartbeat is looked at.
    pub check_every: f64,
    /// After a heartbeat was found too old, it is looked at once more this much later
    /// before anything is done (the Windows takeover gives a daemon the same last moment).
    pub second_look: f64,
}

#[cfg(unix)]
impl Default for Pace {
    fn default() -> Pace {
        Pace {
            limit: STALL_SEC,
            check_every: 60.0,
            second_look: 20.0,
        }
    }
}

/// The moment everything is counted from.
#[cfg(unix)]
static BEGAN: OnceLock<Instant> = OnceLock::new();
/// The last heartbeat, in milliseconds since `BEGAN`, plus one (0: none yet).
#[cfg(unix)]
static LAST_BEAT: AtomicU64 = AtomicU64::new(0);

#[cfg(unix)]
fn elapsed_ms() -> u64 {
    BEGAN.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// The main loop is alive: called where the daemon reports its heartbeat, and once when the
/// loop starts.
#[cfg(unix)]
pub fn beat() {
    LAST_BEAT.store(elapsed_ms() + 1, Ordering::SeqCst);
}

#[cfg(not(unix))]
pub fn beat() {}

/// How long ago the last heartbeat was, in seconds. None if there has been none.
#[cfg(unix)]
pub fn beat_age() -> Option<f64> {
    match LAST_BEAT.load(Ordering::SeqCst) {
        0 => None,
        at => Some(elapsed_ms().saturating_sub(at - 1) as f64 / 1000.0),
    }
}

/// Start the watchdog thread. `home` is where the log is.
#[cfg(unix)]
pub fn start(home: &Path, pace: Pace) {
    let home = home.to_path_buf();
    let _ = std::thread::Builder::new()
        .name(String::from("watchdog"))
        .spawn(move || {
            let pause = |seconds: f64| std::thread::sleep(Duration::from_secs_f64(seconds));
            loop {
                pause(pace.check_every);
                if !stalled(beat_age(), pace.limit) {
                    continue;
                }
                pause(pace.second_look);
                if let Some(age) = beat_age().filter(|age| stalled(Some(*age), pace.limit)) {
                    give_up(home, age);
                }
            }
        });
}

/// Say why, and end the process. Saying it must not be able to keep the process alive: the
/// main thread is stuck, perhaps in a write to the very log or to the terminal, so the line
/// is written by a thread of its own, which is waited for two seconds and no longer.
#[cfg(unix)]
fn give_up(home: PathBuf, age: f64) -> ! {
    let line = format!(
        "the daemon has not come round for {} s: ending this process (exit code {}) so that a fresh one is started",
        age as i64, STALL_EXIT_CODE
    );
    let (done, wait) = std::sync::mpsc::channel::<()>();
    let _ = std::thread::Builder::new()
        .name(String::from("watchdog-says"))
        .spawn(move || {
            use std::io::Write;
            let stamp = crate::winfs::local_format(crate::winfs::real_now(), "%Y-%m-%d %H:%M:%S");
            if let Ok(mut log) = crate::winfs::append_file(&home.join("sync.log")) {
                let _ = log.write_all(format!("{} {}\n", stamp, line).as_bytes());
            }
            let _ = done.send(());
            eprintln!("claude-session-sync: {}", line);
        });
    let _ = wait.recv_timeout(Duration::from_secs(2));
    // SAFETY: ends the process at once, without running anything that the stuck thread
    // could be holding up. What the process had open (the lock among it) is closed by the
    // system.
    unsafe { libc::_exit(STALL_EXIT_CODE) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consts::STALL_SEC;

    #[test]
    fn a_heartbeat_is_too_old_at_the_limit_the_windows_takeover_has() {
        assert!(!stalled(None, STALL_SEC)); // no heartbeat yet: nothing to go by
        assert!(!stalled(Some(0.0), STALL_SEC));
        assert!(!stalled(Some(61.0), STALL_SEC)); // the daemon beats once a minute while it waits
        assert!(!stalled(Some(STALL_SEC - 0.001), STALL_SEC));
        assert!(stalled(Some(STALL_SEC), STALL_SEC));
        assert!(stalled(Some(10.0 * STALL_SEC), STALL_SEC));
        assert!(!stalled(Some(f64::NAN), STALL_SEC));
        assert_eq!(STALL_SEC, 900.0);
        assert_ne!(STALL_EXIT_CODE, 0);
    }

    #[cfg(unix)]
    #[test]
    fn the_watchdog_is_as_patient_as_the_windows_takeover() {
        let pace = Pace::default();
        assert_eq!(pace.limit, STALL_SEC);
        assert_eq!(pace.second_look, 20.0); // the takeover's "20 s to show it is alive after all"
        assert!(pace.check_every <= 60.0);
        // a heartbeat is fresh the moment it is given, and ages from then on
        beat();
        let age = beat_age().unwrap();
        assert!((0.0..5.0).contains(&age), "{}", age);
        assert!(!stalled(Some(age), pace.limit));
    }
}
