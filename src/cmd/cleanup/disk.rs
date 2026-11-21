//! `valet clean`: report, and with `--run` delete, regenerable or inactive
//! files on this machine.

use crate::proc::{self, Cmd};
use crate::repos::Repo;
use crate::{error, info, warn};
use anyhow::{Context, Result, bail};
use regex::Regex;
use std::collections::{BTreeSet, HashSet};
use std::fs::{self, Metadata};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DAY: i64 = 86_400;
/// Merged worktrees changed this recently may still be in use.
const RECENT_SECS: i64 = 2 * 3600;
const LOCAL: Duration = Duration::from_secs(60);
const DOCKER_SLOW: Duration = Duration::from_secs(300);
const PRUNE: Duration = Duration::from_secs(600);
const FETCH: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
pub enum Category {
    /// core.<pid> files in the folder holding Userland, older than 1 day.
    Cores,
    /// My files in <ws>/tmp (ws: the folder holding Userland) and /tmp older
    /// than N days.
    Tmp,
    /// Claude Code, Codex and OMP session transcripts older than N days and
    /// not open; Codex releases other than `current` and not in use.
    Ai,
    /// Stopped or never-started containers created more than N days ago,
    /// dangling images, build cache older than N days; never volumes or
    /// tagged images.
    Docker,
    /// Worktrees of every repo under <Userland>/Repos that are clean, not in
    /// use, not the default, home or listed branch, and either merged into
    /// origin/<default> (idle 2 hours) or, with nothing unpushed, untouched for
    /// --worktree-days; folded (the branch stays). Also leftovers of folds.
    Worktrees,
    /// Entries of <Userland>/Scratch untouched for 2 days, not in use.
    /// Throw-away folders untouched for 3 days, in every worktree under
    /// <Userland>/Repos: the convention names Logs, Builds, Caches, Downloads, Scratch, and the usual
    /// generated ones (.tmp, node_modules, target, .venv, pkg-out, .cache).
    /// Never a folder holding a git-tracked file or in use; other untracked
    /// files are left alone.
    Scratch,
    /// Tool versions installed by mise that config.toml no longer pins (per
    /// app and architecture, `mise prune`), Rust toolchains other than the
    /// pinned one, mise download-cache blobs untouched N days.
    Apps,
    /// Local branches of every repo that no worktree uses, not the default
    /// or home branch, either merged into origin/<default> or the head of a
    /// merged GitHub PR at the PR's last commit (squash merges); deleted
    /// with their remote-tracking ref and `repos take` line.
    Branches,
}

const ALL: [Category; 8] = [
    Category::Cores,
    Category::Tmp,
    Category::Ai,
    Category::Docker,
    Category::Worktrees,
    Category::Scratch,
    Category::Apps,
    Category::Branches,
];

impl Category {
    fn name(self) -> &'static str {
        match self {
            Category::Cores => "cores",
            Category::Tmp => "tmp",
            Category::Ai => "ai",
            Category::Docker => "docker",
            Category::Worktrees => "worktrees",
            Category::Scratch => "scratch",
            Category::Apps => "apps",
            Category::Branches => "branches",
        }
    }
}

#[derive(clap::Args)]
pub struct Args {
    /// Delete (default: report only).
    #[arg(long)]
    run: bool,
    /// Inactive means untouched for N days.
    #[arg(long, value_name = "N", default_value_t = 14)]
    days: u32,
    /// Worktrees (unmerged, nothing unpushed) are folded when untouched N days.
    #[arg(long, value_name = "N", default_value_t = 3)]
    worktree_days: u32,
    /// Do nothing unless the disk holding Userland is at least PCT% full.
    #[arg(long, value_name = "PCT", default_value_t = 0)]
    if_above: u32,
    /// Categories to scan (default: all).
    #[arg(value_enum, value_name = "CATEGORY")]
    categories: Vec<Category>,
}

/// Roots and settings one scan runs against.
struct Ctx {
    /// The folder holding Userland (`<Userland>/..`).
    ws: PathBuf,
    /// `<Userland>/Repos`.
    repos: PathBuf,
    /// `<Userland>/Scratch` (every entry throw-away).
    scratch: PathBuf,
    /// mise's cache dirs (`<Arch>/` below).
    mise_caches: PathBuf,
    home: PathBuf,
    /// Claude Code's and Codex's folders ($CLAUDE_CONFIG_DIR, $CODEX_HOME).
    claude: PathBuf,
    codex: PathBuf,
    tmp: PathBuf,
    uid: u32,
    days: u32,
    worktree_days: u32,
    now: i64,
    procs: Procs,
}

impl Ctx {
    fn days_secs(&self) -> i64 {
        i64::from(self.days) * DAY
    }

    /// Paths that removal may touch. Anything else is refused.
    fn allowed(&self) -> Vec<PathBuf> {
        vec![
            self.ws.clone(),
            self.claude.join("projects"),
            self.codex.join("sessions"),
            self.codex.join("packages"),
            self.home.join(".omp/agent/sessions"),
            self.tmp.clone(),
        ]
    }

    /// `find -mtime +N`: whole days of age strictly greater than N.
    fn older_days(&self, md: &Metadata, n: u32) -> bool {
        self.now - md.mtime() >= (i64::from(n) + 1) * DAY
    }

    /// `find -newermt "-N days"` on `path` and everything below it, up to `depth`.
    fn touched_within(&self, path: &Path, secs: i64, depth: Option<usize>) -> bool {
        let cutoff = self.now - secs;
        newer_than(path, cutoff, depth)
    }
}

/// Processes: what they hold open, where they run, my command lines.
#[derive(Default)]
struct Procs {
    open: BTreeSet<String>,
    cwds: BTreeSet<String>,
    mine: Vec<String>,
}

impl Procs {
    /// Read from /proc: lsof stats each file and hangs on an unresponsive network filesystem.
    fn scan(uid: u32) -> Self {
        let mut p = Procs::default();
        let me = std::process::id().to_string();
        for d in entries(Path::new("/proc")) {
            let pid = name_of(&d);
            if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            if let Ok(rd) = fs::read_dir(d.join("fd")) {
                for e in rd.flatten() {
                    if let Ok(t) = fs::read_link(e.path()) {
                        let t = t.to_string_lossy().into_owned();
                        if t.starts_with('/') {
                            p.open.insert(t);
                        }
                    }
                }
            }
            if let Ok(t) = fs::read_link(d.join("cwd")) {
                p.cwds.insert(t.to_string_lossy().into_owned());
            }
            if pid != me
                && fs::metadata(&d).is_ok_and(|m| m.uid() == uid)
                && let Ok(raw) = fs::read(d.join("cmdline"))
            {
                let s = String::from_utf8_lossy(&raw)
                    .split('\0')
                    .filter(|a| !a.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ");
                if !s.is_empty() {
                    p.mine.push(s);
                }
            }
        }
        p
    }

    /// Open by a process, or a process runs inside it. Checks the path as
    /// given and its resolved form.
    fn in_use(&self, path: &Path) -> bool {
        let is_dir = path.is_dir();
        let mut names = vec![trim_slash(path)];
        if let Ok(c) = fs::canonicalize(path) {
            names.push(c.to_string_lossy().into_owned());
        }
        names.iter().any(|p| {
            self.open.contains(p)
                || (is_dir
                    && (has_prefix(&self.open, p)
                        || self.cwds.contains(p)
                        || has_prefix(&self.cwds, p)))
        })
    }
}

fn has_prefix(set: &BTreeSet<String>, dir: &str) -> bool {
    let pre = format!("{dir}/");
    set.range(pre.clone()..)
        .next()
        .is_some_and(|s| s.starts_with(&pre))
}

fn trim_slash(p: &Path) -> String {
    let s = p.to_string_lossy();
    match s.trim_end_matches('/') {
        "" => "/".to_owned(),
        t => t.to_owned(),
    }
}

/// One thing the report lists.
struct Item {
    cat: Category,
    label: String,
    /// Removed by `--run` (builds, generated, cores, tmp, ai).
    path: Option<PathBuf>,
    why: String,
    size: u64,
}

#[derive(Default)]
struct Report {
    items: Vec<Item>,
    docker_containers: Vec<String>,
    docker_prune: bool,
    /// Worktrees to fold, and repos to `git worktree prune`.
    wt_remove: Vec<(Repo, PathBuf)>,
    wt_prune: Vec<Repo>,
    /// Leftovers of folds (`.git/valet-trash/*`).
    wt_trash: Vec<PathBuf>,
    /// Architectures to run `mise prune` for.
    mise_prune: Vec<&'static str>,
    /// Branches to delete: (repo, branch, tip).
    branch_del: Vec<(Repo, String, String)>,
}

impl Report {
    fn add(&mut self, cat: Category, path: &Path, why: impl Into<String>) {
        self.items.push(Item {
            cat,
            label: path.display().to_string(),
            path: Some(path.to_owned()),
            why: why.into(),
            size: size_of(path),
        });
    }

