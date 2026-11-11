//! Where the store, the state folder, the transcripts and the app's log are looked for.

use super::*;
use crate::applog::AppLog;
use crate::paths::{choose_store, store_candidates};

const PACKAGE: &str = "Claude_abc123xyz";

fn packaged_store(b: &Base) -> PathBuf {
    b.tmp
        .join("local")
        .join("Packages")
        .join(PACKAGE)
        .join("LocalCache")
        .join("Roaming")
        .join("Claude")
        .join("claude-code-sessions")
}

fn record_in(store: &Path, seconds_ago: f64) {
    let dir = store.join(A.0).join(A.1);
    fs::create_dir_all(&dir).unwrap();
    let p = dir.join("local_x.json");
    fs::write(&p, b"{}").unwrap();
    set_mtime(&p, now() - seconds_ago);
}

#[test]
fn defaults_follow_the_environment() {
    let b = Base::new();
    let p = Paths::discover(&b.env()).unwrap();
    assert_eq!(p.root, b.root);
    assert_eq!(p.home, b.tmp.join("user").join(".claude-session-sync")); // not under AppData
    assert_eq!(
        p.projects,
        b.tmp.join("user").join(".claude").join("projects")
    );
    // ...and they are what the tests' explicit paths say
    let explicit = b.paths();
    assert_eq!(
        (p.root, p.home, p.projects),
        (explicit.root, explicit.home, explicit.projects)
    );
}

#[test]
fn nothing_is_created_by_looking() {
    let b = Base::new();
    let before = b.snapshot(&b.tmp);
    Paths::discover(&b.env()).unwrap();
    assert_eq!(before, b.snapshot(&b.tmp));
    assert!(!b.home.exists());
}

#[test]
fn overrides_win() {
    let b = Base::new();
    let mut env = b.env();
    env.home_override = Some(b.tmp.join("my-state"));
    env.store_override = Some(b.tmp.join("my-store"));
    env.claude_config_dir = Some(b.tmp.join("my-claude"));
    let p = Paths::discover(&env).unwrap();
    assert_eq!(p.home, b.tmp.join("my-state"));
    assert_eq!(p.root, b.tmp.join("my-store"));
    assert_eq!(p.projects, b.tmp.join("my-claude").join("projects"));
}

#[test]
fn overrides_stand_in_for_missing_variables() {
    let b = Base::new();
    let env = Env {
        home_override: Some(b.tmp.join("my-state")),
        store_override: Some(b.tmp.join("my-store")),
        claude_config_dir: Some(b.tmp.join("my-claude")),
        ..Env::default()
    };
    assert!(Paths::discover(&env).is_ok());
    assert!(Paths::discover(&Env::default()).is_err());
    // this tool's own folder does not depend on LOCALAPPDATA (only the app's log is looked for there)
    let no_local = Env {
        localappdata: None,
        ..b.env()
    };
    assert_eq!(Paths::discover(&no_local).unwrap().home, b.home);
    let no_profile = Env {
        userprofile: None,
        ..b.env()
    };
    assert!(Paths::discover(&no_profile)
        .unwrap_err()
        .contains("USERPROFILE"));
}

#[test]
fn store_of_a_packaged_install_is_found() {
    let b = Base::new();
    fs::remove_dir_all(b.tmp.join("appdata")).unwrap(); // only the Store package's folder exists
    let packaged = packaged_store(&b);
    record_in(&packaged, 60.0);
    fs::create_dir_all(b.tmp.join("local").join("Packages").join("SomethingElse_1")).unwrap();
    let env = b.env();
    assert_eq!(
        store_candidates(&env),
        vec![b.root.clone(), packaged.clone()]
    );
    assert_eq!(Paths::discover(&env).unwrap().root, packaged);
}

#[test]
fn of_two_stores_the_one_written_most_recently_wins() {
    let b = Base::new();
    let packaged = packaged_store(&b);
    record_in(&b.root, 3.0 * HOUR);
    record_in(&packaged, 60.0);
    assert_eq!(Paths::discover(&b.env()).unwrap().root, packaged);
    record_in(&b.root, 5.0);
    assert_eq!(Paths::discover(&b.env()).unwrap().root, b.root);
    // an empty second store does not displace the one in use
    fs::remove_dir_all(&packaged).unwrap();
    fs::create_dir_all(&packaged).unwrap();
    assert_eq!(Paths::discover(&b.env()).unwrap().root, b.root);
}

#[test]
fn before_any_store_exists_the_classic_place_is_assumed() {
    let b = Base::new();
    fs::remove_dir_all(b.tmp.join("appdata")).unwrap();
    assert_eq!(Paths::discover(&b.env()).unwrap().root, b.root);
    assert_eq!(choose_store(&[]), None);
}

#[test]
fn log_is_looked_for_in_every_known_place_and_the_newest_wins() {
    let b = Base::new();
    let classic = b.applog.clone();
    let packaged = b
        .tmp
        .join("local")
        .join("Packages")
        .join(PACKAGE)
        .join("LocalCache")
        .join("Local")
        .join("Claude")
        .join("logs")
        .join("main.log");
    let roaming = b
        .tmp
        .join("appdata")
        .join("Claude")
        .join("logs")
        .join("main.log");
    fs::create_dir_all(packaged.parent().unwrap()).unwrap();
    let log = AppLog::new(Paths::discover(&b.env()).unwrap().logs);
    assert_eq!(
        log.candidates(),
        vec![classic.clone(), packaged.clone(), roaming.clone()]
    );
    assert_eq!(log.current(), None);
    for (path, ago) in [(&classic, 300.0), (&packaged, 200.0), (&roaming, 100.0)] {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"x\n").unwrap();
        set_mtime(path, now() - ago);
    }
    assert_eq!(log.current().unwrap().0, roaming);
    set_mtime(&packaged, now() - 10.0);
    assert_eq!(log.current().unwrap().0, packaged);
    set_mtime(&classic, now() - 1.0);
    assert_eq!(log.current().unwrap().0, classic);
}
