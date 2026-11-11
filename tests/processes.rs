//! Tests that need a second real process: the built programs are run against a throwaway
//! tree, with every location variable pointed into it.

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use claude_session_sync::consts::STALL_SEC;
use claude_session_sync::winfs;
use claude_session_sync::{acquire_lock, lock_for_daemon, release_lock, LogDirs, Paths, Sync};

const CONSOLE: &str = env!("CARGO_BIN_EXE_claude-session-sync");
const WINDOWLESS: &str = env!("CARGO_BIN_EXE_claude-session-syncw");

type Leaf = (&'static str, &'static str);
const A: Leaf = (
    "00000000-0000-4000-8000-0000000000a0",
    "00000000-0000-4000-8000-0000000000a1",
);
const B: Leaf = (
    "00000000-0000-4000-8000-0000000000b0",
    "00000000-0000-4000-8000-0000000000b1",
);

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct Tree {
    tmp: PathBuf,
    root: PathBuf,
    home: PathBuf,
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.tmp);
    }
}

/// Ends the process when the test is over, however it ends.
struct Guard(Child);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Guard {
    fn exited(&mut self, within: Duration) -> Option<i32> {
        let until = Instant::now() + within;
        loop {
            if let Ok(Some(status)) = self.0.try_wait() {
                return Some(status.code().unwrap_or(-1));
            }
            if Instant::now() >= until {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn wait_for(what: &str, within: Duration, mut done: impl FnMut() -> bool) {
    let until = Instant::now() + within;
    while !done() {
        assert!(Instant::now() < until, "timed out waiting for {}", what);
        std::thread::sleep(Duration::from_millis(100));
    }
}

impl Tree {
    fn new() -> Tree {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let tmp = std::env::temp_dir().join(format!(
            "css-proc-{}-{}-{}",
            std::process::id(),
            n,
            (winfs::real_now() * 1e6) as u64
        ));
        let tree = Tree {
            root: tmp
                .join("appdata")
                .join("Claude")
                .join("claude-code-sessions"),
            home: tmp.join("state"),
            tmp,
        };
        for leaf in [A, B] {
            fs::create_dir_all(tree.root.join(leaf.0).join(leaf.1)).unwrap();
        }
        fs::create_dir_all(
            tree.tmp
                .join("user")
                .join(".claude")
                .join("projects")
                .join("proj"),
        )
        .unwrap();
        tree
    }

    fn paths(&self) -> Paths {
        Paths {
            root: self.root.clone(),
            projects: self.tmp.join("user").join(".claude").join("projects"),
            home: self.home.clone(),
            logs: LogDirs {
                local: Some(self.tmp.join("local")),
                roaming: Some(self.tmp.join("appdata")),
            },
        }
    }

    /// One of the built programs, with every location pointed into this tree.
    fn program(&self, exe: &str, args: &[&str]) -> Command {
        let mut c = Command::new(exe);
        c.args(args)
            .env("APPDATA", self.tmp.join("appdata"))
            .env("LOCALAPPDATA", self.tmp.join("local"))
            .env("USERPROFILE", self.tmp.join("user"))
            .env("CLAUDE_SESSION_SYNC_HOME", &self.home)
            .env("CLAUDE_SESSION_SYNC_STORE", &self.root)
            .env_remove("CLAUDE_CONFIG_DIR")
            .stdin(Stdio::null());
        c
    }

    fn run(&self, exe: &str, args: &[&str]) -> (i32, String) {
        let out = self.program(exe, args).output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    }

    fn path(&self, leaf: Leaf, fid: &str) -> PathBuf {
        self.root
            .join(leaf.0)
            .join(leaf.1)
            .join(format!("local_{}.json", fid))
    }

    /// A session record saved an hour ago, with its transcript.
    fn rec(&self, leaf: Leaf, fid: &str) {
        let last = ((winfs::real_now() - 3600.0) * 1000.0) as i64;
        let j = format!(
            "{{\"sessionId\":\"local_{0}\",\"cliSessionId\":\"cli-{0}\",\"title\":\"a session\",\"isArchived\":false,\"lastActivityAt\":{1}}}",
            fid, last
        );
        let p = self.path(leaf, fid);
        fs::write(&p, j).unwrap();
        let t = ((winfs::real_now() - 3600.0) * 1e9) as i64;
        winfs::set_times(&p, t, t).unwrap();
        fs::write(
            self.tmp
                .join("user")
                .join(".claude")
                .join("projects")
                .join("proj")
                .join(format!("cli-{}.jsonl", fid)),
            b"",
        )
        .unwrap();
    }

    fn status_pid(&self) -> Option<u64> {
        let data = fs::read(self.home.join("status.json")).ok()?;
        serde_json::from_slice::<serde_json::Value>(&data)
            .ok()?
            .get("pid")?
            .as_u64()
    }

    /// When the daemon on record says it started.
    fn status_started(&self) -> f64 {
        let data = fs::read(self.home.join("status.json")).unwrap();
        serde_json::from_slice::<serde_json::Value>(&data).unwrap()["started"]
            .as_f64()
            .unwrap()
    }

    fn write_status(&self, pid: u32, heartbeat_ago: f64, started: Option<f64>) {
        let mut st = format!(
            "{{\"pid\": {}, \"heartbeat\": {}",
            pid,
            winfs::real_now() - heartbeat_ago
        );
        if let Some(started) = started {
            st.push_str(&format!(", \"started\": {}", started as i64));
        }
        fs::write(self.home.join("status.json"), st + "}").unwrap();
    }

    /// A daemon of ours that has come up and holds the lock.
    fn daemon(&self, exe: &str, args: &[&str]) -> Guard {
        let mut c = self.program(exe, args);
        let daemon = Guard(
            c.stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let pid = daemon.0.id() as u64;
        wait_for("the daemon to come up", Duration::from_secs(30), || {
            self.status_pid() == Some(pid)
        });
        assert!(acquire_lock(&self.home).unwrap().is_none());
        daemon
    }
}

fn file_names(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    out.sort();
    out
}

mod daemon {
    use super::*;

    #[test]
    fn test_lock_is_honoured_between_real_processes() {
        let t = Tree::new();
        t.rec(A, "s1");
        t.rec(B, "own"); // so that both folders are ones the app uses
        fs::create_dir_all(&t.home).unwrap();
        let held = acquire_lock(&t.home).unwrap().unwrap();
        // the other process could not take it
        let (code, out) = t.run(CONSOLE, &["sync"]);
        assert_eq!(code, 0);
        assert!(out.contains("the daemon is running"), "{}", out);
        assert!(!t.path(B, "s1").exists());
        let (code, _) = t.run(CONSOLE, &["status"]);
        assert_eq!(code, 0); // to the other process the lock means: a daemon is running
        release_lock(held);
        let (code, out) = t.run(CONSOLE, &["sync"]);
        assert_eq!(code, 0);
        assert!(out.contains("one-off: added 2"), "{}", out);
        assert!(t.path(B, "s1").exists());
        // ...and the other way round: while that process holds it, this one cannot
        let mut daemon = t.daemon(CONSOLE, &["daemon", "--interval", "3600", "--quick", "0"]);
        assert!(acquire_lock(&t.home).unwrap().is_none());
        let (code, out) = t.run(CONSOLE, &["stop"]);
        assert_eq!((code, out.trim()), (0, "stopped"));
        assert_eq!(daemon.exited(Duration::from_secs(10)), Some(0));
        assert!(acquire_lock(&t.home).unwrap().is_some());
    }

    #[test]
    fn test_stalled_daemon_is_replaced_but_a_live_one_is_left_alone() {
        let t = Tree::new();
        let lines: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let sync = || {
            let mut s = Sync::new(t.paths());
            let sink = lines.clone();
            s.out = Box::new(move |l: &str| sink.borrow_mut().push(l.to_string()));
            s
        };
        let quick_sleep = |s: f64| std::thread::sleep(Duration::from_secs_f64(s.min(0.05)));
        let mut other = t.daemon(CONSOLE, &["daemon", "--interval", "3600", "--quick", "0"]);
        let pid = other.0.id();
        let started = t.status_started(); // what the daemon itself reported
                                          // something that is alive but not one of ours
        let mut bystander = Guard(
            Command::new("cmd")
                .args(["/c", "pause"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let bystander_pid = bystander.0.id();
        let mut left_alone = |why: &str| {
            assert!(
                lock_for_daemon(&mut sync(), &quick_sleep)
                    .unwrap()
                    .is_none(),
                "{}",
                why
            );
            assert_eq!(other.exited(Duration::ZERO), None, "{}", why);
            assert_eq!(bystander.exited(Duration::ZERO), None, "{}", why);
        };

        t.write_status(pid, 5.0, Some(started));
        left_alone("it gave a sign of life 5 s ago");

        t.write_status(bystander_pid, 2.0 * STALL_SEC, Some(started));
        left_alone("stale, but that pid is not a daemon of ours");

        // the pid on record is our daemon's, but the report is of a daemon that started an hour
        // before this process existed: the pid has passed to another process since
        t.write_status(pid, 2.0 * STALL_SEC, Some(started - 3600.0));
        left_alone("stale, but the process is younger than the daemon that reported");

        t.write_status(pid, 2.0 * STALL_SEC, None);
        left_alone("stale, but with no start time to tell the process by");
        assert!(!lines.borrow().iter().any(|l| l.contains("taking over")));

        t.write_status(pid, 2.0 * STALL_SEC, Some(started));
        let lock = lock_for_daemon(&mut sync(), &quick_sleep).unwrap(); // silent for half an hour: taken over
        assert!(lock.is_some());
        release_lock(lock.unwrap());
        assert!(other.exited(Duration::from_secs(10)).is_some());
        assert!(lines.borrow().iter().any(|l| l.contains("taking over")));
    }

    /// Only this tool's own programs are ever ended: a stalled process called python.exe is
    /// not, even when everything else about it fits.
    #[test]
    fn stalled_process_of_another_name_is_never_ended() {
        let t = Tree::new();
        let lines: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = lines.clone();
        let mut s = Sync::new(t.paths());
        s.out = Box::new(move |l: &str| sink.borrow_mut().push(l.to_string()));
        // our own program under another name stands in for the Python version's daemon
        let elsewhere = t.tmp.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        let python = elsewhere.join("python.exe");
        fs::copy(CONSOLE, &python).unwrap();
        let mut other = t.daemon(
            &python.to_string_lossy(),
            &["daemon", "--interval", "3600", "--quick", "0"],
        );
        let started = t.status_started();
        t.write_status(other.0.id(), 2.0 * STALL_SEC, Some(started));
        let quick_sleep = |s: f64| std::thread::sleep(Duration::from_secs_f64(s.min(0.05)));
        assert!(lock_for_daemon(&mut s, &quick_sleep).unwrap().is_none());
        assert_eq!(other.exited(Duration::ZERO), None);
        assert!(lines.borrow().is_empty());
    }

    /// The whole thing through the programs as they are installed: the windowless daemon
    /// mirrors a new record within seconds, a second daemon leaves by itself, `status` says
    /// it is running, `stop` ends it.
    #[test]
    fn windowless_daemon_end_to_end() {
        let t = Tree::new();
        t.rec(A, "first");
        t.rec(B, "own");
        // a preview writes nothing
        let before = file_names(&t.tmp);
        let (code, out) = t.run(CONSOLE, &["preview", "--verbose"]);
        assert_eq!(code, 0);
        assert!(out.contains("account folders: 2"), "{}", out);
        assert_eq!(before, file_names(&t.tmp));
        assert!(!t.home.exists());

        let mut daemon = t.daemon(WINDOWLESS, &["daemon", "--interval", "60", "--quick", "2"]);
        // the first pass mirrors what is there
        wait_for("the first pass", Duration::from_secs(20), || {
            t.path(A, "own").exists() && t.path(B, "first").exists()
        });
        t.rec(A, "new");
        wait_for(
            "the new record to be mirrored",
            Duration::from_secs(20),
            || t.path(B, "new").exists(),
        );
        assert_eq!(
            fs::read(t.path(B, "new")).unwrap(),
            fs::read(t.path(A, "new")).unwrap()
        );

        // a second daemon finds the first at work and leaves
        let mut again = t.program(WINDOWLESS, &["daemon", "--interval", "60", "--quick", "2"]);
        let mut second = Guard(
            again
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        assert_eq!(second.exited(Duration::from_secs(15)), Some(0));
        assert_eq!(daemon.exited(Duration::ZERO), None);

        let (code, out) = t.run(CONSOLE, &["status"]);
        assert_eq!(code, 0, "{}", out);
        let summary = format!(
            "OK  daemon {} running | 3 of 3 sessions in every account | 0 conflicts | 0 errors",
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(out.lines().next(), Some(summary.as_str()), "{}", out);
        assert!(
            out.lines()
                .nth(1)
                .is_some_and(|l| l.starts_with("autostart: ")),
            "{}",
            out
        );
        assert!(
            out.contains(&format!(
                "daemon: running (pid {}, version {})",
                daemon.0.id(),
                env!("CARGO_PKG_VERSION")
            )),
            "{}",
            out
        );
        let (code, out) = t.run(CONSOLE, &["log", "-n", "2"]);
        assert_eq!((code, out.lines().count()), (0, 2), "{}", out);
        assert!(
            out.contains(" add ") || out.contains(" synced: ") || out.contains(" full pass: "),
            "{}",
            out
        );

        let (code, out) = t.run(CONSOLE, &["stop"]);
        assert_eq!((code, out.trim()), (0, "stopped"));
        assert_eq!(daemon.exited(Duration::from_secs(10)), Some(0));
        let (code, _) = t.run(CONSOLE, &["status"]);
        assert_eq!(code, 1);
        assert!(fs::read_to_string(t.home.join("sync.log"))
            .unwrap()
            .contains("daemon stopped on request"));
    }
}
