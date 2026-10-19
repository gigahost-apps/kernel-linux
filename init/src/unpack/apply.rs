//! Applies tar entries to the root disk with the OCI layer rules
//! (whiteouts, opaque directories, replacements).
//!
//! Every path is resolved inside the root: an entry's parent is opened
//! with `openat2` and `RESOLVE_IN_ROOT` relative to the root's descriptor
//! (so `..` and absolute symbolic links stay inside, as they do for the
//! container later), and the entry itself is made with an `*at` call on
//! that parent and one plain name, never following a symbolic link.
//!
//! In a redeploy (interface version 4) nothing at or inside a kept path
//! changes: an entry there is skipped and counted, whatever its kind, and
//! whatever route its path takes ([`super::keep`]).

use super::Failure;
use super::keep::{Keep, Place};
use super::tar::{Data, Entry, Kind};
use crate::sys::fs::{self, Stat};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::ffi::{CStr, CString};
use std::io::{self, Read};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

/// The longest path an entry may name (Linux's `PATH_MAX`).
const MAX_PATH: usize = 4096;

/// How deep a removed or emptied tree may be: one directory descriptor is
/// open per level, and PID 1's soft limit is 1024 descriptors.
pub const MAX_DEPTH: usize = 512;

/// The most paths one layer may name: its entries' and their parent
/// directories', a path through a symbolic link counted as a path of its
/// own. Half as many again as [`super::MAX_ENTRIES`], for the parents a
/// layer leaves implicit; it bounds [`Tree::seen`] to a table of 2^21
/// slots of 17 bytes, about 36 MB, and 54 MB while it grows into it.
const MAX_PATHS: usize = 3 << 19;

/// How many bytes a layer spends on the directories whose times it sets
/// at its end (each path and [`DEFERRED_OVERHEAD`]); directories past it
/// get their times at once.
const DEFERRED_BYTES: usize = 16 << 20;

/// What one deferred directory costs besides its path.
const DEFERRED_OVERHEAD: usize = 64;

const WHITEOUT: &[u8] = b".wh.";
const OPAQUE: &[u8] = b".wh..wh..opq";

/// The root disk being unpacked onto, and the current layer's state.
pub struct Tree {
    root: OwnedFd,
    /// The paths this layer has made, as 128-bit hashes of their cleaned
    /// paths: what a whiteout or an opaque directory of the same layer
    /// leaves alone. A hash keeps the memory to 16 bytes a path, and
    /// [`MAX_PATHS`] the number of paths.
    seen: HashSet<[u8; 16]>,
    max_paths: usize,
    /// Directories whose times are set at the layer's end, once their
    /// contents are in.
    deferred: Vec<(Vec<u8>, (i64, u32))>,
    deferred_bytes: usize,
    /// Extended attributes the file system refused (not supported, or not
    /// allowed on that kind of file), as container runtimes skip them.
    pub dropped_xattrs: u64,
    /// A redeploy's kept paths, when any was found.
    keep: Option<Keep>,
    /// The entries skipped as at or inside a kept path, since the caller
    /// last set it to 0.
    pub skipped: u64,
}

fn step(what: &'static str) -> impl Fn(io::Error) -> Failure {
    move |error| Failure::Apply(what, error)
}

fn is(error: &io::Error, code: libc::c_int) -> bool {
    error.raw_os_error() == Some(code)
}

