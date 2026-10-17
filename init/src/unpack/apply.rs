//! Applies tar entries to the root disk with the OCI layer rules
//! (whiteouts, opaque directories, replacements).
//!
//! Every path is resolved inside the root: an entry's parent is opened
//! with `openat2` and `RESOLVE_IN_ROOT` relative to the root's descriptor
//! (so `..` and absolute symbolic links stay inside, as they do for the
//! container later), and the entry itself is made with an `*at` call on
//! that parent and one plain name, never following a symbolic link.

use super::Failure;
use super::tar::{Data, Entry, Kind};
use crate::sys::fs::{self, Stat};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::ffi::{CStr, CString};
use std::io::{self, Read};
use std::os::fd::{AsFd, OwnedFd};

/// The longest path an entry may name (Linux's `PATH_MAX`).
const MAX_PATH: usize = 4096;

/// How deep a removed tree may be; deeper than any path can reach.
const MAX_DEPTH: usize = MAX_PATH;

/// How many bytes of directory paths a layer keeps to set their times at
/// its end; directories past it get their times at once.
const DEFERRED_BYTES: usize = 16 << 20;

const WHITEOUT: &[u8] = b".wh.";
const OPAQUE: &[u8] = b".wh..wh..opq";

/// The root disk being unpacked onto, and the current layer's state.
pub struct Tree {
    root: OwnedFd,
    /// The paths this layer has made, as 128-bit hashes of their cleaned
    /// paths: what a whiteout or an opaque directory of the same layer
    /// leaves alone. A hash keeps the memory to 16 bytes a path.
    seen: HashSet<[u8; 16]>,
    /// Directories whose times are set at the layer's end, once their
    /// contents are in.
    deferred: Vec<(Vec<u8>, (i64, u32))>,
    deferred_bytes: usize,
    /// Extended attributes the file system refused (not supported, or not
    /// allowed on that kind of file), as container runtimes skip them.
    pub dropped_xattrs: u64,
}

fn step(what: &'static str) -> impl Fn(io::Error) -> Failure {
    move |error| Failure::Apply(what, error)
}

fn is(error: &io::Error, code: libc::c_int) -> bool {
    error.raw_os_error() == Some(code)
}

fn c_name(bytes: &[u8]) -> Result<CString, Failure> {
    CString::new(bytes).map_err(|_| Failure::Archive("a path holds a NUL byte"))
}

