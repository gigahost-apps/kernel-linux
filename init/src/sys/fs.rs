//! File system calls relative to directory descriptors, for the unpack
//! mode, the copy of the image's `/run` and the volume directories: every
//! path resolved goes through `openat2` with `RESOLVE_IN_ROOT`, and every
//! change is made with an `*at` call on the resolved parent and a single
//! name.

use super::{check, errno};
use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::ptr::NonNull;

/// `RESOLVE_IN_ROOT | RESOLVE_NO_MAGICLINKS | RESOLVE_NO_XDEV`: `..` and
/// absolute symbolic links stay inside the directory, `/proc`-style links
/// are refused and no mount is crossed.
pub const RESOLVE_INSIDE: u64 =
    libc::RESOLVE_IN_ROOT | libc::RESOLVE_NO_MAGICLINKS | libc::RESOLVE_NO_XDEV;

/// `RESOLVE_IN_ROOT | RESOLVE_NO_MAGICLINKS`: as [`RESOLVE_INSIDE`], but
/// crossing the mounts inside the directory (the running root's `/run`,
/// `/dev`).
pub const RESOLVE_IN_ROOT: u64 = libc::RESOLVE_IN_ROOT | libc::RESOLVE_NO_MAGICLINKS;

/// `setxattrat(2)`, Linux 6.13, x86-64; not every libc release names it.
const SYS_SETXATTRAT: libc::c_long = 463;

/// The `struct xattr_args` of `setxattrat(2)`.
#[repr(C)]
struct XattrArgs {
    value: u64,
    size: u32,
    flags: u32,
}

/// The `struct open_how` of `openat2(2)`.
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

/// What [`stat_at`] reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    /// `st_mode`: the type and the permission bits.
    pub mode: u32,
    pub dev: u64,
    pub ino: u64,
    pub uid: u32,
    pub gid: u32,
    /// `st_size`: a regular file's length in bytes.
    pub size: u64,
    /// The modification time: seconds and nanoseconds.
    pub mtime: (i64, u32),
}

impl Stat {
    pub fn is_dir(&self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFDIR
    }

    pub fn is_symlink(&self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFLNK
    }

    pub fn is_file(&self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFREG
    }
}

fn owned(rc: libc::c_int) -> io::Result<OwnedFd> {
    let fd = check(rc)?;
    // SAFETY: `fd` is a descriptor the kernel just returned to us; nobody
    // else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `openat2(dir, path, flags, RESOLVE_INSIDE)`: `path` is resolved with
/// `dir` as its root.
pub fn open_inside(dir: BorrowedFd<'_>, path: &CStr, flags: libc::c_int) -> io::Result<OwnedFd> {
    openat2(dir, path, flags, RESOLVE_INSIDE)
}

/// `openat2(dir, path, flags, RESOLVE_IN_ROOT)`: `path` is resolved with
/// `dir` as its root, mounts inside it crossed.
pub fn open_in_root(dir: BorrowedFd<'_>, path: &CStr, flags: libc::c_int) -> io::Result<OwnedFd> {
    openat2(dir, path, flags, RESOLVE_IN_ROOT)
}

fn openat2(
    dir: BorrowedFd<'_>,
    path: &CStr,
    flags: libc::c_int,
    resolve: u64,
) -> io::Result<OwnedFd> {
    let how = OpenHow {
        flags: u64::try_from(flags | libc::O_CLOEXEC).unwrap_or(0),
        mode: 0,
        resolve,
    };
    // SAFETY: `path` is a valid C string and `how` a valid open_how of the
    // size passed; the call returns a new descriptor or -1.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            dir.as_raw_fd(),
            path.as_ptr(),
            &raw const how,
            size_of::<OpenHow>(),
        )
    };
    owned(libc::c_int::try_from(rc).map_err(|_| io::Error::from(io::ErrorKind::Other))?)
}

