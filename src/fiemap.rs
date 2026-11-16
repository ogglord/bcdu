use std::os::fd::BorrowedFd;

use rustix::io;
use rustix::ioctl::{Opcode, opcode};

use crate::ioctl::ioctl_buf;

pub const FIEMAP_EXTENT_LAST: u32 = 0x0001;
pub const FIEMAP_EXTENT_UNKNOWN: u32 = 0x0002;
pub const FIEMAP_EXTENT_DELALLOC: u32 = 0x0004;
pub const FIEMAP_EXTENT_ENCODED: u32 = 0x0008;
pub const FIEMAP_EXTENT_DATA_INLINE: u32 = 0x0200;
pub const FIEMAP_EXTENT_UNWRITTEN: u32 = 0x0800;
pub const FIEMAP_EXTENT_SHARED: u32 = 0x2000;

#[repr(C)]
#[derive(Default)]
struct FiemapHead {
    start:          u64,
    length:         u64,
    flags:          u32,
    mapped_extents: u32,
    extent_count:   u32,
    reserved:       u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct FiemapExtent {
    pub logical:  u64,
    pub physical: u64,
    pub length:   u64,
    reserved64:   [u64; 2],
    pub flags:    u32,
    reserved:     [u32; 3],
}

// The opcode encodes the size of struct fiemap's head only: the extents are
// its flexible array.
const FS_IOC_FIEMAP: Opcode = opcode::read_write::<FiemapHead>(b'f', 11);

const NR_EXTENTS: usize = 4096;

#[repr(C)]
struct FiemapBuf {
    head:    FiemapHead,
    extents: [FiemapExtent; NR_EXTENTS],
}

/// One extent of the file: bcachefs emits a record per pointer - each
/// replica, and the cached copy - with the same logical offset and length.
pub struct Mapping<'a> {
    pub length:  u64,
    pub flags:   u32,
    pub ptrs:    &'a [FiemapExtent],
}

pub struct FiemapReader {
    buf:         Box<FiemapBuf>,
    pub resumes: u64,
}

impl FiemapReader {
    pub fn new() -> Self {
        FiemapReader {
            buf:     Box::new(FiemapBuf {
                head:    FiemapHead::default(),
                extents: [FiemapExtent::default(); NR_EXTENTS],
            }),
            resumes: 0,
        }
    }

    pub fn read(&mut self, fd: BorrowedFd<'_>, mut f: impl FnMut(&Mapping<'_>)) -> io::Result<()> {
        let mut start = 0;
        loop {
            self.buf.head = FiemapHead {
                start,
                length: u64::MAX - start,
                extent_count: NR_EXTENTS as u32,
                ..Default::default()
            };
            unsafe { ioctl_buf::<FS_IOC_FIEMAP, _>(fd, &mut *self.buf)? };

            let n = self.buf.head.mapped_extents as usize;
            assert!(n <= NR_EXTENTS, "FIEMAP mapped {n} extents into a buffer of {NR_EXTENTS}");
            match for_each_mapping(&self.buf.extents[..n], NR_EXTENTS, &mut f) {
                Some(next) => {
                    assert!(next > start, "FIEMAP resume at {next} doesn't advance past {start}");
                    start = next;
                    self.resumes += 1;
                }
                None => return Ok(()),
            }
        }
    }
}

fn same_mapping(a: &FiemapExtent, b: &FiemapExtent) -> bool {
    a.logical == b.logical && a.length == b.length
}

/// Calls `f` for each whole mapping in one FIEMAP reply, and returns where to
/// resume when the reply filled the buffer and the file goes on.
///
/// When the buffer fills, its last mapping may be missing pointers that
/// didn't fit; it's left for the next call, which starts at its offset.
fn for_each_mapping(
    exts: &[FiemapExtent],
    capacity: usize,
    f: &mut impl FnMut(&Mapping<'_>),
) -> Option<u64> {
    let Some(last) = exts.last() else { return None };
    let complete = last.flags & FIEMAP_EXTENT_LAST != 0 || exts.len() < capacity;

    let mut end = exts.len();
    if !complete {
        while end > 0 && same_mapping(&exts[end - 1], last) {
            end -= 1;
        }
        assert!(end > 0, "one mapping at {} filled a whole FIEMAP buffer", last.logical);
    }

    let mut i = 0;
    while i < end {
        let mut j = i + 1;
        while j < end && same_mapping(&exts[j], &exts[i]) {
            j += 1;
        }
        f(&Mapping {
            length:  exts[i].length,
            flags:   exts[i].flags,
            ptrs:    &exts[i..j],
        });
        i = j;
    }

    (!complete).then(|| exts[end].logical)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ext(logical: u64, physical: u64, length: u64, flags: u32) -> FiemapExtent {
        FiemapExtent { logical, physical, length, flags, ..Default::default() }
    }

    fn collect(exts: &[FiemapExtent], capacity: usize) -> (Vec<(u64, usize)>, Option<u64>) {
        let mut seen = Vec::new();
        let resume = for_each_mapping(exts, capacity, &mut |m| seen.push((m.ptrs[0].logical, m.ptrs.len())));
        (seen, resume)
    }

    #[test]
    fn groups_pointers_of_one_mapping() {
        let exts = [
            ext(0, 100, 10, 0),
            ext(0, 900, 10, 0),
            ext(10, 110, 5, 0),
            ext(10, 910, 5, 0),
            ext(10, 50, 5, 0),
            ext(15, 120, 5, FIEMAP_EXTENT_LAST),
        ];
        assert_eq!(collect(&exts, 4096), (vec![(0, 2), (10, 3), (15, 1)], None));
    }

    #[test]
    fn full_buffer_resumes_at_the_cut_mapping() {
        let exts = [ext(0, 100, 10, 0), ext(0, 900, 10, 0), ext(10, 110, 5, 0), ext(10, 910, 5, 0)];
        assert_eq!(collect(&exts, 4), (vec![(0, 2)], Some(10)));
    }

    #[test]
    fn short_reply_is_complete_without_last() {
        let exts = [ext(0, 100, 10, 0), ext(0, 900, 10, 0)];
        assert_eq!(collect(&exts, 4), (vec![(0, 2)], None));
    }

    #[test]
    fn empty_reply() {
        assert_eq!(collect(&[], 4), (vec![], None));
    }
}
