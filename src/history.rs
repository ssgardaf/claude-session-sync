//! Taking things back: undo, settling a conflict on request, restoring from quarantine.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{json, Map, Value};

use crate::pyjson::{self, J};
use crate::records::is_id;
use crate::sync::{num, Sync};
use crate::winfs::{self, Expect, WriteError};

/// What an undo did, entry by entry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Undone {
    pub removed: usize,
    pub restored: usize,
    pub put_back: usize,
    pub left_changed_since: usize,
    pub nothing_to_undo: usize,
}

/// The journal entries (both files, oldest file first), unparsable lines left out.
pub(crate) fn journal_entries(home: &Path) -> Vec<Map<String, Value>> {
    let mut out = Vec::new();
    for name in ["journal.jsonl.1", "journal.jsonl"] {
        let Ok(data) = fs::read(home.join(name)) else {
            continue;
        };
        for line in data.split(|&b| b == b'\n') {
            if let Ok(Value::Object(e)) = serde_json::from_slice::<Value>(line) {
                out.push(e);
            }
        }
    }
    out
}

fn quarantined_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^local_([A-Za-z0-9_-]+)\.json\.([0-9]+)$").expect("valid pattern")
    })
}

fn matches_key(fid: &str, key: &str) -> bool {
    fid == key || (key.chars().count() >= 6 && fid.starts_with(key))
}

/// What to add to "nothing matches" when too little of an id was given to go by.
fn too_short(key: &str) -> &'static str {
    if key.chars().count() < 6 {
        " (give the whole session id, or at least its first 6 characters)"
    } else {
        ""
    }
}

fn text<'a>(e: &'a Map<String, Value>, key: &str) -> Result<&'a str, String> {
    e.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("no usable '{}' in the entry", key))
}

impl Sync {
    /// Reverse what this tool did at or after `since` (epoch seconds), newest first.
    /// A file whose content has been changed since is left as it is.
    pub fn undo(&mut self, since: f64) -> io::Result<Undone> {
        let mut entries: Vec<(f64, Map<String, Value>)> = journal_entries(&self.p.home)
            .into_iter()
            .filter_map(|e| num(e.get("t")).filter(|t| *t >= since).map(|t| (t, e)))
            .collect();
        entries.sort_by(|a, b| b.0.total_cmp(&a.0));
        let stamp = winfs::local_format(self.now(), "%Y%m%d-%H%M%S");
        let mut done = Undone::default();
        for (_, e) in &entries {
            if let Err(err) = self.undo_one(e, &stamp, &mut done) {
                self.log(&format!("undo skipped one entry: {}", err));
            }
        }
        self.save_manifest()?;
        self.log(&format!(
            "undo since {}: removed {}, restored {}, put back {}, left (changed since) {}, nothing to undo {}",
            winfs::local_format(since, "%Y-%m-%d %H:%M:%S"),
            done.removed,
            done.restored,
            done.put_back,
            done.left_changed_since,
            done.nothing_to_undo
        ));
        Ok(done)
    }

