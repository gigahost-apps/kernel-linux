//! Interface version 2's unpack mode (docs/interface.md): the image's
//! layers, read from the read-only layers disk and checked by digest, are
//! unpacked onto the empty root disk; then the machine powers off. The
//! host never parses an archive: everything untrusted is read here, in
//! the guest.

pub mod apply;
pub mod tar;
#[cfg(test)]
mod tests;

use crate::config::{Compression, Layer, Unpack};
use crate::console::{self, fail, log};
use crate::protocol::Report;
use crate::setup::{self, DISK, NEWROOT};
use crate::sys::{self, mount};
use apply::Tree;
use sha2::{Digest, Sha256};
use std::fmt;
use std::io::{self, BufRead, BufReader, Read};
use std::os::unix::fs::FileExt;

/// The layers disk: the blobs one after the other, in the config's order.
const LAYERS: &str = "/dev/vdb";

/// The largest zstd window accepted: what every compression level but the
/// largest `--ultra` ones and `--long` uses, and bounded for a small guest.
const MAX_ZSTD_WINDOW: u64 = 64 << 20;

/// How much may follow a tar's end marker in its stream: a writer's zero
/// padding (GNU tar pads to 10 KiB) and a little more.
const MAX_TRAILING: u64 = 1 << 20;

/// What may go wrong in a layer; [`Failure::message`] names the kind, never
/// the layer's contents.
#[derive(Debug)]
pub enum Failure {
    /// Reading the layer's (decompressed) stream failed.
    Stream(io::Error),
    /// The decompressed stream is longer than the root disk could hold.
    TooLarge,
    /// The archive is malformed.
    Archive(&'static str),
    /// The archive holds something the unpack does not make.
    Unsupported(&'static str),
    /// A file system step failed.
    Apply(&'static str, io::Error),
}

/// Marks a stream error with where it arose.
#[derive(Debug)]
enum Fault {
    LayersDisk,
    Decompress,
}

impl fmt::Display for Fault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Fault::LayersDisk => "reading the layers disk failed",
            Fault::Decompress => "the compressed stream is invalid",
        })
    }
}

impl std::error::Error for Fault {}

fn fault(error: &io::Error) -> Option<&Fault> {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<Fault>())
}

impl Failure {
    pub fn message(&self, compression: Compression) -> String {
        let format = match compression {
            Compression::None => "tar",
            Compression::Gzip => "gzip",
            Compression::Zstd => "zstd",
        };
        match self {
            Failure::Stream(error) => match fault(error) {
                Some(Fault::LayersDisk) => "reading the layers disk failed".into(),
                Some(Fault::Decompress) => format!("the {format} stream is invalid"),
                None => format!("reading the {format} stream failed: {error}"),
            },
            Failure::TooLarge => "it expands to more than the root disk can hold".into(),
            Failure::Archive(problem) => format!("the archive is invalid: {problem}"),
            Failure::Unsupported(what) => {
                format!("the archive holds {what}, which is not unpacked")
            }
            Failure::Apply(_, error)
                if matches!(error.raw_os_error(), Some(libc::ENOSPC | libc::EDQUOT)) =>
            {
                "the root disk is full".into()
            }
            Failure::Apply(what, error) => format!("{what} failed: {error}"),
        }
    }
}

/// Exactly one layer's bytes of the layers disk, hashed as they are read.
struct LayerReader<'a> {
    disk: &'a std::fs::File,
    offset: u64,
    remaining: u64,
    hasher: Sha256,
    /// The disk ended before the layer did.
    truncated: bool,
    failed: Option<io::Error>,
}

impl<'a> LayerReader<'a> {
    fn new(disk: &'a std::fs::File, offset: u64, size: u64) -> Self {
        LayerReader {
            disk,
            offset,
            remaining: size,
            hasher: Sha256::new(),
            truncated: false,
            failed: None,
        }
    }

