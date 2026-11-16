use std::collections::HashMap;
use std::os::fd::AsFd;

use anyhow::{Result, bail};

use crate::btree::*;
use crate::counters::Counters;
use crate::walk::{File, NO_PARENT, Node, Walk};

/// An indirect extent, from the reflink btree: data a reflinked file reads
/// through a reflink_p. It's refcounted as a whole, so it stays allocated -
/// all of it - while anything references any part of it. In sectors.
#[derive(Clone, Copy)]
pub struct Indirect {
    pub start: u64,
    pub len: u32,
    pub inline: bool,
}

impl Indirect {
    fn end(&self) -> u64 {
        self.start + self.len as u64
    }
}

pub fn read_indirect(fd: impl AsFd, c: &mut Counters) -> Result<Vec<Indirect>> {
    let mut out = Vec::new();
    for_each_key(fd, BTREE_REFLINK, 0, u64::MAX, |k| {
        let inline = match k.ty() {
            KEY_TYPE_REFLINK_V => false,
            KEY_TYPE_INDIRECT_INLINE_DATA | KEY_TYPE_INDIRECT_INLINE_DATA_V2 => true,
            _ => {
                c.reflink_other += 1;
                return Ok(());
            }
        };
        let e = Indirect {
            start: k.start(),
            len: k.size(),
            inline,
        };
        if let Some(prev) = out.last() {
            if e.start < Indirect::end(prev) {
                bail!(
                    "reflink btree: an extent at {}..{} overlaps the one before it",
                    e.start,
                    e.end()
                );
            }
        }
        out.push(e);
        Ok(())
    })?;
    c.indirect_extents = out.len() as u64;
    Ok(out)
}

pub const OUTSIDE: u32 = u32::MAX;

/// For each indirect extent, the files referencing it: `files[start[i]..start[i + 1]]`.
/// A file that isn't in the walk is OUTSIDE.
pub struct Holders {
    pub start: Vec<u32>,
    pub files: Vec<u32>,
}

struct Lookup<'a> {
    snapshots: &'a HashMap<u32, u64>,
    files: &'a HashMap<(u64, u64), u32>,
    last: (u32, u64, u32),
}

impl Lookup<'_> {
    fn file(&mut self, snapshot: u32, inode: u64) -> u32 {
        if (self.last.0, self.last.1) != (snapshot, inode) {
            let f = self
                .snapshots
                .get(&snapshot)
                .and_then(|&subvol| self.files.get(&(subvol, inode)))
                .copied()
                .unwrap_or(OUTSIDE);
            self.last = (snapshot, inode, f);
        }
        self.last.2
    }
}

const REFLINK_P_IDX_MASK: u64 = (1 << 56) - 1;

struct ScanPart {
    direct: Vec<(u32, u64)>,
    holds: Vec<(u32, u32)>,
    counters: Counters,
}

