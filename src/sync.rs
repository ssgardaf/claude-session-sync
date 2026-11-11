//! The `Sync` object: its settings, its injection points, and the small files of its own
//! (log, journal, manifest, state).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, DirEntry, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::applog::AppLog;
use crate::consts::*;
use crate::paths::Paths;
use crate::plan::Notes;
use crate::process;
use crate::records::{Rec, Scan, Why};
use crate::winfs::{self, Expect, Stat, WriteError};

/// Seconds since 1970. Tests put their own clock in.
pub type Clock = Rc<dyn Fn() -> f64>;

/// What the app's log says about the folder it has loaded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Signed {
    /// Signed in to `<account>/<org>`.
    In(String),
    /// Signed out: nothing is loaded.
    Out,
    Unknown,
}

/// The folders records may be added to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Homes {
    All,
    Only(BTreeSet<String>),
}

impl Homes {
    pub fn only<I: IntoIterator<Item = S>, S: Into<String>>(leaves: I) -> Homes {
        Homes::Only(leaves.into_iter().map(Into::into).collect())
    }
}

/// A yes-or-no question about a path.
pub type PathTest = Box<dyn Fn(&Path) -> bool>;

/// Something that is told of a path.
pub type PathHook = Box<dyn FnMut(&Path)>;

/// Points where a test can make the outside world misbehave, or watch what happens.
#[derive(Default)]
pub struct Hooks {
    /// Reading this file fails (as if another process had it locked).
    pub read_fault: Option<PathTest>,
    /// Listing this folder fails.
    pub list_fault: Option<PathTest>,
    /// Stands in for the process-list test for the desktop app.
    pub probe_app: Option<Box<dyn Fn() -> bool>>,
    /// Stands in for the short real waits between retries.
    pub sleep: Option<Box<dyn Fn(f64)>>,
    /// Called just before a record is put in place at this path.
    pub before_write: Option<PathHook>,
    /// Called before each copy of a pass, with the number of copies made so far.
    pub before_copy: Option<Box<dyn FnMut(usize)>>,
    /// Called when a plan is about to be made.
    pub before_plan: Option<Box<dyn FnMut()>>,
    /// Called by the daemon after each pass it runs.
    pub after_run: Option<Box<dyn FnMut()>>,
    /// Called whenever the daemon reports that it is alive.
    pub on_status: Option<Box<dyn FnMut()>>,
}

pub(crate) type Version = (i64, u64); // (mtime_ns, size)

/// What reading a record file gave, for the version of the file that was read.
pub(crate) type Reading = (Version, Option<Rc<Rec>>, Option<Why>);

/// True once a moment has passed: `delta` is the time still to go. (Also true when that
/// cannot be told, as after a clock change.)
pub(crate) fn past(delta: f64) -> bool {
    delta.is_nan() || delta < 0.0
}

pub struct Sync {
    pub p: Paths,
    /// Archived sessions are added only if used within this many days.
    pub days: f64,
    /// Also take a session out of the other accounts when it is deleted in one.
    pub delete_sync: bool,
    pub clock: Clock,
    pub out: Box<dyn FnMut(&str)>,
    /// Is the desktop app running? None = look at the process list.
    pub app_running_fn: Option<Box<dyn Fn() -> bool>>,
    /// What is the app signed in to? None = follow its log.
    pub signed_in_fn: Option<Box<dyn Fn() -> Signed>>,
    /// Which folders may receive records? None = ask the log's history.
    pub homes_fn: Option<Box<dyn Fn() -> Homes>>,
    pub hooks: Hooks,
    pub max_record_bytes: u64,
    pub max_replaced_bytes: u64,

