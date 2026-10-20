//! Version 5's `volumes`: before the process starts, each of the image's
//! `VOLUME` paths that the root lacks is made a directory (owned by root,
//! mode 0755), as Docker makes a volume's mount point. A path that exists,
//! as anything, is left alone; one that cannot be made is logged by its
//! index and skipped, never failing the boot, and so is one in `/proc`,
//! `/sys` or `/dev` (the init's mounts, not the image's).
//!
//! Every path is resolved inside the root (`openat2` with
//! `RESOLVE_IN_ROOT`): a symbolic link on the way leads somewhere inside
//! it, never out of it.

use crate::console::log;
use crate::sys::fs;
use std::ffi::CString;
use std::io;
use std::os::fd::{AsFd, BorrowedFd};

/// What [`make`] did with one path.
#[derive(Debug, PartialEq, Eq)]
pub enum Made {
    /// It was there already.
    Existed,
    /// Its missing directories were made.
    Created,
}

fn c_path(path: &[u8]) -> io::Result<CString> {
    CString::new(path).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

/// Makes the missing directories of `path` (without its leading `/`,
/// normalised as the config's paths are) inside `root`.
pub fn make(root: BorrowedFd<'_>, path: &[u8]) -> io::Result<Made> {
    let flags = libc::O_PATH | libc::O_NOFOLLOW;
    match fs::open_in_root(root, &c_path(path)?, flags) {
        Ok(_) => return Ok(Made::Existed),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let directory = libc::O_RDONLY | libc::O_DIRECTORY;
    let mut parent = fs::open_in_root(root, c".", directory)?;
    let mut end = 0;
    for part in path.split(|&byte| byte == b'/') {
        end += part.len();
        let name = c_path(part)?;
        match fs::mkdir_at(parent.as_fd(), &name, 0o755) {
            // The mode as given, whatever the umask.
            Ok(()) => fs::chmod_at(parent.as_fd(), &name, 0o755)?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        // The path so far, resolved inside the root: a symbolic link the
        // part is leads where it leads inside it.
        let so_far = path.get(..end).unwrap_or(path);
        parent = fs::open_in_root(root, &c_path(so_far)?, directory)?;
        end += 1;
    }
    Ok(Made::Created)
}

/// The top-level directories that are the init's mounts (`/proc`, `/sys`,
/// `/dev`), not the image's: a volume path below one is not made.
const SYSTEM: &[&[u8]] = &[b"proc", b"sys", b"dev"];

/// Whether `path` (without its leading `/`) is one of [`SYSTEM`] or below
/// one.
pub fn is_system(path: &[u8]) -> bool {
    let first = path.split(|&byte| byte == b'/').next().unwrap_or_default();
    SYSTEM.contains(&first)
}

/// Makes every path of `volumes` the root lacks, in order, and logs what
/// it did. A path in the init's own mounts ([`is_system`]) is skipped and
/// logged by its index.
pub fn make_all(volumes: &[Vec<u8>]) {
    if volumes.is_empty() {
        return;
    }
    let root = match std::fs::File::open("/") {
        Ok(root) => root,
        Err(error) => {
            log(&format!(
                "the volume paths were not made: opening / failed: {error}"
            ));
            return;
        }
    };
    let mut created = 0;
    for (index, path) in volumes.iter().enumerate() {
        if is_system(path) {
            log(&format!(
                "volume path {index} was not made: it is in /proc, /sys or /dev"
            ));
            continue;
        }
        match make(root.as_fd(), path) {
            Ok(Made::Created) => created += 1,
            Ok(Made::Existed) => {}
            Err(error) => log(&format!("volume path {index} was not made: {error}")),
        }
    }
    if created > 0 {
        log(&format!("made {created} missing volume path(s)"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs as stdfs;
    use std::os::unix::fs::{MetadataExt, symlink};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A temporary directory with the root at `root/` inside it, so
    /// anything made outside the root would show next to it.
    struct Scratch {
        dir: PathBuf,
    }

    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "gigahost-init-volumes-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = stdfs::remove_dir_all(&dir);
            stdfs::create_dir_all(dir.join("root")).unwrap();
            Scratch { dir }
        }

        fn root(&self) -> PathBuf {
            self.dir.join("root")
        }

        fn make(&self, path: &str) -> io::Result<Made> {
            let root = stdfs::File::open(self.root()).unwrap();
            make(root.as_fd(), path.as_bytes())
        }

        fn outside(&self) -> Vec<String> {
            let mut names: Vec<String> = stdfs::read_dir(&self.dir)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = stdfs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn system_paths() {
        for path in ["proc", "sys", "dev", "proc/x", "sys/fs/cgroup", "dev/shm/a"] {
            assert!(is_system(path.as_bytes()), "{path}");
        }
        for path in ["process", "system/x", "devices", "app/proc", "data/dev"] {
            assert!(!is_system(path.as_bytes()), "{path}");
        }
    }

    #[test]
    fn makes_nested_missing_directories() {
        let scratch = Scratch::new();
        assert_eq!(scratch.make("app/data/db").unwrap(), Made::Created);
        for path in ["app", "app/data", "app/data/db"] {
            let meta = stdfs::symlink_metadata(scratch.root().join(path)).unwrap();
            assert!(meta.is_dir(), "{path}");
            assert_eq!(meta.mode() & 0o7777, 0o755, "{path}");
        }
        assert_eq!(scratch.make("app/data/db").unwrap(), Made::Existed);
        assert_eq!(scratch.make("app/other").unwrap(), Made::Created);
        assert!(scratch.root().join("app/other").is_dir());
    }

    #[test]
    fn leaves_what_exists_alone() {
        let scratch = Scratch::new();
        stdfs::write(scratch.root().join("file"), "x").unwrap();
        symlink("/nowhere", scratch.root().join("dangling")).unwrap();
        assert_eq!(scratch.make("file").unwrap(), Made::Existed);
        assert_eq!(scratch.make("dangling").unwrap(), Made::Existed);
        assert_eq!(
            stdfs::read_to_string(scratch.root().join("file")).unwrap(),
            "x"
        );
        assert!(
            stdfs::symlink_metadata(scratch.root().join("dangling"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn a_link_on_the_way_stays_inside_the_root() {
        let scratch = Scratch::new();
        stdfs::create_dir(scratch.root().join("srv")).unwrap();
        // Absolute and climbing links resolve inside the root.
        symlink("/srv", scratch.root().join("data")).unwrap();
        symlink("../../../..", scratch.root().join("up")).unwrap();
        assert_eq!(scratch.make("data/vol").unwrap(), Made::Created);
        assert!(scratch.root().join("srv/vol").is_dir());
        assert_eq!(scratch.make("up/escaped").unwrap(), Made::Created);
        assert!(scratch.root().join("escaped").is_dir());
        assert_eq!(scratch.outside(), ["root"]);
    }

    #[test]
    fn a_file_in_the_way_is_an_error() {
        let scratch = Scratch::new();
        stdfs::write(scratch.root().join("file"), "x").unwrap();
        let error = scratch.make("file/vol").unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ENOTDIR));
        symlink("/missing/dir", scratch.root().join("dangling")).unwrap();
        assert!(scratch.make("dangling/vol").is_err());
        assert!(!scratch.root().join("missing").exists());
        assert_eq!(scratch.outside(), ["root"]);
    }
}
