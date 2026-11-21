//! Ad-hoc jobs: one-shot background commands (for example `run-daily`) run
//! as transient systemd user units `valet-job-<id>.service`, so they outlive
//! the shell or Claude session that started them. The id starts with a class
//! (`run-`): `valet job status run` and `systemctl --user list-units
//! 'valet-job-run-*'` filter on it.
//!
//! Each job logs `<ISO time> <LEVEL> <msg>` lines to
//! `<valet Data>/job/adhoc/<id>.log`; finished jobs are listed from
//! their logs for [`KEEP_DAYS`].

use anyhow::{Context, Result, bail};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const UNIT_PREFIX: &str = "valet-job-";
const KEEP_DAYS: u64 = 14;
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(15);

fn unit(id: &str) -> String {
    format!("{UNIT_PREFIX}{id}.service")
}

fn dir() -> Result<PathBuf> {
    Ok(super::dir()?.join("adhoc"))
}

/// Job ids are `<class>-<name>`: lowercase letters, digits, `-`.
pub fn id(parts: &[&str]) -> String {
    parts
        .iter()
        .map(|p| {
            p.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() {
                        c.to_ascii_lowercase()
                    } else {
                        '-'
                    }
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("-")
}

pub(super) fn active(id: &str) -> bool {
    crate::proc::succeeds(
        "systemctl",
        &["--user", "is-active", "--quiet", &unit(id)],
        SYSTEMCTL_TIMEOUT,
    )
}

/// Start `valet <args>` as job `id` in `cwd`, stopped after `limit` when set.
/// A running job with the same id is replaced when `replace`, else refused.
pub fn start(
    id: &str,
    desc: &str,
    cwd: &Path,
    args: &[String],
    replace: bool,
    limit: Option<std::time::Duration>,
) -> Result<()> {
    if active(id) {
        if !replace {
            bail!("job {id} is already running (`valet job log {id}`, `valet job stop {id}`)");
        }
        stop(id)?;
    }
    let exe = std::env::current_exe().context("path of this valet binary")?;
    let d = dir()?;
    fs::create_dir_all(&d)?;
    let _ = fs::remove_file(d.join(format!("{id}.log")));
    let cmd = crate::proc::Cmd::new("systemd-run").args([
        "--user".to_owned(),
        "--quiet".to_owned(),
        "--collect".to_owned(),
        format!("--unit={}", unit(id)),
        format!("--description=valet job {id}: {desc}"),
        format!("--working-directory={}", cwd.display()),
        "--property=Nice=10".to_owned(),
        format!("--setenv={}=1", super::IN_JOB_ENV),
    ]);
    let cmd = match limit {
        Some(l) => cmd.arg(format!("--property=RuntimeMaxSec={}", l.as_secs())),
        None => cmd,
    };
    // Through zsh: the environment every shell gets, not this one's.
    cmd.arg("--")
        .args(super::zsh_argv(&exe))
        .args(args)
        .timeout(SYSTEMCTL_TIMEOUT)
        .checked()
        .with_context(|| format!("start job {id}"))?;
    log_to(id, "INFO", &format!("started: {desc}"));
    Ok(())
}

pub fn stop(id: &str) -> Result<()> {
    crate::proc::output(
        "systemctl",
        &["--user", "stop", &unit(id)],
        SYSTEMCTL_TIMEOUT,
    )?;
    log_to(id, "INFO", "stopped");
    Ok(())
}

fn log_to(id: &str, level: &str, msg: &str) {
    let line = format!("{} {level} {msg}", crate::log::now_iso());
    eprintln!("{line}");
    let Ok(d) = dir() else { return };
    let _ = fs::create_dir_all(&d);
    if let Ok(mut f) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(d.join(format!("{id}.log")))
    {
        let _ = writeln!(f, "{line}");
    }
}

/// `(id, running, last log line)` for jobs whose id starts with `class`
/// (all when empty), newest first; drops logs older than [`KEEP_DAYS`].
pub fn list(class: &str) -> Result<Vec<(String, bool, String)>> {
    let Ok(rd) = fs::read_dir(dir()?) else {
        return Ok(Vec::new());
    };
    let old = SystemTime::now() - Duration::from_secs(KEEP_DAYS * 86_400);
    let mut rows = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        let Some(id) = p
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".log"))
            .map(str::to_owned)
        else {
            continue;
        };
        let mtime = e.metadata()?.modified()?;
        let running = active(&id);
        if mtime < old && !running {
            let _ = fs::remove_file(&p);
            continue;
        }
        if !id.starts_with(class) {
            continue;
        }
        let last = fs::read_to_string(&p)
            .unwrap_or_default()
            .lines()
            .last()
            .unwrap_or_default()
            .to_owned();
        rows.push((mtime, id, running, last));
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.0));
    Ok(rows.into_iter().map(|(_, i, r, l)| (i, r, l)).collect())
}

/// The log of job `id`, last `lines` lines.
pub fn show_log(id: &str, lines: usize) -> Result<()> {
    let p = dir()?.join(format!("{id}.log"));
    let text = fs::read_to_string(&p).with_context(|| format!("no log for job {id}"))?;
    let all: Vec<&str> = text.lines().collect();
    for l in &all[all.len().saturating_sub(lines)..] {
        println!("{l}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unit_safe() {
        assert_eq!(
            id(&["run", "daily-sync", "widgets", "42"]),
            "run-daily-sync-widgets-42"
        );
        assert_eq!(id(&["run", "Re/Sync", "a.b"]), "run-re-sync-a-b");
        assert_eq!(unit("run-x"), "valet-job-run-x.service");
    }
}