    pub(crate) applog: AppLog,
    pub(crate) app_seen: (f64, bool),
    pub(crate) homes_cache: Option<(f64, BTreeSet<String>)>,
    /// (folder signed in to but not settled yet, when to look again)
    pub(crate) home_check: Option<(String, f64)>,
    /// the resolve requests the last plan knew of
    pub(crate) plan_requests: BTreeSet<String>,
    /// items the last pass put off only because their source had just been saved
    pub settling: usize,
    /// resolve requests the last plan made use of
    pub(crate) used: BTreeSet<String>,
    /// fid -> since when its source has been too fresh to copy
    pub(crate) waiting: HashMap<String, f64>,
    pub(crate) meta: HashMap<PathBuf, Reading>,
    pub(crate) shas: HashMap<PathBuf, (Version, String)>,
    pub(crate) tr: (f64, Rc<HashSet<String>>),
    /// the scan of the last plan, for re-checking which folder is in use
    pub(crate) scan_cache: Option<Rc<Scan>>,
    pub(crate) hold: (f64, BTreeSet<String>),
    /// (account folders, sessions) at the last scan
    pub seen: (usize, usize),
    /// what the last plan left out, and why
    pub notes: Notes,
    pub last_error: Option<String>,
    pub aborted: bool,
    /// False in the daemon's routine passes: copies we already placed are refreshed at leisure.
    pub urgent: bool,
    /// normalised path -> [mtime_ns, size, lastActivityAt, sha, user key]: what this tool last wrote there
    pub(crate) manifest: BTreeMap<String, Value>,
    pub(crate) dirty: bool,
    /// leaf -> ids of the record files seen at the previous scan
    pub(crate) prev: Option<HashMap<String, HashSet<String>>>,
    /// (leaf, fid) -> when a record was first seen gone without a marker
    pub(crate) vanished: HashMap<(String, String), f64>,
    /// (leaf, fid) -> (failures, not before)
    pub(crate) retry: HashMap<(String, String), (u32, f64)>,
    /// fid -> when its copies were last refreshed
    pub(crate) pushed: HashMap<String, f64>,
    pub(crate) pushed_before: HashMap<String, f64>,
    pub(crate) waiting_before: HashMap<String, f64>,
    /// message -> when it was last logged
    pub(crate) said: HashMap<String, f64>,
    pub(crate) alive_saved: f64,
}

pub fn short(leaf: &str) -> String {
    let (a, o) = leaf.split_once('/').unwrap_or((leaf, ""));
    let cut = |s: &str| s.chars().take(8).collect::<String>();
    format!("{}/{}", cut(a), cut(o))
}

pub(crate) fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut s = String::with_capacity(64);
    for b in digest {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// A finite JSON number (booleans and everything else: None).
pub(crate) fn num(v: Option<&Value>) -> Option<f64> {
    match v {
        Some(Value::Number(n)) => n.as_f64().filter(|f| f.is_finite()),
        _ => None,
    }
}

/// A number for one of our own files: whole numbers are written without a fraction.
pub(crate) fn num_json(v: f64) -> Value {
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 9.0e15 {
        json!(v as i64)
    } else {
        json!(v)
    }
}

fn value_is(v: Option<&Value>, n: i128) -> bool {
    match v {
        Some(Value::Number(x)) => {
            if let Some(i) = x.as_i64() {
                i as i128 == n
            } else if let Some(u) = x.as_u64() {
                u as i128 == n
            } else {
                x.as_f64().is_some_and(|f| f == n as f64)
            }
        }
        _ => false,
    }
}

/// One line of JSON with the keys in the order given.
pub(crate) fn ordered_json(items: &[(&str, Value)]) -> String {
    let mut s = String::from("{");
    for (i, (k, v)) in items.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        s.push_str(&Value::String((*k).to_string()).to_string());
        s.push_str(": ");
        s.push_str(&v.to_string());
    }
    s.push('}');
    s
}