/// `openat(dir, name, flags, mode)` of one name in `dir`.
pub fn open_at(
    dir: BorrowedFd<'_>,
    name: &CStr,
    flags: libc::c_int,
    mode: u32,
) -> io::Result<OwnedFd> {
    // SAFETY: `name` is a valid C string; the call returns a new
    // descriptor or -1.
    owned(unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC,
            mode,
        )
    })
}

/// `fstatat(dir, name, AT_SYMLINK_NOFOLLOW)`; an empty name is `dir`
/// itself.
pub fn stat_at(dir: BorrowedFd<'_>, name: &CStr) -> io::Result<Stat> {
    let flags = if name.is_empty() {
        libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW
    } else {
        libc::AT_SYMLINK_NOFOLLOW
    };
    // SAFETY: stat is plain data; fstatat fills it before it is read.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `name` is a valid C string and `stat` a live, writable stat.
    check(unsafe { libc::fstatat(dir.as_raw_fd(), name.as_ptr(), &raw mut stat, flags) })?;
    Ok(Stat {
        mode: stat.st_mode,
        dev: stat.st_dev,
        ino: stat.st_ino,
        uid: stat.st_uid,
        gid: stat.st_gid,
        size: u64::try_from(stat.st_size).unwrap_or(0),
        mtime: (
            stat.st_mtime,
            u32::try_from(stat.st_mtime_nsec).unwrap_or(0),
        ),
    })
}

/// The longest symbolic link target [`read_link_at`] reads, in bytes
/// (Linux's `PATH_MAX` less its NUL).
pub const MAX_LINK_BYTES: usize = 4095;

/// The target of the symbolic link `name` in `dir`, as written; a target
/// longer than [`MAX_LINK_BYTES`] is `ENAMETOOLONG`.
pub fn read_link_at(dir: BorrowedFd<'_>, name: &CStr) -> io::Result<Vec<u8>> {
    let mut target = vec![0u8; MAX_LINK_BYTES + 1];
    // SAFETY: `name` is a valid C string; the pointer and length describe
    // the live buffer `target`.
    let size = unsafe {
        libc::readlinkat(
            dir.as_raw_fd(),
            name.as_ptr(),
            target.as_mut_ptr().cast(),
            target.len(),
        )
    };
    let size = usize::try_from(size).map_err(|_| errno())?;
    if size > MAX_LINK_BYTES {
        return Err(io::Error::from_raw_os_error(libc::ENAMETOOLONG));
    }
    target.truncate(size);
    Ok(target)
}

pub fn mkdir_at(dir: BorrowedFd<'_>, name: &CStr, mode: u32) -> io::Result<()> {
    // SAFETY: `name` is a valid C string.
    check(unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), mode) }).map(drop)
}

/// `mknodat(dir, name, mode, device)`: `mode` carries the node's type.
pub fn mknod_at(dir: BorrowedFd<'_>, name: &CStr, mode: u32, device: u64) -> io::Result<()> {
    // SAFETY: `name` is a valid C string.
    check(unsafe { libc::mknodat(dir.as_raw_fd(), name.as_ptr(), mode, device) }).map(drop)
}

/// Creates the symbolic link `name` in `dir`, pointing at `target` as
/// written (it is never followed here).
pub fn symlink_at(target: &CStr, dir: BorrowedFd<'_>, name: &CStr) -> io::Result<()> {
    // SAFETY: both are valid C strings.
    check(unsafe { libc::symlinkat(target.as_ptr(), dir.as_raw_fd(), name.as_ptr()) }).map(drop)
}

/// A hard link `new_dir/new_name` to `old_dir/old_name`, which is not
/// followed when it is a symbolic link.
pub fn link_at(
    old_dir: BorrowedFd<'_>,
    old_name: &CStr,
    new_dir: BorrowedFd<'_>,
    new_name: &CStr,
) -> io::Result<()> {
    // SAFETY: both names are valid C strings.
    check(unsafe {
        libc::linkat(
            old_dir.as_raw_fd(),
            old_name.as_ptr(),
            new_dir.as_raw_fd(),
            new_name.as_ptr(),
            0,
        )
    })
    .map(drop)
}

