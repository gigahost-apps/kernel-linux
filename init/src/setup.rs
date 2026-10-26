//! Steps 1 to 8 of docs/interface.md: from the initramfs to the started
//! process, or in the unpack mode to the unpacked layers ([`unpack`]).
//! Every failure ends in [`console::fail`].

use crate::cmdline::{self, KernelArgs};
use crate::config::{self, Boot, Config};
use crate::console::{self, fail, log};
use crate::etc;
use crate::grow;
use crate::protocol::{self, Report};
use crate::seed;
use crate::supervise;
use crate::sys::{self, mount, net, process};
use crate::unpack;
use crate::user::{self, ExecUser};
use crate::volumes;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::os::fd::AsFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::{Duration, Instant};

pub const NEWROOT: &str = "/newroot";
pub const DISK: &str = "/dev/vda";
const DISK_WAIT: Duration = Duration::from_secs(5);
const OUR_FILES: &str = "/run/gigahost";

/// The cgroup the process's cgroup namespace starts at.
const PAYLOAD_CGROUP: &str = "/sys/fs/cgroup/payload";

/// What the image's init system reads to know it runs in a container,
/// before the config's environment, which may set either.
const INIT_SYSTEM_ENV: &[&str] = &["container=gigahost", "container_ttys="];

/// Runs the whole init; never returns.
pub fn run() -> ! {
    early();
    log(&format!("start {}", sys::monotonic_ms()));
    let cmdline = fs::read_to_string("/proc/cmdline")
        .unwrap_or_else(|error| fail(&format!("reading /proc/cmdline failed: {error}")));
    if let Some(port) = cmdline::vsock_port(&cmdline) {
        console::set_port(port);
    }
    if let Some(version) = cmdline::version(&cmdline) {
        protocol::set_version(version);
    }
    let args = cmdline::parse(&cmdline).unwrap_or_else(|error| fail(&error));
    let config = match read_config(args.version).unwrap_or_else(|error| fail(&error)) {
        Boot::Run(config) => config,
        Boot::Unpack(layers) => unpack::run(&layers),
    };
    switch_root().unwrap_or_else(|error| fail(&error));
    console::set_on_disk();
    grow::root(DISK);
    system_mounts().unwrap_or_else(|error| fail(&error));
    identity_files(&args).unwrap_or_else(|error| fail(&error));
    network(&args).unwrap_or_else(|error| fail(&format!("network setup failed: {error}")));
    if let Some(keys) = &config.authorized_keys {
        write_authorized_keys(keys).unwrap_or_else(|error| fail(&error));
    }
    volumes::make_all(&config.volumes);
    let listener = supervise::bind_control(args.vsock);
    let pid = start(&config, &args).unwrap_or_else(|error| fail(&error));
    console::report(&Report::started(pid, sys::monotonic_ms()));
    supervise::run(
        pid,
        listener,
        config.stop_signal,
        config.stop_timeout_seconds,
        config.init_system,
    )
}

fn create_dir(path: &str, mode: u32) -> io::Result<()> {
    match fs::DirBuilder::new().mode(mode).create(path) {
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        other => other,
    }
}

/// Step 1, in the initramfs: devtmpfs, the console, proc and sysfs.
fn early() {
    let mut problems = Vec::new();
    for directory in ["/dev", "/proc", "/sys", NEWROOT] {
        if let Err(error) = create_dir(directory, 0o755) {
            problems.push(format!("creating {directory} failed: {error}"));
        }
    }
    if let Err(error) = mount::mount(
        Some("devtmpfs"),
        "/dev",
        Some("devtmpfs"),
        mount::NOSUID,
        Some("mode=0755"),
    ) {
        problems.push(format!("mounting devtmpfs failed: {error}"));
    }
    // Without a console nothing can be logged; the vsock report still
    // goes out.
    let _ = sys::console_as_stdio();
    let flags = mount::NOSUID | mount::NODEV | mount::NOEXEC;
    if let Err(error) = mount::mount(Some("proc"), "/proc", Some("proc"), flags, None) {
        problems.push(format!("mounting proc failed: {error}"));
    }
    if let Err(error) = mount::mount(Some("sysfs"), "/sys", Some("sysfs"), flags, None) {
        problems.push(format!("mounting sysfs failed: {error}"));
    }
    if let Some(problem) = problems.first() {
        fail(problem);
    }
}

