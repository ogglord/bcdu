mod bcachefs;
mod btree;
mod counters;
mod extents;
mod ioctl;
mod report;
mod walk;

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::Parser;
use log::info;
use rustix::fs::{CWD, Mode, OFlags, openat};

use counters::Counters;
use report::{Check, Filesystem, Report, Usage};

#[derive(Parser, Debug)]
#[command(
    name = "bcdu",
    version,
    about = "What each folder really occupies on bcachefs",
    long_about = "Walks a directory tree and reads bcachefs's extents and \
reflink btrees, so data shared between files by reflinks (cp --reflink, \
deduplicating importers) and hardlinks is counted once - du counts it again for \
every copy. For each folder: total is what its files occupy, exclusive what only \
its files hold (what deleting it would free), shared the rest, apparent what du \
-b says. Needs root.",
    disable_help_flag = true
)]
struct Cli {
    /// Print help
    #[arg(long = "help", action = clap::ArgAction::Help)]
    _help: (),

    /// Folder levels to show below PATH
    #[arg(short = 'd', long = "depth", default_value_t = 2)]
    depth: u32,

    /// Human-readable units
    #[arg(short = 'h', long = "human-readable")]
    human_readable: bool,

    /// Output JSON
    #[arg(long)]
    json: bool,

    /// Compare each subvolume's direct (never reflinked) extents with
    /// bcachefs's own accounting of it
    #[arg(long)]
    check: bool,

    /// Threads [default: one per CPU]
    #[arg(short = 'j', long = "threads")]
    threads: Option<usize>,

    /// Print counters and timings
    #[arg(short = 'v', long = "verbose")]
    verbose: bool,

    /// Directory to measure
    #[arg(default_value = ".")]
    path: PathBuf,
}

struct Subvolume {
    node: u32,
    snapshot: bcachefs::SubvolSnapshot,
}

fn subvolumes(w: &walk::Walk) -> Result<HashMap<u64, Subvolume>> {
    let mut out = HashMap::new();
    for n in 0..w.nodes.len() as u32 {
        let subvol = w.nodes[n as usize].subvol;
        if subvol == 0 && w.counters.dirs > 0 && n == 0 {
            bail!("the kernel doesn't report subvolumes (STATX_SUBVOL, Linux 6.10)");
        }
        if !report::is_subvol_root(w, n) {
            continue;
        }
        let path = report::node_path(w, n);
        let fd = openat(
            CWD,
            &path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .with_context(|| format!("opening '{}'", path.display()))?;
        let snapshot = bcachefs::subvol_snapshot(&fd, subvol).with_context(|| {
            format!("the snapshot of subvolume {subvol} at '{}'", path.display())
        })?;
        out.insert(subvol, Subvolume { node: n, snapshot });
    }
    Ok(out)
}

fn bcdu(cli: Cli) -> Result<()> {
    let threads = cli
        .threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()));

    let root_fd = openat(
        CWD,
        &cli.path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .with_context(|| format!("opening '{}'", cli.path.display()))?;
    if !bcachefs::is_bcachefs(&root_fd)? {
        bail!("'{}' is not on bcachefs", cli.path.display());
    }

    let t = Instant::now();
    let mut reflink_counters = Counters::default();
    let (indirect, w) = std::thread::scope(|s| {
        let indirect = s.spawn(|| extents::read_indirect(&root_fd, &mut reflink_counters));
        let w = walk::walk(&cli.path, threads);
        (indirect.join().unwrap(), w)
    });
    let (indirect, mut w) = (indirect?, w?);
    w.counters += &reflink_counters;
    info!(
        "walk and reflink btree: {:.2?}, {} files, {} indirect extents",
        t.elapsed(),
        w.files.len(),
        indirect.len()
    );

    let subvols = subvolumes(&w)?;
    let snapshots: HashMap<u32, u64> = subvols.iter().map(|(&id, s)| (s.snapshot.id, id)).collect();

    let t = Instant::now();
    let holders = extents::read_extents(&root_fd, &mut w, &snapshots, &indirect, threads)?;
    info!(
        "extents btree: {:.2?}, {} references",
        t.elapsed(),
        holders.files.len()
    );

    let t = Instant::now();
    let att = extents::attribute(&w, &indirect, &holders, threads);
    let usage = Usage::new(&w, &att);
    info!("attribute: {:.2?}", t.elapsed());

    let user = bcachefs::user_data(&root_fd)?;
    let outside_walk = w.counters.bytes_outside_direct + att.outside_only;
    let filesystem = Filesystem {
        user_data_one_copy: user.one_copy_bytes(),
        user_data_raw: user.raw_bytes(),
        replicas: user.replicas(),
        walked: usage.total[0],
        outside_walk,
        unreferenced: att.unreferenced,
        unexplained: user.one_copy_bytes() as i64
            - (usage.total[0] + outside_walk + att.unreferenced) as i64,
    };

    let check = cli
        .check
        .then(|| check(&w, &subvols, &filesystem))
        .transpose()?;

    let r = Report {
        folders: report::folders(&w, &usage, cli.depth),
        filesystem: Some(filesystem),
        check,
        counters: w.counters,
    };

    let mut out = String::new();
    if cli.json {
        out = serde_json::to_string_pretty(&r)?;
        out.push('\n');
    } else {
        report::report_to_text(&mut out, &r, &cli.path, cli.human_readable, cli.verbose);
    }
    std::io::stdout().write_all(out.as_bytes())?;
    Ok(())
}

fn check(w: &walk::Walk, subvols: &HashMap<u64, Subvolume>, fs: &Filesystem) -> Result<Vec<Check>> {
    let replicas =
        fs.replicas
            .context("--check: user data is stored with different replica counts")? as u64;

    let mut direct: HashMap<u64, u64> = HashMap::new();
    for f in &w.files {
        *direct.entry(w.nodes[f.dir as usize].subvol).or_default() += f.direct;
    }

    let mut out: Vec<Check> = subvols
        .iter()
        .map(|(&subvol, s)| {
            let raw = s.snapshot.sectors << 9;
            let bcdu_direct = direct.get(&subvol).copied().unwrap_or(0);
            Check {
                subvolume: subvol,
                path: report::node_path(w, s.node),
                bcachefs_bytes: raw / replicas,
                bcdu_direct,
                matches: raw == bcdu_direct * replicas,
            }
        })
        .collect();
    out.sort_by_key(|c| c.subvolume);
    Ok(out)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or(if cli.verbose { "info" } else { "warn" }),
    )
    .format(|buf, rec| {
        writeln!(
            buf,
            "{}: {}",
            rec.level().as_str().to_lowercase(),
            rec.args()
        )
    })
    .init();

    match bcdu(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("bcdu: {e:#}");
            ExitCode::FAILURE
        }
    }
}
