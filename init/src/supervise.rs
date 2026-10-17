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

    /// The stop command: SIGTERM to the main process, then, after the
    /// timeout, SIGTERM to all, [`GRACE`], SIGKILL to all. A second stop,
    /// or one after the process ended, changes nothing.
    fn request_stop(self: &Arc<Self>, timeout_seconds: Option<u32>) {
        {
            let mut state = self.lock();
            if state.main_ended || state.stop_requested {
                return;
            }
            state.stop_requested = true;
            process::kill(self.main, SIGTERM);
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
        process::kill_all(SIGTERM);
        std::thread::sleep(GRACE);
        process::kill_all(SIGKILL);
    }
}

/// Reaps every child until the main process ends, then ends the machine.
pub fn run(main: i32, port: u32, default_timeout_seconds: u32) -> ! {
    let shared = Arc::new(Shared {
        main,
        default_timeout_seconds,
        state: Mutex::new(State::default()),
    });
    let listener_state = Arc::clone(&shared);
    if let Err(error) = std::thread::Builder::new()
        .name("control".into())
        .spawn(move || control(&listener_state, port))
    {
        log(&format!("the control listener could not start: {error}"));
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
                    if state.stop_requested {
                        Reason::Stopped
                    } else {
                        Reason::Exited
                    }
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

/// The control listener: one command line per connection, from the host.
fn control(shared: &Arc<Shared>, port: u32) {
    let listener = match vsock::Listener::bind(port) {
        Ok(listener) => listener,
        Err(error) => {
            log(&format!(
                "no control listener on vsock port {port}: {error}"
            ));
            return;
        }
    };
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
    let result = if line.len() > MAX_LINE && end == line.len() {
        Err("the line is too long")
    } else {
        std::str::from_utf8(text)
            .map_err(|_| "the line is not a JSON object")
            .and_then(protocol::decode_command)
    };
    let answer = match result {
        Ok(Command::Stop { timeout_seconds }) => {
            shared.request_stop(timeout_seconds);
            protocol::encode_answer(Ok(()))
        }
        Err(error) => protocol::encode_answer(Err(error)),
    };
    let _ = stream.write_all(format!("{answer}\n").as_bytes());
}
