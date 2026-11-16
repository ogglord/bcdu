use std::collections::BTreeMap;
use std::os::fd::AsFd;

use anyhow::{Context, Result, anyhow, bail};
use rustix::io::Errno;
use rustix::ioctl::{Opcode, opcode};

use crate::btree::for_each_packed_key;
use crate::ioctl::ioctl_buf;

const BCACHEFS_SUPER_MAGIC: u64 = 0xca451a4e;

pub fn is_bcachefs(fd: impl AsFd) -> Result<bool> {
    Ok(rustix::fs::fstatfs(fd)?.f_type as u64 == BCACHEFS_SUPER_MAGIC)
}

#[repr(C)]
#[derive(Default)]
struct QueryAccounting {
    capacity: u64,
    used: u64,
    online_reserved: u64,
    accounting_u64s: u32,
    accounting_types_mask: u32,
}

const BCH_IOCTL_QUERY_ACCOUNTING: Opcode = opcode::write::<QueryAccounting>(0xbc, 21);

const DISK_ACCOUNTING_REPLICAS: u8 = 2;
const DATA_TYPE_USER: u8 = 4;

const BPOS_BYTES: usize = 20;

/// User data by how many devices hold it: the sectors bcachefs has written
/// for each replicas entry, summed over entries with the same device count.
pub struct UserData {
    pub by_nr_devs: BTreeMap<u8, u64>,
}

impl UserData {
    pub fn raw_bytes(&self) -> u64 {
        self.by_nr_devs.values().sum::<u64>() << 9
    }

    pub fn one_copy_bytes(&self) -> u64 {
        self.by_nr_devs
            .iter()
            .map(|(&nr, &s)| s / nr as u64)
            .sum::<u64>()
            << 9
    }

    /// The device count, when all user data has the same.
    pub fn replicas(&self) -> Option<u8> {
        match self.by_nr_devs.keys().collect::<Vec<_>>()[..] {
            [&nr] => Some(nr),
            _ => None,
        }
    }
}

// An accounting position is a 20 byte big endian integer overlaid on bpos;
// on a little endian host the bpos bytes are reversed. Kernels before
// disk_accounting_big_endian wrote it the other way round.
fn decode_replicas(bpos: &[u8]) -> Option<(u8, u8)> {
    let mut acc: [u8; BPOS_BYTES] = bpos.try_into().ok()?;
    if cfg!(target_endian = "little") {
        acc.reverse();
    }
    let valid = |a: &[u8; BPOS_BYTES]| a[0] == DISK_ACCOUNTING_REPLICAS && (1..=16).contains(&a[2]);
    if !valid(&acc) {
        acc.reverse();
        if !valid(&acc) {
            return None;
        }
    }
    Some((acc[1], acc[2]))
}

