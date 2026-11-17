mod daemon;
mod engine;
mod filter;
mod sys;
mod util;
mod watch;

use anyhow::{Context, Result, bail};
use clap::{ArgAction, CommandFactory, FromArgMatches, Parser};
use engine::{Engine, Opts, Pair, PairState};
use filter::Filter;
use serde::Deserialize;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Duration;

/// Keeps directory trees mirrored: one rsync-style pass, then live updates
/// driven by fanotify.
///
/// Always copies like `rsync -aHAXSx --numeric-ids`: permissions, numeric
/// owners, times, symlinks, hard links, devices, FIFOs and sockets, ACLs and
/// extended attributes, sparse files, and it stays on one filesystem per
/// source. Run it as root.
#[derive(Parser, Debug)]
#[command(name = "fansync", version)]
struct Cli {
    /// Source directory. With a trailing slash its contents go into DST;
    /// without one the directory itself does (DST/NAME), like rsync.
    src: Option<String>,
    /// Destination directory.
    dst: Option<String>,
    /// TOML file with [[pair]] entries and options.
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Do one full pass and exit instead of watching for changes.
    #[arg(long)]
    once: bool,
    /// Show what would change without changing anything (implies --once, -i).
    #[arg(short = 'n', long)]
    dry_run: bool,
    /// Print a line per change, in rsync's --itemize-changes format.
    #[arg(short = 'i', long = "itemize-changes")]
    itemize: bool,
    /// Print changed paths; twice for debug output.
    #[arg(short = 'v', long, action = ArgAction::Count)]
    verbose: u8,
    /// Only print warnings and errors.
    #[arg(short = 'q', long)]
    quiet: bool,
    /// Delete destination entries that aren't in the source. Excluded
    /// entries are never deleted.
    #[arg(long)]
    delete: bool,
    /// Skip paths matching PATTERN (rsync syntax). Order relative to
    /// --include matters: the first matching rule wins.
    #[arg(long, value_name = "PATTERN")]
    exclude: Vec<String>,
    /// Don't skip paths matching PATTERN, even if a later --exclude matches.
    #[arg(long, value_name = "PATTERN")]
    include: Vec<String>,
    /// Read exclude patterns from FILE ("+ PATTERN" lines include).
    #[arg(long, value_name = "FILE")]
    exclude_from: Vec<PathBuf>,
    /// Show progress during full passes.
    #[arg(long)]
    progress: bool,
    /// Worker threads [default: CPUs, between 4 and 16].
    #[arg(short = 'j', long)]
    threads: Option<usize>,
    /// Sync once no new events have arrived for this long [default: 2].
    #[arg(long, value_name = "SECS")]
    settle: Option<f64>,
    /// ...but never wait longer than this after the first event [default: 30].
    #[arg(long, value_name = "SECS")]
    max_delay: Option<f64>,
    /// Hours between full safety passes while watching; 0 = never [default: 6].
    #[arg(long, value_name = "HOURS")]
    rescan: Option<f64>,
    /// Minutes between full passes for sources that can't be watched, and
    /// before retrying after an error [default: 10].
    #[arg(long, value_name = "MINUTES")]
    interval: Option<f64>,
    /// When a file gets a new hard link while watching, its other names
    /// may be unknown; a full pass this many seconds later links them
    /// [default: 300].
    #[arg(long, value_name = "SECS")]
    link_delay: Option<f64>,
    /// Don't use fanotify: just do a full pass every --interval minutes.
    #[arg(long)]
    no_watch: bool,
    /// Existing files at least this big are updated in place, writing only
    /// the blocks that changed; 0 = always write a new copy [default: 32M].
    #[arg(long, value_name = "SIZE")]
    inplace_min: Option<String>,
    /// Also drop copied source files from the page cache.
    #[arg(long)]
    drop_cache: bool,
    /// Run with the lowest CPU and I/O priority.
    #[arg(long)]
    low_priority: bool,
    /// Keep FILE updated with the current state as shell KEY=value lines.
    #[arg(long, value_name = "FILE")]
    status_file: Option<PathBuf>,
    /// Exit (code 11) when a pass fails as a whole, for instance because the
    /// destination drive fails, instead of retrying every --interval.
    #[arg(long)]
    exit_on_error: bool,
    /// Copy hard-linked files separately instead of linking them.
    #[arg(long)]
    no_hard_links: bool,
    /// Don't copy extended attributes and ACLs.
    #[arg(long)]
    no_xattrs: bool,
    /// Don't preserve holes in sparse files.
    #[arg(long)]
    no_sparse: bool,

