use std::collections::HashSet;
use std::fmt::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::counters::Counters;
use crate::extents::Attribution;
use crate::walk::{NO_PARENT, Walk};

#[derive(Serialize)]
pub struct Folder {
    pub path:      PathBuf,
    pub depth:     u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subvolume: Option<u64>,
    pub files:     u64,
    pub total:     u64,
    pub exclusive: u64,
    pub shared:    u64,
    pub apparent:  u64,
}

#[derive(Serialize)]
pub struct Filesystem {
    pub user_data_one_copy: u64,
    pub user_data_raw:      u64,
    pub replicas:           Option<u8>,
    pub walked_from_root:   bool,
    pub found:              u64,
}

#[derive(Serialize)]
pub struct Check {
    pub subvolume:        u64,
    pub path:             PathBuf,
    pub bcachefs_bytes:   u64,
    pub bcdu_direct:      u64,
    pub matches:          bool,
}

#[derive(Serialize)]
pub struct Report {
    pub folders:    Vec<Folder>,
    pub filesystem: Option<Filesystem>,
    pub check:      Option<Vec<Check>>,
    pub counters:   Counters,
}

pub struct Usage {
    pub direct:    Vec<u64>,
    pub total:     Vec<u64>,
    pub exclusive: Vec<u64>,
    pub apparent:  Vec<u64>,
    pub files:     Vec<u64>,
}

impl Usage {
    pub fn new(w: &Walk, att: &Attribution) -> Self {
        let n = w.nodes.len();
        let mut u = Usage {
            direct:    vec![0; n],
            total:     vec![0; n],
            exclusive: vec![0; n],
            apparent:  vec![0; n],
            files:     vec![0; n],
        };
        // a hardlinked file's extents are charged to its folders by
        // attribute(); its size goes to each folder it has a name in, below
        for (i, f) in w.files.iter().enumerate() {
            if !w.links.contains_key(&(i as u32)) {
                u.direct[f.dir as usize] += f.direct;
                u.apparent[f.dir as usize] += f.apparent;
                u.files[f.dir as usize] += 1;
            }
        }
        // a child is always added after its parent, so has a higher id
        for i in (1..n).rev() {
            let p = w.nodes[i].parent as usize;
            assert!(p < i, "node {i}'s parent {p} was added after it");
            u.direct[p] += u.direct[i];
            u.apparent[p] += u.apparent[i];
            u.files[p] += u.files[i];
        }
        for (&file, more) in &w.links {
            let f = &w.files[file as usize];
            let mut seen = HashSet::new();
            for &dir in std::iter::once(&f.dir).chain(more) {
                let mut n = dir;
                while n != NO_PARENT && seen.insert(n) {
                    u.apparent[n as usize] += f.apparent;
                    u.files[n as usize] += 1;
                    n = w.nodes[n as usize].parent;
                }
            }
        }
        for i in 0..n {
            u.total[i] = u.direct[i] + att.total[i];
            u.exclusive[i] = u.direct[i] + att.exclusive[i];
            assert!(u.exclusive[i] <= u.total[i], "node {i}: exclusive {} > total {}", u.exclusive[i], u.total[i]);
        }
        u
    }
}

pub fn node_path(w: &Walk, mut n: u32) -> PathBuf {
    let mut names = Vec::new();
    while w.nodes[n as usize].parent != NO_PARENT {
        names.push(&w.nodes[n as usize].name);
        n = w.nodes[n as usize].parent;
    }
    let mut path = PathBuf::from(&w.nodes[n as usize].name);
    path.extend(names.into_iter().rev());
    path
}

pub fn is_subvol_root(w: &Walk, n: u32) -> bool {
    let node = &w.nodes[n as usize];
    node.subvol != 0 && (node.parent == NO_PARENT || w.nodes[node.parent as usize].subvol != node.subvol)
}

/// The tree to `max_depth`, biggest first among siblings.
pub fn folders(w: &Walk, u: &Usage, max_depth: u32) -> Vec<Folder> {
    let mut children = vec![Vec::new(); w.nodes.len()];
    for (i, node) in w.nodes.iter().enumerate().skip(1) {
        if node.depth <= max_depth {
            children[node.parent as usize].push(i as u32);
        }
    }
    for c in &mut children {
        c.sort_unstable_by_key(|&i| std::cmp::Reverse(u.total[i as usize]));
    }

    let mut out = Vec::new();
    let mut stack = vec![0u32];
    while let Some(n) = stack.pop() {
        let i = n as usize;
        out.push(Folder {
            path:      node_path(w, n),
            depth:     w.nodes[i].depth,
            subvolume: is_subvol_root(w, n).then_some(w.nodes[i].subvol),
            files:     u.files[i],
            total:     u.total[i],
            exclusive: u.exclusive[i],
            shared:    u.total[i] - u.exclusive[i],
            apparent:  u.apparent[i],
        });
        stack.extend(children[i].iter().rev());
    }
    out
}