impl Sync {
    /// A `Sync` for these locations with the defaults: 30 days, deletions not carried over,
    /// the real clock, lines printed, and the app watched through its process and its log.
    pub fn new(p: Paths) -> Sync {
        let applog = AppLog::new(p.logs.clone());
        let mut s = Sync {
            p,
            days: 30.0,
            delete_sync: false,
            clock: Rc::new(winfs::real_now),
            out: Box::new(|line: &str| {
                let mut o = io::stdout();
                let _ = writeln!(o, "{}", line);
                let _ = o.flush();
            }),
            app_running_fn: None,
            signed_in_fn: None,
            homes_fn: None,
            hooks: Hooks::default(),
            max_record_bytes: MAX_RECORD_BYTES,
            max_replaced_bytes: MAX_REPLACED_BYTES,
            applog,
            app_seen: (0.0, true),
            homes_cache: None,
            home_check: None,
            plan_requests: BTreeSet::new(),
            settling: 0,
            used: BTreeSet::new(),
            waiting: HashMap::new(),
            meta: HashMap::new(),
            shas: HashMap::new(),
            tr: (0.0, Rc::new(HashSet::new())),
            scan_cache: None,
            hold: (0.0, BTreeSet::new()),
            seen: (0, 0),
            notes: Notes::default(),
            last_error: None,
            aborted: false,
            urgent: true,
            manifest: BTreeMap::new(),
            dirty: false,
            prev: None,
            vanished: HashMap::new(),
            retry: HashMap::new(),
            pushed: HashMap::new(),
            pushed_before: HashMap::new(),
            waiting_before: HashMap::new(),
            said: HashMap::new(),
            alive_saved: 0.0,
        };
        s.manifest = s.load_manifest();
        s
    }

    pub fn now(&self) -> f64 {
        (self.clock)()
    }

    // ------------------------------------------------------------------ small files of our own
    pub fn home(&self, name: &str) -> PathBuf {
        self.p.home.join(name)
    }

