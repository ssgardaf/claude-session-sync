//! claude-session-sync: keep the Claude desktop app's Code-tab session list the same
//! under every account signed in on this computer (Windows, macOS, Linux).
//!
//! The app stores one list per signed-in account and organization:
//!
//! ```text
//! %APPDATA%\Claude\claude-code-sessions\<account>\<org>\local_<id>.json                   Windows
//! ~/Library/Application Support/Claude/claude-code-sessions/<account>/<org>/local_<id>.json   macOS
//! ~/.config/Claude/claude-code-sessions/<account>/<org>/local_<id>.json                       Linux
//! ```
//!
//! Transcripts (`~/.claude/projects/*/<cliSessionId>.jsonl`) are not tied to an account, so
//! a session only "disappears" after an account switch because its record sits in another
//! folder. This tool copies each record, byte for byte, into every account folder, so the
//! list is already complete when another account signs in.
//!
//! Rules (checked against the app's own loader and save code):
//!
//! * The copy with the latest `lastActivityAt` wins. The app re-saves records without any
//!   activity, so file time alone would pick stale copies.
//! * A copy the app wrote is replaced only when nothing of the user's can be lost by it.
//!   Otherwise it is reported as a conflict and both copies stay as they are, until the
//!   session is used again or `resolve` is given:
//!   - more activity elsewhere, but this copy was opened or saved after that activity,
//!     and the two differ in something the user decides (archived or not, the title);
//!   - equal activity and different content, unless the newer file provably descends
//!     from this one.
//!
//!   A copy this tool made gives way, also when the app has merely saved it again; a
//!   decision made on top of it (archived, renamed) is a conflict like any other.
//! * A file this tool cannot read as a session record is never touched. A file that is
//!   empty or not JSON at all (a broken write) is repaired, and the broken file is kept.
//! * Records are added only to folders the app has really settled in (known from its
//!   log), not to the folders it passes through for a second or two while switching.
//! * A session deleted in an account (`deleted_<id>` marker) is never added back there,
//!   and one deleted after its last use anywhere is not handed to further accounts.
//!   Deletions are not carried to accounts that already hold the session unless
//!   `--delete-sync` is given; then a session whose own `deleted_<file id>` marker was
//!   written while this tool was running, after the session's last use anywhere, is
//!   moved to `quarantine\` in the other accounts (nothing is erased; `restore` undoes it).
//! * While the desktop app is running, existing records in the folder it has loaded are
//!   not replaced or removed (it would only write its stale in-memory copy back); new ones
//!   are added. Which folder that is comes from the app's own log; if that cannot be told,
//!   the folder it wrote to in the last ten minutes, and failing that every folder.
//! * Before a version this tool did not write is replaced, it is kept under `replaced\`.
//!   Every write is journaled first and can be reversed with `undo --since`.
//! * Reading never makes the app's save fail: on Windows files are opened with full
//!   sharing, and if the app's atomic rename is refused while we hold a file, its own code
//!   falls back to writing in place (elsewhere an open file is in nobody's way). A
//!   half-written file is recognised and skipped.
//! * `archived-sessions.idx` is left alone (the app treats it as a hint and heals it), as
//!   are transcripts, credentials and every other file.

//!
//! What differs between the systems is kept apart: `winfs` / `unixfs` (files), `process` /
//! `process_unix` (the process list), `install` / `install_unix` (starting by itself: Task
//! Scheduler, launchd, systemd), `sandbox` and `winreg` (Windows only), `signals` and
//! `watchdog` (macOS and Linux only). Everything else is the same code everywhere.

pub mod applog;
pub mod apply;
pub mod cli;
pub mod consts;
pub mod daemon;
pub mod history;
pub mod install;
pub mod paths;
pub mod plan;
pub mod process;
pub mod pyjson;
pub mod records;
#[cfg(windows)]
pub mod sandbox;
pub mod signals;
pub mod sync;
pub mod watchdog;
pub mod winfs;
#[cfg(windows)]
pub mod winreg;

#[cfg(test)]
mod tests;

pub use consts::VERSION;
pub use daemon::{acquire_lock, lock_for_daemon, release_lock, Lock};
pub use paths::{Env, LogDirs, Paths};
pub use sync::{Homes, Signed, Sync};
