use std::os::fd::AsFd;

use anyhow::{Context, Result, anyhow, bail};
use rustix::io::Errno;
use rustix::ioctl::{Opcode, opcode};

use crate::ioctl::ioctl_buf;

pub const BTREE_EXTENTS: u32 = 0;
pub const BTREE_REFLINK: u32 = 7;

pub const KEY_TYPE_EXTENT: u8 = 6;
pub const KEY_TYPE_RESERVATION: u8 = 7;
pub const KEY_TYPE_REFLINK_P: u8 = 15;
pub const KEY_TYPE_REFLINK_V: u8 = 16;
pub const KEY_TYPE_INLINE_DATA: u8 = 17;
pub const KEY_TYPE_INDIRECT_INLINE_DATA: u8 = 19;
pub const KEY_TYPE_INLINE_DATA_V2: u8 = 40;
pub const KEY_TYPE_INDIRECT_INLINE_DATA_V2: u8 = 41;

#[repr(C, packed(4))]
#[derive(Clone, Copy, Default)]
struct Bpos {
    snapshot: u32,
    offset: u64,
    inode: u64,
}

const POS_MAX: Bpos = Bpos {
    snapshot: u32::MAX,
    offset: u64::MAX,
    inode: u64::MAX,
};

#[repr(C)]
#[derive(Default)]
struct QueryBtreeKeys {
    btree: u32,
    level: u32,
    flags: u32,
    done: u32,
    start: Bpos,
    end: Bpos,
    buf: u64,
    buf_size: u32,
    used: u32,
}

const BCH_IOCTL_QUERY_BTREE_KEYS: Opcode = opcode::read_write::<QueryBtreeKeys>(0xbc, 34);
const QUERY_BTREE_KEYS_ALL_SNAPSHOTS: u32 = 1 << 2;

// struct bkey, unpacked, little endian: u64s, format, type, pad,
// bversion (12 bytes), size, then bpos as snapshot, offset, inode
pub const BKEY_U64S: usize = 5;

pub struct Key<'a> {
    pub raw: &'a [u8],
}

fn le<const N: usize>(b: &[u8]) -> [u8; N] {
    b[..N].try_into().unwrap()
}

impl<'a> Key<'a> {
    pub fn ty(&self) -> u8 {
        self.raw[2]
    }

    pub fn size(&self) -> u32 {
        u32::from_le_bytes(le(&self.raw[16..]))
    }

    pub fn snapshot(&self) -> u32 {
        u32::from_le_bytes(le(&self.raw[20..]))
    }

    /// The end of an extent: extents are indexed by where they end.
    pub fn offset(&self) -> u64 {
        u64::from_le_bytes(le(&self.raw[24..]))
    }

    pub fn inode(&self) -> u64 {
        u64::from_le_bytes(le(&self.raw[32..]))
    }

    pub fn pos_bytes(&self) -> &'a [u8] {
        &self.raw[20..40]
    }

    pub fn val(&self) -> &'a [u8] {
        &self.raw[BKEY_U64S * 8..]
    }

    pub fn start(&self) -> u64 {
        self.offset() - self.size() as u64
    }
}

/// Densely packed struct bkey_i, as the query ioctls return them.
pub fn for_each_packed_key(
    mut b: &[u8],
    what: &str,
    mut f: impl FnMut(&Key<'_>) -> Result<()>,
) -> Result<()> {
    while !b.is_empty() {
        let len = b[0] as usize * 8;
        if len < BKEY_U64S * 8 || len > b.len() {
            bail!("{what}: a key of {len} bytes with {} left", b.len());
        }
        f(&Key { raw: &b[..len] })?;
        b = &b[len..];
    }
    Ok(())
}

/// The keys of inodes `lo..=hi` in a btree, from every snapshot, in btree
/// order.
pub fn for_each_key(
    fd: impl AsFd,
    btree: u32,
    lo: u64,
    hi: u64,
    mut f: impl FnMut(&Key<'_>) -> Result<()>,
) -> Result<()> {
    if cfg!(target_endian = "big") {
        bail!("reading btree keys is only implemented for little endian hosts");
    }
    let fd = fd.as_fd();
    let mut buf = vec![0u64; 1 << 17];
    let mut q = QueryBtreeKeys {
        btree,
        flags: QUERY_BTREE_KEYS_ALL_SNAPSHOTS,
        start: Bpos {
            snapshot: 0,
            offset: 0,
            inode: lo,
        },
        end: Bpos {
            inode: hi,
            ..POS_MAX
        },
        ..Default::default()
    };
    loop {
        q.buf = buf.as_mut_ptr() as u64;
        q.buf_size = (buf.len() * 8) as u32;
        q.used = 0;
        match unsafe { ioctl_buf::<BCH_IOCTL_QUERY_BTREE_KEYS, _>(fd, &mut q) } {
            Ok(()) => {}
            Err(Errno::NOTTY) => {
                return Err(anyhow!(
                    "this kernel's bcachefs has no BCH_IOCTL_QUERY_BTREE_KEYS, which bcdu reads extents with"
                ));
            }
            Err(Errno::PERM | Errno::ACCESS) => {
                return Err(anyhow!(
                    "reading btree {btree}: permission denied - bcdu needs root"
                ));
            }
            Err(e) => return Err(anyhow!(e)).with_context(|| format!("reading btree {btree}")),
        }

        let used = q.used as usize;
        assert!(
            used <= buf.len() * 8,
            "QUERY_BTREE_KEYS used {used} bytes of {}",
            buf.len() * 8
        );
        let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), used) };
        for_each_packed_key(bytes, "QUERY_BTREE_KEYS reply", &mut f)?;
        if q.done != 0 {
            return Ok(());
        }
        if used == 0 {
            bail!("reading btree {btree}: the kernel returned no keys and isn't done");
        }
    }
}
