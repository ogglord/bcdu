use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString, OsStr, OsString};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

use anyhow::{Context, Result, bail};
use log::warn;
use rustix::fs::{AtFlags, CWD, Dir, FileType, Mode, OFlags, StatxFlags, fstat, openat, statx};
use rustix::io::Errno;

use crate::counters::Counters;
use crate::extents::SharedRef;
use crate::fiemap::*;

pub const NO_PARENT: u32 = u32::MAX;

// STATX_SUBVOL, Linux 6.10: rustix has the field but not the flag
const STATX_SUBVOL: u32 = 0x8000;

pub struct Node {
    pub name:   OsString,
    pub parent: u32,
    pub depth:  u32,
    pub subvol: u64,
}

pub struct File {
    pub dir:      u32,
    pub apparent: u64,
    pub direct:   u64,
}

pub struct Walk {
    pub nodes:    Vec<Node>,
    pub files:    Vec<File>,
    pub refs:     Vec<SharedRef>,
    /// A hardlinked file's other folders: all of its extents are shared
    /// between every folder it has a name in.
    pub links:    HashMap<u32, Vec<u32>>,
    pub counters: Counters,
}

// Files of a directory are handed out in batches so one huge directory is
// spread over all the threads.
const BATCH: usize = 64;

struct DirCtx {
    fd:     OwnedFd,
    path:   PathBuf,
    node:   u32,
    subvol: u64,
}

enum Job {
    Dir { path: PathBuf, node: u32 },
    Files { dir: Arc<DirCtx>, names: Vec<CString> },
}

struct Queue {
    jobs: Vec<Job>,
    busy: usize,
}

struct Shared {
    queue:     Mutex<Queue>,
    wake:      Condvar,
    nodes:     Mutex<Vec<Node>>,
    hardlinks: Mutex<HashSet<InodeKey>>,
    dev:       (u32, u32),
}

impl Shared {
    fn push(&self, job: Job) {
        self.queue.lock().unwrap().jobs.push(job);
        self.wake.notify_one();
    }

    fn pop(&self) -> Option<Job> {
        let mut q = self.queue.lock().unwrap();
        loop {
            if let Some(job) = q.jobs.pop() {
                q.busy += 1;
                return Some(job);
            }
            if q.busy == 0 {
                return None;
            }
            q = self.wake.wait(q).unwrap();
        }
    }

    fn done(&self) {
        let mut q = self.queue.lock().unwrap();
        q.busy -= 1;
        if q.busy == 0 && q.jobs.is_empty() {
            self.wake.notify_all();
        }
    }

    fn add_node(&self, name: &CStr, parent: u32) -> u32 {
        let mut nodes = self.nodes.lock().unwrap();
        let depth = nodes[parent as usize].depth + 1;
        nodes.push(Node {
            name: OsStr::from_bytes(name.to_bytes()).to_owned(),
            parent,
            depth,
            subvol: 0,
        });
        (nodes.len() - 1) as u32
    }
}

type InodeKey = (u64, u64);

#[derive(Default)]
struct Found {
    files:       Vec<File>,
    refs:        Vec<SharedRef>,
    first_names: Vec<(InodeKey, u32)>,
    other_names: Vec<(InodeKey, u32)>,
    counters:    Counters,
}

struct Worker<'a> {
    shared: &'a Shared,
    reader: FiemapReader,
    found:  Found,
}

