//! Change notification with fanotify.
//!
//! A single FAN_MARK_FILESYSTEM mark per source filesystem reports every
//! change on it, however many directories there are; inotify would need a
//! watch per directory. Each event carries a handle to the parent directory
//! plus the entry's name, and handles are turned back into paths with
//! open_by_handle_at.

use crate::engine::{ENTRY, REC, SELF};
use crate::sys::{DT_DIR, DT_UNKNOWN};
use crate::{dlog, log, sys, vlog};
use std::collections::HashMap;
use std::ffi::CStr;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::Sender;
use std::time::Duration;

pub enum Ev {
    /// A path, relative to a pair's source, that changed.
    Dirty(usize, Vec<u8>, u8),
    /// A rename within one pair: old path, new path.
    Move(usize, Vec<u8>, Vec<u8>),
    /// A file whose link count changed, or that is multiply linked: its
    /// inode number on the pair's filesystem. All its names get synced.
    Links(usize, u64),
    /// The kernel dropped events, so only a full pass is safe.
    Overflow,
}

struct Root {
    pair: usize,
    path: Vec<u8>,
    /// The source's device; btrfs subvolumes on one mark each have their own.
    dev: u64,
}

/// Sources reachable through one mount; handles are opened through it.
struct Mount {
    fsid: [i32; 2],
    mnt_id: u64,
    fd: OwnedFd,
    roots: Vec<Root>,
}

pub struct Watcher {
    fan: OwnedFd,
    mounts: Vec<Mount>,
    /// Which pairs are covered by a mark.
    pub watched: Vec<bool>,
    /// Set when handles can't be opened, see `DirIndex`.
    index: Option<DirIndex>,
    /// The index was rebuilt during the current batch of events.
    rebuilt: bool,
    /// Events also carry the handle of the created, moved or deleted entry
    /// (FAN_REPORT_TARGET_FID, Linux 5.17), which keeps the index exact.
    target: bool,
}

// FAN_MODIFY catches writes to files that stay open (logs, databases, VM
// images); FAN_CLOSE_WRITE additionally catches files written through mmap
// once they're closed.
const BASE_MASK: u64 =
    libc::FAN_CREATE | libc::FAN_DELETE | libc::FAN_MODIFY | libc::FAN_ATTRIB | libc::FAN_CLOSE_WRITE | libc::FAN_ONDIR;

type Rec<'a> = ([i32; 2], &'a [u8], Option<&'a [u8]>);
type Located = Vec<(usize, Vec<u8>)>;

impl Watcher {
    /// `srcs`: (pair index, canonical source directory).
    pub fn new(srcs: &[(usize, PathBuf)], npairs: usize) -> io::Result<Watcher> {
        let base = libc::FAN_CLASS_NOTIF | libc::FAN_CLOEXEC | libc::FAN_NONBLOCK;
        let ev_flags = (libc::O_RDONLY | libc::O_CLOEXEC | libc::O_LARGEFILE) as u32;
        // Unlimited queues need CAP_SYS_ADMIN in the initial namespace; a
        // limited one can overflow under a burst, which costs a full pass.
        // Target handles need Linux 5.17.
        let mut fd = -1;
        let mut target = false;
        for (report, unlimited) in [
            (libc::FAN_REPORT_DFID_NAME_TARGET, true),
            (libc::FAN_REPORT_DFID_NAME_TARGET, false),
            (libc::FAN_REPORT_DFID_NAME, true),
            (libc::FAN_REPORT_DFID_NAME, false),
        ] {
            let q = if unlimited { libc::FAN_UNLIMITED_QUEUE } else { 0 };
            fd = unsafe { libc::fanotify_init(base | report | q, ev_flags) };
            if fd >= 0 {
                target = report == libc::FAN_REPORT_DFID_NAME_TARGET;
                if !unlimited {
                    dlog!("fanotify: using a limited event queue");
                }
                break;
            }
        }
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fan = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut w =
            Watcher { fan, mounts: Vec::new(), watched: vec![false; npairs], index: None, rebuilt: false, target };
        // FAN_RENAME (Linux 5.17) reports both ends of a rename in one
        // event, which lets the destination mirror the rename instead of
        // copying the moved data again.
        let mut mask = BASE_MASK | libc::FAN_RENAME;
        for (pair, src) in srcs {
            match w.watch(*pair, src, &mut mask) {
                Ok(()) => w.watched[*pair] = true,
                Err(e) => {
                    log!("warning: can't watch {} ({e}); it will get a full pass periodically instead", src.display())
                }
            }
        }
        if let Some(m) = w.mounts.first()
            && let Some(key) = handle_at(m.fd.as_raw_fd(), c"", m.fsid)
            && open_handle(m.fd.as_raw_fd(), &key[8..], true).is_err_and(|e| e.raw_os_error() == Some(libc::EPERM))
        {
            log!(
                "note: open_by_handle_at isn't permitted here (missing CAP_DAC_READ_SEARCH, or a container's seccomp policy); indexing source directories instead"
            );
            w.index = Some(DirIndex::default());
        }
        Ok(w)
    }

