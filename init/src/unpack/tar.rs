//! A streaming tar reader with bounds: the headers are decoded by the
//! `tar` crate's [`tar::Header`], the stream around them is walked here so
//! that every extension header (PAX, GNU long names) is read into a
//! bounded buffer and the end marker is required. Nothing here touches
//! the file system.

use super::Failure;
use std::io::{self, Read};

const BLOCK: u64 = 512;

/// The largest PAX extended header (local or global) read.
const MAX_PAX: u64 = 1 << 20;

/// The largest GNU long name or long link read (Linux's `PATH_MAX` and
/// its NUL, with room to spare).
const MAX_LONG_NAME: u64 = 8192;

/// What an entry is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Directory,
    /// A symbolic link to the target, as written.
    Symlink(Vec<u8>),
    /// A hard link to the path, as written.
    HardLink(Vec<u8>),
    CharDevice(u32, u32),
    BlockDevice(u32, u32),
    Fifo,
}

/// One entry's header, its extensions applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub path: Vec<u8>,
    pub kind: Kind,
    /// The permission bits, setuid, setgid and sticky included.
    pub mode: u32,
    pub uid: u64,
    pub gid: u64,
    pub mtime: (i64, u32),
    /// `SCHILY.xattr.<name>` PAX records: the name and the value.
    pub xattrs: Vec<(Vec<u8>, Vec<u8>)>,
}

/// The PAX records that describe the next entry.
#[derive(Default)]
struct Pax {
    path: Option<Vec<u8>>,
    linkpath: Option<Vec<u8>>,
    size: Option<u64>,
    uid: Option<u64>,
    gid: Option<u64>,
    mtime: Option<(i64, u32)>,
    xattrs: Vec<(Vec<u8>, Vec<u8>)>,
}

/// Walks one tar stream. The caller reads each entry's data through
/// [`Reader::data`] before asking for the next entry.
pub struct Reader<R> {
    inner: R,
    /// The data bytes of the current entry not yet read.
    remaining: u64,
    /// The padding after the current entry's data.
    padding: u64,
    /// Bytes read from `inner` so far, and how many it may give.
    consumed: u64,
    limit: u64,
}

fn archive(problem: &'static str) -> Failure {
    Failure::Archive(problem)
}

/// The padding that brings `size` to a whole block.
fn padding(size: u64) -> u64 {
    (BLOCK - size % BLOCK) % BLOCK
}

impl<R: Read> Reader<R> {
    /// A reader that refuses to read more than `limit` bytes of `inner`.
    pub fn new(inner: R, limit: u64) -> Self {
        Reader {
            inner,
            remaining: 0,
            padding: 0,
            consumed: 0,
            limit,
        }
    }