    fn note(&mut self, cat: Category, label: String, why: String, size: u64) {
        self.items.push(Item {
            cat,
            label,
            path: None,
            why,
            size,
        });
    }

    fn total(&self) -> u64 {
        self.items.iter().map(|i| i.size).sum()
    }

    fn print(&self) {
        println!("{:<10} {:>8}  PATH", "CATEGORY", "SIZE");
        for it in &self.items {
            println!(
                "{:<10} {:>8}  {}  ({})",
                it.cat.name(),
                human(it.size),
                it.label,
                it.why
            );
        }
        println!(
            "total: {} in {} items",
            human(self.total()),
            self.items.len()
        );
    }
}

pub fn run(a: Args) -> Result<()> {
    let userland = crate::cmd::apps::root()?;
    let ws = userland
        .parent()
        .context("Userland has no parent")?
        .to_path_buf();
    let repos = crate::repos::root()?;
    if a.if_above > 0 {
        let used = used_pct(&ws)?;
        if used < a.if_above {
            println!(
                "{} is {used}% full, below {}%; nothing to do",
                ws.display(),
                a.if_above
            );
            return Ok(());
        }
    }
    let mut cats: Vec<Category> = Vec::new();
    for c in if a.categories.is_empty() {
        ALL.to_vec()
    } else {
        a.categories.clone()
    } {
        if !cats.contains(&c) {
            cats.push(c);
        }
    }
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    let uid = fs::metadata("/proc/self")
        .context("cannot read /proc/self")?
        .uid();
    let ctx = Ctx {
        mise_caches: crate::cmd::apps::mise_caches(&userland),
        repos,
        scratch: crate::cmd::apps::scratch()?,
        ws,
        claude: super::super::claude_dir()?,
        codex: super::super::codex_dir()?,
        home,
        tmp: PathBuf::from("/tmp"),
        uid,
        days: a.days,
        worktree_days: a.worktree_days,
        now: now(),
        procs: Procs::scan(uid),
    };

    let mut r = Report::default();
    for &c in &cats {
        info!("scanning {}...", c.name());
        let t = Instant::now();
        scan(&ctx, c, &mut r);
        info!("  {}: {}s", c.name(), t.elapsed().as_secs());
    }
    r.print();
    print_df(&ctx);
    if !a.run {
        println!("report only; run again with --run to delete");
        return Ok(());
    }
    let ok = delete(&ctx, &r, cats.contains(&Category::Worktrees));
    println!("after:");
    print_df(&ctx);
    if !ok {
        bail!("some removals failed");
    }
    Ok(())
}

fn scan(ctx: &Ctx, c: Category, r: &mut Report) {
    match c {
        Category::Cores => scan_cores(ctx, r),
        Category::Tmp => scan_tmp(ctx, r),
        Category::Ai => scan_ai(ctx, r),
        Category::Docker => scan_docker(ctx, r),
        Category::Worktrees => scan_worktrees(ctx, r),
        Category::Scratch => scan_scratch(ctx, r),
        Category::Apps => scan_apps(ctx, r),
        Category::Branches => scan_branches(ctx, r),
    }
}

/// Delete everything the report lists. False if anything failed.
fn delete(ctx: &Ctx, r: &Report, prune_worktrees: bool) -> bool {
    let allowed = ctx.allowed();
    let mut ok = true;
    for it in &r.items {
        if matches!(it.cat, Category::Worktrees | Category::Docker) {
            continue;
        }
        if let Some(p) = &it.path
            && let Err(e) = safe_rm(p, &allowed, &ctx.home)
        {
            error!("{e:#}");
            ok = false;
        }
    }
    for id in &r.docker_containers {
        ok &= run_quiet(Cmd::new("docker").args(["rm", "--", id]).timeout(LOCAL));
    }
    if r.docker_prune {
        ok &= run_quiet(
            Cmd::new("docker")
                .args(["image", "prune", "-f"])
                .timeout(PRUNE),
        );
        let until = format!("until={}h", u64::from(ctx.days) * 24);
        ok &= run_quiet(
            Cmd::new("docker")
                .args(["builder", "prune", "-f", "--filter", &until])
                .timeout(PRUNE),
        );
    }
    for (repo, p) in &r.wt_remove {
        if let Err(e) = check_removable(p, &allowed, &ctx.home).and_then(|_| repo.fold_at(p)) {
            error!("{e:#}");
            ok = false;
        }
    }
    for p in &r.wt_trash {
        if let Err(e) =
            check_removable(p, &allowed, &ctx.home).and_then(|t| crate::repos::remove_tree(&t))
        {
            error!("{e:#}");
            ok = false;
        }
    }
    if prune_worktrees {
        for repo in &r.wt_prune {
            ok &= run_inherit(
                Cmd::new("git")
                    .arg("--git-dir")
                    .arg(repo.git_dir())
                    .args(["worktree", "prune"]),
            );
        }
    }
    for (repo, b, tip) in &r.branch_del {
        if let Err(e) = delete_branch(repo, b, tip) {
            error!("{}: branch {b}: {e:#}", repo.slug);
            ok = false;
        }
    }
    for arch in &r.mise_prune {
        if let Err(e) = crate::cmd::apps::mise_each_app(arch, &["prune", "--yes", "--tools"], PRUNE)
        {
            error!("mise prune: {e:#}");
            ok = false;
        }
    }
    ok
}

/// Run with stdout discarded; show stderr on failure.
fn run_quiet(c: Cmd) -> bool {
    match c.output() {
        Ok(o) if o.status.success() => true,
        Ok(o) => {
            error!("{} failed ({}): {}", c.display(), o.status, o.stderr.trim());
            false
        }
        Err(e) => {
            error!("{e:#}");
            false
        }
    }
}

fn run_inherit(c: Cmd) -> bool {
    match c.timeout(LOCAL).run() {
        Ok(()) => true,
        Err(e) => {
            error!("{e:#}");
            false
        }
    }
}

// --- cores ----------------------------------------------------------------

fn scan_cores(ctx: &Ctx, r: &mut Report) {
    for f in entries(&ctx.ws) {
        let n = name_of(&f);
        let is_core = n
            .strip_prefix("core.")
            .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()));
        if !is_core {
            continue;
        }
        let Ok(md) = fs::symlink_metadata(&f) else {
            continue;
        };
        if md.is_file() && ctx.older_days(&md, 0) && !ctx.procs.in_use(&f) {
            r.add(Category::Cores, &f, "core dump");
        }
    }
}

// --- tmp ------------------------------------------------------------------

/// Names in /tmp that belong to running services or sessions.
fn tmp_protected(name: &str) -> bool {
    name.starts_with('.')
        || name.starts_with("claude")
        || name.starts_with("herdr")
        || name.starts_with("tmux")
        || name.starts_with("systemd-")
        || name.starts_with("ssh-")
        || name.ends_with(".sock")
        || name.ends_with(".socket")
}

fn scan_tmp(ctx: &Ctx, r: &mut Report) {
    let why = format!("temp, untouched {} days", ctx.days);
    let mine_old = |p: &Path| {
        fs::symlink_metadata(p).is_ok_and(|m| m.uid() == ctx.uid && ctx.older_days(&m, ctx.days))
    };
    for f in entries(&ctx.ws.join("tmp")) {
        if mine_old(&f) && !ctx.procs.in_use(&f) {
            r.add(Category::Tmp, &f, &why);
        }
    }
    for f in entries(&ctx.tmp) {
        if !mine_old(&f) || tmp_protected(&name_of(&f)) {
            continue;
        }
        if fs::metadata(&f).is_ok_and(|m| m.file_type().is_socket()) || ctx.procs.in_use(&f) {
            continue;
        }
        // A directory counts as untouched only if nothing inside changed either.
        if f.is_dir() && ctx.touched_within(&f, ctx.days_secs(), None) {
            continue;
        }
        r.add(Category::Tmp, &f, &why);
    }
}

// --- scratch --------------------------------------------------------------

