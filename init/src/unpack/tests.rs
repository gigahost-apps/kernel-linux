//! The unpack mode against in-memory tars, applied to temporary
//! directories through the same code the guest runs (openat2 and the
//! `*at` calls; Linux 5.6 and later).

use super::apply::Tree;
use super::unpack_layer;
use crate::config::{Compression, Layer};
use sha2::{Digest, Sha256};
use std::ffi::CString;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// A temporary directory of its own, removed when dropped. The root
/// being unpacked onto is `root/` inside it, so anything that escaped
/// would show next to it.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "gigahost-init-unpack-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("root")).unwrap();
        Scratch { dir }
    }

    fn root(&self) -> PathBuf {
        self.dir.join("root")
    }

    fn tree(&self) -> Tree {
        Tree::new(fs::File::open(self.root()).unwrap().into())
    }

    /// The names next to the root: only `root` and the layers disk.
    fn outside(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&self.dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// The test's own user and group, which every entry is owned by.
fn ids() -> (u64, u64) {
    let mine = fs::metadata("/proc/self").unwrap();
    (u64::from(mine.uid()), u64::from(mine.gid()))
}

/// Writes ustar archives by hand, so hostile ones can be written too.
#[derive(Default)]
struct TarWriter {
    out: Vec<u8>,
}

fn octal(field: &mut [u8], value: u64) {
    let width = field.len() - 1;
    let text = format!("{value:0width$o}");
    field[..width].copy_from_slice(&text.as_bytes()[text.len() - width..]);
    field[width] = 0;
}

impl TarWriter {
    fn header(&mut self, name: &[u8], kind: u8, size: u64, mode: u32, link: &[u8], mtime: u64) {
        let (uid, gid) = ids();
        let mut block = [0u8; 512];
        let (prefix, name) = if name.len() > 100 {
            let slash = (0..name.len())
                .find(|&at| name[at] == b'/' && name.len() - at - 1 <= 100)
                .unwrap();
            (&name[..slash], &name[slash + 1..])
        } else {
            (&b""[..], name)
        };
        block[..name.len()].copy_from_slice(name);
        octal(&mut block[100..108], u64::from(mode));
        octal(&mut block[108..116], uid);
        octal(&mut block[116..124], gid);
        octal(&mut block[124..136], size);
        octal(&mut block[136..148], mtime);
        block[156] = kind;
        block[157..157 + link.len()].copy_from_slice(link);
        block[257..263].copy_from_slice(b"ustar\0");
        block[263..265].copy_from_slice(b"00");
        octal(&mut block[329..337], 0);
        octal(&mut block[337..345], 0);
        block[345..345 + prefix.len()].copy_from_slice(prefix);
        block[148..156].copy_from_slice(b"        ");
        let sum: u64 = block.iter().map(|&b| u64::from(b)).sum();
        octal(&mut block[148..155], sum);
        block[155] = b' ';
        self.out.extend_from_slice(&block);
    }

    fn data(&mut self, data: &[u8]) {
        self.out.extend_from_slice(data);
        let pad = (512 - data.len() % 512) % 512;
        self.out.extend(std::iter::repeat_n(0, pad));
    }

    fn file(&mut self, name: &str, data: &[u8], mode: u32) -> &mut Self {
        self.header(
            name.as_bytes(),
            b'0',
            data.len() as u64,
            mode,
            b"",
            1_600_000_000,
        );
        self.data(data);
        self
    }

    fn dir(&mut self, name: &str, mode: u32) -> &mut Self {
        self.header(name.as_bytes(), b'5', 0, mode, b"", 1_500_000_000);
        self
    }

    fn symlink(&mut self, name: &str, target: &str) -> &mut Self {
        self.header(
            name.as_bytes(),
            b'2',
            0,
            0o777,
            target.as_bytes(),
            1_600_000_000,
        );
        self
    }

    fn hard_link(&mut self, name: &str, target: &str) -> &mut Self {
        self.header(
            name.as_bytes(),
            b'1',
            0,
            0o644,
            target.as_bytes(),
            1_600_000_000,
        );
        self
    }

    fn fifo(&mut self, name: &str) -> &mut Self {
        self.header(name.as_bytes(), b'6', 0, 0o640, b"", 1_600_000_000);
        self
    }

    fn pax(&mut self, records: &[(&str, &[u8])]) -> &mut Self {
        let mut body = Vec::new();
        for (key, value) in records {
            let rest = key.len() + value.len() + 3;
            let mut length = rest + 1;
            while format!("{length}").len() + rest != length {
                length += 1;
            }
            body.extend_from_slice(format!("{length} {key}=").as_bytes());
            body.extend_from_slice(value);
            body.push(b'\n');
        }
        self.header(b"PaxHeader", b'x', body.len() as u64, 0o644, b"", 0);
        self.data(&body);
        self
    }

    fn end(&mut self) -> Vec<u8> {
        self.out.extend_from_slice(&[0u8; 1024]);
        std::mem::take(&mut self.out)
    }
}

fn digest(blob: &[u8]) -> [u8; 32] {
    Sha256::digest(blob).into()
}

/// Writes the blobs as a layers disk next to the root and unpacks them in
/// order; the first failure's message, if any.
fn unpack_all(scratch: &Scratch, blobs: &[(Compression, Vec<u8>)]) -> Result<(), String> {
    let path = scratch.dir.join("layers.img");
    let mut disk = fs::File::create(&path).unwrap();
    for (_, blob) in blobs {
        disk.write_all(blob).unwrap();
    }
    // A block-size tail, as a real disk has.
    disk.write_all(&[0xa5; 300]).unwrap();
    drop(disk);
    let disk = fs::File::open(&path).unwrap();
    let mut tree = scratch.tree();
    let mut offset = 0;
    for (index, (compression, blob)) in blobs.iter().enumerate() {
        let layer = Layer {
            compression: *compression,
            digest: digest(blob),
            size: blob.len() as u64,
        };
        unpack_layer(&disk, offset, &layer, &mut tree, u64::MAX)
            .map_err(|problem| format!("layer {index}: {problem}"))?;
        offset += layer.size;
    }
    Ok(())
}

fn plain(tar: Vec<u8>) -> (Compression, Vec<u8>) {
    (Compression::None, tar)
}

fn gzip(tar: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(tar).unwrap();
    encoder.finish().unwrap()
}

fn zstd(tar: &[u8]) -> Vec<u8> {
    ruzstd::encoding::compress_to_vec(tar, ruzstd::encoding::CompressionLevel::Fastest)
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
}

fn names(path: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

#[test]
fn every_kind_of_entry() {
    let scratch = Scratch::new();
    let tar = TarWriter::default()
        .dir("./", 0o755)
        .dir("etc/", 0o751)
        .file("etc/os-release", b"ID=test\n", 0o644)
        .file("bin/tool", b"#!/bin/sh\n", 0o6755)
        .dir("tmp/", 0o1777)
        .symlink("bin/sh", "busybox")
        .symlink("etc/mtab", "/proc/self/mounts")
        .hard_link("bin/tool2", "bin/tool")
        .fifo("run.fifo")
        .pax(&[
            ("SCHILY.xattr.user.test", b"value\0with nul"),
            ("mtime", b"1700000000.5"),
        ])
        .file("xattr", b"x", 0o600)
        .end();
    unpack_all(&scratch, &[plain(tar)]).unwrap();
    let root = scratch.root();
    assert_eq!(fs::read(root.join("etc/os-release")).unwrap(), b"ID=test\n");
    assert_eq!(mode(&root.join("etc")), 0o751);
    // Created implicitly, as runtimes do.
    assert_eq!(mode(&root.join("bin")), 0o755);
    assert_eq!(mode(&root.join("bin/tool")), 0o6755);
    assert_eq!(mode(&root.join("tmp")), 0o1777);
    assert_eq!(
        fs::read_link(root.join("bin/sh")).unwrap(),
        Path::new("busybox")
    );
    assert_eq!(
        fs::read_link(root.join("etc/mtab")).unwrap(),
        Path::new("/proc/self/mounts")
    );
    let tool = fs::metadata(root.join("bin/tool")).unwrap();
    let tool2 = fs::metadata(root.join("bin/tool2")).unwrap();
    assert_eq!((tool.ino(), tool.nlink()), (tool2.ino(), 2));
    assert!(
        fs::symlink_metadata(root.join("run.fifo"))
            .unwrap()
            .file_type()
            .is_fifo()
    );
    assert_eq!(mode(&root.join("run.fifo")), 0o640);
    assert_eq!(tool.mtime(), 1_600_000_000);
    let xattr = fs::metadata(root.join("xattr")).unwrap();
    assert_eq!(
        (xattr.mtime(), xattr.mtime_nsec()),
        (1_700_000_000, 500_000_000)
    );
    // A directory's time is set once its contents are in.
    assert_eq!(
        fs::metadata(root.join("etc")).unwrap().mtime(),
        1_500_000_000
    );
    assert_eq!(fs::metadata(&root).unwrap().mtime(), 1_500_000_000);
    let (uid, gid) = ids();
    assert_eq!(
        (tool.uid(), tool.gid()),
        (u32::try_from(uid).unwrap(), u32::try_from(gid).unwrap())
    );
    assert_eq!(scratch.outside(), ["layers.img", "root"]);
}

#[test]
fn extended_attributes_are_kept() {
    let scratch = Scratch::new();
    let tar = TarWriter::default()
        .pax(&[("SCHILY.xattr.user.gigahost", b"kept\x00\xff")])
        .file("f", b"x", 0o644)
        .end();
    unpack_all(&scratch, &[plain(tar)]).unwrap();
    let path = CString::new(scratch.root().join("f").into_os_string().into_vec()).unwrap();
    assert_eq!(
        crate::sys::fs::get_xattr(&path, c"user.gigahost").unwrap(),
        b"kept\x00\xff"
    );
}

#[test]
fn whiteouts_and_opaque_directories() {
    let scratch = Scratch::new();
    let lower = TarWriter::default()
        .dir("a/", 0o755)
        .file("a/x", b"x", 0o644)
        .dir("a/sub/", 0o755)
        .file("a/sub/deep", b"d", 0o644)
        .file("b/z", b"z", 0o644)
        .file("keep/k", b"k", 0o644)
        .file("gone", b"g", 0o644)
        .end();
    let upper = TarWriter::default()
        .file("a/early", b"e", 0o644)
        .file("a/.wh..wh..opq", b"", 0o644)
        .file("a/new", b"n", 0o644)
        .file(".wh.b", b"", 0o644)
        .file(".wh.gone", b"", 0o644)
        .file(".wh.never-there", b"", 0o644)
        .file("missing/.wh.x", b"", 0o644)
        // The same layer's own entry stays, whiteout or not.
        .file("c/own", b"o", 0o644)
        .file("c/.wh.own", b"", 0o644)
        .end();
    unpack_all(&scratch, &[plain(lower), plain(upper)]).unwrap();
    let root = scratch.root();
    assert_eq!(names(&root.join("a")), ["early", "new"]);
    assert_eq!(names(&root), ["a", "c", "keep"]);
    assert_eq!(names(&root.join("c")), ["own"]);
    assert_eq!(names(&root.join("keep")), ["k"]);
}

#[test]
fn opaque_keeps_this_layers_directories() {
    let scratch = Scratch::new();
    let lower = TarWriter::default().file("d/sub/lower", b"l", 0o644).end();
    let upper = TarWriter::default()
        .file("d/sub/upper", b"u", 0o644)
        .file("d/.wh..wh..opq", b"", 0o644)
        .end();
    unpack_all(&scratch, &[plain(lower), plain(upper)]).unwrap();
    assert_eq!(names(&scratch.root().join("d")), ["sub"]);
    assert_eq!(names(&scratch.root().join("d/sub")), ["upper"]);
}

#[test]
fn replacements() {
    let scratch = Scratch::new();
    let lower = TarWriter::default()
        .file("p/inside/file", b"1", 0o644)
        .file("q", b"2", 0o644)
        .symlink("r", "q")
        .dir("s/", 0o700)
        .file("s/kept", b"3", 0o644)
        .symlink("t", "/etc")
        .end();
    let upper = TarWriter::default()
        .file("p", b"now a file", 0o644)
        .dir("q/", 0o755)
        .file("r", b"now a file", 0o600)
        .dir("s/", 0o750)
        .dir("t/", 0o755)
        .end();
    unpack_all(&scratch, &[plain(lower), plain(upper)]).unwrap();
    let root = scratch.root();
    assert_eq!(fs::read(root.join("p")).unwrap(), b"now a file");
    assert!(fs::symlink_metadata(root.join("q")).unwrap().is_dir());
    assert!(fs::symlink_metadata(root.join("r")).unwrap().is_file());
    assert_eq!(fs::read(root.join("q")).ok(), None);
    // A directory over a directory merges: the new mode, the old contents.
    assert_eq!(mode(&root.join("s")), 0o750);
    assert_eq!(names(&root.join("s")), ["kept"]);
    // A directory over a symbolic link replaces the link.
    assert!(fs::symlink_metadata(root.join("t")).unwrap().is_dir());
    assert!(!root.join("etc").exists());
}

#[test]
fn nothing_escapes_the_root() {
    let scratch = Scratch::new();
    fs::write(scratch.dir.join("outside"), b"host").unwrap();
    let tar = TarWriter::default()
        .symlink("up", "../../..")
        .symlink("abs", "/")
        .symlink("abs-etc", "/etc")
        .dir("etc/", 0o755)
        .file("up/escaped", b"1", 0o644)
        .file("abs/escaped2", b"2", 0o644)
        .file("abs-etc/passwd", b"3", 0o644)
        .file("../dotdot", b"4", 0o644)
        .file("/absolute", b"5", 0o644)
        .file("a/../../../../b", b"6", 0o644)
        .hard_link("hl", "up/escaped")
        .file("outside", b"inside", 0o644)
        .hard_link("hl2", "../outside")
        .end();
    unpack_all(&scratch, &[plain(tar)]).unwrap();
    let root = scratch.root();
    assert_eq!(fs::read(root.join("escaped")).unwrap(), b"1");
    assert_eq!(fs::read(root.join("escaped2")).unwrap(), b"2");
    assert_eq!(fs::read(root.join("etc/passwd")).unwrap(), b"3");
    assert_eq!(fs::read(root.join("dotdot")).unwrap(), b"4");
    assert_eq!(fs::read(root.join("absolute")).unwrap(), b"5");
    assert_eq!(fs::read(root.join("b")).unwrap(), b"6");
    assert_eq!(fs::read(root.join("hl")).unwrap(), b"1");
    assert_eq!(fs::read(root.join("hl2")).unwrap(), b"inside");
    assert_eq!(fs::read(scratch.dir.join("outside")).unwrap(), b"host");
    assert_eq!(scratch.outside(), ["layers.img", "outside", "root"]);
}

#[test]
fn hard_links_only_to_the_result() {
    let scratch = Scratch::new();
    fs::write(scratch.dir.join("outside"), b"host").unwrap();
    let tar = TarWriter::default().hard_link("hl", "../outside").end();
    let error = unpack_all(&scratch, &[plain(tar)]).unwrap_err();
    assert_eq!(
        error,
        "layer 0: the archive is invalid: a hard link names a path that does not exist"
    );
    let tar = TarWriter::default()
        .dir("d/", 0o755)
        .hard_link("hd", "d")
        .end();
    let error = unpack_all(&Scratch::new(), &[plain(tar)]).unwrap_err();
    assert_eq!(
        error,
        "layer 0: the archive is invalid: a hard link names a directory"
    );
    assert_eq!(
        fs::metadata(scratch.dir.join("outside")).unwrap().nlink(),
        1
    );
}

#[test]
fn a_symbolic_link_is_never_written_through() {
    let scratch = Scratch::new();
    fs::write(scratch.dir.join("outside"), b"host").unwrap();
    let lower = TarWriter::default().symlink("link", "../outside").end();
    let upper = TarWriter::default().file("link", b"replaced", 0o644).end();
    unpack_all(&scratch, &[plain(lower), plain(upper)]).unwrap();
    assert_eq!(fs::read(scratch.root().join("link")).unwrap(), b"replaced");
    assert_eq!(fs::read(scratch.dir.join("outside")).unwrap(), b"host");
}

#[test]
fn compressed_layers() {
    let scratch = Scratch::new();
    let first = TarWriter::default().file("plain", b"p", 0o644).end();
    let second = TarWriter::default().file("gzip", b"g", 0o644).end();
    let third = TarWriter::default().file("zstd", b"z", 0o644).end();
    // Two gzip members and two zstd frames, as parallel compressors write.
    let (head, tail) = second.split_at(512);
    let gzipped = [gzip(head), gzip(tail)].concat();
    let (head, tail) = third.split_at(700);
    let skippable = [
        &0x184D_2A50u32.to_le_bytes()[..],
        &3u32.to_le_bytes(),
        b"abc",
    ]
    .concat();
    let zstded = [zstd(head), skippable, zstd(tail)].concat();
    unpack_all(
        &scratch,
        &[
            plain(first),
            (Compression::Gzip, gzipped),
            (Compression::Zstd, zstded),
        ],
    )
    .unwrap();
    assert_eq!(names(&scratch.root()), ["gzip", "plain", "zstd"]);
}

#[test]
fn a_wrong_digest_or_a_short_disk() {
    let scratch = Scratch::new();
    let tar = TarWriter::default().file("f", b"x", 0o644).end();
    let path = scratch.dir.join("layers.img");
    fs::write(&path, &tar).unwrap();
    let disk = fs::File::open(&path).unwrap();
    let mut tree = scratch.tree();
    let mut layer = Layer {
        compression: Compression::None,
        digest: [0; 32],
        size: tar.len() as u64,
    };
    assert_eq!(
        unpack_layer(&disk, 0, &layer, &mut tree, u64::MAX).unwrap_err(),
        "its digest does not match"
    );
    layer.digest = digest(&tar);
    layer.size += 1;
    assert_eq!(
        unpack_layer(&disk, 0, &layer, &mut tree, u64::MAX).unwrap_err(),
        "the layers disk ends inside it"
    );
    // A bad archive whose digest is wrong too is reported as the digest.
    let mut broken = tar.clone();
    broken[0] ^= 1;
    fs::write(&path, &broken).unwrap();
    let disk = fs::File::open(&path).unwrap();
    layer.size -= 1;
    assert_eq!(
        unpack_layer(&disk, 0, &layer, &mut tree, u64::MAX).unwrap_err(),
        "its digest does not match"
    );
}

#[test]
fn a_layer_cannot_read_into_the_next() {
    let scratch = Scratch::new();
    // The first layer is an archive cut before its end marker; the
    // second one's bytes would complete it.
    let whole = TarWriter::default().file("f", b"x", 0o644).end();
    let (head, tail) = whole.split_at(1024);
    let error = unpack_all(
        &scratch,
        &[
            plain(head.to_vec()),
            plain([tail, &[0u8; 1024][..]].concat()),
        ],
    )
    .unwrap_err();
    assert_eq!(
        error,
        "layer 0: the archive is invalid: the archive has no end marker"
    );
}

#[test]
fn malformed_archives() {
    let cases: Vec<(Vec<u8>, &str)> = vec![
        (
            TarWriter::default().file("f", b"x", 0o644).out.clone(),
            "the archive is invalid: the archive has no end marker",
        ),
        (
            {
                let mut tar = TarWriter::default().file("f", b"x", 0o644).end();
                tar.truncate(1024 + 512);
                tar
            },
            "the archive is invalid: the archive has no end marker",
        ),
        (
            {
                let mut tar = TarWriter::default().file("f", b"x", 0o644).end();
                tar[0] = b'g';
                tar
            },
            "the archive is invalid: a header's checksum is wrong",
        ),
        (
            {
                let mut tar = TarWriter::default().file("f", &[1; 600], 0o644).end();
                tar.truncate(700);
                tar
            },
            "the archive is invalid: the archive ends inside an entry",
        ),
        (
            {
                let mut tar = TarWriter::default().file("f", b"x", 0o644).end();
                tar.extend_from_slice(&vec![7u8; (1 << 20) + 1]);
                tar
            },
            "the archive is invalid: more data follows the archive's end marker",
        ),
        (
            {
                let mut writer = TarWriter::default();
                writer.header(b"sparse", b'S', 0, 0o644, b"", 0);
                writer.end()
            },
            "the archive holds a sparse file, which is not unpacked",
        ),
        (
            {
                let mut writer = TarWriter::default();
                writer.header(b"volume", b'V', 0, 0o644, b"", 0);
                writer.end()
            },
            "the archive holds an entry type this init does not unpack, which is not unpacked",
        ),
        (
            {
                let mut writer = TarWriter::default();
                // A PAX header claiming 1 GiB: refused before any is read.
                writer.header(b"PaxHeader", b'x', 1 << 30, 0o644, b"", 0);
                writer.end()
            },
            "the archive is invalid: an extension header is too long",
        ),
        (
            {
                let mut writer = TarWriter::default();
                writer.header(b"././@LongLink", b'L', 1 << 32, 0o644, b"", 0);
                writer.end()
            },
            "the archive is invalid: an extension header is too long",
        ),
        (
            TarWriter::default()
                .pax(&[("path", "a/".repeat(2100).as_bytes())])
                .file("short", b"", 0o644)
                .end(),
            "the archive is invalid: a path is too long",
        ),
        (
            TarWriter::default().pax(&[("path", b"x")]).end(),
            "the archive is invalid: an extension header describes no entry",
        ),
        (
            TarWriter::default().file("./", b"", 0o644).end(),
            "the archive is invalid: an entry names the root but is not a directory",
        ),
        (
            TarWriter::default()
                .file("file", b"", 0o644)
                .file("file/child", b"", 0o644)
                .end(),
            "the archive is invalid: a path's parent is not a directory",
        ),
    ];
    for (tar, want) in cases {
        let scratch = Scratch::new();
        let error = unpack_all(&scratch, &[plain(tar)]).unwrap_err();
        assert_eq!(error, format!("layer 0: {want}"));
    }
}

#[test]
fn invalid_compressed_streams() {
    let tar = TarWriter::default().file("f", b"x", 0o644).end();
    let mut gzipped = gzip(&tar);
    // The CRC in the trailer.
    let crc = gzipped.len() - 8;
    gzipped[crc] ^= 0xff;
    let error = unpack_all(&Scratch::new(), &[(Compression::Gzip, gzipped)]).unwrap_err();
    assert_eq!(error, "layer 0: the gzip stream is invalid");
    let error = unpack_all(&Scratch::new(), &[(Compression::Zstd, tar.clone())]).unwrap_err();
    assert_eq!(error, "layer 0: the zstd stream is invalid");
    let error = unpack_all(&Scratch::new(), &[(Compression::Gzip, tar)]).unwrap_err();
    assert_eq!(error, "layer 0: the gzip stream is invalid");
}

#[test]
fn the_expansion_is_bounded() {
    let scratch = Scratch::new();
    let tar = TarWriter::default()
        .file("f", &vec![0; 100_000], 0o644)
        .end();
    let blob = gzip(&tar);
    let path = scratch.dir.join("layers.img");
    fs::write(&path, &blob).unwrap();
    let disk = fs::File::open(&path).unwrap();
    let layer = Layer {
        compression: Compression::Gzip,
        digest: digest(&blob),
        size: blob.len() as u64,
    };
    assert_eq!(
        unpack_layer(&disk, 0, &layer, &mut scratch.tree(), 50_000).unwrap_err(),
        "it expands to more than the root disk can hold"
    );
}

#[test]
fn long_names_and_ustar_prefixes() {
    let scratch = Scratch::new();
    let deep = format!("{}file", "directory/".repeat(50));
    let long = format!("{}/{}", "d".repeat(150), "f".repeat(90));
    let mut writer = TarWriter::default();
    writer
        .pax(&[("path", deep.as_bytes())])
        .file("x", b"1", 0o644);
    // A ustar prefix: 150 + name 90.
    writer.file(&long, b"2", 0o644);
    // A GNU long name.
    let gnu = format!("{}/gnu", "g".repeat(200));
    let mut name = gnu.clone().into_bytes();
    name.push(0);
    writer.header(b"././@LongLink", b'L', name.len() as u64, 0o644, b"", 0);
    writer.data(&name);
    writer.file("ignored-short-name", b"3", 0o644);
    let tar = writer.end();
    unpack_all(&scratch, &[plain(tar)]).unwrap();
    let root = scratch.root();
    assert_eq!(fs::read(root.join(&deep)).unwrap(), b"1");
    assert_eq!(fs::read(root.join(&long)).unwrap(), b"2");
    assert_eq!(fs::read(root.join(&gnu)).unwrap(), b"3");
    assert!(!root.join("ignored-short-name").exists());
}