    /// Marks the filesystem that source `src` is on, and records the source.
    fn watch(&mut self, pair: usize, src: &Path, mask: &mut u64) -> io::Result<()> {
        let open = |p: &Path| sys::openat(libc::AT_FDCWD, &sys::path_c(p)?, libc::O_RDONLY | libc::O_DIRECTORY, 0);
        let fd = open(src)?;
        let st = sys::fstat(fd.as_raw_fd())?;
        let mnt_id = sys::mount_id(fd.as_raw_fd()).ok();
        // Each btrfs subvolume has its own fsid, and since Linux 6.8 the
        // kernel refuses filesystem marks (EXDEV) where the fsid isn't the
        // top-level subvolume's. A filesystem mark covers every subvolume
        // anyway, so a directory of the top level does the same job. Look for
        // one above the source, on the same mount, so on the same filesystem;
        // there is one when the top level is what's mounted (subvolid=5).
        let mut at = src;
        let mut above: Option<OwnedFd> = None;
        loop {
            let e = match self.mark(above.as_ref().unwrap_or(&fd).as_raw_fd(), mask) {
                Ok(()) => break,
                Err(e) => e,
            };
            if e.raw_os_error() != Some(libc::EXDEV) {
                return Err(e);
            }
            match at.parent().and_then(|p| open(p).ok().map(|f| (p, f))) {
                Some((p, f)) if mnt_id.is_some() && sys::mount_id(f.as_raw_fd()).ok() == mnt_id => {
                    at = p;
                    above = Some(f);
                }
                _ => {
                    return Err(io::Error::other(format!(
                        "{e}: the kernel only watches a btrfs subvolume through the filesystem's top level, and this mount doesn't include it; mount the top level (subvolid=5) and give the source path through it"
                    )));
                }
            }
        }
        // Events carry the fsid of what was marked, not the subvolume's.
        let fsid = sys::fsid_of(above.as_ref().unwrap_or(&fd).as_raw_fd())?;
        if at != src {
            vlog!("{}: a btrfs subvolume, watched through {}", src.display(), at.display());
        }
        let root = Root { pair, path: src.as_os_str().as_bytes().to_vec(), dev: st.dev };
        let mnt_id = mnt_id.unwrap_or(0);
        match self.mounts.iter_mut().find(|m| m.fsid == fsid && m.mnt_id == mnt_id) {
            Some(m) => m.roots.push(root),
            None => self.mounts.push(Mount { fsid, mnt_id, fd, roots: vec![root] }),
        }
        Ok(())
    }

    /// Adds a filesystem mark through directory `fd`. Before Linux 5.17,
    /// switches `mask` from FAN_RENAME to separate move events.
    fn mark(&self, fd: RawFd, mask: &mut u64) -> io::Result<()> {
        let add = |mask| unsafe {
            libc::fanotify_mark(
                self.fan.as_raw_fd(),
                libc::FAN_MARK_ADD | libc::FAN_MARK_FILESYSTEM,
                mask,
                fd,
                std::ptr::null(),
            )
        };
        let mut r = add(*mask);
        if r < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL) && *mask & libc::FAN_RENAME != 0 {
            *mask = BASE_MASK | libc::FAN_MOVED_FROM | libc::FAN_MOVED_TO;
            r = add(*mask);
        }
        if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    /// Walks the sources and indexes every directory from scratch.
    fn rebuild_index(&mut self) {
        let Some(idx) = &mut self.index else { return };
        let t0 = std::time::Instant::now();
        *idx = DirIndex::default();
        for m in &self.mounts {
            for r in &m.roots {
                if let Ok(fd) = sys::open_dir_path(std::path::Path::new(std::ffi::OsStr::from_bytes(&r.path))) {
                    idx.add_tree(NONE, &r.path, fd, m.fsid);
                }
            }
        }
        dlog!("fanotify: indexed {} directories in {:?}", idx.nodes.len(), t0.elapsed());
    }

