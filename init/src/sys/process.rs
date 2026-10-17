//! Starting, signalling and reaping processes.

use super::{check, cstring, errno};
use std::ffi::CString;
use std::io;
use std::ptr;

/// Everything the child needs, prepared before `fork` so the child only
/// makes system calls.
pub struct Spawn {
    pub program: CString,
    pub argv: Vec<CString>,
    pub env: Vec<CString>,
    pub workdir: CString,
    pub output: CString,
    pub uid: u32,
    pub gid: u32,
    pub groups: Vec<u32>,
}

impl Spawn {
    pub fn new(
        program: &str,
        argv: &[String],
        env: &[String],
        workdir: &str,
        output: &str,
        identity: (u32, u32, &[u32]),
    ) -> io::Result<Self> {
        let all = |items: &[String]| {
            items
                .iter()
                .map(|item| cstring(item))
                .collect::<io::Result<Vec<_>>>()
        };
        Ok(Spawn {
            program: cstring(program)?,
            argv: all(argv)?,
            env: all(env)?,
            workdir: cstring(workdir)?,
            output: cstring(output)?,
            uid: identity.0,
            gid: identity.1,
            groups: identity.2.to_vec(),
        })
    }
}

/// Where in the child the start failed, sent back over the pipe with the
/// error number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Output,
    Workdir,
    Identity,
    Exec,
}

impl Stage {
    fn code(self) -> u8 {
        match self {
            Stage::Output => 1,
            Stage::Workdir => 2,
            Stage::Identity => 3,
            Stage::Exec => 4,
        }
    }

    fn from_code(code: u8) -> Stage {
        match code {
            1 => Stage::Output,
            2 => Stage::Workdir,
            3 => Stage::Identity,
            _ => Stage::Exec,
        }
    }
}

/// Why a start failed.
#[derive(Debug)]
pub enum SpawnError {
    /// Before the child existed (pipe, fork).
    Parent(io::Error),
    /// In the child, before or at `execve`.
    Child(Stage, io::Error),
}

