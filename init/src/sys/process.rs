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
    /// Start the program as PID 1 of PID, mount and cgroup namespaces of
    /// its own, as the init starts every mode's process (the run mode's
    /// and `init_system`'s); `None` starts it as a plain child.
    pub namespace: Option<Namespace>,
}

/// Where the process's namespaces start.
pub struct Namespace {
    /// The `cgroup.procs` of the cgroup that becomes the cgroup
    /// namespace's root.
    pub cgroup_procs: CString,
    /// The device bound over `/dev/console` inside, when there is one
    /// (`/dev/hvc0`).
    pub console: Option<CString>,
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
            namespace: None,
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
    /// Moving into the cgroup, unsharing the namespaces, forking their
    /// PID 1.
    Namespace,
    /// The namespace's own mounts.
    Mounts,
}

/// `CAP_SYS_MODULE` of `<linux/capability.h>`.
const CAP_SYS_MODULE: libc::c_ulong = 16;

/// The message tag that carries the namespace's PID 1 (its PID here)
/// instead of an error.
const PID_TAG: u8 = 0;

impl Stage {
    fn code(self) -> u8 {
        match self {
            Stage::Output => 1,
            Stage::Workdir => 2,
            Stage::Identity => 3,
            Stage::Exec => 4,
            Stage::Namespace => 5,
            Stage::Mounts => 6,
        }
    }