    /// Reads (and hashes) what is left of the layer.
    fn drain(&mut self) {
        let mut buffer = vec![0u8; 1 << 16];
        while self.remaining > 0 && !self.truncated && self.failed.is_none() {
            if self.read(&mut buffer).is_err() {
                return;
            }
        }
    }
}

impl Read for LayerReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 || self.truncated {
            return Ok(0);
        }
        if self.failed.is_some() {
            return Err(io::Error::other(Fault::LayersDisk));
        }
        let want = usize::try_from(self.remaining)
            .unwrap_or(usize::MAX)
            .min(buffer.len());
        let piece = buffer.get_mut(..want).unwrap_or_default();
        loop {
            match self.disk.read_at(piece, self.offset) {
                Ok(0) => {
                    self.truncated = true;
                    return Ok(0);
                }
                Ok(count) => {
                    self.hasher.update(piece.get(..count).unwrap_or_default());
                    let read = u64::try_from(count).unwrap_or(0);
                    self.offset += read;
                    self.remaining -= read;
                    return Ok(count);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    self.failed = Some(error);
                    return Err(io::Error::other(Fault::LayersDisk));
                }
            }
        }
    }
}

/// Marks a decompressor's own errors as [`Fault::Decompress`], keeping
/// the layers disk's.
struct Decoded<R>(R);

impl<R: Read> Read for Decoded<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.0.read(buffer).map_err(|error| {
            if fault(&error).is_some() {
                error
            } else {
                io::Error::other(Fault::Decompress)
            }
        })
    }
}

/// A zstd stream: frames one after the other, skippable frames skipped.
struct ZstdFrames<R> {
    source: R,
    decoder: ruzstd::decoding::FrameDecoder,
    in_frame: bool,
}

impl<R: BufRead> ZstdFrames<R> {
    fn new(source: R) -> Self {
        let mut decoder = ruzstd::decoding::FrameDecoder::new();
        decoder.set_max_window_size(MAX_ZSTD_WINDOW);
        ZstdFrames {
            source,
            decoder,
            in_frame: false,
        }
    }

    /// Starts the next frame; `false` at the end of the stream.
    fn next_frame(&mut self) -> io::Result<bool> {
        use ruzstd::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};
        loop {
            if self.source.fill_buf()?.is_empty() {
                return Ok(false);
            }
            match self.decoder.reset(&mut self.source) {
                Ok(()) => return Ok(true),
                Err(FrameDecoderError::ReadFrameHeaderError(ReadFrameHeaderError::SkipFrame {
                    length,
                    ..
                })) => {
                    let skipped = io::copy(
                        &mut (&mut self.source).take(u64::from(length)),
                        &mut io::sink(),
                    )?;
                    if skipped != u64::from(length) {
                        return Err(io::Error::other(Fault::Decompress));
                    }
                }
                Err(_) => return Err(io::Error::other(Fault::Decompress)),
            }
        }
    }
}

impl<R: BufRead> Read for ZstdFrames<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        use ruzstd::decoding::BlockDecodingStrategy;
        if buffer.is_empty() {
            return Ok(0);
        }
        loop {
            if !self.in_frame {
                if !self.next_frame()? {
                    return Ok(0);
                }
                self.in_frame = true;
            }
            while self.decoder.can_collect() == 0 && !self.decoder.is_finished() {
                self.decoder
                    .decode_blocks(&mut self.source, BlockDecodingStrategy::UptoBlocks(1))
                    .map_err(|_| io::Error::other(Fault::Decompress))?;
            }
            let count = self.decoder.read(buffer)?;
            if count > 0 {
                return Ok(count);
            }
            // The frame is finished and collected: on to the next.
            self.in_frame = false;
        }
    }
}

