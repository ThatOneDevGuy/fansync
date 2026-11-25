//! Running passes: once, or continuously with fanotify (an initial full
//! pass, then syncing whatever changes, plus periodic full passes as a
//! safety net).

use crate::Settings;
use crate::engine::{ENTRY, Engine, Outcome, PairState, SELF, Snap, Stats};
use crate::util::{dur, size};
use crate::watch::{self, Ev, Events, MAX_MOVES, MAX_QUEUED, Watcher};
use crate::{log, vlog};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// A big file that took T to copy isn't copied again for COOL_FACTOR * T,
/// so a constantly written VM image or database gets at most a ~10% duty
/// cycle instead of being copied over and over.
const COOL_FACTOR: u32 = 10;
/// Beyond this many queued paths, a full pass is cheaper to track.
const MAX_DIRTY: usize = MAX_QUEUED;

/// rsync's exit code for "error in file I/O": a pass failed as a whole.
const EXIT_FAILED: i32 = 11;

/// One full pass per pair, then exit. Exit codes follow rsync: 11 when a
/// pass failed as a whole, 23 when some files did, 20 when interrupted.
pub fn run_once(eng: &Engine, states: &[PairState], set: &Settings) -> i32 {
    let status = Status::new(set.status_file.clone());
    let mut failed = false;
    for ps in states {
        if eng.stop.load(Relaxed) {
            break;
        }
        let o = status.begin_pass("syncing", &ps.pair.name, eng).and_then(|()| full_pass(eng, ps, set, "", &status));
        if let Some(code) = give_up(&o, set, &status, eng) {
            return code;
        }
        failed |= failure(&o).is_some();
    }
    status.set(&[("state", "done"), ("pair", "")], eng);
    if eng.stop.load(Relaxed) {
        return 20;
    }
    if failed {
        EXIT_FAILED
    } else if eng.stats.snap().errors > 0 {
        23
    } else {
        0
    }
}

/// Why a pass failed as a whole, if it did: it couldn't start (say, the
/// destination is unmounted) or the destination failed during it.
fn failure(o: &Result<Outcome, String>) -> Option<String> {
    match o {
        Err(e) => Some(e.clone()),
        Ok(o) => o.aborted.clone(),
    }
}

/// With --exit-on-error, a pass that failed as a whole ends fansync: the
/// exit code to use then.
fn give_up(o: &Result<Outcome, String>, set: &Settings, status: &Status, eng: &Engine) -> Option<i32> {
    let why = failure(o).filter(|_| set.exit_on_error)?;
    log!("error: stopping, as --exit-on-error asks: {why}");
    status.set(&[("state", "failed"), ("error", &why)], eng);
    Some(EXIT_FAILED)
}

fn full_pass(eng: &Engine, ps: &PairState, set: &Settings, why: &str, status: &Status) -> Result<Outcome, String> {
    let p = &ps.pair;
    if !set.quiet {
        log!("{}: full pass{why}: {} -> {}", p.name, p.src.display(), p.dst.display());
    }
    let before = eng.stats.snap();
    let t0 = Instant::now();
    let done = Done::default();
    let res = std::thread::scope(|s| {
        if set.progress {
            s.spawn(|| progress(&eng.stats, &done, before));
        }
        if status.path.is_some() {
            // Keep the counters in the status file moving during long passes.
            s.spawn(|| {
                while !done.wait(Duration::from_secs(10)) {
                    status.set(&[], eng);
                }
            });
        }
        let r = eng.full_sync(ps);
        done.set();
        r
    });
    release_memory();
    let d = eng.stats.snap().since(&before);
    match res {
        Ok(o) => {
            if !set.quiet || d.errors > 0 {
                log!(
                    "{}: {} in {}: {}",
                    p.name,
                    if eng.stop.load(Relaxed) { "interrupted" } else { "done" },
                    dur(t0.elapsed()),
                    d.summary()
                );
            }
            Ok(o)
        }
        Err(e) => {
            eng.stats.errors.fetch_add(1, Relaxed);
            log!("error: {}: {e}", p.name);
            Err(format!("{}: {e}", p.name))
        }
    }
}

/// Hands memory freed by a pass back to the system. glibc's allocator keeps
/// freed memory in per-thread arenas, so without this fansync would stay as
/// big as its biggest pass (and creep up from there), however little it
/// keeps between passes.
fn release_memory() {
    #[cfg(target_env = "gnu")]
    unsafe {
        libc::malloc_trim(0);
    }
}

