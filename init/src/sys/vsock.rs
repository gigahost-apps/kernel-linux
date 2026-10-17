//! vsock stream sockets: the reports to the host and the control listener.

use super::{check, errno};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

/// The host's context ID.
pub const HOST_CID: u32 = 2;

/// A connected vsock stream.
pub struct Stream(OwnedFd);

/// A listening vsock socket.
pub struct Listener(OwnedFd);

fn new_socket() -> io::Result<OwnedFd> {
    // SAFETY: socket has no memory preconditions.
    let fd =
        check(unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) })?;
    // SAFETY: `fd` was just returned to us and is owned by nobody else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn address(cid: u32, port: u32) -> libc::sockaddr_vm {
    // SAFETY: sockaddr_vm is plain data; all zeroes is valid.
    let mut address: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
    address.svm_family = libc::sa_family_t::try_from(libc::AF_VSOCK).unwrap_or_default();
    address.svm_cid = cid;
    address.svm_port = port;
    address
}

#[allow(clippy::cast_possible_truncation, reason = "sockaddr_vm is 16 bytes")]
const ADDRESS_LEN: libc::socklen_t = std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t;

fn set_timeout(fd: &OwnedFd, option: libc::c_int, timeout: Duration) -> io::Result<()> {
    let value = libc::timeval {
        // The timeouts are a few seconds; a second is the fallback.
        tv_sec: timeout.as_secs().try_into().unwrap_or(1),
        tv_usec: timeout.subsec_micros().into(),
    };
    // SAFETY: the pointer and length describe the live timeval `value`.
    check(unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            option,
            (&raw const value).cast(),
            libc::socklen_t::try_from(std::mem::size_of::<libc::timeval>()).unwrap_or(0),
        )
    })
    .map(drop)
}

impl Stream {
    /// Connects to `cid:port`.
    pub fn connect(cid: u32, port: u32) -> io::Result<Stream> {
        let fd = new_socket()?;
        let address = address(cid, port);
        loop {
            // SAFETY: the pointer and length describe the live address.
            let rc =
                unsafe { libc::connect(fd.as_raw_fd(), (&raw const address).cast(), ADDRESS_LEN) };
            if rc == 0 {
                return Ok(Stream(fd));
            }
            let error = errno();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    /// Sets the read and write timeouts.
    pub fn set_timeouts(&self, timeout: Duration) -> io::Result<()> {
        set_timeout(&self.0, libc::SO_RCVTIMEO, timeout)?;
        set_timeout(&self.0, libc::SO_SNDTIMEO, timeout)
    }

    /// Closes the writing half, so the peer reads the end of the stream.
    pub fn shutdown_write(&self) {
        // SAFETY: shutdown on our own open descriptor.
        unsafe { libc::shutdown(self.0.as_raw_fd(), libc::SHUT_WR) };
    }
}

impl Read for Stream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        // SAFETY: the pointer and length describe the live slice `buffer`.
        let count =
            unsafe { libc::read(self.0.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
        usize::try_from(count).map_err(|_| errno())
    }
}

impl Write for Stream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        // SAFETY: the pointer and length describe the live slice `bytes`;
        // MSG_NOSIGNAL keeps a closed peer from raising SIGPIPE.
        let count = unsafe {
            libc::send(
                self.0.as_raw_fd(),
                bytes.as_ptr().cast(),
                bytes.len(),
                libc::MSG_NOSIGNAL,
            )
        };
        usize::try_from(count).map_err(|_| errno())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Listener {
    /// Listens on `port` of this machine's own context ID.
    pub fn bind(port: u32) -> io::Result<Listener> {
        let fd = new_socket()?;
        let address = address(libc::VMADDR_CID_ANY, port);
        // SAFETY: the pointer and length describe the live address.
        check(unsafe { libc::bind(fd.as_raw_fd(), (&raw const address).cast(), ADDRESS_LEN) })?;
        // SAFETY: listen on our own bound descriptor.
        check(unsafe { libc::listen(fd.as_raw_fd(), 4) })?;
        Ok(Listener(fd))
    }

    /// The next connection and the peer's context ID.
    pub fn accept(&self) -> io::Result<(Stream, u32)> {
        loop {
            let mut peer = address(0, 0);
            let mut length = ADDRESS_LEN;
            // SAFETY: `peer` and `length` are live and writable, `length`
            // holds the size of `peer`.
            let fd = unsafe {
                libc::accept4(
                    self.0.as_raw_fd(),
                    (&raw mut peer).cast(),
                    &raw mut length,
                    libc::SOCK_CLOEXEC,
                )
            };
            if fd >= 0 {
                // SAFETY: `fd` was just returned to us.
                return Ok((Stream(unsafe { OwnedFd::from_raw_fd(fd) }), peer.svm_cid));
            }
            let error = errno();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}
