//! `valet job`: background jobs on a daily timer.
//!
//! Jobs run in a fixed order ([`JOBS`]) daily on one host (the `job` part
//! of `valet daemon` starts ad-hoc job `run-daily`), or by hand with `valet
//! job run`. Each job's last result goes to
//! `<valet Data>/job/state.json`, every step to `job/job.log`.
//! One run at a time (a lock file).

mod adhoc;
mod home;
mod repos;
mod upstream;

use crate::proc::Cmd;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Jobs in run order: (name, what it does).
pub const JOBS: &[(&str, &str)] = &[
    (
        "upgrade",
        "fetch the valet repo; on new upstream commits run `valet upgrade`, then continue on the new binary",
    ),
    (
        "home",
        "capture the dotfiles listed in the `dotfiles` checkout's files.txt as templates",
    ),
    (
        "repos",
        "auto-commit, pull and push your repos (config.toml `owned`); public ones only when no added line matches `[job] private_patterns`",
    ),
    (
        "upstream",
        "fetch repos you do not own; fast-forward the default branch, rebase other branches onto their upstream (else origin's default) around a stash; a conflict leaves the worktree as it was, with a WARN line",
    ),
    (
        "tools",
        "report pinned tools that have a newer upstream release",
    ),
    (
        "clean",
        "free disk space: `valet clean --run cores tmp docker scratch worktrees apps` (transcripts left alone)",
    ),
];

/// The `valet daemon` part that starts the daily run.
const PART: &str = "job";
/// What the `clean` job passes to `valet clean`: regenerable files only.
/// Session transcripts (`ai`) are left to a manual `valet clean`; worktrees fold
/// (their branches stay).
const CLEAN_CATEGORIES: &[&str] = &["cores", "tmp", "docker", "scratch", "worktrees", "apps"];
/// Set for commands a sync run starts, so `valet upgrade` there does not start
/// another run.
pub const IN_JOB_ENV: &str = "VALET_JOB_RUN";
const LOG_MAX: u64 = 1 << 20;

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    cmd: Sub,
}

#[derive(clap::Subcommand)]
enum Sub {
    /// Jobs in run order, and whether each is enabled.
    List,
    /// Timer state, each job's last run and report, and ad-hoc jobs.
    ///
    /// With CLASS (for example `run`): only ad-hoc jobs whose id starts with it.
    Status { class: Option<String> },
    /// Run jobs now: the named ones, else every enabled job, in order.
    Run(RunArgs),
    /// Turn the daily timer on (no job), or re-enable one job.
    Enable { job: Option<String> },
    /// Turn the daily timer off (no job), or skip one job in later runs.
    Disable { job: Option<String> },
    /// Dotfiles kept as templates in the `dotfiles` checkout: diff, capture, apply.
    Home(home::Args),
    /// Show the end of the job log, or of ad-hoc job ID's log.
    Log {
        id: Option<String>,
        #[arg(short = 'n', long, default_value_t = 60)]
        lines: usize,
    },
    /// Stop the running ad-hoc job ID.
    Stop { id: String },
}