    pub fn run(mut self, tx: Sender<Vec<Ev>>, stop: Arc<AtomicBool>) {
        self.rebuild_index();
        let mut buf = vec![0u64; 256 * 1024 / 8];
        let cap = buf.len() * 8;
        let fd = self.fan.as_raw_fd();
        while !stop.load(Relaxed) {
            let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
            if unsafe { libc::poll(&mut pfd, 1, 500) } <= 0 {
                continue;
            }
            // Directory handles resolved during this batch.
            let mut cache: HashMap<Vec<u8>, Located> = HashMap::new();
            self.rebuilt = false;
            let mut out = Vec::new();
            loop {
                let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, cap) };
                if n < 0 {
                    let e = io::Error::last_os_error();
                    match e.raw_os_error() {
                        Some(libc::EAGAIN) => break,
                        Some(libc::EINTR) => continue,
                        _ => {
                            log!("error: reading fanotify events failed: {e}");
                            return;
                        }
                    }
                }
                let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, n as usize) };
                self.parse(bytes, &mut cache, &mut out);
                if (n as usize) < cap / 2 {
                    break;
                }
            }
            if !out.is_empty() && tx.send(out).is_err() {
                return;
            }
            // Give the kernel a moment to merge repeats (say, many small writes
            // to one file) into single events before reading again.
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn parse(&mut self, b: &[u8], cache: &mut HashMap<Vec<u8>, Located>, out: &mut Vec<Ev>) {
        const META: usize = size_of::<libc::fanotify_event_metadata>();
        let mut off = 0;
        while off + META <= b.len() {
            let m: libc::fanotify_event_metadata = unsafe { std::ptr::read_unaligned(b.as_ptr().add(off) as *const _) };
            let len = m.event_len as usize;
            if len < META || off + len > b.len() {
                break;
            }
            if m.vers != libc::FANOTIFY_METADATA_VERSION {
                log!("error: unexpected fanotify metadata version {}", m.vers);
                return;
            }
            if m.fd >= 0 {
                unsafe { libc::close(m.fd) };
            }
            let info = &b[(off + m.metadata_len as usize).min(off + len)..off + len];
            self.event(m.mask, info, cache, out);
            off += len;
        }
    }

    fn event(&mut self, mask: u64, mut info: &[u8], cache: &mut HashMap<Vec<u8>, Located>, out: &mut Vec<Ev>) {
        if mask & libc::FAN_Q_OVERFLOW != 0 {
            out.push(Ev::Overflow);
            return;
        }
        let (mut main, mut old, mut new, mut target) = (None, None, None, None);
        let mut target_fsid = [0; 2];
        while info.len() >= 4 {
            let typ = info[0];
            let len = u16::from_ne_bytes([info[2], info[3]]) as usize;
            if len < 4 || len > info.len() {
                break;
            }
            let rec = &info[..len];
            info = &info[len..];
            // header(4) fsid(8) struct file_handle { u32 bytes; i32 type; u8 handle[] } [name\0]
            if rec.len() < 20 {
                continue;
            }
            let word = |i: usize| i32::from_ne_bytes(rec[i..i + 4].try_into().unwrap());
            let fsid = [word(4), word(8)];
            let hend = 20 + word(12) as u32 as usize;
            if hend > rec.len() {
                continue;
            }
            let handle = &rec[12..hend];
            let name = match typ {
                libc::FAN_EVENT_INFO_TYPE_DFID_NAME
                | libc::FAN_EVENT_INFO_TYPE_OLD_DFID_NAME
                | libc::FAN_EVENT_INFO_TYPE_NEW_DFID_NAME => {
                    let n = &rec[hend..];
                    Some(&n[..n.iter().position(|&c| c == 0).unwrap_or(n.len())])
                }
                _ => None,
            };
            let r: Rec = (fsid, handle, name);
            match typ {
                libc::FAN_EVENT_INFO_TYPE_OLD_DFID_NAME => old = Some(r),
                libc::FAN_EVENT_INFO_TYPE_NEW_DFID_NAME => new = Some(r),
                libc::FAN_EVENT_INFO_TYPE_DFID | libc::FAN_EVENT_INFO_TYPE_DFID_NAME => {
                    main.get_or_insert(r);
                }
                // The entry itself.
                libc::FAN_EVENT_INFO_TYPE_FID => {
                    target.get_or_insert(handle);
                    target_fsid = fsid;
                }
                _ => {}
            }
        }
        let ondir = mask & libc::FAN_ONDIR != 0;
        // A file's link count changed (link, unlink, or a rename over one of
        // its names): reported with no directory, only the file's handle.
        // Its other names got no events, so look the file up and have all
        // its names synced. This is how a change made through a name that
        // is gone by the time it's synced still reaches the other names.
        if main.is_none()
            && !ondir
            && mask & libc::FAN_ATTRIB != 0
            && self.index.is_none()
            && let Some(t) = target
        {
            self.link_count_changed(target_fsid, t, out);
        }
        if mask & libc::FAN_RENAME != 0 {
            let olds = old.map(|r| self.locate(r, cache)).unwrap_or_default();
            let news = new.map(|r| self.locate(r, cache)).unwrap_or_default();
            if crate::util::verbose() >= 3 {
                for ((_, o), (_, n)) in olds.iter().zip(&news) {
                    crate::util::log_line(format_args!(
                        "event rename {} -> {}",
                        crate::util::show(o),
                        crate::util::show(n)
                    ));
                }
            }
            for (po, ro) in &olds {
                for (pn, rn) in &news {
                    if po == pn {
                        out.push(Ev::Move(*po, ro.clone(), rn.clone()));
                    }
                }
            }
            for (p, r) in olds {
                out.push(Ev::Dirty(p, r, SELF | ENTRY));
            }
            for (p, r) in news {
                out.push(Ev::Dirty(p, r, ENTRY | if ondir { REC } else { SELF }));
            }
            if ondir && let Some((fsid, h, Some(n))) = new {
                self.index_dir(fsid, h, n, target, true);
            }
        }
        if mask & !(libc::FAN_RENAME | libc::FAN_ONDIR) != 0
            && let Some(r) = main
        {
            // A directory that appears may already have contents (moved in,
            // or filled before we looked), so it is synced recursively.
            let appeared = ondir && mask & (libc::FAN_CREATE | libc::FAN_MOVED_TO) != 0;
            let entry = mask & (libc::FAN_CREATE | libc::FAN_DELETE | libc::FAN_MOVED_FROM | libc::FAN_MOVED_TO) != 0;
            for (p, rel) in self.locate(r, cache) {
                if crate::util::verbose() >= 3 {
                    crate::util::log_line(format_args!("event {mask:#x} {}", crate::util::show(&rel)));
                }
                // Without open_by_handle_at the link-count events above can't
                // be used, so note multiply-linked files as their names show
                // up instead (they may be gone before they're synced).
                if !ondir && self.index.is_some() && r.2.is_some_and(|n| n != b".") {
                    self.note_links(p, &rel, cache, out);
                }
                let f = if appeared { REC } else { SELF };
                out.push(Ev::Dirty(p, rel, if entry { f | ENTRY } else { f }));
            }
            if appeared && let (fsid, h, Some(n)) = r {
                self.index_dir(fsid, h, n, target, mask & libc::FAN_MOVED_TO != 0);
            }
            if ondir
                && mask & libc::FAN_DELETE != 0
                && let (Some(idx), Some(t)) = (&mut self.index, target)
            {
                idx.ids.remove(&key(r.0, t));
            }
        }
    }

    fn link_count_changed(&self, fsid: [i32; 2], handle: &[u8], out: &mut Vec<Ev>) {
        let mounts = || self.mounts.iter().filter(|m| m.fsid == fsid);
        // Fails once the last name is gone: then there's nothing to sync.
        let Some(st) = mounts()
            .find_map(|m| open_handle(m.fd.as_raw_fd(), handle, false).ok())
            .and_then(|fd| sys::fstat(fd.as_raw_fd()).ok())
        else {
            return;
        };
        if st.is_reg() && st.nlink > 0 {
            // Inode numbers are only unique per device, and btrfs subvolumes
            // under one mark each have their own.
            for root in mounts().flat_map(|m| &m.roots).filter(|r| r.dev == st.dev) {
                out.push(Ev::Links(root.pair, st.ino));
            }
        }
    }

    fn note_links(&self, pair: usize, rel: &[u8], cache: &mut HashMap<Vec<u8>, Located>, out: &mut Vec<Ev>) {
        let mut ck = b"links\0".to_vec();
        ck.extend_from_slice(&pair.to_ne_bytes());
        ck.extend_from_slice(rel);
        if cache.insert(ck, Vec::new()).is_some() {
            return;
        }
        let Some(root) = self.mounts.iter().flat_map(|m| &m.roots).find(|r| r.pair == pair) else { return };
        let mut path = root.path.clone();
        if path != b"/" {
            path.push(b'/');
        }
        path.extend_from_slice(rel);
        if let Ok(cp) = sys::cstring(&path)
            && let Ok(st) = sys::lstat(&cp)
            && st.is_reg()
            && st.nlink > 1
        {
            out.push(Ev::Links(pair, st.ino));
        }
    }

    /// Keeps the fallback index current when directory `name` appears in
    /// `parent`, created or moved there.
    fn index_dir(&mut self, fsid: [i32; 2], parent: &[u8], name: &[u8], target: Option<&[u8]>, moved: bool) {
        let Some(idx) = &mut self.index else { return };
        let Some(&pid) = idx.ids.get(&key(fsid, parent)) else { return };
        // With the directory's own handle in the event, record exactly what
        // happened, in event order, without looking at the filesystem (where
        // things may have moved on already).
        if let Some(t) = target {
            let k = key(fsid, t);
            let known = idx.ids.contains_key(&k);
            idx.insert(k, pid, name);
            // Moved in from outside the indexed sources: index what it contains.
            if !moved || known {
                return;
            }
        }
        let Some(ppath) = idx.path(pid) else { return };
        let mut full = ppath;
        if full != b"/" {
            full.push(b'/');
        }
        full.extend_from_slice(name);
        if let Ok(fd) = sys::open_dir_path(std::path::Path::new(std::ffi::OsStr::from_bytes(&full))) {
            idx.add_tree(pid, name, fd, fsid);
        }
    }

    /// The pairs and relative paths an event refers to.
    fn locate(&mut self, (fsid, handle, name): Rec, cache: &mut HashMap<Vec<u8>, Located>) -> Located {
        let k = key(fsid, handle);
        let dirs = match cache.get(&k) {
            Some(d) => d.clone(),
            None => {
                let d = self.resolve(fsid, handle);
                cache.insert(k, d.clone());
                d
            }
        };
        match name {
            None | Some(b".") => dirs,
            Some(n) => dirs
                .into_iter()
                .map(|(p, mut d)| {
                    if !d.is_empty() {
                        d.push(b'/');
                    }
                    d.extend_from_slice(n);
                    (p, d)
                })
                .collect(),
        }
    }

    /// Finds the directory with this handle and returns its path relative
    /// to every watched source that contains it.
    fn resolve(&mut self, fsid: [i32; 2], handle: &[u8]) -> Located {
        let mut out = Vec::new();
        if self.index.is_some() {
            let k = key(fsid, handle);
            // The index can be out of date (a directory moved before the event
            // that would have told us where it went was read). Then rebuild
            // it from disk, at most once per batch, so like open_by_handle_at
            // this finds where the directory is now.
            let exact = self.target;
            let lookup = |idx: &DirIndex| {
                let path = idx.ids.get(&k).and_then(|&id| idx.path(id))?;
                if exact {
                    // Kept current from the events themselves, so it gives the
                    // path at the time of the event.
                    return Some(path);
                }
                let cp = sys::cstring(&path).ok()?;
                (handle_at(libc::AT_FDCWD, &cp, fsid).as_deref() == Some(&k[..])).then_some(path)
            };
            let mut found = lookup(self.index.as_ref().unwrap());
            if found.is_none() && !exact && !self.rebuilt {
                self.rebuilt = true;
                self.rebuild_index();
                found = lookup(self.index.as_ref().unwrap());
            }
            let Some(path) = found else {
                dlog!("fanotify: directory not found (deleted already?)");
                return out;
            };
            for m in self.mounts.iter().filter(|m| m.fsid == fsid) {
                for root in &m.roots {
                    if let Some(rel) = under(&path, &root.path) {
                        out.push((root.pair, rel.to_vec()));
                    }
                }
            }
            return out;
        }
        let known = self.mounts.iter().any(|m| m.fsid == fsid);
        for m in self.mounts.iter().filter(|m| !known || m.fsid == fsid) {
            let fd = match open_handle(m.fd.as_raw_fd(), handle, true) {
                Ok(fd) => fd,
                Err(e) => {
                    // Usually ESTALE: deleted since. Its parent's event covers it.
                    dlog!("fanotify: can't open handle: {e}");
                    continue;
                }
            };
            let Ok(path) = sys::readlink_path(&sys::fd_path(fd.as_raw_fd())) else { continue };
            // The kernel's idea of the path may not be reachable through this
            // mount (bind mounts), so make sure it leads to the same object.
            let (Ok(a), Ok(cp)) = (sys::fstat(fd.as_raw_fd()), sys::cstring(&path)) else { continue };
            if !sys::lstat(&cp).is_ok_and(|b| (b.dev, b.ino) == (a.dev, a.ino)) {
                continue;
            }
            for root in &m.roots {
                if let Some(rel) = under(&path, &root.path) {
                    out.push((root.pair, rel.to_vec()));
                }
            }
        }
        out
    }
}

