//! The init's own output (the serial console), its reports and the end of
//! the machine.

use crate::protocol::Report;
use crate::sys::{self, mount, vsock};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// The host's report port, `u32::MAX` while unknown.
static PORT: AtomicU32 = AtomicU32::new(u32::MAX);

/// Set once the renter's disk is the root, so the end remounts it
/// read-only.
static ON_DISK: AtomicBool = AtomicBool::new(false);

/// How long a report retries a host that does not answer.
const REPORT_RETRY: Duration = Duration::from_secs(2);

/// Writes `gigahost-init: <message>` on the console. Never pass a config
/// value here: the console is the host's log.
pub fn log(message: &str) {
    sys::write_all(2, format!("gigahost-init: {message}\n").as_bytes());
}

pub fn set_port(port: u32) {
    PORT.store(port, Ordering::SeqCst);
}

pub fn set_on_disk() {
    ON_DISK.store(true, Ordering::SeqCst);
}

/// Sends a report: on the console, then over vsock to the host.
pub fn report(report: &Report) {
    let line = report.encode();
    log(&format!("report {line}"));
    let port = PORT.load(Ordering::SeqCst);
    if port != u32::MAX {
        send(port, &line);
    }
}

/// Connects, writes the line and closes, retrying for [`REPORT_RETRY`]
/// while the host does not answer. A machine without a vsock device
/// (`ENODEV`) is not retried: the console line is the report there.
fn send(port: u32, line: &str) {
    let deadline = Instant::now() + REPORT_RETRY;
    loop {
        match vsock::Stream::connect(vsock::HOST_CID, port).and_then(|mut stream| {
            stream.set_timeouts(Duration::from_secs(1))?;
            stream.write_all(format!("{line}\n").as_bytes())?;
            stream.shutdown_write();
            // Wait for the host to close (or the timeout), so the line is
            // delivered before the machine powers off.
            let mut rest = [0u8; 64];
            while stream.read(&mut rest).is_ok_and(|count| count > 0) {}
            Ok(())
        }) {
            Ok(()) => return,
            Err(error) => {
                let unavailable = matches!(
                    error.raw_os_error(),
                    Some(libc::ENODEV | libc::EAFNOSUPPORT)
                );
                if unavailable || Instant::now() >= deadline {
                    log(&format!(
                        "vsock report to port {port} not delivered: {error}"
                    ));
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// Reports an error and powers off: the end of every failure.
pub fn fail(message: &str) -> ! {
    report(&Report::error(message));
    power_off()
}

/// Flushes the disk, remounts it read-only and powers the machine off.
pub fn power_off() -> ! {
    sys::sync();
    if ON_DISK.load(Ordering::SeqCst)
        && let Err(error) = mount::remount_root_readonly()
    {
        log(&format!("remounting the root read-only failed: {error}"));
    }
    sys::power_off()
}
