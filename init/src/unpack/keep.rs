//! Interface version 4's redeploy (docs/interface.md): the paths of the
//! root disk the unpack keeps, where they are on the disk, and the removal
//! of everything else before the first layer.
//!
//! A kept path is found as every path is (`openat2` with
//! `RESOLVE_IN_ROOT`), its last component never followed: a kept symbolic
//! link is the link itself. What it resolves to is its real path, the
//! directories' names from the root. Every check of a change against the
//! kept paths uses the real path of the directory the change is made in,
//! read back from `/proc/self/fd` (the kernel's own name for it), so no
//! route through symbolic links, a layer's or the disk's, reaches into a
//! kept tree unseen.

use super::Failure;
use super::apply::{MAX_DEPTH, c_name, join, remove_below, split, stat};
use crate::sys::fs::{self, Dir};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStringExt;

/// Where a path is against the kept paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    /// A kept path or inside one: nothing there changes.
    Kept,
    /// A directory a kept path is inside: it stays a directory, and only
    /// its owner, mode, attributes and times may change.
    Ancestor,
    /// Neither.
    Free,
}

/// Whether `path` is strictly inside `base` (both cleaned, `""` the
/// root).
fn below(path: &[u8], base: &[u8]) -> bool {
    if base.is_empty() {
        return !path.is_empty();
    }
    path.len() > base.len() && path.starts_with(base) && path.get(base.len()) == Some(&b'/')
}

/// Where the cleaned `path` is against the cleaned paths `kept`.
pub fn place(kept: &[Vec<u8>], path: &[u8]) -> Place {
    let mut found = Place::Free;
    for base in kept {
        if path == base.as_slice() || below(path, base) {
            return Place::Kept;
        }
        if below(base, path) {
            found = Place::Ancestor;
        }
    }
    found
}

/// The name the kernel gives an open descriptor (`/proc/self/fd/N`).
fn fd_path(fd: BorrowedFd<'_>) -> io::Result<Vec<u8>> {
    std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd()))
        .map(|path| path.into_os_string().into_vec())
}

/// The kept paths of a redeploy, those found on the disk.
#[derive(Debug)]
pub struct Keep {
    /// As the config names them (without the leading `/`).
    named: Vec<Vec<u8>>,
    /// Where they are on the disk: their real paths from the root.
    real: Vec<Vec<u8>>,
    /// The root's own name, as [`fd_path`] gives it.
    root: Vec<u8>,
}

