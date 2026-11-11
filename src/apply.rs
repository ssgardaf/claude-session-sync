//! Writing: the daily backup, keeping what is replaced, copying, quarantining, housekeeping.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::consts::*;
use crate::plan::{Act, Kind, Plan};
use crate::pyjson::{self, J};
use crate::sync::{past, sha256_hex, short, Signed, Sync};
use crate::winfs::{self, path_str, Expect, Stat, WriteError};

/// What became of one planned item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Add,
    Update,
    Repair,
    Aligned,
    Quarantine,
    Deferred,
}

/// How a pass went, item by item.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub add: usize,
    pub update: usize,
    pub repair: usize,
    pub aligned: usize,
    pub quarantine: usize,
    pub deferred: usize,
    pub failed: usize,
}

impl Counts {
    fn bump(&mut self, o: Outcome) {
        match o {
            Outcome::Add => self.add += 1,
            Outcome::Update => self.update += 1,
            Outcome::Repair => self.repair += 1,
            Outcome::Aligned => self.aligned += 1,
            Outcome::Quarantine => self.quarantine += 1,
            Outcome::Deferred => self.deferred += 1,
        }
    }

    pub fn sum(&self) -> usize {
        self.add
            + self.update
            + self.repair
            + self.aligned
            + self.quarantine
            + self.deferred
            + self.failed
    }

    /// Items that changed a file's content or place.
    pub fn wrote(&self) -> usize {
        self.add + self.update + self.repair + self.quarantine
    }
}

fn e2s(e: io::Error) -> String {
    e.to_string()
}

/// Every file below `root`, without following links to folders. A folder that cannot be
/// listed is passed over.
fn walk_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let mut dirs = Vec::new();
    for e in entries.flatten() {
        let Ok(t) = e.file_type() else {
            out.push(e.path());
            continue;
        };
        if t.is_dir() {
            dirs.push(e.path());
        } else if t.is_symlink() && fs::metadata(e.path()).map(|m| m.is_dir()).unwrap_or(false) {
            // a link to a folder: not entered
        } else {
            out.push(e.path());
        }
    }
    for d in dirs {
        walk_files(&d, out);
    }
}

fn dir_size(root: &Path) -> io::Result<u64> {
    let mut files = Vec::new();
    walk_files(root, &mut files);
    let mut total = 0;
    for f in files {
        total += fs::metadata(&f)?.len();
    }
    Ok(total)
}

