//! The `repos` job: for each worktree of your repos (config.toml `owned`), commit local
//! changes, pull (rebase) and push. Never forces; a branch without an
//! upstream stays local. With `[job] private_patterns` in config.toml, public
//! repos get a commit only when no added line matches one of them.

use super::git;
use anyhow::{Context, Result};
use regex::Regex;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

/// A staged file larger than this stops the auto-commit.
const MAX_FILE: u64 = 10 << 20;
/// Re-check a repo's visibility after this long.
const VIS_TTL: u64 = 7 * 86_400;

/// `[job]` in config.toml.
#[derive(Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    /// Regexes (Rust `regex` syntax) for names that must never reach a public
    /// repo: hosts, private IPs, internal paths. Empty: no check.
    #[serde(default)]
    private_patterns: Vec<String>,
}

/// One regex matching any of PATTERNS; None when there are none.
fn private_regex(patterns: &[String]) -> Result<Option<Regex>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let alt: Vec<String> = patterns.iter().map(|p| format!("(?:{p})")).collect();
    Regex::new(&alt.join("|"))
        .map(Some)
        .context("config.toml: [job] private_patterns")
}

/// Files whose added lines (a `git diff -U0` text) match RE.
pub fn private_hits(diff: &str, re: &Regex) -> Vec<String> {
    let mut file = String::new();
    let mut hits: Vec<String> = Vec::new();
    for l in diff.lines() {
        if let Some(f) = l.strip_prefix("+++ b/") {
            file = f.to_owned();
        } else if l.starts_with('+')
            && !l.starts_with("+++")
            && re.is_match(l)
            && !hits.contains(&file)
        {
            hits.push(file.clone());
        }
    }
    hits
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// "public", "private", or "unknown" (treated as public), from an anonymous
/// request to the repo page: 200 is public, 404 private (or absent).
fn visibility(slug: &str) -> String {
    let code = crate::proc::output(
        "curl",
        &[
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "--max-time",
            "20",
            &format!("https://github.com/{slug}"),
        ],
        Duration::from_secs(30),
    )
    .unwrap_or_default();
    match code.trim() {
        "200" => "public",
        "404" => "private",
        _ => "unknown",
    }
    .to_owned()
}

/// The git operation in progress in worktree PATH (a rebase, merge or
/// cherry-pick), by the name of its state file.
pub(super) fn in_progress(path: &Path) -> Result<Option<&'static str>> {
    for p in [
        "rebase-merge",
        "rebase-apply",
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
    ] {
        let o = git(path, &["rev-parse", "--git-path", p], 10)?;
        if path.join(o.stdout.trim()).exists() || Path::new(o.stdout.trim()).exists() {
            return Ok(Some(p));
        }
    }
    Ok(None)
}

/// One repo's line in the report, or None when there was nothing to do.
/// GUARD: the private-names regex when the repo is public.
fn sync_repo(path: &Path, slug: &str, guard: Option<&Regex>, dry: bool) -> Result<Option<String>> {
    if let Some(p) = in_progress(path)? {
        return Ok(Some(format!("{slug}: {p} in progress; skipped")));
    }
    let branch = git(path, &["symbolic-ref", "--short", "-q", "HEAD"], 10)?;
    if !branch.status.success() {
        return Ok(Some(format!("{slug}: detached HEAD; skipped")));
    }
    let branch = branch.stdout.trim().to_owned();
    if dry {
        return dry_report(path, slug, &branch, guard);
    }
    let mut done = Vec::new();

    if !super::git_ok(path, &["status", "--porcelain"], 60)?.is_empty() {
        super::git_ok(path, &["add", "-A"], 120)?;
        let unstage = || super::git_ok(path, &["reset", "-q"], 60);
        let staged = super::git_ok(path, &["diff", "--cached", "--name-only"], 60)?;
        if let Some(big) = staged
            .lines()
            .find(|f| path.join(f).metadata().is_ok_and(|m| m.len() > MAX_FILE))
        {
            unstage()?;
            return Ok(Some(format!(
                "{slug}: not committed: {big} is over {} MiB",
                MAX_FILE >> 20
            )));
        }
        if let Some(re) = guard {
            let diff = super::git_ok(path, &["diff", "--cached", "-U0", "--no-color"], 120)?;
            let hits = private_hits(&diff, re);
            if !hits.is_empty() {
                unstage()?;
                return Ok(Some(format!(
                    "{slug}: public; not committed: private names in {}",
                    hits.join(", ")
                )));
            }
        }
        let msg = format!("sync: auto-commit from {}", super::hostname());
        let c = git(path, &["commit", "-q", "-m", &msg], 300)?;
        if !c.status.success() {
            unstage()?;
            let why = format!("{}{}", c.stderr, c.stdout);
            let why = why.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("");
            return Ok(Some(format!("{slug}: commit refused: {why}")));
        }
        done.push(format!("committed {} files", staged.lines().count()));
    }

    if !git(path, &["rev-parse", "--abbrev-ref", "@{upstream}"], 10)?
        .status
        .success()
    {
        return Ok((!done.is_empty()).then(|| {
            format!(
                "{slug} ({branch}): {}; no upstream, kept local",
                done.join(", ")
            )
        }));
    }
    let pull = git(path, &["pull", "--rebase", "-q"], 300)?;
    if !pull.status.success() {
        let _ = git(path, &["rebase", "--abort"], 60);
        return Ok(Some(format!(
            "{slug} ({branch}): {}pull failed: {}",
            done.iter().map(|d| format!("{d}; ")).collect::<String>(),
            pull.stderr.trim().lines().last().unwrap_or("")
        )));
    }
    let ahead: u32 = super::git_ok(path, &["rev-list", "--count", "@{upstream}..HEAD"], 30)?
        .parse()
        .context("rev-list count")?;
    if ahead > 0 {
        let push = git(path, &["push", "-q"], 300)?;
        if push.status.success() {
            done.push(format!("pushed {ahead} commits"));
        } else {
            done.push(format!(
                "push failed: {}",
                push.stderr.trim().lines().last().unwrap_or("")
            ));
        }
    }
    Ok((!done.is_empty()).then(|| format!("{slug} ({branch}): {}", done.join(", "))))
}