    fn undo_one(
        &mut self,
        e: &Map<String, Value>,
        stamp: &str,
        done: &mut Undone,
    ) -> Result<(), String> {
        let e2s = |err: io::Error| err.to_string();
        let kind = e.get("kind").and_then(Value::as_str).unwrap_or("");
        if kind == "restore" || kind == "resolve" {
            // a request to settle a conflict that was made in the time now taken back is taken
            // back with it, if it is still standing
            if kind == "resolve" {
                if let Some(fid) = e.get("fid").and_then(Value::as_str) {
                    self.drop_request(fid).map_err(e2s)?;
                }
            }
            done.nothing_to_undo += 1;
            return Ok(());
        }
        if kind == "quarantine" {
            let q = PathBuf::from(text(e, "q")?);
            let dst = PathBuf::from(text(e, "dst")?);
            if q.exists() && !dst.exists() {
                winfs::move_file(&q, &dst).map_err(e2s)?;
                let ts = match e.get("ts") {
                    None | Some(Value::Null) => String::from("None"),
                    Some(Value::String(s)) => s.clone(),
                    Some(v) => v.to_string(),
                };
                if let Some(Value::Array(markers)) = e.get("markers") {
                    for m in markers.iter().filter_map(Value::as_str) {
                        let marker = PathBuf::from(m);
                        if let Ok(data) = self.read_shared(&marker) {
                            let written: String = data
                                .iter()
                                .filter(|b| b.is_ascii())
                                .map(|&b| b as char)
                                .collect();
                            if written.trim() == ts && fs::remove_file(&marker).is_ok() {
                                self.forget(&marker);
                            }
                        }
                    }
                }
                // The deletion was carried over, and now it is taken back: its marker in the
                // account it was made in is still there, and must not carry it over again.
                if let (Some(fid), Some(ts)) =
                    (e.get("fid").and_then(Value::as_str), num(e.get("ts")))
                {
                    self.exempt(fid, ts).map_err(e2s)?;
                }
                done.put_back += 1;
            } else {
                done.left_changed_since += 1;
            }
            return Ok(());
        }
        let dst = PathBuf::from(text(e, "dst")?);
        let st = match winfs::stat(&dst) {
            Ok(st) => st,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                done.nothing_to_undo += 1;
                return Ok(());
            }
            Err(err) => return Err(e2s(err)),
        };
        // The entry was written before the file was touched. Does the file hold what it describes
        // (same time and size, or the same bytes saved again by the app)?
        let same_size = e.get("size").and_then(Value::as_u64) == Some(st.size);
        let mut described =
            e.get("mtime").and_then(Value::as_i64) == Some(st.mtime_ns) && same_size;
        if !described && same_size {
            if let Some(sha) = e
                .get("sha")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                described = self.sha(&dst, st.mtime_ns, st.size).as_deref() == Some(sha);
            }
        }
        let saved = e
            .get("saved")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        if kind == "aligned" {
            // only the file time was changed: put it back, if nothing came after
            let old = e.get("old_mtime");
            match old
                .and_then(Value::as_i64)
                .or_else(|| num(old).map(|f| f as i64))
            {
                Some(old) if described => {
                    winfs::set_times(&dst, st.atime_ns, old).map_err(e2s)?;
                    self.forget(&dst);
                    done.restored += 1;
                }
                _ => done.nothing_to_undo += 1,
            }
            return Ok(());
        }
        if !(described || self.is_ours(&dst, &st)) {
            // ours too if we refreshed it since (not journaled)
            done.left_changed_since += 1;
            return Ok(());
        }
        if kind == "add" || (kind == "repair" && saved.is_none()) {
            let gone = self
                .home("undone")
                .join(stamp)
                .join(text(e, "leaf")?.replace('/', "_"));
            winfs::make_dirs(&gone).map_err(e2s)?;
            let base = dst
                .file_name()
                .ok_or_else(|| String::from("the entry names no file"))?;
            // (a second undo within the same second finds the first one's file there: it is
            // not replaced, the next free name is taken)
            let mut kept = gone.join(base);
            let mut n = 1;
            while kept.exists() {
                n += 1;
                kept = gone.join(format!("{}.{}", base.to_string_lossy(), n));
            }
            winfs::move_file(&dst, &kept).map_err(e2s)?;
            let _ = winfs::make_private(&kept);
            self.forget(&dst);
            done.removed += 1;
        } else if let Some(saved) = saved.filter(|s| s.exists()) {
            let s = winfs::stat(&saved).map_err(e2s)?;
            let data = self.read_shared(&saved).map_err(e2s)?;
            match winfs::atomic_write(
                &dst,
                &data,
                Some((s.atime_ns, s.mtime_ns)),
                true,
                Expect::Any,
            ) {
                Ok(()) => {}
                Err(WriteError::Io(err)) => return Err(e2s(err)),
                Err(WriteError::Changed) => return Err(String::from("the destination changed")),
            }
            self.forget(&dst);
            // the version that gave way is back: a request that would replace it again is void
            if let Some(fid) = e.get("fid").and_then(Value::as_str) {
                self.drop_request(fid).map_err(e2s)?;
            }
            done.restored += 1;
        } else {
            done.nothing_to_undo += 1;
        }
        Ok(())
    }

    /// Settle a conflict: let the most advanced copy of that session replace the others (the
    /// replaced versions are kept under `replaced\`). The daemon carries it out on its next pass.
    pub fn resolve(&mut self, key: &str) -> io::Result<Option<String>> {
        let plan = self.plan();
        let fids: BTreeSet<&String> = plan
            .notes
            .conflicts
            .iter()
            .map(|c| &c.3)
            .filter(|f| matches_key(f, key))
            .collect();
        if fids.len() != 1 {
            let msg = if fids.is_empty() {
                format!("no conflict matches '{}'{}", key, too_short(key))
            } else {
                format!(
                    "'{}' matches several: {}",
                    key,
                    fids.iter()
                        .map(|f| f.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            (self.out)(&msg);
            return Ok(None);
        }
        let fid = fids.into_iter().next().cloned().unwrap_or_default();
        self.request(std::slice::from_ref(&fid))?;
        self.log(&format!(
            "resolve {}: the most advanced copy will replace the others",
            fid.chars().take(8).collect::<String>()
        ));
        Ok(Some(fid))
    }

    /// Settle every conflict there is now. The sessions asked for.
    pub fn resolve_all(&mut self) -> io::Result<Vec<String>> {
        let plan = self.plan();
        let fids: BTreeSet<String> = plan.notes.conflicts.iter().map(|c| c.3.clone()).collect();
        let fids: Vec<String> = fids.into_iter().collect();
        if !fids.is_empty() {
            self.request(&fids)?;
            self.log(&format!(
                "resolve --all: for {} session(s) the most advanced copy will replace the others",
                fids.len()
            ));
        }
        Ok(fids)
    }

    /// Put resolve requests on record, for the next pass to carry out.
    fn request(&mut self, fids: &[String]) -> io::Result<()> {
        let mut st = self.state();
        let mut asked = match st.get("resolve") {
            Some(Value::Object(a)) => a.clone(),
            _ => Map::new(),
        };
        for fid in fids {
            asked.insert(fid.clone(), json!(self.now()));
        }
        st.insert("resolve".into(), Value::Object(asked));
        self.write_state(&st)?;
        for fid in fids {
            self.journal(&[("kind", json!("resolve")), ("fid", json!(fid))])?;
        }
        Ok(())
    }

    /// Bring back a session whose copies were moved to quarantine: clear its deletion markers
    /// in every account (the one it was deleted in included), then put the records back. The
    /// markers go first, so a daemon that looks in between does not take the session for
    /// deleted again. Returns (session id, markers cleared, records put back).
    pub fn restore(&mut self, key: &str) -> io::Result<Option<(String, usize, usize)>> {
        let qroot = self.home("quarantine");
        // fid -> {leaf: newest quarantined file}
        let mut found: BTreeMap<String, BTreeMap<String, (u128, PathBuf)>> = BTreeMap::new();
        let _ = (|| -> io::Result<()> {
            for d in fs::read_dir(&qroot)? {
                let d = d?;
                let dname = d.file_name().to_string_lossy().into_owned();
                for f in fs::read_dir(d.path())? {
                    let f = f?;
                    let fname = f.file_name().to_string_lossy().into_owned();
                    let Some(m) = quarantined_re().captures(&fname) else {
                        continue;
                    };
                    if !matches_key(&m[1], key) {
                        continue;
                    }
                    let when: u128 = m[2].parse().unwrap_or(u128::MAX);
                    let slot = found.entry(m[1].to_string()).or_default();
                    let leaf = dname.replace('_', "/");
                    if slot.get(&leaf).is_none_or(|have| when > have.0) {
                        slot.insert(leaf, (when, f.path()));
                    }
                }
            }
            Ok(())
        })();
        if found.len() != 1 {
            let msg = if found.is_empty() {
                format!("no quarantined session matches '{}'{}", key, too_short(key))
            } else {
                format!(
                    "'{}' matches several sessions: {}",
                    key,
                    found
                        .keys()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            (self.out)(&msg);
            return Ok(None);
        }
        let Some((fid, slots)) = found.into_iter().next() else {
            return Ok(None);
        };
        let mut ids: BTreeSet<String> = BTreeSet::from([fid.clone()]);
        for (_, path) in slots.values() {
            let Ok(data) = self.read_shared(path) else {
                continue;
            };
            // (whatever was moved to quarantine is read for its ids, also a file the app could not read)
            let Some((j, _)) = pyjson::decode_utf8_sig(&data).and_then(|t| pyjson::parse(t).ok())
            else {
                continue;
            };
            if !j.is_object() {
                continue;
            }
            for k in ["cliSessionId", "unarchivedCliSessionId"] {
                if let Some(s) = j.get(k).and_then(J::as_str) {
                    ids.insert(s.to_string());
                }
            }
            for k in ["priorCliSessionIds", "overflowCliSessionIds"] {
                if let Some(list) = j.get(k).and_then(J::as_array) {
                    ids.extend(list.iter().filter_map(J::as_str).map(String::from));
                }
            }
        }
        ids.retain(|i| is_id(i));
        let lv = self.leaves();
        let (mut cleared, mut back) = (0, 0);
        for (_, path) in &lv {
            for i in &ids {
                let marker = path.join(format!("deleted_{}", i));
                if marker.exists() {
                    fs::remove_file(&marker)?;
                    self.forget(&marker);
                    cleared += 1;
                }
            }
        }
        for (leaf, (_, q)) in &slots {
            let Some((_, dir)) = lv.iter().find(|(n, _)| n == leaf) else {
                continue;
            };
            let dst = dir.join(format!("local_{}.json", fid));
            if !dst.exists() {
                winfs::move_file(q, &dst)?;
                back += 1;
            }
        }
        self.save_manifest()?;
        winfs::make_dirs(&self.p.home)?;
        self.journal(&[
            ("kind", json!("restore")),
            ("fid", json!(fid)),
            ("cleared", json!(cleared)),
            ("back", json!(back)),
        ])?;
        self.log(&format!(
            "restore {}: {} marker(s) cleared, put back in {} account folder(s)",
            fid.chars().take(8).collect::<String>(),
            cleared,
            back
        ));
        Ok(Some((fid, cleared, back)))
    }
}
