//! Reading: the account folders, the session records in them, the deletion markers.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::OnceLock;

use regex::Regex;

use crate::pyjson::{self, JsonError, J};
use crate::sync::Sync;
use crate::winfs::{self, Stat};

/// The app's own shape for session ids; nothing else becomes a file name.
pub fn is_id(s: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    // `\z` is the very end: an id followed by a line feed is not an id (the Python version's
    // `$` let that through, and such an id then failed as a file name on every pass)
    RE.get_or_init(|| Regex::new(r"^[A-Za-z0-9_-]+\z").expect("valid pattern"))
        .is_match(s)
}

pub fn is_uuid(s: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^[0-9a-fA-F]{8}(-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}\z").expect("valid pattern")
    })
    .is_match(s)
}

/// One session record as found in one account folder.
#[derive(Clone, Debug)]
pub struct Rec {
    pub path: PathBuf,
    /// File time in ns, and size, when it was read.
    pub mtime: i64,
    pub size: u64,
    /// The conversation it points at.
    pub cli: String,
    /// Every conversation id it has had.
    pub ids: BTreeSet<String>,
    pub archived: bool,
    /// `lastActivityAt` (epoch ms).
    pub last: f64,
    /// `lastFocusedAt` (epoch ms).
    pub focus: f64,
    /// For display: the first 40 characters of the title.
    pub title: String,
    /// What the user decides about a session: (archived, full title).
    pub user: (bool, String),
}

/// Why a `local_*.json` file is not a record we can use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// Empty or not JSON (a broken write).
    Torn,
    /// Something we do not understand or could not read, and therefore never touch.
    Foreign,
}

/// The records of one session, by account folder, in folder order.
pub type Copies = Vec<(String, Rc<Rec>)>;

pub fn copy_in<'a>(copies: &'a Copies, leaf: &str) -> Option<&'a Rc<Rec>> {
    copies.iter().find(|(n, _)| n == leaf).map(|(_, r)| r)
}

/// A `local_*.json` file that is not a usable record: (why, mtime_ns, size).
pub type BadFile = (Why, Option<i64>, Option<u64>);

/// What one look at the store found.
#[derive(Debug, Default)]
pub struct Scan {
    /// The `<account>/<org>` folders that could be listed, with their paths.
    pub lv: Vec<(String, PathBuf)>,
    /// Session ids in the order they were first seen.
    pub order: Vec<String>,
    /// fid -> its record in each folder that has one.
    pub records: HashMap<String, Copies>,
    /// leaf -> fid -> (why, mtime_ns, size) for files that are not usable records.
    pub bad: HashMap<String, HashMap<String, BadFile>>,
    /// leaf -> id -> epoch ms of its deletion marker.
    pub tombs: HashMap<String, HashMap<String, i64>>,
    /// Folders whose listing failed: nothing is known about them.
    pub unlisted: Vec<String>,
}

impl Scan {
    pub fn dir(&self, leaf: &str) -> Option<&PathBuf> {
        self.lv.iter().find(|(n, _)| n == leaf).map(|(_, p)| p)
    }
}