/// Removes `name` from `dir`: a directory (empty) with `directory`.
pub fn unlink_at(dir: BorrowedFd<'_>, name: &CStr, directory: bool) -> io::Result<()> {
    let flags = if directory { libc::AT_REMOVEDIR } else { 0 };
    // SAFETY: `name` is a valid C string.
    check(unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), flags) }).map(drop)
}

/// `fchownat(dir, name, uid, gid, AT_SYMLINK_NOFOLLOW)`; an empty name is
/// `dir` itself.
pub fn chown_at(dir: BorrowedFd<'_>, name: &CStr, uid: u32, gid: u32) -> io::Result<()> {
    let flags = if name.is_empty() {
        libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW
    } else {
        libc::AT_SYMLINK_NOFOLLOW
    };
    // SAFETY: `name` is a valid C string.
    check(unsafe { libc::fchownat(dir.as_raw_fd(), name.as_ptr(), uid, gid, flags) }).map(drop)
}

/// `fchmod(fd, mode)`, the setuid, setgid and sticky bits included.
pub fn chmod_fd(fd: BorrowedFd<'_>, mode: u32) -> io::Result<()> {
    // SAFETY: fchmod on an open descriptor.
    check(unsafe { libc::fchmod(fd.as_raw_fd(), mode) }).map(drop)
}

/// `fchmodat(dir, name, mode, 0)`, for a node just created that is not a
/// symbolic link.
pub fn chmod_at(dir: BorrowedFd<'_>, name: &CStr, mode: u32) -> io::Result<()> {
    // SAFETY: `name` is a valid C string.
    check(unsafe { libc::fchmodat(dir.as_raw_fd(), name.as_ptr(), mode, 0) }).map(drop)
}