    /// Reads from `inner` into `buffer`, counting against the limit.
    fn read_inner(&mut self, buffer: &mut [u8]) -> Result<usize, Failure> {
        loop {
            match self.inner.read(buffer) {
                Ok(count) => {
                    self.consumed = self
                        .consumed
                        .saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
                    if self.consumed > self.limit {
                        return Err(Failure::TooLarge);
                    }
                    return Ok(count);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(Failure::Stream(error)),
            }
        }
    }

    /// Fills `buffer`; `false` when the stream ended before its first byte.
    fn fill(&mut self, buffer: &mut [u8]) -> Result<bool, Failure> {
        let mut filled = 0;
        while filled < buffer.len() {
            let count = self.read_inner(buffer.get_mut(filled..).unwrap_or_default())?;
            if count == 0 {
                if filled == 0 {
                    return Ok(false);
                }
                return Err(archive("the archive ends inside a header"));
            }
            filled += count;
        }
        Ok(true)
    }

    /// Reads and drops `count` bytes.
    fn skip(&mut self, mut count: u64) -> Result<(), Failure> {
        let mut buffer = [0u8; 8192];
        while count > 0 {
            let want = usize::try_from(count.min(8192)).unwrap_or(8192);
            let got = self.read_inner(buffer.get_mut(..want).unwrap_or_default())?;
            if got == 0 {
                return Err(archive("the archive ends inside an entry"));
            }
            count -= u64::try_from(got).unwrap_or(count);
        }
        Ok(())
    }

    /// Skips what is left of the current entry: its unread data and its
    /// padding.
    fn finish_entry(&mut self) -> Result<(), Failure> {
        let rest = self.remaining.saturating_add(self.padding);
        self.remaining = 0;
        self.padding = 0;
        self.skip(rest)
    }

    /// Reads an extension header's body of `size` bytes (at most `max`)
    /// and its padding.
    fn read_extension(&mut self, size: u64, max: u64) -> Result<Vec<u8>, Failure> {
        if size > max {
            return Err(archive("an extension header is too long"));
        }
        let mut body = vec![0u8; usize::try_from(size).unwrap_or(0)];
        if !body.is_empty() && !self.fill(&mut body)? {
            return Err(archive("the archive ends inside an entry"));
        }
        self.skip(padding(size))?;
        Ok(body)
    }

    /// The next entry, or `None` at the end marker (two zero blocks).
    /// A stream that ends without the marker is an error.
    pub fn next_entry(&mut self) -> Result<Option<(Entry, u64)>, Failure> {
        self.finish_entry()?;
        let mut pax: Option<Pax> = None;
        let mut long_name: Option<Vec<u8>> = None;
        let mut long_link: Option<Vec<u8>> = None;
        loop {
            let mut block = [0u8; 512];
            if !self.fill(&mut block)? {
                return Err(archive("the archive has no end marker"));
            }
            if block.iter().all(|&byte| byte == 0) {
                if !self.fill(&mut block)? || block.iter().any(|&byte| byte != 0) {
                    return Err(archive("the archive has no end marker"));
                }
                if pax.is_some() || long_name.is_some() || long_link.is_some() {
                    return Err(archive("an extension header describes no entry"));
                }
                return Ok(None);
            }
            let header = tar::Header::from_byte_slice(&block);
            check_sum(&block, header)?;
            let header_size = header
                .entry_size()
                .map_err(|_| archive("a header's size is invalid"))?;
            let size = pax.as_ref().and_then(|pax| pax.size).unwrap_or(header_size);
            match header.entry_type().as_byte() {
                b'x' => {
                    if pax.is_some() {
                        return Err(archive("two PAX headers describe one entry"));
                    }
                    pax = Some(parse_pax(&self.read_extension(header_size, MAX_PAX)?)?);
                    continue;
                }
                b'g' => {
                    // Global records are dropped, as container runtimes do.
                    parse_pax(&self.read_extension(header_size, MAX_PAX)?)?;
                    continue;
                }
                b'L' => {
                    if long_name.is_some() {
                        return Err(archive("two long names describe one entry"));
                    }
                    long_name = Some(trim_nul(self.read_extension(header_size, MAX_LONG_NAME)?));
                    continue;
                }
                b'K' => {
                    if long_link.is_some() {
                        return Err(archive("two long links describe one entry"));
                    }
                    long_link = Some(trim_nul(self.read_extension(header_size, MAX_LONG_NAME)?));
                    continue;
                }
                _ => {}
            }
            let pax = pax.take().unwrap_or_default();
            let entry = entry(header, pax, long_name.take(), long_link.take())?;
            self.remaining = size;
            self.padding = padding(size);
            return Ok(Some((entry, size)));
        }
    }

    /// The current entry's data.
    pub fn data(&mut self) -> Data<'_, R> {
        Data { reader: self }
    }

    /// After the end marker: reads to the end of the stream, which may
    /// hold at most `max` more bytes (a writer's zero padding, a
    /// compressor's trailer).
    pub fn finish(&mut self, max: u64) -> Result<(), Failure> {
        let mut buffer = [0u8; 8192];
        let mut extra: u64 = 0;
        loop {
            let count = self.read_inner(&mut buffer)?;
            if count == 0 {
                return Ok(());
            }
            extra = extra.saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
            if extra > max {
                return Err(archive("more data follows the archive's end marker"));
            }
        }
    }
}

/// The data of the current entry; reading past it gives the end of file.
pub struct Data<'a, R> {
    reader: &'a mut Reader<R>,
}