/// Folder names that only ever hold regenerable or throw-away files.
const SCRATCH_NAMES: &[&str] = &[
    "Logs",
    "Builds",
    "Caches",
    "Downloads",
    "Scratch",
    ".tmp",
    "node_modules",
    "target",
    ".venv",
    "pkg-out",
    ".cache",
];
const SCRATCH_DAYS: u32 = 3;
/// How deep below Repos to look: <host>/<owner>/<repo>/tree/<branch,
/// up to 2 parts>/<topic>/<sub>/<sub>/X.
const SCRATCH_DEPTH: usize = 10;

/// A directory holds a file git tracks (false outside a repository).
fn has_tracked(dir: &Path) -> bool {
    git_in(dir, &["ls-files", "--", "."], LOCAL)
        .is_some_and(|o| o.status.success() && !o.stdout.trim().is_empty())
}

fn scan_scratch(ctx: &Ctx, r: &mut Report) {
    let days = crate::cmd::apps::SCRATCH_DAYS;
    let why = format!("in Scratch, untouched {days} days");
    for e in entries(&ctx.scratch) {
        let md = match fs::symlink_metadata(&e) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if (md.is_dir() || md.is_file())
            && !ctx.touched_within(&e, i64::from(days) * DAY, None)
            && !ctx.procs.in_use(&e)
        {
            r.add(Category::Scratch, &e, &why);
        }
    }
    let why = format!("throw-away folder, untouched {SCRATCH_DAYS} days");
    let secs = i64::from(SCRATCH_DAYS) * DAY;
    {
        let depth = &SCRATCH_DEPTH;
        let mut stack = vec![(ctx.repos.clone(), 0usize)];
        while let Some((dir, d)) = stack.pop() {
            for e in entries(&dir) {
                if !is_real_dir(&e) {
                    continue;
                }
                let name = name_of(&e);
                if SCRATCH_NAMES.contains(&name.as_str()) {
                    if !ctx.touched_within(&e, secs, None)
                        && !ctx.procs.in_use(&e)
                        && !has_tracked(&e)
                    {
                        r.add(Category::Scratch, &e, &why);
                    }
                } else if name != ".git" && d + 1 < *depth {
                    stack.push((e, d + 1));
                }
            }
        }
    }
}

// --- branches -------------------------------------------------------------

/// Local branches (name, tip) of REPOS safe to delete, with why: not KEEP
/// (checked out, default, home), and either in MERGED (merged into
/// origin/<default>) or the head of a merged PR at its last commit (PRS:
/// headRefName, headRefOid).
fn deletable_branches(
    locals: &[(String, String)],
    keep: &[String],
    merged: &HashSet<String>,
    prs: &[(String, String, u64)],
) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for (b, tip) in locals {
        if keep.contains(b) {
            continue;
        }
        let why = if merged.contains(b) {
            "merged".to_owned()
        } else if let Some((_, _, n)) = prs.iter().find(|(h, oid, _)| h == b && oid == tip) {
            format!("PR #{n} merged")
        } else {
            continue;
        };
        out.push((b.clone(), tip.clone(), why));
    }
    out
}

fn scan_branches(ctx: &Ctx, r: &mut Report) {
    let gh = crate::tools::resolve("gh").ok();
    for repo in crate::repos::all_in(&ctx.repos) {
        let git = |args: &[&str]| {
            Cmd::new("git")
                .arg("--git-dir")
                .arg(repo.git_dir())
                .args(args)
                .timeout(LOCAL)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| o.stdout)
        };
        let Ok(def) = repo.default_branch() else {
            continue;
        };
        let upstream = format!("refs/remotes/origin/{def}");
        let Some(out) = git(&[
            "for-each-ref",
            "--format=%(refname:short) %(objectname)",
            "refs/heads",
        ]) else {
            continue;
        };
        let locals: Vec<(String, String)> = out
            .lines()
            .filter_map(|l| l.split_once(' '))
            .map(|(a, b)| (a.to_owned(), b.to_owned()))
            .collect();
        let merged: HashSet<String> = git(&[
            "for-each-ref",
            "--format=%(refname:short)",
            "--merged",
            &upstream,
            "refs/heads",
        ])
        .map(|o| o.lines().map(str::to_owned).collect())
        .unwrap_or_default();
        let mut keep = repo.pinned_branches();
        keep.extend(
            repo.worktrees()
                .unwrap_or_default()
                .into_iter()
                .map(|(_, b)| b),
        );
        // Squash merges: only for branches not merged by ancestry. Your
        // merged PRs in one call; others' (taken) branches one by one when few.
        let mut prs = Vec::new();
        let rest: Vec<&String> = locals
            .iter()
            .map(|(b, _)| b)
            .filter(|b| !keep.contains(b) && !merged.contains(*b))
            .collect();
        if let (Some(gh), Some(slug)) = (&gh, repo.slug.strip_prefix("github.com/"))
            && !rest.is_empty()
        {
            prs = merged_prs(gh, slug, &["--author", "@me", "--limit", "500"]);
            if rest.len() <= PR_LOOKUPS {
                for b in rest {
                    if !prs.iter().any(|(h, _, _)| h == b) {
                        prs.extend(merged_prs(gh, slug, &["--head", b, "--limit", "5"]));
                    }
                }
            }
        }
        for (b, tip, why) in deletable_branches(&locals, &keep, &merged, &prs) {
            r.note(Category::Branches, format!("{} {b}", repo.slug), why, 0);
            r.branch_del.push((repo.clone(), b, tip));
        }
    }
}

/// Branches looked up one by one (`gh pr list --head`) at most, per repo.
const PR_LOOKUPS: usize = 20;

/// Merged PRs of REPO (`owner/name`) matching FILTER (gh flags): (head
/// branch, head commit, number).
fn merged_prs(gh: &Path, repo: &str, filter: &[&str]) -> Vec<(String, String, u64)> {
    let Ok(out) = Cmd::new(gh)
        .args(["pr", "list", "--repo", repo, "--state", "merged"])
        .args(filter)
        .args(["--json", "number,headRefName,headRefOid"])
        .timeout(FETCH)
        .checked()
    else {
        warn!("branches: {repo}: gh pr list failed; merged-PR check skipped");
        return Vec::new();
    };
    let v: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap_or_default();
    v.iter()
        .filter_map(|p| {
            Some((
                p["headRefName"].as_str()?.to_owned(),
                p["headRefOid"].as_str()?.to_owned(),
                p["number"].as_u64()?,
            ))
        })
        .collect()
}

/// Delete branch B (only while still at TIP), its remote-tracking ref, its
/// config and its `repos take` line.
fn delete_branch(repo: &Repo, b: &str, tip: &str) -> Result<()> {
    let stdin = format!(
        "delete refs/heads/{b} {tip}
delete refs/remotes/origin/{b}
"
    );
    Cmd::new("git")
        .arg("--git-dir")
        .arg(repo.git_dir())
        .args(["update-ref", "--stdin"])
        .stdin(stdin.into_bytes())
        .timeout(LOCAL)
        .checked()?;
    repo.drop_branch_config(&[b.to_owned()]);
    repo.untake(b)
}

// --- apps -----------------------------------------------------------------

const QUERY: Duration = Duration::from_secs(120);

fn scan_apps(ctx: &Ctx, r: &mut Report) {
    match crate::cmd::apps::rustup_homes() {
        Ok(homes) => scan_rustup(ctx, &homes, r),
        Err(e) => warn!("apps: {e:#}"),
    }
    scan_mise_downloads(ctx, r);
    let arches = crate::cmd::apps::installed_arches().unwrap_or_else(|e| {
        warn!("apps: {e:#}");
        Vec::new()
    });
    for arch in arches {
        let out = match crate::cmd::apps::mise_each_app(arch, &["ls", "--prunable"], QUERY) {
            Ok(o) => o,
            Err(e) => {
                warn!("apps: mise ls --prunable: {e:#}; skipping {arch}");
                continue;
            }
        };
        let mut any = false;
        for (app, text) in out {
            for (tool, ver) in prunable(&text) {
                r.note(
                    Category::Apps,
                    format!("{app} ({arch}): {tool} {ver}"),
                    "installed version config.toml no longer pins; mise prune".into(),
                    0,
                );
                any = true;
            }
        }
        if any {
            r.mise_prune.push(arch);
        }
    }
}

/// (tool, version) per line of `mise ls --prunable`.
fn prunable(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            Some((f.next()?.to_owned(), f.next()?.to_owned()))
        })
        .collect()
}

