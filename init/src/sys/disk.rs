//! Block device and ext4 ioctls, for growing the root file system.

use super::check;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};

// From `<linux/fs.h>` and `fs/ext4/ext4.h` (x86_64 encodings): not every
// libc release carries them. The request's 32 bits, in the type the
// target's libc gives `ioctl` (musl's `int`, glibc's `unsigned long`).
/// `BLKGETSIZE64`: `_IOR(0x12, 114, size_t)`.
#[allow(
    clippy::cast_possible_wrap,
    clippy::cast_lossless,
    reason = "the request's bits, as the target's libc types them"
)]
const BLKGETSIZE64: libc::Ioctl = 0x8008_1272_u32 as libc::Ioctl;
/// `EXT4_IOC_RESIZE_FS`: `_IOW('f', 16, __u64)`.
#[allow(
    clippy::cast_possible_wrap,
    clippy::cast_lossless,
    reason = "the request's bits, as the target's libc types them"
)]
const EXT4_IOC_RESIZE_FS: libc::Ioctl = 0x4008_6610_u32 as libc::Ioctl;

/// The size in bytes of the block device open as `fd`.
pub fn block_device_bytes(fd: BorrowedFd<'_>) -> io::Result<u64> {
    let mut bytes: u64 = 0;
    // SAFETY: `fd` is open and BLKGETSIZE64 writes one u64 to the live,
    // writable `bytes`.
    check(unsafe { libc::ioctl(fd.as_raw_fd(), BLKGETSIZE64, &raw mut bytes) })?;
    Ok(bytes)
}

/// Grows the mounted ext4 file system that `fd` (any file or directory
/// on it) is on to `blocks` blocks, online. The kernel refuses to shrink
/// and changes nothing when `blocks` is the current count.
pub fn resize_ext4(fd: BorrowedFd<'_>, blocks: u64) -> io::Result<()> {
    // SAFETY: `fd` is open and EXT4_IOC_RESIZE_FS reads one u64 from the
    // live `blocks`.
    check(unsafe { libc::ioctl(fd.as_raw_fd(), EXT4_IOC_RESIZE_FS, &raw const blocks) }).map(drop)
}