pub fn c_name(bytes: &[u8]) -> Result<CString, Failure> {
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
pub fn split(path: &[u8]) -> (&[u8], &[u8]) {
    match path.iter().rposition(|&byte| byte == b'/') {
        Some(slash) => (
            path.get(..slash).unwrap_or_default(),
            path.get(slash + 1..).unwrap_or_default(),
        ),
        None => (&[], path),
    }
}

pub fn join(parent: &[u8], name: &[u8]) -> Vec<u8> {
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

pub fn stat(dir: BorrowedFd<'_>, name: &CStr) -> Result<Option<Stat>, Failure> {
    match fs::stat_at(dir, name) {
        Ok(stat) => Ok(Some(stat)),
        Err(error) if is(&error, libc::ENOENT) => Ok(None),
        Err(error) => Err(Failure::Apply("examining a path", error)),
    }
}

/// Removes `name` from `dir`, a directory with everything in it.
fn remove(dir: BorrowedFd<'_>, name: &CStr) -> Result<(), Failure> {
    remove_below(dir, name, 0)
}

/// As [`remove`], with `dir` itself `depth` directories deep in a tree
/// being removed or emptied.
pub fn remove_below(dir: BorrowedFd<'_>, name: &CStr, depth: usize) -> Result<(), Failure> {
    match stat(dir, name)? {
        None => Ok(()),
        Some(found) if found.is_dir() => {
            remove_contents(dir, name, depth)?;
            fs::unlink_at(dir, name, true).map_err(step("removing a directory"))
        }
        Some(_) => fs::unlink_at(dir, name, false).map_err(step("removing a file")),
    }
}

/// Empties the directory `name` in `dir`. Each name is removed as it is
/// read, so the memory does not grow with the directory; as POSIX leaves
/// open whether a stream still returns every name once others are
/// removed, passes from the start repeat until one finds nothing.
fn remove_contents(dir: BorrowedFd<'_>, name: &CStr, depth: usize) -> Result<(), Failure> {
    if depth > MAX_DEPTH {
        return Err(Failure::Archive("a directory tree is too deep"));
    }
    let mut inner = fs::Dir::open_at(dir, name).map_err(step("opening a directory"))?;
    loop {
        let mut removed = false;
        while let Some(child) = inner.next_name().map_err(step("reading a directory"))? {
            match stat(inner.fd(), &child)? {
                None => continue,
                Some(found) if found.is_dir() => {
                    remove_contents(inner.fd(), &child, depth + 1)?;
                    fs::unlink_at(inner.fd(), &child, true)
                        .map_err(step("removing a directory"))?;
                }
                Some(_) => {
                    fs::unlink_at(inner.fd(), &child, false).map_err(step("removing a file"))?;
                }
            }
            removed = true;
        }
        if !removed {
            return Ok(());
        }
        inner.rewind();
    }
}

impl Tree {
    /// A tree over the open root directory.
    pub fn new(root: OwnedFd) -> Self {
        Tree {
            root,
            seen: HashSet::new(),
            max_paths: MAX_PATHS,
            deferred: Vec::new(),
            deferred_bytes: 0,
            dropped_xattrs: 0,
            keep: None,
            skipped: 0,
        }
    }

    /// A redeploy's tree: nothing at or inside a path of `keep` changes.
    pub fn with_keep(root: OwnedFd, keep: Keep) -> Self {
        Tree {
            keep: (!keep.is_empty()).then_some(keep),
            ..Tree::new(root)
        }
    }

    /// Refuses, as [`Failure::Kept`], a change at the cleaned `path` that
    /// an entry names when a kept path is it or holds it.
    fn check_named(&self, path: &[u8]) -> Result<(), Failure> {
        match &self.keep {
            Some(keep) if keep.named(path) == Place::Kept => Err(Failure::Kept),
            _ => Ok(()),
        }
    }

    /// Where `name` in `dir` is on the disk against the kept paths.
    fn real_place(&self, dir: BorrowedFd<'_>, name: &[u8]) -> Result<Place, Failure> {
        match &self.keep {
            Some(keep) => keep.real_place(dir, name),
            None => Ok(Place::Free),
        }
    }

    /// Where the open directory `dir` is on the disk against the kept
    /// paths.
    fn dir_place(&self, dir: BorrowedFd<'_>) -> Result<Place, Failure> {
        match &self.keep {
            Some(keep) => keep.dir_place(dir),
            None => Ok(Place::Free),
        }
    }

    /// A tree that refuses a layer naming more than `max_paths` paths.
    #[cfg(test)]
    pub fn with_max_paths(root: OwnedFd, max_paths: usize) -> Self {
        Tree {
            max_paths,
            ..Tree::new(root)
        }
    }

    /// Starts a layer: nothing of it is seen yet, and a larger layer's
    /// table before it is freed.
    pub fn begin_layer(&mut self) {
        self.seen = HashSet::new();
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
                // A later entry may have put a symbolic link on its path.
                if self.dir_place(dir.as_fd())? == Place::Kept {
                    continue;
                }
                fs::set_times_fd(dir.as_fd(), *seconds, *nanoseconds)
                    .map_err(step("setting a directory's times"))?;
            }
        }
        self.deferred_bytes = 0;
        Ok(())
    }

    /// Marks `path` as this layer's, and its ancestors as holding this
    /// layer's entries (so an opaque directory keeps and descends into
    /// them, and a whiteout empties rather than removes them). Past
    /// [`MAX_PATHS`] the layer is refused: paths through symbolic links
    /// name the same directories without making any, so the entries
    /// alone do not bound them.
    fn mark(&mut self, path: &[u8]) -> Result<(), Failure> {
        let mut rest = path;
        loop {
            let key = key(rest);
            if self.seen.contains(&key) {
                return Ok(());
            }
            if self.seen.len() >= self.max_paths {
                return Err(Failure::Archive("the layer names more than 1572864 paths"));
            }
            self.seen.insert(key);
            match rest.iter().rposition(|&byte| byte == b'/') {
                Some(slash) => rest = rest.get(..slash).unwrap_or_default(),
                None => return Ok(()),
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
            Ok(dir) => {
                if self.dir_place(dir.as_fd())? == Place::Kept {
                    return Err(Failure::Kept);
                }
                return Ok(Some(dir));
            }
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
                    self.check_named(&prefix)?;
                    if self.real_place(current.as_fd(), part)? == Place::Kept {
                        return Err(Failure::Kept);
                    }
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
                    self.mark(&prefix)?;
                    made
                }
                Err(error) if is(&error, libc::ENOTDIR) => {
                    return Err(Failure::Archive("a path's parent is not a directory"));
                }
                Err(error) => return Err(Failure::Apply("resolving a path", error)),
            };
        }
        if self.dir_place(current.as_fd())? == Place::Kept {
            return Err(Failure::Kept);
        }
        Ok(Some(current))
    }

    /// Applies one entry; its file data is read from `data`. An entry at
    /// or inside a kept path is skipped and counted in [`Tree::skipped`].
    pub fn apply<R: Read>(&mut self, entry: &Entry, data: &mut Data<'_, R>) -> Result<(), Failure> {
        match self.apply_entry(entry, data) {
            Err(Failure::Kept) => {
                self.skipped += 1;
                Ok(())
            }
            other => other,
        }
    }

    fn apply_entry<R: Read>(
        &mut self,
        entry: &Entry,
        data: &mut Data<'_, R>,
    ) -> Result<(), Failure> {
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
        self.check_named(&path)?;
        let Some(dir) = self.open_dir(parent, true)? else {
            return Err(Failure::Archive("a path's parent is not a directory"));
        };
        // A directory a kept path is in stays one: only a directory entry
        // (its owner, mode, attributes and times) applies to it.
        match self.real_place(dir.as_fd(), name)? {
            Place::Kept => return Err(Failure::Kept),
            Place::Ancestor if entry.kind != Kind::Directory => return Err(Failure::Kept),
            _ => {}
        }
        let c = c_name(name)?;
        match &entry.kind {
            Kind::Directory => self.directory(&dir, &c, entry, &path)?,
            Kind::File => {
                remove(dir.as_fd(), &c)?;
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
                remove(dir.as_fd(), &c)?;
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
                remove(dir.as_fd(), &c)?;
                fs::mknod_at(dir.as_fd(), &c, kind | 0o600, libc::makedev(*major, *minor))
                    .map_err(step("creating a device node"))?;
                self.node_metadata(&dir, &c, entry, true)?;
            }
            Kind::Fifo => {
                remove(dir.as_fd(), &c)?;
                fs::mknod_at(dir.as_fd(), &c, libc::S_IFIFO | 0o600, 0)
                    .map_err(step("creating a FIFO"))?;
                self.node_metadata(&dir, &c, entry, true)?;
            }
        }
        self.mark(&path)
    }

    /// `.wh.<name>` removes `<name>` as the lower layers left it;
    /// `.wh..wh..opq` empties its directory of the lower layers' entries.
    /// Neither is created. Neither changes a kept path, and a directory a
    /// kept path is in is emptied of the rest, never removed.
    fn whiteout(&mut self, parent: &[u8], name: &[u8], target: &[u8]) -> Result<(), Failure> {
        if name == OPAQUE {
            self.check_named(parent)?;
            let Some(dir) = self.open_dir(parent, true)? else {
                return Err(Failure::Archive("a path's parent is not a directory"));
            };
            let parent = parent.to_vec();
            self.mark(&parent)?;
            let mut names =
                fs::Dir::open_at(dir.as_fd(), c".").map_err(step("opening a directory"))?;
            return self.opaque(&mut names, &parent, 0);
        }
        if target.starts_with(WHITEOUT) {
            // Other `.wh..wh.` names are aufs's own bookkeeping.
            return Ok(());
        }
        if target.is_empty() || target == b"." || target == b".." {
            return Err(Failure::Archive("a whiteout names nothing"));
        }
        let target_path = join(parent, target);
        self.check_named(&target_path)?;
        let Some(dir) = self.open_dir(parent, false)? else {
            return Ok(());
        };
        let c = c_name(target)?;
        let place = self.real_place(dir.as_fd(), target)?;
        if place == Place::Kept {
            return Err(Failure::Kept);
        }
        if place == Place::Free && !self.was_seen(&target_path) {
            return remove(dir.as_fd(), &c);
        }
        // This layer's own entry stays: a whiteout hides lower layers
        // only. A directory of this layer may hold the lower layers'
        // entries too (it merged into theirs): those go, as from an
        // opaque directory. So do those of a directory a kept path is in,
        // the kept path aside.
        if stat(dir.as_fd(), &c)?.is_some_and(|found| found.is_dir()) {
            let mut names =
                fs::Dir::open_at(dir.as_fd(), &c).map_err(step("opening a directory"))?;
            self.opaque(&mut names, &target_path, 0)?;
        }
        Ok(())
    }

    /// Removes from the directory `dir` (at `path`) whatever this layer
    /// has not made, keeping this layer's directories but emptying them of
    /// what lower layers left in them. Each name is dealt with as it is
    /// read, so the memory does not grow with the directory. Only names
    /// already read are removed, which on ext4 (whose stream positions are
    /// hashes or offsets) leaves the names still to come in the stream, as
    /// `rm -r` relies on too. A kept path stays as it is; a directory a
    /// kept path is in stays, emptied of the rest.
    fn opaque(&self, dir: &mut fs::Dir, path: &[u8], depth: usize) -> Result<(), Failure> {
        if depth > MAX_DEPTH {
            return Err(Failure::Archive("a directory tree is too deep"));
        }
        let real = match &self.keep {
            Some(keep) => Some(keep.relative(dir.fd())?),
            None => None,
        };
        while let Some(child) = dir.next_name().map_err(step("reading a directory"))? {
            let child_path = join(path, child.as_bytes());
            let place = match (&self.keep, &real) {
                (Some(keep), Some(real)) => keep.place_of(&join(real, child.as_bytes())),
                _ => Place::Free,
            };
            if place == Place::Kept {
                continue;
            }
            if place == Place::Free && !self.was_seen(&child_path) {
                remove(dir.fd(), &child)?;
                continue;
            }
            if stat(dir.fd(), &child)?.is_some_and(|found| found.is_dir()) {
                let mut inner =
                    fs::Dir::open_at(dir.fd(), &child).map_err(step("opening a directory"))?;
                self.opaque(&mut inner, &child_path, depth + 1)?;
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
        match stat(dir.as_fd(), name)? {
            Some(found) if found.is_dir() => {}
            found => {
                if found.is_some() {
                    remove(dir.as_fd(), name)?;
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
        let cost = path.len() + DEFERRED_OVERHEAD;
        if self.deferred_bytes + cost <= DEFERRED_BYTES {
            self.deferred_bytes += cost;
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
        // Not to a kept file: the image's own file there was skipped, and
        // a link would change the kept one's link count.
        self.check_named(&target)?;
        let (target_parent, target_name) = split(&target);
        let target_dir = self.open_dir(target_parent, false)?.ok_or_else(missing)?;
        if self.real_place(target_dir.as_fd(), target_name)? == Place::Kept {
            return Err(Failure::Kept);
        }
        let target_name = c_name(target_name)?;
        let found = stat(target_dir.as_fd(), &target_name)?.ok_or_else(missing)?;
        if found.is_dir() {
            return Err(Failure::Archive("a hard link names a directory"));
        }
        if let Some(existing) = stat(dir.as_fd(), name)? {
            if existing.dev == found.dev && existing.ino == found.ino {
                return Ok(());
            }
            remove(dir.as_fd(), name)?;
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