#[derive(clap::Args)]
struct RunArgs {
    /// Jobs to run (see `valet job list`).
    jobs: Vec<String>,
    /// Wait for a running sync to finish instead of failing.
    #[arg(long)]
    wait: bool,
    /// Report what each job would do; change nothing (nothing is recorded).
    #[arg(long, short = 'n')]
    dry_run: bool,
    /// Started by the timer: exit quietly on a host other than the one that
    /// enabled it (the home dir, and so the unit, is shared between VMs).
    #[arg(long, hide = true)]
    scheduled: bool,
    /// Set when re-executed on a freshly upgraded binary.
    #[arg(long, hide = true)]
    after_upgrade: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    /// Jobs skipped by runs that name no jobs.
    #[serde(default)]
    disabled: Vec<String>,
    /// Host that enabled the timer.
    #[serde(default)]
    timer_host: Option<String>,
    #[serde(default)]
    jobs: BTreeMap<String, JobRun>,
    /// Repo visibility cache for the repos job: repo -> (visibility, unix time).
    #[serde(default)]
    visibility: BTreeMap<String, (String, u64)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JobRun {
    start: String,
    secs: u64,
    ok: bool,
    report: Vec<String>,
}

/// What a job did.
pub enum Outcome {
    Done(Vec<String>),
    /// The binary was replaced; continue the run on the new one.
    Upgraded(Vec<String>),
}

pub fn run(a: Args) -> Result<()> {
    match a.cmd {
        Sub::List => list(),
        Sub::Status { class: None } => status(),
        Sub::Status { class: Some(c) } => adhoc_status(&c),
        Sub::Run(r) => run_jobs(r),
        Sub::Enable { job: None } => enable_timer(),
        Sub::Disable { job: None } => disable_timer(),
        Sub::Enable { job: Some(j) } => set_job(&j, true),
        Sub::Disable { job: Some(j) } => set_job(&j, false),
        Sub::Home(h) => home::run(h),
        Sub::Log { id: None, lines } => show_log(lines),
        Sub::Log {
            id: Some(id),
            lines,
        } => adhoc::show_log(&id, lines),
        Sub::Stop { id } => adhoc::stop(&id),
    }
}

// ---------- state, lock, log ----------

fn dir() -> Result<PathBuf> {
    Ok(crate::cmd::apps::self_dir("Data")?.join("job"))
}

fn load() -> Result<State> {
    let p = dir()?.join("state.json");
    match fs::read_to_string(&p) {
        Ok(t) => serde_json::from_str(&t).with_context(|| format!("parse {}", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(e) => Err(e).with_context(|| format!("read {}", p.display())),
    }
}

fn save(s: &State) -> Result<()> {
    let d = dir()?;
    fs::create_dir_all(&d)?;
    let tmp = d.join(format!("state.json.tmp.{}", std::process::id()));
    fs::write(&tmp, serde_json::to_string_pretty(s)? + "\n")?;
    fs::rename(&tmp, d.join("state.json"))?;
    Ok(())
}

/// Held for a whole run; released on drop or exec (the fd is close-on-exec).
fn lock(wait: bool) -> Result<fs::File> {
    let d = dir()?;
    fs::create_dir_all(&d)?;
    let f = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(d.join("lock"))?;
    if wait {
        f.lock()?;
    } else if f.try_lock().is_err() {
        bail!("another `valet job run` is in progress (see `valet job log`)");
    }
    Ok(f)
}

/// Append `<ISO time> <LEVEL> <msg>` to the log (rotated at 1 MiB) and stderr.
fn log(level: &str, msg: &str) {
    let line = format!("{} {level} {msg}", crate::log::now_iso());
    eprintln!("{line}");
    let Ok(d) = dir() else { return };
    let _ = fs::create_dir_all(&d);
    let p = d.join("job.log");
    if fs::metadata(&p).is_ok_and(|m| m.len() > LOG_MAX) {
        let _ = fs::rename(&p, d.join("job.log.1"));
    }
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = writeln!(f, "{line}");
    }
}

fn hostname() -> String {
    fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|h| h.trim().to_owned())
        .unwrap_or_default()
}

fn check_job(j: &str) -> Result<()> {
    if JOBS.iter().any(|(n, _)| *n == j) {
        Ok(())
    } else {
        bail!(
            "no job {j:?}; jobs: {}",
            JOBS.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
        )
    }
}

/// Jobs a run does: the named ones in [`JOBS`] order, else the enabled ones.
fn plan(named: &[String], disabled: &[String]) -> Vec<&'static str> {
    JOBS.iter()
        .map(|(n, _)| *n)
        .filter(|n| {
            if named.is_empty() {
                !disabled.iter().any(|d| d == n)
            } else {
                named.iter().any(|x| x == n)
            }
        })
        .collect()
}

// ---------- commands ----------

fn list() -> Result<()> {
    let st = load()?;
    for (n, what) in JOBS {
        let on = if st.disabled.iter().any(|d| d == n) {
            "off"
        } else {
            "on "
        };
        println!("{n:8} {on} {what}");
    }
    Ok(())
}

fn status() -> Result<()> {
    let st = load()?;
    if crate::cmd::daemon::part_on(PART)? {
        let next = crate::cmd::daemon::next_job_run().map_or("?".into(), crate::log::iso_from_unix);
        println!(
            "timer: on (host {}), next run ~{next}",
            st.timer_host.as_deref().unwrap_or("?"),
        );
    } else {
        println!("timer: off (`valet job enable` turns it on)");
    }
    for (n, _) in JOBS {
        let off = if st.disabled.iter().any(|d| d == n) {
            " (disabled)"
        } else {
            ""
        };
        match st.jobs.get(*n) {
            None => println!("{n}{off}: never run"),
            Some(r) => {
                let res = if r.ok { "ok" } else { "FAILED" };
                println!("{n}{off}: {res} at {} ({}s)", r.start, r.secs);
                for l in &r.report {
                    println!("    {l}");
                }
            }
        }
    }
    adhoc_status("")
}

