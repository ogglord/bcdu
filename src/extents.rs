use std::collections::HashMap;

use crate::counters::Counters;
use crate::fiemap::Mapping;
use crate::walk::{File, NO_PARENT, Node};

/// A file's reference to a slice of a shared (reflinked) extent.
///
/// FIEMAP gives each pointer's offset on its device, not which device: two
/// different extents can sit at the same offset on different devices. So an
/// extent is told apart by `sig`, a hash of where its other pointers are
/// relative to the first - unchanged when a file references only a slice,
/// which moves every pointer by the same amount. `start` and `len` are the
/// first pointer's, in sectors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SharedRef {
    pub sig:   u64,
    pub start: u64,
    pub len:   u32,
    pub file:  u32,
}

// sig of a slice seen with a single pointer: bcachefs reports only the first
// pointer of the last mapping of a FIEMAP call, so it isn't known yet
const SIG_UNRESOLVED: u64 = 0;

fn mix(h: u64, v: u64) -> u64 {
    let mut h = (h ^ v).wrapping_mul(0xff51afd7ed558ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ceb9fe1a85ec53);
    h ^ (h >> 33)
}

fn nonzero(h: u64) -> u64 {
    if h == SIG_UNRESOLVED { 1 } else { h }
}

impl SharedRef {
    pub fn new(m: &Mapping<'_>, file: u32) -> Self {
        let first = m.ptrs[0].physical;
        let sig = if m.ptrs.len() == 1 {
            SIG_UNRESOLVED
        } else {
            nonzero(m.ptrs[1..].iter().fold(mix(0x6263_6475, m.ptrs.len() as u64), |h, p| {
                mix(h, p.physical.wrapping_sub(first))
            }))
        };
        SharedRef {
            sig,
            start: first >> 9,
            len: u32::try_from(m.length >> 9).expect("an extent of 2^32 sectors"),
            file,
        }
    }

    fn end(&self) -> u64 {
        self.start + self.len as u64
    }
}

/// Gives each single-pointer slice the sig of the extent whose first pointer
/// it lines up with: the same slice, else one containing it, else one
/// overlapping it - and only if exactly one extent does, as offsets on
/// different devices coincide. One without a match is keyed by its own offset
/// and length, so identical slices still count once.
pub fn resolve_single_ptr(refs: &mut [SharedRef], c: &mut Counters) {
    refs.sort_unstable_by_key(|r| r.start);
    let max_len = refs.iter().filter(|r| r.sig != SIG_UNRESOLVED).map(|r| r.len as u64).max().unwrap_or(0);

    let mut assign = Vec::new();
    let mut cands: Vec<(u8, u64)> = Vec::new();
    for (i, r) in refs.iter().enumerate().filter(|(_, r)| r.sig == SIG_UNRESOLVED) {
        cands.clear();
        let before = refs[..i].iter().rev().take_while(|o| o.start + max_len > r.start);
        let after = refs[i + 1..].iter().take_while(|o| o.start < r.end());
        for o in before.chain(after) {
            if o.sig == SIG_UNRESOLVED || o.start >= r.end() || o.end() <= r.start {
                continue;
            }
            let fit = if o.start == r.start && o.len == r.len {
                2
            } else if o.start <= r.start && o.end() >= r.end() {
                1
            } else {
                0
            };
            cands.push((fit, o.sig));
        }
        cands.sort_unstable_by(|a, b| b.cmp(a));
        cands.dedup();

        let sig = match cands[..] {
            [] => {
                c.single_ptr_unmatched += 1;
                c.bytes_single_ptr_unmatched += (r.len as u64) << 9;
                None
            }
            [(best, sig), ref rest @ ..] if rest.first().is_none_or(|&(fit, _)| fit < best) => Some(sig),
            _ => {
                c.single_ptr_ambiguous += 1;
                c.bytes_single_ptr_ambiguous += (r.len as u64) << 9;
                None
            }
        };
        if sig.is_some() {
            c.single_ptr_resolved += 1;
        }
        assign.push((i, sig.unwrap_or_else(|| nonzero(mix(mix(0x6c6f6e65, r.start), r.len as u64)))));
    }
    for (i, sig) in assign {
        refs[i].sig = sig;
    }
}

/// Bytes of shared extents charged to each folder: `total` once for every
/// folder some referencing file is under, `exclusive` only where every
/// referencing file is under it.
pub struct Attribution {
    pub total:     Vec<u64>,
    pub exclusive: Vec<u64>,
}

struct Tree {
    parent: Vec<u32>,
    depth:  Vec<u32>,
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
    tree:   &'a Tree,
    files:  &'a [File],
    links:  &'a HashMap<u32, Vec<u32>>,
    att:    Attribution,
    stamp:  Vec<u32>,
    epoch:  u32,
    events: Vec<(u64, bool, u32)>,
    active: Vec<u32>,
    dirs:   Vec<u32>,
}