    /// Read a file with full sharing (see `winfs::open_shared`).
    pub(crate) fn read_shared(&self, path: &Path) -> io::Result<Vec<u8>> {
        if let Some(fault) = &self.hooks.read_fault {
            if fault(path) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "sharing violation",
                ));
            }
        }
        winfs::read_shared(path)
    }

    /// List a folder. An entry that cannot be read fails the whole listing.
    pub(crate) fn list_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        if let Some(fault) = &self.hooks.list_fault {
            if fault(path) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "cannot list",
                ));
            }
        }
        fs::read_dir(path)?.collect()
    }

    pub(crate) fn pause(&self, seconds: f64) {
        match &self.hooks.sleep {
            Some(f) => f(seconds),
            None if seconds > 0.0 && seconds.is_finite() => {
                std::thread::sleep(std::time::Duration::from_secs_f64(seconds))
            }
            None => {}
        }
    }

    pub fn log(&mut self, msg: &str) {
        self.log_opts(msg, true, 0.0);
    }

    /// Log a message at most once per `every` seconds.
    pub fn log_every(&mut self, msg: &str, every: f64) {
        self.log_opts(msg, true, every);
    }

    pub fn log_opts(&mut self, msg: &str, to_file: bool, every: f64) {
        if every != 0.0 {
            let now = self.now();
            if self
                .said
                .get(msg)
                .is_some_and(|last| (0.0..every).contains(&(now - last)))
            {
                return;
            }
            self.said.insert(msg.to_string(), now);
        }
        let line = format!(
            "{} {}",
            winfs::local_format(winfs::real_now(), "%Y-%m-%d %H:%M:%S"),
            msg
        );
        (self.out)(&line);
        if !to_file {
            return;
        }
        let _ = (|| -> io::Result<()> {
            fs::create_dir_all(&self.p.home)?;
            let path = self.home("sync.log");
            if fs::metadata(&path)
                .map(|m| m.len() > 2_000_000)
                .unwrap_or(false)
            {
                winfs::replace(&path, &self.home("sync.log.1"))?;
            }
            let mut fh = OpenOptions::new().append(true).create(true).open(&path)?;
            fh.write_all(format!("{}\n", line).as_bytes())
        })();
    }

    /// Put an entry on record. The time is added last.
    pub(crate) fn journal(&mut self, entry: &[(&str, Value)]) -> io::Result<()> {
        let mut items: Vec<(&str, Value)> = entry.to_vec();
        items.push(("t", json!(self.now())));
        let path = self.home("journal.jsonl");
        if fs::metadata(&path)
            .map(|m| m.len() > 5_000_000)
            .unwrap_or(false)
        {
            winfs::replace(&path, &self.home("journal.jsonl.1"))?;
        }
        let mut fh = OpenOptions::new().append(true).create(true).open(&path)?;
        fh.write_all(format!("{}\n", ordered_json(&items)).as_bytes())
    }

    // The list of our own writes is kept in manifest.json plus manifest.log, a line appended per
    // write, so that a kill in the middle of a pass loses nothing.
    fn load_manifest(&self) -> BTreeMap<String, Value> {
        let mut m = BTreeMap::new();
        if let Ok(data) = fs::read(self.home("manifest.json")) {
            if let Ok(Value::Object(loaded)) = serde_json::from_slice::<Value>(&data) {
                m = loaded.into_iter().collect();
            }
        }
        if let Ok(data) = fs::read(self.home("manifest.log")) {
            for line in data.split(|&b| b == b'\n') {
                let Ok(Value::Array(e)) = serde_json::from_slice::<Value>(line) else {
                    continue;
                };
                if e.len() != 2 {
                    continue;
                }
                let Value::String(key) = &e[0] else {
                    continue;
                };
                match &e[1] {
                    Value::Null => {
                        m.remove(key);
                    }
                    Value::Array(_) => {
                        m.insert(key.clone(), e[1].clone());
                    }
                    _ => {}
                }
            }
        }
        m
    }

    fn manifest_line(&mut self, key: &str, value: &Value) {
        self.dirty = true;
        let _ = (|| -> io::Result<()> {
            let mut fh = OpenOptions::new()
                .append(true)
                .create(true)
                .open(self.home("manifest.log"))?;
            fh.write_all(format!("{}\n", json!([key, value])).as_bytes())
        })();
    }

    pub(crate) fn save_manifest(&mut self) -> io::Result<()> {
        if self.dirty {
            let data = serde_json::to_vec(&self.manifest).map_err(io::Error::other)?;
            match winfs::atomic_write(&self.home("manifest.json"), &data, None, true, Expect::Any) {
                Ok(()) => {}
                Err(WriteError::Io(e)) => return Err(e),
                Err(WriteError::Changed) => return Err(io::Error::other("manifest.json changed")),
            }
            let _ = fs::File::create(self.home("manifest.log"));
            self.dirty = false;
        }
        Ok(())
    }

    /// The version `[mtime_ns, size, lastActivityAt, sha, user key]` this tool last wrote at
    /// `path`, if any. If the file has been changed since, this is the version the change was
    /// made on top of. (Entries written by older versions may be shorter.)
    pub fn base(&self, path: &Path) -> Option<Vec<Value>> {
        match self.manifest.get(&winfs::norm(path)) {
            Some(Value::Array(b)) if b.len() >= 2 => Some(b.clone()),
            _ => None,
        }
    }

    pub(crate) fn sha(&mut self, path: &Path, mtime_ns: i64, size: u64) -> Option<String> {
        if let Some((version, sha)) = self.shas.get(path) {
            if *version == (mtime_ns, size) {
                return Some(sha.clone());
            }
        }
        let data = self.read_shared(path).ok()?;
        let sha = sha256_hex(&data)[..32].to_string();
        self.shas
            .insert(path.to_path_buf(), ((mtime_ns, size), sha.clone()));
        Some(sha)
    }

    /// Is the file at `path` the version `b` (a manifest entry)? Same time and size, or the same bytes.
    pub(crate) fn is_version(
        &mut self,
        b: Option<&[Value]>,
        path: &Path,
        mtime_ns: i64,
        size: u64,
    ) -> bool {
        let Some(b) = b else {
            return false;
        };
        if value_is(b.first(), mtime_ns as i128) && value_is(b.get(1), size as i128) {
            return true;
        }
        let Some(Value::String(want)) = b.get(3) else {
            return false;
        };
        if want.is_empty() || !value_is(b.get(1), size as i128) {
            return false;
        }
        self.sha(path, mtime_ns, size).as_deref() == Some(want.as_str())
    }

    /// True if the file holds exactly what this tool last wrote there (so nothing unique is in
    /// it). The app saving the very same bytes again, as it does at sign-in, does not change that.
    pub fn is_ours(&mut self, path: &Path, st: &Stat) -> bool {
        let b = self.base(path);
        self.is_version(b.as_deref(), path, st.mtime_ns, st.size)
    }

    pub(crate) fn rec_is_ours(&mut self, r: &Rec) -> bool {
        let b = self.base(&r.path);
        self.is_version(b.as_deref(), &r.path, r.mtime, r.size)
    }

    /// A short stand-in for what the user decides about a session (archived or not, its
    /// title). Keys written by this program start with `r1:`; a key in any other form (the
    /// Python version's) is never equal to one of ours, which errs towards a conflict.
    pub(crate) fn user_key(user: &(bool, String)) -> String {
        let mut data = vec![if user.0 { b'1' } else { b'0' }, 0];
        data.extend_from_slice(user.1.as_bytes());
        format!("r1:{}", &sha256_hex(&data)[..16])
    }

    pub(crate) fn mark_ours(
        &mut self,
        path: &Path,
        mtime_ns: i64,
        size: u64,
        last: Option<f64>,
        sha: Option<&str>,
        user: Option<&(bool, String)>,
    ) {
        let key = winfs::norm(path);
        let value = json!([
            mtime_ns,
            size,
            last.map(num_json),
            sha,
            user.map(Self::user_key)
        ]);
        self.manifest.insert(key.clone(), value.clone());
        self.manifest_line(&key, &value);
    }

    pub(crate) fn forget(&mut self, path: &Path) {
        let key = winfs::norm(path);
        if self.manifest.remove(&key).is_some() {
            self.manifest_line(&key, &Value::Null);
        }
    }

    pub(crate) fn state(&self) -> Map<String, Value> {
        match fs::read(self.home("state.json"))
            .ok()
            .and_then(|d| serde_json::from_slice::<Value>(&d).ok())
        {
            Some(Value::Object(st)) => st,
            _ => Map::new(),
        }
    }

    pub(crate) fn write_state(&self, st: &Map<String, Value>) -> io::Result<()> {
        fs::create_dir_all(&self.p.home)?;
        let data = serde_json::to_vec(st).map_err(io::Error::other)?;
        match winfs::atomic_write(&self.home("state.json"), &data, None, true, Expect::Any) {
            Ok(()) => Ok(()),
            Err(WriteError::Io(e)) => Err(e),
            Err(WriteError::Changed) => Err(io::Error::other("state.json changed")),
        }
    }

    /// `(since_ms, blackouts)`: with `--delete-sync` a deletion is carried to the other accounts
    /// only if it was made at or after `since` and not inside a blackout (a stretch when this
    /// tool was not running).
    pub(crate) fn deletion_window(&self) -> (Option<f64>, Vec<(f64, f64)>) {
        let st = self.state();
        let Some(mut since) = num(st.get("propagate_since")) else {
            return (None, Vec::new());
        };
        let mut blackouts: Vec<(f64, f64)> = Vec::new();
        if let Some(Value::Array(list)) = st.get("blackouts") {
            for b in list {
                if let Value::Array(pair) = b {
                    if pair.len() == 2 {
                        if let (Some(a), Some(z)) = (num(pair.first()), num(pair.get(1))) {
                            blackouts.push((a, z));
                        }
                    }
                }
            }
        }
        let now_ms = self.now() * 1000.0;
        if let Some(alive) = num(st.get("alive_until")) {
            if now_ms - alive > DOWNTIME_GAP_SEC * 1000.0 {
                blackouts.push((alive, now_ms));
            }
        }
        // a stretch that starts at or before `since` just moves `since` forward; so does every
        // stretch that would not fit in the list, which keeps the answer right however many
        // nights the PC has slept
        blackouts.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
        while !blackouts.is_empty() && (blackouts[0].0 <= since || blackouts.len() > 50) {
            since = since.max(blackouts.remove(0).1);
        }
        (Some(since), blackouts)
    }

    /// Record that the tool is running now (and, on first use, from when deletions may be carried).
    pub fn touch_alive(&mut self, force: bool) -> io::Result<()> {
        let now = self.now();
        if !force && (0.0..300.0).contains(&(now - self.alive_saved)) {
            return Ok(());
        }
        let mut st = self.state();
        let (since, blackouts) = self.deletion_window();
        let since = match since {
            Some(s) => s,
            None => {
                let s = now * 1000.0;
                st.insert("installed_at".into(), json!(s as i64));
                s
            }
        };
        st.insert("propagate_since".into(), num_json(since));
        st.insert(
            "blackouts".into(),
            Value::Array(
                blackouts
                    .iter()
                    .map(|b| json!([num_json(b.0), num_json(b.1)]))
                    .collect(),
            ),
        );
        st.insert("alive_until".into(), json!((now * 1000.0) as i64));
        st.insert("version".into(), json!(VERSION));
        if self.homes_fn.is_none() {
            if let Some((_, found)) = &self.homes_cache {
                let mut all: BTreeSet<String> = found.clone();
                if let Some(Value::Array(known)) = st.get("homes") {
                    all.extend(known.iter().filter_map(|v| v.as_str().map(String::from)));
                }
                st.insert("homes".into(), json!(all));
            }
        }
        self.write_state(&st)?;
        self.alive_saved = now;
        Ok(())
    }

    pub fn app_running(&mut self) -> bool {
        if let Some(f) = &self.app_running_fn {
            return f();
        }
        let since = self.now() - self.app_seen.0;
        if !(0.0..2.0).contains(&since) {
            // looked at afresh: a stale "not running" is the dangerous answer
            let mut running = match &self.hooks.probe_app {
                Some(probe) => probe(),
                None => process::desktop_app_running(),
            };
            if !running {
                // the process test knows the app by where it is installed; should that ever
                // change, an app log that is still being written says it is running all the same
                if let Some(newest) = self.applog.current() {
                    let age = winfs::real_now() - winfs::ns_to_secs(newest.1);
                    running = -60.0 < age && age < 20.0; // (file clocks can run a little ahead)
                }
            }
            self.app_seen = (self.now(), running);
        }
        self.app_seen.1
    }

    pub fn signed_in(&mut self) -> Signed {
        match &self.signed_in_fn {
            Some(f) => f(),
            None => self.applog.state(),
        }
    }

    /// `(running, signed-in state)`: when this changes, a pass is due at once.
    pub fn app_state(&mut self) -> (bool, Option<Signed>) {
        let running = self.app_running();
        (
            running,
            if running {
                Some(self.signed_in())
            } else {
                None
            },
        )
    }

    /// The folders records may be added to: those the app has really settled in, by its log
    /// (plus any remembered from earlier logs).
    pub fn homes(&mut self) -> Homes {
        if let Some(f) = &self.homes_fn {
            return f();
        }
        let now = self.now();
        let state = self.signed_in();
        let age = self.homes_cache.as_ref().map(|h| now - h.0);
        let unknown_home = |cache: &Option<(f64, BTreeSet<String>)>| match (&state, cache) {
            (Signed::In(leaf), Some((_, known))) => !known.contains(leaf),
            (Signed::In(_), None) => true,
            _ => false,
        };
        // look again every ten minutes; every 20 s while the app is signed in to a folder we do not
        // know yet (a first sign-in to a new account), whatever was concluded before that sign-in
        let stale = match age {
            None => true,
            Some(a) => !(0.0..600.0).contains(&a) || (unknown_home(&self.homes_cache) && a >= 20.0),
        };
        if stale {
            let found = self.applog.homes(self.homes_cache.is_none());
            let mut known: BTreeSet<String> = BTreeSet::new();
            if let Some(Value::Array(list)) = self.state().get("homes") {
                known.extend(list.iter().filter_map(|v| v.as_str().map(String::from)));
            }
            if let Some((_, had)) = &self.homes_cache {
                known.extend(had.iter().cloned());
            }
            known.extend(found.unwrap_or_default());
            self.homes_cache = Some((now, known));
        }
        if unknown_home(&self.homes_cache) {
            // signed in to a folder that is not settled yet: have a pass run once it is, even if
            // nothing changes on disk
            if let Signed::In(leaf) = &state {
                if self.home_check.as_ref().is_none_or(|c| &c.0 != leaf) {
                    self.home_check = Some((leaf.clone(), now + HOME_SETTLE_SEC + 25.0));
                }
            }
        } else {
            self.home_check = None;
        }
        Homes::Only(
            self.homes_cache
                .as_ref()
                .map(|h| h.1.clone())
                .unwrap_or_default(),
        )
    }
}