    // rsync options that are accepted for compatibility; their behaviour is
    // always on.
    #[arg(short = 'a', hide = true)]
    _a: bool,
    #[arg(short = 'H', hide = true)]
    _hl: bool,
    #[arg(short = 'A', hide = true)]
    _acl: bool,
    #[arg(short = 'X', hide = true)]
    _xa: bool,
    #[arg(short = 'S', hide = true)]
    _sp: bool,
    #[arg(short = 'x', hide = true)]
    _x: bool,
    #[arg(long = "numeric-ids", hide = true)]
    _numeric_ids: bool,
    /// rsync's --info; "progress2" turns on --progress.
    #[arg(long = "info", hide = true, value_name = "FLAGS")]
    info: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Config {
    delete: Option<bool>,
    threads: Option<usize>,
    settle: Option<f64>,
    max_delay: Option<f64>,
    rescan: Option<f64>,
    interval: Option<f64>,
    link_delay: Option<f64>,
    inplace_min: Option<SizeVal>,
    drop_cache: Option<bool>,
    low_priority: Option<bool>,
    status_file: Option<PathBuf>,
    exit_on_error: Option<bool>,
    hard_links: Option<bool>,
    xattrs: Option<bool>,
    sparse: Option<bool>,
    progress: Option<bool>,
    /// Excludes for every pair.
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default, rename = "pair")]
    pairs: Vec<PairConfig>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SizeVal {
    Num(u64),
    Str(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PairConfig {
    name: Option<String>,
    src: String,
    dst: String,
    /// Ordered rules: "- PATTERN" excludes, "+ PATTERN" includes.
    #[serde(default)]
    rules: Vec<String>,
    /// Checked after `rules`, before `exclude`.
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
    exclude_from: Option<PathBuf>,
}

pub struct Settings {
    pub progress: bool,
    pub quiet: bool,
    pub settle: Duration,
    pub max_delay: Duration,
    pub rescan: Option<Duration>,
    pub interval: Duration,
    pub link_delay: Duration,
    pub no_watch: bool,
    pub status_file: Option<PathBuf>,
    pub exit_on_error: bool,
}

/// glibc gives each thread its own memory arena. Full passes allocate on
/// many threads, and what they keep ends up scattered over all the arenas,
/// whose free space can't be handed back to the system: fansync grew a few
/// MB with every full pass. Two arenas keep it flat, for about 3% of pass
/// time. Has to happen before any thread starts; MALLOC_ARENA_MAX overrides it.
fn limit_arenas() {
    #[cfg(target_env = "gnu")]
    if std::env::var_os("MALLOC_ARENA_MAX").is_none() {
        unsafe { libc::mallopt(libc::M_ARENA_MAX, 2) };
    }
}

fn main() {
    limit_arenas();
    let code = match run() {
        Ok(c) => c,
        Err(e) => {
            log!("error: {e:#}");
            1
        }
    };
    std::process::exit(code);
}

fn secs(v: f64, what: &str) -> Result<Duration> {
    Duration::try_from_secs_f64(v).with_context(|| format!("invalid {what}: {v}"))
}

fn read_rules(f: &mut Filter, file: &Path) -> Result<()> {
    let text = std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    for line in text.lines() {
        f.add_line(line, false).map_err(anyhow::Error::msg)?;
    }
    Ok(())
}

fn run() -> Result<i32> {
    let matches = Cli::command().get_matches();
    let cli = Cli::from_arg_matches(&matches)?;
    util::VERBOSE.store(cli.verbose, Relaxed);
    let cfg: Config = match &cli.config {
        Some(p) => {
            let text = std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
            toml::from_str(&text).with_context(|| format!("parsing {}", p.display()))?
        }
        None => Config::default(),
    };

    // Command-line rules come first, in the order they were given.
    enum Arg<'a> {
        Rule(bool, &'a String),
        File(&'a PathBuf),
    }
    let mut args: Vec<(usize, Arg)> = Vec::new();
    for (id, include, vals) in [("exclude", false, &cli.exclude), ("include", true, &cli.include)] {
        if let Some(idx) = matches.indices_of(id) {
            args.extend(idx.zip(vals).map(|(i, v)| (i, Arg::Rule(include, v))));
        }
    }
    if let Some(idx) = matches.indices_of("exclude_from") {
        args.extend(idx.zip(&cli.exclude_from).map(|(i, f)| (i, Arg::File(f))));
    }
    args.sort_by_key(|a| a.0);
    let mut base = Filter::default();
    for (_, a) in args {
        match a {
            Arg::Rule(include, p) => base.add(include, p).map_err(anyhow::Error::msg)?,
            Arg::File(f) => read_rules(&mut base, f)?,
        }
    }
    for p in &cfg.exclude {
        base.add(false, p).map_err(anyhow::Error::msg)?;
    }

    let mut pairs = Vec::new();
    for pc in &cfg.pairs {
        let mut f = base.clone();
        for r in &pc.rules {
            f.add_line(r, false).map_err(anyhow::Error::msg)?;
        }
        for p in &pc.include {
            f.add(true, p).map_err(anyhow::Error::msg)?;
        }
        for p in &pc.exclude {
            f.add(false, p).map_err(anyhow::Error::msg)?;
        }
        if let Some(file) = &pc.exclude_from {
            read_rules(&mut f, file)?;
        }
        pairs.extend(make_pair(pc.name.clone(), &pc.src, &pc.dst, f)?);
    }
    match (&cli.src, &cli.dst) {
        (Some(s), Some(d)) => pairs.extend(make_pair(None, s, d, base)?),
        (Some(_), None) => bail!("missing destination (usage: fansync [OPTIONS] SRC DST)"),
        _ if cfg.pairs.is_empty() => bail!("nothing to sync: give SRC DST or --config FILE"),
        _ => {}
    }
    if pairs.is_empty() {
        bail!("none of the source directories exist");
    }
    for p in &pairs {
        check_overlap(p)?;
    }

    let root = unsafe { libc::geteuid() } == 0;
    if !root {
        log!("warning: not running as root, so owners, device files and some extended attributes won't be copied");
    }
    let inplace_min = match (&cli.inplace_min, &cfg.inplace_min) {
        (Some(s), _) | (None, Some(SizeVal::Str(s))) => util::parse_size(s).map_err(anyhow::Error::msg)?,
        (None, Some(SizeVal::Num(n))) => *n,
        (None, None) => 32 << 20,
    };
    let opts = Opts {
        delete: cli.delete || cfg.delete.unwrap_or(false),
        dry_run: cli.dry_run,
        itemize: cli.itemize || cli.dry_run,
        hard_links: !cli.no_hard_links && cfg.hard_links.unwrap_or(true),
        xattrs: !cli.no_xattrs && cfg.xattrs.unwrap_or(true),
        sparse: !cli.no_sparse && cfg.sparse.unwrap_or(true),
        owner: root,
        inplace_min,
        drop_cache: cli.drop_cache || cfg.drop_cache.unwrap_or(false),
    };
    let rescan = cli.rescan.or(cfg.rescan).unwrap_or(6.0);
    let set = Settings {
        progress: cli.progress
            || cli.info.as_deref().is_some_and(|i| i.contains("progress"))
            || cfg.progress.unwrap_or(false),
        quiet: cli.quiet,
        settle: secs(cli.settle.or(cfg.settle).unwrap_or(2.0), "--settle")?,
        max_delay: secs(cli.max_delay.or(cfg.max_delay).unwrap_or(30.0), "--max-delay")?,
        rescan: if rescan > 0.0 { Some(secs(rescan * 3600.0, "--rescan")?) } else { None },
        interval: secs(cli.interval.or(cfg.interval).unwrap_or(10.0) * 60.0, "--interval")?,
        link_delay: secs(cli.link_delay.or(cfg.link_delay).unwrap_or(300.0), "--link-delay")?,
        no_watch: cli.no_watch,
        status_file: cli.status_file.clone().or(cfg.status_file.clone()),
        exit_on_error: cli.exit_on_error || cfg.exit_on_error.unwrap_or(false),
    };

    unsafe { libc::umask(0) };
    sys::raise_nofile();
    if cli.low_priority || cfg.low_priority.unwrap_or(false) {
        sys::lower_priority();
    }
    let threads = cli
        .threads
        .or(cfg.threads)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(4, 16));
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .stack_size(32 << 20)
        .thread_name(|i| format!("fansync-{i}"))
        .build_global()?;

    // The first Ctrl-C stops cleanly at the next file; a second one exits
    // immediately.
    let stop = Arc::new(AtomicBool::new(false));
    for sig in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register_conditional_shutdown(sig, 130, stop.clone())?;
        signal_hook::flag::register(sig, stop.clone())?;
    }

    let eng = Engine::new(opts, stop);
    let states: Vec<PairState> = pairs.into_iter().map(PairState::new).collect();
    if cli.once || cli.dry_run {
        Ok(daemon::run_once(&eng, &states, &set))
    } else {
        Ok(daemon::run(&eng, &states, &set))
    }
}

fn make_pair(name: Option<String>, src: &str, dst: &str, filter: Filter) -> Result<Option<Pair>> {
    let label = name.clone().unwrap_or_else(|| src.to_string());
    let canon = match std::fs::canonicalize(src) {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            log!("{label}: {src} not found, skipped");
            return Ok(None);
        }
        Err(e) => return Err(e).with_context(|| format!("{label}: {src}")),
    };
    if !canon.is_dir() {
        bail!("{label}: {src} is not a directory");
    }
    let mut dstp = std::path::absolute(dst).with_context(|| format!("{label}: {dst}"))?;
    let mut filter = filter;
    if !src.ends_with('/')
        && let Some(base) = canon.file_name()
    {
        dstp.push(base);
        filter.set_prefix(base.as_bytes());
    }
    let name = name.unwrap_or_else(|| canon.display().to_string());
    Ok(Some(Pair { name, src: canon, dst: dstp, filter }))
}

