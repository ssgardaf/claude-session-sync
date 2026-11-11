//! Planning: what should be copied where, and what must be left alone.

use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;

use crate::consts::*;
use crate::records::{copy_in, Rec, Scan, Why};
use crate::sync::{num, short, Homes, Signed, Sync};
use crate::winfs;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Add,
    Update,
    Repair,
    Quarantine,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Add => "add",
            Kind::Update => "update",
            Kind::Repair => "repair",
            Kind::Quarantine => "quarantine",
        }
    }
}

/// One thing to do.
#[derive(Clone, Debug)]
pub struct Act {
    pub kind: Kind,
    pub fid: String,
    pub leaf: String,
    pub leaf_dir: PathBuf,
    /// The record to copy from (not for a quarantine).
    pub src: Option<Rc<Rec>>,
    /// The record that is there now (update, quarantine).
    pub cur: Option<Rc<Rec>>,
    /// The (mtime_ns, size) the destination must still have; None = there must be no file.
    pub expect: Option<(i64, u64)>,
    /// Quarantine: the time of the deletion that is carried over (epoch ms).
    pub ts: Option<i64>,
    /// Quarantine: every id of the session, for its markers.
    pub ids: BTreeSet<String>,
    /// Update: the copy that is there is one this tool placed, so this is the routine refresh
    /// of a session in use (which the daemon makes once a minute), not a change in waiting.
    pub refresh: bool,
}

impl Act {
    /// The record this is about, for display.
    pub fn rec(&self) -> &Rec {
        self.src
            .as_ref()
            .or(self.cur.as_ref())
            .expect("an act has a record")
    }
}

/// What a plan left out, and why.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Notes {
    pub old: usize,
    pub no_transcript: usize,
    /// (leaf, title)
    pub deleted_there: Vec<(String, String)>,
    /// (leaf, title)
    pub deleted_elsewhere: Vec<(String, String)>,
    pub same_conversation: usize,
    pub unreadable_there: usize,
    pub just_removed: usize,
    pub retry_later: usize,
    pub not_settled: usize,
    pub waiting_turn: usize,
    /// (leaf, title)
    pub held: Vec<(String, String)>,
    /// (leaf, title, why, fid)
    pub conflicts: Vec<(String, String, String, String)>,
    pub unlisted: Vec<String>,
    pub everywhere: usize,
    pub next_refresh: Option<f64>,
}

pub struct Plan {
    pub scan: Rc<Scan>,
    pub acts: Vec<Act>,
    pub notes: Notes,
}

type Rank = (f64, bool, i64);

fn rank_cmp(a: &Rank, b: &Rank) -> Ordering {
    a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2))
}

struct Gate<'a> {
    hold: &'a BTreeSet<String>,
    now: f64,
    wanted: HashSet<(String, String)>,
}

impl Sync {
    /// Most activity first; for the same activity the app's own file before our copy of it,
    /// then the newer file.
    fn rank(&mut self, r: &Rec) -> Rank {
        (r.last, !self.rec_is_ours(r), r.mtime)
    }

    /// Our copies carry the exact file time of what they were copied from.
    fn same_version(a: &Rec, b: &Rec) -> bool {
        a.size == b.size && a.mtime == b.mtime && a.last == b.last
    }

