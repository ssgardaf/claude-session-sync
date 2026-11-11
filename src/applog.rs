//! Follows the desktop app's own log to learn which account folder it has loaded.

use std::collections::BTreeSet;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::bytes::Regex;

use crate::consts::{HOME_SETTLE_SEC, TRANSIENT_SEC};
use crate::paths::LogDirs;
use crate::sync::Signed;
use crate::winfs;

fn event_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"(?-u)(\d{4}-\d\d-\d\d \d\d:\d\d:\d\d)[^\r\n]{0,40}?\[LocalSessionManager\] ",
            r"(?:Initialization succeeded[^\r\n]{0,12}?accountId=([0-9a-fA-F-]{36}), ",
            r"orgId=([0-9a-fA-F-]{36})|(Account logged out))"
        ))
        .expect("the log pattern is valid")
    })
}

fn rotated_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^main[0-9]+\.log$").expect("valid pattern"))
}

/// One sign-in or sign-out line: its local time stamp (as text) and the folder, None for a sign-out.
#[derive(Clone, Debug)]
struct Event {
    stamp: String,
    leaf: Option<String>,
}

fn events_in(data: &[u8]) -> Vec<Event> {
    event_re()
        .captures_iter(data)
        .map(|c| {
            let stamp = String::from_utf8_lossy(&c[1]).into_owned();
            let leaf = if c.get(4).is_some() {
                None
            } else {
                let account = String::from_utf8_lossy(&c[2]).to_lowercase();
                let org = String::from_utf8_lossy(&c[3]).to_lowercase();
                Some(format!("{}/{}", account, org))
            };
            Event { stamp, leaf }
        })
        .collect()
}

/// `state()` gives `Signed::In("<account>/<org>")`, `Signed::Out` after a sign-out, or
/// `Signed::Unknown`.
pub struct AppLog {
    dirs: LogDirs,
    path: Option<PathBuf>,
    offset: u64,
    result: Signed,
}

impl AppLog {
    pub fn new(dirs: LogDirs) -> AppLog {
        AppLog {
            dirs,
            path: None,
            offset: 0,
            result: Signed::Unknown,
        }
    }

    pub fn candidates(&self) -> Vec<PathBuf> {
        self.dirs.candidates()
    }

    /// `(path, mtime in ns, size)` of the log written most recently.
    pub fn current(&self) -> Option<(PathBuf, i64, u64)> {
        let mut newest: Option<(PathBuf, i64, u64)> = None;
        for p in self.candidates() {
            let Ok(st) = winfs::stat(&p) else {
                continue;
            };
            if newest.as_ref().is_none_or(|n| st.mtime_ns > n.1) {
                newest = Some((p, st.mtime_ns, st.size));
            }
        }
        newest
    }

    /// The last event in `path` from `start` on, and where to read from next time.
    fn last(path: &Path, start: u64) -> io::Result<(Option<Event>, u64)> {
        let mut fh = winfs::open_shared(path)?;
        fh.seek(SeekFrom::Start(start))?;
        let mut data = Vec::new();
        fh.read_to_end(&mut data)?;
        // leave a line that is still being written for next time
        let end = data.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        Ok((events_in(&data[..end]).pop(), start + end as u64))
    }

    pub fn state(&mut self) -> Signed {
        let _ = self.advance();
        self.result.clone()
    }

    fn advance(&mut self) -> io::Result<()> {
        let Some(newest) = self.current() else {
            return Ok(());
        };
        // another file, or it was rotated
        let fresh = self.path.as_ref() != Some(&newest.0) || newest.2 < self.offset;
        if fresh {
            self.path = Some(newest.0.clone());
            self.offset = 0;
        }
        let (mut event, offset) = Self::last(&newest.0, self.offset)?;
        self.offset = offset;
        if event.is_none() && fresh {
            // nothing in the current file yet: the answer is in the previous one
            let older = newest.0.with_file_name("main1.log");
            if older.exists() {
                event = Self::last(&older, 0)?.0;
            }
        }
        if let Some(e) = event {
            self.result = match e.leaf {
                None => Signed::Out,
                Some(leaf) => Signed::In(leaf),
            };
        }
        Ok(())
    }

    /// Folders the app has really settled in, from the log's history. While switching accounts
    /// the app signs in for a second or two to a mix of the old account and the new
    /// organization; such a sign-in, replaced within TRANSIENT_SEC by one to another folder,
    /// does not count. None if no log can be read.
    pub fn homes(&self, full: bool) -> Option<BTreeSet<String>> {
        let newest = self.current()?;
        let mut files = vec![newest.0.clone()];
        if full {
            if let Some(older) = Self::rotated(&newest.0) {
                files = older;
                files.push(newest.0.clone());
            }
        }
        let mut events: Vec<(f64, Option<String>)> = Vec::new();
        for path in &files {
            let Ok(data) = winfs::read_shared(path) else {
                continue;
            };
            for e in events_in(&data) {
                if let Some(t) = winfs::parse_local_stamp(&e.stamp) {
                    events.push((t, e.leaf));
                }
            }
        }
        events.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut homes = BTreeSet::new();
        for (i, (t, leaf)) in events.iter().enumerate() {
            let Some(leaf) = leaf else {
                continue;
            };
            if homes.contains(leaf) {
                continue;
            }
            let next = events[i + 1..].iter().find(|e| e.1.as_ref() != Some(leaf));
            let settled = match next {
                None => winfs::real_now() - t >= HOME_SETTLE_SEC,
                Some((_, None)) => true, // the user signed out of it: they were really there
                Some((nt, Some(_))) => nt - t > TRANSIENT_SEC,
            };
            if settled {
                homes.insert(leaf.clone());
            }
        }
        Some(homes)
    }

    /// The rotated logs beside `newest` (`main1.log`...), oldest first. None if they cannot be listed.
    fn rotated(newest: &Path) -> Option<Vec<PathBuf>> {
        let folder = newest.parent()?;
        let mut older = Vec::new();
        for e in std::fs::read_dir(folder).ok()? {
            let e = e.ok()?;
            if rotated_re().is_match(&e.file_name().to_string_lossy()) {
                let path = e.path();
                let mtime = winfs::stat(&path).ok()?.mtime_ns;
                older.push((mtime, path));
            }
        }
        older.sort_by_key(|o| o.0);
        Some(older.into_iter().map(|o| o.1).collect())
    }
}