pub fn user_data(fd: impl AsFd) -> Result<UserData> {
    let fd = fd.as_fd();
    let head_u64s = size_of::<QueryAccounting>() / 8;
    let mut cap = 1usize << 10;
    let reply = loop {
        let mut buf = vec![0u64; head_u64s + cap];
        let head = buf.as_mut_ptr().cast::<QueryAccounting>();
        unsafe {
            *head = QueryAccounting {
                accounting_u64s: cap as u32,
                accounting_types_mask: 1 << DISK_ACCOUNTING_REPLICAS,
                ..Default::default()
            };
        }
        match unsafe { ioctl_buf::<BCH_IOCTL_QUERY_ACCOUNTING, _>(fd, head) } {
            Ok(()) => {
                let used = unsafe { (*head).accounting_u64s } as usize;
                if used > cap {
                    bail!("QUERY_ACCOUNTING: the kernel says it used {used} u64s of {cap}");
                }
                buf.truncate(head_u64s + used);
                buf.drain(..head_u64s);
                break buf;
            }
            Err(Errno::RANGE) if cap < 1 << 27 => cap *= 4,
            Err(e) => return Err(anyhow!(e)).context("QUERY_ACCOUNTING"),
        }
    };

    let mut by_nr_devs = BTreeMap::new();
    let mut undecoded = None;
    let bytes = unsafe { std::slice::from_raw_parts(reply.as_ptr().cast::<u8>(), reply.len() * 8) };
    for_each_packed_key(bytes, "QUERY_ACCOUNTING reply", |k| {
        let sectors = k
            .val()
            .get(..8)
            .map(|v| u64::from_le_bytes(v.try_into().unwrap()));
        match (decode_replicas(k.pos_bytes()), sectors) {
            (Some((DATA_TYPE_USER, nr_devs)), Some(sectors)) => {
                *by_nr_devs.entry(nr_devs).or_insert(0) += sectors
            }
            (Some(_), Some(_)) => {}
            _ => undecoded = Some(k.raw.to_vec()),
        }
        Ok(())
    })?;
    if let Some(raw) = undecoded {
        bail!("QUERY_ACCOUNTING reply: a replicas key that doesn't decode: {raw:02x?}");
    }
    Ok(UserData { by_nr_devs })
}

#[repr(C)]
#[derive(Default)]
struct SnapshotTreeQuery {
    tree_id: u32,
    master_subvol: u32,
    root_snapshot: u32,
    nr: u32,
    total: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SnapshotNode {
    id: u32,
    parent: u32,
    children: [u32; 2],
    subvol: u32,
    flags: u32,
    pad: [u32; 2],
    sectors: u64,
}

const BCH_IOCTL_SNAPSHOT_TREE: Opcode = opcode::read_write::<SnapshotTreeQuery>(0xbc, 33);

pub struct SubvolSnapshot {
    pub id: u32,
    /// What bcachefs accounts to the subvolume (all replicas), summed up its
    /// snapshot's ancestors as `bcachefs subvolume list` does. Extents that
    /// were ever reflinked aren't in it: those belong to no snapshot.
    pub sectors: u64,
}

/// `fd` is a directory in the subvolume.
pub fn subvol_snapshot(fd: impl AsFd, subvol: u64) -> Result<SubvolSnapshot> {
    let fd = fd.as_fd();
    let mut cap = 16usize;
    let nodes = loop {
        let words = (size_of::<SnapshotTreeQuery>() + cap * size_of::<SnapshotNode>()).div_ceil(8);
        let mut buf = vec![0u64; words];
        let head = buf.as_mut_ptr().cast::<SnapshotTreeQuery>();
        unsafe { (*head).nr = cap as u32 };
        match unsafe { ioctl_buf::<BCH_IOCTL_SNAPSHOT_TREE, _>(fd, head) } {
            Ok(()) => {
                let nr = unsafe { (*head).nr } as usize;
                assert!(
                    nr <= cap,
                    "SNAPSHOT_TREE returned {nr} nodes into room for {cap}"
                );
                let first = unsafe { head.add(1).cast::<SnapshotNode>() };
                break unsafe { std::slice::from_raw_parts(first, nr) }.to_vec();
            }
            Err(Errno::RANGE) => cap = (unsafe { (*head).total } as usize).max(cap * 2),
            Err(e) => return Err(anyhow!(e)).context("SNAPSHOT_TREE"),
        }
    };

    let leaf = nodes
        .iter()
        .find(|n| n.subvol as u64 == subvol)
        .ok_or_else(|| anyhow!("SNAPSHOT_TREE: no snapshot for subvolume {subvol} in its tree"))?;
    let mut sectors = 0;
    let mut cur = Some(leaf);
    while let Some(n) = cur {
        sectors += n.sectors;
        cur = (n.parent != 0)
            .then(|| nodes.iter().find(|p| p.id == n.parent))
            .flatten();
    }
    Ok(SubvolSnapshot {
        id: leaf.id,
        sectors,
    })
}