    fn from_code(code: u8) -> Stage {
        match code {
            1 => Stage::Output,
            2 => Stage::Workdir,
            3 => Stage::Identity,
            5 => Stage::Namespace,
            6 => Stage::Mounts,
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
///
/// With [`Spawn::namespace`] a first child moves into the cgroup,
/// unshares the PID, mount and cgroup namespaces and forks the process,
/// which is PID 1 inside them, then exits: the process becomes this
/// init's child, and its PID here is the one returned. The process waits
/// before anything else until this init has reaped the first child, so
/// the cgroup holds only the process when it execs.
pub fn spawn(spec: &Spawn) -> Result<i32, SpawnError> {
    let mut argv: Vec<*const libc::c_char> = spec.argv.iter().map(|a| a.as_ptr()).collect();
    argv.push(ptr::null());
    let mut env: Vec<*const libc::c_char> = spec.env.iter().map(|e| e.as_ptr()).collect();
    env.push(ptr::null());
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` has room for the two descriptors pipe2 writes.
    check(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }).map_err(SpawnError::Parent)?;
    let [read_end, write_end] = fds;
    // The namespace's gate: its PID 1 waits for the end of file, which
    // comes once this init has reaped the first child and closed the
    // write end, so the process never runs while the first child is
    // still in the cgroup.
    let gate = if spec.namespace.is_some() {
        let mut gate = [0 as libc::c_int; 2];
        // SAFETY: `gate` has room for the two descriptors pipe2 writes.
        if let Err(error) = check(unsafe { libc::pipe2(gate.as_mut_ptr(), libc::O_CLOEXEC) }) {
            close(read_end);
            close(write_end);
            return Err(SpawnError::Parent(error));
        }
        Some(gate)
    } else {
        None
    };
    // SAFETY: fork has no memory preconditions. The init forks while it
    // has a single thread, and the child below only makes system calls on
    // memory prepared before the fork.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        let error = errno();
        close(read_end);
        close(write_end);
        if let Some([gate_read, gate_write]) = gate {
            close(gate_read);
            close(gate_write);
        }
        return Err(SpawnError::Parent(error));
    }
    if pid == 0 {
        close(read_end);
        match (&spec.namespace, gate) {
            (Some(namespace), Some([gate_read, gate_write])) => {
                close(gate_write);
                namespace_parent(spec, namespace, &argv, &env, write_end, gate_read)
            }
            _ => child(spec, &argv, &env, write_end),
        }
    }
    close(write_end);
    let result = if let Some([gate_read, gate_write]) = gate {
        close(gate_read);
        namespace_result(pid, read_end, gate_write)
    } else {
        // The pipe closes on a successful execve.
        match read_message(read_end) {
            None => Ok(pid),
            Some(message) => {
                reap(pid);
                Err(child_error(message))
            }
        }
    };
    close(read_end);
    result
}

/// The namespace's start, from the first child's messages: its PID 1's
/// PID, then nothing on a successful execve. `gate` is the write end its
/// PID 1 waits on, closed once the first child is reaped.
fn namespace_result(
    first: i32,
    read_end: libc::c_int,
    gate: libc::c_int,
) -> Result<i32, SpawnError> {
    // The first child exits once it has forked (or failed), its message
    // already in the pipe; reaping it makes the process this init's child,
    // and its exit has taken it out of the cgroup: the process may go
    // on. Reaped before the read, so a first child killed before it wrote
    // cannot leave the read waiting on a PID 1 that waits on the gate.
    reap(first);
    close(gate);
    let message = read_message(read_end);
    let main = match message {
        Some([PID_TAG, a, b, c, d]) => i32::from_ne_bytes([a, b, c, d]),
        Some(message) => return Err(child_error(message)),
        None => {
            return Err(SpawnError::Child(
                Stage::Namespace,
                io::Error::from_raw_os_error(libc::EIO),
            ));
        }
    };
    match read_message(read_end) {
        None => Ok(main),
        Some(message) => {
            reap(main);
            Err(child_error(message))
        }
    }
}

fn child_error([stage, a, b, c, d]: [u8; 5]) -> SpawnError {
    let error = io::Error::from_raw_os_error(i32::from_ne_bytes([a, b, c, d]));
    SpawnError::Child(Stage::from_code(stage), error)
}

/// One 5-byte message from the pipe, or `None` when it closed first.
fn read_message(read_end: libc::c_int) -> Option<[u8; 5]> {
    let mut message = [0u8; 5];
    let mut got = 0;
    while got < message.len() {
        let rest = message.get_mut(got..).unwrap_or_default();
        // SAFETY: the pointer and length describe the live slice `rest`.
        let count = unsafe { libc::read(read_end, rest.as_mut_ptr().cast(), rest.len()) };
        match usize::try_from(count) {
            Ok(0) => break,
            Ok(count) => got += count,
            Err(_) if errno().kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    (got > 0).then_some(message)
}

fn close(fd: libc::c_int) {
    // SAFETY: closing a descriptor we own and do not use again.
    unsafe { libc::close(fd) };
}

/// The highest signal number on Linux (`SIGRTMAX`, `_NSIG - 1`).
pub const SIGNAL_MAX: libc::c_int = 64;

/// Writes `[stage, errno]` to the pipe and leaves the child.
fn fail_child(report: libc::c_int, stage: Stage) -> ! {
    let code = errno().raw_os_error().unwrap_or(libc::EIO).to_ne_bytes();
    let [a, b, c, d] = code;
    let message = [stage.code(), a, b, c, d];
    // SAFETY: writing a live 5-byte array, then leaving the child
    // without running any of the parent's exit handlers.
    unsafe {
        libc::write(report, message.as_ptr().cast(), message.len());
        libc::_exit(127)
    }
}

/// The first child of a start in namespaces: into the cgroup, out of
/// the namespaces, the fork of their PID 1, whose PID it sends before it
/// exits. Their PID 1 first waits for the end of file on `gate`. Never
/// returns.
fn namespace_parent(
    spec: &Spawn,
    namespace: &Namespace,
    argv: &[*const libc::c_char],
    env: &[*const libc::c_char],
    report: libc::c_int,
    gate: libc::c_int,
) -> ! {
    // SAFETY: plain system calls on values prepared before the fork.
    unsafe {
        // Into the cgroup first, so the new cgroup namespace's root is
        // that cgroup.
        let procs = libc::open(
            namespace.cgroup_procs.as_ptr(),
            libc::O_WRONLY | libc::O_CLOEXEC,
        );
        if procs < 0 || libc::write(procs, c"0".as_ptr().cast(), 1) != 1 {
            fail_child(report, Stage::Namespace);
        }
        libc::close(procs);
        if libc::unshare(libc::CLONE_NEWPID | libc::CLONE_NEWNS | libc::CLONE_NEWCGROUP) < 0 {
            fail_child(report, Stage::Namespace);
        }
        // This process stays in the parent PID namespace; its child is
        // the new namespace's PID 1.
        let pid = libc::fork();
        if pid < 0 {
            fail_child(report, Stage::Namespace);
        }
        if pid == 0 {
            wait_for_end_of_file(gate);
            libc::close(gate);
            if namespace_mounts(namespace).is_err() {
                fail_child(report, Stage::Mounts);
            }
            // The kernel loads no modules: without the capability in the
            // bounding set the process knows it, as under a
            // container manager, instead of failing to load any.
            if libc::prctl(libc::PR_CAPBSET_DROP, CAP_SYS_MODULE, 0, 0, 0) < 0 {
                fail_child(report, Stage::Namespace);
            }
            child(spec, argv, env, report);
        }
        let [a, b, c, d] = pid.to_ne_bytes();
        let message = [PID_TAG, a, b, c, d];
        libc::write(report, message.as_ptr().cast(), message.len());
        // Leaves the cgroup to the namespace's PID 1 alone, as cgroup v2
        // wants of a cgroup whose children use controllers.
        libc::_exit(0)
    }
}

/// Reads `fd` until the end of file (or an error other than an
/// interruption): the wait of the namespace's PID 1 for the first child
/// to be reaped. Async-signal-safe, for a forked child.
fn wait_for_end_of_file(fd: libc::c_int) {
    let mut byte = 0u8;
    loop {
        // SAFETY: reading at most one byte into the live `byte`.
        let count = unsafe { libc::read(fd, (&raw mut byte).cast(), 1) };
        if count == 0 || (count < 0 && errno().kind() != io::ErrorKind::Interrupted) {
            return;
        }
    }
}

/// The namespace's own mounts, from its PID 1 before `execve`, as
/// container managers set them up for a container: nothing propagates
/// back, a `/proc` of the new PID namespace, `/sys` read-only (so the
/// process manages no devices) but for a writable cgroup2 at
/// `/sys/fs/cgroup` that shows the cgroup namespace's root, and the
/// output device as `/dev/console`.
fn namespace_mounts(namespace: &Namespace) -> Result<(), ()> {
    let quiet = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
    let ok = |rc: libc::c_int| if rc < 0 { Err(()) } else { Ok(()) };
    // SAFETY: every pointer is null or a NUL-terminated literal or a C
    // string prepared before the fork.
    unsafe {
        ok(libc::mount(
            ptr::null(),
            c"/".as_ptr(),
            ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            ptr::null(),
        ))?;
        ok(libc::mount(
            c"proc".as_ptr(),
            c"/proc".as_ptr(),
            c"proc".as_ptr(),
            quiet,
            ptr::null(),
        ))?;
        ok(libc::mount(
            ptr::null(),
            c"/sys".as_ptr(),
            ptr::null(),
            libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY | quiet,
            ptr::null(),
        ))?;
        ok(libc::umount2(c"/sys/fs/cgroup".as_ptr(), libc::MNT_DETACH))?;
        ok(libc::mount(
            c"cgroup2".as_ptr(),
            c"/sys/fs/cgroup".as_ptr(),
            c"cgroup2".as_ptr(),
            quiet,
            ptr::null(),
        ))?;
        if let Some(console) = &namespace.console {
            ok(libc::mount(
                console.as_ptr(),
                c"/dev/console".as_ptr(),
                ptr::null(),
                libc::MS_BIND,
                ptr::null(),
            ))?;
        }
    }
    Ok(())
}

/// The forked child: never returns.
fn child(
    spec: &Spawn,
    argv: &[*const libc::c_char],
    env: &[*const libc::c_char],
    report: libc::c_int,
) -> ! {
    let fail = |stage: Stage| -> ! { fail_child(report, stage) };
    // SAFETY: plain system calls on values prepared before the fork; the
    // sigset is initialised by sigemptyset before use.
    unsafe {
        // Every signal back to its default action, as a container runtime
        // starts the process. An ignored signal stays ignored across
        // execve, and Rust's runtime ignores SIGPIPE in the init: without
        // this a pipeline's writer would get EPIPE instead of dying
        // quietly, and a shell could not trap the signal. `signal` is
        // async-signal-safe; SIGKILL, SIGSTOP and libc's own signals refuse
        // the call, which changes nothing.
        for signal in 1..=SIGNAL_MAX {
            libc::signal(signal, libc::SIG_DFL);
        }
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