    fn same_bytes(&self, a: &Rec, b: &Rec) -> bool {
        if a.size != b.size {
            return false;
        }
        match (self.read_shared(&a.path), self.read_shared(&b.path)) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    /// Why `cur` (a copy that would be replaced by `best`) must stay as it is, or None if it may go.
    fn in_doubt(&mut self, best: &Rec, cur: &Rec) -> Option<&'static str> {
        if self.rec_is_ours(cur) {
            return None; // our own copy of an earlier version: nothing unique in it
        }
        if best.last == cur.last {
            // same activity: only a provable descendant may replace it
            let b = self.base(&best.path);
            if self.is_version(b.as_deref(), &cur.path, cur.mtime, cur.size)
                || self.same_bytes(best, cur)
            {
                return None;
            }
            return Some("same activity, different content");
        }
        let touched = (cur.mtime as f64 / 1e6)
            .max(cur.focus)
            .min(self.now() * 1000.0);
        if touched <= best.last {
            return None; // untouched since the other copy moved on
        }
        if best.user == cur.user {
            return None; // nothing the user decides differs: only the progress does
        }
        if let Some(b) = self.base(&cur.path) {
            if let Some(based_on) = num(b.get(2)) {
                if based_on > cur.last {
                    return None; // the app wrote an older state back over our newer copy: stale memory
                }
                if based_on == cur.last
                    && b.get(4).and_then(|k| k.as_str()) == Some(Self::user_key(&cur.user).as_str())
                {
                    // our earlier copy, merely saved again: archived state and title as we left them
                    return None;
                }
            }
        }
        Some("opened or saved here after the other copy's last activity; archived state or title differ")
    }

    /// Remember records that disappeared since the last scan without a deletion marker: the app
    /// may be in the middle of deleting them, so they are not put back for a few minutes.
    fn note_vanished(&mut self, scan: &Scan) -> HashMap<String, HashSet<String>> {
        let now = self.now();
        let mut cur: HashMap<String, HashSet<String>> = HashMap::new();
        for (name, _) in &scan.lv {
            cur.insert(name.clone(), scan.bad[name].keys().cloned().collect());
        }
        for (fid, copies) in &scan.records {
            for (name, _) in copies {
                if let Some(set) = cur.get_mut(name) {
                    set.insert(fid.clone());
                }
            }
        }
        if self.prev.is_none() {
            // The first look of this process. What an earlier process saw vanish is still to be
            // respected for the rest of its five minutes...
            let noted = self.noted_vanished();
            for (name, fid, t) in &noted {
                self.vanished
                    .entry((name.clone(), fid.clone()))
                    .or_insert(*t);
            }
            // ...and a copy this tool placed that is gone now, with no marker beside it, may be in
            // the middle of being deleted for all this process knows: it has just vanished.
            for (name, path) in &scan.lv {
                for fid in &scan.order {
                    if cur[name].contains(fid) || scan.tombs[name].contains_key(fid) {
                        continue;
                    }
                    if noted.iter().any(|n| n.0 == *name && n.1 == *fid) {
                        continue; // (seen before: its time runs from then, and may be up)
                    }
                    let file = path.join(format!("local_{}.json", fid));
                    if self.manifest.contains_key(&winfs::norm(&file)) {
                        self.vanished
                            .entry((name.clone(), fid.clone()))
                            .or_insert(now);
                    }
                }
            }
        }
        if let Some(prev) = &self.prev {
            for (name, fids) in prev {
                let Some(here) = cur.get(name) else {
                    continue;
                };
                for fid in fids.difference(here) {
                    if !scan.tombs[name].contains_key(fid) {
                        self.vanished
                            .entry((name.clone(), fid.clone()))
                            .or_insert(now);
                    }
                }
            }
        }
        self.vanished.retain(|(name, fid), t| {
            let fresh = (0.0..VANISH_GRACE_SEC).contains(&(now - *t));
            fresh
                && cur.get(name).is_some_and(|here| !here.contains(fid))
                && !scan.tombs[name].contains_key(fid)
        });
        self.prev = Some(cur.clone());
        cur
    }

    /// (folder, age in seconds) of the most recent file this tool did not write.
    fn last_app_write(&mut self, scan: &Scan) -> (Option<String>, f64) {
        let mut best: (i64, Option<String>) = (0, None);
        for fid in &scan.order {
            for (name, r) in &scan.records[fid] {
                if r.mtime > best.0 && !self.rec_is_ours(r) {
                    best = (r.mtime, Some(name.clone()));
                }
            }
        }
        for (name, path) in &scan.lv {
            for id in scan.tombs[name].keys() {
                let marker = path.join(format!("deleted_{}", id));
                let Ok(st) = winfs::stat(&marker) else {
                    continue;
                };
                if st.mtime_ns > best.0 && !self.is_ours(&marker, &st) {
                    best = (st.mtime_ns, Some(name.clone()));
                }
            }
        }
        (best.1, self.now() - best.0 as f64 / 1e9)
    }