/// Lets helper threads sleep until a pass ends.
#[derive(Default)]
struct Done {
    flag: Mutex<bool>,
    cv: Condvar,
}

impl Done {
    fn set(&self) {
        *self.flag.lock().unwrap() = true;
        self.cv.notify_all();
    }

    /// Waits up to `d`; returns whether the pass is over.
    fn wait(&self, d: Duration) -> bool {
        let g = self.flag.lock().unwrap();
        *self.cv.wait_timeout_while(g, d, |done| !*done).unwrap().0
    }
}

fn progress(stats: &Stats, done: &Done, base: Snap) {
    let tty = unsafe { libc::isatty(2) } == 1;
    let every = Duration::from_secs(if tty { 1 } else { 30 });
    let mut last = (Instant::now(), 0u64);
    let mut shown = false;
    while !done.wait(every) {
        let s = stats.snap().since(&base);
        let rate = (s.bytes - last.1) as f64 / last.0.elapsed().as_secs_f64();
        last = (Instant::now(), s.bytes);
        let line = format!(
            "{} checked, {} copied, {} written ({}/s), {} deleted",
            s.scanned,
            s.copied,
            size(s.bytes),
            size(rate as u64),
            s.deleted
        );
        if tty {
            eprint!("\r\x1b[K{line}");
            shown = true;
        } else {
            log!("progress: {line}");
        }
    }
    if shown {
        eprint!("\r\x1b[K");
    }
}

#[derive(Default)]
struct Pending {
    dirty: HashMap<Vec<u8>, u8>,
    /// Inodes of multiply-linked files to sync under all their names.
    links: HashSet<u64>,
    moves: Vec<(Vec<u8>, Vec<u8>)>,
    rescan: bool,
    why: &'static str,
    /// Run as soon as possible instead of waiting for things to settle.
    now: bool,
    first: Option<Instant>,
    last: Option<Instant>,
    /// Paths held back while cooling down: flags and until when.
    deferred: HashMap<Vec<u8>, (u8, Instant)>,
    /// Nothing is attempted before this (after a failure).
    hold: Option<Instant>,
    /// A full pass to re-link newly hard-linked files is due then.
    relink: Option<Instant>,
    /// How long the last full pass took.
    full_took: Duration,
}

impl Pending {
    fn mark(&mut self, t: Instant) {
        self.first.get_or_insert(t);
        self.last = Some(t);
    }

    fn full(&mut self, t: Instant, why: &'static str) {
        self.rescan = true;
        self.now = true;
        self.why = why;
        self.mark(t);
    }

    fn due_at(&self, set: &Settings) -> Option<Instant> {
        if self.dirty.is_empty() && self.moves.is_empty() && self.links.is_empty() && !self.rescan {
            return None;
        }
        let (first, last) = (self.first?, self.last?);
        let t = if self.now { first } else { (last + set.settle).min(first + set.max_delay) };
        Some(self.hold.map_or(t, |h| t.max(h)))
    }
}

