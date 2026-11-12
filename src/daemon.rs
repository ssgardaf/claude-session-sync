//! The daemon: the cheap change check, the heartbeat, the watch loop, the single-instance lock.

use std::fs::{self, File, OpenOptions};
use std::io;
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;

use serde_json::{json, Value};
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    LockFileEx, UnlockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
};
#[cfg(windows)]
use windows_sys::Win32::System::IO::OVERLAPPED;

use crate::apply::Counts;
use crate::consts::*;
use crate::process;
use crate::sync::{ordered_json, Signed, Sync};
use crate::winfs::{self, Expect};

/// (leaf, file name, mtime_ns, size) of every record and marker, sorted.
pub type Fingerprint = Vec<(String, String, i64, u64)>;

#[derive(Clone, Copy, Default)]
struct Tally {
    add: usize,
    update: usize,
    repair: usize,
    quarantine: usize,
}

struct Loop {
    interval: f64,
    quick: f64,
    started: i64,
    last_full: f64,
    baseline: Option<Fingerprint>,
    last_change: Option<i64>,
    trouble: u32,
    app: Option<(bool, Option<Signed>)>,
    done: Counts,
    tally: Tally,
    tally_at: f64,
    /// passes must not wait until then
    must_until: f64,
    /// put-off items are owed another look
    again: bool,
    beat_at: f64,
}

fn panic_text(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        String::from("unknown error")
    }
}

impl Sync {
    pub fn fingerprint(&self) -> Fingerprint {
        let mut items = Vec::new();
        for (name, path) in self.leaves() {
            // so that a brand-new, still empty account folder counts as a change
            items.push((name.clone(), String::new(), 0, 0));
            let listed = (|| -> io::Result<()> {
                for e in self.list_dir(&path)? {
                    let n = e.file_name().to_string_lossy().into_owned();
                    if n.starts_with("local_") || n.starts_with("deleted_") {
                        let st = winfs::stat_entry(&e)?;
                        items.push((name.clone(), n, st.mtime_ns, st.size));
                    }
                }
                Ok(())
            })();
            if listed.is_err() {
                items.push((name, String::from("?"), 0, 0));
            }
        }
        items.sort();
        items
    }

    fn write_status(&mut self, fields: &[(&str, Value)]) {
        if let Some(hook) = self.hooks.on_status.as_mut() {
            hook();
        }
        if winfs::make_dirs(&self.p.home).is_ok() {
            let data = ordered_json(fields).into_bytes();
            let _ = winfs::atomic_write(&self.home("status.json"), &data, None, false, Expect::Any);
        }
    }

    pub fn stop_requested(&self) -> bool {
        let flag = self.home("stop.flag");
        if flag.exists() {
            let _ = fs::remove_file(&flag);
            return true;
        }
        false
    }

    /// One summary line a minute at most, however busy the sessions are.
    fn report(&mut self, lp: &mut Loop, force: bool) {
        let t = lp.tally;
        if (t.add + t.update + t.repair + t.quarantine > 0)
            && (force || !(0.0..60.0).contains(&(self.now() - lp.tally_at)))
        {
            self.log(&format!(
                "synced: added {}, updated {}, repaired {}, removed {}",
                t.add, t.update, t.repair, t.quarantine
            ));
            lp.tally = Tally::default();
            lp.tally_at = self.now();
        }
    }

    fn beat(&mut self, lp: &mut Loop) {
        // (macOS, Linux: the daemon's own watchdog goes by the same event as the heartbeat
        // in status.json, by which the Windows takeover judges)
        crate::watchdog::beat();
        lp.beat_at = self.now();
        let fields = [
            ("pid", json!(std::process::id())),
            ("version", json!(VERSION)),
            ("started", json!(lp.started)),
            ("heartbeat", json!(self.now() as i64)),
            ("last_full", json!(lp.last_full as i64)),
            ("last_change", json!(lp.last_change)),
            ("interval", json!(lp.interval as i64)),
            ("quick", json!(lp.quick as i64)),
            ("folders", json!(self.seen.0)),
            ("sessions", json!(self.seen.1)),
            ("waiting", json!(lp.done.deferred)),
            ("failed", json!(lp.done.failed)),
            ("held", json!(self.notes.held.len())),
            ("conflicts", json!(self.notes.conflicts.len())),
            ("retrying", json!(self.retry.len())),
            ("delete_sync", json!(self.delete_sync)),
            ("days", crate::sync::num_json(self.days)),
            ("last_error", json!(self.last_error)),
            ("lock_stamp", json!(self.lock_stamp)),
        ];
        self.write_status(&fields);
    }