/// Forks and execs the process, as Docker's runtime does: stdin from
/// /dev/null, stdout and stderr to `output`, a new session, the working
/// directory, then the user's groups, group and user. Returns the child's
/// PID once `execve` succeeded.
pub fn spawn(spec: &Spawn) -> Result<i32, SpawnError> {
    let mut argv: Vec<*const libc::c_char> = spec.argv.iter().map(|a| a.as_ptr()).collect();
    argv.push(ptr::null());
    let mut env: Vec<*const libc::c_char> = spec.env.iter().map(|e| e.as_ptr()).collect();
    env.push(ptr::null());
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` has room for the two descriptors pipe2 writes.
    check(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }).map_err(SpawnError::Parent)?;
    let [read_end, write_end] = fds;
    // SAFETY: fork has no memory preconditions. The init forks while it
    // has a single thread, and the child below only makes system calls on
    // memory prepared before the fork.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        let error = errno();
        close(read_end);
        close(write_end);
        return Err(SpawnError::Parent(error));
    }
    if pid == 0 {
        close(read_end);
        child(spec, &argv, &env, write_end);
    }
    close(write_end);
    let mut report = [0u8; 5];
    let mut got = 0;
    while got < report.len() {
        let rest = report.get_mut(got..).unwrap_or_default();
        // SAFETY: the pointer and length describe the live slice `rest`.
        let count = unsafe { libc::read(read_end, rest.as_mut_ptr().cast(), rest.len()) };
        match usize::try_from(count) {
            Ok(0) => break,
            Ok(count) => got += count,
            Err(_) if errno().kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    close(read_end);
    if got == 0 {
        // The pipe closed on a successful execve.
        return Ok(pid);
    }
    let [stage, a, b, c, d] = report;
    let error = io::Error::from_raw_os_error(i32::from_ne_bytes([a, b, c, d]));
    reap(pid);
    Err(SpawnError::Child(Stage::from_code(stage), error))
}

fn close(fd: libc::c_int) {
    // SAFETY: closing a descriptor we own and do not use again.
    unsafe { libc::close(fd) };
}

/// The forked child: never returns.
fn child(
    spec: &Spawn,
    argv: &[*const libc::c_char],
    env: &[*const libc::c_char],
    report: libc::c_int,
) -> ! {
    let fail = |stage: Stage| -> ! {
        let code = errno().raw_os_error().unwrap_or(libc::EIO).to_ne_bytes();
        let [a, b, c, d] = code;
        let message = [stage.code(), a, b, c, d];
        // SAFETY: writing a live 5-byte array, then leaving the child
        // without running any of the parent's exit handlers.
        unsafe {
            libc::write(report, message.as_ptr().cast(), message.len());
            libc::_exit(127)
        }
    };
    // SAFETY: plain system calls on values prepared before the fork; the
    // sigset is initialised by sigemptyset before use.
    unsafe {
        let mut empty: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&raw mut empty);
        libc::sigprocmask(libc::SIG_SETMASK, &raw const empty, ptr::null_mut());
        libc::setsid();
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY);
        if null < 0 || libc::dup2(null, 0) < 0 {
            fail(Stage::Output);
        }
        let output = libc::open(spec.output.as_ptr(), libc::O_WRONLY | libc::O_NOCTTY);
        if output < 0 || libc::dup2(output, 1) < 0 || libc::dup2(output, 2) < 0 {
            fail(Stage::Output);
        }
        if null > 2 {
            libc::close(null);
        }
        if output > 2 {
            libc::close(output);
        }
        if libc::chdir(spec.workdir.as_ptr()) < 0 {
            fail(Stage::Workdir);
        }
        if libc::setgroups(spec.groups.len(), spec.groups.as_ptr()) < 0
            || libc::setgid(spec.gid) < 0
            || libc::setuid(spec.uid) < 0
        {
            fail(Stage::Identity);
        }
        libc::execve(spec.program.as_ptr(), argv.as_ptr(), env.as_ptr());
    }
    fail(Stage::Exec)
}

/// How a child ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    Exited(i32),
    Signaled(i32),
}

/// What `peek_ended` saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Peek {
    /// No child is left.
    NoChildren,
    /// Children remain and none has ended (only without blocking).
    Running,
    /// The child `pid` ended; it is still to be reaped.
    Ended(i32, Ended),
}

/// The next child that ended, without reaping it (`WNOWAIT`), so its PID
/// cannot be reused before the caller has decided what to do. Blocks when
/// `block` is set.
pub fn peek_ended(block: bool) -> Peek {
    loop {
        // SAFETY: siginfo_t is plain data; all zeroes is valid.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let mut options = libc::WEXITED | libc::WNOWAIT;
        if !block {
            options |= libc::WNOHANG;
        }
        // SAFETY: `info` is a live, writable siginfo_t.
        let rc = unsafe { libc::waitid(libc::P_ALL, 0, &raw mut info, options) };
        if rc < 0 {
            if errno().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Peek::NoChildren;
        }
        // SAFETY: waitid filled `info` for a CLD_* event (or left it zero).
        let (pid, status) = unsafe { (info.si_pid(), info.si_status()) };
        if pid == 0 {
            return Peek::Running;
        }
        let ended = if info.si_code == libc::CLD_EXITED {
            Ended::Exited(status)
        } else {
            Ended::Signaled(status)
        };
        return Peek::Ended(pid, ended);
    }
}

/// Reaps the child `pid` (which `peek_ended` reported).
pub fn reap(pid: i32) {
    loop {
        // SAFETY: a null status pointer is allowed.
        let rc = unsafe { libc::waitpid(pid, ptr::null_mut(), 0) };
        if rc >= 0 || errno().kind() != io::ErrorKind::Interrupted {
            return;
        }
    }
}

/// Sends `signal` to `pid`.
pub fn kill(pid: i32, signal: libc::c_int) {
    // SAFETY: kill has no memory preconditions.
    unsafe { libc::kill(pid, signal) };
}

/// Sends `signal` to every process but PID 1 (`kill(-1, …)` from PID 1).
pub fn kill_all(signal: libc::c_int) {
    // SAFETY: kill has no memory preconditions.
    unsafe { libc::kill(-1, signal) };
}

pub use libc::{SIGKILL, SIGTERM};
