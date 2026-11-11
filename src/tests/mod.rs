//! Tests. Every test runs against a throwaway tree with explicit paths; the real profile is
//! never touched and no test reads the process environment for a location.
//!
//! The modules `adds` ... `daemon` are the behaviour contract carried over from the Python
//! version's test_sync.py, test for test and under the same names. The rest covers what is
//! new here.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{json, Map, Value};

use crate::paths::{Env, LogDirs, Paths};
use crate::sync::{Homes, Signed, Sync};
use crate::winfs;

mod adds;
mod args;
mod bad_input;
mod compat;
mod daemon;
mod deletions;
mod homes;
mod last_moments;
mod locations;
mod newest;
mod pace;
mod safety;
mod setup;
mod the_apps_log;
mod while_the_app_runs;

pub type Leaf = (&'static str, &'static str);

// all share the same first 8 characters on purpose
pub const A: Leaf = (
    "00000000-0000-4000-8000-0000000000a0",
    "00000000-0000-4000-8000-0000000000a1",
);
pub const B: Leaf = (
    "00000000-0000-4000-8000-0000000000b0",
    "00000000-0000-4000-8000-0000000000b1",
);
pub const C: Leaf = (
    "00000000-0000-4000-8000-0000000000c0",
    "00000000-0000-4000-8000-0000000000c1",
);
pub const HOUR: f64 = 3600.0;
pub const DAY: f64 = 86400.0;

pub fn uid(n: u32) -> String {
    format!("00000000-0000-4000-8000-{:012x}", n)
}

pub fn name(leaf: Leaf) -> String {
    format!("{}/{}", leaf.0, leaf.1)
}

pub fn la() -> String {
    name(A)
}

pub fn lb() -> String {
    name(B)
}

pub fn lc() -> String {
    name(C)
}

pub fn now() -> f64 {
    winfs::real_now()
}

/// Epoch milliseconds of a moment `seconds_ago`.
pub fn ms(seconds_ago: f64) -> i64 {
    ((now() - seconds_ago) * 1000.0) as i64
}

pub fn set_mtime(path: &Path, epoch: f64) {
    let ns = (epoch * 1e9) as i64;
    winfs::set_times(path, ns, ns).unwrap();
}

pub fn mtime_ns(path: &Path) -> i64 {
    winfs::stat(path).unwrap().mtime_ns
}

pub fn listdir(path: &Path) -> Vec<String> {
    let mut out: Vec<String> = fs::read_dir(path)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    out.sort();
    out
}

fn walk(top: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(top) else {
        return;
    };
    for e in entries.flatten() {
        if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            walk(&e.path(), out);
        } else {
            out.push(e.path());
        }
    }
}

/// How a test record is to look.
pub struct R {
    last_ago: f64,
    mtime_ago: f64,
    archived: bool,
    cli: Option<String>,
    transcript: bool,
    title: String,
    extra: Option<Value>,
    raw: Option<Vec<u8>>,
}

pub fn r() -> R {
    R {
        last_ago: HOUR,
        mtime_ago: HOUR,
        archived: false,
        cli: None,
        transcript: true,
        title: String::from("sesión"),
        extra: None,
        raw: None,
    }
}

impl R {
    pub fn last(mut self, seconds_ago: f64) -> R {
        self.last_ago = seconds_ago;
        self
    }
    pub fn mtime(mut self, seconds_ago: f64) -> R {
        self.mtime_ago = seconds_ago;
        self
    }
    pub fn archived(mut self) -> R {
        self.archived = true;
        self
    }
    pub fn cli(mut self, cli: &str) -> R {
        self.cli = Some(cli.to_string());
        self
    }
    pub fn no_transcript(mut self) -> R {
        self.transcript = false;
        self
    }
    pub fn title(mut self, title: &str) -> R {
        self.title = title.to_string();
        self
    }
    pub fn extra(mut self, extra: Value) -> R {
        self.extra = Some(extra);
        self
    }
    pub fn raw(mut self, raw: &[u8]) -> R {
        self.raw = Some(raw.to_vec());
        self
    }
}

pub struct Tree {
    pub tmp: PathBuf,
    pub root: PathBuf,
    pub proj: PathBuf,
    pub home: PathBuf,
    pub applog: PathBuf,
    pub lines: Rc<RefCell<Vec<String>>>,
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.tmp);
    }
}

/// A throwaway profile: three account folders, a projects folder, nothing else.
#[derive(Clone)]
pub struct Base(Rc<Tree>);

impl Deref for Base {
    type Target = Tree;
    fn deref(&self) -> &Tree {
        &self.0
    }
}

static COUNTER: AtomicUsize = AtomicUsize::new(0);