/// Parse the leading integer of a deletion marker the way the Python version did.
fn parse_marker(data: &[u8]) -> Option<i64> {
    let head = &data[..data.len().min(32)];
    let text: String = head
        .iter()
        .filter(|b| b.is_ascii())
        .map(|&b| b as char)
        .collect();
    let t = text.trim_matches(|c: char| {
        c == ' ' || ('\t'..='\r').contains(&c) || ('\x1c'..='\x1f').contains(&c)
    });
    let (neg, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
    {
        return None;
    }
    let mut v: i128 = 0;
    for c in digits.chars() {
        if c == '_' {
            continue;
        }
        v = v * 10 + c.to_digit(10)? as i128;
    }
    if neg {
        v = -v;
    }
    Some(v.clamp(i64::MIN as i128, i64::MAX as i128) as i64)
}

impl Sync {
    /// Every real `<account>/<org>` folder, keyed by its full name. Folders that are links are
    /// skipped (the app refuses to write through them), as is anything not named like a uuid.
    pub fn leaves(&self) -> Vec<(String, PathBuf)> {
        fn names(dir: &Path) -> std::io::Result<Vec<String>> {
            let mut out = Vec::new();
            for e in std::fs::read_dir(dir)? {
                out.push(e?.file_name().to_string_lossy().into_owned());
            }
            out.sort();
            Ok(out)
        }
        let root = &self.p.root;
        let mut out = Vec::new();
        if !winfs::real_dir(root) {
            return out;
        }
        let Ok(accounts) = names(root) else {
            return Vec::new();
        };
        for a in accounts {
            let ap = root.join(&a);
            if !is_uuid(&a) || !winfs::real_dir(&ap) {
                continue;
            }
            let Ok(orgs) = names(&ap) else {
                return Vec::new();
            };
            for o in orgs {
                let op = ap.join(&o);
                if is_uuid(&o) && winfs::real_dir(&op) {
                    out.push((format!("{}/{}", a.to_lowercase(), o.to_lowercase()), op));
                }
            }
        }
        out
    }

    /// `(Some(record), None)` or `(None, Some(Torn | Foreign))`.
    pub(crate) fn read(
        &mut self,
        path: &Path,
        st: &Stat,
        fid: &str,
    ) -> (Option<Rc<Rec>>, Option<Why>) {
        let key = (st.mtime_ns, st.size);
        if let Some((version, rec, why)) = self.meta.get(path) {
            if *version == key {
                return (rec.clone(), *why);
            }
        }
        let (rec, why) = self.parse_record(path, st, fid);
        if why != Some(Why::Foreign) || rec.is_some() || st.size > self.max_record_bytes {
            // a read that failed is tried again next time
            self.meta
                .insert(path.to_path_buf(), (key, rec.clone(), why));
        }
        (rec, why)
    }

    fn parse_record(&self, path: &Path, st: &Stat, fid: &str) -> (Option<Rc<Rec>>, Option<Why>) {
        let foreign = (None, Some(Why::Foreign));
        let torn = (None, Some(Why::Torn));
        if st.size == 0 {
            return torn;
        }
        if st.size > self.max_record_bytes {
            return foreign;
        }
        let Ok(data) = self.read_shared(path) else {
            return foreign; // one odd or locked record must never stop the pass
        };
        let Some(text) = pyjson::decode_utf8_sig(&data) else {
            return torn;
        };
        let j = match pyjson::loads(text) {
            Ok(j) => j,
            Err(JsonError::Syntax) => return torn,
            Err(JsonError::TooDeep) => return foreign,
            // `NaN`, `Infinity` and `-Infinity` are not JSON: the app itself cannot read such a
            // file, so it is neither copied to other accounts nor replaced. (The Python version
            // took it for a record like any other and spread it.)
            Err(JsonError::NonFinite) => return foreign,
        };
        if !j.is_object()
            || j.get("sessionId").and_then(J::as_str) != Some(format!("local_{}", fid).as_str())
        {
            return foreign;
        }
        let Some(cli) = j
            .get("cliSessionId")
            .and_then(J::as_str)
            .filter(|c| is_id(c))
        else {
            return foreign;
        };
        let mut ids: BTreeSet<String> = BTreeSet::new();
        ids.insert(cli.to_string());
        if let Some(u) = j.get("unarchivedCliSessionId").and_then(J::as_str) {
            ids.insert(u.to_string());
        }
        for k in ["priorCliSessionIds", "overflowCliSessionIds"] {
            if let Some(list) = j.get(k).and_then(J::as_array) {
                ids.extend(list.iter().filter_map(J::as_str).map(String::from));
            }
        }
        ids.retain(|i| is_id(i));
        let number = |key: &str| -> Result<Option<f64>, pyjson::Overflow> {
            match j.get(key) {
                Some(v) => v.num(),
                None => Ok(None),
            }
        };
        // a number too large to be one makes the record foreign, like any other oddity
        let parsed = (|| -> Result<(f64, f64), pyjson::Overflow> {
            let last = match number("lastActivityAt")? {
                Some(v) => v,
                None => number("createdAt")?.unwrap_or(0.0),
            };
            let focus = number("lastFocusedAt")?.unwrap_or(0.0);
            Ok((last, focus))
        })();
        let Ok((last, focus)) = parsed else {
            return foreign;
        };
        let archived = matches!(j.get("isArchived"), Some(J::Bool(true)));
        let full_title = match j.get("title") {
            Some(t) if t.truthy() => t.text(),
            _ => String::new(),
        };
        let rec = Rec {
            path: path.to_path_buf(),
            mtime: st.mtime_ns,
            size: st.size,
            cli: cli.to_string(),
            ids,
            archived,
            last,
            focus,
            title: full_title.chars().take(40).collect(),
            user: (archived, full_title),
        };
        (Some(Rc::new(rec)), None)
    }

    /// The conversation ids that have a transcript.
    pub(crate) fn transcripts(&mut self, force: bool) -> Rc<HashSet<String>> {
        let since = self.now() - self.tr.0;
        if !force && (0.0..60.0).contains(&since) {
            return self.tr.1.clone();
        }
        let mut found = HashSet::new();
        let _ = (|| -> std::io::Result<()> {
            for d in self.list_dir(&self.p.projects)? {
                if winfs::entry_is_dir(&d) {
                    for f in self.list_dir(&d.path())? {
                        let name = f.file_name().to_string_lossy().into_owned();
                        if let Some(id) = name.strip_suffix(".jsonl") {
                            found.insert(id.to_string());
                        }
                    }
                }
            }
            Ok(())
        })();
        self.tr = (self.now(), Rc::new(found));
        self.tr.1.clone()
    }

    /// Look at every account folder. A folder whose listing fails is left out of the pass
    /// entirely: nothing is known about it.
    pub fn scan(&mut self) -> Scan {
        let mut scan = Scan::default();
        let mut seen: HashSet<PathBuf> = HashSet::new();
        for (name, path) in self.leaves() {
            let entries = match self.list_dir(&path) {
                Ok(e) => e,
                Err(_) => {
                    scan.unlisted.push(name);
                    continue;
                }
            };
            scan.lv.push((name.clone(), path.clone()));
            let mut bad = HashMap::new();
            let mut tombs = HashMap::new();
            for e in entries {
                let n = e.file_name().to_string_lossy().into_owned();
                if let Some(id) = n.strip_prefix("deleted_") {
                    if !is_id(id) {
                        continue;
                    }
                    let ts = match self
                        .read_shared(&e.path())
                        .ok()
                        .and_then(|d| parse_marker(&d))
                    {
                        Some(ts) => ts,
                        None => match winfs::stat_entry(&e) {
                            Ok(st) => (st.mtime() * 1000.0) as i64,
                            Err(_) => (self.now() * 1000.0) as i64,
                        },
                    };
                    tombs.insert(id.to_string(), ts);
                } else if let Some(fid) = n
                    .strip_prefix("local_")
                    .and_then(|rest| rest.strip_suffix(".json"))
                {
                    if !is_id(fid) {
                        continue;
                    }
                    let st = match winfs::stat_entry(&e) {
                        Ok(st) => st,
                        Err(_) => {
                            bad.insert(fid.to_string(), (Why::Foreign, None, None));
                            continue;
                        }
                    };
                    let p = e.path();
                    seen.insert(p.clone());
                    match self.read(&p, &st, fid) {
                        (Some(rec), _) => {
                            if !scan.records.contains_key(fid) {
                                scan.order.push(fid.to_string());
                            }
                            scan.records
                                .entry(fid.to_string())
                                .or_default()
                                .push((name.clone(), rec));
                        }
                        (None, why) => {
                            bad.insert(
                                fid.to_string(),
                                (
                                    why.unwrap_or(Why::Foreign),
                                    Some(st.mtime_ns),
                                    Some(st.size),
                                ),
                            );
                        }
                    }
                }
            }
            scan.bad.insert(name.clone(), bad);
            scan.tombs.insert(name, tombs);
        }
        self.meta.retain(|p, _| seen.contains(p));
        self.shas.retain(|p, _| seen.contains(p));
        scan
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_numbers() {
        assert_eq!(parse_marker(b"1794404914215"), Some(1794404914215));
        assert_eq!(parse_marker(b" 12\r\n"), Some(12));
        assert_eq!(parse_marker(b"-5"), Some(-5));
        assert_eq!(parse_marker(b"1_000"), Some(1000));
        assert_eq!(parse_marker(b""), None);
        assert_eq!(parse_marker(b"12x"), None);
        assert_eq!(parse_marker(b"1.5"), None);
        // only the first 32 bytes count
        assert_eq!(
            parse_marker(format!("{}{}", "1".repeat(10), " ".repeat(40)).as_bytes()),
            Some(1111111111)
        );
    }

    #[test]
    fn ids() {
        assert!(is_id("abc-DEF_09"));
        assert!(!is_id(""));
        assert!(!is_id("..\\x"));
        assert!(!is_id("a b"));
        assert!(!is_id("abc\n"));
        assert!(!is_id("abc\r\n"));
        assert!(!is_id("\nabc"));
        assert!(!is_uuid("00000000-0000-4000-8000-0000000000a0\n"));
        assert!(is_uuid("00000000-0000-4000-8000-0000000000a0"));
        assert!(!is_uuid("backup-copy"));
    }
}