impl<'a> Sweep<'a> {
    fn new(tree: &'a Tree, files: &'a [File], links: &'a HashMap<u32, Vec<u32>>, nodes: usize) -> Self {
        Sweep {
            tree,
            files,
            links,
            att: Attribution { total: vec![0; nodes], exclusive: vec![0; nodes] },
            stamp: vec![0; nodes],
            epoch: 0,
            events: Vec::new(),
            active: Vec::new(),
            dirs: Vec::new(),
        }
    }

    fn charge_one(&mut self, mut n: u32, bytes: u64) {
        while n != NO_PARENT {
            self.att.total[n as usize] += bytes;
            self.att.exclusive[n as usize] += bytes;
            n = self.tree.parent[n as usize];
        }
    }

    fn add_file_dirs(&mut self, file: u32) {
        self.dirs.push(self.files[file as usize].dir);
        if let Some(more) = self.links.get(&file) {
            self.dirs.extend(more);
        }
    }

    // self.dirs: the folders of the files referencing one stretch of an extent
    fn charge(&mut self, bytes: u64) {
        self.dirs.sort_unstable();
        self.dirs.dedup();
        if let [dir] = self.dirs[..] {
            self.charge_one(dir, bytes);
            return;
        }

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

        let mut n = self.dirs[1..].iter().fold(self.dirs[0], |a, &b| self.tree.lca(a, b));
        while n != NO_PARENT {
            debug_assert_eq!(self.stamp[n as usize], self.epoch);
            self.att.exclusive[n as usize] += bytes;
            n = self.tree.parent[n as usize];
        }
    }

    // refs: one extent's slices, sorted by start
    fn extent(&mut self, refs: &[SharedRef]) {
        if let [r] = refs {
            self.dirs.clear();
            self.add_file_dirs(r.file);
            self.charge((r.len as u64) << 9);
            return;
        }

        self.events.clear();
        for r in refs {
            self.events.push((r.start, true, r.file));
            self.events.push((r.end(), false, r.file));
        }
        // ends before starts at the same offset: touching slices don't overlap
        self.events.sort_unstable_by_key(|&(pos, start, _)| (pos, start));

        self.active.clear();
        let mut prev = self.events[0].0;
        for i in 0..self.events.len() {
            let (pos, start, file) = self.events[i];
            if pos > prev && !self.active.is_empty() {
                self.dirs.clear();
                for j in 0..self.active.len() {
                    self.add_file_dirs(self.active[j]);
                }
                self.charge((pos - prev) << 9);
            }
            prev = pos;
            if start {
                self.active.push(file);
            } else {
                let i = self.active.iter().position(|&f| f == file).expect("a slice ended that never started");
                self.active.swap_remove(i);
            }
        }
        assert!(self.active.is_empty(), "slices still open at the end of an extent");
    }
}

