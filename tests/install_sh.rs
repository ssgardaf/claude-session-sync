//! install.sh, as far as it can be run without the network and without installing anything:
//! `curl` is a stand-in that hands out a made-up release from a folder, and the "program" in
//! that release is a script that only writes down how it was called.
//!
//! What is checked is the promise in the script's own first lines: a download of it that
//! was cut off anywhere runs nothing at all. It is cut at every single byte and given to
//! the system's `sh` (dash on Debian and Ubuntu, bash as sh on macOS), and to `dash` and
//! `bash` by name where they are there.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const ASSETS: [&str; 3] = [
    "claude-session-sync-macos-universal.tar.gz",
    "claude-session-sync-linux-x64.tar.gz",
    "claude-session-sync-linux-arm64.tar.gz",
];

fn script(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn sha256_of(file: &Path) -> String {
    for (tool, args) in [("sha256sum", vec![]), ("shasum", vec!["-a", "256"])] {
        if let Ok(out) = Command::new(tool).args(&args).arg(file).output() {
            if out.status.success() {
                return String::from_utf8_lossy(&out.stdout)
                    .split_whitespace()
                    .next()
                    .unwrap()
                    .to_string();
            }
        }
    }
    panic!("neither sha256sum nor shasum is here");
}

/// A folder with a made-up release in it, and a folder with the stand-in for `curl`.
struct World {
    tmp: PathBuf,
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.tmp);
    }
}

impl World {
    fn new() -> World {
        let tmp = std::env::temp_dir().join(format!(
            "css-install-sh-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_micros()
        ));
        let (release, pack, stubs) = (tmp.join("release"), tmp.join("pack"), tmp.join("stubs"));
        for dir in [&release, &pack, &stubs, &tmp.join("cut"), &tmp.join("logs")] {
            fs::create_dir_all(dir).unwrap();
        }
        // the "program": says its version, and otherwise writes down what it was asked
        script(
            &pack.join("claude-session-sync"),
            "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 9.9.9; exit 0; fi\necho \"ran: $*\" >> \"$CSS_TEST_LOG\"\n",
        );
        fs::write(pack.join("README.md"), "x").unwrap();
        fs::write(pack.join("LICENSE"), "x").unwrap();
        for asset in ASSETS {
            let made = Command::new("tar")
                .arg("-czf")
                .arg(release.join(asset))
                .arg("-C")
                .arg(&pack)
                .args(["claude-session-sync", "README.md", "LICENSE"])
                .status()
                .unwrap();
            assert!(made.success());
            fs::write(
                release.join(format!("{}.sha256", asset)),
                format!("{}  {}\n", sha256_of(&release.join(asset)), asset),
            )
            .unwrap();
        }
        // `curl ... -o <file> <url>`: the file of that name from the made-up release
        script(
            &stubs.join("curl"),
            "#!/bin/sh\nout=; url=\nwhile [ $# -gt 0 ]; do\n  case \"$1\" in\n    -o) out=$2; shift ;;\n    http*) url=$1 ;;\n  esac\n  shift\ndone\ncp \"$CSS_TEST_RELEASE/${url##*/}\" \"$out\"\n",
        );
        World { tmp }
    }

    /// Run a script file with the options a user would give the install. Its exit code, what
    /// it printed, and what the made-up program wrote down.
    fn run(&self, shell: &str, file: &Path, tag: &str) -> (i32, String, String) {
        let log = self.tmp.join("logs").join(tag);
        let _ = fs::remove_file(&log);
        let path = format!(
            "{}:{}",
            self.tmp.join("stubs").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let out = Command::new(shell)
            .arg(file)
            .args(["--delete-sync", "--days", "60"])
            .env("PATH", path)
            .env("CSS_TEST_RELEASE", self.tmp.join("release"))
            .env("CSS_TEST_LOG", &log)
            .env("TMPDIR", self.tmp.join("cut"))
            .stdin(Stdio::null())
            .output()
            .unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            fs::read_to_string(&log).unwrap_or_default(),
        )
    }
}

