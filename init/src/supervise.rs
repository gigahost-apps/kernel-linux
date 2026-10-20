//! Step 9 of docs/interface.md: reaping, the stop command and the end.

use crate::console::{self, log};
use crate::protocol::{self, Command, Reason, Report};
use crate::sys::process::{self, Ended, Peek, SIGKILL, SIGTERM};
use crate::sys::vsock;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// How long the others get between SIGTERM and SIGKILL.
const GRACE: Duration = Duration::from_secs(2);

/// The longest command line read from the host.
const MAX_LINE: usize = 4096;

#[derive(Default)]
struct State {
    /// The main process has ended (seen, not yet reaped, so its PID cannot
    /// be reused while this is false).
    main_ended: bool,
    stop_requested: bool,
}

struct Shared {
    main: i32,
    /// Whom the stop's escalation signals: -1, every process but this
    /// init (`kill(-1, …)` from PID 1, as [`process::kill_all`]); a test's
    /// own process group instead.
    everyone: i32,
    /// What a stop sends the main process first (the config's
    /// `stop_signal`).
    stop_signal: i32,
    default_timeout_seconds: u32,
    state: Mutex<State>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn main_ended(&self) -> bool {
        self.lock().main_ended
    }

    /// The stop command: the stop signal (SIGTERM unless the config names
    /// another) to the main process, then, after the timeout, SIGTERM to
    /// all, [`GRACE`], SIGKILL to all. A second stop, or one after the
    /// process ended, changes nothing.
    fn request_stop(self: &Arc<Self>, timeout_seconds: Option<u32>) {
        {
            let mut state = self.lock();
            if state.main_ended || state.stop_requested {
                return;
            }
            state.stop_requested = true;
            process::kill(self.main, self.stop_signal);
        }
        let timeout = Duration::from_secs(u64::from(
            timeout_seconds.unwrap_or(self.default_timeout_seconds),
        ));
        let shared = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("stop".into())
            .spawn(move || shared.escalate(timeout));
        if let Err(error) = spawned {
            log(&format!("the stop timer could not start: {error}"));
        }
    }

    fn escalate(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.main_ended() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if self.main_ended() {
            return;
        }
        process::kill(self.everyone, SIGTERM);
        std::thread::sleep(GRACE);
        process::kill(self.everyone, SIGKILL);
    }
}

/// Why the main process ended, from how it ended: after a stop command it
/// was stopped, whatever its status. An init system (version 3's
/// `init_system`) that asked for a power-off, a halt or a reboot ends as
/// the kernel ends a PID namespace's init then (`reboot(2)` there, see
/// `pid_namespaces(7)`): killed by SIGINT for a power-off or a halt, by
/// SIGHUP for a reboot. Nothing else kills it by those signals: no process
/// of its namespace can, and this init sends neither unless they are the
/// stop signal, after which it is stopped.
fn reason(ended: Ended, stop_requested: bool, init_system: bool) -> Reason {
    if stop_requested {
        return Reason::Stopped;
    }
    match ended {
        Ended::Signaled(libc::SIGINT) if init_system => Reason::Poweroff,
        Ended::Signaled(libc::SIGHUP) if init_system => Reason::Reboot,
        _ => Reason::Exited,
    }
}

