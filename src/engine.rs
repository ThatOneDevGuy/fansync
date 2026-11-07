//! The sync engine: makes a destination tree match a source tree.
//!
//! It uses rsync's ideas: a file whose size and mtime match is
//! assumed unchanged, extraneous entries are deleted before anything new is
//! written in a directory, excluded destination entries are never deleted,
//! and replaced files are written aside and renamed into place. On top of
//! that, the tree walk runs in parallel and works through directory file
//! descriptors, data is copied inside the kernel (copy_file_range, or
//! sendfile across filesystems), holes are found with SEEK_DATA/SEEK_HOLE
//! instead of by scanning for zeros, and large files are updated in place by
//! writing only the blocks that differ.

use crate::filter::Filter;
use crate::sys::{self, DT_DIR, DT_UNKNOWN, Stat, Xt};
use crate::util::{show, size, verbose};
use crate::{dlog, log, vlog};
use rayon::prelude::*;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Incremental-sync flag: the entry itself changed.
pub const SELF: u8 = 1;
/// Incremental-sync flag: sync everything below this directory too (it was
/// just created or moved in).
pub const REC: u8 = 2;
/// Internal: already the result of a missing destination parent.
const ESCALATED: u8 = 4;

/// Copy chunk per syscall; also how often a copy checks for Ctrl-C.
const CHUNK: u64 = 64 << 20;
/// In-place updates read this much of both files at a time...
const CMP_CHUNK: usize = 2 << 20;
/// ...and compare and rewrite them in blocks of this size.
const CMP_BLOCK: usize = 32 << 10;
/// Copies taking longer than this are reported back so busy files can be
/// rate-limited.
const SLOW: Duration = Duration::from_secs(2);
/// Files at least this big get their destination pages dropped from the
/// page cache once written.
const BIG: u64 = 32 << 20;

const MODE_CFR: u8 = 0;
const MODE_SENDFILE: u8 = 1;
const MODE_RW: u8 = 2;

pub struct Opts {
    pub delete: bool,
    pub dry_run: bool,
    pub itemize: bool,
    pub hard_links: bool,
    pub xattrs: bool,
    pub sparse: bool,
    pub owner: bool,
    /// Update existing files at least this big in place (0 = never).
    pub inplace_min: u64,
    pub drop_cache: bool,
}

macro_rules! stats {
    ($($f:ident),*) => {
        #[derive(Default)]
        pub struct Stats { $(pub $f: AtomicU64,)* }
        #[derive(Clone, Copy, Default, Debug)]
        pub struct Snap { $(pub $f: u64,)* }
        impl Stats {
            pub fn snap(&self) -> Snap { Snap { $($f: self.$f.load(Relaxed),)* } }
        }
        impl Snap {
            pub fn since(&self, b: &Snap) -> Snap { Snap { $($f: self.$f - b.$f,)* } }
        }
    };
}
stats!(scanned, copied, bytes, created, deleted, meta, linked, moved, vanished, errors);

impl Snap {
    pub fn changes(&self) -> u64 {
        self.copied + self.created + self.deleted + self.meta + self.linked + self.moved
    }

    pub fn summary(&self) -> String {
        let mut s = vec![format!("{} checked", self.scanned)];
        if self.copied > 0 {
            s.push(format!("{} copied ({} written)", self.copied, size(self.bytes)));
        }
        for (n, what) in [
            (self.created, "created"),
            (self.deleted, "deleted"),
            (self.meta, "metadata updated"),
            (self.linked, "hard-linked"),
            (self.moved, "moved"),
            (self.vanished, "vanished"),
            (self.errors, "errors"),
        ] {
            if n > 0 {
                s.push(format!("{n} {what}"));
            }
        }
        s.join(", ")
    }
}

pub struct Pair {
    pub name: String,
    /// The directory whose contents are copied.
    pub src: PathBuf,
    pub dst: PathBuf,
    pub filter: Filter,
}

/// Multiply-linked files met during a pass: (dev, ino) -> (paths, link count).
type Links = HashMap<(u64, u64), (Vec<Box<[u8]>>, u64)>;

/// A hard-link group being synced: inode, paths, link count.
type GroupWork = ((u64, u64), Vec<Box<[u8]>>, u64);

/// The multiply-linked source files known so far, by inode and by name.
#[derive(Default)]
struct LinkMap {
    groups: HashMap<(u64, u64), Group>,
    /// Every name in `groups`, keyed so that a directory's contents form one
    /// range (see `tree_key`).
    by_path: BTreeMap<Vec<u8>, (u64, u64)>,
}

/// A path with "/" turned into NUL, so names inside a directory sort right
/// after it and before any sibling.
fn tree_key(rel: &[u8]) -> Vec<u8> {
    rel.iter().map(|&c| if c == b'/' { 0 } else { c }).collect()
}

impl LinkMap {
    fn set(&mut self, k: (u64, u64), g: Group) {
        self.remove(k);
        for p in &g.paths {
            self.by_path.insert(tree_key(p), k);
        }
        self.groups.insert(k, g);
    }

    fn remove(&mut self, k: (u64, u64)) {
        if let Some(old) = self.groups.remove(&k) {
            for p in &old.paths {
                let t = tree_key(p);
                if self.by_path.get(&t) == Some(&k) {
                    self.by_path.remove(&t);
                }
            }
        }
    }

    /// Groups with a name at `rel` or anywhere below it.
    fn groups_at(&self, rel: &[u8]) -> Vec<(u64, u64)> {
        let t = tree_key(rel);
        let mut out: Vec<(u64, u64)> = self.by_path.get(&t).copied().into_iter().collect();
        let (mut lo, mut hi) = (t.clone(), t);
        lo.push(0);
        hi.push(1);
        out.extend(self.by_path.range(lo..hi).map(|(_, &k)| k));
        out.sort_unstable();
        out.dedup();
        out
    }
}

const INO_SHARDS: usize = 64;

/// Interned directory paths: id -> path, and path -> id.
type DirTable = (Vec<Arc<[u8]>>, HashMap<Arc<[u8]>, u32>);

/// The directory of every single-linked source file. Hard links are just
/// equal names for one inode, and nothing records an inode's names, so when
/// a file gains a second name while watching, this is how its first one is
/// found: look up the directory, then find the entry with that inode number
/// in it. Only single-linked files are recorded, so a new name never hides
/// the old one; multiply-linked files are tracked by name in `hardlinks`.
struct InoIndex {
    /// Bumped on clear(), so per-thread caches of directory ids go stale.
    generation: AtomicU64,
    /// Directory paths (relative to the source), interned.
    dirs: Mutex<DirTable>,
    inos: Vec<Mutex<HashMap<u64, u32>>>,
}

thread_local! {
    /// The last directory interned by this thread: (index, generation, path, id).
    static LAST_DIR: RefCell<(usize, u64, Vec<u8>, u32)> = const { RefCell::new((0, 0, Vec::new(), 0)) };
}

impl InoIndex {
    fn new() -> InoIndex {
        InoIndex {
            generation: AtomicU64::new(1),
            dirs: Mutex::new((Vec::new(), HashMap::new())),
            inos: (0..INO_SHARDS).map(|_| Mutex::new(HashMap::new())).collect(),
        }
    }

    fn dir_id(&self, dir: &[u8]) -> u32 {
        let me = self as *const InoIndex as usize;
        let generation = self.generation.load(Relaxed);
        LAST_DIR.with(|last| {
            let mut last = last.borrow_mut();
            // Files of one directory mostly arrive in a row on one thread.
            if last.0 == me && last.1 == generation && last.2 == dir {
                return last.3;
            }
            let mut d = self.dirs.lock().unwrap();
            let id = match d.1.get(dir) {
                Some(&id) => id,
                None => {
                    let path: Arc<[u8]> = dir.into();
                    let id = d.0.len() as u32;
                    d.0.push(path.clone());
                    d.1.insert(path, id);
                    id
                }
            };
            *last = (me, generation, dir.to_vec(), id);
            id
        })
    }

    fn record(&self, ino: u64, dir: &[u8]) {
        let id = self.dir_id(dir);
        self.inos[ino as usize % INO_SHARDS].lock().unwrap().insert(ino, id);
    }

    fn dir_of(&self, ino: u64) -> Option<Arc<[u8]>> {
        let id = *self.inos[ino as usize % INO_SHARDS].lock().unwrap().get(&ino)?;
        self.dirs.lock().unwrap().0.get(id as usize).cloned()
    }

    /// Done at the start of each full pass, which records everything again;
    /// this drops entries for deleted files.
    fn clear(&self) {
        self.generation.fetch_add(1, Relaxed);
        *self.dirs.lock().unwrap() = (Vec::new(), HashMap::new());
        for shard in &self.inos {
            *shard.lock().unwrap() = HashMap::new();
        }
    }
}

/// What is known about one multiply-linked source file.
struct Group {
    paths: Vec<Box<[u8]>>,
    /// Its link count when last seen. Links outside the pair are never
    /// found, so only an increase means an unknown new link.
    nlink: u64,
}

/// A pair plus what fansync has learned about it while running.
pub struct PairState {
    pub pair: Pair,
    src_dev: AtomicU64,
    dst_dev: AtomicU64,
    copy_mode: AtomicU8,
    tmpfile: AtomicBool,
    /// Source files with several links: (dev, ino) -> paths seen.
    hardlinks: Mutex<LinkMap>,
    inodes: InoIndex,
}

impl PairState {
    pub fn new(pair: Pair) -> PairState {
        PairState {
            pair,
            src_dev: AtomicU64::new(0),
            dst_dev: AtomicU64::new(0),
            copy_mode: AtomicU8::new(MODE_CFR),
            tmpfile: AtomicBool::new(true),
            hardlinks: Mutex::new(LinkMap::default()),
            inodes: InoIndex::new(),
        }
    }
}

pub struct Engine {
    pub opts: Opts,
    pub stats: Stats,
    pub stop: Arc<AtomicBool>,
    warned: Mutex<HashSet<String>>,
}