/// Reads the extents btree: each walked file's direct extents into
/// `files[].direct`, and which indirect extents every file holds.
/// `snapshots` maps a snapshot id to its subvolume.
pub fn read_extents(
    fd: impl AsFd + Sync,
    w: &mut Walk,
    snapshots: &HashMap<u32, u64>,
    indirect: &[Indirect],
    threads: usize,
) -> Result<Holders> {
    let by_inode: HashMap<(u64, u64), u32> = w
        .files
        .iter()
        .enumerate()
        .map(|(i, f)| ((w.nodes[f.dir as usize].subvol, f.inode), i as u32))
        .collect();

    // The btree is ordered by inode first: split it between the threads at
    // inode numbers that divide the walked files evenly.
    let mut inodes: Vec<u64> = w.files.iter().map(|f| f.inode).collect();
    inodes.sort_unstable();
    let mut bounds = vec![0u64];
    for t in 1..threads {
        let b = inodes
            .get(inodes.len() * t / threads)
            .copied()
            .unwrap_or(u64::MAX);
        if b > *bounds.last().unwrap() {
            bounds.push(b);
        }
    }

    let fd = fd.as_fd();
    let parts: Vec<Result<ScanPart>> = std::thread::scope(|s| {
        let handles: Vec<_> = bounds
            .iter()
            .enumerate()
            .map(|(i, &lo)| {
                let hi = bounds.get(i + 1).map_or(u64::MAX, |&b| b - 1);
                let by_inode = &by_inode;
                s.spawn(move || {
                    let mut p = ScanPart {
                        direct: Vec::new(),
                        holds: Vec::new(),
                        counters: Counters::default(),
                    };
                    let mut lookup = Lookup {
                        snapshots,
                        files: by_inode,
                        last: (u32::MAX, u64::MAX, OUTSIDE),
                    };
                    for_each_key(fd, BTREE_EXTENTS, lo, hi, |k| {
                        add_key(k, &mut lookup, snapshots, indirect, &mut p)
                    })?;
                    Ok(p)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut count = vec![0u32; indirect.len() + 1];
    let mut holds = Vec::new();
    for p in parts {
        let p = p?;
        for (file, bytes) in p.direct {
            w.files[file as usize].direct += bytes;
        }
        for &(extent, _) in &p.holds {
            count[extent as usize] += 1;
        }
        holds.push(p.holds);
        w.counters += &p.counters;
    }

    // a counting sort, by extent
    let mut start = Vec::with_capacity(count.len());
    let mut sum = 0u32;
    for n in &count {
        start.push(sum);
        sum = sum
            .checked_add(*n)
            .expect("more than 2^32 references to indirect extents");
    }
    let mut next = start.clone();
    let mut files = vec![0u32; sum as usize];
    for (extent, file) in holds.into_iter().flatten() {
        files[next[extent as usize] as usize] = file;
        next[extent as usize] += 1;
    }
    Ok(Holders { start, files })
}

fn add_key(
    k: &Key<'_>,
    lookup: &mut Lookup<'_>,
    snapshots: &HashMap<u32, u64>,
    indirect: &[Indirect],
    p: &mut ScanPart,
) -> Result<()> {
    let c = &mut p.counters;
    let bytes = (k.size() as u64) << 9;
    let file = lookup.file(k.snapshot(), k.inode());
    if file == OUTSIDE && !snapshots.contains_key(&k.snapshot()) {
        c.keys_unmapped_snapshot += 1;
    }

    match k.ty() {
        KEY_TYPE_EXTENT => {
            c.extents_direct += 1;
            if file == OUTSIDE {
                c.bytes_outside_direct += bytes;
            } else if let Some(last) = p.direct.last_mut().filter(|d| d.0 == file) {
                last.1 += bytes;
            } else {
                p.direct.push((file, bytes));
            }
        }
        KEY_TYPE_REFLINK_P => {
            c.extents_reflinked += 1;
            let v = k.val();
            if v.len() < 16 {
                bail!(
                    "extents btree: a reflink_p of {} value bytes at {}:{}",
                    v.len(),
                    k.inode(),
                    k.offset()
                );
            }
            let idx = u64::from_le_bytes(v[0..8].try_into().unwrap()) & REFLINK_P_IDX_MASK;
            let front_pad = u32::from_le_bytes(v[8..12].try_into().unwrap()) as u64;
            let back_pad = u32::from_le_bytes(v[12..16].try_into().unwrap()) as u64;
            // the pads are what it holds a reference on beyond what it reads
            let lo = idx.saturating_sub(front_pad);
            let hi = idx + k.size() as u64 + back_pad;

            let first = indirect.partition_point(|e| e.end() <= lo);
            let held = indirect[first..]
                .iter()
                .take_while(|e| e.start < hi)
                .count();
            if held == 0 {
                c.refs_dangling += 1;
            }
            p.holds
                .extend((first..first + held).map(|e| (e as u32, file)));
        }
        KEY_TYPE_RESERVATION => c.bytes_reserved += bytes,
        KEY_TYPE_INLINE_DATA | KEY_TYPE_INLINE_DATA_V2 => c.bytes_inline += bytes,
        _ => c.extents_other += 1,
    }
    Ok(())
}

/// Bytes charged to each folder: `total` once for every folder some file
/// reading the data is under, `exclusive` only where every such file is.
pub struct Attribution {
    pub total: Vec<u64>,
    pub exclusive: Vec<u64>,
    /// Indirect extents only files outside the walk reference.
    pub outside_only: u64,
    /// Indirect extents nothing references.
    pub unreferenced: u64,
}

struct Tree {
    parent: Vec<u32>,
    depth: Vec<u32>,
}

impl Tree {
    fn lca(&self, mut a: u32, mut b: u32) -> u32 {
        while self.depth[a as usize] > self.depth[b as usize] {
            a = self.parent[a as usize];
        }
        while self.depth[b as usize] > self.depth[a as usize] {
            b = self.parent[b as usize];
        }
        while a != b {
            a = self.parent[a as usize];
            b = self.parent[b as usize];
        }
        a
    }
}

struct Sweep<'a> {
    tree: &'a Tree,
    files: &'a [File],
    links: &'a HashMap<u32, Vec<u32>>,
    att: Attribution,
    stamp: Vec<u32>,
    epoch: u32,
    dirs: Vec<u32>,
}

impl<'a> Sweep<'a> {
    fn new(tree: &'a Tree, files: &'a [File], links: &'a HashMap<u32, Vec<u32>>) -> Self {
        let n = tree.parent.len();
        Sweep {
            tree,
            files,
            links,
            att: Attribution {
                total: vec![0; n],
                exclusive: vec![0; n],
                outside_only: 0,
                unreferenced: 0,
            },
            stamp: vec![0; n],
            epoch: 0,
            dirs: Vec::new(),
        }
    }

    fn add_file_dirs(&mut self, file: u32) {
        self.dirs.push(self.files[file as usize].dir);
        if let Some(more) = self.links.get(&file) {
            self.dirs.extend(more);
        }
    }

    // self.dirs: the folders of the files reading some data; `outside`: a file
    // outside the walk reads it too
    fn charge(&mut self, bytes: u64, outside: bool) {
        self.dirs.sort_unstable();
        self.dirs.dedup();

        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 {
            self.stamp.fill(0);
            self.epoch = 1;
        }
        for i in 0..self.dirs.len() {
            let mut n = self.dirs[i];
            while n != NO_PARENT && self.stamp[n as usize] != self.epoch {
                self.stamp[n as usize] = self.epoch;
                self.att.total[n as usize] += bytes;
                n = self.tree.parent[n as usize];
            }
        }

        if outside {
            return;
        }
        let mut n = self.dirs[1..]
            .iter()
            .fold(self.dirs[0], |a, &b| self.tree.lca(a, b));
        while n != NO_PARENT {
            debug_assert_eq!(self.stamp[n as usize], self.epoch);
            self.att.exclusive[n as usize] += bytes;
            n = self.tree.parent[n as usize];
        }
    }

    fn extent(&mut self, e: &Indirect, holders: &[u32]) {
        let bytes = (e.len as u64) << 9;
        if holders.is_empty() {
            self.att.unreferenced += bytes;
            return;
        }
        self.dirs.clear();
        let mut outside = false;
        for &f in holders {
            if f == OUTSIDE {
                outside = true;
            } else {
                self.add_file_dirs(f);
            }
        }
        if self.dirs.is_empty() {
            self.att.outside_only += bytes;
        } else {
            self.charge(bytes, outside);
        }
    }
}

/// Charges the indirect extents to the folders of the files holding them, and
/// a hardlinked file's direct extents to each folder it has a name in.
/// Inline indirect extents are btree metadata, not user data, and skipped.
pub fn attribute(
    w: &Walk,
    indirect: &[Indirect],
    holders: &Holders,
    threads: usize,
) -> Attribution {
    let tree = Tree {
        parent: w.nodes.iter().map(|n: &Node| n.parent).collect(),
        depth: w.nodes.iter().map(|n| n.depth).collect(),
    };
    let chunk = indirect.len().div_ceil(threads.max(1)).max(1);

    let parts: Vec<Attribution> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..indirect.len())
            .step_by(chunk)
            .map(|lo| {
                let tree = &tree;
                s.spawn(move || {
                    let mut sw = Sweep::new(tree, &w.files, &w.links);
                    let end = (lo + chunk).min(indirect.len());
                    let starts = holders.start[lo..=end].windows(2);
                    for (e, s) in indirect[lo..end].iter().zip(starts) {
                        if !e.inline {
                            sw.extent(e, &holders.files[s[0] as usize..s[1] as usize]);
                        }
                    }
                    sw.att
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut sw = Sweep::new(&tree, &w.files, &w.links);
    for &file in w.links.keys() {
        sw.dirs.clear();
        sw.add_file_dirs(file);
        sw.charge(w.files[file as usize].direct, false);
    }
    let mut att = sw.att;
    for p in parts {
        for (a, b) in att.total.iter_mut().zip(p.total) {
            *a += b;
        }
        for (a, b) in att.exclusive.iter_mut().zip(p.exclusive) {
            *a += b;
        }
        att.outside_only += p.outside_only;
        att.unreferenced += p.unreferenced;
    }
    att
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    // root 0 ─┬─ a 1 ─── a/x 3
    //         └─ b 2
    fn walk(files: &[u32], links: &[(u32, u32)]) -> Walk {
        let n = |parent, depth| Node {
            name: OsString::new(),
            parent,
            depth,
            subvol: 0,
        };
        let mut l: HashMap<u32, Vec<u32>> = HashMap::new();
        for &(f, d) in links {
            l.entry(f).or_default().push(d);
        }
        Walk {
            nodes: vec![n(NO_PARENT, 0), n(0, 1), n(0, 1), n(1, 2)],
            files: files
                .iter()
                .map(|&dir| File {
                    dir,
                    inode: 0,
                    apparent: 0,
                    direct: 8 << 9,
                })
                .collect(),
            links: l,
            counters: Counters::default(),
        }
    }

    fn extents(n: usize) -> Vec<Indirect> {
        (0..n as u64)
            .map(|i| Indirect {
                start: i * 100,
                len: 10,
                inline: false,
            })
            .collect()
    }

    fn holders(per_extent: &[&[u32]]) -> Holders {
        let mut start = vec![0];
        let mut files = Vec::new();
        for h in per_extent {
            files.extend_from_slice(h);
            start.push(files.len() as u32);
        }
        Holders { start, files }
    }

    fn sectors(v: &[u64]) -> Vec<u64> {
        v.iter().map(|b| b / 512).collect()
    }

    #[test]
    fn extent_held_from_two_folders_is_shared_by_both() {
        let w = walk(&[1, 2], &[]);
        let att = attribute(&w, &extents(1), &holders(&[&[0, 1]]), 1);
        assert_eq!(sectors(&att.total), [10, 10, 10, 0]);
        assert_eq!(sectors(&att.exclusive), [10, 0, 0, 0]);
    }

    #[test]
    fn nested_folders_share_within_their_parent() {
        let w = walk(&[1, 3], &[]);
        let att = attribute(&w, &extents(1), &holders(&[&[0, 1, 1]]), 1);
        assert_eq!(sectors(&att.total), [10, 10, 0, 10]);
        assert_eq!(sectors(&att.exclusive), [10, 10, 0, 0]);
    }

    #[test]
    fn held_from_outside_is_never_exclusive() {
        let w = walk(&[3], &[]);
        let att = attribute(&w, &extents(2), &holders(&[&[0, OUTSIDE], &[OUTSIDE]]), 1);
        assert_eq!(sectors(&att.total), [10, 10, 0, 10]);
        assert_eq!(sectors(&att.exclusive), [0, 0, 0, 0]);
        assert_eq!(att.outside_only / 512, 10);
    }

    #[test]
    fn unreferenced_and_inline_extents() {
        let w = walk(&[1], &[]);
        let mut ext = extents(3);
        ext[2].inline = true;
        let att = attribute(&w, &ext, &holders(&[&[0], &[], &[0]]), 1);
        assert_eq!(sectors(&att.total), [10, 10, 0, 0]);
        assert_eq!(att.unreferenced / 512, 10);
    }

    #[test]
    fn hardlinked_file_is_shared_by_its_folders() {
        let w = walk(&[1], &[(0, 2)]);
        let att = attribute(&w, &extents(1), &holders(&[&[0]]), 1);
        assert_eq!(sectors(&att.total), [18, 18, 18, 0]);
        assert_eq!(sectors(&att.exclusive), [18, 0, 0, 0]);
    }

    #[test]
    fn threads_agree() {
        let w = walk(&[1, 2, 3], &[(1, 3)]);
        let per: Vec<Vec<u32>> = (0..500u32)
            .map(|i| (0..i % 4).map(|j| (i + j) % 3).collect())
            .collect();
        let per: Vec<&[u32]> = per.iter().map(|v| &v[..]).collect();
        let (e, h) = (extents(500), holders(&per));
        let (a, b) = (attribute(&w, &e, &h, 1), attribute(&w, &e, &h, 7));
        assert_eq!(
            (a.total, a.exclusive, a.unreferenced),
            (b.total, b.exclusive, b.unreferenced)
        );
    }
}
