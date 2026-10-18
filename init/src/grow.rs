//! Step 3's growth of the root: an ext4 file system smaller than its disk
//! (a disk made larger than the image's file system, or resized since) is
//! grown online to the disk's size. Only ever grown, never shrunk; a
//! failure is logged and the boot goes on with the space unused.

use crate::console::log;
use crate::sys::disk;
use std::fs;
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;

/// Where ext4's superblock starts on the device, and the bytes read.
const SUPERBLOCK_OFFSET: u64 = 1024;
const SUPERBLOCK_LEN: usize = 1024;
const EXT4_MAGIC: u16 = 0xEF53;
/// `EXT4_FEATURE_INCOMPAT_64BIT`: the block count's high half is valid.
const INCOMPAT_64BIT: u32 = 0x80;
/// The largest block size ext4 has (64 KiB, `s_log_block_size` 6).
const MAX_LOG_BLOCK_SIZE: u32 = 6;

/// What the growth needs from an ext4 superblock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Superblock {
    pub block_size: u64,
    pub blocks: u64,
}

fn le_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let field = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes(field.try_into().ok()?))
}

fn le_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let field = bytes.get(offset..offset.checked_add(2)?)?;
    Some(u16::from_le_bytes(field.try_into().ok()?))
}

impl Superblock {
    /// Parses the superblock's first 1024 bytes, or says why not.
    pub fn parse(bytes: &[u8]) -> Result<Superblock, &'static str> {
        let short = "the superblock is short";
        if le_u16(bytes, 0x38).ok_or(short)? != EXT4_MAGIC {
            return Err("the disk holds no ext4 superblock");
        }
        let log_block_size = le_u32(bytes, 0x18).ok_or(short)?;
        if log_block_size > MAX_LOG_BLOCK_SIZE {
            return Err("the superblock's block size is out of range");
        }
        let low = u64::from(le_u32(bytes, 0x04).ok_or(short)?);
        let incompat = le_u32(bytes, 0x60).ok_or(short)?;
        let high = if incompat & INCOMPAT_64BIT == 0 {
            0
        } else {
            u64::from(le_u32(bytes, 0x150).ok_or(short)?)
        };
        Ok(Superblock {
            block_size: 1024 << log_block_size,
            blocks: high << 32 | low,
        })
    }
}

/// The block count to grow to: the device's whole blocks, when that is
/// more than the file system has; `None` when there is nothing to grow
/// (or the device is smaller, which is never shrunk to).
pub fn grown_blocks(device_bytes: u64, block_size: u64, blocks: u64) -> Option<u64> {
    let whole = device_bytes.checked_div(block_size)?;
    (whole > blocks).then_some(whole)
}

/// Grows the mounted root on `disk` to the disk's size when it is larger,
/// logging a failure instead of failing the boot.
pub fn root(disk: &str) {
    if let Err(error) = grow(disk) {
        log(&format!("growing the root file system failed: {error}"));
    }
}

fn grow(disk: &str) -> Result<(), String> {
    let device = fs::File::open(disk).map_err(|error| format!("opening {disk}: {error}"))?;
    let device_bytes = disk::block_device_bytes(device.as_fd())
        .map_err(|error| format!("reading the disk's size: {error}"))?;
    let mut bytes = [0u8; SUPERBLOCK_LEN];
    device
        .read_exact_at(&mut bytes, SUPERBLOCK_OFFSET)
        .map_err(|error| format!("reading the superblock: {error}"))?;
    let superblock = Superblock::parse(&bytes)?;
    let Some(blocks) = grown_blocks(device_bytes, superblock.block_size, superblock.blocks) else {
        return Ok(());
    };
    let root = fs::File::open("/").map_err(|error| format!("opening /: {error}"))?;
    disk::resize_ext4(root.as_fd(), blocks).map_err(|error| {
        // The kernel's own reason is a warning the quiet console hides.
        let hint = if error.raw_os_error() == Some(libc::EPERM) {
            " (the file system has errors: run e2fsck on the disk)"
        } else {
            ""
        };
        format!(
            "from {} to {blocks} blocks: {error}{hint}",
            superblock.blocks
        )
    })?;
    log(&format!(
        "grew the root file system from {} to {blocks} blocks",
        superblock.blocks
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn superblock(log_block_size: u32, low: u32, high: u32, incompat: u32) -> Vec<u8> {
        let mut bytes = vec![0u8; SUPERBLOCK_LEN];
        bytes[0x04..0x08].copy_from_slice(&low.to_le_bytes());
        bytes[0x18..0x1c].copy_from_slice(&log_block_size.to_le_bytes());
        bytes[0x38..0x3a].copy_from_slice(&EXT4_MAGIC.to_le_bytes());
        bytes[0x60..0x64].copy_from_slice(&incompat.to_le_bytes());
        bytes[0x150..0x154].copy_from_slice(&high.to_le_bytes());
        bytes
    }

    #[test]
    fn grows_only_to_whole_larger_counts() {
        const GIB: u64 = 1 << 30;
        // 1 GiB of 4 KiB blocks on an 8 GiB disk.
        assert_eq!(grown_blocks(8 * GIB, 4096, 262_144), Some(2_097_152));
        // A partial last block is left out.
        assert_eq!(grown_blocks(8 * GIB + 4095, 4096, 262_144), Some(2_097_152));
        // Already the disk's size, or less than a block more: nothing.
        assert_eq!(grown_blocks(GIB, 4096, 262_144), None);
        assert_eq!(grown_blocks(GIB + 4095, 4096, 262_144), None);
        // A disk smaller than the file system is never shrunk to.
        assert_eq!(grown_blocks(GIB / 2, 4096, 262_144), None);
        // 1 KiB blocks, and a block size of zero.
        assert_eq!(grown_blocks(2 * GIB, 1024, 1_048_576), Some(2_097_152));
        assert_eq!(grown_blocks(2 * GIB, 0, 1), None);
        // Past 2^32 blocks.
        assert_eq!(grown_blocks(u64::MAX, 4096, 1), Some(u64::MAX / 4096));
    }

    #[test]
    fn parses_the_superblock() {
        let parsed = Superblock::parse(&superblock(2, 262_144, 0, 0)).unwrap();
        assert_eq!(
            parsed,
            Superblock {
                block_size: 4096,
                blocks: 262_144
            }
        );
        // The high half counts only with the 64bit feature.
        let parsed = Superblock::parse(&superblock(2, 5, 1, INCOMPAT_64BIT)).unwrap();
        assert_eq!(parsed.blocks, (1 << 32) + 5);
        let parsed = Superblock::parse(&superblock(2, 5, 1, 0)).unwrap();
        assert_eq!(parsed.blocks, 5);
        let parsed = Superblock::parse(&superblock(0, 5, 0, 0)).unwrap();
        assert_eq!(parsed.block_size, 1024);
    }

    #[test]
    fn refuses_what_is_not_ext4() {
        let mut bytes = superblock(2, 1, 0, 0);
        bytes[0x38] = 0;
        assert!(Superblock::parse(&bytes).is_err());
        assert!(Superblock::parse(&superblock(7, 1, 0, 0)).is_err());
        assert!(Superblock::parse(&superblock(2, 1, 0, INCOMPAT_64BIT)[..0x100]).is_err());
        assert!(Superblock::parse(&superblock(2, 1, 0, 0)[..0x40]).is_err());
        assert!(Superblock::parse(&[]).is_err());
    }
}