/// What [`sync_repo`] would do, changing nothing but the remote-tracking refs.
fn dry_report(
    path: &Path,
    slug: &str,
    branch: &str,
    guard: Option<&Regex>,
) -> Result<Option<String>> {
    let mut would = Vec::new();
    let st = super::git_ok(path, &["status", "--porcelain"], 60)?;
    if !st.is_empty() {
        would.push(format!("commit {} paths", st.lines().count()));
        if let Some(re) = guard {
            let diff = super::git_ok(path, &["diff", "HEAD", "-U0", "--no-color"], 120)?;
            let hits = private_hits(&diff, re);
            if !hits.is_empty() {
                would.push(format!(
                    "refuse (public; private names in {})",
                    hits.join(", ")
                ));
            }
        }
    }
    if git(path, &["rev-parse", "--abbrev-ref", "@{upstream}"], 10)?
        .status
        .success()
    {
        super::git_ok(path, &["fetch", "-q"], 300)?;
        let lr = super::git_ok(
            path,
            &["rev-list", "--left-right", "--count", "@{upstream}...HEAD"],
            30,
        )?;
        let mut it = lr.split_whitespace();
        let (behind, ahead) = (it.next().unwrap_or("0"), it.next().unwrap_or("0"));
        if behind != "0" {
            would.push(format!("pull {behind} commits"));
        }
        if ahead != "0" {
            would.push(format!("push {ahead} commits"));
        }
    } else if !would.is_empty() {
        would.push("keep local (no upstream)".into());
    }
    Ok((!would.is_empty()).then(|| format!("{slug} ({branch}): would {}", would.join(", "))))
}

pub fn run(vis: &mut BTreeMap<String, (String, u64)>, dry: bool) -> Result<Vec<String>> {
    let m = crate::repos::manifest()?;
    let cfg: Config = crate::config::section("job")?;
    let private = private_regex(&cfg.private_patterns)?;
    let mut rep = Vec::new();
    let mut clean = 0;
    for r in crate::repos::all()?
        .into_iter()
        .filter(|r| m.owned.iter().any(|o| r.slug.starts_with(&format!("{o}/"))))
    {
        // `owner/name` for GitHub's API.
        let slug = r
            .slug
            .split_once('/')
            .map_or(r.slug.as_str(), |x| x.1)
            .to_owned();
        // Visibility matters only when there are names to keep out.
        let guard = match &private {
            None => None,
            Some(re) => {
                let fresh = vis
                    .get(&slug)
                    .is_some_and(|(v, t)| v != "unknown" && now() - t < VIS_TTL);
                if !fresh {
                    vis.insert(slug.clone(), (visibility(&slug), now()));
                }
                (vis[&slug].0 != "private").then_some(re)
            }
        };
        for (wt, _) in r.worktrees()? {
            match sync_repo(&wt, &slug, guard, dry) {
                Ok(Some(line)) => rep.push(line),
                Ok(None) => clean += 1,
                Err(e) => rep.push(format!("{slug}: {e:#}")),
            }
        }
    }
    rep.push(format!("{clean} worktrees already in sync"));
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_names_are_found_in_added_lines_only() {
        let re = private_regex(&[
            r"\bintranet\.example\.com\b".into(),
            r"\b192\.0\.2\.\d{1,3}\b".into(),
        ])
        .unwrap()
        .unwrap();
        let diff = "\
diff --git a/a.md b/a.md
+++ b/a.md
-ssh build.intranet.example.com
+ssh some-host
+++ b/b.sh
+curl https://build.intranet.example.com/x
+++ b/c.txt
+ip 192.0.2.7
+++ b/d.txt
+version 192.0.3.1, example.com ok
";
        assert_eq!(private_hits(diff, &re), ["b.sh", "c.txt"]);
        assert!(private_regex(&[]).unwrap().is_none());
        assert!(private_regex(&["(".into()]).is_err());
    }
}
