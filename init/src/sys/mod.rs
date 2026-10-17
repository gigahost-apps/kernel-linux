//! The init's only unsafe code: thin wrappers over the system calls it
//! needs. Every function here returns an error instead of panicking, and
//! nothing here decides policy; the callers do.
#![allow(unsafe_code)]

pub mod mount;
pub mod net;
pub mod process;
pub mod vsock;

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

/// The last system call's error.
pub fn errno() -> io::Error {
    io::Error::last_os_error()
}

fn check(rc: libc::c_int) -> io::Result<libc::c_int> {
    if rc < 0 { Err(errno()) } else { Ok(rc) }
}

/// A C string, or `InvalidInput` when the text holds a NUL byte.
pub fn cstring(text: &str) -> io::Result<CString> {
    CString::new(text).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

/// Milliseconds on the monotonic clock (since boot, without suspend).
pub fn monotonic_ms() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `now` is a valid, writable timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut now) };
    let seconds = u64::try_from(now.tv_sec).unwrap_or(0);
    let nanos = u64::try_from(now.tv_nsec).unwrap_or(0);
    seconds
        .saturating_mul(1000)
        .saturating_add(nanos / 1_000_000)
}

/// This process's ID.
pub fn getpid() -> i32 {
    // SAFETY: getpid has no preconditions.
    unsafe { libc::getpid() }
}

/// Writes all of `bytes` to `fd`, giving up silently on an error: the
/// console is best effort.
pub fn write_all(fd: libc::c_int, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        // SAFETY: the pointer and length describe the live slice `bytes`.
        let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        match usize::try_from(written) {
            Ok(0) => return,
            Ok(count) => bytes = bytes.get(count..).unwrap_or_default(),
            Err(_) if errno().kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

/// Opens `/dev/console` as stdin, stdout and stderr.
pub fn console_as_stdio() -> io::Result<()> {
    // SAFETY: the path is a NUL-terminated literal.
    let fd = check(unsafe {
        libc::open(
            c"/dev/console".as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    })?;
    for target in 0..3 {
        // SAFETY: dup2 on a descriptor we own; the target numbers are the
        // standard streams, which dup2 opens without O_CLOEXEC.
        check(unsafe { libc::dup2(fd, target) })?;
    }
    if fd > 2 {
        // SAFETY: `fd` is ours and no longer used.
        unsafe { libc::close(fd) };
    }
    Ok(())
}

/// Opens a terminal and switches it to raw mode, so what the process
/// writes reaches the host byte for byte (no `\n` to `\r\n`, as a
/// container runtime's log without a TTY). The kernel keeps a terminal's
/// settings while it is open, so the caller keeps the descriptor.
pub fn open_raw_tty(path: &str) -> io::Result<OwnedFd> {
    let path = cstring(path)?;
    // SAFETY: `path` is a valid C string.
    let fd = check(unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    })?;
    // SAFETY: `fd` was just returned to us and is owned by nobody else.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: termios is plain data; tcgetattr fills it before use.
    let mut settings: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: `settings` is a live, writable termios and `fd` is open.
    check(unsafe { libc::tcgetattr(fd.as_raw_fd(), &raw mut settings) })?;
    // SAFETY: `settings` is a valid termios.
    unsafe { libc::cfmakeraw(&raw mut settings) };
    // SAFETY: `settings` is a valid termios and `fd` is open.
    check(unsafe { libc::tcsetattr(fd.as_raw_fd(), libc::TCSANOW, &raw const settings) })?;
    Ok(fd)
}

/// Sets the kernel's host name.
pub fn sethostname(name: &str) -> io::Result<()> {
    // SAFETY: the pointer and length describe the live string `name`.
    check(unsafe { libc::sethostname(name.as_ptr().cast(), name.len()) }).map(drop)
}

/// `chroot(path)`.
pub fn chroot(path: &str) -> io::Result<()> {
    let path = cstring(path)?;
    // SAFETY: `path` is a valid C string.
    check(unsafe { libc::chroot(path.as_ptr()) }).map(drop)
}

/// Flushes every file system's dirty data.
pub fn sync() {
    // SAFETY: sync has no preconditions.
    unsafe { libc::sync() };
}

/// Powers the machine off. Never returns: if the kernel refuses, the
/// calling thread sleeps for ever rather than letting PID 1 exit (which
/// would panic the kernel).
pub fn power_off() -> ! {
    sync();
    // SAFETY: reboot with a valid command; it only returns on an error.
    unsafe { libc::reboot(libc::RB_POWER_OFF) };
    loop {
        std::thread::sleep(Duration::from_hours(1));
    }
}