/// Initial full pass, then watch.
pub fn run(eng: &Engine, states: &[PairState], set: &Settings) -> i32 {
    let n = states.len();
    let status = Status::new(set.status_file.clone());
    // Watching starts before the initial pass, so nothing that changes
    // during it is missed.
    let mut watched = vec![false; n];
    let mut rx: Option<Events> = None;
    if !set.no_watch {
        let srcs: Vec<(usize, PathBuf)> = states.iter().enumerate().map(|(i, s)| (i, s.pair.src.clone())).collect();
        match Watcher::new(&srcs, n) {
            Ok(w) => {
                watched = w.watched.clone();
                let (sink, r) = watch::queue();
                let stop = eng.stop.clone();
                std::thread::Builder::new()
                    .name("fanotify".into())
                    .spawn(move || w.run(sink, stop))
                    .expect("spawning thread");
                rx = Some(r);
            }
            Err(e) => log!("warning: can't use fanotify ({e}); doing a full pass every {} instead", dur(set.interval)),
        }
    }

    let mut pend: Vec<Pending> = (0..n).map(|_| Pending::default()).collect();
    let mut cool: Vec<HashMap<Vec<u8>, Instant>> = vec![HashMap::new(); n];
    for (i, ps) in states.iter().enumerate() {
        if eng.stop.load(Relaxed) {
            return 0;
        }
        let t0 = Instant::now();
        let o =
            status.begin_pass("initial-sync", &ps.pair.name, eng).and_then(|()| full_pass(eng, ps, set, "", &status));
        pend[i].full_took = t0.elapsed();
        if let Some(code) = give_up(&o, set, &status, eng) {
            return code;
        }
        after_pass(&ps.pair.name, &mut pend[i], o, set, &cool[i]);
    }
    status.set(&[("state", "watching"), ("pair", ""), ("last_full_pass", &epoch())], eng);

    let start = Instant::now();
    let mut next_rescan = set.rescan.map(|r| start + r);
    let mut next_poll: Vec<Option<Instant>> = watched.iter().map(|&w| (!w).then(|| start + set.interval)).collect();
    if !set.quiet {
        let w = watched.iter().filter(|&&w| w).count();
        log!("watching for changes ({w} of {n} pairs through fanotify)");
    }

    while !eng.stop.load(Relaxed) {
        let now = Instant::now();
        let mut wake = now + Duration::from_secs(1);
        for (i, p) in pend.iter().enumerate() {
            wake = [p.due_at(set), next_poll[i], p.relink, p.deferred.values().map(|d| d.1).min()]
                .into_iter()
                .flatten()
                .fold(wake, Instant::min);
        }
        if let Some(t) = next_rescan {
            wake = wake.min(t);
        }
        let timeout = wake.saturating_duration_since(now);
        let mut batches = Vec::new();
        match &rx {
            Some(r) => match r.recv_timeout(timeout) {
                Ok(b) => batches.push(b),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    rx = None;
                    if eng.stop.load(Relaxed) {
                        break;
                    }
                    log!("warning: the event watcher stopped; falling back to a full pass every {}", dur(set.interval));
                    // Events may have been lost on the way (as with an
                    // overflow), so start with a full pass.
                    let now = Instant::now();
                    for i in 0..n {
                        if watched[i] {
                            watched[i] = false;
                            pend[i].full(now, " (event watcher stopped)");
                            next_poll[i] = Some(now + set.interval);
                        }
                    }
                }
            },
            None => std::thread::sleep(timeout),
        }
        let now = Instant::now();
        for evs in batches {
            absorb(evs, &mut pend, &cool, &watched, states, now);
        }
        for p in pend.iter_mut() {
            let ready: Vec<Vec<u8>> = p.deferred.iter().filter(|(_, d)| d.1 <= now).map(|(k, _)| k.clone()).collect();
            for k in ready {
                let (f, _) = p.deferred.remove(&k).unwrap();
                *p.dirty.entry(k).or_insert(0) |= f;
                p.mark(now);
            }
        }
        if next_rescan.is_some_and(|t| now >= t) {
            for p in pend.iter_mut() {
                p.full(now, " (periodic)");
            }
            next_rescan = set.rescan.map(|r| now + r);
        }
        for p in pend.iter_mut() {
            if p.relink.is_some_and(|t| now >= t) {
                p.relink = None;
                p.full(now, " (new hard links)");
            }
        }
        for i in 0..n {
            if next_poll[i].is_some_and(|t| now >= t) {
                pend[i].full(now, " (not watched)");
                next_poll[i] = Some(now + set.interval);
            }
        }
        for i in 0..n {
            if eng.stop.load(Relaxed) {
                break;
            }
            if pend[i].due_at(set).is_some_and(|t| t <= Instant::now())
                && let Some(code) = process(eng, &states[i], &mut pend[i], &mut cool[i], set, &status)
            {
                return code;
            }
        }
    }
    status.set(&[("state", "stopped"), ("pair", "")], eng);
    0
}