pub fn temp_dir(prefix: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "{}-{}-{}-{}",
        prefix,
        std::process::id(),
        n,
        (now() * 1e6) as u64
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

impl Base {
    pub fn new() -> Base {
        let tmp = temp_dir("css-test");
        let tree = Tree {
            root: tmp
                .join("appdata")
                .join("Claude")
                .join("claude-code-sessions"),
            proj: tmp
                .join("user")
                .join(".claude")
                .join("projects")
                .join("proj"),
            home: tmp.join("user").join(".claude-session-sync"),
            applog: tmp
                .join("local")
                .join("Claude")
                .join("logs")
                .join("main.log"),
            lines: Rc::new(RefCell::new(Vec::new())),
            tmp,
        };
        fs::create_dir_all(&tree.proj).unwrap();
        for leaf in [A, B, C] {
            fs::create_dir_all(tree.root.join(leaf.0).join(leaf.1)).unwrap();
        }
        Base(Rc::new(tree))
    }

    /// The environment that leads to this tree by the defaults.
    pub fn env(&self) -> Env {
        Env {
            appdata: Some(self.tmp.join("appdata")),
            localappdata: Some(self.tmp.join("local")),
            userprofile: Some(self.tmp.join("user")),
            userdomain: Some(String::from("PC")),
            username: Some(String::from("someone")),
            ..Env::default()
        }
    }

    pub fn log_dirs(&self) -> LogDirs {
        LogDirs {
            local: Some(self.tmp.join("local")),
            roaming: Some(self.tmp.join("appdata")),
        }
    }

    pub fn paths(&self) -> Paths {
        Paths {
            root: self.root.clone(),
            projects: self.tmp.join("user").join(".claude").join("projects"),
            home: self.home.clone(),
            logs: self.log_dirs(),
        }
    }

    /// A `Sync` with the tool's own defaults, its lines kept in `lines`.
    pub fn plain(&self) -> Sync {
        let mut s = Sync::new(self.paths());
        let lines = self.lines.clone();
        s.out = Box::new(move |l: &str| lines.borrow_mut().push(l.to_string()));
        s
    }

    /// A `Sync` for the test tree. Deletions are carried over here unless a test says otherwise
    /// (the tool's own default is off), and every test folder is one the app really uses.
    pub fn s(&self) -> Sync {
        self.s_app(false, Signed::Unknown)
    }

    pub fn s_app(&self, app: bool, signed: Signed) -> Sync {
        let mut s = self.plain();
        s.delete_sync = true;
        s.homes_fn = Some(Box::new(|| Homes::All));
        s.app_running_fn = Some(Box::new(move || app));
        s.signed_in_fn = Some(Box::new(move || signed.clone()));
        s
    }

    pub fn s_homes(&self, homes: &[String]) -> Sync {
        let mut s = self.s();
        let homes = Homes::only(homes.iter().cloned());
        s.homes_fn = Some(Box::new(move || homes.clone()));
        s
    }

    pub fn write_log(&self, lines: &[&str]) {
        self.write_log_mode(lines, false);
    }

    pub fn write_log_mode(&self, lines: &[&str], truncate: bool) {
        use std::io::Write;
        fs::create_dir_all(self.applog.parent().unwrap()).unwrap();
        let mut fh = fs::OpenOptions::new()
            .create(true)
            .append(!truncate)
            .write(true)
            .truncate(truncate)
            .open(&self.applog)
            .unwrap();
        for line in lines {
            fh.write_all(line.as_bytes()).unwrap();
        }
    }

    pub fn login(leaf: Leaf, n: u32) -> String {
        format!(
            "2026-11-11 09:07:09 [info] [LocalSessionManager] Initialization succeeded — accountId={}, orgId={}, existingSessions={}\r\n",
            leaf.0, leaf.1, n
        )
    }

    pub const LOGOUT: &'static str =
        "2026-11-11 09:05:45 [info] [LocalSessionManager] Account logged out, marking for re-init on next login\r\n";

    pub fn dir(&self, leaf: Leaf) -> PathBuf {
        self.root.join(leaf.0).join(leaf.1)
    }

    pub fn path(&self, leaf: Leaf, fid: &str) -> PathBuf {
        self.dir(leaf).join(format!("local_{}.json", fid))
    }

    pub fn rec(&self, leaf: Leaf, fid: &str, r: R) -> PathBuf {
        let cli = r.cli.clone().unwrap_or_else(|| format!("cli-{}", fid));
        let mut j = Map::new();
        j.insert("sessionId".into(), json!(format!("local_{}", fid)));
        j.insert("cliSessionId".into(), json!(cli));
        j.insert("title".into(), json!(r.title));
        j.insert("isArchived".into(), json!(r.archived));
        j.insert("createdAt".into(), json!(ms(90.0 * DAY)));
        j.insert("lastActivityAt".into(), json!(ms(r.last_ago)));
        if let Some(Value::Object(extra)) = r.extra {
            for (k, v) in extra {
                j.insert(k, v);
            }
        }
        let p = self.path(leaf, fid);
        let data = match r.raw {
            Some(raw) => raw,
            None => serde_json::to_vec(&Value::Object(j)).unwrap(),
        };
        fs::write(&p, data).unwrap();
        set_mtime(&p, now() - r.mtime_ago);
        if r.transcript {
            fs::write(self.proj.join(format!("{}.jsonl", cli)), b"").unwrap();
        }
        p
    }

    pub fn tomb(&self, leaf: Leaf, ident: &str, ts: i64) {
        let p = self.dir(leaf).join(format!("deleted_{}", ident));
        fs::write(&p, ts.to_string()).unwrap();
        set_mtime(&p, ts as f64 / 1000.0);
    }

    /// What the app does when the user deletes a session: remove the record, write its markers.
    pub fn delete(&self, leaf: Leaf, fid: &str, ago: f64, cli: Option<&str>) {
        if self.has(leaf, fid) {
            fs::remove_file(self.path(leaf, fid)).unwrap();
        }
        let cli = cli
            .map(String::from)
            .unwrap_or_else(|| format!("cli-{}", fid));
        for ident in [fid, cli.as_str()] {
            self.tomb(leaf, ident, ms(ago));
        }
    }

    pub fn install(&self, seconds_ago: f64, alive_ago: f64) {
        fs::create_dir_all(&self.home).unwrap();
        let st = json!({"installed_at": ms(seconds_ago), "propagate_since": ms(seconds_ago), "alive_until": ms(alive_ago)});
        fs::write(self.home.join("state.json"), st.to_string()).unwrap();
    }

    pub fn state(&self) -> Value {
        serde_json::from_slice(&fs::read(self.home.join("state.json")).unwrap()).unwrap()
    }

    pub fn data(&self, leaf: Leaf, fid: &str) -> Vec<u8> {
        fs::read(self.path(leaf, fid)).unwrap()
    }

    pub fn title(&self, leaf: Leaf, fid: &str) -> String {
        let j: Value = serde_json::from_slice(&self.data(leaf, fid)).unwrap();
        j["title"].as_str().unwrap().to_string()
    }

    pub fn last_activity(&self, leaf: Leaf, fid: &str) -> Value {
        let j: Value = serde_json::from_slice(&self.data(leaf, fid)).unwrap();
        j["lastActivityAt"].clone()
    }

    pub fn has(&self, leaf: Leaf, fid: &str) -> bool {
        self.path(leaf, fid).exists()
    }

    /// (mtime_ns, size) of every file below `top`.
    pub fn snapshot(&self, top: &Path) -> BTreeMap<PathBuf, (i64, u64)> {
        let mut files = Vec::new();
        walk(top, &mut files);
        files
            .into_iter()
            .map(|p| {
                let st = winfs::stat(&p).unwrap();
                (p, (st.mtime_ns, st.size))
            })
            .collect()
    }

    /// The bytes of every file in the store.
    pub fn contents(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut files = Vec::new();
        walk(&self.root, &mut files);
        files
            .into_iter()
            .map(|p| {
                (
                    p.strip_prefix(&self.root).unwrap().to_path_buf(),
                    fs::read(&p).unwrap(),
                )
            })
            .collect()
    }

    /// The versions kept under replaced\.
    pub fn kept(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        walk(&self.home.join("replaced"), &mut files);
        files
    }

    pub fn said(&self, what: &str) -> usize {
        self.lines
            .borrow()
            .iter()
            .filter(|l| l.contains(what))
            .count()
    }
}

/// The system's tools as on a PC where nothing is installed; nothing real is asked or changed.
pub struct NoSystem;

impl crate::install::System for NoSystem {
    fn run_output(&mut self, _program: &str, _args: &[String]) -> std::io::Result<(i32, String)> {
        Ok((1, String::new()))
    }
    fn spawn_detached(&mut self, _program: &Path, _args: &[String]) -> std::io::Result<()> {
        Err(std::io::Error::other("not in a test"))
    }
    fn sleep(&mut self, _seconds: f64) {}
    fn is_ours(&mut self, _pid: u32, _started: Option<f64>) -> bool {
        false
    }
    fn terminate(&mut self, _pid: u32) -> bool {
        false
    }
    fn user_path(&mut self) -> std::io::Result<Option<crate::winreg::Text>> {
        Ok(None)
    }
    fn set_user_path(&mut self, _path: &crate::winreg::Text) -> std::io::Result<()> {
        Err(std::io::Error::other("not in a test"))
    }
    fn environment_changed(&mut self) {}
    fn sandboxed(&mut self, _localappdata: Option<&Path>) -> bool {
        false
    }
}

/// Run a command line against a test tree. Its exit code and what it printed.
pub fn command_line(b: &Base, args: &[&str]) -> (i32, String) {
    let (con, shown) = crate::cli::Console::capture();
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let code = crate::cli::run_with(&args, &b.env(), &con, &mut NoSystem);
    let text = String::from_utf8_lossy(&shown.borrow()).into_owned();
    (code, text)
}

/// A clock a test can move.
#[derive(Clone)]
pub struct FakeClock(Rc<std::cell::Cell<f64>>);

impl FakeClock {
    /// Give `s` a clock that starts now and only moves when told.
    pub fn on(s: &mut Sync) -> FakeClock {
        Self::at(s, now())
    }

    pub fn at(s: &mut Sync, t: f64) -> FakeClock {
        let c = FakeClock(Rc::new(std::cell::Cell::new(t)));
        let inner = c.0.clone();
        s.clock = Rc::new(move || inner.get());
        c
    }

    pub fn t(&self) -> f64 {
        self.0.get()
    }

    pub fn add(&self, d: f64) {
        self.0.set(self.0.get() + d);
    }
}