/// `fsetxattr(fd, name, value, 0)`.
pub fn set_xattr_fd(fd: BorrowedFd<'_>, name: &CStr, value: &[u8]) -> io::Result<()> {
    // SAFETY: `name` is a valid C string; the pointer and length describe
    // the live slice `value`.
    check(unsafe {
        libc::fsetxattr(
            fd.as_raw_fd(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    })
    .map(drop)
}

/// `setxattrat(dir, path, AT_SYMLINK_NOFOLLOW, name, value)`: an extended
/// attribute of a symbolic link or a special file, which cannot be
/// opened for `fsetxattr`.
pub fn set_xattr_at(dir: BorrowedFd<'_>, path: &CStr, name: &CStr, value: &[u8]) -> io::Result<()> {
    let args = XattrArgs {
        value: value.as_ptr() as u64,
        size: u32::try_from(value.len()).map_err(|_| io::Error::from_raw_os_error(libc::E2BIG))?,
        flags: 0,
    };
    // SAFETY: `path` and `name` are valid C strings; `args` is a valid
    // xattr_args of the size passed, whose value pointer and size describe
    // the live slice `value`.
    let rc = unsafe {
        libc::syscall(
            SYS_SETXATTRAT,
            dir.as_raw_fd(),
            path.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
            name.as_ptr(),
            &raw const args,
            size_of::<XattrArgs>(),
        )
    };
    if rc < 0 { Err(errno()) } else { Ok(()) }
}

fn timespecs(seconds: i64, nanoseconds: u32) -> [libc::timespec; 2] {
    let time = libc::timespec {
        tv_sec: seconds,
        tv_nsec: libc::c_long::from(nanoseconds.min(999_999_999)),
    };
    [time, time]
}

/// Sets the access and modification times of `name` in `dir` (not
/// followed when it is a symbolic link) to `seconds` and `nanoseconds`.
pub fn set_times_at(
    dir: BorrowedFd<'_>,
    name: &CStr,
    seconds: i64,
    nanoseconds: u32,
) -> io::Result<()> {
    let times = timespecs(seconds, nanoseconds);
    // SAFETY: `name` is a valid C string and `times` two valid timespecs.
    check(unsafe {
        libc::utimensat(
            dir.as_raw_fd(),
            name.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    })
    .map(drop)
}

/// Sets the access and modification times of an open file.
pub fn set_times_fd(fd: BorrowedFd<'_>, seconds: i64, nanoseconds: u32) -> io::Result<()> {
    let times = timespecs(seconds, nanoseconds);
    // SAFETY: `times` is two valid timespecs and `fd` is open.
    check(unsafe { libc::futimens(fd.as_raw_fd(), times.as_ptr()) }).map(drop)
}

/// A directory read one name at a time, through a descriptor of its
/// own (with a position of its own), which the `*at` calls on its names
/// use too.
pub struct Dir {
    stream: NonNull<libc::DIR>,
    /// The stream's descriptor, which it owns.
    fd: RawFd,
}

impl Dir {
    /// Opens the directory `name` in `dir` for reading, never following a
    /// symbolic link; `.` opens `dir` itself anew.
    pub fn open_at(dir: BorrowedFd<'_>, name: &CStr) -> io::Result<Dir> {
        let own = open_at(
            dir,
            name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            0,
        )?;
        let fd = own.into_raw_fd();
        // SAFETY: `fd` is an open directory descriptor we own; on success
        // the stream owns it.
        let stream = unsafe { libc::fdopendir(fd) };
        let Some(stream) = NonNull::new(stream) else {
            let error = errno();
            // SAFETY: fdopendir failed, so `fd` is still ours to close.
            unsafe { libc::close(fd) };
            return Err(error);
        };
        Ok(Dir { stream, fd })
    }

    /// The directory's descriptor, for `*at` calls on its names.
    pub fn fd(&self) -> BorrowedFd<'_> {
        // SAFETY: the stream owns `fd` and keeps it open until `self` is
        // dropped, and the borrow cannot outlive `self`.
        unsafe { BorrowedFd::borrow_raw(self.fd) }
    }

    /// The next name, `.` and `..` left out; `None` at the end.
    pub fn next_name(&mut self) -> io::Result<Option<CString>> {
        loop {
            // SAFETY: errno is this thread's; readdir reports an error only
            // through it.
            unsafe { *libc::__errno_location() = 0 };
            // SAFETY: `stream` is an open directory stream.
            let entry = unsafe { libc::readdir(self.stream.as_ptr()) };
            if entry.is_null() {
                let error = errno();
                return if error.raw_os_error() == Some(0) {
                    Ok(None)
                } else {
                    Err(error)
                };
            }
            // SAFETY: readdir returned a valid entry whose d_name is a
            // NUL-terminated string, alive until the next readdir.
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            if name != c"." && name != c".." {
                return Ok(Some(name.to_owned()));
            }
        }
    }

    /// Back to the directory's first name.
    pub fn rewind(&mut self) {
        // SAFETY: `stream` is an open directory stream.
        unsafe { libc::rewinddir(self.stream.as_ptr()) };
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        // SAFETY: `stream` is open and not used after this; closing it
        // closes `fd`.
        unsafe { libc::closedir(self.stream.as_ptr()) };
    }
}

/// `lgetxattr(path, name)`, for the tests.
#[cfg(test)]
pub fn get_xattr(path: &CStr, name: &CStr) -> io::Result<Vec<u8>> {
    let mut value = vec![0u8; 65536];
    // SAFETY: both are valid C strings; the pointer and length describe
    // the live buffer `value`.
    let size = unsafe {
        libc::lgetxattr(
            path.as_ptr(),
            name.as_ptr(),
            value.as_mut_ptr().cast(),
            value.len(),
        )
    };
    let size = usize::try_from(size).map_err(|_| errno())?;
    value.truncate(size);
    Ok(value)
}