/// Refuses a destination inside its own source unless it is excluded, or
/// the copy would feed on itself.
fn check_overlap(p: &Pair) -> Result<()> {
    // The destination may not exist yet: resolve the part that does.
    let mut existing = p.dst.as_path();
    let mut rest = Vec::new();
    while !existing.exists() {
        rest.push(existing.file_name().unwrap_or_default().to_owned());
        existing = existing.parent().unwrap_or(Path::new("/"));
    }
    let mut dst = std::fs::canonicalize(existing)?;
    let dev = std::fs::metadata(&dst)?.dev();
    dst.extend(rest.iter().rev());
    if p.src.starts_with(&dst) && dev == std::fs::metadata(&p.src)?.dev() {
        bail!("{}: the source {} is inside the destination {}", p.name, p.src.display(), dst.display());
    }
    if let Ok(rel) = dst.strip_prefix(&p.src) {
        if rel.as_os_str().is_empty() {
            bail!("{}: source and destination are the same directory", p.name);
        }
        let src_dev = std::fs::metadata(&p.src)?.dev();
        if dev == src_dev && !p.filter.excluded_path(rel.as_os_str().as_bytes(), true) {
            bail!("{}: the destination {} is inside the source; exclude it", p.name, dst.display());
        }
    }
    Ok(())
}