pub fn walk(root: &Path, threads: usize) -> Result<Walk> {
    let st = statx(CWD, root, AtFlags::empty(), StatxFlags::TYPE)
        .with_context(|| format!("stat '{}'", root.display()))?;
    if FileType::from_raw_mode(st.stx_mode.into()) != FileType::Directory {
        bail!("'{}' is not a directory", root.display());
    }

    let shared = Shared {
        queue:     Mutex::new(Queue {
            jobs: vec![Job::Dir { path: root.to_owned(), node: 0 }],
            busy: 0,
        }),
        wake:      Condvar::new(),
        nodes:     Mutex::new(vec![Node {
            name:   root.as_os_str().to_owned(),
            parent: NO_PARENT,
            depth:  0,
            subvol: 0,
        }]),
        hardlinks: Mutex::new(HashSet::new()),
        dev:       (st.stx_dev_major, st.stx_dev_minor),
    };

    let results: Vec<Found> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| {
                    let mut w = Worker { shared: &shared, reader: FiemapReader::new(), found: Found::default() };
                    while let Some(job) = shared.pop() {
                        match job {
                            Job::Dir { path, node } => w.walk_dir(path, node),
                            Job::Files { dir, names } => {
                                for name in &names {
                                    w.walk_file(&dir, name);
                                }
                            }
                        }
                        shared.done();
                    }
                    w.found.counters.fiemap_resumes = w.reader.resumes;
                    w.found
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut out = Walk {
        nodes:    shared.nodes.into_inner().unwrap(),
        files:    Vec::new(),
        refs:     Vec::new(),
        links:    HashMap::new(),
        counters: Counters::default(),
    };
    out.files.reserve(results.iter().map(|f| f.files.len()).sum());
    out.refs.reserve(results.iter().map(|f| f.refs.len()).sum());
    let mut first_names = HashMap::new();
    let mut other_names = Vec::new();
    for f in results {
        let offset = u32::try_from(out.files.len()).expect("more than 2^32 files");
        out.files.extend(f.files);
        out.refs.extend(f.refs.into_iter().map(|r| SharedRef { file: r.file + offset, ..r }));
        first_names.extend(f.first_names.into_iter().map(|(k, file)| (k, file + offset)));
        other_names.extend(f.other_names);
        out.counters += &f.counters;
    }
    // a name whose first was never read (it failed to open) has nothing to share
    for (k, dir) in other_names {
        if let Some(&file) = first_names.get(&k) {
            out.links.entry(file).or_default().push(dir);
        }
    }
    Ok(out)
}

fn open_noatime(dirfd: impl AsFd, name: &CStr, flags: OFlags) -> rustix::io::Result<OwnedFd> {
    // O_NOATIME is refused with EPERM on files we don't own, without
    // CAP_FOWNER: fall back to an ordinary open
    match openat(&dirfd, name, flags | OFlags::NOATIME, Mode::empty()) {
        Err(Errno::PERM) => openat(&dirfd, name, flags, Mode::empty()),
        r => r,
    }
}

impl Worker<'_> {
    fn walk_dir(&mut self, path: PathBuf, node: u32) {
        let fd = match openat(
            CWD,
            &path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(e) => {
                warn!("opening directory '{}': {e} (skipped, counted under errors)", path.display());
                self.found.counters.err_readdir += 1;
                return;
            }
        };

        let st = match statx(&fd, c"", AtFlags::EMPTY_PATH, StatxFlags::from_bits_retain(STATX_SUBVOL)) {
            Ok(st) => st,
            Err(e) => {
                warn!("stat '{}': {e} (skipped, counted under errors)", path.display());
                self.found.counters.err_stat += 1;
                return;
            }
        };
        if (st.stx_dev_major, st.stx_dev_minor) != self.shared.dev {
            self.found.counters.special += 1;
            return;
        }
        let subvol = if st.stx_mask & STATX_SUBVOL != 0 { st.stx_subvol } else { 0 };
        self.shared.nodes.lock().unwrap()[node as usize].subvol = subvol;
        self.found.counters.dirs += 1;

        let mut names = Vec::new();
        let mut dir = match Dir::read_from(&fd) {
            Ok(dir) => dir,
            Err(e) => {
                warn!("reading directory '{}': {e} (skipped, counted under errors)", path.display());
                self.found.counters.err_readdir += 1;
                return;
            }
        };
        while let Some(ent) = dir.read() {
            let ent = match ent {
                Ok(ent) => ent,
                Err(e) => {
                    warn!(
                        "reading directory '{}': {e} (the rest of it skipped, counted under errors)",
                        path.display()
                    );
                    self.found.counters.err_readdir += 1;
                    break;
                }
            };
            let name = ent.file_name();
            if name == c"." || name == c".." {
                continue;
            }
            let ft = match ent.file_type() {
                FileType::Unknown => match statx(&fd, name, AtFlags::SYMLINK_NOFOLLOW, StatxFlags::TYPE) {
                    Ok(st) => FileType::from_raw_mode(st.stx_mode.into()),
                    Err(e) => {
                        warn!(
                            "stat '{}': {e} (skipped, counted under errors)",
                            path.join(OsStr::from_bytes(name.to_bytes())).display()
                        );
                        self.found.counters.err_stat += 1;
                        continue;
                    }
                },
                ft => ft,
            };
            match ft {
                FileType::Directory => {
                    let child = self.shared.add_node(name, node);
                    let path = path.join(OsStr::from_bytes(name.to_bytes()));
                    self.shared.push(Job::Dir { path, node: child });
                }
                FileType::RegularFile => names.push(name.to_owned()),
                FileType::Symlink => self.found.counters.symlinks += 1,
                _ => self.found.counters.special += 1,
            }
        }
        drop(dir);

        let dir = Arc::new(DirCtx { fd, path, node, subvol });
        let mut batches = names.chunks(BATCH);
        let first = batches.next();
        for batch in batches {
            self.shared.push(Job::Files { dir: dir.clone(), names: batch.to_vec() });
        }
        for name in first.into_iter().flatten() {
            self.walk_file(&dir, name);
        }
    }

    fn walk_file(&mut self, dir: &DirCtx, name: &CStr) {
        let path = || dir.path.join(OsStr::from_bytes(name.to_bytes()));

        let fd = match open_noatime(
            &dir.fd,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        ) {
            Ok(fd) => fd,
            Err(e) => {
                warn!("opening '{}': {e} (skipped, counted under errors)", path().display());
                self.found.counters.err_open += 1;
                return;
            }
        };
        let st = match fstat(&fd) {
            Ok(st) => st,
            Err(e) => {
                warn!("stat '{}': {e} (skipped, counted under errors)", path().display());
                self.found.counters.err_stat += 1;
                return;
            }
        };
        if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile {
            // replaced since readdir
            self.found.counters.special += 1;
            return;
        }
        let inode = (st.st_nlink > 1).then_some((dir.subvol, st.st_ino));
        if let Some(k) = inode {
            if !self.shared.hardlinks.lock().unwrap().insert(k) {
                self.found.other_names.push((k, dir.node));
                self.found.counters.hardlinks_skipped += 1;
                return;
            }
        }
        self.found.counters.files += 1;

        let file = u32::try_from(self.found.files.len()).expect("more than 2^32 files in one thread");
        let mut direct = 0;
        let Worker { reader, found: Found { refs, counters, .. }, .. } = self;
        let r = reader.read(fd.as_fd(), |m| {
            add_mapping(m, file, &mut direct, refs, counters);
        });
        if let Err(e) = r {
            warn!(
                "reading the extents of '{}': {e} (its extents so far counted, counted under errors)",
                path().display()
            );
            self.found.counters.err_fiemap += 1;
        }

        if let Some(k) = inode {
            self.found.first_names.push((k, file));
        }
        self.found.files.push(File {
            dir: dir.node,
            apparent: st.st_size as u64,
            direct,
        });
    }
}