/// Step 2's config, from `fw_cfg`, for a boot of interface `version`.
fn read_config(version: u32) -> Result<Boot, String> {
    let file = fs::File::open(config::PATH).map_err(|error| {
        format!("the config (fw_cfg opt/gigahost/config) cannot be opened: {error}")
    })?;
    let mut bytes = Vec::new();
    let limit = u64::try_from(config::MAX_BYTES)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    file.take(limit)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("reading the config failed: {error}"))?;
    config::parse(&bytes, version)
}

/// Waits up to 5 s for a disk to appear.
pub fn wait_for(disk: &str) -> Result<(), String> {
    let deadline = Instant::now() + DISK_WAIT;
    while !Path::new(disk).exists() {
        if Instant::now() >= deadline {
            return Err(format!("no disk: {disk} did not appear within 5 s"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

/// Step 3: mounts the disk, moves /dev, /proc and /sys onto it, frees the
/// initramfs and makes the disk the root.
fn switch_root() -> Result<(), String> {
    wait_for(DISK)?;
    mount::mount(Some(DISK), NEWROOT, Some("ext4"), 0, None)
        .map_err(|error| format!("mounting the disk failed: {error}"))?;
    for directory in ["dev", "proc", "sys"] {
        let target = format!("{NEWROOT}/{directory}");
        match fs::symlink_metadata(&target) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Err(format!("the image's /{directory} is not a directory")),
            Err(_) => create_dir(&target, 0o755)
                .map_err(|error| format!("creating /{directory} on the disk failed: {error}"))?,
        }
        mount::move_mount(&format!("/{directory}"), &target)
            .map_err(|error| format!("moving /{directory} failed: {error}"))?;
    }
    std::env::set_current_dir(NEWROOT)
        .map_err(|error| format!("entering the disk failed: {error}"))?;
    free_initramfs();
    mount::move_mount(NEWROOT, "/")
        .map_err(|error| format!("moving the disk to / failed: {error}"))?;
    sys::chroot(".").map_err(|error| format!("chroot failed: {error}"))?;
    std::env::set_current_dir("/").map_err(|error| format!("entering / failed: {error}"))
}

/// Deletes the initramfs's files (as `switch_root` does), staying on its
/// own file system and away from the disk's mount point, so its memory is
/// freed.
fn free_initramfs() {
    let Ok(root) = fs::symlink_metadata("/") else {
        return;
    };
    remove_tree(Path::new("/"), root.dev());
}

fn remove_tree(directory: &Path, device: u64) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == Path::new(NEWROOT) {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.dev() != device {
            continue;
        }
        if meta.is_dir() {
            remove_tree(&path, device);
            let _ = fs::remove_dir(&path);
        } else {
            let _ = fs::remove_file(&path);
        }
    }
}

/// Step 4: devpts, /dev/shm, /dev/mqueue, cgroup2 and /run, as Docker
/// mounts them; /run seeded with the image's own (see [`seed`]).
fn system_mounts() -> Result<(), String> {
    let quiet = mount::NOSUID | mount::NODEV | mount::NOEXEC;
    let each: [(&str, &str, &str, u64, &str); 4] = [
        (
            "devpts",
            "/dev/pts",
            "devpts",
            mount::NOSUID | mount::NOEXEC,
            "newinstance,ptmxmode=0666,mode=0620,gid=5",
        ),
        ("shm", "/dev/shm", "tmpfs", quiet, "mode=1777,size=65536k"),
        ("mqueue", "/dev/mqueue", "mqueue", quiet, ""),
        ("cgroup2", "/sys/fs/cgroup", "cgroup2", quiet, "nsdelegate"),
    ];
    for (source, target, fstype, flags, data) in each {
        create_dir(target, 0o755).map_err(|error| format!("creating {target} failed: {error}"))?;
        let data = (!data.is_empty()).then_some(data);
        mount::mount(Some(source), target, Some(fstype), flags, data)
            .map_err(|error| format!("mounting {target} failed: {error}"))?;
    }
    if fs::symlink_metadata("/dev/ptmx").is_err() {
        fs::File::create("/dev/ptmx")
            .map_err(|error| format!("creating /dev/ptmx failed: {error}"))?;
    }
    mount::bind("/dev/pts/ptmx", "/dev/ptmx")
        .map_err(|error| format!("binding /dev/ptmx failed: {error}"))?;
    for (link, target) in [
        ("/dev/fd", "/proc/self/fd"),
        ("/dev/stdin", "/proc/self/fd/0"),
        ("/dev/stdout", "/proc/self/fd/1"),
        ("/dev/stderr", "/proc/self/fd/2"),
    ] {
        let _ = std::os::unix::fs::symlink(target, link);
    }
    // The disk's own /run, opened before the tmpfs covers it, when it is
    // a directory; a symbolic link there is left as it is (the mount
    // lands where it leads), with nothing to copy.
    let root = fs::File::open("/").map_err(|error| format!("opening / failed: {error}"))?;
    let image_run = match fs::symlink_metadata("/run") {
        Ok(meta) if meta.is_dir() => match sys::fs::Dir::open_at(root.as_fd(), c"run") {
            Ok(dir) => Some(dir),
            Err(error) => {
                log(&format!(
                    "the image's /run is not copied: opening it failed: {error}"
                ));
                None
            }
        },
        Ok(_) => None,
        Err(_) => {
            create_dir("/run", 0o755)
                .map_err(|error| format!("creating /run on the disk failed: {error}"))?;
            None
        }
    };
    mount::mount(
        Some("tmpfs"),
        "/run",
        Some("tmpfs"),
        mount::NOSUID | mount::NODEV,
        Some("mode=0755"),
    )
    .map_err(|error| format!("mounting /run failed: {error}"))?;
    if let Some(mut image_run) = image_run {
        seed_run(&mut image_run, &root);
    }
    Ok(())
}

/// Copies the image's /run into the tmpfs now over it; never fails the
/// boot.
fn seed_run(image_run: &mut sys::fs::Dir, root: &fs::File) {
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW;
    let tmpfs = match sys::fs::open_at(root.as_fd(), c"run", flags, 0) {
        Ok(tmpfs) => tmpfs,
        Err(error) => {
            log(&format!(
                "the image's /run is not copied: opening the tmpfs failed: {error}"
            ));
            return;
        }
    };
    let outcome = seed::copy(image_run, tmpfs.as_fd(), seed::BOUNDS);
    if outcome.skipped > 0 {
        log(&format!(
            "the image's /run: {} FIFO(s), socket(s) or device(s) not copied",
            outcome.skipped
        ));
    }
    if let Some(stop) = outcome.stopped {
        log(&format!(
            "copying the image's /run stopped after {} entries: {}",
            outcome.copied,
            stop.describe(&seed::BOUNDS)
        ));
    }
}

/// Step 5: the host name, and /etc/hosts, /etc/resolv.conf and
/// /etc/hostname bind-mounted from /run/gigahost.
fn identity_files(args: &KernelArgs) -> Result<(), String> {
    sys::sethostname(&args.hostname)
        .map_err(|error| format!("setting the host name failed: {error}"))?;
    create_dir(OUR_FILES, 0o755)
        .map_err(|error| format!("creating {OUR_FILES} failed: {error}"))?;
    for (name, text) in [
        ("hosts", etc::hosts(args)),
        ("resolv.conf", etc::resolv_conf(args)),
        ("hostname", etc::hostname(args)),
    ] {
        let source = format!("{OUR_FILES}/{name}");
        let target = format!("/etc/{name}");
        fs::write(&source, text).map_err(|error| format!("writing {source} failed: {error}"))?;
        let _ = fs::set_permissions(&source, fs::Permissions::from_mode(0o644));
        prepare_target(&target)?;
        mount::bind_over(&source, &target)
            .map_err(|error| format!("mounting {target} failed: {error}"))?;
    }
    Ok(())
}

/// Makes sure there is something to mount over: an existing file or
/// symbolic link is kept as it is (the mount covers it), a missing one is
/// created empty.
fn prepare_target(target: &str) -> Result<(), String> {
    match fs::symlink_metadata(target) {
        Ok(meta) if meta.is_dir() => Err(format!("the image's {target} is a directory")),
        Ok(_) => Ok(()),
        Err(_) => {
            create_dir("/etc", 0o755).map_err(|error| format!("creating /etc failed: {error}"))?;
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o644)
                .custom_flags(libc::O_NOFOLLOW)
                .open(target)
                .map(drop)
                .map_err(|error| format!("creating {target} failed: {error}"))
        }
    }
}

fn in_prefix4(address: Ipv4Addr, network: Ipv4Addr, prefix: u8) -> bool {
    let mask = u32::MAX
        .checked_shl(32 - u32::from(prefix.min(32)))
        .unwrap_or(0);
    u32::from(address) & mask == u32::from(network) & mask
}

fn in_prefix6(address: Ipv6Addr, network: Ipv6Addr, prefix: u8) -> bool {
    let mask = u128::MAX
        .checked_shl(128 - u32::from(prefix.min(128)))
        .unwrap_or(0);
    u128::from(address) & mask == u128::from(network) & mask
}

/// Step 6: lo and eth0, from the command line.
fn network(args: &KernelArgs) -> Result<(), String> {
    let step = |what: &'static str| move |error: io::Error| format!("{what}: {error}");
    net::set_up("lo").map_err(step("lo up"))?;
    if args.ip6.is_some() {
        // The platform hands the address out: no duplicate detection, so
        // the addresses are usable at once instead of tentative.
        fs::write("/proc/sys/net/ipv6/conf/eth0/accept_dad", "0")
            .map_err(step("eth0 accept_dad"))?;
    }
    if let Some(mtu) = args.mtu {
        net::set_mtu("eth0", mtu).map_err(step("eth0 mtu"))?;
    }
    let (address, prefix) = args.ip4;
    net::add_ipv4("eth0", address, prefix).map_err(step("eth0 IPv4 address"))?;
    net::set_up("eth0").map_err(step("eth0 up"))?;
    if !in_prefix4(args.gw4, address, prefix) {
        net::add_ipv4_route("eth0", args.gw4, 32, None).map_err(step("IPv4 gateway route"))?;
    }
    net::add_ipv4_route("eth0", Ipv4Addr::UNSPECIFIED, 0, Some(args.gw4))
        .map_err(step("IPv4 default route"))?;
    if let (Some((address, prefix)), Some(gateway)) = (args.ip6, args.gw6) {
        let index = net::index("eth0").map_err(step("eth0 index"))?;
        net::add_ipv6(index, address, prefix).map_err(step("eth0 IPv6 address"))?;
        let link_local = gateway
            .segments()
            .first()
            .is_some_and(|first| first & 0xffc0 == 0xfe80);
        if !link_local && !in_prefix6(gateway, address, prefix) {
            net::add_ipv6_route(index, gateway, 128, None).map_err(step("IPv6 gateway route"))?;
        }
        net::add_ipv6_route(index, Ipv6Addr::UNSPECIFIED, 0, Some(gateway))
            .map_err(step("IPv6 default route"))?;
    }
    Ok(())
}

fn read_accounts() -> (Vec<user::Passwd>, Vec<user::Group>) {
    let passwd = fs::read_to_string("/etc/passwd").unwrap_or_default();
    let group = fs::read_to_string("/etc/group").unwrap_or_default();
    (user::parse_passwd(&passwd), user::parse_group(&group))
}

/// Step 7: the stock images' SSH keys, written as cloud-init writes them.
fn write_authorized_keys(keys: &config::AuthorizedKeys) -> Result<(), String> {
    let (passwd, groups) = read_accounts();
    let owner = user::resolve(&keys.user, &passwd, &groups)
        .map_err(|error| format!("authorized_keys: {}", error.message()))?;
    let home = owner.home.trim_end_matches('/');
    if !fs::symlink_metadata(if home.is_empty() { "/" } else { home })
        .is_ok_and(|meta| meta.is_dir())
    {
        return Err("authorized_keys: the user's home directory is missing".into());
    }
    let directory = format!("{home}/.ssh");
    match fs::symlink_metadata(&directory) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Err("authorized_keys: the user's ~/.ssh is not a directory".into()),
        Err(_) => create_dir(&directory, 0o700)
            .map_err(|error| format!("authorized_keys: creating ~/.ssh failed: {error}"))?,
    }
    let fail_with = |what: &'static str| {
        move |error: io::Error| format!("authorized_keys: {what} failed: {error}")
    };
    std::os::unix::fs::lchown(&directory, Some(owner.uid), Some(owner.gid))
        .map_err(fail_with("chown ~/.ssh"))?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
        .map_err(fail_with("chmod ~/.ssh"))?;
    let temporary = format!("{directory}/.authorized_keys.gigahost-init");
    let _ = fs::remove_file(&temporary);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temporary)
        .map_err(fail_with("creating the key file"))?;
    let text = keys
        .keys
        .iter()
        .fold(String::new(), |text, key| text + key + "\n");
    file.write_all(text.as_bytes())
        .map_err(fail_with("writing the key file"))?;
    std::os::unix::fs::fchown(&file, Some(owner.uid), Some(owner.gid))
        .map_err(fail_with("chown the key file"))?;
    file.sync_all().map_err(fail_with("syncing the key file"))?;
    fs::rename(&temporary, format!("{directory}/authorized_keys"))
        .map_err(fail_with("replacing authorized_keys"))?;
    log(&format!("wrote {} SSH key(s)", keys.keys.len()));
    Ok(())
}