/// Ad-hoc jobs whose id starts with `class`: running ones, then finished.
fn adhoc_status(class: &str) -> Result<()> {
    let rows = adhoc::list(class)?;
    let label = if class.is_empty() { "ad-hoc" } else { class };
    println!("{label} jobs ({}):", rows.len());
    for (id, running, last) in rows {
        let state = if running { "running" } else { "done" };
        println!("    {id:36} {state:8} {last}");
    }
    Ok(())
}

fn show_log(lines: usize) -> Result<()> {
    let p = dir()?.join("job.log");
    let text = fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
    let all: Vec<&str> = text.lines().collect();
    for l in &all[all.len().saturating_sub(lines)..] {
        println!("{l}");
    }
    Ok(())
}

fn set_job(job: &str, on: bool) -> Result<()> {
    check_job(job)?;
    let mut st = load()?;
    st.disabled.retain(|d| d != job);
    if !on {
        st.disabled.push(job.to_owned());
    }
    save(&st)?;
    println!("{job}: {}", if on { "enabled" } else { "disabled" });
    Ok(())
}

fn run_jobs(r: RunArgs) -> Result<()> {
    for j in &r.jobs {
        check_job(j)?;
    }
    let mut st = load()?;
    if r.scheduled {
        let me = hostname();
        if st.timer_host.as_deref().is_some_and(|h| h != me) {
            log(
                "INFO",
                &format!(
                    "timer fired on {me}, but it belongs to {}; nothing to do",
                    st.timer_host.as_deref().unwrap_or("?")
                ),
            );
            return Ok(());
        }
    }
    let jobs = plan(&r.jobs, &st.disabled);
    let _lock = lock(r.wait)?;
    // A run that waited may see state another run wrote.
    st = load()?;
    let mut failed = Vec::new();
    for (i, job) in jobs.iter().enumerate() {
        if r.after_upgrade && *job == "upgrade" {
            continue;
        }
        log("INFO", &format!("{job}: start"));
        let started = (crate::log::now_iso(), Instant::now());
        let res = run_job(job, &mut st, r.dry_run);
        let secs = started.1.elapsed().as_secs();
        let (ok, report, upgraded) = match res {
            Ok(Outcome::Done(rep)) => (true, rep, false),
            Ok(Outcome::Upgraded(rep)) => (true, rep, true),
            Err(e) => (false, vec![format!("{e:#}")], false),
        };
        for l in &report {
            log(if ok { "INFO" } else { "ERROR" }, &format!("{job}: {l}"));
        }
        log(
            if ok { "INFO" } else { "ERROR" },
            &format!("{job}: {} in {secs}s", if ok { "done" } else { "failed" }),
        );
        if !ok {
            failed.push(*job);
        }
        if r.dry_run {
            continue;
        }
        st.jobs.insert(
            (*job).to_owned(),
            JobRun {
                start: started.0,
                secs,
                ok,
                report,
            },
        );
        save(&st)?;
        if upgraded {
            let rest: Vec<&str> = jobs[i + 1..].to_vec();
            if rest.is_empty() {
                return Ok(());
            }
            let exe = crate::cmd::apps::self_bin()?;
            log(
                "INFO",
                &format!("continuing on the new binary: {}", rest.join(" ")),
            );
            Cmd::new(exe)
                .args(["job", "run", "--wait", "--after-upgrade"])
                .args(&rest)
                .exec()?;
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        bail!("failed: {} (see `valet job status`)", failed.join(", "))
    }
}

fn run_job(job: &str, st: &mut State, dry: bool) -> Result<Outcome> {
    match job {
        "upgrade" => upgrade_job(dry),
        "home" if crate::repos::alias_slug("dotfiles").is_none() => Ok(Outcome::Done(vec![
            "no `dotfiles` alias in config.toml; skipped".into(),
        ])),
        "home" if dry => home::changed().map(Outcome::Done),
        "home" => home::capture().map(Outcome::Done),
        "repos" => repos::run(&mut st.visibility, dry).map(Outcome::Done),
        "upstream" => upstream::run(dry).map(Outcome::Done),
        "clean" => clean_job(dry),
        "tools" => tools_job(),
        _ => unreachable!("checked against JOBS"),
    }
}

// ---------- jobs ----------

fn git(repo: &Path, args: &[&str], secs: u64) -> Result<crate::proc::Output> {
    Cmd::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .timeout(Duration::from_secs(secs))
        .output()
}

fn git_ok(repo: &Path, args: &[&str], secs: u64) -> Result<String> {
    let o = git(repo, args, secs)?;
    if !o.status.success() {
        bail!("git {} failed: {}", args.join(" "), o.stderr.trim());
    }
    Ok(o.stdout.trim().to_owned())
}

fn upgrade_job(dry: bool) -> Result<Outcome> {
    let repo = &crate::cmd::upgrade::repo();
    git_ok(repo, &["fetch", "--quiet"], 120)?;
    if !git(repo, &["rev-parse", "--abbrev-ref", "@{upstream}"], 10)?
        .status
        .success()
    {
        return Ok(Outcome::Done(vec![
            "branch has no upstream; skipped".into(),
        ]));
    }
    let behind: u32 = git_ok(repo, &["rev-list", "--count", "HEAD..@{upstream}"], 30)?
        .parse()
        .context("rev-list count")?;
    let head = git_ok(repo, &["rev-parse", "--short", "HEAD"], 10)?;
    if behind == 0 {
        return Ok(Outcome::Done(vec![format!("up to date at {head}")]));
    }
    if !git_ok(repo, &["status", "--porcelain"], 30)?.is_empty() {
        return Ok(Outcome::Done(vec![format!(
            "{behind} new upstream commits, but the checkout has local changes; not upgrading"
        )]));
    }
    if dry {
        return Ok(Outcome::Done(vec![format!(
            "would upgrade: {behind} new upstream commits"
        )]));
    }
    let exe = crate::cmd::apps::self_bin()?;
    Cmd::new(exe)
        .arg("upgrade")
        .env(IN_JOB_ENV, "1")
        .timeout(Duration::from_secs(3600))
        .run()?;
    let new = git_ok(repo, &["rev-parse", "--short", "HEAD"], 10)?;
    Ok(Outcome::Upgraded(vec![format!("upgraded {head} -> {new}")]))
}

/// Worktrees untouched this long are folded by the daily clean job.
const REPOS_IDLE_DAYS: u64 = 3;

fn clean_job(dry: bool) -> Result<Outcome> {
    let exe = crate::cmd::apps::self_bin()?;
    let mut c = Cmd::new(&exe).arg("clean");
    if !dry {
        c = c.arg("--run");
    }
    let out = c
        .args(CLEAN_CATEGORIES)
        .env(IN_JOB_ENV, "1")
        .timeout(Duration::from_secs(3600))
        .output()?;
    if !out.status.success() {
        bail!("valet clean failed: {}", out.stderr.trim());
    }
    // The summary (`total: ...`, freed/report line) and the data disk usage.
    let rep: Vec<String> = out
        .stdout
        .lines()
        .filter(|l| {
            l.starts_with("total:")
                || l.starts_with("freed")
                || l.starts_with("report only")
                // The disk usage row (`df`: 5th field is `NN%`).
                || l.split_whitespace()
                    .nth(4)
                    .is_some_and(|u| u.ends_with('%'))
        })
        .map(str::to_owned)
        .collect();
    let mut rep = if rep.is_empty() {
        vec!["nothing to clean".into()]
    } else {
        rep
    };
    // Commands links follow the manifest: drop links of removed apps and
    // links whose binary is gone.
    if !dry {
        let link = Cmd::new(&exe)
            .args(["apps", "link", "--arch", "all"])
            .env(IN_JOB_ENV, "1")
            .timeout(Duration::from_secs(600))
            .output()?;
        rep.push(if link.status.success() {
            "apps: commands relinked".into()
        } else {
            format!("apps link failed: {}", link.stderr.trim())
        });
    }
    // Idle worktrees fold back into branches (recreated on use).
    match crate::repos::gc(Duration::from_secs(REPOS_IDLE_DAYS * 86_400), dry) {
        Ok(v) if v.is_empty() => {}
        Ok(v) => rep.extend(v),
        Err(e) => rep.push(format!("repos gc failed: {e:#}")),
    }
    rep.extend(missing_path_dirs());
    Ok(Outcome::Done(rep))
}

/// PATH entries that do not exist: in a fresh login zsh (zshenv and
/// zprofile; not zshrc, whose cleanup would hide them) and in this job.
fn missing_path_dirs() -> Vec<String> {
    let login = Cmd::new("zsh")
        .args(["-l", "-c", "print -rl -- $path"])
        .timeout(Duration::from_secs(60))
        .output()
        .map(|o| o.stdout)
        .unwrap_or_default();
    let here = std::env::var("PATH").unwrap_or_default();
    let mut seen = std::collections::BTreeSet::new();
    login
        .lines()
        .map(str::to_owned)
        .chain(here.split(':').map(str::to_owned))
        .filter(|d| !d.is_empty() && seen.insert(d.clone()))
        .filter(|d| !std::path::Path::new(d).is_dir())
        .map(|d| format!("PATH dir missing: {d} (drop it from the shell startup files)"))
        .collect()
}

fn tools_job() -> Result<Outcome> {
    let gh = crate::tools::resolve("gh")?;
    let mut rep = Vec::new();
    let mut checked = 0;
    for (name, _, url) in crate::tools::pins() {
        let Some((repo, tag)) = crate::tools::release_of(url) else {
            continue;
        };
        let out = Cmd::new(&gh)
            .args([
                "api",
                &format!("repos/{repo}/releases/latest"),
                "--jq",
                ".tag_name",
            ])
            .timeout(Duration::from_secs(30))
            .output()?;
        if !out.status.success() {
            rep.push(format!(
                "{name}: latest release unknown ({})",
                out.stderr.trim()
            ));
            continue;
        }
        checked += 1;
        let latest = out.stdout.trim();
        if latest != tag {
            rep.push(format!("{name}: pinned {tag}, latest {latest}"));
        }
    }
    if rep.is_empty() {
        rep.push(format!("all {checked} pins are the latest release"));
    } else {
        rep.push("bump a pin in src/tools.rs (digest from `gh api .../releases/tags/<tag>`), then `valet upgrade`".into());
    }
    Ok(Outcome::Done(rep))
}

// ---------- schedule ----------

/// zsh, which the daemon and ad-hoc jobs run through: it reads `.zshenv`
/// (USERLAND, PATH, app env), so they get the environment every shell gets,
/// computed fresh rather than copied from whichever shell installed them.
pub(crate) fn zsh() -> std::path::PathBuf {
    crate::proc::which("zsh").unwrap_or_else(|| "/usr/bin/zsh".into())
}

/// argv running EXE through zsh, for `systemd-run` (no `$` escaping).
pub(crate) fn zsh_argv(exe: &Path) -> Vec<std::ffi::OsString> {
    vec![
        zsh().into(),
        "-c".into(),
        "exec \"$@\"".into(),
        "zsh".into(),
        exe.into(),
    ]
}

fn enable_timer() -> Result<()> {
    let mut st = load()?;
    st.timer_host = Some(hostname());
    save(&st)?;
    crate::cmd::daemon::ensure(PART)?;
    println!(
        "timer on for {} (daily ~03:30); `valet job status` shows the next run",
        hostname()
    );
    Ok(())
}

/// A scheduled run stops after this (a hung fetch or ssh must not hold the
/// next day's run off).
const RUN_LIMIT: std::time::Duration = std::time::Duration::from_secs(3 * 60 * 60);

/// Ad-hoc job `run-<what>`: a transient unit outside the daemon, so the
/// `valet upgrade` it may run can restart the daemon. Running already: left alone.
fn start_run(what: &str, desc: &str, args: &[&str]) -> Result<()> {
    let id = adhoc::id(&["run", what]);
    if adhoc::active(&id) {
        return Ok(());
    }
    let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    adhoc::start(
        &id,
        desc,
        &crate::cmd::home()?,
        &args,
        false,
        Some(RUN_LIMIT),
    )
}

/// The daily run of every enabled job (the daemon's `job` part calls this when due).
pub fn start_daily() -> Result<()> {
    start_run(
        "daily",
        "every enabled job",
        &["job", "run", "--wait", "--scheduled"],
    )
}

fn disable_timer() -> Result<()> {
    crate::cmd::daemon::set_part(PART, false)?;
    let mut st = load()?;
    st.timer_host = None;
    save(&st)?;
    println!("timer off");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_keeps_job_order_and_honours_disabled() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(plan(&s(&["tools", "upgrade"]), &[]), ["upgrade", "tools"]);
        assert_eq!(
            plan(&[], &s(&["repos", "tools"])),
            ["upgrade", "home", "upstream", "clean"]
        );
        // A named job runs even when disabled.
        assert_eq!(plan(&s(&["repos"]), &s(&["repos"])), ["repos"]);
    }

    #[test]
    fn unknown_job_is_an_error() {
        assert!(check_job("upstream").is_ok());
        assert!(check_job("nope").is_err());
    }
}
