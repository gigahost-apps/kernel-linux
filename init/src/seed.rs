//! Step 4's `/run`: the tmpfs mounted there is seeded with the image's own
//! `/run` (the disk's directory under it), so what an image ships there
//! (`/run/mysqld` owned by its user, `/run/php`, `/run/nginx`) is there
//! when its process starts, as under Docker, which mounts nothing on
//! `/run`.
//!
//! The copy reads the disk's tree through descriptors only, one name at a
//! time, never following a symbolic link, and is bounded by [`BOUNDS`]: an
//! image cannot make it unbounded. Directories, regular files and symbolic
//! links are copied with their owner, group, mode and modification time;
//! FIFOs, sockets and devices are skipped and counted. A copy that meets a
//! bound or an error stops there; it never fails the boot.

use crate::sys::fs::{self, Dir, Stat};
use std::ffi::CStr;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

/// How much of an image's `/run` is copied at most.
#[derive(Clone, Copy, Debug)]
pub struct Bounds {
    /// Entries of every kind, the skipped ones included.
    pub entries: usize,
    /// Bytes of regular files' data.
    pub bytes: u64,
    /// Components below `/run` (`/run/a/b` is at depth 2).
    pub depth: usize,
}

pub const BOUNDS: Bounds = Bounds {
    entries: 4096,
    bytes: 16 << 20,
    depth: 32,
};

/// The name in `/run` that is the init's own (`/run/gigahost`): an image's
/// entry by that name is not copied.
const OURS: &CStr = c"gigahost";

/// Why a copy stopped before its end.
#[derive(Debug, PartialEq, Eq)]
pub enum Stop {
    Entries,
    Bytes,
    Depth,
    /// What failed, and why.
    Failed(&'static str, io::ErrorKind),
}

impl Stop {
    /// The log line's words for it.
    pub fn describe(&self, bounds: &Bounds) -> String {
        match self {
            Stop::Entries => format!("it holds more than {} entries", bounds.entries),
            Stop::Bytes => format!("its files hold more than {} bytes", bounds.bytes),
            Stop::Depth => format!("it is more than {} directories deep", bounds.depth),
            Stop::Failed(what, kind) => format!("{what} failed: {kind}"),
        }
    }
}

/// What a copy did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Entries copied.
    pub copied: usize,
    /// FIFOs, sockets and devices, not copied.
    pub skipped: usize,
    /// Why it stopped early, if it did.
    pub stopped: Option<Stop>,
}

struct Copy {
    bounds: Bounds,
    entries: usize,
    bytes: u64,
    outcome: Outcome,
}

fn failed(what: &'static str) -> impl Fn(io::Error) -> Stop {
    move |error| Stop::Failed(what, error.kind())
}

/// Copies the tree of `source` (the disk's `/run`) into the directory
/// `target` (the tmpfs), and gives `target` the source's owner, group,
/// mode and time. An entry named `gigahost` directly in the source is
/// left out.
pub fn copy(source: &mut Dir, target: BorrowedFd<'_>, bounds: Bounds) -> Outcome {
    let mut copy = Copy {
        bounds,
        entries: 0,
        bytes: 0,
        outcome: Outcome::default(),
    };
    let result = copy
        .tree(source, target, 1)
        .and_then(|()| fs::stat_at(source.fd(), c"").map_err(failed("reading /run")))
        .and_then(|stat| metadata_fd(target, &stat));
    copy.outcome.stopped = result.err();
    copy.outcome
}

/// The owner, group, mode and time of `stat` given to the open `fd`
/// (the owner first: a change of owner clears the setuid and setgid
/// bits).
fn metadata_fd(fd: BorrowedFd<'_>, stat: &Stat) -> Result<(), Stop> {
    fs::chown_at(fd, c"", stat.uid, stat.gid).map_err(failed("setting an owner"))?;
    fs::chmod_fd(fd, stat.mode & 0o7777).map_err(failed("setting a mode"))?;
    fs::set_times_fd(fd, stat.mtime.0, stat.mtime.1).map_err(failed("setting a time"))
}