#[derive(Default)]
pub struct Outcome {
    /// Files whose copy took a while, and how long.
    pub slow: Vec<(Vec<u8>, Duration)>,
    /// Files that changed while being copied.
    pub redo: Vec<Vec<u8>>,
    /// The pass stopped early on a destination error such as a full disk.
    pub aborted: bool,
    /// A file gained a hard link whose other names aren't known; a full
    /// pass is needed to link them in the destination.
    pub links_unknown: bool,
}

impl Engine {
    pub fn new(opts: Opts, stop: Arc<AtomicBool>) -> Engine {
        Engine { opts, stats: Stats::default(), stop, warned: Mutex::new(HashSet::new()) }
    }

    fn warn_once(&self, key: String, msg: impl FnOnce() -> String) {
        if self.warned.lock().unwrap().insert(key) {
            log!("warning: {}", msg());
        }
    }

    /// Walks the whole pair and fixes every difference.
    pub fn full_sync(&self, ps: &PairState) -> io::Result<Outcome> {
        let run = Run::new(self, ps, true)?;
        // An empty source next to a full destination is almost always a
        // source that isn't mounted; mirroring it would delete the backup.
        if self.opts.delete
            && let Some(d) = &run.dst_root
            && sys::read_dir(run.src_root.as_raw_fd())?.is_empty()
            && !sys::read_dir(d.as_raw_fd())?.is_empty()
        {
            return Err(io::Error::other(format!(
                "{} is empty but {} isn't; not deleting everything (is the source mounted?)",
                ps.pair.src.display(),
                ps.pair.dst.display()
            )));
        }
        if self.opts.hard_links && !self.opts.dry_run {
            ps.inodes.clear();
        }
        run.sync_root(true);
        run.finish_hardlinks();
        run.restore_dir_times();
        Ok(run.outcome())
    }

    /// Syncs only the given entries (paths relative to the pair's source,
    /// with SELF/REC flags), after first mirroring renames so moved data
    /// isn't copied again.
    pub fn sync_paths(
        &self,
        ps: &PairState,
        entries: Vec<(Vec<u8>, u8)>,
        moves: Vec<(Vec<u8>, Vec<u8>)>,
        links: Vec<u64>,
    ) -> io::Result<Outcome> {
        let run = Run::new(self, ps, false)?;
        if run.dst_root.is_none() {
            return Ok(run.outcome());
        }
        run.apply_moves(&moves);
        collapse(entries).par_iter().for_each(|(rel, f)| run.sync_path(rel, *f));
        run.add_link_groups(&links);
        run.drain_escalations();
        run.finish_hardlinks();
        run.drain_escalations();
        run.finish_hardlinks();
        run.restore_dir_times();
        Ok(run.outcome())
    }
}

/// Sorts paths so each directory comes right before its contents, merges
/// duplicates, and drops entries inside a directory that is synced
/// recursively anyway.
pub fn collapse(mut v: Vec<(Vec<u8>, u8)>) -> Vec<(Vec<u8>, u8)> {
    v.sort_by(|a, b| cmp_path(&a.0, &b.0));
    let mut out: Vec<(Vec<u8>, u8)> = Vec::with_capacity(v.len());
    let mut cover: Option<Vec<u8>> = None;
    for (rel, f) in v {
        if let Some(c) = &cover
            && (c.is_empty() || (rel.starts_with(c) && rel.get(c.len()) == Some(&b'/')))
        {
            continue;
        }
        if f & REC != 0 {
            cover = Some(rel.clone());
        }
        if let Some(last) = out.last_mut()
            && last.0 == rel
        {
            last.1 |= f;
            continue;
        }
        out.push((rel, f));
    }
    out
}

/// Path order where "/" sorts before every other byte, so "a/b" comes
/// between "a" and "a-b".
fn cmp_path(a: &[u8], b: &[u8]) -> Ordering {
    let key = |c: &u8| if *c == b'/' { 0 } else { *c };
    a.iter().map(key).cmp(b.iter().map(key))
}

fn join(rel: &[u8], name: &[u8]) -> Vec<u8> {
    if rel.is_empty() {
        return name.to_vec();
    }
    let mut v = Vec::with_capacity(rel.len() + 1 + name.len());
    v.extend_from_slice(rel);
    v.push(b'/');
    v.extend_from_slice(name);
    v
}

fn split(rel: &[u8]) -> (&[u8], &[u8]) {
    match rel.iter().rposition(|&c| c == b'/') {
        Some(i) => (&rel[..i], &rel[i + 1..]),
        None => (b"", rel),
    }
}

const TMP_PREFIX: &str = ".fansync-tmp.";
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn tmp_name() -> CString {
    CString::new(format!("{TMP_PREFIX}{}.{}", std::process::id(), TMP_SEQ.fetch_add(1, Relaxed))).unwrap()
}

fn is_temp(name: &CStr) -> bool {
    name.to_bytes().starts_with(TMP_PREFIX.as_bytes())
}

fn is_acl(name: &[u8]) -> bool {
    name.starts_with(b"system.posix_acl_")
}

fn unsupported(e: &io::Error) -> bool {
    matches!(sys::errno(e), libc::EOPNOTSUPP | libc::ENOSYS)
}

/// rsync's --itemize-changes line: YXcstpoguax.
#[derive(Clone, Copy)]
struct Item {
    y: u8,
    x: u8,
    f: [u8; 9],
}

const FLAGS: &[u8; 9] = b"cstpoguax";
const IC: usize = 0;
const IS: usize = 1;
const IT: usize = 2;
const IP: usize = 3;
const IO: usize = 4;
const IG: usize = 5;
const IA: usize = 7;
const IX: usize = 8;

impl Item {
    /// An existing entry; nothing changed yet.
    fn same(x: u8) -> Item {
        Item { y: b'.', x, f: [b'.'; 9] }
    }
    /// An existing entry that is being replaced or rewritten.
    fn update(y: u8, x: u8) -> Item {
        Item { y, x, f: [b'.'; 9] }
    }
    /// A new entry.
    fn new(y: u8, x: u8) -> Item {
        Item { y, x, f: [b'+'; 9] }
    }
    fn mark(&mut self, i: usize) {
        if self.f[i] == b'.' {
            self.f[i] = FLAGS[i];
        }
    }
    fn changed(&self) -> bool {
        self.y != b'.' || self.f.iter().any(|&c| c != b'.')
    }
}

fn kind_char(mode: u32) -> u8 {
    match mode & libc::S_IFMT {
        libc::S_IFREG => b'f',
        libc::S_IFDIR => b'd',
        libc::S_IFLNK => b'L',
        libc::S_IFCHR | libc::S_IFBLK => b'D',
        _ => b'S',
    }
}

/// Where metadata is applied: an open file, or a name in a directory.
#[derive(Clone, Copy)]
enum Target<'a> {
    Fd(RawFd),
    At(RawFd, &'a CStr),
}

/// A failed step of a file transfer.
struct Fail {
    what: &'static str,
    err: io::Error,
    src: bool,
}

fn sf(what: &'static str) -> impl FnOnce(io::Error) -> Fail {
    move |err| Fail { what, err, src: true }
}

fn df(what: &'static str) -> impl FnOnce(io::Error) -> Fail {
    move |err| Fail { what, err, src: false }
}

thread_local! {
    static RW_BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static CMP_BUFS: RefCell<(Vec<u8>, Vec<u8>)> = const { RefCell::new((Vec::new(), Vec::new())) };
}

/// One pass over a pair.
struct Run<'a> {
    e: &'a Engine,
    ps: &'a PairState,
    filter: &'a Filter,
    src_root: OwnedFd,
    /// None only in a dry run whose destination doesn't exist yet.
    dst_root: Option<OwnedFd>,
    src_dev: u64,
    dst_dev: u64,
    /// Mount IDs of the two roots, to spot bind mounts inside them.
    src_mnt: Option<u64>,
    dst_mnt: Option<u64>,
    full: bool,
    root_created: bool,
    abort: AtomicBool,
    links_unknown: AtomicBool,
    /// Multiply-linked files met in this pass, synced together at the end.
    hl: Mutex<Links>,
    /// Destination directories whose mtime may need restoring.
    touched: Mutex<HashSet<Vec<u8>>>,
    /// Directories missing from the destination, to be synced in full.
    escalate: Mutex<HashSet<Vec<u8>>>,
    slow: Mutex<Vec<(Vec<u8>, Duration)>>,
    redo: Mutex<Vec<Vec<u8>>>,
}