fn key(fsid: [i32; 2], handle: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(8 + handle.len());
    k.extend_from_slice(&fsid[0].to_ne_bytes());
    k.extend_from_slice(&fsid[1].to_ne_bytes());
    k.extend_from_slice(handle);
    k
}

/// Opens the file with this handle (struct file_handle bytes).
fn open_handle(mount_fd: RawFd, handle: &[u8], dir: bool) -> io::Result<OwnedFd> {
    let mut h = vec![0u64; handle.len().div_ceil(8) + 1];
    unsafe { std::ptr::copy_nonoverlapping(handle.as_ptr(), h.as_mut_ptr() as *mut u8, handle.len()) };
    // O_DIRECTORY also lets unprivileged users decode directory handles on
    // their own mounts (Linux 6.10+).
    let mut flags = libc::O_PATH | libc::O_CLOEXEC;
    if dir {
        flags |= libc::O_DIRECTORY;
    }
    let fd = unsafe { libc::open_by_handle_at(mount_fd, h.as_mut_ptr() as *mut libc::file_handle, flags) };
    if fd < 0 { Err(io::Error::last_os_error()) } else { Ok(unsafe { OwnedFd::from_raw_fd(fd) }) }
}

/// The fanotify-style key (fsid + struct file_handle) of `name` in `dir`,
/// or of `dir` itself when `name` is empty.
fn handle_at(dir: RawFd, name: &CStr, fsid: [i32; 2]) -> Option<Vec<u8>> {
    const MAX_HANDLE: usize = 128;
    let mut buf = [0u64; (8 + MAX_HANDLE) / 8];
    unsafe { *(buf.as_mut_ptr() as *mut u32) = MAX_HANDLE as u32 };
    let mut mnt = 0;
    let flags = if name.is_empty() { libc::AT_EMPTY_PATH } else { 0 };
    let fh = buf.as_mut_ptr() as *mut libc::file_handle;
    if unsafe { libc::name_to_handle_at(dir, name.as_ptr(), fh, &mut mnt, flags) } < 0 {
        return None;
    }
    let len = 8 + unsafe { *(buf.as_ptr() as *const u32) } as usize;
    let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, len.min(8 + MAX_HANDLE)) };
    Some(key(fsid, bytes))
}