/// The path with `.` and empty components dropped and `..` taken
/// lexically (never above the root), its components joined by `/`.
pub fn clean(path: &[u8]) -> Result<Vec<u8>, Failure> {
    if path.contains(&0) {
        return Err(Failure::Archive("a path holds a NUL byte"));
    }
    let mut parts: Vec<&[u8]> = Vec::new();
    for part in path.split(|&byte| byte == b'/') {
        match part {
            b"" | b"." => {}
            b".." => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    let cleaned = parts.join(&b'/');
    if cleaned.len() >= MAX_PATH {
        return Err(Failure::Archive("a path is too long"));
    }
    Ok(cleaned)
}

/// The parent and the last name of a cleaned, non-empty path.
fn split(path: &[u8]) -> (&[u8], &[u8]) {
    match path.iter().rposition(|&byte| byte == b'/') {
        Some(slash) => (
            path.get(..slash).unwrap_or_default(),
            path.get(slash + 1..).unwrap_or_default(),
        ),
        None => (&[], path),
    }
}

fn join(parent: &[u8], name: &[u8]) -> Vec<u8> {
    if parent.is_empty() {
        name.to_vec()
    } else {
        [parent, b"/", name].concat()
    }
}

fn key(path: &[u8]) -> [u8; 16] {
    let digest = Sha256::digest(path);
    let mut key = [0u8; 16];
    key.copy_from_slice(digest.get(..16).unwrap_or(&[0; 16]));
    key
}

fn stat(dir: &OwnedFd, name: &CStr) -> Result<Option<Stat>, Failure> {
    match fs::stat_at(dir.as_fd(), name) {
        Ok(stat) => Ok(Some(stat)),
        Err(error) if is(&error, libc::ENOENT) => Ok(None),
        Err(error) => Err(Failure::Apply("examining a path", error)),
    }
}

/// Removes `name` from `dir`, a directory with everything in it.
fn remove(dir: &OwnedFd, name: &CStr) -> Result<(), Failure> {
    match stat(dir, name)? {
        None => Ok(()),
        Some(found) if found.is_dir() => {
            remove_contents(dir, name, 0)?;
            fs::unlink_at(dir.as_fd(), name, true).map_err(step("removing a directory"))
        }
        Some(_) => fs::unlink_at(dir.as_fd(), name, false).map_err(step("removing a file")),
    }
}

/// Empties the directory `name` in `dir`.
fn remove_contents(dir: &OwnedFd, name: &CStr, depth: usize) -> Result<(), Failure> {
    if depth > MAX_DEPTH {
        return Err(Failure::Archive("a directory tree is too deep"));
    }
    let inner = fs::open_at(
        dir.as_fd(),
        name,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        0,
    )
    .map_err(step("opening a directory"))?;
    for child in fs::read_names(inner.as_fd()).map_err(step("reading a directory"))? {
        match stat(&inner, &child)? {
            None => {}
            Some(found) if found.is_dir() => {
                remove_contents(&inner, &child, depth + 1)?;
                fs::unlink_at(inner.as_fd(), &child, true).map_err(step("removing a directory"))?;
            }
            Some(_) => {
                fs::unlink_at(inner.as_fd(), &child, false).map_err(step("removing a file"))?;
            }
        }
    }
    Ok(())
}

impl Tree {
    /// A tree over the open root directory.
    pub fn new(root: OwnedFd) -> Self {
        Tree {
            root,
            seen: HashSet::new(),
            deferred: Vec::new(),
            deferred_bytes: 0,
            dropped_xattrs: 0,
        }
    }

    /// Starts a layer: nothing of it is seen yet.
    pub fn begin_layer(&mut self) {
        self.seen.clear();
        self.deferred.clear();
        self.deferred_bytes = 0;
    }

    /// Ends a layer: the directories' times, now that nothing more goes
    /// into them. A directory a later entry removed or replaced is
    /// skipped.
    pub fn end_layer(&mut self) -> Result<(), Failure> {
        let deferred = std::mem::take(&mut self.deferred);
        for (path, (seconds, nanoseconds)) in deferred.iter().rev() {
            let opened = if path.is_empty() {
                self.root.try_clone()
            } else {
                fs::open_inside(
                    self.root.as_fd(),
                    &c_name(path)?,
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                )
            };
            if let Ok(dir) = opened {
                fs::set_times_fd(dir.as_fd(), *seconds, *nanoseconds)
                    .map_err(step("setting a directory's times"))?;
            }
        }
        self.deferred_bytes = 0;
        Ok(())
    }

    /// Marks `path` as this layer's, and its ancestors as holding this
    /// layer's entries (so an opaque directory keeps and descends into
    /// them, and a whiteout leaves them).
    fn mark(&mut self, path: &[u8]) {
        if !self.seen.insert(key(path)) {
            return;
        }
        let mut rest = path;
        while let Some(slash) = rest.iter().rposition(|&byte| byte == b'/') {
            rest = rest.get(..slash).unwrap_or_default();
            if !self.seen.insert(key(rest)) {
                return;
            }
        }
    }

    fn was_seen(&self, path: &[u8]) -> bool {
        self.seen.contains(&key(path))
    }

    /// Opens the directory `parent` (a cleaned path, empty for the root)
    /// inside the root. Missing directories are created (mode 0755, as
    /// container runtimes create them) when `create` is set, else `None`
    /// is returned for them.
    fn open_dir(&mut self, parent: &[u8], create: bool) -> Result<Option<OwnedFd>, Failure> {
        if parent.is_empty() {
            return self
                .root
                .try_clone()
                .map(Some)
                .map_err(step("opening the root"));
        }
        let flags = libc::O_RDONLY | libc::O_DIRECTORY;
        match fs::open_inside(self.root.as_fd(), &c_name(parent)?, flags) {
            Ok(dir) => return Ok(Some(dir)),
            Err(error) if is(&error, libc::ENOENT) && create => {}
            Err(error) if is(&error, libc::ENOENT) || is(&error, libc::ENOTDIR) => {
                return if create {
                    Err(Failure::Archive("a path's parent is not a directory"))
                } else {
                    Ok(None)
                };
            }
            Err(error) => return Err(Failure::Apply("resolving a path", error)),
        }
        // Component by component, each resolved from the root.
        let mut current = self.root.try_clone().map_err(step("opening the root"))?;
        let mut prefix: Vec<u8> = Vec::new();
        for part in parent.split(|&byte| byte == b'/') {
            prefix = join(&prefix, part);
            let path = c_name(&prefix)?;
            current = match fs::open_inside(self.root.as_fd(), &path, flags) {
                Ok(dir) => dir,
                Err(error) if is(&error, libc::ENOENT) => {
                    let name = c_name(part)?;
                    match fs::mkdir_at(current.as_fd(), &name, 0o755) {
                        Ok(()) => {}
                        Err(error) if is(&error, libc::EEXIST) => {
                            return Err(Failure::Archive("a path's parent is not a directory"));
                        }
                        Err(error) => return Err(Failure::Apply("creating a directory", error)),
                    }
                    let made = fs::open_at(current.as_fd(), &name, flags | libc::O_NOFOLLOW, 0)
                        .map_err(step("opening a directory"))?;
                    fs::chmod_fd(made.as_fd(), 0o755).map_err(step("setting a mode"))?;
                    self.mark(&prefix);
                    made
                }
                Err(error) if is(&error, libc::ENOTDIR) => {
                    return Err(Failure::Archive("a path's parent is not a directory"));
                }
                Err(error) => return Err(Failure::Apply("resolving a path", error)),
            };
        }
        Ok(Some(current))
    }

    /// Applies one entry; its file data is read from `data`.
    pub fn apply<R: Read>(&mut self, entry: &Entry, data: &mut Data<'_, R>) -> Result<(), Failure> {
        let path = clean(&entry.path)?;
        if path.is_empty() {
            return if entry.kind == Kind::Directory {
                let root = self.root.try_clone().map_err(step("opening the root"))?;
                self.directory_metadata(&root, entry, &path)
            } else {
                Err(Failure::Archive(
                    "an entry names the root but is not a directory",
                ))
            };
        }
        let (parent, name) = split(&path);
        if let Some(target) = name.strip_prefix(WHITEOUT) {
            return self.whiteout(parent, name, target);
        }
        let Some(dir) = self.open_dir(parent, true)? else {
            return Err(Failure::Archive("a path's parent is not a directory"));
        };
        let c = c_name(name)?;
        match &entry.kind {
            Kind::Directory => self.directory(&dir, &c, entry, &path)?,
            Kind::File => {
                remove(&dir, &c)?;
                let file = fs::open_at(
                    dir.as_fd(),
                    &c,
                    libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
                    0o600,
                )
                .map_err(step("creating a file"))?;
                copy(data, &file)?;
                fs::chown_at(file.as_fd(), c"", owner(entry.uid)?, owner(entry.gid)?)
                    .map_err(step("setting an owner"))?;
                fs::chmod_fd(file.as_fd(), entry.mode).map_err(step("setting a mode"))?;
                self.xattrs(entry, |name, value| {
                    fs::set_xattr_fd(file.as_fd(), name, value)
                })?;
                fs::set_times_fd(file.as_fd(), entry.mtime.0, entry.mtime.1)
                    .map_err(step("setting a file's times"))?;
            }
            Kind::Symlink(target) => {
                remove(&dir, &c)?;
                let target = CString::new(target.as_slice())
                    .map_err(|_| Failure::Archive("a link target holds a NUL byte"))?;
                fs::symlink_at(&target, dir.as_fd(), &c)
                    .map_err(step("creating a symbolic link"))?;
                self.node_metadata(&dir, &c, entry, false)?;
            }
            Kind::HardLink(target) => self.hard_link(&dir, &c, target)?,
            Kind::CharDevice(major, minor) | Kind::BlockDevice(major, minor) => {
                let kind = if matches!(entry.kind, Kind::CharDevice(..)) {
                    libc::S_IFCHR
                } else {
                    libc::S_IFBLK
                };
                remove(&dir, &c)?;
                fs::mknod_at(dir.as_fd(), &c, kind | 0o600, libc::makedev(*major, *minor))
                    .map_err(step("creating a device node"))?;
                self.node_metadata(&dir, &c, entry, true)?;
            }
            Kind::Fifo => {
                remove(&dir, &c)?;
                fs::mknod_at(dir.as_fd(), &c, libc::S_IFIFO | 0o600, 0)
                    .map_err(step("creating a FIFO"))?;
                self.node_metadata(&dir, &c, entry, true)?;
            }
        }
        self.mark(&path);
        Ok(())
    }

    /// `.wh.<name>` removes `<name>` as the lower layers left it;
    /// `.wh..wh..opq` empties its directory of the lower layers' entries.
    /// Neither is created.
    fn whiteout(&mut self, parent: &[u8], name: &[u8], target: &[u8]) -> Result<(), Failure> {
        if name == OPAQUE {
            let Some(dir) = self.open_dir(parent, true)? else {
                return Err(Failure::Archive("a path's parent is not a directory"));
            };
            let parent = parent.to_vec();
            self.mark(&parent);
            return self.opaque(&dir, &parent, 0);
        }
        if target.starts_with(WHITEOUT) {
            // Other `.wh..wh.` names are aufs's own bookkeeping.
            return Ok(());
        }
        if target.is_empty() || target == b"." || target == b".." {
            return Err(Failure::Archive("a whiteout names nothing"));
        }
        let target_path = join(parent, target);
        if self.was_seen(&target_path) {
            // This layer's own entry: a whiteout hides lower layers only.
            return Ok(());
        }
        match self.open_dir(parent, false)? {
            Some(dir) => remove(&dir, &c_name(target)?),
            None => Ok(()),
        }
    }

    /// Removes from `dir` (at `path`) whatever this layer has not made,
    /// keeping this layer's directories but emptying them of what lower
    /// layers left in them.
    fn opaque(&mut self, dir: &OwnedFd, path: &[u8], depth: usize) -> Result<(), Failure> {
        if depth > MAX_DEPTH {
            return Err(Failure::Archive("a directory tree is too deep"));
        }
        for child in fs::read_names(dir.as_fd()).map_err(step("reading a directory"))? {
            let child_path = join(path, child.as_bytes());
            if !self.was_seen(&child_path) {
                remove(dir, &child)?;
                continue;
            }
            if stat(dir, &child)?.is_some_and(|found| found.is_dir()) {
                let inner = fs::open_at(
                    dir.as_fd(),
                    &child,
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                    0,
                )
                .map_err(step("opening a directory"))?;
                self.opaque(&inner, &child_path, depth + 1)?;
            }
        }
        Ok(())
    }

    /// A directory: merged into one already there, else replacing
    /// whatever is there.
    fn directory(
        &mut self,
        dir: &OwnedFd,
        name: &CStr,
        entry: &Entry,
        path: &[u8],
    ) -> Result<(), Failure> {
        match stat(dir, name)? {
            Some(found) if found.is_dir() => {}
            found => {
                if found.is_some() {
                    remove(dir, name)?;
                }
                fs::mkdir_at(dir.as_fd(), name, 0o700).map_err(step("creating a directory"))?;
            }
        }
        let opened = fs::open_at(
            dir.as_fd(),
            name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            0,
        )
        .map_err(step("opening a directory"))?;
        self.directory_metadata(&opened, entry, path)
    }

    /// Owner, mode and extended attributes now; times at the layer's end.
    fn directory_metadata(
        &mut self,
        opened: &OwnedFd,
        entry: &Entry,
        path: &[u8],
    ) -> Result<(), Failure> {
        fs::chown_at(opened.as_fd(), c"", owner(entry.uid)?, owner(entry.gid)?)
            .map_err(step("setting an owner"))?;
        fs::chmod_fd(opened.as_fd(), entry.mode).map_err(step("setting a mode"))?;
        self.xattrs(entry, |name, value| {
            fs::set_xattr_fd(opened.as_fd(), name, value)
        })?;
        if self.deferred_bytes + path.len() <= DEFERRED_BYTES {
            self.deferred_bytes += path.len();
            self.deferred.push((path.to_vec(), entry.mtime));
        } else {
            fs::set_times_fd(opened.as_fd(), entry.mtime.0, entry.mtime.1)
                .map_err(step("setting a directory's times"))?;
        }
        Ok(())
    }

    /// Owner, mode (unless a symbolic link), extended attributes and
    /// times of a node that cannot be opened.
    fn node_metadata(
        &mut self,
        dir: &OwnedFd,
        name: &CStr,
        entry: &Entry,
        mode: bool,
    ) -> Result<(), Failure> {
        fs::chown_at(dir.as_fd(), name, owner(entry.uid)?, owner(entry.gid)?)
            .map_err(step("setting an owner"))?;
        if mode {
            fs::chmod_at(dir.as_fd(), name, entry.mode).map_err(step("setting a mode"))?;
        }
        self.xattrs(entry, |xattr, value| {
            fs::set_xattr_at(dir.as_fd(), name, xattr, value)
        })?;
        fs::set_times_at(dir.as_fd(), name, entry.mtime.0, entry.mtime.1)
            .map_err(step("setting a file's times"))
    }

    /// A hard link to a path already in the result (lower layers'
    /// included), resolved inside the root like every other path.
    fn hard_link(&mut self, dir: &OwnedFd, name: &CStr, target: &[u8]) -> Result<(), Failure> {
        let missing = || Failure::Archive("a hard link names a path that does not exist");
        let target = clean(target)?;
        if target.is_empty() {
            return Err(Failure::Archive("a hard link names the root"));
        }
        let (target_parent, target_name) = split(&target);
        let target_dir = self.open_dir(target_parent, false)?.ok_or_else(missing)?;
        let target_name = c_name(target_name)?;
        let found = stat(&target_dir, &target_name)?.ok_or_else(missing)?;
        if found.is_dir() {
            return Err(Failure::Archive("a hard link names a directory"));
        }
        if let Some(existing) = stat(dir, name)? {
            if existing.dev == found.dev && existing.ino == found.ino {
                return Ok(());
            }
            remove(dir, name)?;
        }
        fs::link_at(target_dir.as_fd(), &target_name, dir.as_fd(), name)
            .map_err(step("creating a hard link"))
    }

    /// Sets the entry's extended attributes with `set`, skipping those
    /// the file system refuses as not supported or not allowed there.
    fn xattrs(
        &mut self,
        entry: &Entry,
        set: impl Fn(&CStr, &[u8]) -> io::Result<()>,
    ) -> Result<(), Failure> {
        for (name, value) in &entry.xattrs {
            let name = CString::new(name.as_slice())
                .map_err(|_| Failure::Archive("an extended attribute's name holds a NUL byte"))?;
            match set(&name, value) {
                Ok(()) => {}
                Err(error)
                    if is(&error, libc::EOPNOTSUPP)
                        || is(&error, libc::EPERM)
                        || is(&error, libc::ENOSYS) =>
                {
                    self.dropped_xattrs += 1;
                }
                Err(error) => return Err(Failure::Apply("setting an extended attribute", error)),
            }
        }
        Ok(())
    }
}

/// A numeric owner as the kernel takes it.
fn owner(id: u64) -> Result<u32, Failure> {
    u32::try_from(id)
        .ok()
        .filter(|&id| id != u32::MAX)
        .ok_or(Failure::Archive("an owner ID is out of range"))
}

/// Copies an entry's data into the new file, in bounded pieces.
fn copy<R: Read>(data: &mut Data<'_, R>, file: &OwnedFd) -> Result<(), Failure> {
    let mut file = std::fs::File::from(file.try_clone().map_err(step("writing a file"))?);
    let mut buffer = vec![0u8; 1 << 16];
    loop {
        let count = data.read(&mut buffer)?;
        if count == 0 {
            return Ok(());
        }
        std::io::Write::write_all(&mut file, buffer.get(..count).unwrap_or_default())
            .map_err(step("writing a file"))?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleaning() {
        let cleaned = |path: &str| String::from_utf8(clean(path.as_bytes()).unwrap()).unwrap();
        assert_eq!(cleaned("./a/b/"), "a/b");
        assert_eq!(cleaned("/a//b/./c"), "a/b/c");
        assert_eq!(cleaned("../../etc/passwd"), "etc/passwd");
        assert_eq!(cleaned("a/../../b"), "b");
        assert_eq!(cleaned("./"), "");
        assert_eq!(cleaned(".."), "");
        assert!(clean(b"a\0b").is_err());
        assert!(clean("a/".repeat(2049).as_bytes()).is_err());
    }

    #[test]
    fn splitting() {
        assert_eq!(split(b"a/b/c"), (&b"a/b"[..], &b"c"[..]));
        assert_eq!(split(b"c"), (&b""[..], &b"c"[..]));
    }
}