/// Toolchains in each RUSTUP_HOME other than the pinned version's.
fn scan_rustup(ctx: &Ctx, homes: &[(PathBuf, String)], r: &mut Report) {
    for (home, ver) in homes {
        let keep = format!("{ver}-");
        for tc in entries(&home.join("toolchains")) {
            let name = name_of(&tc);
            let needle = format!("{}/", tc.display());
            if name.starts_with(&keep)
                || !is_real_dir(&tc)
                || ctx.procs.in_use(&tc)
                || ctx.procs.mine.iter().any(|c| c.contains(&needle))
            {
                continue;
            }
            r.add(
                Category::Apps,
                &tc,
                format!("Rust toolchain, not the pinned {ver}"),
            );
            let hash = home.join("update-hashes").join(&name);
            if hash.is_file() {
                r.add(Category::Apps, &hash, "its rustup update hash");
            }
        }
    }
}

/// mise's downloaded archives (`<Arch>/downloads-cas/blobs/<hash>/`)
/// untouched N days: installed tools no longer need them.
fn scan_mise_downloads(ctx: &Ctx, r: &mut Report) {
    let why = format!("mise download cache, untouched {} days", ctx.days);
    for arch in entries(&ctx.mise_caches) {
        for b in entries(&arch.join("downloads-cas/blobs")) {
            if is_real_dir(&b)
                && !ctx.touched_within(&b, ctx.days_secs(), None)
                && !ctx.procs.in_use(&b)
            {
                r.add(Category::Apps, &b, &why);
            }
        }
    }
}

// --- ai -------------------------------------------------------------------

fn scan_ai(ctx: &Ctx, r: &mut Report) {
    let days = ctx.days;
    let old = |p: &Path, want_file: bool| {
        fs::symlink_metadata(p)
            .is_ok_and(|m| (!want_file || m.is_file()) && ctx.older_days(&m, days))
    };
    // Claude Code: <project>/<session>.jsonl and its <session>/ directory.
    for proj in entries(&ctx.claude.join("projects")) {
        if !is_real_dir(&proj) {
            continue;
        }
        for f in entries(&proj) {
            let n = name_of(&f);
            if !n.ends_with(".jsonl") || !old(&f, false) || ctx.procs.in_use(&f) {
                continue;
            }
            r.add(
                Category::Ai,
                &f,
                format!("Claude Code session, untouched {days} days"),
            );
            let d = proj.join(&n[..n.len() - ".jsonl".len()]);
            if d.is_dir() && !ctx.procs.in_use(&d) {
                r.add(Category::Ai, &d, "Claude Code session data");
            }
        }
    }
    for (root, what) in [
        (ctx.codex.join("sessions"), "Codex"),
        (ctx.home.join(".omp/agent/sessions"), "OMP"),
    ] {
        for f in walk_files(&root) {
            if name_of(&f).ends_with(".jsonl") && old(&f, true) && !ctx.procs.in_use(&f) {
                r.add(
                    Category::Ai,
                    &f,
                    format!("{what} session, untouched {days} days"),
                );
            }
        }
    }
    // Codex releases other than the current one and not running.
    for d in entries(&ctx.codex.join("packages")) {
        if !d.is_dir() {
            continue;
        }
        let cur = fs::canonicalize(d.join("current")).ok();
        for rel in entries(&d.join("releases")) {
            if !rel.is_dir() {
                continue;
            }
            let canon = fs::canonicalize(&rel).ok();
            if cur.is_some() && (cur.as_ref() == Some(&rel) || cur == canon) {
                continue;
            }
            let mut needles = vec![format!("{}/", rel.display())];
            if let Some(c) = &canon {
                needles.push(format!("{}/", c.display()));
            }
            if ctx
                .procs
                .mine
                .iter()
                .any(|cmd| needles.iter().any(|n| cmd.contains(n.as_str())))
            {
                continue;
            }
            r.add(Category::Ai, &rel, "Codex release, not current");
        }
    }
}

// --- docker ---------------------------------------------------------------

fn scan_docker(ctx: &Ctx, r: &mut Report) {
    if proc::which("docker").is_none() || !proc::succeeds("docker", &["info"], LOCAL) {
        warn!("docker: not reachable; skipping");
        return;
    }
    let ps = docker_out(&[
        "ps",
        "-a",
        "--size",
        "--filter",
        "status=exited",
        "--filter",
        "status=created",
        "--filter",
        "status=dead",
        "--format",
        "{{.ID}}|{{.Names}}|{{.Image}}|{{.Status}}|{{.CreatedAt}}|{{.Size}}",
    ]);
    let images = docker_out(&["images", "-f", "dangling=true", "--format", "{{.Size}}"]);
    let df = docker_out(&["system", "df", "--format", "{{.Type}}|{{.Reclaimable}}"]);
    docker_items(ctx, &ps, &images, &df, r);
}

fn docker_out(args: &[&str]) -> String {
    Cmd::new("docker")
        .args(args)
        .timeout(DOCKER_SLOW)
        .output()
        .map(|o| o.stdout)
        .unwrap_or_default()
}

fn docker_items(ctx: &Ctx, ps: &str, images: &str, df: &str, r: &mut Report) {
    let cutoff = ctx.now - ctx.days_secs();
    for line in ps.lines().filter(|l| !l.is_empty()) {
        let f: Vec<&str> = line.splitn(6, '|').collect();
        let [id, name, image, status, created, size] = f[..] else {
            continue;
        };
        let created = created.rsplit_once(' ').map_or(created, |(a, _)| a);
        if parse_created(created).unwrap_or(cutoff) >= cutoff {
            continue;
        }
        r.note(
            Category::Docker,
            format!("container {name} ({image})"),
            format!("{status}, created {created}"),
            bytes(size),
        );
        r.docker_containers.push(id.to_owned());
    }
    let (n, sum) = images
        .lines()
        .fold((0u64, 0u64), |(n, s), l| (n + 1, s + bytes(l)));
    if n > 0 {
        r.note(
            Category::Docker,
            format!("{n} dangling images"),
            "untagged".into(),
            sum,
        );
        r.docker_prune = true;
    }
    let cache = df
        .lines()
        .find_map(|l| l.strip_prefix("Build Cache|"))
        .map(|v| v.split(' ').next().unwrap_or_default());
    if let Some(sz) = cache
        && !sz.is_empty()
        && sz != "0B"
    {
        r.note(
            Category::Docker,
            "build cache".into(),
            format!(
                "of {sz} reclaimable, only entries older than {} days go",
                ctx.days
            ),
            0,
        );
        r.docker_prune = true;
    }
}

/// Docker size such as `1.2GB`, `12.5MB (virtual 1GB)` or `0B`, in bytes
/// (SI units, rounded up, like `numfmt --from=si`). Unparseable is 0.
fn bytes(s: &str) -> u64 {
    let v = s.split(' ').next().unwrap_or_default();
    let v = v.strip_suffix('B').unwrap_or(v).to_ascii_uppercase();
    let (num, mult) = match v.char_indices().last() {
        Some((i, c)) if c.is_ascii_alphabetic() => {
            let Some(p) = "KMGTPEZY".find(c) else {
                return 0;
            };
            (&v[..i], 1000f64.powi(p as i32 + 1))
        }
        _ => (v.as_str(), 1.0),
    };
    if num.is_empty() || !num.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return 0;
    }
    num.parse::<f64>()
        .map(|n| (n * mult).ceil() as u64)
        .unwrap_or(0)
}

/// `2026-11-05 12:25:30 +0530` to UNIX seconds.
fn parse_created(s: &str) -> Option<i64> {
    let re = Regex::new(r"^(\d{4})-(\d{2})-(\d{2}) (\d{2}):(\d{2}):(\d{2}) ([+-])(\d{2})(\d{2})$")
        .expect("static regex");
    let c = re.captures(s)?;
    let n = |i: usize| c[i].parse::<i64>().ok();
    let days = days_from_civil(n(1)?, n(2)?, n(3)?);
    let off = (n(8)? * 3600 + n(9)? * 60) * if &c[7] == "-" { -1 } else { 1 };
    Some(days * DAY + n(4)? * 3600 + n(5)? * 60 + n(6)? - off)
}

/// Days since 1970-01-01 of a civil date. Howard Hinnant's algorithm.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// --- worktrees ------------------------------------------------------------

struct Worktree {
    path: String,
    head: String,
    branch: String,
}

