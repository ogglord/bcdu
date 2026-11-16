use std::collections::HashMap;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

use anyhow::{Context, Result, bail};
use log::warn;
use rustix::fs::{AtFlags, CWD, Dir, FileType, Mode, OFlags, StatxFlags, openat, statx};

use crate::counters::Counters;

pub const NO_PARENT: u32 = u32::MAX;

// STATX_SUBVOL, Linux 6.10: rustix has the field but not the flag
const STATX_SUBVOL: u32 = 0x8000;

pub struct Node {
    pub name: OsString,
    pub parent: u32,
    pub depth: u32,
    pub subvol: u64,
}

pub struct File {
    pub dir: u32,
    pub inode: u64,
    pub apparent: u64,
    /// Bytes of extents only this file references, filled in from the
    /// extents btree.
    pub direct: u64,
}

pub struct Walk {
    pub nodes: Vec<Node>,
    pub files: Vec<File>,
    /// A hardlinked file's other folders: its data is shared between every
    /// folder it has a name in.
    pub links: HashMap<u32, Vec<u32>>,
    pub counters: Counters,
}

// Files of a directory are handed out in batches so one huge directory is
// spread over all the threads.
const BATCH: usize = 256;

struct DirCtx {
    fd: OwnedFd,
    path: PathBuf,
    node: u32,
}

enum Job {
    Dir {
        path: PathBuf,
        node: u32,
    },
    Files {
        dir: Arc<DirCtx>,
        names: Vec<CString>,
    },
}

struct Queue {
    jobs: Vec<Job>,
    busy: usize,
}

struct Shared {
    queue: Mutex<Queue>,
    wake: Condvar,
    nodes: Mutex<Vec<Node>>,
    dev: (u32, u32),
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

#[derive(Default)]
struct Found {
    files: Vec<File>,
    counters: Counters,
}

struct Worker<'a> {
    shared: &'a Shared,
    found: Found,
}

