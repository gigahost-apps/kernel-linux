//! gigahost-init: PID 1 of a container virtual machine.
//!
//! It runs from the initramfs, switches into the renter's disk, sets up
//! the system as a container runtime would, runs the image's process and
//! powers the machine off when the process ends. docs/interface.md is the
//! contract with the host.
//!
//! As PID 1 it must never exit or panic (either panics the kernel): every
//! failure ends in an error report and a power-off.

mod cmdline;
mod config;
mod console;
mod etc;
mod grow;
mod protocol;
mod setup;
mod supervise;
mod sys;
mod unpack;
mod user;

fn main() {
    if sys::getpid() != 1 {
        eprintln!("gigahost-init runs only as PID 1 of a container virtual machine");
        #[allow(clippy::exit, reason = "not PID 1, so exiting is safe")]
        std::process::exit(2);
    }
    // A panic would abort PID 1; end the machine the documented way
    // instead. The message is fixed: a panic's text could quote a value.
    std::panic::set_hook(Box::new(|_| console::fail("the init failed internally")));
    setup::run()
}

#[cfg(test)]
mod tests {
    /// One list of interface versions across the command line, the config
    /// and the vsock lines; scripts/build.sh reads `cmdline::VERSIONS` for
    /// build-info.json.
    #[test]
    fn one_interface_version() {
        assert_eq!(crate::cmdline::VERSIONS, &[1, 2, 3]);
        let config: Vec<u32> = crate::config::VERSIONS
            .iter()
            .map(|&v| u32::try_from(v).unwrap())
            .collect();
        assert_eq!(config, crate::cmdline::VERSIONS);
        assert_eq!(crate::protocol::VERSIONS, crate::cmdline::VERSIONS);
    }
}