impl Copy {
    /// Copies the entries of `source` into `target`; they are at `depth`.
    fn tree(&mut self, source: &mut Dir, target: BorrowedFd<'_>, depth: usize) -> Result<(), Stop> {
        while let Some(name) = source.next_name().map_err(failed("reading a directory"))? {
            if depth == 1 && name.as_c_str() == OURS {
                continue;
            }
            if depth > self.bounds.depth {
                return Err(Stop::Depth);
            }
            if self.entries >= self.bounds.entries {
                return Err(Stop::Entries);
            }
            self.entries += 1;
            let stat = fs::stat_at(source.fd(), &name).map_err(failed("reading an entry"))?;
            if stat.is_dir() {
                self.directory(source, target, &name, &stat, depth)?;
            } else if stat.is_file() {
                self.file(source, target, &name, &stat)?;
            } else if stat.is_symlink() {
                let link =
                    fs::read_link_at(source.fd(), &name).map_err(failed("reading a link"))?;
                let link = std::ffi::CString::new(link)
                    .map_err(|_| Stop::Failed("reading a link", io::ErrorKind::InvalidData))?;
                fs::symlink_at(&link, target, &name).map_err(failed("making a link"))?;
                fs::chown_at(target, &name, stat.uid, stat.gid)
                    .map_err(failed("setting an owner"))?;
                fs::set_times_at(target, &name, stat.mtime.0, stat.mtime.1)
                    .map_err(failed("setting a time"))?;
            } else {
                self.outcome.skipped += 1;
                continue;
            }
            self.outcome.copied += 1;
        }
        Ok(())
    }

    fn directory(
        &mut self,
        source: &Dir,
        target: BorrowedFd<'_>,
        name: &CStr,
        stat: &Stat,
        depth: usize,
    ) -> Result<(), Stop> {
        let mut inner = Dir::open_at(source.fd(), name).map_err(failed("opening a directory"))?;
        fs::mkdir_at(target, name, 0o700).map_err(failed("making a directory"))?;
        let made = fs::open_at(
            target,
            name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            0,
        )
        .map_err(failed("opening a directory"))?;
        let result = self.tree(&mut inner, made.as_fd(), depth + 1);
        // The directory's own metadata even when its copy stopped, so what
        // was copied of it is as the image has it.
        let own = metadata_fd(made.as_fd(), stat);
        result.and(own)
    }

    fn file(
        &mut self,
        source: &Dir,
        target: BorrowedFd<'_>,
        name: &CStr,
        stat: &Stat,
    ) -> Result<(), Stop> {
        let left = self.bounds.bytes.saturating_sub(self.bytes);
        if stat.size > left {
            return Err(Stop::Bytes);
        }
        let from = fs::open_at(
            source.fd(),
            name,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            0,
        )
        .map_err(failed("opening a file"))?;
        let to: OwnedFd = fs::open_at(
            target,
            name,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
            0o600,
        )
        .map_err(failed("making a file"))?;
        let mut from = std::fs::File::from(from);
        let mut to = std::fs::File::from(to);
        // At most what is left of the bound, even if the file grew.
        let mut buffer = vec![0u8; 64 << 10];
        let mut reader = (&mut from).take(left);
        loop {
            let count = match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(failed("reading a file")(error)),
            };
            let chunk = buffer.get(..count).unwrap_or_default();
            to.write_all(chunk).map_err(failed("writing a file"))?;
            self.bytes = self
                .bytes
                .saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
        }
        metadata_fd(to.as_fd(), stat)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs as stdfs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A temporary directory holding `source/` (the disk's `/run`) and
    /// `target/` (the tmpfs), removed when dropped.
    struct Scratch {
        dir: PathBuf,
    }

    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "gigahost-init-seed-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = stdfs::remove_dir_all(&dir);
            stdfs::create_dir_all(dir.join("source")).unwrap();
            stdfs::create_dir_all(dir.join("target")).unwrap();
            Scratch { dir }
        }

        fn source(&self) -> PathBuf {
            self.dir.join("source")
        }

        fn target(&self) -> PathBuf {
            self.dir.join("target")
        }