    /// The folders whose existing records must be left alone right now.
    pub(crate) fn in_use(&mut self, scan: &Scan) -> BTreeSet<String> {
        if !self.app_running() {
            return BTreeSet::new();
        }
        match self.signed_in() {
            Signed::In(leaf) => BTreeSet::from([leaf]),
            // signed out: nothing is loaded, the best moment to bring every folder up to date
            Signed::Out => BTreeSet::new(),
            Signed::Unknown => {
                let (name, age) = self.last_app_write(scan);
                match name {
                    Some(name) if (0.0..=IN_USE_SEC).contains(&age) => BTreeSet::from([name]),
                    // running, but no telling where: touch no existing record anywhere
                    _ => scan.lv.iter().map(|(n, _)| n.clone()).collect(),
                }
            }
        }
    }

    /// Asked again just before an existing record is changed: the answer at planning time may
    /// be seconds old.
    pub(crate) fn in_use_now(&mut self, leaf: &str) -> bool {
        let Some(scan) = self.scan_cache.clone() else {
            return false;
        };
        if !(0.0..1.0).contains(&(self.now() - self.hold.0)) {
            let hold = self.in_use(&scan);
            self.hold = (self.now(), hold);
        }
        self.hold.1.contains(leaf)
    }

    /// May this be done now? (not an existing record in the folder the app has loaded, not backing off)
    fn go(
        &mut self,
        gate: &mut Gate,
        notes: &mut Notes,
        kind: Kind,
        fid: &str,
        name: &str,
        title: &str,
    ) -> bool {
        let key = (name.to_string(), fid.to_string());
        if kind != Kind::Add && gate.hold.contains(name) {
            notes.held.push((name.to_string(), title.to_string()));
            self.retry.remove(&key);
            return false;
        }
        let backing_off = self
            .retry
            .get(&key)
            .is_some_and(|slot| 0.0 <= slot.1 - gate.now);
        gate.wanted.insert(key);
        if backing_off {
            notes.retry_later += 1;
            return false;
        }
        true
    }

    /// Sessions the user asked to settle with `resolve`. A request stays until it has been
    /// carried out (it may have to wait for the app to leave the account) or there is no
    /// longer anything to settle.
    pub(crate) fn overrides(&self) -> BTreeSet<String> {
        match self.state().get("resolve") {
            Some(serde_json::Value::Object(asked)) => asked.keys().cloned().collect(),
            _ => BTreeSet::new(),
        }
    }

