//! Where things are on this PC. Nothing here is fixed: other people's PCs differ.
//!
//! This tool's own folder is under `%USERPROFILE%`, not under `%LOCALAPPDATA%`: for a process
//! started inside the Claude desktop app, Windows redirects what is written under AppData to
//! a private copy inside the app's package, where the daemon (started outside) never looks.

use std::fs;
use std::path::{Path, PathBuf};

use crate::winfs;

/// The environment variables this program looks at. `from_process` reads the real ones;
/// everything else in the library takes an `Env` (or the `Paths` made from it) explicitly.
#[derive(Clone, Debug, Default)]
pub struct Env {
    pub appdata: Option<PathBuf>,
    pub localappdata: Option<PathBuf>,
    pub userprofile: Option<PathBuf>,
    /// `CLAUDE_CONFIG_DIR`: where Claude Code keeps `projects\` when it is not `~\.claude`.
    pub claude_config_dir: Option<PathBuf>,
    /// `CLAUDE_SESSION_SYNC_HOME`: this tool's state directory.
    pub home_override: Option<PathBuf>,
    /// `CLAUDE_SESSION_SYNC_STORE`: the app's `claude-code-sessions` folder.
    pub store_override: Option<PathBuf>,
    pub userdomain: Option<String>,
    pub username: Option<String>,
}

impl Env {
    pub fn from_process() -> Env {
        fn var(name: &str) -> Option<String> {
            std::env::var_os(name)
                .map(|v| v.to_string_lossy().into_owned())
                .filter(|v| !v.is_empty())
        }
        Env {
            appdata: var("APPDATA").map(PathBuf::from),
            localappdata: var("LOCALAPPDATA").map(PathBuf::from),
            userprofile: var("USERPROFILE").map(PathBuf::from),
            claude_config_dir: var("CLAUDE_CONFIG_DIR").map(PathBuf::from),
            home_override: var("CLAUDE_SESSION_SYNC_HOME").map(PathBuf::from),
            store_override: var("CLAUDE_SESSION_SYNC_STORE").map(PathBuf::from),
            userdomain: var("USERDOMAIN"),
            username: var("USERNAME"),
        }
    }
}

/// Where the desktop app may keep its log.
#[derive(Clone, Debug, Default)]
pub struct LogDirs {
    /// `%LOCALAPPDATA%`
    pub local: Option<PathBuf>,
    /// `%APPDATA%`
    pub roaming: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct Paths {
    /// The app's `claude-code-sessions` folder: `<account>\<org>\local_<id>.json` below it.
    pub root: PathBuf,
    /// Claude Code's `projects` folder, where the transcripts are.
    pub projects: PathBuf,
    /// This tool's own state directory.
    pub home: PathBuf,
    pub logs: LogDirs,
}

/// The `Claude_*` package folders under `%LOCALAPPDATA%\Packages` (a Store install).
fn packages(local: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(local.join("Packages")) {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().starts_with("Claude_") {
                out.push(e.path());
            }
        }
    }
    out.sort();
    out
}

/// Every place the app may keep its session store, the classic one first.
pub fn store_candidates(env: &Env) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(appdata) = &env.appdata {
        out.push(appdata.join("Claude").join("claude-code-sessions"));
    }
    if let Some(local) = &env.localappdata {
        for pkg in packages(local) {
            out.push(
                pkg.join("LocalCache")
                    .join("Roaming")
                    .join("Claude")
                    .join("claude-code-sessions"),
            );
        }
    }
    out
}

/// The time of the most recently modified record or deletion marker in a store (0 if none).
fn newest_record(store: &Path) -> i64 {
    let mut newest = 0;
    let Ok(accounts) = fs::read_dir(store) else {
        return newest;
    };
    for a in accounts.flatten() {
        let Ok(orgs) = fs::read_dir(a.path()) else {
            continue;
        };
        for o in orgs.flatten() {
            let Ok(files) = fs::read_dir(o.path()) else {
                continue;
            };
            for f in files.flatten() {
                let name = f.file_name().to_string_lossy().into_owned();
                if name.starts_with("local_") || name.starts_with("deleted_") {
                    if let Ok(st) = winfs::stat_entry(&f) {
                        newest = newest.max(st.mtime_ns);
                    }
                }
            }
        }
    }
    newest
}

/// The store to use: the candidate that exists; of several, the one whose records were
/// modified most recently; if none exists yet, the first (classic) location.
pub fn choose_store(candidates: &[PathBuf]) -> Option<PathBuf> {
    let existing: Vec<&PathBuf> = candidates.iter().filter(|c| c.is_dir()).collect();
    match existing.len() {
        0 => candidates.first().cloned(),
        1 => Some(existing[0].clone()),
        _ => {
            let mut best = (existing[0], newest_record(existing[0]));
            for c in &existing[1..] {
                let t = newest_record(c);
                if t > best.1 {
                    best = (c, t);
                }
            }
            Some(best.0.clone())
        }
    }
}

/// This tool's own folder when nothing else is said: `%USERPROFILE%\.claude-session-sync`.
/// The installed programs are in its `bin` folder, wherever the state is kept.
pub fn default_home(userprofile: &Path) -> PathBuf {
    userprofile.join(".claude-session-sync")
}

impl Paths {
    /// Work out the locations from the environment and what is on disk. Nothing is created.
    pub fn discover(env: &Env) -> Result<Paths, String> {
        let root = match &env.store_override {
            Some(p) => p.clone(),
            None => choose_store(&store_candidates(env)).ok_or_else(|| {
                String::from("APPDATA is not set, and CLAUDE_SESSION_SYNC_STORE is not given")
            })?,
        };
        let home = match (&env.home_override, &env.userprofile) {
            (Some(p), _) => p.clone(),
            (None, Some(user)) => default_home(user),
            (None, None) => {
                return Err(String::from(
                    "USERPROFILE is not set, and CLAUDE_SESSION_SYNC_HOME is not given",
                ))
            }
        };
        let projects = match (&env.claude_config_dir, &env.userprofile) {
            (Some(p), _) => p.join("projects"),
            (None, Some(user)) => user.join(".claude").join("projects"),
            (None, None) => {
                return Err(String::from(
                    "USERPROFILE is not set, and CLAUDE_CONFIG_DIR is not given",
                ))
            }
        };
        Ok(Paths {
            root,
            projects,
            home,
            logs: LogDirs {
                local: env.localappdata.clone(),
                roaming: env.appdata.clone(),
            },
        })
    }

    pub fn home_file(&self, name: &str) -> PathBuf {
        self.home.join(name)
    }
}

impl LogDirs {
    /// Every place the app's `main.log` may be.
    pub fn candidates(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Some(local) = &self.local {
            out.push(local.join("Claude").join("logs").join("main.log"));
            for pkg in packages(local) {
                out.push(
                    pkg.join("LocalCache")
                        .join("Local")
                        .join("Claude")
                        .join("logs")
                        .join("main.log"),
                );
            }
        }
        if let Some(roaming) = &self.roaming {
            out.push(roaming.join("Claude").join("logs").join("main.log"));
        }
        out
    }
}