pub fn bytes_to_text(out: &mut String, bytes: u64, human: bool) {
    if !human {
        write!(out, "{bytes}").unwrap();
        return;
    }
    const UNITS: [&str; 7] = ["B", "K", "M", "G", "T", "P", "E"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    match (u, v) {
        (0, _) => write!(out, "{bytes}B"),
        (_, v) if v < 10.0 => write!(out, "{v:.2}{}", UNITS[u]),
        (_, v) if v < 100.0 => write!(out, "{v:.1}{}", UNITS[u]),
        (_, v) => write!(out, "{v:.0}{}", UNITS[u]),
    }
    .unwrap();
}

fn bytes(b: u64, human: bool) -> String {
    let mut s = String::new();
    bytes_to_text(&mut s, b, human);
    s
}

pub fn report_to_text(out: &mut String, r: &Report, root: &Path, human: bool, verbose: bool) {
    let rows: Vec<[String; 4]> = r
        .folders
        .iter()
        .map(|f| {
            [bytes(f.total, human), bytes(f.exclusive, human), bytes(f.shared, human), bytes(f.apparent, human)]
        })
        .collect();
    let header = ["total", "exclusive", "shared", "apparent"];
    let width: Vec<usize> =
        (0..4).map(|c| rows.iter().map(|r| r[c].len()).chain([header[c].len()]).max().unwrap()).collect();

    for (c, h) in header.iter().enumerate() {
        write!(out, "{h:>w$}  ", w = width[c]).unwrap();
    }
    writeln!(out, "path").unwrap();
    for (f, row) in r.folders.iter().zip(&rows) {
        for (c, v) in row.iter().enumerate() {
            write!(out, "{v:>w$}  ", w = width[c]).unwrap();
        }
        let name = if f.depth == 0 { f.path.as_path() } else { f.path.strip_prefix(root).unwrap_or(&f.path) };
        write!(out, "{:indent$}{}", "", name.display(), indent = 2 * f.depth as usize).unwrap();
        if let Some(s) = f.subvolume {
            write!(out, "  [subvolume {s}]").unwrap();
        }
        writeln!(out).unwrap();
    }

    if let Some(fs) = &r.filesystem {
        writeln!(out).unwrap();
        let label = |out: &mut String, l: &str, b: u64| {
            writeln!(out, "{l:<36} {}", bytes(b, human)).unwrap();
        };
        label(out, "bcachefs user data, one copy:", fs.user_data_one_copy);
        if fs.walked_from_root {
            label(out, "  found by this walk:", fs.found);
            let missing = fs.user_data_one_copy.saturating_sub(fs.found);
            write!(out, "{:<36} {}", "  referenced by no file:", bytes(missing, human)).unwrap();
            if fs.user_data_one_copy > 0 {
                write!(out, " ({:.2}%)", 100.0 * missing as f64 / fs.user_data_one_copy as f64).unwrap();
            }
            writeln!(out).unwrap();
        } else {
            writeln!(out, "  (this walk covers part of the filesystem, not compared)").unwrap();
        }
    }

    if let Some(check) = &r.check {
        writeln!(out, "\nsubvolume direct extents, bcachefs vs bcdu:").unwrap();
        for c in check {
            writeln!(
                out,
                "  {:<6} {:<30} {:>16} {:>16}  {}",
                c.subvolume,
                c.path.display(),
                bytes(c.bcachefs_bytes, human),
                bytes(c.bcdu_direct, human),
                if c.matches { "ok" } else { "DIFFERS" }
            )
            .unwrap();
        }
    }

    if r.counters.errors() > 0 {
        writeln!(out, "\n{} errors, see the warnings above", r.counters.errors()).unwrap();
    }
    if r.counters.mappings_encoded > 0 {
        writeln!(
            out,
            "\nnote: {} compressed extents; sizes are uncompressed",
            r.counters.mappings_encoded
        )
        .unwrap();
    }
    if verbose {
        writeln!(out, "\ncounters:").unwrap();
        r.counters.to_text(out);
    }
}
