mod bcachefs;
mod counters;
mod extents;
mod fiemap;
mod ioctl;
mod report;
mod walk;

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Parser;
use log::{info, warn};
use rustix::fs::{AtFlags, CWD, Mode, OFlags, StatxAttributes, StatxFlags, openat, statx};

use report::{Check, Filesystem, Report, Usage};

#[derive(Parser, Debug)]
#[command(
    name = "bcdu",
    version,
    about = "What each folder really occupies on bcachefs",
    long_about = "Walks a directory tree and reads every file's extents, \
counting each extent once - so data shared between files by reflinks (cp \
--reflink, deduplicating importers) isn't counted again for every copy, as du \
counts it. For each folder: total is what its files occupy, exclusive what \
only its files reference (what deleting it would free), shared the rest, \
apparent what du -b says. Run as root to read every file.",
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

fn raise_nofile_limit() {
    use rustix::process::{Resource, getrlimit, setrlimit};
    let mut lim = getrlimit(Resource::Nofile);
    if lim.current < lim.maximum {
        lim.current = lim.maximum;
        let _ = setrlimit(Resource::Nofile, lim);
    }
}

fn bcdu(cli: Cli) -> Result<()> {
    raise_nofile_limit();
    let threads = cli.threads.unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()));

    let root_fd = openat(CWD, &cli.path, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())
        .with_context(|| format!("opening '{}'", cli.path.display()))?;
    let bcachefs = bcachefs::is_bcachefs(&root_fd)?;
    if !bcachefs {
        warn!(
            "'{}' is not on bcachefs: extents are told apart by FIEMAP offsets alone, and nothing is compared",
            cli.path.display()
        );
    }

    let t = Instant::now();
    let mut w = walk::walk(&cli.path, threads)?;
    info!("walk: {:.2?}, {} files, {} shared slices", t.elapsed(), w.files.len(), w.refs.len());

    let t = Instant::now();
    extents::resolve_single_ptr(&mut w.refs, &mut w.counters);
    w.refs.sort_unstable_by_key(|r| (r.sig, r.start));
    info!("resolve and sort: {:.2?}", t.elapsed());

    let t = Instant::now();
    let att = extents::attribute(&w.refs, &w.files, &w.links, &w.nodes, threads);
    let usage = Usage::new(&w, &att);
    info!("attribute: {:.2?}", t.elapsed());

    let filesystem = if bcachefs {
        let user = bcachefs::user_data(&root_fd)?;
        let st = statx(&root_fd, c"", AtFlags::EMPTY_PATH, StatxFlags::BASIC_STATS)?;
        let walked_from_root = st.stx_attributes_mask.contains(StatxAttributes::MOUNT_ROOT)
            && st.stx_attributes.contains(StatxAttributes::MOUNT_ROOT);
        Some(Filesystem {
            user_data_one_copy: user.one_copy_bytes(),
            user_data_raw: user.raw_bytes(),
            replicas: user.replicas(),
            walked_from_root,
            found: usage.total[0],
        })
    } else {
        None
    };

    let check = match (&filesystem, cli.check) {
        (Some(fs), true) => Some(check(&w, fs)?),
        (None, true) => anyhow::bail!("--check needs bcachefs"),
        _ => None,
    };

    let r = Report {
        folders: report::folders(&w, &usage, cli.depth),
        filesystem,
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

fn check(w: &walk::Walk, fs: &Filesystem) -> Result<Vec<Check>> {
    let replicas = fs.replicas.context("--check: user data is stored with different replica counts")? as u64;

    let mut direct: HashMap<u64, u64> = HashMap::new();
    for f in &w.files {
        *direct.entry(w.nodes[f.dir as usize].subvol).or_default() += f.direct;
    }

    let mut out = Vec::new();
    for n in 0..w.nodes.len() as u32 {
        if !report::is_subvol_root(w, n) {
            continue;
        }
        let path = report::node_path(w, n);
        let subvol = w.nodes[n as usize].subvol;
        let fd = openat(CWD, &path, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())
            .with_context(|| format!("opening '{}'", path.display()))?;
        let raw = bcachefs::subvol_sectors(&fd, subvol).with_context(|| format!("subvolume {subvol}"))? << 9;
        let bcdu_direct = direct.get(&subvol).copied().unwrap_or(0);
        out.push(Check {
            subvolume: subvol,
            path,
            bcachefs_bytes: raw / replicas,
            bcdu_direct,
            matches: raw == bcdu_direct * replicas,
        });
    }
    Ok(out)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or(if cli.verbose { "info" } else { "warn" }),
    )
    .format(|buf, rec| writeln!(buf, "{}: {}", rec.level().as_str().to_lowercase(), rec.args()))
    .init();

    match bcdu(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("bcdu: {e:#}");
            ExitCode::FAILURE
        }
    }
}