fn add_mapping(m: &Mapping<'_>, file: u32, direct: &mut u64, refs: &mut Vec<SharedRef>, c: &mut Counters) {
    let f = m.flags;
    if f & FIEMAP_EXTENT_DATA_INLINE != 0 {
        c.mappings_inline += 1;
        c.bytes_inline += m.length;
    } else if f & FIEMAP_EXTENT_DELALLOC != 0 {
        if f & FIEMAP_EXTENT_UNWRITTEN != 0 {
            c.mappings_reserved += 1;
            c.bytes_reserved += m.length;
        } else {
            c.mappings_delalloc += 1;
            c.bytes_delalloc += m.length;
        }
    } else if f & FIEMAP_EXTENT_UNKNOWN != 0 {
        c.mappings_unknown += 1;
    } else {
        if f & FIEMAP_EXTENT_UNWRITTEN != 0 {
            c.mappings_unwritten += 1;
        }
        if f & FIEMAP_EXTENT_ENCODED != 0 {
            c.mappings_encoded += 1;
            c.bytes_encoded += m.length;
        }
        let aligned = m.length % 512 == 0 && m.ptrs.iter().all(|p| p.physical % 512 == 0);
        if f & FIEMAP_EXTENT_SHARED != 0 && aligned {
            c.mappings_shared += 1;
            if m.ptrs.len() == 1 {
                c.shared_single_ptr += 1;
            }
            refs.push(SharedRef::new(m, file));
        } else {
            if !aligned {
                c.mappings_unaligned += 1;
            }
            c.mappings_direct += 1;
            *direct += m.length;
        }
    }
}
