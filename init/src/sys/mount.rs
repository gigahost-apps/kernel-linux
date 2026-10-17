//! Mounts.

use super::{check, cstring};
use std::ffi::CString;
use std::io;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::raw::c_ulong;
use std::ptr;

pub const NOSUID: c_ulong = libc::MS_NOSUID;
pub const NODEV: c_ulong = libc::MS_NODEV;
pub const NOEXEC: c_ulong = libc::MS_NOEXEC;
pub const RDONLY: c_ulong = libc::MS_RDONLY;
pub const REMOUNT: c_ulong = libc::MS_REMOUNT;

// From `<linux/mount.h>`; not every libc release carries them.
const OPEN_TREE_CLONE: libc::c_uint = 1;
const MOVE_MOUNT_F_EMPTY_PATH: libc::c_uint = 0x0000_0004;

/// `mount(2)`. `source` and `fstype` may be absent (a move or remount).
pub fn mount(
    source: Option<&str>,
    target: &str,
    fstype: Option<&str>,
    flags: c_ulong,
    data: Option<&str>,
) -> io::Result<()> {
    let source = source.map(cstring).transpose()?;
    let target = cstring(target)?;
    let fstype = fstype.map(cstring).transpose()?;
    let data = data.map(cstring).transpose()?;
    let pointer = |value: &Option<CString>| value.as_ref().map_or(ptr::null(), |v| v.as_ptr());
    // SAFETY: every pointer is either null or a valid C string that lives
    // until the call returns.
    check(unsafe {
        libc::mount(
            pointer(&source),
            target.as_ptr(),
            pointer(&fstype),
            flags,
            pointer(&data).cast(),
        )
    })
    .map(drop)
}

/// Moves the mount at `from` to `to` (`MS_MOVE`).
pub fn move_mount(from: &str, to: &str) -> io::Result<()> {
    mount(Some(from), to, None, libc::MS_MOVE, None)
}

/// Bind-mounts `source` onto `target` (`MS_BIND`), following symbolic
/// links as `mount(2)` does.
pub fn bind(source: &str, target: &str) -> io::Result<()> {
    mount(Some(source), target, None, libc::MS_BIND, None)
}

/// Bind-mounts the file `source` onto `target` without following `target`
/// when it is a symbolic link: the mount covers the link itself, so a
/// link pointing anywhere on the disk is never written through.
/// (`open_tree` and `move_mount`, Linux 5.2; `move_mount` follows a
/// target's link only when asked to with `MOVE_MOUNT_T_SYMLINKS`.)
pub fn bind_over(source: &str, target: &str) -> io::Result<()> {
    let source = cstring(source)?;
    let target = cstring(target)?;
    // SAFETY: `source` is a valid C string; the call returns a new
    // descriptor or -1.
    let raw = unsafe {
        libc::syscall(
            libc::SYS_open_tree,
            libc::AT_FDCWD,
            source.as_ptr(),
            OPEN_TREE_CLONE | libc::O_CLOEXEC as libc::c_uint,
        )
    };
    let raw = libc::c_int::try_from(raw).map_err(|_| io::Error::from(io::ErrorKind::Other))?;
    let tree = check(raw)?;
    // SAFETY: `tree` is a descriptor open_tree just returned to us.
    let tree = unsafe { OwnedFd::from_raw_fd(tree) };
    // SAFETY: the descriptor is open, the empty path and `target` are
    // valid C strings.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_move_mount,
            std::os::fd::AsRawFd::as_raw_fd(&tree),
            c"".as_ptr(),
            libc::AT_FDCWD,
            target.as_ptr(),
            MOVE_MOUNT_F_EMPTY_PATH,
        )
    };
    if rc < 0 { Err(super::errno()) } else { Ok(()) }
}

/// Remounts the root read-only, so a power-off loses nothing.
pub fn remount_root_readonly() -> io::Result<()> {
    mount(None, "/", None, libc::MS_REMOUNT | libc::MS_RDONLY, None)
}

/// Unmounts `target`.
pub fn umount(target: &str) -> io::Result<()> {
    let target = cstring(target)?;
    // SAFETY: `target` is a valid C string.
    check(unsafe { libc::umount2(target.as_ptr(), 0) }).map(drop)
}