    /// The part of one turn of the loop that may fail without ending the daemon.
    fn turn(&mut self, lp: &mut Loop, now: f64, due_full: bool) {
        let quick = lp.quick != 0.0;
        let seen = if quick {
            Some(self.app_state())
        } else {
            lp.app.clone()
        };
        // signed out or in, quit or started: sync right now
        let switched = quick && lp.app.is_some() && seen != lp.app;
        lp.app = seen;
        if switched {
            if let Some((_, known)) = self.homes_cache.take() {
                // where the app is at home may have changed: ask the log again
                self.homes_cache = Some((0.0, known));
            }
        }
        if due_full
            || switched
            || lp.again
            || (quick && (Some(self.fingerprint()) != lp.baseline || self.something_due()))
        {
            let mut wrote_any = false;
            // In such a pass every copy is brought up to date, busy session or not. If some of it
            // had to be put off because a file was saved a moment before (the sign-out), the next
            // looks are such passes too, for a few seconds: long enough for that save, not for
            // anything else.
            if due_full || switched {
                lp.must_until = now + 15.0;
            }
            self.urgent = 0.0 <= lp.must_until - now;
            let mut pre = None;
            for _ in 0..3 {
                // what was on disk before this pass looked: anything that changes after this
                // point, even during the pass, differs from it and triggers the next pass
                pre = Some(self.fingerprint());
                lp.done = self.run();
                if let Some(hook) = self.hooks.after_run.as_mut() {
                    hook();
                }
                if lp.done.wrote() != 0 {
                    wrote_any = true;
                    lp.last_change = Some(now as i64);
                    lp.tally.add += lp.done.add;
                    lp.tally.update += lp.done.update;
                    lp.tally.repair += lp.done.repair;
                    lp.tally.quarantine += lp.done.quarantine;
                }
                if lp.done.wrote() == 0 || self.cut_short {
                    break; // (what a pass that was cut short did is counted like any other)
                }
                pre = None; // still writing after three passes: look again next time
            }
            // an item that was put off (its file had just changed) is simply looked at again
            // shortly; an item that failed backs off by itself; only a pass that could not start
            // slows us down
            lp.baseline = if lp.done.deferred != 0 { None } else { pre };
            lp.trouble = if self.aborted { lp.trouble + 1 } else { 0 };
            lp.again = lp.done.deferred != 0 && !self.aborted;
            if self.settling == 0 {
                lp.must_until = 0.0; // nothing is waiting for a save to finish
            }
            self.report(lp, due_full || self.cut_short);
            if self.cut_short {
                // told to end in the middle of it: not "in sync", and not a full pass made
                self.log(&format!(
                    "pass stopped before it was done: {} change(s) left for the next start",
                    lp.done.deferred
                ));
            } else if due_full {
                let waiting = if lp.done.deferred != 0 {
                    format!(", {} waiting", lp.done.deferred)
                } else {
                    String::new()
                };
                self.log(&format!(
                    "full pass: {} ({} folders, {} sessions){}",
                    if wrote_any { "synced" } else { "in sync" },
                    self.seen.0,
                    self.seen.1,
                    waiting
                ));
                lp.last_full = now;
                self.sweep();
            }
        }
    }