/// `refs` must be sorted by (sig, start). A hardlinked file's direct extents
/// are charged here too, as they're shared between its folders.
pub fn attribute(
    refs: &[SharedRef],
    files: &[File],
    links: &HashMap<u32, Vec<u32>>,
    nodes: &[Node],
    threads: usize,
) -> Attribution {
    let tree = Tree {
        parent: nodes.iter().map(|n| n.parent).collect(),
        depth:  nodes.iter().map(|n| n.depth).collect(),
    };
    debug_assert!(refs.windows(2).all(|w| (w[0].sig, w[0].start) <= (w[1].sig, w[1].start)));

    // chunks end on extent boundaries
    let mut bounds = vec![0];
    let step = refs.len().div_ceil(threads.max(1)).max(1);
    while let Some(&last) = bounds.last() {
        if last >= refs.len() {
            break;
        }
        let mut end = (last + step).min(refs.len());
        while end < refs.len() && refs[end].sig == refs[end - 1].sig {
            end += 1;
        }
        bounds.push(end);
    }

    let parts: Vec<Attribution> = std::thread::scope(|s| {
        let handles: Vec<_> = bounds
            .windows(2)
            .map(|w| {
                let (refs, tree) = (&refs[w[0]..w[1]], &tree);
                s.spawn(move || {
                    let mut sw = Sweep::new(tree, files, links, nodes.len());
                    for extent in refs.chunk_by(|a, b| a.sig == b.sig) {
                        sw.extent(extent);
                    }
                    sw.att
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut sw = Sweep::new(&tree, files, links, nodes.len());
    for &file in links.keys() {
        sw.dirs.clear();
        sw.add_file_dirs(file);
        sw.charge(files[file as usize].direct);
    }
    let mut att = sw.att;
    for p in parts {
        for (a, b) in att.total.iter_mut().zip(p.total) {
            *a += b;
        }
        for (a, b) in att.exclusive.iter_mut().zip(p.exclusive) {
            *a += b;
        }
    }
    att
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    // root 0 ─┬─ a 1 ─── a/x 3
    //         └─ b 2
    fn nodes() -> Vec<Node> {
        let n = |parent, depth| Node { name: OsString::new(), parent, depth, subvol: 0 };
        vec![n(NO_PARENT, 0), n(0, 1), n(0, 1), n(1, 2)]
    }

    fn file(dir: u32) -> File {
        File { dir, apparent: 0, direct: 0 }
    }

    fn r(sig: u64, start: u64, len: u32, file: u32) -> SharedRef {
        SharedRef { sig, start, len, file }
    }

    fn run(mut refs: Vec<SharedRef>, files: &[File], threads: usize) -> (Vec<u64>, Vec<u64>) {
        refs.sort_unstable_by_key(|r| (r.sig, r.start));
        let att = attribute(&refs, files, &HashMap::new(), &nodes(), threads);
        let s = |v: Vec<u64>| v.into_iter().map(|b| b / 512).collect();
        (s(att.total), s(att.exclusive))
    }

    #[test]
    fn one_extent_in_two_folders_is_shared_by_both() {
        let files = [file(1), file(2)];
        let (total, excl) = run(vec![r(7, 100, 10, 0), r(7, 100, 10, 1)], &files, 1);
        assert_eq!(total, [10, 10, 10, 0]);
        assert_eq!(excl, [10, 0, 0, 0]);
    }

    #[test]
    fn overlapping_slices_split_into_stretches() {
        // a/x has 100..110, b has 105..120: 105..110 is shared
        let files = [file(3), file(2)];
        let (total, excl) = run(vec![r(7, 100, 10, 0), r(7, 105, 15, 1)], &files, 1);
        assert_eq!(total, [20, 10, 15, 10]);
        assert_eq!(excl, [20, 5, 10, 5]);
    }

    #[test]
    fn same_offset_different_sig_is_a_different_extent() {
        let files = [file(1), file(2)];
        let (total, excl) = run(vec![r(7, 100, 10, 0), r(8, 100, 10, 1)], &files, 1);
        assert_eq!(total, [20, 10, 10, 0]);
        assert_eq!(excl, [20, 10, 10, 0]);
    }

    #[test]
    fn nested_folders_share_within_their_parent() {
        let files = [file(1), file(3)];
        let (total, excl) = run(vec![r(7, 0, 8, 0), r(7, 0, 8, 1)], &files, 1);
        assert_eq!(total, [8, 8, 0, 8]);
        assert_eq!(excl, [8, 8, 0, 0]);
    }

    #[test]
    fn threads_agree() {
        let files = [file(1), file(2), file(3)];
        let refs: Vec<_> = (0..300)
            .flat_map(|i| [r(i % 37 + 1, i * 3, 5, (i % 3) as u32), r(i % 37 + 1, i * 3 + 2, 4, ((i + 1) % 3) as u32)])
            .collect();
        assert_eq!(run(refs.clone(), &files, 1), run(refs, &files, 7));
    }

    #[test]
    fn single_pointer_slice_takes_the_extent_it_overlaps() {
        let mut c = Counters::default();
        let mut refs = vec![r(7, 100, 10, 0), r(SIG_UNRESOLVED, 104, 6, 1), r(SIG_UNRESOLVED, 500, 6, 2)];
        resolve_single_ptr(&mut refs, &mut c);
        refs.sort_unstable_by_key(|r| r.start);
        assert_eq!(refs[1].sig, 7);
        assert_ne!(refs[2].sig, SIG_UNRESOLVED);
        assert_eq!((c.single_ptr_resolved, c.single_ptr_unmatched, c.single_ptr_ambiguous), (1, 1, 0));
    }

    #[test]
    fn single_pointer_slice_prefers_the_extent_containing_it() {
        let mut c = Counters::default();
        let mut refs = vec![r(7, 100, 10, 0), r(8, 104, 3, 1), r(SIG_UNRESOLVED, 101, 4, 2)];
        resolve_single_ptr(&mut refs, &mut c);
        assert_eq!(refs.iter().find(|r| r.file == 2).unwrap().sig, 7);
        assert_eq!(c.single_ptr_resolved, 1);
    }

    #[test]
    fn single_pointer_slice_over_two_extents_is_ambiguous() {
        let mut c = Counters::default();
        let mut refs = vec![r(7, 100, 10, 0), r(8, 105, 10, 1), r(SIG_UNRESOLVED, 106, 2, 2)];
        resolve_single_ptr(&mut refs, &mut c);
        assert_eq!(c.single_ptr_ambiguous, 1);
    }
}