/// Reaps every child until the main process ends, then ends the machine.
/// A stop sends `stop_signal` to `main` first and waits
/// `default_timeout_seconds` unless the command gives a timeout.
/// `main` is PID 1 of a PID namespace of its own in every mode; with
/// `init_system` (version 3) it is the image's init, whose power-off and
/// reboot are reported as such. When it ends the kernel ends every
/// process of its namespace; the orphans inside are its to reap, this
/// init reaps its own children. A stop signal `main` has no handler for
/// does nothing to it (the kernel drops it for a namespace's init), so
/// such a process is killed after the timeout, as under Docker.
pub fn run(
    main: i32,
    listener: Option<vsock::Listener>,
    stop_signal: i32,
    default_timeout_seconds: u32,
    init_system: bool,
) -> ! {
    let shared = Arc::new(Shared {
        main,
        everyone: -1,
        stop_signal,
        default_timeout_seconds,
        state: Mutex::new(State::default()),
    });
    if let Some(listener) = listener {
        let listener_state = Arc::clone(&shared);
        if let Err(error) = std::thread::Builder::new()
            .name("control".into())
            .spawn(move || control(&listener_state, &listener))
        {
            log(&format!("the control listener could not start: {error}"));
        }
    }
    loop {
        match process::peek_ended(true) {
            Peek::NoChildren => {
                // No child at all: the main process is gone unseen.
                finish(None, Reason::Exited);
            }
            Peek::Running => {}
            Peek::Ended(pid, ended) if pid == main => {
                let reason = {
                    let mut state = shared.lock();
                    state.main_ended = true;
                    reason(ended, state.stop_requested, init_system)
                };
                process::reap(pid);
                finish(Some(ended), reason);
            }
            Peek::Ended(pid, _) => process::reap(pid),
        }
    }
}