impl<'a> Run<'a> {
    fn new(e: &'a Engine, ps: &'a PairState, full: bool) -> io::Result<Run<'a>> {
        let src_root = sys::open_dir_path(&ps.pair.src)
            .map_err(|e| io::Error::new(e.kind(), format!("can't open {}: {e}", ps.pair.src.display())))?;
        let src_dev = sys::fstat(src_root.as_raw_fd())?.dev;
        let old = ps.src_dev.load(Relaxed);
        if old == 0 {
            ps.src_dev.store(src_dev, Relaxed);
        } else if old != src_dev {
            // Most likely unmounted: syncing the empty mount point would
            // delete the whole backup.
            return Err(io::Error::other(format!(
                "{} is on a different filesystem than when fansync started (was it unmounted?)",
                ps.pair.src.display()
            )));
        }
        let mut root_created = false;
        let dst_root = match sys::open_dir_path(&ps.pair.dst) {
            Ok(fd) => Some(fd),
            Err(err) if sys::errno(&err) == libc::ENOENT => {
                root_created = true;
                if e.opts.dry_run {
                    None
                } else {
                    // Only the last component is created, like rsync, so an
                    // unmounted destination fails instead of filling up the
                    // filesystem underneath it.
                    sys::mkdir_path(&ps.pair.dst, 0o700).map_err(|e| {
                        io::Error::new(
                            e.kind(),
                            format!("can't create {} (its parent must exist): {e}", ps.pair.dst.display()),
                        )
                    })?;
                    Some(sys::open_dir_path(&ps.pair.dst)?)
                }
            }
            Err(err) => return Err(io::Error::new(err.kind(), format!("can't open {}: {err}", ps.pair.dst.display()))),
        };
        let mut dst_dev = 0;
        if let Some(d) = &dst_root {
            let dev = sys::fstat(d.as_raw_fd())?.dev;
            dst_dev = dev;
            let old = ps.dst_dev.load(Relaxed);
            if old == 0 {
                ps.dst_dev.store(dev, Relaxed);
            } else if old != dev {
                return Err(io::Error::other(format!(
                    "{} is on a different filesystem than when fansync started (was it unmounted?)",
                    ps.pair.dst.display()
                )));
            }
        }
        let src_mnt = sys::mount_id(src_root.as_raw_fd()).ok();
        let dst_mnt = dst_root.as_ref().and_then(|d| sys::mount_id(d.as_raw_fd()).ok());
        Ok(Run {
            e,
            ps,
            filter: &ps.pair.filter,
            src_root,
            dst_root,
            src_dev,
            dst_dev,
            src_mnt,
            dst_mnt,
            full,
            root_created,
            abort: AtomicBool::new(false),
            links_unknown: AtomicBool::new(false),
            hl: Mutex::new(HashMap::new()),
            touched: Mutex::new(HashSet::new()),
            escalate: Mutex::new(HashSet::new()),
            slow: Mutex::new(Vec::new()),
            redo: Mutex::new(Vec::new()),
        })
    }

    fn outcome(self) -> Outcome {
        Outcome {
            slow: self.slow.into_inner().unwrap(),
            redo: self.redo.into_inner().unwrap(),
            aborted: self.abort.into_inner(),
            links_unknown: self.links_unknown.into_inner(),
        }
    }

    fn dry(&self) -> bool {
        self.e.opts.dry_run
    }

    fn stopped(&self) -> bool {
        self.abort.load(Relaxed) || self.e.stop.load(Relaxed)
    }

    fn error(&self, rel: &[u8], what: &str, err: &io::Error) {
        if self.stopped() && sys::errno(err) == libc::EINTR {
            return;
        }
        self.e.stats.errors.fetch_add(1, Relaxed);
        if matches!(sys::errno(err), libc::ENOSPC | libc::EDQUOT | libc::EROFS) {
            if !self.abort.swap(true, Relaxed) {
                log!("error: {}: {what} {}: {err}; stopping this pass", self.ps.pair.name, show(rel));
            }
            return;
        }
        log!("error: {}: {what} {}: {err}", self.ps.pair.name, show(rel));
    }

    /// Errors reading the source. Files disappearing is normal on a live
    /// system, so those are only counted.
    fn src_error(&self, rel: &[u8], what: &str, err: &io::Error) {
        if matches!(sys::errno(err), libc::ENOENT | libc::ENOTDIR | libc::ELOOP | libc::ESTALE) {
            self.e.stats.vanished.fetch_add(1, Relaxed);
            dlog!("{}: {} vanished", self.ps.pair.name, show(rel));
        } else {
            self.error(rel, what, err);
        }
    }

    fn emit(&self, it: &Item, rel: &[u8], extra: &str) {
        if !it.changed() {
            return;
        }
        let slash = if it.x == b'd' { "/" } else { "" };
        if self.e.opts.itemize {
            let f = std::str::from_utf8(&it.f).unwrap();
            println!("{}{}{f} {}{slash}{extra}", it.y as char, it.x as char, show(rel));
        } else if verbose() >= 1 {
            println!("{}{slash}{extra}", show(rel));
        }
    }

    fn emit_delete(&self, rel: &[u8], dir: bool) {
        let slash = if dir { "/" } else { "" };
        if self.e.opts.itemize {
            println!("*deleting   {}{slash}", show(rel));
        } else if verbose() >= 1 {
            println!("deleting {}{slash}", show(rel));
        }
    }

    fn touch(&self, dir: &[u8]) {
        self.touched.lock().unwrap().insert(dir.to_vec());
    }

    // ---- tree walk ----------------------------------------------------

    fn sync_root(&self, recurse: bool) {
        // Possible when the source has no trailing slash and excludes itself.
        if self.filter.excluded_path(b"", true) {
            return;
        }
        let sfd = self.src_root.as_raw_fd();
        let st = match sys::fstat(sfd) {
            Ok(s) => s,
            Err(e) => return self.src_error(b"", "stat", &e),
        };
        let dfd = self.dst_root.as_ref().map(|d| d.as_raw_fd());
        if self.root_created {
            self.e.stats.created.fetch_add(1, Relaxed);
            self.emit(&Item::new(b'c', b'd'), b"", "");
        }
        if recurse {
            self.sync_dir_contents(sfd, dfd, b"");
        }
        if let Some(d) = dfd {
            match sys::fstat(d) {
                Ok(cur) => {
                    let mut item = Item::same(b'd');
                    if self.apply_meta(Target::Fd(d), &st, Xt::Fd(sfd), &cur, &mut item, b"") && !self.root_created {
                        self.e.stats.meta.fetch_add(1, Relaxed);
                    }
                    if !self.root_created {
                        self.emit(&item, b"", "");
                    }
                }
                Err(e) => self.error(b"", "stat", &e),
            }
        }
    }

    fn sync_dir_contents(&self, sfd: RawFd, dfd: Option<RawFd>, rel: &[u8]) {
        if self.stopped() {
            return;
        }
        let src = match sys::read_dir(sfd) {
            Ok(v) => v,
            Err(e) => return self.src_error(rel, "list", &e),
        };
        let dst: HashMap<CString, u8> = match dfd {
            Some(d) => match sys::read_dir(d) {
                Ok(v) => v.into_iter().map(|e| (e.name, e.dtype)).collect(),
                Err(e) => return self.error(rel, "list destination", &e),
            },
            None => HashMap::new(),
        };
        // Deletions first, so space is freed and type changes can proceed.
        if let Some(d) = dfd
            && !dst.is_empty()
        {
            let names: HashSet<&CStr> = src.iter().map(|e| e.name.as_c_str()).collect();
            let extra: Vec<(&CString, u8)> =
                dst.iter().filter(|(n, _)| !names.contains(n.as_c_str())).map(|(n, &t)| (n, t)).collect();
            extra.par_iter().for_each(|&(name, dtype)| {
                let crel = join(rel, name.to_bytes());
                // Leftover temporary files are always removed.
                let ours = is_temp(name);
                if !ours && (!self.e.opts.delete || self.protected(d, name, &crel, dtype)) {
                    return;
                }
                self.delete_entry(d, name, &crel);
            });
        }
        src.par_iter().for_each(|ent| {
            if self.stopped() {
                return;
            }
            let crel = join(rel, ent.name.to_bytes());
            let known = ent.dtype != DT_UNKNOWN;
            if known && self.filter.excluded(&crel, ent.dtype == DT_DIR) {
                return;
            }
            let present = dfd.is_some() && dst.contains_key(&ent.name);
            self.sync_entry(sfd, dfd, &ent.name, &crel, Some(present), !known, true);
        });
    }

    /// Whether a destination entry is excluded and therefore kept.
    fn protected(&self, dir: RawFd, name: &CStr, rel: &[u8], dtype: u8) -> bool {
        if self.filter.is_empty() {
            return false;
        }
        let is_dir = match dtype {
            DT_UNKNOWN => sys::fstatat(dir, name).is_ok_and(|s| s.is_dir()),
            t => t == DT_DIR,
        };
        self.filter.excluded(rel, is_dir)
    }

    /// `present`: whether the destination has the name (None = unknown).
    #[allow(clippy::too_many_arguments)]
    fn sync_entry(
        &self,
        sdir: RawFd,
        ddir: Option<RawFd>,
        name: &CStr,
        rel: &[u8],
        present: Option<bool>,
        check_filter: bool,
        recurse: bool,
    ) -> bool {
        let st = match sys::fstatat(sdir, name) {
            Ok(s) => s,
            Err(e) => {
                self.src_error(rel, "stat", &e);
                return false;
            }
        };
        if check_filter && self.filter.excluded(rel, st.is_dir()) {
            return false;
        }
        self.sync_stat(sdir, ddir, name, &st, rel, present, recurse)
    }

    /// Syncs one entry whose source stat is known. Returns whether the
    /// destination entry was created or replaced.
    #[allow(clippy::too_many_arguments)]
    fn sync_stat(
        &self,
        sdir: RawFd,
        ddir: Option<RawFd>,
        name: &CStr,
        st: &Stat,
        rel: &[u8],
        present: Option<bool>,
        recurse: bool,
    ) -> bool {
        self.e.stats.scanned.fetch_add(1, Relaxed);
        let dst = match ddir {
            Some(d) if present != Some(false) => match sys::fstatat(d, name) {
                Ok(s) => Some(s),
                Err(e) if sys::errno(&e) == libc::ENOENT => None,
                Err(e) => {
                    self.error(rel, "stat destination", &e);
                    return false;
                }
            },
            _ => None,
        };
        match st.kind() {
            libc::S_IFDIR => self.sync_dir(sdir, ddir, name, st, rel, dst, recurse),
            libc::S_IFREG if self.e.opts.hard_links && st.nlink > 1 => {
                let mut hl = self.hl.lock().unwrap();
                let e = hl.entry((st.dev, st.ino)).or_default();
                e.0.push(rel.into());
                e.1 = e.1.max(st.nlink);
                false
            }
            libc::S_IFREG => {
                if self.e.opts.hard_links && !self.dry() {
                    self.ps.inodes.record(st.ino, split(rel).0);
                }
                self.sync_file(sdir, ddir, name, st, rel, dst)
            }
            libc::S_IFLNK => self.sync_symlink(sdir, ddir, name, st, rel, dst),
            _ => self.sync_special(sdir, ddir, name, st, rel, dst),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn sync_dir(
        &self,
        sdir: RawFd,
        ddir: Option<RawFd>,
        name: &CStr,
        st: &Stat,
        rel: &[u8],
        mut dst: Option<Stat>,
        recurse: bool,
    ) -> bool {
        if let (Some(dd), Some(d)) = (ddir, dst)
            && !d.is_dir()
        {
            if !self.delete_entry(dd, name, rel) {
                return false;
            }
            dst = None;
        }
        let created = ddir.is_none() || dst.is_none();
        let mut dfd = None;
        if let Some(dd) = ddir
            && !(created && self.dry())
        {
            if created
                && let Err(e) = sys::mkdirat(dd, name, 0o700)
                && sys::errno(&e) != libc::EEXIST
            {
                self.error(rel, "create directory", &e);
                return false;
            }
            match sys::open_dir(dd, name) {
                Ok(fd) => dfd = Some(fd),
                Err(e) => {
                    self.error(rel, "open destination directory", &e);
                    return false;
                }
            }
        }
        if created {
            self.e.stats.created.fetch_add(1, Relaxed);
            self.emit(&Item::new(b'c', b'd'), rel, "");
        }
        // One filesystem and one mount only: a mount point is copied as an
        // empty directory. That includes bind mounts of the same filesystem,
        // which keep the device number (rsync -x goes into those and copies
        // their contents a second time).
        if (recurse || created) && st.dev == self.src_dev {
            match sys::open_dir(sdir, name) {
                Ok(sfd) if !sys::on_mount(sfd.as_raw_fd(), self.src_mnt) => {
                    vlog!("{}: not going into {}/, something is mounted there", self.ps.pair.name, show(rel));
                }
                Ok(sfd) => self.sync_dir_contents(sfd.as_raw_fd(), dfd.as_ref().map(|d| d.as_raw_fd()), rel),
                Err(e) => self.src_error(rel, "open directory", &e),
            }
        }
        // Metadata last, so the mtime isn't disturbed by creating entries.
        if let Some(dfd) = &dfd {
            match sys::fstat(dfd.as_raw_fd()) {
                Ok(cur) => {
                    let mut item = Item::same(b'd');
                    let sx = sys::proc_path(sdir, name);
                    let changed = self.apply_meta(Target::Fd(dfd.as_raw_fd()), st, Xt::Path(&sx), &cur, &mut item, rel);
                    if !created {
                        if changed {
                            self.e.stats.meta.fetch_add(1, Relaxed);
                        }
                        self.emit(&item, rel, "");
                    }
                }
                Err(e) => self.error(rel, "stat destination", &e),
            }
        }
        created
    }

    fn sync_file(
        &self,
        sdir: RawFd,
        ddir: Option<RawFd>,
        name: &CStr,
        st: &Stat,
        rel: &[u8],
        dst: Option<Stat>,
    ) -> bool {
        // rsync's quick check: same size and mtime means same content.
        if let (Some(dd), Some(d)) = (ddir, &dst)
            && d.is_reg()
            && d.size == st.size
            && d.mtime == st.mtime
        {
            let mut item = Item::same(b'f');
            let sx = sys::proc_path(sdir, name);
            if self.apply_meta(Target::At(dd, name), st, Xt::Path(&sx), d, &mut item, rel) {
                self.e.stats.meta.fetch_add(1, Relaxed);
            }
            self.emit(&item, rel, "");
            return false;
        }
        let mut item = match &dst {
            Some(d) if d.is_reg() => {
                let mut it = Item::update(b'>', b'f');
                if d.size != st.size {
                    it.mark(IS);
                }
                if d.mtime != st.mtime {
                    it.mark(IT);
                }
                it
            }
            _ => Item::new(b'>', b'f'),
        };
        let Some(dd) = ddir.filter(|_| !self.dry()) else {
            self.e.stats.copied.fetch_add(1, Relaxed);
            self.e.stats.bytes.fetch_add(st.size, Relaxed);
            self.emit(&item, rel, "");
            return true;
        };
        let t0 = Instant::now();
        match self.transfer(sdir, dd, name, rel, dst, &mut item) {
            Ok(()) => {
                self.e.stats.copied.fetch_add(1, Relaxed);
                self.emit(&item, rel, "");
                let took = t0.elapsed();
                if took >= SLOW {
                    self.slow.lock().unwrap().push((rel.to_vec(), took));
                }
                true
            }
            Err(f) => {
                if f.src {
                    self.src_error(rel, f.what, &f.err);
                } else {
                    self.error(rel, f.what, &f.err);
                }
                false
            }
        }
    }

    /// Copies a regular file's data and metadata.
    fn transfer(
        &self,
        sdir: RawFd,
        dd: RawFd,
        name: &CStr,
        rel: &[u8],
        dst: Option<Stat>,
        item: &mut Item,
    ) -> Result<(), Fail> {
        let sfile = sys::open_read(sdir, name).map_err(sf("open"))?;
        let sfd = sfile.as_raw_fd();
        let s = sys::fstat(sfd).map_err(sf("stat"))?;
        if !s.is_reg() {
            // Replaced by something else since it was listed.
            return Err(Fail { what: "open", err: sys::err(libc::ENOENT), src: true });
        }
        sys::fadvise(sfd, libc::POSIX_FADV_SEQUENTIAL);
        let mut dst = dst;
        if let Some(d) = dst
            && d.is_dir()
        {
            if !self.delete_entry(dd, name, rel) {
                return Err(Fail { what: "replace directory", err: sys::err(libc::ENOTEMPTY), src: false });
            }
            dst = None;
        }
        let sparse = self.e.opts.sparse && s.blocks * 512 < s.size;
        let min = self.e.opts.inplace_min;
        let complete = match dst {
            // Large file already there: rewrite only the blocks that differ.
            // Much less writing for big files with small changes, and on a
            // copy-on-write destination the unchanged extents stay shared
            // with snapshots.
            Some(d) if min > 0 && d.is_reg() && d.nlink == 1 && d.size > 0 && s.size >= min => {
                let dfile = sys::openat(dd, name, libc::O_RDWR | libc::O_NOFOLLOW, 0).map_err(df("open"))?;
                let dfd = dfile.as_raw_fd();
                let complete = self.update_inplace(sfd, dfd, s.size, d.size, sparse)?;
                self.finish_file(sfd, &s, dfd, rel)?;
                self.done_with(sfd, dfd, s.size);
                *item = Item::update(b'>', b'f');
                item.mark(if d.size != s.size { IS } else { IC });
                if d.mtime != s.mtime {
                    item.mark(IT);
                }
                complete
            }
            // Nothing there yet: write straight to the final name. If this is
            // interrupted, the partial file's mtime won't match and it gets
            // copied again next time.
            None => {
                match sys::openat(dd, name, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW, 0o600) {
                    Ok(dfile) => {
                        let dfd = dfile.as_raw_fd();
                        let r = self.copy_data(sfd, dfd, s.size, sparse).and_then(|c| {
                            self.finish_file(sfd, &s, dfd, rel)?;
                            Ok(c)
                        });
                        if r.is_err() {
                            let _ = sys::unlinkat(dd, name, false);
                        }
                        self.done_with(sfd, dfd, s.size);
                        r?
                    }
                    Err(e) if sys::errno(&e) == libc::EEXIST => self.replace(sfd, &s, dd, name, rel, sparse)?,
                    Err(e) => return Err(Fail { what: "create", err: e, src: false }),
                }
            }
            // Replace atomically, so the old copy survives a failed update.
            Some(_) => self.replace(sfd, &s, dd, name, rel, sparse)?,
        };
        let after = sys::fstat(sfd).map_err(sf("stat"))?;
        if !complete || after.size != s.size || after.mtime != s.mtime {
            vlog!("{}: {} changed while being copied", self.ps.pair.name, show(rel));
            self.redo.lock().unwrap().push(rel.to_vec());
        }
        Ok(())
    }

    /// Writes a new copy aside and renames it over the old one.
    fn replace(&self, sfd: RawFd, s: &Stat, dd: RawFd, name: &CStr, rel: &[u8], sparse: bool) -> Result<bool, Fail> {
        let (tmp, tname) = self.create_temp(dd).map_err(df("create temporary file"))?;
        let tfd = tmp.as_raw_fd();
        let r = self.copy_data(sfd, tfd, s.size, sparse).and_then(|c| {
            self.finish_file(sfd, s, tfd, rel)?;
            self.done_with(sfd, tfd, s.size);
            self.install(dd, tfd, tname.as_deref(), name).map_err(df("rename into place"))?;
            Ok(c)
        });
        if r.is_err()
            && let Some(t) = &tname
        {
            let _ = sys::unlinkat(dd, t, false);
        }
        r
    }

    /// An unnamed O_TMPFILE where supported (nothing is left behind if
    /// fansync dies), else a hidden temporary name.
    fn create_temp(&self, dd: RawFd) -> io::Result<(OwnedFd, Option<CString>)> {
        if self.ps.tmpfile.load(Relaxed) {
            match sys::openat(dd, c".", libc::O_WRONLY | libc::O_TMPFILE, 0o600) {
                Ok(fd) => return Ok((fd, None)),
                Err(e) if matches!(sys::errno(&e), libc::EOPNOTSUPP | libc::EISDIR | libc::EINVAL) => {
                    self.ps.tmpfile.store(false, Relaxed)
                }
                Err(e) => return Err(e),
            }
        }
        loop {
            let t = tmp_name();
            match sys::openat(dd, &t, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW, 0o600) {
                Ok(fd) => return Ok((fd, Some(t))),
                Err(e) if sys::errno(&e) == libc::EEXIST => continue,
                Err(e) => return Err(e),
            }
        }
    }

    fn install(&self, dd: RawFd, fd: RawFd, tmp: Option<&CStr>, name: &CStr) -> io::Result<()> {
        let linked;
        let t = match tmp {
            Some(t) => t,
            None => {
                linked = tmp_name();
                sys::linkat(libc::AT_FDCWD, &sys::fd_path(fd), dd, &linked, libc::AT_SYMLINK_FOLLOW)?;
                &linked
            }
        };
        sys::renameat(dd, t, dd, name).inspect_err(|_| {
            let _ = sys::unlinkat(dd, t, false);
        })
    }

    fn finish_file(&self, sfd: RawFd, s: &Stat, dfd: RawFd, rel: &[u8]) -> Result<(), Fail> {
        let cur = sys::fstat(dfd).map_err(df("stat"))?;
        let mut scratch = Item::same(b'f');
        self.apply_meta(Target::Fd(dfd), s, Xt::Fd(sfd), &cur, &mut scratch, rel);
        Ok(())
    }

    /// Keeps a backup run from flushing everything else out of the page
    /// cache: the destination copy of a big file is never read again.
    fn done_with(&self, sfd: RawFd, dfd: RawFd, size: u64) {
        if size >= BIG {
            sys::fadvise(dfd, libc::POSIX_FADV_DONTNEED);
        }
        if self.e.opts.drop_cache {
            sys::fadvise(sfd, libc::POSIX_FADV_DONTNEED);
        }
    }

    // ---- data copying ---------------------------------------------------

    /// Copies `len` bytes at `off`. Returns the bytes copied, which is less
    /// than `len` only if the source got shorter.
    fn copy_range(&self, sfd: RawFd, dfd: RawFd, off: u64, len: u64) -> Result<u64, Fail> {
        let mut done = 0u64;
        while done < len {
            if self.stopped() {
                return Err(Fail { what: "copy", err: sys::err(libc::EINTR), src: false });
            }
            let pos = off + done;
            let want = (len - done).min(CHUNK) as usize;
            let n = match self.ps.copy_mode.load(Relaxed) {
                // In-kernel copy; on the same filesystem this may even be a
                // reflink or a server-side copy.
                MODE_CFR => match sys::copy_file_range(sfd, pos, dfd, pos, want) {
                    Ok(n) => n,
                    Err(e)
                        if matches!(sys::errno(&e), libc::EXDEV | libc::EINVAL | libc::EOPNOTSUPP | libc::ENOSYS) =>
                    {
                        dlog!("{}: copy_file_range unavailable ({e}); using sendfile", self.ps.pair.name);
                        self.ps.copy_mode.store(MODE_SENDFILE, Relaxed);
                        continue;
                    }
                    Err(e) => return Err(Fail { what: "copy", err: e, src: false }),
                },
                // Across filesystems: still a single in-kernel copy, no trip
                // through user space.
                MODE_SENDFILE => {
                    sys::lseek(dfd, pos, libc::SEEK_SET).map_err(df("seek"))?;
                    match sys::sendfile(dfd, sfd, pos, want) {
                        Ok(n) => n,
                        Err(e) if matches!(sys::errno(&e), libc::EINVAL | libc::ENOSYS) => {
                            dlog!("{}: sendfile unavailable ({e}); using read/write", self.ps.pair.name);
                            self.ps.copy_mode.store(MODE_RW, Relaxed);
                            continue;
                        }
                        Err(e) => return Err(Fail { what: "copy", err: e, src: false }),
                    }
                }
                _ => RW_BUF.with(|b| {
                    let mut buf = b.borrow_mut();
                    if buf.is_empty() {
                        buf.resize(1 << 20, 0);
                    }
                    let n = want.min(buf.len());
                    let n = sys::pread_full(sfd, &mut buf[..n], pos).map_err(sf("read"))?;
                    sys::pwrite_all(dfd, &buf[..n], pos).map_err(df("write"))?;
                    Ok::<usize, Fail>(n)
                })?,
            };
            if n == 0 {
                break;
            }
            done += n as u64;
            self.e.stats.bytes.fetch_add(n as u64, Relaxed);
        }
        Ok(done)
    }

    /// Source data ranges, found with SEEK_DATA/SEEK_HOLE.
    fn data_segments(&self, sfd: RawFd, size: u64) -> Vec<(u64, u64)> {
        let mut segs = Vec::new();
        let mut pos = 0;
        while pos < size {
            let data = match sys::lseek(sfd, pos, libc::SEEK_DATA) {
                Ok(d) => d,
                Err(e) if sys::errno(&e) == libc::ENXIO => break,
                Err(_) => {
                    segs.push((pos, size));
                    break;
                }
            };
            if data >= size {
                break;
            }
            let hole = sys::lseek(sfd, data, libc::SEEK_HOLE).unwrap_or(size).min(size);
            let hole = if hole <= data { size } else { hole };
            segs.push((data, hole));
            pos = hole;
        }
        segs
    }

    /// Copies the data of a file into an empty destination. Holes in the
    /// source stay holes. Returns false if the source got shorter meanwhile.
    fn copy_data(&self, sfd: RawFd, dfd: RawFd, size: u64, sparse: bool) -> Result<bool, Fail> {
        if !sparse {
            return Ok(self.copy_range(sfd, dfd, 0, size)? == size);
        }
        for (a, b) in self.data_segments(sfd, size) {
            let n = self.copy_range(sfd, dfd, a, b - a)?;
            if n < b - a {
                sys::ftruncate(dfd, a + n).map_err(df("truncate"))?;
                return Ok(false);
            }
        }
        sys::ftruncate(dfd, size).map_err(df("truncate"))?;
        Ok(true)
    }

    /// Updates an existing copy in place, reading both files and writing
    /// only the blocks that differ.
    fn update_inplace(&self, sfd: RawFd, dfd: RawFd, size: u64, dsize: u64, sparse: bool) -> Result<bool, Fail> {
        if dsize != size {
            sys::ftruncate(dfd, size).map_err(df("truncate"))?;
        }
        let old = dsize.min(size);
        let mut segs = if sparse { self.data_segments(sfd, size) } else { vec![(0, size)] };
        if sparse {
            // Punch holes where the source has a hole but the old copy has data.
            // Where the destination can't punch holes, those ranges are
            // compared and overwritten (with the source's zeros) instead.
            let mut zero = Vec::new();
            let mut pos = 0;
            for &(a, b) in segs.iter().chain(std::iter::once(&(size, size))) {
                let end = a.min(old);
                if pos < end
                    && matches!(sys::lseek(dfd, pos, libc::SEEK_DATA), Ok(d) if d < end)
                    && let Err(e) = sys::punch_hole(dfd, pos, end - pos)
                {
                    self.e.warn_once(format!("punch-{}", self.ps.pair.name), || {
                        format!(
                            "{}: can't punch holes in the destination ({e}); writing zeros instead",
                            self.ps.pair.name
                        )
                    });
                    zero.push((pos, end));
                }
                pos = b;
            }
            if !zero.is_empty() {
                segs.extend(zero);
                segs.sort_unstable();
            }
        }
        CMP_BUFS.with(|cell| {
            let mut bufs = cell.borrow_mut();
            if bufs.0.len() < CMP_CHUNK {
                bufs.0.resize(CMP_CHUNK, 0);
                bufs.1.resize(CMP_CHUNK, 0);
            }
            let (sb, db) = &mut *bufs;
            for &(a, b) in &segs {
                let mut off = a;
                while off < b {
                    if self.stopped() {
                        return Err(Fail { what: "copy", err: sys::err(libc::EINTR), src: false });
                    }
                    let n = ((b - off) as usize).min(CMP_CHUNK);
                    let got = sys::pread_full(sfd, &mut sb[..n], off).map_err(sf("read"))?;
                    let dn = if off < old {
                        let want = n.min((old - off) as usize);
                        sys::pread_full(dfd, &mut db[..want], off).map_err(df("read"))?
                    } else {
                        0
                    };
                    let same = |i: usize| {
                        let e = (i + CMP_BLOCK).min(got);
                        e <= dn && sb[i..e] == db[i..e]
                    };
                    let mut i = 0;
                    while i < got {
                        if same(i) {
                            i = (i + CMP_BLOCK).min(got);
                            continue;
                        }
                        let start = i;
                        while i < got && !same(i) {
                            i = (i + CMP_BLOCK).min(got);
                        }
                        let run = &sb[start..i];
                        let at = off + start as u64;
                        // Past the old end the file is already a hole.
                        if sparse && at >= old && run.iter().all(|&c| c == 0) {
                            continue;
                        }
                        sys::pwrite_all(dfd, run, at).map_err(df("write"))?;
                        self.e.stats.bytes.fetch_add(run.len() as u64, Relaxed);
                    }
                    sys::fadvise_range(dfd, off, n as u64, libc::POSIX_FADV_DONTNEED);
                    if got < n {
                        return Ok(false);
                    }
                    off += n as u64;
                }
            }
            Ok(true)
        })
    }

    // ---- other file types -------------------------------------------------

    fn sync_symlink(
        &self,
        sdir: RawFd,
        ddir: Option<RawFd>,
        name: &CStr,
        st: &Stat,
        rel: &[u8],
        dst: Option<Stat>,
    ) -> bool {
        let target = match sys::readlinkat(sdir, name) {
            Ok(t) => t,
            Err(e) => {
                self.src_error(rel, "read symlink", &e);
                return false;
            }
        };
        let shown = format!(" -> {}", String::from_utf8_lossy(&target));
        let mut item = Item::new(b'c', b'L');
        if let (Some(dd), Some(d)) = (ddir, &dst)
            && d.is_lnk()
        {
            if sys::readlinkat(dd, name).is_ok_and(|t| t == target) {
                let mut item = Item::same(b'L');
                let sx = sys::proc_path(sdir, name);
                if self.apply_meta(Target::At(dd, name), st, Xt::Path(&sx), d, &mut item, rel) {
                    self.e.stats.meta.fetch_add(1, Relaxed);
                }
                self.emit(&item, rel, &shown);
                return false;
            }
            item = Item::update(b'c', b'L');
            item.mark(IC);
        }
        self.e.stats.created.fetch_add(1, Relaxed);
        let Some(dd) = ddir.filter(|_| !self.dry()) else {
            self.emit(&item, rel, &shown);
            return true;
        };
        let r = sys::cstring(&target).and_then(|t| self.place(dd, name, rel, dst, |dir, n| sys::symlinkat(&t, dir, n)));
        if let Err(e) = r {
            self.error(rel, "create symlink", &e);
            return false;
        }
        self.meta_after_create(sdir, dd, name, st, rel);
        self.emit(&item, rel, &shown);
        true
    }

    /// Devices, FIFOs and sockets. These are never opened.
    fn sync_special(
        &self,
        sdir: RawFd,
        ddir: Option<RawFd>,
        name: &CStr,
        st: &Stat,
        rel: &[u8],
        dst: Option<Stat>,
    ) -> bool {
        let x = kind_char(st.mode);
        let is_dev = matches!(st.kind(), libc::S_IFCHR | libc::S_IFBLK);
        let mut item = Item::new(b'c', x);
        if let (Some(dd), Some(d)) = (ddir, &dst)
            && d.kind() == st.kind()
        {
            if !is_dev || d.rdev == st.rdev {
                let mut item = Item::same(x);
                let sx = sys::proc_path(sdir, name);
                if self.apply_meta(Target::At(dd, name), st, Xt::Path(&sx), d, &mut item, rel) {
                    self.e.stats.meta.fetch_add(1, Relaxed);
                }
                self.emit(&item, rel, "");
                return false;
            }
            item = Item::update(b'c', x);
            item.mark(IC);
        }
        self.e.stats.created.fetch_add(1, Relaxed);
        let Some(dd) = ddir.filter(|_| !self.dry()) else {
            self.emit(&item, rel, "");
            return true;
        };
        if let Err(e) = self.place(dd, name, rel, dst, |dir, n| sys::mknodat(dir, n, st.mode, st.rdev)) {
            if is_dev && sys::errno(&e) == libc::EPERM {
                self.e.stats.errors.fetch_add(1, Relaxed);
                self.e.warn_once("mknod".into(), || format!("can't create device files ({e}); run as root"));
            } else {
                self.error(rel, "create", &e);
            }
            return false;
        }
        self.meta_after_create(sdir, dd, name, st, rel);
        self.emit(&item, rel, "");
        true
    }

    /// Creates a non-regular entry with `create`. If something is in the
    /// way it is replaced: atomically through a temporary name, or for a
    /// directory by deleting it first.
    fn place(
        &self,
        dd: RawFd,
        name: &CStr,
        rel: &[u8],
        dst: Option<Stat>,
        create: impl Fn(RawFd, &CStr) -> io::Result<()>,
    ) -> io::Result<()> {
        match dst {
            None => match create(dd, name) {
                Err(e) if sys::errno(&e) == libc::EEXIST => {}
                r => return r,
            },
            Some(d) if d.is_dir() => {
                if !self.delete_entry(dd, name, rel) {
                    return Err(sys::err(libc::ENOTEMPTY));
                }
                return create(dd, name);
            }
            Some(_) => {}
        }
        let tmp = tmp_name();
        create(dd, &tmp)?;
        sys::renameat(dd, &tmp, dd, name).inspect_err(|_| {
            let _ = sys::unlinkat(dd, &tmp, false);
        })
    }

    fn meta_after_create(&self, sdir: RawFd, dd: RawFd, name: &CStr, st: &Stat, rel: &[u8]) {
        match sys::fstatat(dd, name) {
            Ok(cur) => {
                let sx = sys::proc_path(sdir, name);
                self.apply_meta(Target::At(dd, name), st, Xt::Path(&sx), &cur, &mut Item::same(0), rel);
            }
            Err(e) => self.error(rel, "stat destination", &e),
        }
    }

    // ---- metadata -----------------------------------------------------------

    /// Makes owner, permissions, xattrs/ACLs and mtime match. Order matters:
    /// chown clears setuid bits and file capabilities, so chmod and xattrs
    /// come after it, and the times come last. Returns whether anything
    /// differed.
    fn apply_meta(&self, t: Target, s: &Stat, sx: Xt, d: &Stat, item: &mut Item, rel: &[u8]) -> bool {
        let dry = self.dry();
        let mut changed = false;
        let mut chowned = false;
        if self.e.opts.owner && (s.uid != d.uid || s.gid != d.gid) {
            if s.uid != d.uid {
                item.mark(IO);
            }
            if s.gid != d.gid {
                item.mark(IG);
            }
            changed = true;
            if !dry {
                let r = match t {
                    Target::Fd(fd) => sys::fchown(fd, s.uid, s.gid),
                    Target::At(dir, n) => sys::fchownat(dir, n, s.uid, s.gid),
                };
                match r {
                    Ok(()) => chowned = true,
                    Err(e) => self.error(rel, "chown", &e),
                }
            }
        }
        // xattrs before chmod: a non-root user can't set them on a file
        // that is about to become read-only. (chmod keeps the ACL in step
        // with the mode, so the order doesn't matter for ACLs.)
        let mut unlocked = false;
        if self.e.opts.xattrs {
            let dpath;
            let dx = match t {
                Target::Fd(fd) => Xt::Fd(fd),
                Target::At(dir, n) => {
                    dpath = sys::proc_path(dir, n);
                    Xt::Path(&dpath)
                }
            };
            // Without root, an existing read-only file needs u+w briefly.
            let unlock = || {
                if self.e.opts.owner || s.is_lnk() || d.perms() & 0o200 != 0 {
                    return false;
                }
                let r = match t {
                    Target::Fd(fd) => sys::fchmod(fd, d.perms() | 0o200),
                    Target::At(dir, n) => sys::fchmodat(dir, n, d.perms() | 0o200),
                };
                r.is_ok()
            };
            let (c, u) = self.sync_xattrs(sx, dx, unlock, item, rel);
            changed |= c;
            unlocked = u;
        }
        if !s.is_lnk() && (s.perms() != d.perms() || unlocked || (chowned && s.perms() & 0o6000 != 0)) {
            if s.perms() != d.perms() {
                item.mark(IP);
                changed = true;
            }
            if !dry {
                let r = match t {
                    Target::Fd(fd) => sys::fchmod(fd, s.perms()),
                    Target::At(dir, n) => sys::fchmodat(dir, n, s.perms()),
                };
                if let Err(e) = r {
                    self.error(rel, "chmod", &e);
                }
            }
        }
        if s.mtime != d.mtime {
            item.mark(IT);
            changed = true;
            if !dry {
                let r = match t {
                    Target::Fd(fd) => sys::futimens(fd, s.atime, s.mtime),
                    Target::At(dir, n) => sys::utimensat(dir, n, s.atime, s.mtime),
                };
                if let Err(e) = r {
                    self.error(rel, "set times", &e);
                }
            }
        }
        changed
    }

    /// Copies extended attributes, which includes POSIX ACLs
    /// (system.posix_acl_*), SELinux labels and file capabilities, and
    /// removes ones the source doesn't have. `unlock` makes the destination
    /// writable after a permission error. Returns whether anything differed
    /// and whether `unlock` was used.
    fn sync_xattrs(&self, sx: Xt, dx: Xt, unlock: impl Fn() -> bool, item: &mut Item, rel: &[u8]) -> (bool, bool) {
        let snames = match sys::xlist(sx) {
            Ok(v) => v,
            Err(e) if unsupported(&e) => Vec::new(),
            Err(e) => {
                self.src_error(rel, "list xattrs", &e);
                return (false, false);
            }
        };
        let dnames = match sys::xlist(dx) {
            Ok(v) => v,
            Err(e) if unsupported(&e) => {
                if !snames.is_empty() {
                    self.e.warn_once(format!("xattr-{}", self.ps.pair.name), || {
                        format!("{}: the destination doesn't support extended attributes or ACLs", self.ps.pair.name)
                    });
                }
                return (false, false);
            }
            Err(e) => {
                self.error(rel, "list destination xattrs", &e);
                return (false, false);
            }
        };
        if snames.is_empty() && dnames.is_empty() {
            return (false, false);
        }
        let mut unlocked = false;
        let mut retry = |r: io::Result<()>, f: &dyn Fn() -> io::Result<()>| match r {
            Err(e) if sys::errno(&e) == libc::EACCES && !unlocked && unlock() => {
                unlocked = true;
                f()
            }
            r => r,
        };
        let names = |v: &[u8]| v.split(|&c| c == 0).filter(|s| !s.is_empty()).map(|s| s.to_vec()).collect::<Vec<_>>();
        let sl = names(&snames);
        let dl = names(&dnames);
        let mut changed = false;
        for n in &sl {
            let cn = sys::cstring(n).unwrap();
            let sv = match sys::xget(sx, &cn) {
                Ok(v) => v,
                Err(e) if sys::errno(&e) == libc::ENODATA => continue,
                Err(e) => {
                    self.src_error(rel, "read xattr", &e);
                    continue;
                }
            };
            if dl.contains(n) && sys::xget(dx, &cn).is_ok_and(|v| v == sv) {
                continue;
            }
            item.mark(if is_acl(n) { IA } else { IX });
            changed = true;
            if !self.dry() {
                let set = || sys::xset(dx, &cn, &sv);
                if let Err(e) = retry(set(), &set) {
                    self.xattr_error(n, rel, &e);
                }
            }
        }
        for n in dl.iter().filter(|n| !sl.contains(n)) {
            item.mark(if is_acl(n) { IA } else { IX });
            changed = true;
            if !self.dry() {
                let cn = sys::cstring(n).unwrap();
                let rm = || sys::xremove(dx, &cn);
                if let Err(e) = retry(rm(), &rm)
                    && sys::errno(&e) != libc::ENODATA
                {
                    self.xattr_error(n, rel, &e);
                }
            }
        }
        (changed, unlocked)
    }

    fn xattr_error(&self, name: &[u8], rel: &[u8], e: &io::Error) {
        // Namespaces that need privileges (trusted.*, security.*) or that
        // the destination doesn't support: warn once, not for every file.
        if matches!(sys::errno(e), libc::EPERM | libc::EACCES | libc::EOPNOTSUPP) {
            let ns = String::from_utf8_lossy(name.split(|&c| c == b'.').next().unwrap_or(b"")).into_owned();
            self.e.stats.errors.fetch_add(1, Relaxed);
            self.e.warn_once(format!("xattr-{ns}-{}", sys::errno(e)), || {
                format!("can't set {ns}.* extended attributes in the destination ({e}), e.g. on {}", show(rel))
            });
        } else {
            self.error(rel, "set xattr", e);
        }
    }

    // ---- deletion -------------------------------------------------------------

    /// Deletes a destination entry, recursively for directories. Excluded
    /// entries inside are kept, like rsync does, in which case the directory
    /// stays too. Returns whether the entry is gone.
    fn delete_entry(&self, dir: RawFd, name: &CStr, rel: &[u8]) -> bool {
        let st = match sys::fstatat(dir, name) {
            Ok(s) => s,
            Err(e) if sys::errno(&e) == libc::ENOENT => return true,
            Err(e) => {
                self.error(rel, "stat for deletion", &e);
                return false;
            }
        };
        if st.is_dir() {
            let fd = match sys::open_dir(dir, name) {
                Ok(fd) => fd,
                Err(e) => {
                    self.error(rel, "open for deletion", &e);
                    return false;
                }
            };
            // Something is mounted here: never delete into it. Checking the
            // mount ID too catches bind mounts of the same filesystem, through
            // which rsync -x would delete files that live elsewhere.
            if st.dev != self.dst_dev || !sys::on_mount(fd.as_raw_fd(), self.dst_mnt) {
                vlog!("{}: kept {}/, it is a mount point", self.ps.pair.name, show(rel));
                return false;
            }
            let emptied = self.delete_contents(fd.as_raw_fd(), rel);
            if !emptied {
                if !self.stopped() {
                    vlog!("{}: kept {}/, it has excluded entries", self.ps.pair.name, show(rel));
                }
                return false;
            }
            if !self.dry()
                && let Err(e) = sys::unlinkat(dir, name, true)
            {
                self.error(rel, "remove directory", &e);
                return false;
            }
        } else if !self.dry()
            && let Err(e) = sys::unlinkat(dir, name, false)
        {
            if sys::errno(&e) == libc::ENOENT {
                return true;
            }
            self.error(rel, "delete", &e);
            return false;
        }
        self.e.stats.deleted.fetch_add(1, Relaxed);
        self.emit_delete(rel, st.is_dir());
        true
    }

    fn delete_contents(&self, fd: RawFd, rel: &[u8]) -> bool {
        let ents = match sys::read_dir(fd) {
            Ok(v) => v,
            Err(e) => {
                self.error(rel, "list for deletion", &e);
                return false;
            }
        };
        let ok = AtomicBool::new(true);
        ents.par_iter().for_each(|e| {
            let crel = join(rel, e.name.to_bytes());
            if self.stopped() || self.protected(fd, &e.name, &crel, e.dtype) || !self.delete_entry(fd, &e.name, &crel) {
                ok.store(false, Relaxed);
            }
        });
        ok.into_inner()
    }

    // ---- hard links -------------------------------------------------------------

    /// Syncs the multiply-linked files met during the pass, one group per
    /// source inode: the first path is copied, the others are linked to it.
    fn finish_hardlinks(&self) {
        let collected = std::mem::take(&mut *self.hl.lock().unwrap());
        if collected.is_empty() && !self.full {
            return;
        }
        let groups: Vec<(GroupWork, u64)> = {
            let known = self.ps.hardlinks.lock().unwrap();
            collected
                .into_iter()
                .map(|(k, (mut v, now))| {
                    // An incremental pass only sees the path that changed;
                    // the other links come from earlier passes.
                    let mut before = 1;
                    if !self.full
                        && let Some(old) = known.groups.get(&k)
                    {
                        v.extend(old.paths.iter().cloned());
                        before = old.nlink;
                    }
                    ((k, v, before), now)
                })
                .collect()
        };
        let listings = Mutex::new(HashMap::new());
        let results: Vec<GroupWork> = groups
            .into_par_iter()
            .map(|((k, mut v, before), now)| {
                // During a full pass every name inside the pair is seen. In
                // an incremental one, a link count that went up past the
                // names we know means a new link to a file that had a single
                // name until now: find that name through the inode index.
                if !self.full && now > before && now > v.len() as u64 {
                    let found = self.find_links(k.1, &v, &listings);
                    v.extend(found);
                }
                v.sort_by(|a, b| cmp_path(a, b));
                v.dedup();
                let (paths, nlink) = self.sync_link_group(k, v);
                // Still names we don't know (stale index, or a name outside
                // this pair): a full pass later links whatever it can find.
                if !self.full && nlink > before && nlink > paths.len() as u64 {
                    vlog!("{}: {} has hard links fansync doesn't know yet", self.ps.pair.name, show(&paths[0]));
                    self.links_unknown.store(true, Relaxed);
                }
                (k, paths, nlink)
            })
            .collect();
        if self.dry() || self.stopped() {
            return;
        }
        let mut known = self.ps.hardlinks.lock().unwrap();
        if self.full {
            *known = LinkMap::default();
        }
        for (k, paths, nlink) in results {
            if paths.is_empty() {
                known.remove(k);
            } else {
                known.set(k, Group { paths, nlink });
            }
        }
    }

    /// A name of a multiply-linked file that disappeared (`now` is None) or
    /// now refers to another file may be the one the file was just changed
    /// through: a write followed by unlink, an atomic replace of that name,
    /// or its directory going away. The change shows up under the file's
    /// other names, which got no events of their own, so sync those.
    fn recheck_links(&self, rel: &[u8], now: Option<(u64, u64)>) {
        if !self.e.opts.hard_links {
            return;
        }
        let groups = {
            let known = self.ps.hardlinks.lock().unwrap();
            match now {
                None => known.groups_at(rel),
                Some(k) => known.by_path.get(&tree_key(rel)).copied().filter(|g| *g != k).into_iter().collect(),
            }
        };
        if groups.is_empty() {
            return;
        }
        let mut hl = self.hl.lock().unwrap();
        for k in groups {
            hl.entry(k).or_default();
        }
    }

    /// Queues every known name of these inodes for syncing as a group.
    fn add_link_groups(&self, inos: &[u64]) {
        if !self.e.opts.hard_links || inos.is_empty() {
            return;
        }
        let listings = Mutex::new(HashMap::new());
        for &ino in inos {
            let k = (self.src_dev, ino);
            let mut names =
                self.ps.hardlinks.lock().unwrap().groups.get(&k).map(|g| g.paths.clone()).unwrap_or_default();
            let more = self.find_links(ino, &names, &listings);
            names.extend(more);
            if !names.is_empty() {
                self.hl.lock().unwrap().entry(k).or_default().0.extend(names);
            }
        }
    }

    /// Other names of inode `ino`, from the directory the inode index says
    /// it lived in while it had a single link. `listings` caches directory
    /// contents for the pass, so linking a whole tree (cp -al) reads each
    /// directory once.
    #[allow(clippy::type_complexity)]
    fn find_links(
        &self,
        ino: u64,
        known: &[Box<[u8]>],
        listings: &Mutex<HashMap<Arc<[u8]>, Arc<Vec<(u64, CString)>>>>,
    ) -> Vec<Box<[u8]>> {
        let Some(dir) = self.ps.inodes.dir_of(ino) else { return Vec::new() };
        let cached = listings.lock().unwrap().get(&dir).cloned();
        let list = match cached {
            Some(l) => l,
            None => {
                let ents = sys::open_beneath(self.src_root.as_raw_fd(), &dir, true)
                    .and_then(|fd| sys::read_dir(fd.as_raw_fd()))
                    .unwrap_or_default();
                let l: Arc<Vec<(u64, CString)>> = Arc::new(ents.into_iter().map(|e| (e.ino, e.name)).collect());
                listings.lock().unwrap().insert(dir.clone(), l.clone());
                l
            }
        };
        list.iter()
            .filter(|(i, _)| *i == ino)
            .map(|(_, n)| join(&dir, n.to_bytes()).into_boxed_slice())
            .filter(|r| !known.contains(r) && !self.filter.excluded_path(r, false))
            .collect()
    }

    /// Returns the paths that still belong to the group, and its link count.
    fn sync_link_group(&self, key: (u64, u64), paths: Vec<Box<[u8]>>) -> (Vec<Box<[u8]>>, u64) {
        if self.stopped() {
            return (paths, 0);
        }
        // Keep the names that are still links to this inode. Directories are
        // opened again when needed, so a group with thousands of links
        // doesn't hold thousands of descriptors.
        let mut members: Vec<(Box<[u8]>, Stat)> = Vec::new();
        for rel in paths {
            let (parent, name) = split(&rel);
            let Ok(name) = sys::cstring(name) else { continue };
            let Ok(sdir) = sys::open_beneath(self.src_root.as_raw_fd(), parent, true) else { continue };
            match sys::fstatat(sdir.as_raw_fd(), &name) {
                Ok(st) if (st.dev, st.ino) == key && st.is_reg() => members.push((rel, st)),
                _ => {}
            }
        }
        let nlink = members.iter().map(|m| m.1.nlink).max().unwrap_or(0);
        let rels = |m: Vec<(Box<[u8]>, Stat)>| m.into_iter().map(|m| m.0).collect();
        let Some((mrel, mst)) = members.first() else { return (Vec::new(), 0) };
        let (mp, mn) = split(mrel);
        let mname = sys::cstring(mn).unwrap();
        let Ok(msdir) = sys::open_beneath(self.src_root.as_raw_fd(), mp, true) else { return (rels(members), nlink) };
        let Some(mddir) = self.dst_parent(mp, mrel) else { return (rels(members), nlink) };
        let mdir = mddir.as_ref().map(|d| d.as_raw_fd());
        let mdst = mdir.and_then(|d| sys::fstatat(d, &mname).ok());
        if self.sync_file(msdir.as_raw_fd(), mdir, &mname, mst, mrel, mdst) {
            self.touch(mp);
        }
        let master = mdir.and_then(|d| sys::fstatat(d, &mname).ok());
        // Only link the other names to a copy that is up to date; if the
        // copy failed they keep what they have.
        let master_ok = master.is_some_and(|m| m.is_reg() && m.size == mst.size && m.mtime == mst.mtime);
        // If that's because the file changed while being copied (rather than
        // an error), the other names are retried later instead of being left
        // with old data.
        let retry = !master_ok
            && !self.full
            && sys::fstatat(msdir.as_raw_fd(), &mname).is_ok_and(|s| s.size != mst.size || s.mtime != mst.mtime);
        for (orel, _) in &members[1..] {
            let (op, on) = split(orel);
            let oname = sys::cstring(on).unwrap();
            let Some(oddir) = self.dst_parent(op, orel) else { continue };
            let odir = oddir.as_ref().map(|d| d.as_raw_fd());
            let odst = odir.and_then(|d| sys::fstatat(d, &oname).ok());
            if let (Some(a), Some(b)) = (&master, &odst)
                && (a.dev, a.ino) == (b.dev, b.ino)
            {
                continue;
            }
            let item = Item::new(b'h', b'f');
            let extra = format!(" => {}", show(mrel));
            if self.dry() {
                self.e.stats.linked.fetch_add(1, Relaxed);
                self.emit(&item, orel, &extra);
                continue;
            }
            if retry {
                self.redo.lock().unwrap().push(orel.to_vec());
            }
            let (Some(md), Some(od), true) = (mdir, odir, master_ok) else { continue };
            match self.place(od, &oname, orel, odst, |dir, n| sys::linkat(md, &mname, dir, n, 0)) {
                Ok(()) => {
                    self.e.stats.linked.fetch_add(1, Relaxed);
                    self.emit(&item, orel, &extra);
                    self.touch(op);
                }
                Err(e) => self.error(orel, "hard link", &e),
            }
        }
        (rels(members), nlink)
    }

    /// Opens the destination directory `parent`. Outer None: it's missing
    /// (queued for a full sync) or failed; inner None: dry run, not there.
    fn dst_parent(&self, parent: &[u8], rel: &[u8]) -> Option<Option<OwnedFd>> {
        let root = self.dst_root.as_ref()?;
        match sys::open_beneath(root.as_raw_fd(), parent, false) {
            Ok(d) => Some(Some(d)),
            Err(e) if self.dry() && sys::errno(&e) == libc::ENOENT => Some(None),
            Err(e) if matches!(sys::errno(&e), libc::ENOENT | libc::ENOTDIR | libc::ELOOP) => {
                self.escalate_missing(parent);
                None
            }
            Err(e) => {
                self.error(rel, "open destination parent", &e);
                None
            }
        }
    }

    /// Restores the mtime of destination directories whose contents changed
    /// after the directory itself was synced.
    fn restore_dir_times(&self) {
        let dirs: Vec<Vec<u8>> = std::mem::take(&mut *self.touched.lock().unwrap()).into_iter().collect();
        let Some(droot) = &self.dst_root else { return };
        if self.dry() {
            return;
        }
        dirs.par_iter().for_each(|rel| {
            let Ok(s) = sys::open_beneath(self.src_root.as_raw_fd(), rel, true) else { return };
            let Ok(d) = sys::open_beneath(droot.as_raw_fd(), rel, false) else { return };
            let (Ok(ss), Ok(ds)) = (sys::fstat(s.as_raw_fd()), sys::fstat(d.as_raw_fd())) else { return };
            if ss.mtime != ds.mtime
                && let Err(e) = sys::futimens(d.as_raw_fd(), ss.atime, ss.mtime)
            {
                self.error(rel, "set times", &e);
            }
        });
    }

    // ---- incremental syncs ------------------------------------------------------

    /// Brings one path up to date, as reported by a filesystem event.
    fn sync_path(&self, rel: &[u8], flags: u8) {
        if self.stopped() {
            return;
        }
        if rel.is_empty() {
            return self.sync_root(flags & REC != 0);
        }
        if self.filter.parent_excluded(rel) {
            return;
        }
        let (parent, name) = split(rel);
        let Ok(cname) = sys::cstring(name) else { return };
        // If the source parent is gone, its own deletion event covers this.
        let sdir = match sys::open_beneath(self.src_root.as_raw_fd(), parent, true) {
            Ok(fd) => fd,
            Err(e) => {
                dlog!("{}: skipping {}: {e}", self.ps.pair.name, show(rel));
                return self.recheck_links(rel, None);
            }
        };
        // Stay on the source's filesystem (nested btrfs subvolumes have their
        // own device numbers but no mount).
        if !sys::fstat(sdir.as_raw_fd()).is_ok_and(|s| s.dev == self.src_dev) {
            return;
        }
        let droot = self.dst_root.as_ref().expect("checked by sync_paths").as_raw_fd();
        let ddir = match sys::open_beneath(droot, parent, false) {
            Ok(fd) => fd,
            Err(e)
                if flags & ESCALATED == 0 && matches!(sys::errno(&e), libc::ENOENT | libc::ENOTDIR | libc::ELOOP) =>
            {
                return self.escalate_missing(parent);
            }
            Err(e) => return self.error(rel, "open destination parent", &e),
        };
        self.touch(parent);
        let (sd, dd) = (sdir.as_raw_fd(), ddir.as_raw_fd());
        match sys::fstatat(sd, &cname) {
            Ok(st) => {
                if !st.is_dir() {
                    self.recheck_links(rel, Some((st.dev, st.ino)));
                }
                if !self.filter.excluded(rel, st.is_dir()) {
                    self.sync_stat(sd, Some(dd), &cname, &st, rel, None, flags & REC != 0);
                }
            }
            Err(e) if matches!(sys::errno(&e), libc::ENOENT | libc::ENOTDIR) => {
                self.recheck_links(rel, None);
                if self.e.opts.delete
                    && let Ok(d) = sys::fstatat(dd, &cname)
                    && !self.filter.excluded(rel, d.is_dir())
                {
                    self.delete_entry(dd, &cname, rel);
                }
            }
            Err(e) => self.src_error(rel, "stat", &e),
        }
    }

    /// The destination lacks a parent directory of a changed path, so sync
    /// the topmost missing one in full.
    fn escalate_missing(&self, parent: &[u8]) {
        let droot = self.dst_root.as_ref().unwrap().as_raw_fd();
        let mut end = 0;
        let target = loop {
            let next = parent[end..].iter().position(|&c| c == b'/').map_or(parent.len(), |i| end + i);
            if sys::open_beneath(droot, &parent[..next], false).is_err() || next == parent.len() {
                break parent[..next].to_vec();
            }
            end = next + 1;
        };
        self.escalate.lock().unwrap().insert(target);
    }

    fn drain_escalations(&self) {
        for _ in 0..3 {
            let esc: Vec<(Vec<u8>, u8)> =
                std::mem::take(&mut *self.escalate.lock().unwrap()).into_iter().map(|r| (r, REC | ESCALATED)).collect();
            if esc.is_empty() || self.stopped() {
                return;
            }
            collapse(esc).par_iter().for_each(|(rel, f)| self.sync_path(rel, *f));
        }
    }

    /// Mirrors renames in the destination before syncing, so moving a big
    /// file or directory doesn't mean copying it again. The sync that
    /// follows checks the result like any other entry, so a rename that no
    /// longer fits the destination's state is harmless.
    fn apply_moves(&self, moves: &[(Vec<u8>, Vec<u8>)]) {
        // Without --delete the old name stays in the destination, like rsync.
        if self.dry() || !self.e.opts.delete {
            return;
        }
        let droot = self.dst_root.as_ref().unwrap().as_raw_fd();
        for (old, new) in moves {
            if self.stopped() {
                return;
            }
            if old.is_empty() || new.is_empty() || old == new {
                continue;
            }
            if self.filter.parent_excluded(old) || self.filter.parent_excluded(new) {
                continue;
            }
            let (op, on) = split(old);
            let (np, nn) = split(new);
            let (Ok(on), Ok(nn)) = (sys::cstring(on), sys::cstring(nn)) else { continue };
            let Ok(od) = sys::open_beneath(droot, op, false) else { continue };
            let Ok(nd) = sys::open_beneath(droot, np, false) else { continue };
            let Ok(ost) = sys::fstatat(od.as_raw_fd(), &on) else { continue };
            if self.filter.excluded(old, ost.is_dir()) || self.filter.excluded(new, ost.is_dir()) {
                continue;
            }
            if let Ok(nst) = sys::fstatat(nd.as_raw_fd(), &nn)
                && (nst.is_dir() || ost.is_dir())
            {
                continue;
            }
            match sys::renameat(od.as_raw_fd(), &on, nd.as_raw_fd(), &nn) {
                Ok(()) => {
                    self.e.stats.moved.fetch_add(1, Relaxed);
                    if self.e.opts.itemize || verbose() >= 1 {
                        println!("*moving     {} => {}", show(old), show(new));
                    }
                    self.touch(op);
                    self.touch(np);
                }
                Err(e) => dlog!("{}: couldn't mirror rename of {}: {e}", self.ps.pair.name, show(old)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapse_covers_subtrees() {
        let v = vec![
            (b"a/b".to_vec(), SELF),
            (b"a".to_vec(), REC),
            (b"a-b".to_vec(), SELF),
            (b"a/c/d".to_vec(), SELF),
            (b"b".to_vec(), SELF),
            (b"b".to_vec(), REC),
            (b"b/x".to_vec(), SELF),
        ];
        let got: Vec<_> = collapse(v).into_iter().map(|(p, f)| (String::from_utf8(p).unwrap(), f)).collect();
        assert_eq!(got, vec![("a".into(), REC), ("a-b".into(), SELF), ("b".into(), SELF | REC)]);
    }

    #[test]
    fn root_covers_all() {
        let v = vec![(b"x".to_vec(), SELF), (b"".to_vec(), REC)];
        assert_eq!(collapse(v).len(), 1);
    }
}