impl<R: Read> Data<'_, R> {
    /// Reads at most `buffer.len()` bytes of the entry's data; 0 at its
    /// end. A stream that ends inside the data is an error.
    pub fn read(&mut self, buffer: &mut [u8]) -> Result<usize, Failure> {
        if self.reader.remaining == 0 {
            return Ok(0);
        }
        let want = usize::try_from(self.reader.remaining)
            .unwrap_or(usize::MAX)
            .min(buffer.len());
        let count = self
            .reader
            .read_inner(buffer.get_mut(..want).unwrap_or_default())?;
        if count == 0 {
            return Err(archive("the archive ends inside an entry"));
        }
        self.reader.remaining -= u64::try_from(count).unwrap_or(self.reader.remaining);
        Ok(count)
    }
}

fn check_sum(block: &[u8; 512], header: &tar::Header) -> Result<(), Failure> {
    let stored = header
        .cksum()
        .map_err(|_| archive("a header's checksum is wrong"))?;
    let sum: u32 = block
        .iter()
        .enumerate()
        .map(|(index, &byte)| {
            if (148..156).contains(&index) {
                u32::from(b' ')
            } else {
                u32::from(byte)
            }
        })
        .sum();
    if sum == stored {
        Ok(())
    } else {
        Err(archive("a header's checksum is wrong"))
    }
}

fn trim_nul(mut bytes: Vec<u8>) -> Vec<u8> {
    while bytes.last() == Some(&0) {
        bytes.pop();
    }
    bytes
}

fn entry(
    header: &tar::Header,
    pax: Pax,
    long_name: Option<Vec<u8>>,
    long_link: Option<Vec<u8>>,
) -> Result<Entry, Failure> {
    let bad = |_| archive("a header field is invalid");
    let path = pax
        .path
        .or(long_name)
        .unwrap_or_else(|| header.path_bytes().into_owned());
    let link = pax
        .linkpath
        .or(long_link)
        .or_else(|| header.link_name_bytes().map(std::borrow::Cow::into_owned))
        .unwrap_or_default();
    let device = || -> Result<(u32, u32), Failure> {
        let major = header.device_major().map_err(bad)?.unwrap_or(0);
        let minor = header.device_minor().map_err(bad)?.unwrap_or(0);
        Ok((major, minor))
    };
    let kind = match header.entry_type().as_byte() {
        // An old-style regular file whose name ends in a slash is a
        // directory.
        b'\0' if path.ends_with(b"/") => Kind::Directory,
        b'0' | b'\0' | b'7' => Kind::File,
        b'1' => Kind::HardLink(link),
        b'2' => Kind::Symlink(link),
        b'3' => {
            let (major, minor) = device()?;
            Kind::CharDevice(major, minor)
        }
        b'4' => {
            let (major, minor) = device()?;
            Kind::BlockDevice(major, minor)
        }
        b'5' => Kind::Directory,
        b'6' => Kind::Fifo,
        b'S' => return Err(Failure::Unsupported("a sparse file")),
        _ => {
            return Err(Failure::Unsupported(
                "an entry type this init does not unpack",
            ));
        }
    };
    let mode = header.mode().map_err(bad)? & 0o7777;
    let uid = match pax.uid {
        Some(uid) => uid,
        None => header.uid().map_err(bad)?,
    };
    let gid = match pax.gid {
        Some(gid) => gid,
        None => header.gid().map_err(bad)?,
    };
    let mtime = match pax.mtime {
        Some(mtime) => mtime,
        None => (
            i64::try_from(header.mtime().map_err(bad)?).unwrap_or(i64::MAX),
            0,
        ),
    };
    Ok(Entry {
        path,
        kind,
        mode,
        uid,
        gid,
        mtime,
        xattrs: pax.xattrs,
    })
}

fn decimal(value: &[u8]) -> Option<u64> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(value).ok()?.parse().ok()
}

/// A PAX time: decimal seconds, optionally negative, with an optional
/// fraction.
fn pax_time(value: &[u8]) -> Option<(i64, u32)> {
    let (negative, rest) = match value.strip_prefix(b"-") {
        Some(rest) => (true, rest),
        None => (false, value),
    };
    let (whole, fraction) = match rest.iter().position(|&byte| byte == b'.') {
        Some(dot) => (rest.get(..dot)?, rest.get(dot + 1..)?),
        None => (rest, &[][..]),
    };
    let seconds = i64::try_from(decimal(whole)?).ok()?;
    if !fraction.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut nanoseconds: u32 = 0;
    for index in 0..9 {
        let digit = fraction
            .get(index)
            .map_or(0, |&byte| u32::from(byte - b'0'));
        nanoseconds = nanoseconds * 10 + digit;
    }
    if negative {
        // -1.25 is 1.25 s before the epoch: -2 s plus 0.75 s.
        if nanoseconds == 0 {
            Some((-seconds, 0))
        } else {
            Some((-seconds - 1, 1_000_000_000 - nanoseconds))
        }
    } else {
        Some((seconds, nanoseconds))
    }
}

