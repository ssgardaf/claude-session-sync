//! Being told to end, on macOS and Linux: by launchd or systemd when the daemon is stopped
//! or the user logs out (SIGTERM), by Ctrl+C (SIGINT), by a terminal that closes (SIGHUP).
//!
//! The signal only leaves a note. The daemon looks for it where it looks for `stop`'s flag,
//! between passes, so it never ends in the middle of a change; whatever call was under way
//! when the signal came is carried on with. On Windows there is nothing to watch for: the
//! daemon has no console, and is ended through the stop flag.

#[cfg(unix)]
use std::sync::atomic::{AtomicI32, Ordering};

#[cfg(unix)]
static RECEIVED: AtomicI32 = AtomicI32::new(0);

#[cfg(unix)]
extern "C" fn note(signal: libc::c_int) {
    // (storing a number is all a signal handler may safely do)
    RECEIVED.store(signal, Ordering::SeqCst);
}

/// From now on, a signal to end is noted instead of ending this process on the spot.
#[cfg(unix)]
pub fn watch_for_termination() {
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        // SAFETY: sigaction is plain data; the handler only stores an integer. With
        // SA_RESTART, a call the signal interrupts is taken up again instead of failing.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = note as extern "C" fn(libc::c_int) as libc::sighandler_t;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }
}

#[cfg(not(unix))]
pub fn watch_for_termination() {}

/// The signal to end that has come in, if any, by name.
#[cfg(unix)]
pub fn received() -> Option<&'static str> {
    match RECEIVED.load(Ordering::SeqCst) {
        0 => None,
        libc::SIGTERM => Some("SIGTERM"),
        libc::SIGINT => Some("SIGINT"),
        libc::SIGHUP => Some("SIGHUP"),
        _ => Some("a signal"),
    }
}

#[cfg(not(unix))]
pub fn received() -> Option<&'static str> {
    None
}
