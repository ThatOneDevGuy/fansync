//! Thin wrappers around the Linux system calls fansync relies on.
//!
//! Almost everything works relative to directory file descriptors, so a path
//! is never re-resolved from the root and symlinks are never followed.

use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

pub type Res<T> = io::Result<T>;

fn cvt(r: libc::c_int) -> Res<libc::c_int> {
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(r) }
}

fn cvt_s(r: libc::ssize_t) -> Res<usize> {
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(r as usize) }
}

pub fn errno(e: &io::Error) -> i32 {
    e.raw_os_error().unwrap_or(0)
}

pub fn err(code: i32) -> io::Error {
    io::Error::from_raw_os_error(code)
}

pub fn cstring(b: &[u8]) -> Res<CString> {
    CString::new(b).map_err(|_| err(libc::EINVAL))
}

pub fn path_c(p: &Path) -> Res<CString> {
    cstring(p.as_os_str().as_bytes())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ts {
    pub sec: i64,
    pub nsec: i64,
}

impl Ts {
    fn raw(self) -> libc::timespec {
        libc::timespec { tv_sec: self.sec as _, tv_nsec: self.nsec as _ }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Stat {
    pub dev: u64,
    pub ino: u64,
    pub mode: u32,
    pub nlink: u64,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u64,
    pub size: u64,
    pub blocks: u64,
    pub atime: Ts,
    pub mtime: Ts,
}

impl Stat {
    // The field types differ between architectures.
    #[allow(clippy::unnecessary_cast)]
    fn from_raw(s: &libc::stat) -> Stat {
        Stat {
            dev: s.st_dev as u64,
            ino: s.st_ino as u64,
            mode: s.st_mode as u32,
            nlink: s.st_nlink as u64,
            uid: s.st_uid as u32,
            gid: s.st_gid as u32,
            rdev: s.st_rdev as u64,
            size: s.st_size as u64,
            blocks: s.st_blocks as u64,
            atime: Ts { sec: s.st_atime as i64, nsec: s.st_atime_nsec as i64 },
            mtime: Ts { sec: s.st_mtime as i64, nsec: s.st_mtime_nsec as i64 },
        }
    }
    pub fn kind(&self) -> u32 {
        self.mode & libc::S_IFMT
    }
    pub fn is_dir(&self) -> bool {
        self.kind() == libc::S_IFDIR
    }
    pub fn is_reg(&self) -> bool {
        self.kind() == libc::S_IFREG
    }
    pub fn is_lnk(&self) -> bool {
        self.kind() == libc::S_IFLNK
    }
    pub fn perms(&self) -> u32 {
        self.mode & 0o7777
    }
}

pub fn fstatat(dir: RawFd, name: &CStr) -> Res<Stat> {
    let mut st = MaybeUninit::<libc::stat>::uninit();
    cvt(unsafe { libc::fstatat(dir, name.as_ptr(), st.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) })?;
    Ok(Stat::from_raw(&unsafe { st.assume_init() }))
}

pub fn fstat(fd: RawFd) -> Res<Stat> {
    let mut st = MaybeUninit::<libc::stat>::uninit();
    cvt(unsafe { libc::fstat(fd, st.as_mut_ptr()) })?;
    Ok(Stat::from_raw(&unsafe { st.assume_init() }))
}

pub fn lstat(path: &CStr) -> Res<Stat> {
    fstatat(libc::AT_FDCWD, path)
}

pub fn openat(dir: RawFd, name: &CStr, flags: i32, mode: u32) -> Res<OwnedFd> {
    let fd = cvt(unsafe { libc::openat(dir, name.as_ptr(), flags | libc::O_CLOEXEC, mode as libc::c_uint) })?;
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

// O_NOATIME needs CAP_FOWNER (or owning the file); after the first refusal
// stop asking for it.
static NOATIME: AtomicBool = AtomicBool::new(true);

fn open_noatime(dir: RawFd, name: &CStr, flags: i32) -> Res<OwnedFd> {
    if NOATIME.load(Relaxed) {
        match openat(dir, name, flags | libc::O_NOATIME, 0) {
            Err(e) if errno(&e) == libc::EPERM => NOATIME.store(false, Relaxed),
            r => return r,
        }
    }
    openat(dir, name, flags, 0)
}

/// Opens a subdirectory without following symlinks.
pub fn open_dir(dir: RawFd, name: &CStr) -> Res<OwnedFd> {
    open_noatime(dir, name, libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW)
}

/// Opens a regular file for reading without following symlinks or updating
/// its access time.
pub fn open_read(dir: RawFd, name: &CStr) -> Res<OwnedFd> {
    open_noatime(dir, name, libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
}

/// Opens a directory by absolute path, following symlinks like rsync does
/// for the top of a transfer.
pub fn open_dir_path(p: &Path) -> Res<OwnedFd> {
    openat(libc::AT_FDCWD, &path_c(p)?, libc::O_RDONLY | libc::O_DIRECTORY, 0)
}

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}
const RESOLVE_NO_XDEV: u64 = 0x01;
const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
const RESOLVE_NO_SYMLINKS: u64 = 0x04;
const RESOLVE_BENEATH: u64 = 0x08;
static OPENAT2: AtomicBool = AtomicBool::new(true);

/// Opens the directory `rel` below `root` without following any symlink on
/// the way. With `same_mount` it also refuses to cross a mount point.
pub fn open_beneath(root: RawFd, rel: &[u8], same_mount: bool) -> Res<OwnedFd> {
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    if rel.is_empty() {
        return openat(root, c".", flags, 0);
    }
    if OPENAT2.load(Relaxed) {
        let path = cstring(rel)?;
        let mut resolve = RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS;
        if same_mount {
            resolve |= RESOLVE_NO_XDEV;
        }
        let how = OpenHow { flags: flags as u64, mode: 0, resolve };
        let r = unsafe {
            libc::syscall(libc::SYS_openat2, root, path.as_ptr(), &how as *const OpenHow, size_of::<OpenHow>())
        };
        if r >= 0 {
            return Ok(unsafe { OwnedFd::from_raw_fd(r as RawFd) });
        }
        let e = io::Error::last_os_error();
        match errno(&e) {
            libc::ENOSYS | libc::EPERM => OPENAT2.store(false, Relaxed),
            libc::EAGAIN | libc::ENAMETOOLONG => {}
            _ => return Err(e),
        }
    }
    let mut cur = openat(root, c".", flags, 0)?;
    let dev = if same_mount { fstat(cur.as_raw_fd())?.dev } else { 0 };
    for comp in rel.split(|&c| c == b'/').filter(|c| !c.is_empty()) {
        cur = openat(cur.as_raw_fd(), &cstring(comp)?, flags, 0)?;
        if same_mount && fstat(cur.as_raw_fd())?.dev != dev {
            return Err(err(libc::EXDEV));
        }
    }
    Ok(cur)
}

pub const DT_UNKNOWN: u8 = 0;
pub const DT_DIR: u8 = 4;

pub struct DirEnt {
    pub name: CString,
    pub dtype: u8,
    pub ino: u64,
}

thread_local! {
    static DENTS: RefCell<Vec<u64>> = RefCell::new(vec![0u64; 64 * 1024 / 8]);
}

/// Lists a directory (without "." and "..") using large getdents64 batches.
pub fn read_dir(fd: RawFd) -> Res<Vec<DirEnt>> {
    if unsafe { libc::lseek(fd, 0, libc::SEEK_SET) } < 0 {
        return Err(io::Error::last_os_error());
    }
    DENTS.with(|buf| {
        let mut buf = buf.borrow_mut();
        let mut out = Vec::new();
        loop {
            let n = unsafe { libc::syscall(libc::SYS_getdents64, fd, buf.as_mut_ptr(), buf.len() * 8) };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            if n == 0 {
                return Ok(out);
            }
            let base = buf.as_ptr() as *const u8;
            let mut off = 0usize;
            while off < n as usize {
                // struct linux_dirent64 { u64 ino; i64 off; u16 reclen; u8 type; char name[]; }
                let (ino, reclen, dtype, name) = unsafe {
                    let p = base.add(off);
                    let ino = std::ptr::read_unaligned(p as *const u64);
                    let reclen = std::ptr::read_unaligned(p.add(16) as *const u16) as usize;
                    (ino, reclen, *p.add(18), CStr::from_ptr(p.add(19) as *const libc::c_char))
                };
                let nb = name.to_bytes();
                if nb != b"." && nb != b".." {
                    out.push(DirEnt { name: name.to_owned(), dtype, ino });
                }
                off += reclen;
            }
        }
    })
}

pub fn mkdirat(dir: RawFd, name: &CStr, mode: u32) -> Res<()> {
    cvt(unsafe { libc::mkdirat(dir, name.as_ptr(), mode as libc::mode_t) }).map(drop)
}

pub fn mkdir_path(p: &Path, mode: u32) -> Res<()> {
    mkdirat(libc::AT_FDCWD, &path_c(p)?, mode)
}

pub fn unlinkat(dir: RawFd, name: &CStr, rmdir: bool) -> Res<()> {
    let flags = if rmdir { libc::AT_REMOVEDIR } else { 0 };
    cvt(unsafe { libc::unlinkat(dir, name.as_ptr(), flags) }).map(drop)
}

pub fn renameat(odir: RawFd, oname: &CStr, ndir: RawFd, nname: &CStr) -> Res<()> {
    cvt(unsafe { libc::renameat(odir, oname.as_ptr(), ndir, nname.as_ptr()) }).map(drop)
}

pub fn linkat(odir: RawFd, oname: &CStr, ndir: RawFd, nname: &CStr, flags: i32) -> Res<()> {
    cvt(unsafe { libc::linkat(odir, oname.as_ptr(), ndir, nname.as_ptr(), flags) }).map(drop)
}

pub fn symlinkat(target: &CStr, dir: RawFd, name: &CStr) -> Res<()> {
    cvt(unsafe { libc::symlinkat(target.as_ptr(), dir, name.as_ptr()) }).map(drop)
}

pub fn readlinkat(dir: RawFd, name: &CStr) -> Res<Vec<u8>> {
    let mut buf = vec![0u8; 256];
    loop {
        let n =
            cvt_s(unsafe { libc::readlinkat(dir, name.as_ptr(), buf.as_mut_ptr() as *mut libc::c_char, buf.len()) })?;
        if n < buf.len() {
            buf.truncate(n);
            return Ok(buf);
        }
        buf.resize(buf.len() * 4, 0);
    }
}

pub fn mknodat(dir: RawFd, name: &CStr, mode: u32, rdev: u64) -> Res<()> {
    cvt(unsafe { libc::mknodat(dir, name.as_ptr(), mode as libc::mode_t, rdev as libc::dev_t) }).map(drop)
}

pub fn fchownat(dir: RawFd, name: &CStr, uid: u32, gid: u32) -> Res<()> {
    cvt(unsafe { libc::fchownat(dir, name.as_ptr(), uid, gid, libc::AT_SYMLINK_NOFOLLOW) }).map(drop)
}

pub fn fchown(fd: RawFd, uid: u32, gid: u32) -> Res<()> {
    cvt(unsafe { libc::fchown(fd, uid, gid) }).map(drop)
}

static FCHMODAT2: AtomicBool = AtomicBool::new(true);

/// chmod on a directory entry without following a symlink, so an entry
/// swapped for a symlink can't redirect the chmod elsewhere.
pub fn fchmodat(dir: RawFd, name: &CStr, mode: u32) -> Res<()> {
    if FCHMODAT2.load(Relaxed) {
        // fchmodat2 (Linux 6.6) honours AT_SYMLINK_NOFOLLOW.
        let r = unsafe { libc::syscall(libc::SYS_fchmodat2, dir, name.as_ptr(), mode, libc::AT_SYMLINK_NOFOLLOW) };
        if r == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if !matches!(errno(&e), libc::ENOSYS | libc::EPERM) {
            return Err(e);
        }
        FCHMODAT2.store(false, Relaxed);
    }
    // Older kernels: pin the inode with O_PATH, make sure it isn't a
    // symlink, and chmod it through /proc.
    let fd = openat(dir, name, libc::O_PATH | libc::O_NOFOLLOW, 0)?;
    if fstat(fd.as_raw_fd())?.is_lnk() {
        return Err(err(libc::ELOOP));
    }
    cvt(unsafe { libc::chmod(fd_path(fd.as_raw_fd()).as_ptr(), mode as libc::mode_t) }).map(drop)
}

pub fn fchmod(fd: RawFd, mode: u32) -> Res<()> {
    cvt(unsafe { libc::fchmod(fd, mode as libc::mode_t) }).map(drop)
}

pub fn utimensat(dir: RawFd, name: &CStr, atime: Ts, mtime: Ts) -> Res<()> {
    let ts = [atime.raw(), mtime.raw()];
    cvt(unsafe { libc::utimensat(dir, name.as_ptr(), ts.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) }).map(drop)
}

pub fn futimens(fd: RawFd, atime: Ts, mtime: Ts) -> Res<()> {
    let ts = [atime.raw(), mtime.raw()];
    cvt(unsafe { libc::futimens(fd, ts.as_ptr()) }).map(drop)
}

pub fn ftruncate(fd: RawFd, len: u64) -> Res<()> {
    cvt(unsafe { libc::ftruncate(fd, len as libc::off_t) }).map(drop)
}

pub fn lseek(fd: RawFd, off: u64, whence: i32) -> Res<u64> {
    let r = unsafe { libc::lseek(fd, off as libc::off_t, whence) };
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(r as u64) }
}

pub fn copy_file_range(fd_in: RawFd, off_in: u64, fd_out: RawFd, off_out: u64, len: usize) -> Res<usize> {
    let mut i = off_in as i64;
    let mut o = off_out as i64;
    let r = unsafe {
        libc::syscall(libc::SYS_copy_file_range, fd_in, &mut i as *mut i64, fd_out, &mut o as *mut i64, len, 0u32)
    };
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(r as usize) }
}

/// Copies from `fd_in` at `off_in` to the current position of `fd_out`.
pub fn sendfile(fd_out: RawFd, fd_in: RawFd, off_in: u64, len: usize) -> Res<usize> {
    let mut off = off_in as libc::off_t;
    cvt_s(unsafe { libc::sendfile(fd_out, fd_in, &mut off, len) })
}

/// Reads until `buf` is full or end of file; returns the bytes read.
pub fn pread_full(fd: RawFd, buf: &mut [u8], off: u64) -> Res<usize> {
    let mut done = 0;
    while done < buf.len() {
        let r = unsafe {
            libc::pread(
                fd,
                buf[done..].as_mut_ptr() as *mut libc::c_void,
                buf.len() - done,
                (off + done as u64) as libc::off_t,
            )
        };
        match r {
            0 => break,
            r if r < 0 => {
                let e = io::Error::last_os_error();
                if errno(&e) != libc::EINTR {
                    return Err(e);
                }
            }
            r => done += r as usize,
        }
    }
    Ok(done)
}

pub fn pwrite_all(fd: RawFd, buf: &[u8], off: u64) -> Res<()> {
    let mut done = 0;
    while done < buf.len() {
        let r = unsafe {
            libc::pwrite(
                fd,
                buf[done..].as_ptr() as *const libc::c_void,
                buf.len() - done,
                (off + done as u64) as libc::off_t,
            )
        };
        if r < 0 {
            let e = io::Error::last_os_error();
            if errno(&e) != libc::EINTR {
                return Err(e);
            }
        } else {
            done += r as usize;
        }
    }
    Ok(())
}

pub fn punch_hole(fd: RawFd, off: u64, len: u64) -> Res<()> {
    let mode = libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE;
    cvt(unsafe { libc::fallocate(fd, mode, off as libc::off_t, len as libc::off_t) }).map(drop)
}

pub fn fadvise(fd: RawFd, advice: i32) {
    unsafe { libc::posix_fadvise(fd, 0, 0, advice) };
}

pub fn fadvise_range(fd: RawFd, off: u64, len: u64, advice: i32) {
    unsafe { libc::posix_fadvise(fd, off as libc::off_t, len as libc::off_t, advice) };
}

/// "/proc/self/fd/N/name": lets the path-only xattr calls address an entry
/// relative to a directory fd without re-resolving its full path.
pub fn proc_path(dir: RawFd, name: &CStr) -> CString {
    let mut v = format!("/proc/self/fd/{dir}/").into_bytes();
    v.extend_from_slice(name.to_bytes());
    CString::new(v).expect("file names never contain NUL")
}

pub fn fd_path(fd: RawFd) -> CString {
    CString::new(format!("/proc/self/fd/{fd}")).unwrap()
}

/// Something xattrs can be read from or written to: an open file, or a path
/// whose last component is never followed.
#[derive(Clone, Copy)]
pub enum Xt<'a> {
    Fd(RawFd),
    Path(&'a CStr),
}

/// Runs `f` with a growing buffer until the value fits.
fn sized<F: Fn(*mut u8, usize) -> isize>(f: F) -> Res<Vec<u8>> {
    let mut stack = [0u8; 1024];
    let r = f(stack.as_mut_ptr(), stack.len());
    if r >= 0 {
        return Ok(stack[..r as usize].to_vec());
    }
    let e = io::Error::last_os_error();
    if errno(&e) != libc::ERANGE {
        return Err(e);
    }
    loop {
        let need = f(std::ptr::null_mut(), 0);
        if need < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut v = vec![0u8; need as usize + 64];
        let r = f(v.as_mut_ptr(), v.len());
        if r >= 0 {
            v.truncate(r as usize);
            return Ok(v);
        }
        let e = io::Error::last_os_error();
        if errno(&e) != libc::ERANGE {
            return Err(e);
        }
    }
}

/// Returns the NUL-separated list of xattr names.
pub fn xlist(t: Xt) -> Res<Vec<u8>> {
    sized(|p, n| unsafe {
        match t {
            Xt::Fd(fd) => libc::flistxattr(fd, p as *mut libc::c_char, n),
            Xt::Path(path) => libc::llistxattr(path.as_ptr(), p as *mut libc::c_char, n),
        }
    })
}

pub fn xget(t: Xt, name: &CStr) -> Res<Vec<u8>> {
    sized(|p, n| unsafe {
        match t {
            Xt::Fd(fd) => libc::fgetxattr(fd, name.as_ptr(), p as *mut libc::c_void, n),
            Xt::Path(path) => libc::lgetxattr(path.as_ptr(), name.as_ptr(), p as *mut libc::c_void, n),
        }
    })
}

pub fn xset(t: Xt, name: &CStr, val: &[u8]) -> Res<()> {
    let v = val.as_ptr() as *const libc::c_void;
    cvt(unsafe {
        match t {
            Xt::Fd(fd) => libc::fsetxattr(fd, name.as_ptr(), v, val.len(), 0),
            Xt::Path(path) => libc::lsetxattr(path.as_ptr(), name.as_ptr(), v, val.len(), 0),
        }
    })
    .map(drop)
}

pub fn xremove(t: Xt, name: &CStr) -> Res<()> {
    cvt(unsafe {
        match t {
            Xt::Fd(fd) => libc::fremovexattr(fd, name.as_ptr()),
            Xt::Path(path) => libc::lremovexattr(path.as_ptr(), name.as_ptr()),
        }
    })
    .map(drop)
}

pub fn is_btrfs(fd: RawFd) -> bool {
    // f_type's type differs between architectures; the magic fits in 32 bits.
    const BTRFS_SUPER_MAGIC: u32 = 0x9123_683E;
    let mut s = MaybeUninit::<libc::statfs>::uninit();
    cvt(unsafe { libc::fstatfs(fd, s.as_mut_ptr()) }).is_ok()
        && unsafe { s.assume_init() }.f_type as u32 == BTRFS_SUPER_MAGIC
}

// btrfs ioctls (include/uapi/linux/btrfs.h): _IOWR(0x94, nr, size).
const BTRFS_IOC_TREE_SEARCH_V2: libc::c_ulong = 0xC070_9411;
const BTRFS_IOC_INO_LOOKUP: libc::c_ulong = 0xD000_9412;

#[repr(C)]
struct BtrfsSearchKey {
    tree_id: u64,
    min_objectid: u64,
    max_objectid: u64,
    min_offset: u64,
    max_offset: u64,
    min_transid: u64,
    max_transid: u64,
    min_type: u32,
    max_type: u32,
    nr_items: u32,
    unused: [u32; 9],
}

#[repr(C)]
struct BtrfsInoLookup {
    tree_id: u64,
    objectid: u64,
    name: [u8; 4080],
}

/// The path of directory `dir` inside the btrfs subvolume that `fd` is on:
/// "" for the subvolume's root, otherwise "a/b/". Needs CAP_SYS_ADMIN.
pub fn btrfs_dir_path(fd: RawFd, dir: u64) -> Res<Vec<u8>> {
    let mut a = Box::new(BtrfsInoLookup { tree_id: 0, objectid: dir, name: [0; 4080] });
    cvt(unsafe { libc::ioctl(fd, BTRFS_IOC_INO_LOOKUP, &mut *a as *mut BtrfsInoLookup) })?;
    let n = a.name.iter().position(|&c| c == 0).unwrap_or(a.name.len());
    let mut p = a.name[..n].to_vec();
    if !p.is_empty() && !p.ends_with(b"/") {
        p.push(b'/');
    }
    Ok(p)
}

/// Every name of inode `ino` in the btrfs subvolume that `fd` is on, as
/// (directory inode, name): btrfs keeps a back reference for each link
/// (INODE_REF items, and INODE_EXTREF ones for directories with very many
/// links to one inode). Needs CAP_SYS_ADMIN.
pub fn btrfs_links(fd: RawFd, ino: u64) -> Res<Vec<(u64, Vec<u8>)>> {
    const INODE_REF: u32 = 12;
    const INODE_EXTREF: u32 = 13;
    const KEY: usize = size_of::<BtrfsSearchKey>();
    let mut out = Vec::new();
    let mut size = 64 * 1024;
    let (mut min_type, mut min_offset) = (INODE_REF, 0u64);
    loop {
        let mut args = vec![0u64; (KEY + 8 + size).div_ceil(8)];
        let key = BtrfsSearchKey {
            tree_id: 0,
            min_objectid: ino,
            max_objectid: ino,
            min_offset,
            max_offset: u64::MAX,
            min_transid: 0,
            max_transid: u64::MAX,
            min_type,
            max_type: INODE_EXTREF,
            nr_items: u32::MAX,
            unused: [0; 9],
        };
        unsafe { std::ptr::write(args.as_mut_ptr() as *mut BtrfsSearchKey, key) };
        args[KEY / 8] = size as u64;
        if let Err(e) = cvt(unsafe { libc::ioctl(fd, BTRFS_IOC_TREE_SEARCH_V2, args.as_mut_ptr()) }) {
            // An item bigger than the buffer: the kernel says how big.
            if e.raw_os_error() == Some(libc::EOVERFLOW) && (args[KEY / 8] as usize) > size {
                size = args[KEY / 8] as usize;
                continue;
            }
            return Err(e);
        }
        let n = unsafe { (*(args.as_ptr() as *const BtrfsSearchKey)).nr_items };
        if n == 0 {
            return Ok(out);
        }
        let b = unsafe { std::slice::from_raw_parts((args.as_ptr() as *const u8).add(KEY + 8), size) };
        let u64_at = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        let u16_at = |i: usize| u16::from_le_bytes(b[i..i + 2].try_into().unwrap()) as usize;
        let mut pos = 0;
        let mut last = (min_type, min_offset);
        for _ in 0..n {
            // struct btrfs_ioctl_search_header: transid, objectid, offset, type, len.
            let (offset, typ) = (u64_at(pos + 16), u32::from_le_bytes(b[pos + 24..pos + 28].try_into().unwrap()));
            let len = u32::from_le_bytes(b[pos + 28..pos + 32].try_into().unwrap()) as usize;
            let (mut i, end) = (pos + 32, pos + 32 + len);
            while i < end {
                // btrfs_inode_ref: index, name_len, name (the key's offset is
                // the directory); btrfs_inode_extref: dir, index, name_len, name.
                let (dir, hdr) = if typ == INODE_REF { (offset, 10) } else { (u64_at(i), 18) };
                let nl = u16_at(i + hdr - 2);
                out.push((dir, b[i + hdr..i + hdr + nl].to_vec()));
                i += hdr + nl;
            }
            last = (typ, offset);
            pos = end;
        }
        // Go on after the last item found.
        (min_type, min_offset) = match last {
            (t, u64::MAX) if t >= INODE_EXTREF => return Ok(out),
            (t, u64::MAX) => (t + 1, 0),
            (t, o) => (t, o + 1),
        };
    }
}

pub fn fsid_of(fd: RawFd) -> Res<[i32; 2]> {
    let mut s = MaybeUninit::<libc::statfs>::uninit();
    cvt(unsafe { libc::fstatfs(fd, s.as_mut_ptr()) })?;
    let s = unsafe { s.assume_init() };
    // fsid_t is two ints with private fields.
    Ok(unsafe { std::mem::transmute::<libc::fsid_t, [i32; 2]>(s.f_fsid) })
}

/// The ID of the mount `fd` is on (Linux 5.8+). IDs of mounts that exist
/// at the same time never collide, which is all the callers rely on.
pub fn mount_id(fd: RawFd) -> Res<u64> {
    let mut stx = MaybeUninit::<libc::statx>::zeroed();
    cvt(unsafe { libc::statx(fd, c"".as_ptr(), libc::AT_EMPTY_PATH, libc::STATX_MNT_ID, stx.as_mut_ptr()) })?;
    let stx = unsafe { stx.assume_init() };
    if stx.stx_mask & libc::STATX_MNT_ID == 0 {
        return Err(err(libc::EOPNOTSUPP));
    }
    Ok(stx.stx_mnt_id)
}

/// Whether directory `fd` is on mount `mnt`. A bind mount of the same
/// filesystem keeps the device number, so only the mount ID shows it.
/// Without mount IDs (or a reference mount) it can't tell, and says yes.
pub fn on_mount(fd: RawFd, mnt: Option<u64>) -> bool {
    match (mnt, mount_id(fd)) {
        (Some(a), Ok(b)) => a == b,
        _ => true,
    }
}

pub fn readlink_path(p: &CStr) -> Res<Vec<u8>> {
    readlinkat(libc::AT_FDCWD, p)
}

/// Raises the open file limit to the hard limit; a parallel tree walk keeps
/// many directories open at once.
pub fn raise_nofile() {
    unsafe {
        let mut r = MaybeUninit::<libc::rlimit>::uninit();
        if libc::getrlimit(libc::RLIMIT_NOFILE, r.as_mut_ptr()) == 0 {
            let mut r = r.assume_init();
            r.rlim_cur = r.rlim_max;
            libc::setrlimit(libc::RLIMIT_NOFILE, &r);
        }
    }
}

/// Lowest CPU priority and lowest best-effort I/O priority. Threads created
/// afterwards inherit both.
pub fn lower_priority() {
    const IOPRIO_WHO_PROCESS: i32 = 1;
    const IOPRIO_CLASS_BE: i32 = 2;
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 19);
        libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, (IOPRIO_CLASS_BE << 13) | 7);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn chmod_never_follows_symlinks() {
        let d = tempfile::tempdir().unwrap();
        let target = d.path().join("target");
        std::fs::write(&target, b"x").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::os::unix::fs::symlink(&target, d.path().join("link")).unwrap();
        let dir = open_dir_path(d.path()).unwrap();
        assert!(fchmodat(dir.as_raw_fd(), c"link", 0o4755).is_err());
        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o644);
        fchmodat(dir.as_raw_fd(), c"target", 0o600).unwrap();
        assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o7777, 0o600);
    }
}