const NONE: usize = usize::MAX;

/// Directory handle -> path, for when open_by_handle_at isn't allowed (for
/// instance under a container's seccomp policy). Built by walking the
/// sources once, then kept current from the create and rename events, which
/// carry the parent's handle and the new name. Each node stores its parent,
/// so renaming a directory updates one node, not its whole subtree. Deleted
/// directories are left in place: their handles never come back.
#[derive(Default)]
struct DirIndex {
    ids: HashMap<Vec<u8>, usize>,
    /// (parent node, name); a source root has NONE and its absolute path.
    nodes: Vec<(usize, Box<[u8]>)>,
}

impl DirIndex {
    fn insert(&mut self, key: Vec<u8>, parent: usize, name: &[u8]) -> usize {
        match self.ids.get(&key) {
            Some(&id) => {
                // A known directory that moved.
                self.nodes[id] = (parent, name.into());
                id
            }
            None => {
                self.nodes.push((parent, name.into()));
                self.ids.insert(key, self.nodes.len() - 1);
                self.nodes.len() - 1
            }
        }
    }

    /// Indexes `fd` and every directory below it on the same filesystem.
    fn add_tree(&mut self, parent: usize, name: &[u8], fd: OwnedFd, fsid: [i32; 2]) {
        let Some(k) = handle_at(fd.as_raw_fd(), c"", fsid) else { return };
        // Walked even when the directory is known (it was renamed): a
        // subdirectory created just before the rename may have been missed,
        // because its parent's path was already stale when its event came.
        let id = self.insert(k, parent, name);
        let Ok(dev) = sys::fstat(fd.as_raw_fd()).map(|s| s.dev) else { return };
        let mnt = sys::mount_id(fd.as_raw_fd()).ok();
        let mut stack = vec![(Rc::new(fd), id)];
        while let Some((dfd, id)) = stack.pop() {
            let Ok(ents) = sys::read_dir(dfd.as_raw_fd()) else { continue };
            for e in ents.iter().filter(|e| e.dtype == DT_DIR || e.dtype == DT_UNKNOWN) {
                let Ok(sub) = sys::open_dir(dfd.as_raw_fd(), &e.name) else { continue };
                // Same filesystem and same mount, like the sync itself.
                if sys::fstat(sub.as_raw_fd()).map_or(true, |s| s.dev != dev) || !sys::on_mount(sub.as_raw_fd(), mnt) {
                    continue;
                }
                let Some(k) = handle_at(sub.as_raw_fd(), c"", fsid) else { continue };
                let cid = self.insert(k, id, e.name.to_bytes());
                stack.push((Rc::new(sub), cid));
            }
        }
    }