/// Parses PAX records, `<length> <key>=<value>\n` each, the length
/// counting the whole record.
fn parse_pax(body: &[u8]) -> Result<Pax, Failure> {
    let malformed = || archive("a PAX header is malformed");
    let mut pax = Pax::default();
    let mut rest = body;
    while !rest.is_empty() {
        let space = rest
            .iter()
            .position(|&byte| byte == b' ')
            .ok_or_else(malformed)?;
        let length = decimal(rest.get(..space).ok_or_else(malformed)?).ok_or_else(malformed)?;
        let length = usize::try_from(length).map_err(|_| malformed())?;
        if length <= space + 1 || length > rest.len() {
            return Err(malformed());
        }
        let record = rest.get(space + 1..length).ok_or_else(malformed)?;
        rest = rest.get(length..).unwrap_or_default();
        let record = record.strip_suffix(b"\n").ok_or_else(malformed)?;
        let equals = record
            .iter()
            .position(|&byte| byte == b'=')
            .ok_or_else(malformed)?;
        let key = record.get(..equals).unwrap_or_default();
        let value = record.get(equals + 1..).unwrap_or_default();
        if key.is_empty() {
            return Err(malformed());
        }
        if key.starts_with(b"GNU.sparse.") {
            return Err(Failure::Unsupported("a sparse file"));
        }
        let bad_number = || archive("a PAX record's number is invalid");
        // An empty value removes the record's effect.
        let present = !value.is_empty();
        match key {
            b"path" => pax.path = present.then(|| value.to_vec()),
            b"linkpath" => pax.linkpath = present.then(|| value.to_vec()),
            b"size" if present => pax.size = Some(decimal(value).ok_or_else(bad_number)?),
            b"uid" if present => pax.uid = Some(decimal(value).ok_or_else(bad_number)?),
            b"gid" if present => pax.gid = Some(decimal(value).ok_or_else(bad_number)?),
            b"mtime" if present => pax.mtime = Some(pax_time(value).ok_or_else(bad_number)?),
            _ => {
                if let Some(name) = key.strip_prefix(b"SCHILY.xattr.") {
                    pax.xattrs.push((name.to_vec(), value.to_vec()));
                }
            }
        }
    }
    Ok(pax)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pax_times() {
        assert_eq!(pax_time(b"1700000000"), Some((1_700_000_000, 0)));
        assert_eq!(pax_time(b"1.5"), Some((1, 500_000_000)));
        assert_eq!(pax_time(b"1.123456789123"), Some((1, 123_456_789)));
        assert_eq!(pax_time(b"-1.25"), Some((-2, 750_000_000)));
        assert_eq!(pax_time(b"-3"), Some((-3, 0)));
        assert_eq!(pax_time(b"1.x"), None);
        assert_eq!(pax_time(b""), None);
        assert_eq!(pax_time(b"+1"), None);
    }

    #[test]
    fn pax_records() {
        let body =
            b"30 mtime=1700000000.000000001\n20 path=a/long/name\n27 SCHILY.xattr.user.k=v=w\n";
        let pax = parse_pax(body).unwrap();
        assert_eq!(pax.path.as_deref(), Some(&b"a/long/name"[..]));
        assert_eq!(pax.mtime, Some((1_700_000_000, 1)));
        assert_eq!(pax.xattrs, [(b"user.k".to_vec(), b"v=w".to_vec())]);
        for bad in [
            &b"5 a=b\n"[..],
            b"7 a=b\n",
            b"x a=b\n",
            b"6 a=bc",
            b"5 =b\n\n",
            b"9 size=x\n",
            b"99 path=a\n",
        ] {
            assert!(parse_pax(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
        assert!(matches!(
            parse_pax(b"22 GNU.sparse.major=1\n").map(drop),
            Err(Failure::Unsupported(_))
        ));
    }
}