impl Sync {
    /// Zip the whole store once a day, before the first write. An error means: write nothing.
    pub fn backup_if_needed(&mut self) -> Result<Option<PathBuf>, String> {
        let bdir = self.home("backups");
        fs::create_dir_all(&bdir).map_err(e2s)?;
        let day = winfs::local_format(self.now(), "%Y%m%d");
        let names = |dir: &Path| -> Result<Vec<String>, String> {
            let mut out = Vec::new();
            for e in fs::read_dir(dir).map_err(e2s)? {
                out.push(e.map_err(e2s)?.file_name().to_string_lossy().into_owned());
            }
            Ok(out)
        };
        if names(&bdir)?
            .iter()
            .any(|n| n.starts_with(&day) && n.ends_with(".zip"))
        {
            return Ok(None);
        }
        if winfs::disk_free(&bdir).map_err(e2s)? < MIN_FREE_BYTES {
            return Err(String::from("less than 200 MB free; not writing"));
        }
        for n in names(&bdir)? {
            if n.ends_with(".zip.tmp") {
                fs::remove_file(bdir.join(n)).map_err(e2s)?;
            }
        }
        let name = format!("{}{}.zip", day, winfs::local_format(self.now(), "-%H%M%S"));
        let tmp = bdir.join(format!("{}.tmp", name));
        let root = self.p.root.clone();
        let mut files = Vec::new();
        walk_files(&root, &mut files);
        let mut count = 0usize;
        {
            let fh = File::create(&tmp).map_err(e2s)?;
            let mut z = zip::ZipWriter::new(fh);
            for full in files {
                let mut found: Option<(Stat, Vec<u8>)> = None;
                let mut gone = false;
                for _attempt in 0..4 {
                    let got = winfs::stat(&full)
                        .and_then(|st| self.read_shared(&full).map(|data| (st, data)));
                    match got {
                        Ok(pair) => {
                            found = Some(pair);
                            break;
                        }
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {
                            gone = true; // removed meanwhile: nothing to keep
                            break;
                        }
                        Err(_) => self.pause(0.3),
                    }
                }
                let Some((st, data)) = found else {
                    if gone {
                        continue;
                    }
                    let base = full
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    return Err(format!("backup incomplete: cannot read {}", base));
                };
                // zip dates span 1980..2107, in any time zone
                let when = st.mtime().clamp(315_705_600.0, 4_354_560_000.0);
                let stamp = winfs::local_fields(when)
                    .and_then(|(y, mo, d, h, mi, s)| {
                        zip::DateTime::from_date_and_time(
                            y as u16, mo as u8, d as u8, h as u8, mi as u8, s as u8,
                        )
                        .ok()
                    })
                    .unwrap_or_default();
                let rel = full
                    .strip_prefix(&root)
                    .unwrap_or(&full)
                    .to_string_lossy()
                    .replace('\\', "/");
                let options = zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated)
                    .last_modified_time(stamp)
                    .large_file(data.len() as u64 >= u32::MAX as u64);
                z.start_file(rel, options).map_err(|e| e.to_string())?;
                z.write_all(&data).map_err(e2s)?;
                count += 1;
            }
            z.finish().map_err(|e| e.to_string())?;
        }
        {
            let failed = || String::from("backup archive failed its integrity check");
            let mut z =
                zip::ZipArchive::new(File::open(&tmp).map_err(e2s)?).map_err(|e| e.to_string())?;
            if z.len() != count {
                return Err(failed());
            }
            for i in 0..z.len() {
                let mut entry = z.by_index(i).map_err(|_| failed())?;
                io::copy(&mut entry, &mut io::sink()).map_err(|_| failed())?;
            }
        }
        winfs::replace(&tmp, &bdir.join(&name)).map_err(e2s)?;
        let mut zips: Vec<String> = names(&bdir)?
            .into_iter()
            .filter(|n| n.ends_with(".zip"))
            .collect();
        zips.sort();
        // the very first backup (the state before this tool) is kept for good
        if zips.len() > 1 + KEEP_BACKUPS {
            for n in &zips[1..zips.len() - KEEP_BACKUPS] {
                let _ = fs::remove_file(bdir.join(n));
            }
        }
        Ok(Some(bdir.join(name)))
    }

    pub(crate) fn age(&self, st: &Stat) -> f64 {
        self.now() - st.mtime()
    }

    /// How recent a change by something else must be for a file to be left alone for now.
    /// Holding off for two minutes only makes sense while the app may be writing in that
    /// folder. When it is not running, is signed out, or has another folder loaded, nothing but
    /// a save still in progress can be in the way, and that is over in seconds. (The Python
    /// version always waited the two minutes: a session used under one account could then
    /// reach the account the user had just come from too late.)
    fn fresh_window(&mut self, leaf: &str) -> f64 {
        if !self.app_running() {
            return SRC_SETTLE_SEC;
        }
        match self.signed_in() {
            Signed::Out => SRC_SETTLE_SEC,
            Signed::In(at) if at != leaf => SRC_SETTLE_SEC,
            _ => APP_FRESH_SEC, // loaded here, or no telling where
        }
    }

    /// True if something other than this tool changed the file very recently.
    fn app_is_writing(&mut self, leaf: &str, path: &Path, st: &Stat) -> bool {
        let age = self.age(st);
        if self.is_ours(path, st) || age < -2.0 {
            return false;
        }
        age < self.fresh_window(leaf)
    }

    /// Keep a version this tool did not write before it is replaced. An error means it could not.
    fn preserve(&self, leaf: &str, path: &Path, st: &Stat) -> io::Result<PathBuf> {
        let day = winfs::local_format(self.now(), "%Y%m%d");
        let kept_dir = self.home("replaced").join(day).join(leaf.replace('/', "_"));
        fs::create_dir_all(&kept_dir)?;
        let base = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let kept = kept_dir.join(format!("{}.{}", base, st.mtime_ns));
        let data = self.read_shared(path)?;
        if data.len() as u64 != st.size {
            return Err(io::Error::other(
                "destination changed while it was being kept",
            ));
        }
        {
            let mut fh = File::create(&kept)?;
            fh.write_all(&data)?;
            fh.flush()?;
            fh.sync_all()?;
        }
        winfs::set_times(&kept, st.atime_ns, st.mtime_ns)?;
        Ok(kept)
    }

    fn copy(&mut self, act: &Act) -> Result<Outcome, String> {
        let source = act
            .src
            .clone()
            .ok_or_else(|| String::from("nothing to copy from"))?;
        let src = source.path.clone();
        let dst = act.leaf_dir.join(format!("local_{}.json", act.fid));
        let st = winfs::stat(&src).map_err(e2s)?;
        if (st.mtime_ns, st.size) != (source.mtime, source.size) {
            return Ok(Outcome::Deferred); // source changed since it was planned
        }
        // let a save settle; a session that never goes quiet is still followed, on the strength of
        // the checks below (unchanged while read, valid JSON), once it has waited long enough
        let now = self.now();
        let stale = |since: Option<&f64>| {
            since.is_some_and(|t| !(0.0..2.0 * REFRESH_SEC).contains(&(now - t)))
        };
        let overdue =
            stale(self.pushed_before.get(&act.fid)) || stale(self.waiting_before.get(&act.fid));
        let settle = if overdue {
            0.5
        } else if self.urgent {
            URGENT_SETTLE_SEC
        } else {
            SRC_SETTLE_SEC
        };
        if (-2.0..settle).contains(&self.age(&st)) {
            // (a file time may be a few ms ahead of our clock)
            self.waiting.entry(act.fid.clone()).or_insert(now);
            self.settling += 1;
            return Ok(Outcome::Deferred);
        }
        let data = self.read_shared(&src).map_err(e2s)?;
        let st2 = winfs::stat(&src).map_err(e2s)?;
        if (st2.mtime_ns, st2.size) != (st.mtime_ns, st.size) || data.len() as u64 != st.size {
            return Ok(Outcome::Deferred); // source changed while it was being read
        }
        let text = pyjson::decode_utf8_sig(&data)
            .ok_or_else(|| String::from("source is not valid UTF-8"))?;
        let j = pyjson::loads(text).map_err(|_| String::from("source is not valid JSON"))?;
        if !j.is_object()
            || j.get("sessionId").and_then(J::as_str) != Some(format!("local_{}", act.fid).as_str())
        {
            return Err(String::from("source is not the expected record"));
        }
        let dst_st = match winfs::stat(&dst) {
            Ok(s) => Some(s),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e2s(e)),
        };
        if dst_st.map(|s| (s.mtime_ns, s.size)) != act.expect {
            return Ok(Outcome::Deferred); // destination is not what was planned for
        }
        if act.kind == Kind::Add {
            // deleted here since the plan was made? then it stays deleted
            let gone = |id: &str| act.leaf_dir.join(format!("deleted_{}", id)).exists();
            if gone(&act.fid) || source.ids.iter().any(|i| gone(i)) {
                return Ok(Outcome::Deferred);
            }
        } else if self.in_use_now(&act.leaf) {
            return Ok(Outcome::Deferred); // the app has loaded this folder since the plan was made
        }
        let sha = sha256_hex(&data)[..32].to_string();
        let mut saved: Option<PathBuf> = None;
        if let Some(ds) = &dst_st {
            if self.app_is_writing(&act.leaf, &dst, ds) {
                return Ok(Outcome::Deferred);
            }
            if act.kind == Kind::Update
                && ds.size == data.len() as u64
                && self.read_shared(&dst).map_err(e2s)? == data
            {
                self.journal(&[
                    ("kind", json!("aligned")),
                    ("leaf", json!(act.leaf)),
                    ("fid", json!(act.fid)),
                    ("dst", json!(path_str(&dst))),
                    ("old_mtime", json!(ds.mtime_ns)),
                    ("mtime", json!(st.mtime_ns)),
                    ("size", json!(st.size)),
                    ("sha", json!(sha)),
                ])
                .map_err(e2s)?;
                winfs::set_times(&dst, st.atime_ns, st.mtime_ns).map_err(e2s)?;
                self.mark_ours(
                    &dst,
                    st.mtime_ns,
                    st.size,
                    Some(source.last),
                    Some(&sha),
                    Some(&source.user),
                );
                return Ok(Outcome::Aligned);
            }
            if !self.is_ours(&dst, ds) {
                let kept = self.preserve(&act.leaf, &dst, ds).map_err(e2s)?;
                if act.cur.as_ref().is_some_and(|cur| cur.cli != source.cli) {
                    self.log(&format!(
                        "note: {} {} pointed at another conversation; that version is kept at {}",
                        short(&act.leaf),
                        act.fid.chars().take(8).collect::<String>(),
                        path_str(&kept)
                    ));
                }
                saved = Some(kept);
            }
        }
        if act.kind != Kind::Update || saved.is_some() {
            // on record before the file is touched, so undo can always find it
            self.journal(&[
                ("kind", json!(act.kind.name())),
                ("leaf", json!(act.leaf)),
                ("fid", json!(act.fid)),
                ("dst", json!(path_str(&dst))),
                ("saved", json!(saved.as_deref().map(path_str))),
                ("mtime", json!(st.mtime_ns)),
                ("size", json!(st.size)),
                ("sha", json!(sha)),
            ])
            .map_err(e2s)?;
        }
        if let Some(hook) = self.hooks.before_write.as_mut() {
            hook(&dst);
        }
        // the last look at the destination is part of putting the file in place
        let expect = match &dst_st {
            None => Expect::Absent,
            Some(ds) => Expect::Is(ds.mtime_ns, ds.size),
        };
        match winfs::atomic_write(&dst, &data, Some((st.atime_ns, st.mtime_ns)), true, expect) {
            Ok(()) => {}
            // something appeared or changed there in that instant: it is left alone
            Err(WriteError::Changed) => return Ok(Outcome::Deferred),
            Err(WriteError::Io(e)) => return Err(e2s(e)),
        }
        self.waiting.remove(&act.fid);
        self.mark_ours(
            &dst,
            st.mtime_ns,
            st.size,
            Some(source.last),
            Some(&sha),
            Some(&source.user),
        );
        if saved.is_some() {
            self.log(&format!(
                "update {} {}  {}  (the version that was there is kept)",
                short(&act.leaf),
                act.fid.chars().take(8).collect::<String>(),
                source.title
            ));
        }
        Ok(match act.kind {
            Kind::Add => Outcome::Add,
            Kind::Update => Outcome::Update,
            Kind::Repair => Outcome::Repair,
            Kind::Quarantine => Outcome::Quarantine,
        })
    }

    fn quarantine(&mut self, act: &Act) -> Result<Outcome, String> {
        let r = act
            .cur
            .clone()
            .ok_or_else(|| String::from("nothing to remove"))?;
        let st = winfs::stat(&r.path).map_err(e2s)?;
        if Some((st.mtime_ns, st.size)) != act.expect
            || self.app_is_writing(&act.leaf, &r.path, &st)
            || self.in_use_now(&act.leaf)
        {
            return Ok(Outcome::Deferred);
        }
        let ts = act.ts.unwrap_or(0);
        let qdir = self.home("quarantine").join(act.leaf.replace('/', "_"));
        fs::create_dir_all(&qdir).map_err(e2s)?;
        let base = r
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let q = qdir.join(format!("{}.{}", base, (self.now() * 1000.0) as i64));
        let todo: Vec<PathBuf> = act
            .ids
            .iter()
            .map(|i| act.leaf_dir.join(format!("deleted_{}", i)))
            .filter(|m| !m.exists())
            .collect();
        self.journal(&[
            ("kind", json!("quarantine")),
            ("leaf", json!(act.leaf)),
            ("fid", json!(act.fid)),
            ("dst", json!(path_str(&r.path))),
            ("q", json!(path_str(&q))),
            (
                "markers",
                json!(todo.iter().map(|m| path_str(m)).collect::<Vec<_>>()),
            ),
            ("ts", json!(ts)),
            ("title", json!(r.title)),
        ])
        .map_err(e2s)?;
        for marker in &todo {
            // markers first: a crash in between must not look like a live session
            fs::write(marker, ts.to_string()).map_err(e2s)?;
            let ms = winfs::stat(marker).map_err(e2s)?;
            self.mark_ours(marker, ms.mtime_ns, ms.size, None, None, None);
        }
        winfs::move_file(&r.path, &q).map_err(e2s)?;
        self.forget(&r.path);
        Ok(Outcome::Quarantine)
    }

    /// Housekeeping on the full pass: stale temp files, and kept versions past their retention.
    pub fn sweep(&mut self) {
        for (_, path) in self.leaves() {
            let _ = (|| -> io::Result<()> {
                for e in self.list_dir(&path)? {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if name.starts_with(".sync-")
                        && name.ends_with(".tmp")
                        && self.age(&winfs::stat_entry(&e)?) > 600.0
                    {
                        fs::remove_file(e.path())?;
                    }
                }
                Ok(())
            })();
        }
        let limit = winfs::local_format(self.now() - KEEP_REPLACED_DAYS * 86400.0, "%Y%m%d");
        let replaced = self.home("replaced");
        let _ = (|| -> io::Result<()> {
            let mut days = Vec::new();
            for e in fs::read_dir(&replaced)? {
                let d = e?.file_name().to_string_lossy().into_owned();
                if d.len() == 8 && d.bytes().all(|b| b.is_ascii_digit()) {
                    days.push(d);
                }
            }
            days.sort();
            let mut sizes = Vec::new();
            for d in &days {
                sizes.push(dir_size(&replaced.join(d))?);
            }
            let mut total: u64 = sizes.iter().sum();
            // oldest first; the newest day is never dropped
            for (d, size) in days.iter().zip(&sizes).take(days.len().saturating_sub(1)) {
                if *d < limit || total > self.max_replaced_bytes {
                    let _ = fs::remove_dir_all(replaced.join(d));
                    total -= size;
                }
            }
            Ok(())
        })();
    }

    /// Carry out a plan. An error means the pass could not start (or finish its bookkeeping).
    pub fn apply(&mut self, acts: &[Act]) -> Result<Counts, String> {
        let mut done = Counts::default();
        self.settling = 0;
        if acts.is_empty() {
            return Ok(done);
        }
        let first = self
            .state()
            .get("propagate_since")
            .is_none_or(|v| v.is_null());
        self.touch_alive(first).map_err(e2s)?;
        if let Some(b) = self.backup_if_needed()? {
            // an error -> nothing is written this pass
            self.log(&format!("backup -> {}", path_str(&b)));
        }
        self.pushed_before = self.pushed.clone(); // as things stood when this pass began
        self.waiting_before = self.waiting.clone();
        let mut copies = 0usize;
        for (n, a) in acts.iter().enumerate() {
            let key = (a.leaf.clone(), a.fid.clone());
            if a.kind != Kind::Quarantine {
                if let Some(hook) = self.hooks.before_copy.as_mut() {
                    hook(copies);
                }
                copies += 1;
            }
            let result = (|| -> Result<(), String> {
                let res = if a.kind == Kind::Quarantine {
                    self.quarantine(a)?
                } else {
                    self.copy(a)?
                };
                done.bump(res);
                if res == Outcome::Deferred && self.used.contains(&a.fid) {
                    self.owed.insert(a.fid.clone()); // asked for, and not done yet
                }
                if res != Outcome::Deferred {
                    self.retry.remove(&key);
                    let now = self.now();
                    self.pushed.insert(a.fid.clone(), now);
                }
                if matches!(res, Outcome::Add | Outcome::Repair | Outcome::Quarantine) {
                    // routine refreshes of our own copies are only counted
                    let word = match res {
                        Outcome::Add => "add",
                        Outcome::Repair => "repair",
                        _ => "quarantine",
                    };
                    self.log(&format!(
                        "{} {} {}  {}",
                        word,
                        short(&a.leaf),
                        a.fid.chars().take(8).collect::<String>(),
                        a.rec().title
                    ));
                }
                if matches!(res, Outcome::Add | Outcome::Repair) {
                    if let Some(set) = self.prev.as_mut().and_then(|p| p.get_mut(&a.leaf)) {
                        set.insert(a.fid.clone()); // so that its disappearance is noticed next time
                    }
                }
                Ok(())
            })();
            if let Err(e) = result {
                done.failed += 1;
                if self.used.contains(&a.fid) {
                    self.owed.insert(a.fid.clone());
                }
                // this one item backs off; the rest carry on as usual
                let fails = self.retry.get(&key).map_or(0, |slot| slot.0) + 1;
                let wait = (15.0 * 2f64.powi(fails.min(6) as i32)).min(600.0);
                self.retry.insert(key, (fails, self.now() + wait));
                let msg = format!(
                    "{} {} {}: {}",
                    a.kind.name(),
                    short(&a.leaf),
                    a.fid.chars().take(8).collect::<String>(),
                    e
                );
                self.last_error = Some(msg.clone());
                self.log_every(&format!("failed {}", msg), 3600.0);
            }
            if (n + 1) % MANIFEST_EVERY == 0 {
                // Folding the running list into manifest.json is housekeeping, not part of the item
                // just done: if it cannot be saved now, every write is still in manifest.log, and
                // the save at the end of the pass is made all the same. (In the Python version a
                // failure here counted against the item, and put it on back-off.)
                if let Err(e) = self.save_manifest() {
                    self.log_every(
                        &format!("the list of our writes could not be saved yet: {}", e),
                        3600.0,
                    );
                }
            }
        }
        self.save_manifest().map_err(e2s)?;
        Ok(done)
    }

    /// Drop the requests that are done with: those the pass has just carried out in full, and
    /// those the plan looked at and found nothing left to settle for. One that was only partly
    /// carried out (a copy in the folder the app has loaded must wait), or that came in after
    /// that plan was made, stays. (A request is closed in the very pass that carries it out,
    /// so that an `undo` of that pass does not find it still standing and have it applied again.)
    pub(crate) fn close_requests(&mut self) -> io::Result<()> {
        let mut st = self.state();
        let asked = match st.get("resolve") {
            Some(serde_json::Value::Object(a)) => a.clone(),
            _ => serde_json::Map::new(),
        };
        let keep: serde_json::Map<String, serde_json::Value> = asked
            .iter()
            .filter(|(f, _)| self.owed.contains(*f) || !self.plan_requests.contains(*f))
            .map(|(f, t)| (f.clone(), t.clone()))
            .collect();
        if keep != asked {
            st.insert("resolve".into(), serde_json::Value::Object(keep));
            self.write_state(&st)?;
        }
        Ok(())
    }

    /// One pass: plan, then write. (`run(apply=True)` of the Python version; a preview is `plan`.)
    pub fn run(&mut self) -> Counts {
        let plan = self.plan();
        self.run_plan(&plan)
    }

    pub(crate) fn run_plan(&mut self, plan: &Plan) -> Counts {
        self.aborted = false;
        let result = (|| -> Result<Counts, String> {
            let done = self.apply(&plan.acts)?;
            self.save_vanished().map_err(e2s)?;
            if !self.plan_requests.is_empty() {
                self.close_requests().map_err(e2s)?; // a resolve with nothing left to settle is done with
            }
            if !self.state().is_empty() {
                self.touch_alive(false).map_err(e2s)?;
            }
            if done.failed == 0 {
                self.last_error = None;
            }
            Ok(done)
        })();
        match result {
            Ok(done) => done,
            Err(e) => {
                self.aborted = true;
                self.last_error = Some(format!("nothing written: {}", e));
                self.log_every(&format!("pass aborted before writing: {}", e), 3600.0);
                Counts {
                    deferred: plan.acts.len(),
                    ..Counts::default()
                }
            }
        }
    }

    /// True when something other than a change on disk calls for a pass: a retry or a held-back
    /// refresh has come due, a resolve request has come in, or a first sign-in should have
    /// settled by now.
    pub fn something_due(&mut self) -> bool {
        let now = self.now();
        if self.retry.values().any(|slot| past(slot.1 - now)) {
            return true;
        }
        if self.notes.next_refresh.is_some_and(|due| past(due - now)) {
            return true;
        }
        if self
            .overrides()
            .difference(&self.plan_requests)
            .next()
            .is_some()
        {
            return true;
        }
        if let Some((leaf, at)) = self.home_check.clone() {
            if past(at - now) {
                // ...and again in a while, should it still not be
                self.home_check = Some((leaf, now + 30.0));
                return true;
            }
        }
        false
    }
}