fn absorb(
    evs: Vec<Ev>,
    pend: &mut [Pending],
    cool: &[HashMap<Vec<u8>, Instant>],
    watched: &[bool],
    states: &[PairState],
    now: Instant,
) {
    for ev in evs {
        match ev {
            Ev::Dirty(i, rel, f) => {
                let p = &mut pend[i];
                if p.rescan {
                    continue;
                }
                // Drop changes inside excluded areas right away (busy caches,
                // /tmp...). Whether it's a directory isn't known yet, so only
                // when excluded either way.
                let filter = &states[i].pair.filter;
                if filter.parent_excluded(&rel) {
                    continue;
                }
                if filter.excluded(&rel, false) && filter.excluded(&rel, true) {
                    // Creating, deleting or renaming an excluded entry still
                    // changes its directory's mtime, which is mirrored.
                    if f & ENTRY == 0 {
                        continue;
                    }
                    let parent = rel[..rel.iter().rposition(|&c| c == b'/').unwrap_or(0)].to_vec();
                    queue(p, &cool[i], parent, SELF, now);
                } else {
                    queue(p, &cool[i], rel, f, now);
                }
                if p.dirty.len() > MAX_DIRTY {
                    log!(
                        "{}: more than {MAX_DIRTY} changed paths queued; doing a full pass instead",
                        states[i].pair.name
                    );
                    p.dirty = HashMap::new();
                    p.moves = Vec::new();
                    p.links = HashSet::new();
                    p.full(now, " (many changes)");
                }
            }
            Ev::Links(i, ino) => {
                let p = &mut pend[i];
                if !p.rescan {
                    p.links.insert(ino);
                    p.mark(now);
                }
            }
            Ev::Move(i, old, new) => {
                let p = &mut pend[i];
                if !p.rescan && p.moves.len() < MAX_MOVES {
                    p.moves.push((old, new));
                }
                p.mark(now);
            }
            Ev::Overflow(why) => {
                log!("warning: {why}; doing a full pass of everything watched");
                for (i, p) in pend.iter_mut().enumerate() {
                    if watched[i] {
                        p.dirty = HashMap::new();
                        p.moves = Vec::new();
                        p.links = HashSet::new();
                        p.full(now, " (event overflow)");
                    }
                }
            }
        }
    }
}

/// Runs what's pending for a pair. Returns an exit code if fansync should
/// stop (--exit-on-error).
fn process(
    eng: &Engine,
    ps: &PairState,
    p: &mut Pending,
    cool: &mut HashMap<Vec<u8>, Instant>,
    set: &Settings,
    status: &Status,
) -> Option<i32> {
    if let Err(why) = status.begin_pass("syncing", &ps.pair.name, eng) {
        // Keep what's pending, and try again later.
        if let Some(code) = give_up(&Err(why), set, status, eng) {
            return Some(code);
        }
        log!("will retry in {}", dur(set.interval));
        p.hold = Some(Instant::now() + set.interval);
        return None;
    }
    let dirty = std::mem::take(&mut p.dirty);
    let moves = std::mem::take(&mut p.moves);
    let links = std::mem::take(&mut p.links);
    let rescan = std::mem::take(&mut p.rescan);
    if rescan {
        p.relink = None;
    }
    let why = p.why;
    (p.first, p.last, p.now, p.hold) = (None, None, false, None);
    let o = if rescan {
        let t0 = Instant::now();
        let o = full_pass(eng, ps, set, why, status);
        p.full_took = t0.elapsed();
        if o.is_ok() {
            status.set(&[("last_full_pass", &epoch())], eng);
        }
        o
    } else {
        let count = dirty.len();
        let before = eng.stats.snap();
        let t0 = Instant::now();
        let res = eng.sync_paths(ps, dirty.into_iter().collect(), moves, links.into_iter().collect());
        if count >= 10_000 {
            release_memory();
        }
        match res {
            Ok(o) => {
                let d = eng.stats.snap().since(&before);
                if d.changes() > 0 || d.errors > 0 {
                    if !set.quiet || d.errors > 0 {
                        log!(
                            "{}: synced {count} changed paths in {}: {}",
                            ps.pair.name,
                            dur(t0.elapsed()),
                            d.summary()
                        );
                    }
                    status.set(&[("last_change", &epoch())], eng);
                } else {
                    vlog!("{}: {count} paths checked, nothing to do", ps.pair.name);
                }
                Ok(o)
            }
            Err(e) => {
                eng.stats.errors.fetch_add(1, Relaxed);
                log!("error: {}: {e}", ps.pair.name);
                Err(format!("{}: {e}", ps.pair.name))
            }
        }
    };
    if let Some(code) = give_up(&o, set, status, eng) {
        return Some(code);
    }
    let now = Instant::now();
    if let Ok(o) = &o {
        for (rel, took) in &o.slow {
            cool.insert(rel.clone(), now + *took * COOL_FACTOR);
        }
    }
    cool.retain(|_, t| *t > now);
    after_pass(&ps.pair.name, p, o, set, cool);
    status.set(&[("state", "watching"), ("pair", ""), ("pending", &p.dirty.len().to_string())], eng);
    None
}