        fn copy(&self, bounds: Bounds) -> Outcome {
            let parent = stdfs::File::open(&self.dir).unwrap();
            let mut source = Dir::open_at(parent.as_fd(), c"source").unwrap();
            let target = stdfs::File::open(self.target()).unwrap();
            copy(&mut source, target.as_fd(), bounds)
        }

        /// The target's entries, relative, sorted.
        fn listing(&self) -> Vec<String> {
            fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
                for entry in stdfs::read_dir(dir).unwrap() {
                    let path = entry.unwrap().path();
                    out.push(path.strip_prefix(base).unwrap().display().to_string());
                    if stdfs::symlink_metadata(&path).unwrap().is_dir() {
                        walk(base, &path, out);
                    }
                }
            }
            let mut out = Vec::new();
            walk(&self.target(), &self.target(), &mut out);
            out.sort();
            out
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = stdfs::remove_dir_all(&self.dir);
        }
    }

    fn mode(path: &Path) -> u32 {
        stdfs::symlink_metadata(path).unwrap().mode() & 0o7777
    }

    #[test]
    fn copies_directories_files_and_links() {
        let scratch = Scratch::new();
        let source = scratch.source();
        stdfs::create_dir_all(source.join("mysqld/inner")).unwrap();
        stdfs::write(source.join("mysqld/inner/pid"), "1234\n").unwrap();
        stdfs::write(source.join("top"), "").unwrap();
        stdfs::set_permissions(source.join("mysqld"), stdfs::Permissions::from_mode(0o750))
            .unwrap();
        stdfs::set_permissions(source.join("top"), stdfs::Permissions::from_mode(0o640)).unwrap();
        symlink("/etc/shadow", source.join("away")).unwrap();
        symlink("mysqld", source.join("near")).unwrap();
        stdfs::set_permissions(&source, stdfs::Permissions::from_mode(0o751)).unwrap();
        let mtime = stdfs::metadata(source.join("mysqld/inner/pid"))
            .unwrap()
            .mtime();
        let outcome = scratch.copy(BOUNDS);
        assert_eq!(
            outcome,
            Outcome {
                copied: 6,
                skipped: 0,
                stopped: None
            }
        );
        let target = scratch.target();
        assert_eq!(
            scratch.listing(),
            [
                "away",
                "mysqld",
                "mysqld/inner",
                "mysqld/inner/pid",
                "near",
                "top"
            ]
        );
        assert_eq!(
            stdfs::read_to_string(target.join("mysqld/inner/pid")).unwrap(),
            "1234\n"
        );
        assert_eq!(mode(&target.join("mysqld")), 0o750);
        assert_eq!(mode(&target.join("top")), 0o640);
        assert_eq!(mode(&target), 0o751);
        assert_eq!(
            stdfs::read_link(target.join("away")).unwrap(),
            Path::new("/etc/shadow")
        );
        assert_eq!(
            stdfs::read_link(target.join("near")).unwrap(),
            Path::new("mysqld")
        );
        let pid = stdfs::metadata(target.join("mysqld/inner/pid")).unwrap();
        let mine = stdfs::metadata("/proc/self").unwrap();
        assert_eq!((pid.uid(), pid.gid()), (mine.uid(), mine.gid()));
        assert_eq!(pid.mtime(), mtime);
    }

    #[test]
    fn skips_specials_and_our_own_name() {
        let scratch = Scratch::new();
        let source = scratch.source();
        let made = std::process::Command::new("mkfifo")
            .arg(source.join("fifo"))
            .status()
            .unwrap();
        assert!(made.success());
        let _socket = std::os::unix::net::UnixListener::bind(source.join("socket")).unwrap();
        stdfs::create_dir(source.join("gigahost")).unwrap();
        stdfs::write(source.join("gigahost/hosts"), "image's").unwrap();
        stdfs::create_dir(source.join("sub")).unwrap();
        stdfs::write(source.join("sub/gigahost"), "kept below the top").unwrap();
        let outcome = scratch.copy(BOUNDS);
        assert_eq!(
            outcome,
            Outcome {
                copied: 2,
                skipped: 2,
                stopped: None
            }
        );
        assert_eq!(scratch.listing(), ["sub", "sub/gigahost"]);
    }

    #[test]
    fn stops_at_the_entry_bound() {
        let scratch = Scratch::new();
        for n in 0..5 {
            stdfs::write(scratch.source().join(format!("f{n}")), "x").unwrap();
        }
        let bounds = Bounds {
            entries: 5,
            ..BOUNDS
        };
        assert_eq!(scratch.copy(bounds).stopped, None);
        let scratch = Scratch::new();
        for n in 0..6 {
            stdfs::write(scratch.source().join(format!("f{n}")), "x").unwrap();
        }
        let outcome = scratch.copy(bounds);
        assert_eq!(outcome.stopped, Some(Stop::Entries));
        assert_eq!(outcome.copied, 5);
        assert_eq!(scratch.listing().len(), 5);
    }

    #[test]
    fn stops_at_the_byte_bound() {
        let bounds = Bounds {
            bytes: 10,
            ..BOUNDS
        };
        let scratch = Scratch::new();
        stdfs::write(scratch.source().join("a"), "12345").unwrap();
        stdfs::write(scratch.source().join("b"), "12345").unwrap();
        assert_eq!(scratch.copy(bounds).stopped, None);
        let scratch = Scratch::new();
        stdfs::write(scratch.source().join("a"), "123456").unwrap();
        stdfs::write(scratch.source().join("b"), "12345").unwrap();
        let outcome = scratch.copy(bounds);
        assert_eq!(outcome.stopped, Some(Stop::Bytes));
        assert_eq!(outcome.copied, 1);
        let scratch = Scratch::new();
        stdfs::write(scratch.source().join("big"), "12345678901").unwrap();
        assert_eq!(scratch.copy(bounds).stopped, Some(Stop::Bytes));
        assert!(scratch.listing().is_empty());
    }

    #[test]
    fn stops_at_the_depth_bound() {
        let bounds = Bounds { depth: 3, ..BOUNDS };
        let scratch = Scratch::new();
        stdfs::create_dir_all(scratch.source().join("a/b/c")).unwrap();
        assert_eq!(scratch.copy(bounds).stopped, None);
        assert_eq!(scratch.listing(), ["a", "a/b", "a/b/c"]);
        let scratch = Scratch::new();
        stdfs::create_dir_all(scratch.source().join("a/b/c/d")).unwrap();
        let outcome = scratch.copy(bounds);
        assert_eq!(outcome.stopped, Some(Stop::Depth));
        assert_eq!(scratch.listing(), ["a", "a/b", "a/b/c"]);
        // An empty directory at the bound is copied; a file below it not.
        let scratch = Scratch::new();
        stdfs::create_dir_all(scratch.source().join("a/b/c")).unwrap();
        stdfs::write(scratch.source().join("a/b/c/f"), "").unwrap();
        assert_eq!(scratch.copy(bounds).stopped, Some(Stop::Depth));
    }

    #[test]
    fn never_follows_a_link_out() {
        let scratch = Scratch::new();
        let outside = scratch.dir.join("outside");
        stdfs::create_dir(&outside).unwrap();
        stdfs::write(outside.join("secret"), "s").unwrap();
        symlink(&outside, scratch.source().join("dir-link")).unwrap();
        symlink(outside.join("secret"), scratch.source().join("file-link")).unwrap();
        // A loop of links is two links.
        symlink("loop-b", scratch.source().join("loop-a")).unwrap();
        symlink("loop-a", scratch.source().join("loop-b")).unwrap();
        let outcome = scratch.copy(BOUNDS);
        assert_eq!(outcome.stopped, None);
        assert_eq!(outcome.copied, 4);
        for name in ["dir-link", "file-link", "loop-a", "loop-b"] {
            assert!(
                stdfs::symlink_metadata(scratch.target().join(name))
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "{name}"
            );
        }
        assert_eq!(scratch.listing().len(), 4);
    }

    #[test]
    fn a_symlinked_run_is_not_opened() {
        let scratch = Scratch::new();
        symlink("source", scratch.dir.join("run")).unwrap();
        let parent = stdfs::File::open(&scratch.dir).unwrap();
        assert!(Dir::open_at(parent.as_fd(), c"run").is_err());
    }
}