/// Unpacks one layer, `layer.size` bytes at `offset` of `disk`, onto
/// `tree`; `limit` bounds its decompressed stream. Every byte of the
/// layer is hashed, whatever the archive holds; a short or unreadable
/// disk, then a wrong digest, then the archive's own failure is the
/// error.
pub fn unpack_layer(
    disk: &std::fs::File,
    offset: u64,
    layer: &Layer,
    tree: &mut Tree,
    limit: u64,
) -> Result<(), String> {
    let mut raw = LayerReader::new(disk, offset, layer.size);
    let applied = {
        let buffered = BufReader::with_capacity(1 << 17, &mut raw);
        let stream: Box<dyn Read + '_> = match layer.compression {
            Compression::None => Box::new(buffered),
            Compression::Gzip => Box::new(Decoded(flate2::bufread::MultiGzDecoder::new(buffered))),
            Compression::Zstd => Box::new(Decoded(ZstdFrames::new(buffered))),
        };
        apply_archive(stream, tree, limit)
    };
    raw.drain();
    if raw.truncated {
        return Err("the layers disk ends inside it".into());
    }
    if raw.failed.is_some() {
        return Err("reading the layers disk failed".into());
    }
    let digest = std::mem::take(&mut raw.hasher).finalize();
    if digest.as_slice() != layer.digest.as_slice() {
        return Err("its digest does not match".into());
    }
    applied.map_err(|failure| failure.message(layer.compression))
}

/// Applies every entry of one tar stream, then requires its end.
fn apply_archive(stream: impl Read, tree: &mut Tree, limit: u64) -> Result<(), Failure> {
    let mut reader = tar::Reader::new(stream, limit);
    tree.begin_layer();
    while let Some((entry, _)) = reader.next_entry()? {
        tree.apply(&entry, &mut reader.data())?;
    }
    reader.finish(MAX_TRAILING)?;
    tree.end_layer()
}

/// How many decompressed bytes a layer may hold: twice the root disk (an
/// archive may write a path more than once) and 1 GiB for its headers.
fn expansion_limit() -> u64 {
    let sectors = std::fs::read_to_string("/sys/block/vda/size")
        .ok()
        .and_then(|text| text.trim().parse::<u64>().ok())
        .unwrap_or(0);
    sectors
        .saturating_mul(512)
        .saturating_mul(2)
        .saturating_add(1 << 30)
}

/// The unpack mode's steps 2 to 5: never returns.
pub fn run(config: &Unpack) -> ! {
    match unpack(config) {
        Ok(()) => {
            console::report(&Report::unpacked());
            console::power_off()
        }
        Err(message) => {
            sys::sync();
            let _ = mount::umount(NEWROOT);
            fail(&message)
        }
    }
}

fn unpack(config: &Unpack) -> Result<(), String> {
    setup::wait_for(DISK)?;
    setup::wait_for(LAYERS)?;
    mount::mount(
        Some(DISK),
        NEWROOT,
        Some("ext4"),
        mount::NOSUID | mount::NODEV | mount::NOEXEC,
        None,
    )
    .map_err(|error| format!("mounting the root disk failed: {error}"))?;
    let entries = std::fs::read_dir(NEWROOT)
        .map_err(|error| format!("reading the root disk failed: {error}"))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("reading the root disk failed: {error}"))?;
        if entry.file_name() != "lost+found" {
            return Err("the root disk is not empty".into());
        }
    }
    let disk = std::fs::File::open(LAYERS)
        .map_err(|error| format!("opening the layers disk failed: {error}"))?;
    let root = std::fs::File::open(NEWROOT)
        .map_err(|error| format!("opening the root disk failed: {error}"))?;
    let mut tree = Tree::new(root.into());
    let limit = expansion_limit();
    let mut offset: u64 = 0;
    for (index, layer) in config.layers.iter().enumerate() {
        unpack_layer(&disk, offset, layer, &mut tree, limit)
            .map_err(|problem| format!("layer {index}: {problem}"))?;
        log(&format!("unpacked layer {index}"));
        offset = offset.saturating_add(layer.size);
    }
    if tree.dropped_xattrs > 0 {
        log(&format!(
            "{} extended attribute(s) not kept: the file system refused them",
            tree.dropped_xattrs
        ));
    }
    drop(tree);
    sys::sync();
    mount::umount(NEWROOT).map_err(|error| format!("unmounting the root disk failed: {error}"))?;
    Ok(())
}