/// `git worktree list --porcelain`, one record per worktree.
fn parse_worktrees(out: &str) -> Vec<Worktree> {
    let mut v = Vec::new();
    let mut cur: Option<Worktree> = None;
    for line in out.lines().chain(std::iter::once("")) {
        if let Some(p) = line.strip_prefix("worktree ") {
            v.extend(cur.take());
            cur = Some(Worktree {
                path: p.to_owned(),
                head: String::new(),
                branch: String::new(),
            });
        } else if let Some(w) = cur.as_mut() {
            if let Some(h) = line.strip_prefix("HEAD ") {
                w.head = h.to_owned();
            } else if let Some(b) = line.strip_prefix("branch ") {
                w.branch = b.strip_prefix("refs/heads/").unwrap_or(b).to_owned();
            } else if line.is_empty() {
                v.extend(cur.take());
            }
        }
    }
    v
}

fn git_in(dir: &Path, args: &[&str], timeout: Duration) -> Option<proc::Output> {
    Cmd::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .timeout(timeout)
        .output()
        .ok()
}

fn scan_worktrees(ctx: &Ctx, r: &mut Report) {
    let secs = i64::from(ctx.worktree_days) * DAY;
    for repo in crate::repos::all_in(&ctx.repos) {
        scan_repo_worktrees(ctx, &repo, secs, r);
    }
}

/// [`Category::Worktrees`] for one repo: leftovers of folds, then worktrees
/// under `tree/` safe to fold.
fn scan_repo_worktrees(ctx: &Ctx, repo: &Repo, secs: i64, r: &mut Report) {
    for t in entries(&repo.trash()) {
        if !ctx.procs.in_use(&t) {
            r.add(Category::Worktrees, &t, "leftover of a fold");
            r.wt_trash.push(t);
        }
    }
    let git = |args: &[&str], timeout| {
        Cmd::new("git")
            .arg("--git-dir")
            .arg(repo.git_dir())
            .args(args)
            .timeout(timeout)
            .output()
            .ok()
            .filter(|o| o.status.success())
    };
    let Ok(def) = repo.default_branch() else {
        return;
    };
    let top = repo.dir.join(crate::repos::TREE);
    let Some(list) = git(&["worktree", "list", "--porcelain"], LOCAL).map(|o| o.stdout) else {
        return;
    };
    // git lists resolved paths; match them to `tree/` however it is spelled.
    let ctop = fs::canonicalize(&top).unwrap_or_else(|_| top.clone());
    let wts: Vec<(Worktree, PathBuf)> = parse_worktrees(&list)
        .into_iter()
        .filter_map(|w| {
            let rel = Path::new(&w.path).strip_prefix(&ctop).ok()?.to_owned();
            (!rel.as_os_str().is_empty()).then(|| (w, top.join(rel)))
        })
        .collect();
    if wts.is_empty() {
        return;
    }
    let upstream = format!("origin/{def}");
    if git(&["fetch", "-q", "origin", &def], FETCH).is_none() {
        warn!(
            "worktrees: {}: fetch failed; using the local {upstream}",
            repo.slug
        );
    }
    let pinned = repo.pinned_branches();
    for (w, path) in wts {
        if !path.is_dir() {
            r.note(
                Category::Worktrees,
                w.path.clone(),
                "missing directory; git worktree prune drops it".into(),
                0,
            );
            if !r.wt_prune.contains(repo) {
                r.wt_prune.push(repo.clone());
            }
            continue;
        }
        if pinned.contains(&w.branch) || ctx.procs.in_use(&path) {
            continue;
        }
        // Dirty, or status failed: keep.
        if !matches!(crate::repos::unclean(&path), Ok(None)) {
            continue;
        }
        let merged = !w.head.is_empty()
            && git(&["merge-base", "--is-ancestor", &w.head, &upstream], LOCAL).is_some();
        // Unmerged: only a branch with nothing unpushed (a detached HEAD's
        // commits would be lost).
        if !merged
            && (w.branch.is_empty()
                || git_in(&path, &["rev-list", "--count", "@{u}..HEAD"], LOCAL)
                    .filter(|o| o.status.success())
                    .is_none_or(|o| o.stdout.trim() != "0"))
        {
            continue;
        }
        let used = crate::repos::last_used(&path)
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs() as i64);
        let idle = |s: i64| !ctx.touched_within(&path, s, Some(1)) && ctx.now - used >= s;
        let b = if w.branch.is_empty() {
            String::new()
        } else {
            format!(" ({})", w.branch)
        };
        let why = if merged && idle(RECENT_SECS) {
            format!("clean, merged into {upstream}{b}")
        } else if idle(secs) {
            format!(
                "clean, nothing unpushed, untouched {} days{b}",
                ctx.worktree_days
            )
        } else {
            continue;
        };
        r.add(Category::Worktrees, &path, why);
        r.wt_remove.push((repo.clone(), path));
    }
}

// --- removal --------------------------------------------------------------

/// Check that `p` may be removed: absolute, no `..`, not `/` or $HOME, strictly
/// inside an allowed root, and its parent resolves inside that root (no
/// symlink escape). Returns the resolved path to remove.
fn check_removable(p: &Path, allowed: &[PathBuf], home: &Path) -> Result<PathBuf> {
    let shown = p.display();
    if p.as_os_str().is_empty() || !p.is_absolute() {
        bail!("refused: {shown:?}");
    }
    if p.components().any(|c| matches!(c, Component::ParentDir)) {
        bail!("refused: {shown}");
    }
    if p == Path::new("/") || p == home {
        bail!("refused: {shown}");
    }
    let Some(root) = allowed
        .iter()
        .find(|r| p.starts_with(r) && p != r.as_path())
    else {
        bail!("refused (outside the allowed roots): {shown}");
    };
    let (Some(parent), Some(name)) = (p.parent(), p.file_name()) else {
        bail!("refused: {shown}");
    };
    let croot = fs::canonicalize(root).with_context(|| format!("refused: {shown}"))?;
    let cparent = fs::canonicalize(parent).with_context(|| format!("refused: {shown}"))?;
    if croot == Path::new("/") || !cparent.starts_with(&croot) {
        bail!("refused (resolves outside {}): {shown}", root.display());
    }
    let target = cparent.join(name);
    let chome = fs::canonicalize(home).unwrap_or_else(|_| home.to_owned());
    if target == croot || target == chome || target == Path::new("/") {
        bail!("refused: {shown}");
    }
    Ok(target)
}

/// `rm -rf` limited by [`check_removable`]; never follows a symlink.
fn safe_rm(p: &Path, allowed: &[PathBuf], home: &Path) -> Result<()> {
    let target = check_removable(p, allowed, home)?;
    let md = match fs::symlink_metadata(&target) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("cannot remove {}", p.display())),
    };
    if md.is_dir() {
        fs::remove_dir_all(&target)
    } else {
        fs::remove_file(&target)
    }
    .with_context(|| format!("cannot remove {}", p.display()))
}

// --- filesystem helpers ---------------------------------------------------

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn name_of(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn is_real_dir(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())
}

/// Directory entries, sorted by name; empty on any error.
fn entries(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

/// Regular files anywhere below `root`, not following symlinks.
fn walk_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_owned()];
    while let Some(d) = stack.pop() {
        for e in entries(&d) {
            match fs::symlink_metadata(&e) {
                Ok(m) if m.is_dir() => stack.push(e),
                Ok(m) if m.is_file() => out.push(e),
                _ => {}
            }
        }
    }
    out.sort();
    out
}

/// True if `path` or anything below it (to `depth`) was modified after
/// `cutoff`. Does not follow symlinks.
fn newer_than(path: &Path, cutoff: i64, depth: Option<usize>) -> bool {
    let mut stack = vec![(path.to_owned(), 0usize)];
    while let Some((p, d)) = stack.pop() {
        let Ok(md) = fs::symlink_metadata(&p) else {
            continue;
        };
        if md.mtime() > cutoff || (md.mtime() == cutoff && md.mtime_nsec() > 0) {
            return true;
        }
        if md.is_dir() && depth.is_none_or(|max| d < max) {
            stack.extend(entries(&p).into_iter().map(|e| (e, d + 1)));
        }
    }
    false
}

/// Apparent size in bytes, like `du -sb`: every entry counted once, links
/// not followed. 0 if missing.
fn size_of(path: &Path) -> u64 {
    let mut seen = HashSet::new();
    let mut total = 0;
    let mut stack = vec![path.to_owned()];
    while let Some(p) = stack.pop() {
        let Ok(md) = fs::symlink_metadata(&p) else {
            continue;
        };
        if md.nlink() > 1 && !md.is_dir() && !seen.insert((md.dev(), md.ino())) {
            continue;
        }
        total += md.len();
        if md.is_dir() {
            stack.extend(entries(&p));
        }
    }
    total
}