    /// Run as a daemon: a full pass every `interval` seconds, a cheap change check every `quick`
    /// seconds (0 = full passes only), and a pass the moment the app signs out, quits or starts.
    /// `tick` is how often the stop flag and the app's sign-in state are looked at while waiting.
    pub fn watch(
        &mut self,
        interval: f64,
        quick: f64,
        tick: f64,
        max_loops: Option<usize>,
        sleep: &mut dyn FnMut(f64),
    ) {
        self.stop_requested(); // a flag left by an earlier stop must not end this run
        self.daemon = true;
        let mut lp = Loop {
            interval,
            quick,
            started: self.now() as i64,
            last_full: 0.0,
            baseline: None,
            last_change: None,
            trouble: 0,
            app: None,
            done: Counts::default(),
            tally: Tally::default(),
            tally_at: self.now(),
            must_until: 0.0,
            again: false,
            beat_at: 0.0,
        };
        let mut loops = 0usize;
        self.log(&format!(
            "daemon {} started: full pass every {}s, change check every {}s{}",
            VERSION,
            interval as i64,
            quick as i64,
            if self.delete_sync {
                ", deletions carried over"
            } else {
                ""
            }
        ));
        // The first sign of life comes before the first pass, not after it: a pass that hangs
        // (or merely takes long) is then that of a daemon known to have started, to `status`,
        // to the takeover on Windows and to the watchdog elsewhere.
        self.beat(&mut lp);
        loop {
            let now = self.now();
            let due_full = !(0.0..interval).contains(&(now - lp.last_full)); // also true if the clock went backwards
            let outcome = catch_unwind(AssertUnwindSafe(|| self.turn(&mut lp, now, due_full)));
            if let Err(payload) = outcome {
                // never let one bad pass kill the daemon
                let what = format!("panic: {}", panic_text(payload));
                self.last_error = Some(what.clone());
                self.log_every(&format!("pass failed: {}", what), 3600.0);
                lp.trouble += 1;
                if due_full {
                    lp.last_full = now;
                }
            }
            self.beat(&mut lp);
            loops += 1;
            self.urgent = true; // whatever is run outside this loop is a pass in its own right
            if max_loops.is_some_and(|m| loops >= m) {
                self.daemon = false;
                return;
            }
            let mut wait = if quick != 0.0 { quick } else { interval };
            if lp.again {
                // a file had just changed: look again as soon as it has settled
                let hurry = self.settling != 0 && 0.0 <= lp.must_until - self.now();
                wait = wait.min(
                    if hurry {
                        URGENT_SETTLE_SEC
                    } else {
                        SRC_SETTLE_SEC
                    } + 1.0,
                );
            }
            if lp.trouble != 0 {
                // no pass can even start: do not hammer it
                let base = if quick != 0.0 { quick } else { 60.0 };
                wait = (base * 2f64.powi(lp.trouble.min(6) as i32)).min(600.0);
            }
            wait = wait.min(interval - (self.now() - lp.last_full)).max(1.0);
            let end = self.now() + wait;
            // bounded, so a clock set back cannot stretch the wait
            for _ in 0..((wait / tick.min(wait)) as usize + 2) {
                if let Some(signal) = crate::signals::received() {
                    // told to end by the system (a stop, a logout) or from the keyboard
                    self.report(&mut lp, true);
                    self.log(&format!("daemon stopped by {}", signal));
                    self.daemon = false;
                    return;
                }
                if self.stop_requested() {
                    self.report(&mut lp, true);
                    self.log("daemon stopped on request");
                    self.daemon = false;
                    return;
                }
                let left = end - self.now();
                if !(0.0 < left && left <= wait) {
                    break;
                }
                sleep(tick.min(left));
                if !(0.0..BEAT_SEC).contains(&(self.now() - lp.beat_at)) {
                    self.beat(&mut lp); // alive, whatever the settings: the watchdog goes by this
                }
                if quick != 0.0 && lp.trouble == 0 {
                    if let Some((true, was)) = &lp.app {
                        // the app signed out or in: do not wait for the next check
                        if Some(self.signed_in()) != *was {
                            break;
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------- the single-instance lock

/// Holds the single-instance lock until dropped.
pub struct Lock {
    file: File,
}

#[cfg(windows)]
fn first_byte() -> OVERLAPPED {
    // SAFETY: OVERLAPPED is plain data; all zeroes means offset 0 and no event.
    unsafe { std::mem::zeroed() }
}

/// Lock the open file exclusively, without waiting. False if another process (or another
/// handle in this one) holds the lock. On Windows it is the first byte that is locked (the
/// same byte the Python version locks, so the two exclude each other).
#[cfg(windows)]
fn try_lock(file: &File) -> bool {
    let mut range = first_byte();
    // SAFETY: the handle is open for the duration of the call; `range` is a valid OVERLAPPED.
    let ok = unsafe {
        LockFileEx(
            file.as_raw_handle() as _,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut range,
        )
    };
    ok != 0
}

#[cfg(windows)]
fn unlock(file: &File) {
    let mut range = first_byte();
    // SAFETY: as in `try_lock`.
    unsafe {
        UnlockFileEx(file.as_raw_handle() as _, 0, 1, 0, &mut range);
    }
}

/// On macOS and Linux the lock is on the open file as a whole (`flock`): like the lock on
/// Windows it is held against other processes and against other handles in this one, is not
/// passed on to programs this one starts, and ends with the process however that ends.
#[cfg(unix)]
fn try_lock(file: &File) -> bool {
    // SAFETY: the descriptor is open for the duration of the call.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

#[cfg(unix)]
fn unlock(file: &File) {
    // SAFETY: as in `try_lock`.
    unsafe {
        libc::flock(file.as_raw_fd(), libc::LOCK_UN);
    }
}

/// Take the single-instance lock on `daemon.lock`, exclusively, without waiting.
/// None if another process (or handle) holds it.
pub fn acquire_lock(home: &Path) -> io::Result<Option<Lock>> {
    winfs::make_dirs(home)?;
    let file = winfs::open_own(
        OpenOptions::new().read(true).append(true).create(true),
        &home.join("daemon.lock"),
    )?;
    if !try_lock(&file) {
        return Ok(None);
    }
    Ok(Some(Lock { file }))
}

/// Does a daemon (or anything else) hold the lock right now? Unlike `acquire_lock`, this
/// creates nothing: where there is no lock file, nothing is running.
pub fn is_running(home: &Path) -> bool {
    let Ok(file) = OpenOptions::new().read(true).open(home.join("daemon.lock")) else {
        return false;
    };
    if !try_lock(&file) {
        return true;
    }
    unlock(&file); // the lock just taken is given back
    false
}

impl Drop for Lock {
    fn drop(&mut self) {
        unlock(&self.file);
    }
}

pub fn release_lock(lock: Lock) {
    drop(lock);
}

/// What status.json says of the daemon that wrote it.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Holder {
    pid: u32,
    heartbeat: f64,
    /// When it says it started (reports of the first Python versions have no such entry).
    started: Option<f64>,
}

/// The daemon on record in status.json, if that can be read.
fn holder(home: &Path) -> Option<Holder> {
    fn number(v: &Value) -> Option<f64> {
        match v {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.trim().parse().ok(),
            _ => None,
        }
    }
    let data = fs::read(home.join("status.json")).ok()?;
    let st: Value = serde_json::from_slice(&data).ok()?;
    let pid = number(st.get("pid")?)?;
    let heartbeat = number(st.get("heartbeat")?)?;
    if !(0.0..=u32::MAX as f64).contains(&pid) {
        return None;
    }
    Some(Holder {
        pid: pid as u32,
        heartbeat,
        started: st.get("started").and_then(number),
    })
}

/// Mark the lock as taken by this daemon: the lock file is given the present time, and
/// what the file system kept of it is the mark. The daemon repeats the mark in every report
/// it makes; `status` takes a report for the lock holder's own only if the two are the same
/// number. They are compared as marks, never as times: setting the clock, forwards or back,
/// changes nothing about which report belongs to which daemon.
fn stamp(lock: &Lock) -> Option<i64> {
    lock.file.set_modified(std::time::SystemTime::now()).ok()?;
    lock_mark(&lock.file.metadata().ok()?)
}

/// The mark on a lock file: its time, as whole nanoseconds.
pub fn lock_mark(lock: &fs::Metadata) -> Option<i64> {
    let since = lock
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    i64::try_from(since.as_nanos()).ok()
}

/// The lock for the daemon. A `status` or `stop` may hold it for a moment, so try for a few
/// seconds; and if the daemon that holds it has not given a sign of life for STALL_SEC, end it
/// and take its place.
///
/// Only a process that is certainly that daemon is ended: it must run one of this tool's own
/// programs, and have been started by the time the daemon on record says it started. (The
/// Python version went by the program's name alone, and counted any Python process; a pid
/// that had passed to an unrelated program could have been ended.) A stalled Python daemon is
/// therefore left alone: it is for `uninstall` to remove.
pub fn lock_for_daemon(s: &mut Sync, sleep: &dyn Fn(f64)) -> io::Result<Option<Lock>> {
    let home = s.p.home.clone();
    for _attempt in 0..12 {
        if let Some(lock) = acquire_lock(&home)? {
            s.lock_stamp = stamp(&lock);
            return Ok(Some(lock));
        }
        if let Some(h) = holder(&home) {
            if (0.0..60.0).contains(&(winfs::real_now() - h.heartbeat)) {
                return Ok(None); // a daemon is at work (the watchdog's usual finding): nothing to do
            }
        }
        sleep(0.5);
    }
    let Some(stalled) = holder(&home) else {
        return Ok(None); // no telling: leave it be
    };
    if winfs::real_now() - stalled.heartbeat < STALL_SEC || stalled.pid == std::process::id() {
        return Ok(None); // alive: leave it be
    }
    let is_that_daemon = || process::is_daemon(stalled.pid, &DAEMON_PROCESS_NAMES, stalled.started);
    if !is_that_daemon() {
        return Ok(None); // another program, or the pid has passed to another process: hands off
    }
    for _ in 0..40 {
        sleep(0.5); // a PC just back from sleep: give it 20 s to show it is alive after all
    }
    // asked again in the last moment: the report must be unchanged, the process still that one
    if holder(&home) != Some(stalled) || !is_that_daemon() {
        return Ok(None);
    }
    s.log(&format!(
        "the running daemon (pid {}) has been silent for {} s: ending it and taking over",
        stalled.pid,
        (winfs::real_now() - stalled.heartbeat) as i64
    ));
    if !process::terminate(stalled.pid) {
        return Ok(None);
    }
    for _ in 0..20 {
        sleep(0.5);
        if let Some(lock) = acquire_lock(&home)? {
            s.lock_stamp = stamp(&lock);
            return Ok(Some(lock));
        }
    }
    Ok(None)
}