impl Keep {
    /// Finds `paths` (the config's, without their leading `/`) inside
    /// `root`; the indexes of those that do not exist come back with it,
    /// and are not kept.
    pub fn resolve(root: BorrowedFd<'_>, paths: &[Vec<u8>]) -> Result<(Keep, Vec<usize>), Failure> {
        let mut keep = Keep {
            named: Vec::new(),
            real: Vec::new(),
            root: fd_path(root).map_err(|error| Failure::Apply("resolving the root", error))?,
        };
        let mut missing = Vec::new();
        for (index, path) in paths.iter().enumerate() {
            let (parent, name) = split(path);
            let dir: OwnedFd = if parent.is_empty() {
                root.try_clone_to_owned()
                    .map_err(|error| Failure::Apply("opening the root", error))?
            } else {
                match fs::open_inside(root, &c_name(parent)?, libc::O_RDONLY | libc::O_DIRECTORY) {
                    Ok(dir) => dir,
                    Err(error)
                        if matches!(
                            error.raw_os_error(),
                            Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP)
                        ) =>
                    {
                        missing.push(index);
                        continue;
                    }
                    Err(error) => return Err(Failure::Apply("resolving a kept path", error)),
                }
            };
            let borrowed = dir.as_fd();
            if stat(borrowed, &c_name(name)?)?.is_none() {
                missing.push(index);
                continue;
            }
            let real = join(&keep.relative(borrowed)?, name);
            keep.named.push(path.clone());
            keep.real.push(real);
        }
        Ok((keep, missing))
    }

    /// No kept path was found.
    pub fn is_empty(&self) -> bool {
        self.real.is_empty()
    }

    /// Where the cleaned path `path`, as an entry names it, is against
    /// the kept paths as the config names them.
    pub fn named(&self, path: &[u8]) -> Place {
        place(&self.named, path)
    }

    /// Where the real path `real` is.
    pub fn place_of(&self, real: &[u8]) -> Place {
        place(&self.real, real)
    }

    /// Where the open directory `dir` is on the disk.
    pub fn dir_place(&self, dir: BorrowedFd<'_>) -> Result<Place, Failure> {
        Ok(place(&self.real, &self.relative(dir)?))
    }

    /// Where `name` in the open directory `dir` is on the disk.
    pub fn real_place(&self, dir: BorrowedFd<'_>, name: &[u8]) -> Result<Place, Failure> {
        Ok(place(&self.real, &join(&self.relative(dir)?, name)))
    }

    /// The real path from the root of the open directory `dir`, `""` for
    /// the root; a directory outside the root is an error.
    pub fn relative(&self, dir: BorrowedFd<'_>) -> Result<Vec<u8>, Failure> {
        let outside = || {
            Failure::Apply(
                "resolving a path",
                io::Error::other("the path is outside the root"),
            )
        };
        let path = fd_path(dir).map_err(|error| Failure::Apply("resolving a path", error))?;
        if self.root == b"/" {
            return path
                .strip_prefix(b"/")
                .map(<[u8]>::to_vec)
                .ok_or_else(outside);
        }
        let rest = path
            .strip_prefix(self.root.as_slice())
            .ok_or_else(outside)?;
        if rest.is_empty() {
            return Ok(Vec::new());
        }
        rest.strip_prefix(b"/")
            .map(<[u8]>::to_vec)
            .ok_or_else(outside)
    }

    /// Removes everything on the root but the kept paths (whole trees),
    /// the directories they are in (whose other names go) and the root's
    /// `lost+found`.
    pub fn clear(&self, root: BorrowedFd<'_>) -> Result<(), Failure> {
        let mut names =
            Dir::open_at(root, c".").map_err(|error| Failure::Apply("opening the root", error))?;
        self.clear_dir(&mut names, b"", 0)
    }

    /// Empties `dir` (at the real path `path`) of what is not kept, a name
    /// at a time as [`super::apply`] empties a directory; passes repeat
    /// until one removes nothing.
    fn clear_dir(&self, dir: &mut Dir, path: &[u8], depth: usize) -> Result<(), Failure> {
        if depth > MAX_DEPTH {
            return Err(Failure::Archive("a directory tree is too deep"));
        }
        loop {
            let mut removed = false;
            while let Some(child) = dir
                .next_name()
                .map_err(|error| Failure::Apply("reading a directory", error))?
            {
                if path.is_empty() && child.as_bytes() == b"lost+found" {
                    continue;
                }
                let child_path = join(path, child.as_bytes());
                match place(&self.real, &child_path) {
                    Place::Kept => {}
                    Place::Ancestor => {
                        // Always a directory: the kept path was found
                        // through it.
                        if stat(dir.fd(), &child)?.is_some_and(|found| found.is_dir()) {
                            let mut inner = Dir::open_at(dir.fd(), &child)
                                .map_err(|error| Failure::Apply("opening a directory", error))?;
                            self.clear_dir(&mut inner, &child_path, depth + 1)?;
                        }
                    }
                    Place::Free => {
                        remove_below(dir.fd(), &child, depth)?;
                        removed = true;
                    }
                }
            }
            if !removed {
                return Ok(());
            }
            dir.rewind();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(list: &[&str]) -> Vec<Vec<u8>> {
        list.iter().map(|path| path.as_bytes().to_vec()).collect()
    }

    #[test]
    fn places() {
        let kept = paths(&["data", "srv/www/html", "srv/www/html/inner"]);
        let at = |path: &str| place(&kept, path.as_bytes());
        assert_eq!(at("data"), Place::Kept);
        assert_eq!(at("data/db/file"), Place::Kept);
        assert_eq!(at("database"), Place::Free);
        assert_eq!(at("dat"), Place::Free);
        assert_eq!(at(""), Place::Ancestor);
        assert_eq!(at("srv"), Place::Ancestor);
        assert_eq!(at("srv/www"), Place::Ancestor);
        assert_eq!(at("srv/www/html"), Place::Kept);
        assert_eq!(at("srv/www/html/inner/x"), Place::Kept);
        assert_eq!(at("srv/www/htm"), Place::Free);
        assert_eq!(at("srv/other"), Place::Free);
        assert_eq!(place(&[], b""), Place::Free);
        assert_eq!(place(&[], b"a"), Place::Free);
    }
}