/// Queues a changed path, or holds it back while it is cooling down.
fn queue(p: &mut Pending, cool: &HashMap<Vec<u8>, Instant>, rel: Vec<u8>, f: u8, now: Instant) {
    if let Some(&until) = cool.get(&rel)
        && until > now
    {
        p.deferred.entry(rel).or_insert((0, until)).0 |= f;
        return;
    }
    *p.dirty.entry(rel).or_insert(0) |= f;
    p.mark(now);
}

/// Requeues what a pass couldn't finish.
fn after_pass(
    name: &str,
    p: &mut Pending,
    o: Result<Outcome, String>,
    set: &Settings,
    cool: &HashMap<Vec<u8>, Instant>,
) {
    let now = Instant::now();
    match o {
        Ok(o) if o.aborted.is_none() => {
            // Files that changed while being copied go through the cooldown
            // too, or a constantly written big file would be copied nonstop.
            for rel in o.redo {
                queue(p, cool, rel, SELF, now);
            }
            // Wait a while so a burst of linking (cp -al, package installs)
            // costs one pass, and on a big pair long enough that these
            // passes take at most a tenth of the time, like busy files.
            if o.links_unknown && p.relink.is_none() {
                let wait = set.link_delay.max(p.full_took * COOL_FACTOR);
                vlog!("{name}: full pass for new hard links in {}", dur(wait));
                p.relink = Some(now + wait);
            }
        }
        _ => {
            log!("will retry with a full pass in {}", dur(set.interval));
            p.full(now, " (retry)");
            p.hold = Some(now + set.interval);
        }
    }
}

fn epoch() -> String {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs()).to_string()
}

/// A small file of shell KEY='value' lines for monitoring scripts.
struct Status {
    path: Option<PathBuf>,
    fields: Mutex<BTreeMap<&'static str, String>>,
    passes: std::sync::atomic::AtomicU64,
}

impl Status {
    fn new(path: Option<PathBuf>) -> Status {
        let mut f = BTreeMap::new();
        f.insert("pid", std::process::id().to_string());
        f.insert("started", epoch());
        f.insert("passes", "0".into());
        Status { path, fields: Mutex::new(f), passes: Default::default() }
    }

    /// Before a pass writes anything to `pair`'s destination: sets the state
    /// and counts the pass, so a reader that sees the same `passes` before
    /// and after something knows no pass started in between. A pass that
    /// can't be recorded doesn't start: that would break this promise.
    fn begin_pass(&self, state: &'static str, pair: &str, eng: &Engine) -> Result<(), String> {
        let n = self.passes.fetch_add(1, Relaxed) + 1;
        if self.set(&[("state", state), ("pair", pair), ("passes", &n.to_string())], eng) {
            return Ok(());
        }
        eng.stats.errors.fetch_add(1, Relaxed);
        let why = format!("{pair}: not syncing while the status file can't be written");
        log!("error: {why}");
        Err(why)
    }

    /// Returns false if the status file couldn't be written.
    fn set(&self, kv: &[(&'static str, &str)], eng: &Engine) -> bool {
        let Some(path) = &self.path else { return true };
        let mut f = self.fields.lock().unwrap();
        for (k, v) in kv {
            f.insert(k, v.to_string());
        }
        let s = eng.stats.snap();
        f.insert("updated", epoch());
        f.insert("copied_files", s.copied.to_string());
        f.insert("written_bytes", s.bytes.to_string());
        f.insert("deleted", s.deleted.to_string());
        f.insert("errors", s.errors.to_string());
        let text: String = f.iter().map(|(k, v)| format!("{k}='{}'\n", v.replace('\'', r"'\''"))).collect();
        let tmp = path.with_extension("tmp");
        let write = || {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o644).open(&tmp)?;
            f.write_all(text.as_bytes())?;
            std::fs::rename(&tmp, path)
        };
        if let Err(e) = write() {
            log!("warning: can't write status file {}: {e}", path.display());
            return false;
        }
        true
    }
}