/// Reaps what is left until none is or `grace` has passed.
fn drain(grace: Duration) {
    let deadline = Instant::now() + grace;
    loop {
        match process::peek_ended(false) {
            Peek::NoChildren => return,
            Peek::Ended(pid, _) => process::reap(pid),
            Peek::Running => {
                if Instant::now() >= deadline {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// SIGTERM to all others, [`GRACE`], SIGKILL to all, the one exited
/// report, the power-off.
fn finish(ended: Option<Ended>, reason: Reason) -> ! {
    process::kill_all(SIGTERM);
    drain(GRACE);
    process::kill_all(SIGKILL);
    drain(GRACE);
    let (code, signal) = match ended {
        Some(Ended::Exited(code)) => (Some(code), None),
        Some(Ended::Signaled(signal)) => (None, Some(signal)),
        None => (None, None),
    };
    console::report(&Report::exited(code, signal, reason));
    console::power_off()
}

/// Binds the control port before the process starts, so no process of
/// the image can take it first.
pub fn bind_control(port: u32) -> Option<vsock::Listener> {
    match vsock::Listener::bind(port) {
        Ok(listener) => Some(listener),
        Err(error) => {
            log(&format!(
                "no control listener on vsock port {port}: {error}"
            ));
            None
        }
    }
}

/// The control listener: one command line per connection, from the host.
fn control(shared: &Arc<Shared>, listener: &vsock::Listener) {
    loop {
        match listener.accept() {
            Ok((stream, cid)) if cid == vsock::HOST_CID => serve(shared, stream),
            Ok(_) => {}
            Err(error) => {
                log(&format!("vsock accept failed: {error}"));
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

fn serve(shared: &Arc<Shared>, mut stream: vsock::Stream) {
    if stream.set_timeouts(Duration::from_secs(2)).is_err() {
        return;
    }
    let mut line = Vec::new();
    let mut buffer = [0u8; 512];
    while line.len() <= MAX_LINE && !line.contains(&b'\n') {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(count) => line.extend_from_slice(buffer.get(..count).unwrap_or_default()),
        }
    }
    let end = line
        .iter()
        .position(|&byte| byte == b'\n')
        .unwrap_or(line.len());
    let text = line.get(..end).unwrap_or_default();
    let version = protocol::version();
    let result = if end > MAX_LINE {
        Err("the line is too long")
    } else {
        std::str::from_utf8(text)
            .map_err(|_| "the line is not a JSON object")
            .and_then(|line| protocol::decode_command(line, version))
    };
    let answer = match result {
        Ok(Command::Stop { timeout_seconds }) => {
            shared.request_stop(timeout_seconds);
            protocol::encode_answer(Ok(()), version)
        }
        Err(error) => protocol::encode_answer(Err(error), version),
    };
    let _ = stream.write_all(format!("{answer}\n").as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    /// Marks the main process ended when dropped, even by a failing
    /// assertion, so the stop's escalation thread returns instead of
    /// signalling every process after its timeout.
    struct Ended(Arc<Shared>);

    impl Drop for Ended {
        fn drop(&mut self) {
            self.0.lock().main_ended = true;
        }
    }

    #[test]
    fn reasons() {
        use super::Ended as How;
        let int = How::Signaled(libc::SIGINT);
        let hup = How::Signaled(libc::SIGHUP);
        assert_eq!(reason(int, false, true), Reason::Poweroff);
        assert_eq!(reason(hup, false, true), Reason::Reboot);
        // A stop that ends in the init system's power-off was a stop.
        assert_eq!(reason(int, true, true), Reason::Stopped);
        assert_eq!(reason(hup, true, true), Reason::Stopped);
        assert_eq!(reason(How::Exited(0), false, true), Reason::Exited);
        assert_eq!(reason(How::Signaled(9), false, true), Reason::Exited);
        // A plain process killed by SIGINT or SIGHUP just exited.
        assert_eq!(reason(int, false, false), Reason::Exited);
        assert_eq!(reason(hup, false, false), Reason::Exited);
        assert_eq!(reason(How::Exited(3), true, false), Reason::Stopped);
    }

    /// The stop goes to the init system's PID (the namespace's PID 1, by
    /// its PID here) with the config's signal, SIGRTMIN+3 for systemd.
    #[test]
    fn a_stop_sends_an_init_system_its_signal() {
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let sigrtmin_3 = libc::SIGRTMIN() + 3;
        let main = i32::try_from(child.id()).unwrap();
        let shared = Arc::new(Shared {
            main,
            everyone: main,
            stop_signal: sigrtmin_3,
            default_timeout_seconds: 300,
            state: Mutex::new(State::default()),
        });
        let guard = Ended(Arc::clone(&shared));
        shared.request_stop(None);
        let status = child.wait().unwrap();
        assert_eq!(status.signal(), Some(sigrtmin_3));
        drop(guard);
    }

    #[test]
    fn a_stop_sends_the_stop_signal_once() {
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let main = i32::try_from(child.id()).unwrap();
        let shared = Arc::new(Shared {
            main,
            everyone: main,
            stop_signal: libc::SIGUSR1,
            default_timeout_seconds: 300,
            state: Mutex::new(State::default()),
        });
        let guard = Ended(Arc::clone(&shared));
        shared.request_stop(None);
        shared.request_stop(Some(300));
        let status = child.wait().unwrap();
        assert_eq!(status.signal(), Some(libc::SIGUSR1));
        assert!(shared.lock().stop_requested);
        drop(guard);
    }

    /// A process that does nothing on its stop signal (as a namespace's
    /// PID 1 without a handler for it, a shell entrypoint say) survives
    /// it and the SIGTERM to all after the timeout, and ends by the
    /// SIGKILL to all [`GRACE`] later. The child and its own child form
    /// a process group of their own, which stands for every process here.
    #[test]
    fn a_stop_kills_a_process_that_ignores_its_signal_after_the_timeout() {
        use std::io::BufRead;
        use std::os::unix::process::CommandExt;
        let mut child = std::process::Command::new("sh")
            .args(["-c", "trap '' TERM; echo ready; while :; do sleep 1; done"])
            .stdout(std::process::Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        // The trap is set once the shell says so; a stop before it would
        // end the shell by its first signal.
        let mut ready = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready, "ready\n");
        let main = i32::try_from(child.id()).unwrap();
        let shared = Arc::new(Shared {
            main,
            everyone: -main,
            stop_signal: SIGTERM,
            default_timeout_seconds: 300,
            state: Mutex::new(State::default()),
        });
        let guard = Ended(Arc::clone(&shared));
        let timeout = Duration::from_secs(1);
        let latest = timeout + GRACE + Duration::from_secs(5);
        let started = Instant::now();
        shared.request_stop(Some(1));
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if started.elapsed() > latest {
                process::kill(-main, SIGKILL);
                let _ = child.wait();
                panic!("the process still ran {latest:?} after the stop");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let elapsed = started.elapsed();
        drop(guard);
        assert_eq!(status.signal(), Some(SIGKILL));
        assert!(elapsed >= timeout + GRACE, "ended after {elapsed:?}");
    }
}