    pub fn plan(&mut self) -> Plan {
        if let Some(hook) = self.hooks.before_plan.as_mut() {
            hook();
        }
        let scan = Rc::new(self.scan());
        self.seen = (scan.lv.len(), scan.records.len());
        let occupied = self.note_vanished(&scan);
        self.scan_cache = Some(scan.clone());
        let have = self.transcripts(false);
        let mut fresh: Option<Rc<HashSet<String>>> = None;
        let now = self.now();
        let cutoff = (now - self.days * 86400.0) * 1000.0;
        let (since, blackouts) = self.deletion_window();
        let exempt = self.exemptions();
        let hold = self.in_use(&scan);
        self.hold = (now, hold.clone());
        let homes = self.homes();
        let overrides = self.overrides();
        self.plan_requests = overrides.clone();
        self.used = BTreeSet::new();
        let mut acts: Vec<Act> = Vec::new();
        let mut gate = Gate {
            hold: &hold,
            now,
            wanted: HashSet::new(),
        };
        let mut notes = Notes {
            unlisted: scan.unlisted.iter().map(|n| short(n)).collect(),
            ..Notes::default()
        };
        // every id each leaf already holds, or is about to
        let mut leaf_ids: HashMap<String, HashSet<String>> = scan
            .lv
            .iter()
            .map(|(n, _)| (n.clone(), HashSet::new()))
            .collect();
        for (fid, copies) in &scan.records {
            for (name, r) in copies {
                if let Some(set) = leaf_ids.get_mut(name) {
                    set.insert(fid.clone());
                    set.extend(r.ids.iter().cloned());
                }
            }
        }
        // a folder the app passes through while switching is given nothing
        let is_home = |name: &str| -> bool {
            match &homes {
                Homes::All => true,
                Homes::Only(set) => {
                    set.contains(name) || !occupied[name].is_empty() || !scan.tombs[name].is_empty()
                }
            }
        };

        // most advanced sessions first, so that of two records for one conversation the better one is placed
        let mut order: Vec<(Rank, &String)> = Vec::with_capacity(scan.order.len());
        for fid in &scan.order {
            let mut top: Option<Rank> = None;
            for (_, r) in &scan.records[fid] {
                let k = self.rank(r);
                if top
                    .as_ref()
                    .is_none_or(|t| rank_cmp(&k, t) == Ordering::Greater)
                {
                    top = Some(k);
                }
            }
            order.push((top.expect("a session has at least one record"), fid));
        }
        order.sort_by(|a, b| rank_cmp(&b.0, &a.0));

        for (_, fid) in order {
            let copies = &scan.records[fid];
            let mut ids: BTreeSet<String> = BTreeSet::from([fid.clone()]);
            for (_, r) in copies {
                ids.extend(r.ids.iter().cloned());
            }
            // 1) deleted somewhere: its own file-id marker in a folder that no longer holds it
            let stones: Vec<i64> = scan
                .lv
                .iter()
                .filter(|(name, _)| copy_in(copies, name).is_none())
                .filter_map(|(name, _)| scan.tombs[name].get(fid).copied())
                .collect();
            let before = |limit: Option<i64>| {
                limit.is_some_and(|m| copies.iter().all(|(_, r)| r.last <= m as f64))
            };
            // ...after its last use anywhere
            let dead = before(stones.iter().max().copied());
            if dead && self.delete_sync {
                if let Some(since) = since {
                    let carried: Vec<i64> = stones
                        .iter()
                        .copied()
                        .filter(|&ts| {
                            ts as f64 >= since
                                && !blackouts
                                    .iter()
                                    .any(|b| b.0 <= ts as f64 && ts as f64 <= b.1)
                                // (a deletion that was carried over and undone is not carried again)
                                && !exempt.get(fid).is_some_and(|until| ts as f64 <= *until)
                        })
                        .collect();
                    let latest = carried.iter().max().copied();
                    if before(latest) {
                        for (name, r) in copies {
                            if self.go(&mut gate, &mut notes, Kind::Quarantine, fid, name, &r.title)
                            {
                                acts.push(Act {
                                    kind: Kind::Quarantine,
                                    fid: fid.clone(),
                                    leaf: name.clone(),
                                    leaf_dir: scan.dir(name).cloned().unwrap_or_default(),
                                    src: None,
                                    cur: Some(r.clone()),
                                    expect: Some((r.mtime, r.size)),
                                    ts: latest,
                                    ids: ids.clone(),
                                    refresh: false,
                                });
                            }
                        }
                        continue;
                    }
                }
            }
            // 2) the most advanced copy goes everywhere it should be
            let mut best_at = 0;
            let mut best_rank = self.rank(&copies[0].1);
            for (i, (_, r)) in copies.iter().enumerate().skip(1) {
                let k = self.rank(r);
                if rank_cmp(&k, &best_rank) == Ordering::Greater {
                    best_at = i;
                    best_rank = k;
                }
            }
            let (best_leaf, best) = (&copies[best_at].0, copies[best_at].1.clone());
            let mut complete = true;
            for (name, path) in &scan.lv {
                if name == best_leaf {
                    continue;
                }
                if let Some(cur) = copy_in(copies, name) {
                    if cur.last > best.last || Self::same_version(&best, cur) {
                        continue;
                    }
                    let mut doubt = self.in_doubt(&best, cur);
                    if doubt.is_some() && overrides.contains(fid) {
                        // the user asked for this one to be settled
                        self.used.insert(fid.clone());
                        doubt = None;
                    }
                    if let Some(why) = doubt {
                        // reported even where nothing could be changed right now
                        notes.conflicts.push((
                            name.clone(),
                            cur.title.clone(),
                            why.to_string(),
                            fid.clone(),
                        ));
                    } else if self.go(&mut gate, &mut notes, Kind::Update, fid, name, &cur.title) {
                        let pushed = self.pushed.get(fid).copied().unwrap_or(0.0);
                        let ours = self.rec_is_ours(cur);
                        if !self.urgent && ours && (0.0..REFRESH_SEC).contains(&(now - pushed)) {
                            // a busy session: our copies follow it once a minute, not every save
                            notes.waiting_turn += 1;
                            let due = pushed + REFRESH_SEC;
                            notes.next_refresh =
                                Some(notes.next_refresh.map_or(due, |n| n.min(due)));
                        } else {
                            acts.push(Act {
                                kind: Kind::Update,
                                fid: fid.clone(),
                                leaf: name.clone(),
                                leaf_dir: path.clone(),
                                src: Some(best.clone()),
                                cur: Some(cur.clone()),
                                expect: Some((cur.mtime, cur.size)),
                                ts: None,
                                ids: BTreeSet::new(),
                                refresh: ours,
                            });
                        }
                    }
                    continue;
                }
                let there = scan.bad[name].get(fid);
                let mut placed = false;
                if ids.iter().any(|i| scan.tombs[name].contains_key(i)) {
                    notes.deleted_there.push((name.clone(), best.title.clone()));
                } else if there.is_some_and(|t| t.0 != Why::Torn) {
                    notes.unreadable_there += 1;
                } else if self.vanished.contains_key(&(name.clone(), fid.clone())) {
                    notes.just_removed += 1;
                } else if let Some(there) = there {
                    // a broken file sits there: mend it whatever its age
                    if self.go(&mut gate, &mut notes, Kind::Repair, fid, name, &best.title) {
                        acts.push(Act {
                            kind: Kind::Repair,
                            fid: fid.clone(),
                            leaf: name.clone(),
                            leaf_dir: path.clone(),
                            src: Some(best.clone()),
                            cur: None,
                            expect: Some((there.1.unwrap_or(0), there.2.unwrap_or(0))),
                            ts: None,
                            ids: BTreeSet::new(),
                            refresh: false,
                        });
                        placed = true;
                    }
                } else if dead {
                    notes
                        .deleted_elsewhere
                        .push((name.clone(), best.title.clone()));
                } else if !is_home(name) {
                    notes.not_settled += 1;
                    placed = true; // not a place the user works in: nothing is missing there
                } else if best.archived && best.last < cutoff {
                    notes.old += 1; // the age limit is for the archive only
                } else if !have.contains(&best.cli) && {
                    // a brand-new session may postdate the cached listing: look once more
                    if fresh.is_none() {
                        fresh = Some(self.transcripts(true));
                    }
                    !fresh.as_ref().is_some_and(|f| f.contains(&best.cli))
                } {
                    notes.no_transcript += 1;
                } else if best.ids.iter().any(|i| leaf_ids[name].contains(i)) {
                    notes.same_conversation += 1;
                } else if self.go(&mut gate, &mut notes, Kind::Add, fid, name, &best.title) {
                    acts.push(Act {
                        kind: Kind::Add,
                        fid: fid.clone(),
                        leaf: name.clone(),
                        leaf_dir: path.clone(),
                        src: Some(best.clone()),
                        cur: None,
                        expect: None,
                        ts: None,
                        ids: BTreeSet::new(),
                        refresh: false,
                    });
                    if let Some(set) = leaf_ids.get_mut(name) {
                        set.insert(fid.clone());
                        set.extend(best.ids.iter().cloned());
                    }
                    placed = true;
                }
                complete = complete && placed;
            }
            notes.everywhere += complete as usize;
        }
        // no longer something we want to do: forget its failures
        let wanted = gate.wanted;
        self.retry.retain(|key, _| wanted.contains(key));
        self.waiting.retain(|fid, _| scan.records.contains_key(fid));
        self.notes = notes.clone();
        Plan { scan, acts, notes }
    }
}