/// The payload cgroup the process's cgroup namespace starts at, with
/// every controller the kernel offers enabled for it. The init stays in
/// the root cgroup, which cgroup v2's no-internal-process rule exempts.
fn payload_cgroup() -> Result<(), String> {
    create_dir(PAYLOAD_CGROUP, 0o755)
        .map_err(|error| format!("creating the payload cgroup failed: {error}"))?;
    let controllers = fs::read_to_string("/sys/fs/cgroup/cgroup.controllers")
        .map_err(|error| format!("reading the cgroup controllers failed: {error}"))?;
    for controller in controllers.split_ascii_whitespace() {
        fs::write(
            "/sys/fs/cgroup/cgroup.subtree_control",
            format!("+{controller}"),
        )
        .map_err(|error| format!("enabling the cgroup controller {controller} failed: {error}"))?;
    }
    Ok(())
}

/// Where the process starts, in every mode: as Docker starts a
/// container's entrypoint, PID 1 of PID, mount and cgroup namespaces of
/// its own, the cgroup namespace's root the payload cgroup, `output`
/// bound over `/dev/console` inside when it is the hypervisor console.
fn namespace(output: &str) -> Result<process::Namespace, String> {
    let path = |text: &str| sys::cstring(text).map_err(|_| "a path holds a NUL byte".to_owned());
    Ok(process::Namespace {
        cgroup_procs: path(&format!("{PAYLOAD_CGROUP}/cgroup.procs"))?,
        console: (output == "/dev/hvc0").then(|| path(output)).transpose()?,
    })
}