    fn path(&self, mut id: usize) -> Option<Vec<u8>> {
        let mut parts = Vec::new();
        loop {
            let (parent, name) = self.nodes.get(id)?;
            parts.push(&name[..]);
            if *parent == NONE {
                break;
            }
            id = *parent;
            if parts.len() > 4096 {
                return None;
            }
        }
        let mut p = parts.pop()?.to_vec();
        for n in parts.into_iter().rev() {
            if p != b"/" {
                p.push(b'/');
            }
            p.extend_from_slice(n);
        }
        Some(p)
    }
}

/// `path` relative to `root`, if it is inside it.
fn under<'p>(path: &'p [u8], root: &[u8]) -> Option<&'p [u8]> {
    if root == b"/" {
        return path.strip_prefix(b"/");
    }
    let rest = path.strip_prefix(root)?;
    if rest.is_empty() { Some(rest) } else { rest.strip_prefix(b"/") }
}

#[cfg(test)]
mod tests {
    #[test]
    fn under() {
        assert_eq!(super::under(b"/a/b", b"/"), Some(&b"a/b"[..]));
        assert_eq!(super::under(b"/", b"/"), Some(&b""[..]));
        assert_eq!(super::under(b"/a/b", b"/a"), Some(&b"b"[..]));
        assert_eq!(super::under(b"/a", b"/a"), Some(&b""[..]));
        assert_eq!(super::under(b"/ab", b"/a"), None);
    }
}