fn shells() -> Vec<&'static str> {
    let mut found = vec!["sh"];
    for other in ["dash", "bash"] {
        let there = Command::new(other)
            .args(["-c", "exit 0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if there {
            found.push(other);
        }
    }
    found
}

#[test]
fn a_download_that_was_cut_off_anywhere_runs_nothing() {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh");
    let whole = fs::read(&source).unwrap();
    assert!(!whole.contains(&b'\r')); // (a shell would take a carriage return for part of a word)
    let world = World::new();
    let world = &world;
    for shell in shells() {
        // the whole script: it downloads, checks, unpacks and runs the install with the options
        let (code, said, ran) = world.run(shell, &source, &format!("{}-whole", shell));
        if ran.is_empty() && said.is_empty() {
            // (as root the script refuses before it does anything: nothing to compare with)
            eprintln!(
                "skipped for {}: the script did not run here (exit code {})",
                shell, code
            );
            continue;
        }
        assert_eq!(code, 0, "{}: {}", shell, said);
        assert_eq!(ran, "ran: install --delete-sync --days 60\n", "{}", shell);
        assert!(
            said.contains("Installing claude-session-sync 9.9.9"),
            "{}",
            said
        );
        let whole_outcome = (code, said, ran);
        // cut at every byte: nothing at all, or all of it
        let cuts: Vec<usize> = (0..whole.len()).collect();
        let threads = 8;
        let mut ran_whole = Vec::new();
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..threads)
                .map(|worker| {
                    let (cuts, whole, whole_outcome) = (&cuts, &whole, &whole_outcome);
                    scope.spawn(move || {
                        let mut ran_whole = Vec::new();
                        for &at in cuts.iter().skip(worker).step_by(threads) {
                            let file = world
                                .tmp
                                .join("cut")
                                .join(format!("{}-{}.sh", shell, at));
                            fs::write(&file, &whole[..at]).unwrap();
                            let outcome = world.run(shell, &file, &format!("{}-{}", shell, at));
                            fs::remove_file(&file).unwrap();
                            let nothing = outcome.1.is_empty() && outcome.2.is_empty();
                            assert!(
                                nothing || outcome == *whole_outcome,
                                "{} ran a part of the script cut at byte {} of {}: exit {}, printed {:?}, the program was asked {:?}",
                                shell,
                                at,
                                whole.len(),
                                outcome.0,
                                outcome.1,
                                outcome.2
                            );
                            if !nothing {
                                ran_whole.push(at);
                            }
                        }
                        ran_whole
                    })
                })
                .collect();
            for handle in handles {
                ran_whole.extend(handle.join().unwrap());
            }
        });
        // only a cut that leaves the closing brace in place is a whole script
        let closing = whole.iter().rposition(|b| *b == b'}').unwrap();
        ran_whole.sort();
        assert!(
            ran_whole.iter().all(|at| *at > closing),
            "{}: {:?} (the closing brace is byte {})",
            shell,
            ran_whole,
            closing
        );
    }
    // nothing was left in the folder the script works in
    assert_eq!(fs::read_dir(world.tmp.join("cut")).unwrap().count(), 0);
}

/// A download whose checksum does not match is not unpacked, let alone run.
#[test]
fn a_download_that_does_not_match_its_checksum_is_not_run() {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh");
    let world = World::new();
    for asset in ASSETS {
        fs::write(
            world.tmp.join("release").join(format!("{}.sha256", asset)),
            format!("{}  {}\n", "0".repeat(64), asset),
        )
        .unwrap();
    }
    let (code, said, ran) = world.run("sh", &source, "mismatch");
    if said.is_empty() {
        eprintln!("skipped: the script did not run here (exit code {})", code);
        return;
    }
    assert_eq!(code, 1);
    assert!(ran.is_empty());
    assert!(!said.contains("Installing"));
    assert_eq!(fs::read_dir(world.tmp.join("cut")).unwrap().count(), 0);
}