/// Step 8: the process, as Docker starts it, or (version 3's
/// `init_system`) the image's init; either is PID 1 of namespaces of its
/// own ([`namespace`]). Returns its PID here.
fn start(config: &Config, args: &KernelArgs) -> Result<i32, String> {
    let (passwd, groups) = read_accounts();
    let ExecUser {
        uid,
        gid,
        groups,
        home,
    } = user::resolve(config.user.as_deref().unwrap_or(""), &passwd, &groups)
        .map_err(|error| error.message().to_owned())?;
    let env = if config.init_system {
        if uid != 0 || gid != 0 {
            return Err("config field `user` is not root, which `init_system` requires".into());
        }
        let mut env: Vec<String> = INIT_SYSTEM_ENV
            .iter()
            .map(|&entry| entry.to_owned())
            .collect();
        env.extend(config.env.iter().cloned());
        etc::process_env(&env, &args.hostname, &home)
    } else {
        etc::process_env(&config.env, &args.hostname, &home)
    };
    if fs::metadata(&config.workdir).is_err() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(&config.workdir)
            .map_err(|error| format!("creating the working directory failed: {error}"))?;
    }
    let first = config.argv.first().map_or("", String::as_str);
    let program = etc::look_path(first, etc::path_of(&env), Path::new(&config.workdir)).ok_or(
        if config.init_system {
            "the image's init (`argv`) was not found or is not executable"
        } else {
            "the command was not found in PATH"
        },
    )?;
    let output = if Path::new("/dev/hvc0").exists() {
        match sys::open_raw_tty("/dev/hvc0") {
            // Held open for the machine's life, which keeps it raw.
            Ok(terminal) => std::mem::forget(terminal),
            Err(error) => log(&format!("/dev/hvc0 stays in cooked mode: {error}")),
        }
        "/dev/hvc0"
    } else {
        "/dev/console"
    };
    // Apps may reopen their streams after dropping privileges to any UID.
    fs::set_permissions(output, fs::Permissions::from_mode(0o622))
        .map_err(|error| format!("setting output device {output} permissions failed: {error}"))?;
    let mut spawn = process::Spawn::new(
        &program,
        &config.argv,
        &env,
        &config.workdir,
        output,
        (uid, gid, &groups),
    )
    .map_err(|_| "the command, environment or working directory holds a NUL byte".to_owned())?;
    payload_cgroup()?;
    spawn.namespace = Some(namespace(output)?);
    process::spawn(&spawn).map_err(|error| match error {
        process::SpawnError::Parent(error) => format!("starting the process failed: {error}"),
        process::SpawnError::Child(stage, error) => {
            let what = match stage {
                process::Stage::Output => "opening the output",
                process::Stage::Workdir => "entering the working directory",
                process::Stage::Identity => "switching to the user",
                process::Stage::Exec if config.init_system => "executing the image's init",
                process::Stage::Exec => "executing the command",
                process::Stage::Namespace => "entering the process's namespaces",
                process::Stage::Mounts => "mounting the process's namespace",
            };
            format!("{what} failed: {error}")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes() {
        let net4 = Ipv4Addr::new(192, 0, 2, 10);
        assert!(in_prefix4(Ipv4Addr::new(192, 0, 2, 1), net4, 24));
        assert!(!in_prefix4(Ipv4Addr::new(192, 0, 3, 1), net4, 24));
        assert!(!in_prefix4(Ipv4Addr::new(192, 0, 2, 1), net4, 32));
        assert!(in_prefix4(Ipv4Addr::new(10, 0, 0, 1), net4, 0));
        let net6: Ipv6Addr = "2001:db8::10".parse().unwrap();
        assert!(in_prefix6("2001:db8::1".parse().unwrap(), net6, 64));
        assert!(!in_prefix6("2001:db9::1".parse().unwrap(), net6, 64));
        assert!(!in_prefix6("2001:db8::1".parse().unwrap(), net6, 128));
    }

    /// The run mode and `init_system` start in the same namespaces; the
    /// console is bound inside only when it is the hypervisor console.
    #[test]
    fn every_mode_starts_in_a_namespace() {
        let hvc = namespace("/dev/hvc0").unwrap();
        assert_eq!(
            hvc.cgroup_procs.to_bytes(),
            b"/sys/fs/cgroup/payload/cgroup.procs"
        );
        assert_eq!(hvc.console.unwrap().to_bytes(), b"/dev/hvc0");
        let console = namespace("/dev/console").unwrap();
        assert!(console.console.is_none());
    }
}