pub fn walk(root: &Path, threads: usize) -> Result<Walk> {
    let st = statx(CWD, root, AtFlags::empty(), StatxFlags::TYPE)
        .with_context(|| format!("stat '{}'", root.display()))?;
    if FileType::from_raw_mode(st.stx_mode.into()) != FileType::Directory {
        bail!("'{}' is not a directory", root.display());
    }

    let shared = Shared {
        queue: Mutex::new(Queue {
            jobs: vec![Job::Dir {
                path: root.to_owned(),
                node: 0,
            }],
            busy: 0,
        }),
        wake: Condvar::new(),
        nodes: Mutex::new(vec![Node {
            name: root.as_os_str().to_owned(),
            parent: NO_PARENT,
            depth: 0,
            subvol: 0,
        }]),
        dev: (st.stx_dev_major, st.stx_dev_minor),
    };

    let results: Vec<Found> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| {
                    let mut w = Worker {
                        shared: &shared,
                        found: Found::default(),
                    };
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
                    w.found
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let nodes = shared.nodes.into_inner().unwrap();
    let mut files = Vec::with_capacity(results.iter().map(|f| f.files.len()).sum());
    let mut counters = Counters::default();
    for f in results {
        files.extend(f.files);
        counters += &f.counters;
    }

    // A hardlinked file is listed once per name; keep the first, and
    // remember the folders of the rest.
    let mut first: HashMap<(u64, u64), u32> = HashMap::with_capacity(files.len());
    let mut links: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut kept = Vec::with_capacity(files.len());
    for f in files {
        let key = (nodes[f.dir as usize].subvol, f.inode);
        match first.get(&key) {
            Some(&i) => {
                links.entry(i).or_default().push(f.dir);
                counters.hardlinks_skipped += 1;
            }
            None => {
                first.insert(key, kept.len() as u32);
                kept.push(f);
            }
        }
    }
    u32::try_from(kept.len()).expect("more than 2^32 files");
    counters.files = kept.len() as u64;

    Ok(Walk {
        nodes,
        files: kept,
        links,
        counters,
    })
}

impl Worker<'_> {
    fn walk_dir(&mut self, path: PathBuf, node: u32) {
        let c = &mut self.found.counters;
        let fd = match openat(
            CWD,
            &path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(e) => {
                warn!(
                    "opening directory '{}': {e} (skipped, counted under errors)",
                    path.display()
                );
                c.err_readdir += 1;
                return;
            }
        };

        let st = match statx(
            &fd,
            c"",
            AtFlags::EMPTY_PATH,
            StatxFlags::from_bits_retain(STATX_SUBVOL),
        ) {
            Ok(st) => st,
            Err(e) => {
                warn!(
                    "stat '{}': {e} (skipped, counted under errors)",
                    path.display()
                );
                c.err_stat += 1;
                return;
            }
        };
        if (st.stx_dev_major, st.stx_dev_minor) != self.shared.dev {
            c.other_fs += 1;
            return;
        }
        self.shared.nodes.lock().unwrap()[node as usize].subvol = if st.stx_mask & STATX_SUBVOL != 0
        {
            st.stx_subvol
        } else {
            0
        };
        c.dirs += 1;

        let mut dir = match Dir::read_from(&fd) {
            Ok(dir) => dir,
            Err(e) => {
                warn!(
                    "reading directory '{}': {e} (skipped, counted under errors)",
                    path.display()
                );
                c.err_readdir += 1;
                return;
            }
        };
        let mut names = Vec::new();
        while let Some(ent) = dir.read() {
            let ent = match ent {
                Ok(ent) => ent,
                Err(e) => {
                    warn!(
                        "reading directory '{}': {e} (the rest of it skipped, counted under errors)",
                        path.display()
                    );
                    c.err_readdir += 1;
                    break;
                }
            };
            let name = ent.file_name();
            if name == c"." || name == c".." {
                continue;
            }
            match ent.file_type() {
                FileType::Directory => {
                    let child = self.shared.add_node(name, node);
                    let path = path.join(OsStr::from_bytes(name.to_bytes()));
                    self.shared.push(Job::Dir { path, node: child });
                }
                // walk_file() stats it, and skips what isn't a file
                FileType::RegularFile | FileType::Unknown => names.push(name.to_owned()),
                FileType::Symlink => c.symlinks += 1,
                _ => c.special += 1,
            }
        }
        drop(dir);

        let dir = Arc::new(DirCtx { fd, path, node });
        let mut batches = names.chunks(BATCH);
        let first = batches.next();
        for batch in batches {
            self.shared.push(Job::Files {
                dir: dir.clone(),
                names: batch.to_vec(),
            });
        }
        for name in first.into_iter().flatten() {
            self.walk_file(&dir, name);
        }
    }

    fn walk_file(&mut self, dir: &DirCtx, name: &CStr) {
        let c = &mut self.found.counters;
        let st = match statx(
            &dir.fd,
            name,
            AtFlags::SYMLINK_NOFOLLOW,
            StatxFlags::TYPE | StatxFlags::INO | StatxFlags::SIZE,
        ) {
            Ok(st) => st,
            Err(e) => {
                warn!(
                    "stat '{}': {e} (skipped, counted under errors)",
                    dir.path.join(OsStr::from_bytes(name.to_bytes())).display()
                );
                c.err_stat += 1;
                return;
            }
        };
        match FileType::from_raw_mode(st.stx_mode.into()) {
            FileType::RegularFile => {}
            FileType::Directory => {
                // d_type was unknown, and it's a directory after all
                let child = self.shared.add_node(name, dir.node);
                let path = dir.path.join(OsStr::from_bytes(name.to_bytes()));
                self.shared.push(Job::Dir { path, node: child });
                return;
            }
            FileType::Symlink => {
                c.symlinks += 1;
                return;
            }
            _ => {
                c.special += 1;
                return;
            }
        }
        self.found.files.push(File {
            dir: dir.node,
            inode: st.stx_ino,
            apparent: st.stx_size,
            direct: 0,
        });
    }
}