/// Bytes in IEC units, like `numfmt --to=iec --suffix=B` (rounds up).
fn human(n: u64) -> String {
    const UNITS: [char; 8] = ['K', 'M', 'G', 'T', 'P', 'E', 'Z', 'Y'];
    if n < 1024 {
        return format!("{n}B");
    }
    let mut v = n as f64 / 1024.0;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if v < 10.0 {
        let r = (v * 10.0).ceil() / 10.0;
        if r < 10.0 {
            return format!("{r:.1}{}B", UNITS[i]);
        }
        v = r;
    }
    let r = v.ceil();
    if r >= 1024.0 && i < UNITS.len() - 1 {
        return format!("1.0{}B", UNITS[i + 1]);
    }
    format!("{}{}B", r as u64, UNITS[i])
}

/// Use% of the filesystem holding `path`, from `df`.
fn used_pct(path: &Path) -> Result<u32> {
    let out = Cmd::new("df")
        .arg("--output=pcent")
        .arg(path)
        .timeout(LOCAL)
        .checked()?;
    let digits: String = out
        .lines()
        .last()
        .unwrap_or_default()
        .chars()
        .filter(char::is_ascii_digit)
        .collect();
    digits
        .parse()
        .with_context(|| format!("cannot read disk usage of {}", path.display()))
}

/// `df -h` of the disks this cleans, one line per mount point.
fn print_df(ctx: &Ctx) {
    let Ok(o) = Cmd::new("df")
        .arg("-h")
        .args([&ctx.ws, &ctx.home, &ctx.tmp, Path::new("/var/lib/docker")])
        .timeout(LOCAL)
        .output()
    else {
        return;
    };
    print!("{}", dedupe_df(&o.stdout));
}

fn dedupe_df(out: &str) -> String {
    let mut seen = HashSet::new();
    let mut s = String::new();
    for (i, l) in out.lines().enumerate() {
        let mount = l.split_whitespace().last().unwrap_or_default();
        if i == 0 || seen.insert(mount.to_owned()) {
            s.push_str(l);
            s.push('\n');
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{File, FileTimes};

    /// Scratch tree, removed on drop.
    struct Fx(PathBuf);
    impl Fx {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "valet-clean-test-{tag}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(&p).unwrap();
            Fx(fs::canonicalize(&p).unwrap())
        }
        fn file(&self, rel: &str, age_secs: i64) -> PathBuf {
            let p = self.0.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, b"x").unwrap();
            age(&p, age_secs);
            p
        }
        fn dir(&self, rel: &str, age_secs: i64) -> PathBuf {
            let p = self.0.join(rel);
            fs::create_dir_all(&p).unwrap();
            age(&p, age_secs);
            p
        }
        fn ctx(&self) -> Ctx {
            let ws = self.0.join("ws");
            Ctx {
                mise_caches: ws.join("Userland/Applications/Mise/Caches"),
                repos: ws.join("Userland/Repos"),
                scratch: ws.join("Userland/Scratch"),
                ws,
                home: self.0.join("home"),
                claude: self.0.join("home/.claude"),
                codex: self.0.join("home/.codex"),
                tmp: self.0.join("tmp"),
                uid: fs::metadata("/proc/self").unwrap().uid(),
                days: 14,
                worktree_days: 3,
                now: now(),
                procs: Procs::default(),
            }
        }
    }
    impl Drop for Fx {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn age(p: &Path, secs: i64) {
        let t = SystemTime::now() - Duration::from_secs(secs as u64);
        File::open(p)
            .unwrap()
            .set_times(FileTimes::new().set_modified(t))
            .unwrap();
    }

    fn labels(r: &Report) -> Vec<String> {
        r.items.iter().map(|i| i.label.clone()).collect()
    }

    const H: i64 = 3600;

    #[test]
    fn scratch_lists_idle_throwaway_folders_only() {
        let fx = Fx::new("scratch");
        let old = 4 * DAY;
        let sb = "ws/Userland/Repos/h/o/wiki/tree/me/me/topic";
        fx.file(&format!("{sb}/Logs/run.log"), old);
        fx.dir(&format!("{sb}/Logs"), old);
        fx.file(&format!("{sb}/a/b/node_modules/x/i.js"), old);
        fx.dir(&format!("{sb}/a/b/node_modules/x"), old);
        fx.dir(&format!("{sb}/a/b/node_modules"), old);
        // Fresh file inside: the folder is in use.
        fx.file(&format!("{sb}/Builds/out.tgz"), H);
        fx.dir(&format!("{sb}/Builds"), old);
        // Not a throw-away name: never listed, however old.
        fx.file(&format!("{sb}/Sources/run.sh"), old);
        fx.dir(&format!("{sb}/Sources"), old);
        fx.file("ws/Userland/Repos/h/o/r/tree/main/target/release/x", old);
        fx.dir("ws/Userland/Repos/h/o/r/tree/main/target/release", old);
        fx.dir("ws/Userland/Repos/h/o/r/tree/main/target", old);
        // Userland/Scratch: any entry, 2 days; a fresh file keeps its folder.
        fx.file("ws/Userland/Scratch/dump.json", 3 * DAY);
        fx.file("ws/Userland/Scratch/gone/a/b.txt", 3 * DAY);
        fx.dir("ws/Userland/Scratch/gone/a", 3 * DAY);
        fx.dir("ws/Userland/Scratch/gone", 3 * DAY);
        fx.file("ws/Userland/Scratch/live/old.txt", 3 * DAY);
        fx.file("ws/Userland/Scratch/live/new.txt", H);
        fx.dir("ws/Userland/Scratch/live", 3 * DAY);
        fx.file("ws/Userland/Scratch/young.txt", DAY);
        let mut r = Report::default();
        scan_scratch(&fx.ctx(), &mut r);
        let mut got = labels(&r);
        got.sort();
        let root = fx.0.display();
        assert_eq!(
            got,
            vec![
                format!("{root}/ws/Userland/Repos/h/o/r/tree/main/target"),
                format!("{root}/{sb}/Logs"),
                format!("{root}/{sb}/a/b/node_modules"),
                format!("{root}/ws/Userland/Scratch/dump.json"),
                format!("{root}/ws/Userland/Scratch/gone"),
            ]
        );
    }

    #[test]
    fn human_matches_numfmt() {
        for (n, want) in [
            (0, "0B"),
            (1, "1B"),
            (1023, "1023B"),
            (1024, "1.0KB"),
            (1025, "1.1KB"),
            (1536, "1.5KB"),
            (10239, "10KB"),
            (10240, "10KB"),
            (10241, "11KB"),
            (102_400, "100KB"),
            (1_048_575, "1.0MB"),
            (1_048_576, "1.0MB"),
            (1_073_741_824, "1.0GB"),
            (5_000_000_000, "4.7GB"),
            (1_099_511_627_776, "1.0TB"),
        ] {
            assert_eq!(human(n), want, "{n}");
        }
    }

    #[test]
    fn docker_bytes() {
        assert_eq!(bytes("0B"), 0);
        assert_eq!(bytes(""), 0);
        assert_eq!(bytes("208MB (virtual 1.59GB)"), 208_000_000);
        assert_eq!(bytes("1.2kB"), 1200);
        assert_eq!(bytes("3.33333kB"), 3334);
        assert_eq!(bytes("17.92GB"), 17_920_000_000);
        assert_eq!(bytes("999B"), 999);
        assert_eq!(bytes("junk"), 0);
        assert_eq!(bytes("1.2XB"), 0);
    }

    #[test]
    fn created_dates() {
        assert_eq!(parse_created("1970-01-01 00:00:00 +0000"), Some(0));
        assert_eq!(
            parse_created("2026-11-05 12:25:30 +0530"),
            Some(1_793_861_730)
        );
        assert_eq!(
            parse_created("2026-11-05 06:55:30 +0000"),
            Some(1_793_861_730)
        );
        assert_eq!(
            parse_created("2026-11-04 22:55:30 -0800"),
            Some(1_793_861_730)
        );
        assert_eq!(parse_created("yesterday"), None);
        assert_eq!(log_iso(1_793_861_730), "2026-11-05T06:55:30Z");
    }

    fn log_iso(s: i64) -> String {
        crate::log::iso_from_unix(s)
    }

    #[test]
    fn docker_selection() {
        let fx = Fx::new("docker");
        let mut ctx = fx.ctx();
        ctx.now = parse_created("2026-11-05 00:00:00 +0000").unwrap();
        let ps = "a1|old|img:1|Exited (0) 3 weeks ago|2026-10-01 10:00:00 +0000 UTC|1.5MB (virtual 2GB)\n\
                  b2|new|img:2|Created|2026-11-01 10:00:00 +0000 UTC|0B\n\
                  c3|bad|img:3|Dead|garbage|1MB\n";
        let mut r = Report::default();
        docker_items(
            &ctx,
            ps,
            "1GB\n500MB\n",
            "Images|1GB (5%)\nBuild Cache|13.64GB\n",
            &mut r,
        );
        assert_eq!(
            labels(&r),
            ["container old (img:1)", "2 dangling images", "build cache"]
        );
        assert_eq!(
            r.items[0].why,
            "Exited (0) 3 weeks ago, created 2026-10-01 10:00:00 +0000"
        );
        assert_eq!(r.items[0].size, 1_500_000);
        assert_eq!(r.items[1].size, 1_500_000_000);
        assert_eq!(r.docker_containers, ["a1"]);
        assert!(r.docker_prune);
        assert!(r.items.iter().all(|i| i.path.is_none()));

        let mut r = Report::default();
        docker_items(&ctx, "", "", "Build Cache|0B\n", &mut r);
        assert!(r.items.is_empty() && !r.docker_prune);
    }

    #[test]
    fn porcelain_records_once() {
        let out = "worktree /r\nHEAD aaa\nbranch refs/heads/master\n\n\
                   worktree /r=x\nHEAD bbb\ndetached\n\n\
                   worktree /r=y\nHEAD ccc\nbranch refs/heads/feat/y\n\n";
        let w = parse_worktrees(out);
        assert_eq!(w.len(), 3);
        assert_eq!(
            (w[1].path.as_str(), w[1].head.as_str(), w[1].branch.as_str()),
            ("/r=x", "bbb", "")
        );
        assert_eq!(w[2].branch, "feat/y");
        assert_eq!(parse_worktrees("worktree /a\nHEAD x").len(), 1);
    }

    #[test]
    fn df_keeps_header_and_first_per_mount() {
        let out = "Filesystem Size Used Avail Use% Mounted on\n\
                   a 1G 1G 0 100% /ws\nb 1G 1G 0 100% /\na 1G 1G 0 100% /ws\n";
        assert_eq!(dedupe_df(out).lines().count(), 3);
    }

    #[test]
    fn tmp_names() {
        for n in [
            ".X11-unix",
            "claude-1",
            "herdr",
            "tmux-1",
            "systemd-private",
            "ssh-abc",
            "a.sock",
            "b.socket",
        ] {
            assert!(tmp_protected(n), "{n}");
        }
        assert!(!tmp_protected("build-123"));
    }

    #[test]
    fn refuses_root_home_empty_relative_dotdot_outside() {
        let fx = Fx::new("refuse");
        let ctx = fx.ctx();
        fx.dir("ws/a", 0);
        fx.dir("home/.claude/projects", 0);
        let allowed = ctx.allowed();
        for p in ["", "/", "relative/x"] {
            assert!(
                check_removable(Path::new(p), &allowed, &ctx.home).is_err(),
                "{p:?}"
            );
        }
        assert!(check_removable(&ctx.home, &allowed, &ctx.home).is_err());
        // The root itself, a path climbing out, and a sibling of the roots.
        assert!(check_removable(&ctx.ws, &allowed, &ctx.home).is_err());
        assert!(check_removable(&ctx.ws.join("a/../.."), &allowed, &ctx.home).is_err());
        assert!(check_removable(&fx.0.join("other"), &allowed, &ctx.home).is_err());
        assert!(
            check_removable(&ctx.home.join(".claude/settings.json"), &allowed, &ctx.home).is_err()
        );
        assert!(check_removable(&ctx.ws.join("a"), &allowed, &ctx.home).is_ok());
        // $HOME inside an allowed root is still refused.
        let allowed = vec![fx.0.clone()];
        assert!(check_removable(&ctx.home, &allowed, &ctx.home).is_err());
        assert!(check_removable(Path::new("/"), &[PathBuf::from("/")], &ctx.home).is_err());
    }

    #[test]
    fn refuses_symlink_escape_and_removes_links_not_targets() {
        let fx = Fx::new("symlink");
        let ctx = fx.ctx();
        let outside = fx.dir("outside", 0);
        let keep = fx.file("outside/keep", 0);
        fx.dir("ws", 0);
        std::os::unix::fs::symlink(&outside, ctx.ws.join("link")).unwrap();
        let allowed = ctx.allowed();
        // Something reached through a symlinked directory: refused.
        let err = safe_rm(&ctx.ws.join("link/keep"), &allowed, &ctx.home).unwrap_err();
        assert!(err.to_string().contains("resolves outside"), "{err}");
        assert!(keep.exists());
        // The symlink itself: only the link goes.
        safe_rm(&ctx.ws.join("link"), &allowed, &ctx.home).unwrap();
        assert!(keep.exists() && !ctx.ws.join("link").exists());
        // A directory holding a symlink out: contents of the target survive.
        fx.dir("ws/d", 0);
        std::os::unix::fs::symlink(&outside, ctx.ws.join("d/out")).unwrap();
        safe_rm(&ctx.ws.join("d"), &allowed, &ctx.home).unwrap();
        assert!(keep.exists() && !ctx.ws.join("d").exists());
        // Missing is fine, like rm -f.
        safe_rm(&ctx.ws.join("gone"), &allowed, &ctx.home).unwrap();
    }

    #[test]
    fn apps_old_rust_toolchains_and_downloads() {
        let fx = Fx::new("apps");
        let mut ctx = fx.ctx();
        let old = 20 * DAY;
        let home = fx.dir("ws/Userland/Applications/Rust/Data/Amd64/Rustup", 0);
        fx.dir(
            "ws/Userland/Applications/Rust/Data/Amd64/Rustup/toolchains/1.100.0-x86_64",
            0,
        );
        let tc = fx.dir(
            "ws/Userland/Applications/Rust/Data/Amd64/Rustup/toolchains/1.99.1-x86_64",
            0,
        );
        let hash = fx.file(
            "ws/Userland/Applications/Rust/Data/Amd64/Rustup/update-hashes/1.99.1-x86_64",
            0,
        );
        let busy = fx.dir(
            "ws/Userland/Applications/Rust/Data/Amd64/Rustup/toolchains/1.98.0-x86_64",
            0,
        );
        ctx.procs
            .mine
            .push(format!("{}/bin/rustc --x", busy.display()));
        let c = "ws/Userland/Applications/Mise/Caches/Amd64/downloads-cas/blobs";
        fx.file(&format!("{c}/aa/artifact"), old);
        let blob = fx.dir(&format!("{c}/aa"), old);
        fx.file(&format!("{c}/bb/artifact"), DAY);
        fx.dir(&format!("{c}/bb"), old);
        let mut r = Report::default();
        scan_rustup(&ctx, &[(home, "1.100.0".into())], &mut r);
        scan_mise_downloads(&ctx, &mut r);
        let want: Vec<String> = [tc, hash, blob]
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        assert_eq!(labels(&r), want);
        assert!(delete(&ctx, &r, false));
        assert!(r.items.iter().all(|i| !i.path.as_ref().unwrap().exists()));
        assert!(busy.exists() && fx.0.join(c).join("bb/artifact").exists());
        assert_eq!(
            prunable("go  1.26.0  ~/x/config.toml  1.27.4\n\nnode 20.1.0\n"),
            [
                ("go".to_owned(), "1.26.0".to_owned()),
                ("node".to_owned(), "20.1.0".to_owned())
            ]
        );
    }

    #[test]
    fn cores_tmp_ai() {
        let fx = Fx::new("misc");
        let mut ctx = fx.ctx();
        let old = 20 * DAY;
        let core = fx.file("ws/core.123", 2 * DAY);
        fx.file("ws/core.456", 3 * H);
        fx.file("ws/core.dump", 2 * DAY);
        let wtmp = fx.file("ws/tmp/old", old);
        fx.file("ws/tmp/new", DAY);
        let t1 = fx.file("tmp/build.log", old);
        fx.file("tmp/claude-x", old);
        fx.file("tmp/a.sock", old);
        let td = fx.dir("tmp/olddir", old);
        fx.dir("tmp/busydir", old);
        fx.file("tmp/busydir/inner", DAY);
        age(&fx.0.join("tmp/busydir"), old);
        let held = fx.file("tmp/held", old);
        ctx.procs.open.insert(held.display().to_string());
        let p = "home/.claude/projects/proj";
        let s1 = fx.file(&format!("{p}/s1.jsonl"), old);
        let s1d = fx.dir(&format!("{p}/s1"), 0);
        fx.file(&format!("{p}/s2.jsonl"), DAY);
        fx.file(&format!("{p}/memory/MEMORY.md"), old);
        fx.file("home/.claude/projects/top.jsonl", old);
        let cx = fx.file("home/.codex/sessions/2026/01/a.jsonl", old);
        let omp = fx.file("home/.omp/agent/sessions/x/b.jsonl", old);
        let pk = "home/.codex/packages/standalone";
        fx.dir(&format!("{pk}/releases/1"), 0);
        let r2 = fx.dir(&format!("{pk}/releases/2"), 0);
        fx.dir(&format!("{pk}/releases/3"), 0);
        std::os::unix::fs::symlink("releases/1", fx.0.join(pk).join("current")).unwrap();
        ctx.procs.mine.push(format!(
            "{}/codex --x",
            fx.0.join(pk).join("releases/3").display()
        ));

        let mut r = Report::default();
        for c in [Category::Cores, Category::Tmp, Category::Ai] {
            scan(&ctx, c, &mut r);
        }
        let want: Vec<String> = [core, wtmp, t1, td, s1, s1d, cx, omp, r2]
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        assert_eq!(labels(&r), want);

        // --run removes exactly those, inside the fixture.
        assert!(delete(&ctx, &r, false));
        for it in &r.items {
            assert!(!it.path.as_ref().unwrap().exists(), "{}", it.label);
        }
        assert!(held.exists() && fx.0.join(p).join("memory/MEMORY.md").exists());
        assert!(fx.0.join(pk).join("releases/1").exists());
    }

    #[test]
    fn in_use_matches_files_dirs_and_cwds() {
        let fx = Fx::new("inuse");
        let d = fx.dir("d", 0);
        let mut p = Procs::default();
        assert!(!p.in_use(&d));
        p.cwds.insert(d.join("sub").display().to_string());
        assert!(p.in_use(&d));
        let mut p = Procs::default();
        p.open.insert(format!("{}x/f", d.display())); // sibling prefix, not inside
        assert!(!p.in_use(&d));
        p.open.insert(format!("{}/f (deleted)", d.display()));
        assert!(p.in_use(&d));
        assert!(p.in_use(Path::new(&format!("{}/", d.display()))));
    }

    #[test]
    fn worktrees_clean_merged_or_stale() {
        let fx = Fx::new("wt");
        let mut ctx = fx.ctx();
        let repo = ctx.repos.join("h/o/widgets");
        let repo = &repo;
        fs::create_dir_all(repo).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            Cmd::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .timeout(LOCAL)
                .checked()
                .unwrap()
        };
        git(repo, &["init", "-q", "-b", "master"]);
        git(repo, &["remote", "add", "origin", "/nonexistent"]);
        git(repo, &["commit", "-q", "--allow-empty", "-m", "a"]);
        git(repo, &["update-ref", "refs/remotes/origin/master", "HEAD"]);
        let root = &repo.join(crate::repos::TREE);
        let wt = |n: &str| root.join(n);
        let s = |p: PathBuf| p.display().to_string();
        git(
            repo,
            &["worktree", "add", "-q", "--detach", &s(wt("merged"))],
        );
        git(
            repo,
            &["worktree", "add", "-q", "-b", "dirty", &s(wt("dirty"))],
        );
        fs::write(wt("dirty").join("f"), "x").unwrap();
        git(
            repo,
            &["worktree", "add", "-q", "-b", "ahead", &s(wt("ahead"))],
        );
        git(&wt("ahead"), &["commit", "-q", "--allow-empty", "-m", "b"]);
        // Pushed, unmerged: folded only once untouched --worktree-days.
        git(
            repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "me/pushed",
                &s(wt("me/pushed")),
            ],
        );
        git(
            &wt("me/pushed"),
            &["commit", "-q", "--allow-empty", "-m", "c"],
        );
        git(
            repo,
            &["update-ref", "refs/remotes/origin/me/pushed", "me/pushed"],
        );
        git(
            &wt("me/pushed"),
            &["branch", "-q", "--set-upstream-to", "origin/me/pushed"],
        );
        git(repo, &["worktree", "add", "-q", "--detach", &s(wt("gone"))]);
        fs::remove_dir_all(wt("gone")).unwrap();
        // A fold that could not delete its checkout: read-only inside.
        let trash = repo.join(".git/valet-trash/x-1");
        fs::create_dir_all(trash.join("ro")).unwrap();
        fs::write(trash.join("ro/f"), "x").unwrap();
        fs::set_permissions(
            trash.join("ro"),
            std::os::unix::fs::PermissionsExt::from_mode(0o555),
        )
        .unwrap();

        // Just made: a merged worktree waits 2 hours.
        let mut r = Report::default();
        scan_worktrees(&ctx, &mut r);
        assert_eq!(labels(&r), [s(trash.clone()), s(wt("gone"))]);

        ctx.now += 3 * H;
        let mut r = Report::default();
        scan_worktrees(&ctx, &mut r);
        assert_eq!(
            labels(&r),
            [s(trash.clone()), s(wt("gone")), s(wt("merged"))],
            "{:?}",
            r.items.iter().map(|i| &i.why).collect::<Vec<_>>()
        );
        assert_eq!(r.items[2].why, "clean, merged into origin/master");

        ctx.now += 4 * DAY;
        let mut r = Report::default();
        scan_worktrees(&ctx, &mut r);
        let got: Vec<&PathBuf> = r.wt_remove.iter().map(|(_, p)| p).collect();
        assert_eq!(got, [&wt("me/pushed"), &wt("merged")]);
        assert_eq!(
            r.items[2].why,
            "clean, nothing unpushed, untouched 3 days (me/pushed)"
        );

        // --run folds them: checkouts gone, branches kept, trash emptied.
        assert!(delete(&ctx, &r, true));
        assert!(!wt("merged").exists() && !wt("me").exists() && !trash.exists());
        assert!(wt("ahead").exists() && wt("dirty").exists());
        let list = git(repo, &["worktree", "list", "--porcelain"]);
        assert!(!list.contains("merged") && !list.contains("gone") && !list.contains("pushed"));
        git(
            repo,
            &["rev-parse", "--verify", "-q", "refs/heads/me/pushed"],
        );
        assert!(entries(&repo.join(".git/valet-trash")).is_empty());
    }

    #[test]
    fn merged_branches_selection_and_delete() {
        let l = |v: &[(&str, &str)]| -> Vec<(String, String)> {
            v.iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect()
        };
        let locals = l(&[
            ("main", "1"),
            ("used", "2"),
            ("anc", "3"),
            ("squash", "4"),
            ("squash-more", "5"),
            ("open", "6"),
        ]);
        let merged: HashSet<String> = ["main", "used", "anc"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let prs = vec![
            ("squash".to_owned(), "4".to_owned(), 7),
            ("squash-more".to_owned(), "x".to_owned(), 8),
        ];
        let got = deletable_branches(&locals, &["main".into(), "used".into()], &merged, &prs);
        assert_eq!(
            got,
            [
                ("anc".into(), "3".into(), "merged".into()),
                ("squash".into(), "4".into(), "PR #7 merged".into()),
            ]
        );

        // Scan and delete in a real repo (no GitHub: ancestry only).
        let fx = Fx::new("br");
        let ctx = fx.ctx();
        let repo = ctx.repos.join("h/o/r");
        fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            Cmd::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .timeout(LOCAL)
                .checked()
                .unwrap()
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["commit", "-q", "--allow-empty", "-m", "a"]);
        git(&["branch", "done"]);
        git(&["update-ref", "refs/remotes/origin/done", "done"]);
        git(&["config", "--add", "valet.take", "done"]);
        git(&["config", "branch.done.merge", "refs/heads/done"]);
        git(&["checkout", "-q", "-b", "wip"]);
        git(&["commit", "-q", "--allow-empty", "-m", "b"]);
        git(&["checkout", "-q", "main"]);
        git(&["update-ref", "refs/remotes/origin/main", "main"]);
        // Not a github.com slug: no PR lookups.
        let mut r = Report::default();
        let c = ctx;
        scan_branches(&c, &mut r);
        assert_eq!(labels(&r), ["h/o/r done"]);
        assert!(delete(&c, &r, false));
        let refs = git(&["for-each-ref", "--format=%(refname)"]);
        assert!(!refs.contains("done"), "{refs}");
        assert!(refs.contains("refs/heads/wip"));
        let take = Cmd::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["config", "--get-all", "valet.take"]);
        assert!(!take.timeout(LOCAL).output().unwrap().status.success());
        assert!(!git(&["config", "--list"]).contains("branch.done"));
    }
}
