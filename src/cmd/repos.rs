//! `valet repos`: the git checkouts under `<Userland>/Repos` (layout and
//! fold/recreate model in [`crate::repos`]).

use crate::repos::{self, Repo};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// Clone the repos in config.toml that are missing; check out their listed branches.
    Sync {
        #[arg(short = 'n', long)]
        dry_run: bool,
    },
    /// Clone URL into <Userland>/Repos (bare + home-branch worktree), add it
    /// to config.toml, print the worktree path. A plain `git clone URL` in
    /// zsh runs this.
    Clone { url: String },
    /// Line config.toml and git up with the folders: drop deleted repos,
    /// add unlisted ones, prune deleted worktrees (branches stay). Runs on
    /// every change (`valet repos watch`).
    Reconcile {
        #[arg(short = 'n', long)]
        dry_run: bool,
    },
    /// Watch repo and worktree folders and reconcile on change (the `repos`
    /// part of `valet daemon`); `--off` turns it off.
    Watch {
        #[arg(long)]
        off: bool,
    },
    /// Every repo: slug, default branch, worktree branches (with their PR,
    /// `branch(#N)`, when GitHub has one; skipped when gh fails).
    List {
        /// `<name>\t<branch>\t<path>` for every repo and worktree (no git or
        /// GitHub calls).
        #[arg(long)]
        raw: bool,
    },
    /// Fuzzy-pick a repo, worktree or branch with fzf and print its path
    /// (`valet cd` and the `wt` shell function use it).
    Pick(crate::cmd::repo::PickArgs),
    /// Print a checkout's path, making the worktree when needed (local
    /// branch, else origin/BRANCH, else a new branch off the default branch
    /// or --base). `-b NEW`: a new branch off the latest default branch.
    ///
    /// TARGET: an alias from config.toml (valet, sandbox, ...), a repo
    /// (slug or unique name suffix), or `.` for the repo you are in. A
    /// TARGET that is none of these is a branch of the repo you are in, else
    /// of config.toml's default repo. No TARGET: the worktree you are in, else
    /// the default branch of that repo. BRANCH defaults to the alias's
    /// branch, else the repo's default branch.
    ///
    /// BRANCH (or a TARGET read as a branch) may be a PR number (`123`,
    /// `#123`): its head branch, taken over when not yours. A digits-only
    /// branch name therefore reads as a PR.
    Path {
        #[arg(id = "repos_target", value_name = "TARGET")]
        target: Option<String>,
        #[arg(id = "repos_branch", value_name = "BRANCH")]
        branch: Option<String>,
        #[arg(long)]
        base: Option<String>,
        /// Make new branch NEW off the latest default branch (fetched first;
        /// the local one when it is ahead of origin). TARGET, if given, is
        /// the repo or alias.
        #[arg(short = 'b', long = "new", value_name = "NEW", conflicts_with_all = ["repos_branch", "base"])]
        new: Option<String>,
    },
    /// Remove a clean worktree and keep its branch (BRANCH or PR number, or
    /// the current worktree). Repo: --repo, else the one you are in, else the default.
    Fold {
        #[arg(id = "repos_branch", value_name = "BRANCH")]
        branch: Option<String>,
        #[arg(short, long, id = "repos_repo", value_name = "REPO")]
        repo: Option<String>,
    },
    /// Take over someone else's branch (BRANCH, or a PR number: `123` or
    /// `#123`): fetch it from now on, fetch it now, make its worktree and
    /// print the path. Repo: --repo, else the one you are in, else the default.
    Take {
        #[arg(id = "repos_branch", value_name = "BRANCH|PR")]
        branch: String,
        #[arg(short, long, id = "repos_repo", value_name = "REPO")]
        repo: Option<String>,
    },
    /// Fetch only the default branch and yours (`$USER/*`; every branch of an
    /// `owned` repo; plus listed and taken ones): rewrite the refspecs and
    /// delete remote-tracking refs outside them, and local branches outside
    /// them that no worktree uses and that hold nothing origin lacks (tip
    /// equals origin's, or merged into the default). Report only without
    /// --run. Objects go with git's auto gc.
    Narrow {
        #[arg(short, long, id = "repos_repo", value_name = "REPO")]
        repo: Option<String>,
        /// Every repo.
        #[arg(long, conflicts_with = "repos_repo")]
        all: bool,
        #[arg(long)]
        run: bool,
    },
    /// Fold clean worktrees idle for longer than --idle-days (daily job).
    Gc {
        #[arg(long, default_value_t = 3)]
        idle_days: u64,
        #[arg(short = 'n', long)]
        dry_run: bool,
    },
    /// Completion candidates: `target` (aliases, repos, branches of the
    /// current repo) or `branch` (branches of WORD's repo).
    #[command(hide = true)]
    Complete { kind: String, word: Option<String> },
    /// Recreate the folded worktree PATH points into; mark it active (hooks).
    #[command(hide = true)]
    Ensure { path: PathBuf },
}

/// The repo named by A (slug, or a unique name / owner/name suffix).
fn find_repo(a: &str) -> Result<Option<Repo>> {
    let all = repos::all()?;
    let hits: Vec<_> = all
        .iter()
        .filter(|r| r.slug == a || r.slug.ends_with(&format!("/{a}")))
        .collect();
    match hits.as_slice() {
        [one] => Ok(Some((*one).clone())),
        [] => Ok(None),
        many => anyhow::bail!(
            "{a:?} is ambiguous: {}",
            many.iter()
                .map(|r| r.slug.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// The repo holding the current directory.
fn here() -> Option<Repo> {
    repos::repo_of(&std::env::current_dir().ok()?)
}

/// The repo holding the current directory, else config.toml's default.
fn here_or_default() -> Result<Repo> {
    here().map_or_else(repos::default_repo, Ok)
}

/// The repo named by `--repo`, else the one holding the current directory,
/// else the default.
fn pick_repo(arg: Option<&str>) -> Result<Repo> {
    if let Some(a) = arg {
        return find_repo(a)?.with_context(|| format!("no repo {a:?} (`valet repos list`)"));
    }
    here_or_default()
}

/// `valet repos path`: see [`Cmd::Path`].
fn path(target: Option<&str>, branch: Option<&str>, base: Option<&str>) -> Result<PathBuf> {
    let in_repo = |b: Option<&str>| -> Result<PathBuf> {
        let r = here_or_default()?;
        let b = match b {
            Some(b) => branch_arg(&r, b)?,
            None => r.home_branch()?,
        };
        r.add(&b, base)
    };
    match target {
        None => {
            let cwd = std::env::current_dir()?;
            match repos::locate(&cwd) {
                Some((r, b, _)) => r.add(&b, base),
                None => in_repo(None),
            }
        }
        Some(".") => in_repo(branch),
        Some(t) if repos::alias_spec(t).is_some() => {
            let r = Repo::at(&repos::alias_slug(t).unwrap_or_default())?;
            let b = branch.map(|b| branch_arg(&r, b)).transpose()?;
            match base {
                None => repos::alias(t, b.as_deref()),
                Some(_) => r.add(&b.context("--base needs a BRANCH")?, base),
            }
        }
        Some(t) => match (find_repo(t)?, branch) {
            (Some(r), b) => {
                let b = match b {
                    Some(b) => branch_arg(&r, b)?,
                    None => r.home_branch()?,
                };
                r.add(&b, base)
            }
            (None, None) => in_repo(Some(t)),
            (None, _) => anyhow::bail!("no alias or repo {t:?} (`valet repos list`)"),
        },
    }
}

/// `valet repos path [TARGET] -b NEW`: TARGET is an alias or repo, else the
/// repo you are in, else the default.
fn new_branch(target: Option<&str>, new: &str) -> Result<PathBuf> {
    let r = match target {
        None | Some(".") => here_or_default()?,
        Some(t) if repos::alias_spec(t).is_some() => {
            Repo::at(&repos::alias_slug(t).unwrap_or_default())?
        }
        Some(t) => {
            find_repo(t)?.with_context(|| format!("no alias or repo {t:?} (`valet repos list`)"))?
        }
    };
    r.branch_off_latest(new)
}

/// `123` or `#123`: a PR number. A digits-only branch name reads as a PR.
fn pr_arg(s: &str) -> Option<u32> {
    let d = s.strip_prefix('#').unwrap_or(s);
    (!d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
        .then(|| d.parse().ok())
        .flatten()
}

/// `owner/name` of a GitHub repo.
fn gh_repo(r: &Repo) -> Option<&str> {
    r.slug.strip_prefix("github.com/")
}

/// A BRANCH argument: the branch, or a PR number's head branch.
fn branch_arg(r: &Repo, s: &str) -> Result<String> {
    let Some(n) = pr_arg(s) else {
        return Ok(s.to_owned());
    };
    let repo =
        gh_repo(r).with_context(|| format!("{}: not a GitHub repo, so no PR #{n}", r.slug))?;
    let gh = crate::tools::resolve("gh")?;
    let n = n.to_string();
    let out = crate::proc::output(
        &gh,
        &[
            "pr",
            "view",
            &n,
            "--repo",
            repo,
            "--json",
            "headRefName,isCrossRepository",
        ],
        crate::proc::NET_TIMEOUT,
    )?;
    pr_head(&out).with_context(|| format!("{repo}#{n}"))
}

/// The head branch in `gh pr view --json headRefName,isCrossRepository`;
/// a fork's PR has no branch in origin.
fn pr_head(json: &str) -> Result<String> {
    let v: serde_json::Value = serde_json::from_str(json).context("parse gh pr view output")?;
    if v["isCrossRepository"].as_bool() == Some(true) {
        anyhow::bail!("the PR comes from a fork; its branch is not in origin");
    }
    let b = v["headRefName"].as_str().context("no headRefName")?;
    if !repos::branch_ok(b) {
        anyhow::bail!("PR head {b:?} is not a usable branch name");
    }
    Ok(b.to_owned())
}

/// The PR of each of BRANCHES in REPO (open, else newest), looked up in
/// parallel with a short timeout; branches without one, or on failure, absent.
fn prs_of(repo: &str, branches: &[String]) -> std::collections::HashMap<String, u32> {
    let Ok(gh) = crate::tools::resolve("gh") else {
        return Default::default();
    };
    std::thread::scope(|s| {
        let hs: Vec<_> = branches
            .iter()
            .map(|b| {
                let gh = &gh;
                s.spawn(move || {
                    let out = crate::proc::output(
                        gh,
                        &[
                            "pr",
                            "list",
                            "--repo",
                            repo,
                            "--head",
                            b,
                            "--state",
                            "all",
                            "--limit",
                            "20",
                            "--json",
                            "number,state",
                        ],
                        Duration::from_secs(10),
                    )
                    .ok()?;
                    Some((b.clone(), pick_pr(&out)?))
                })
            })
            .collect();
        hs.into_iter()
            .filter_map(|h| h.join().ok().flatten())
            .collect()
    })
}

/// From `gh pr list --json number,state`: the open PR, else the newest.
fn pick_pr(json: &str) -> Option<u32> {
    let v: Vec<serde_json::Value> = serde_json::from_str(json).ok()?;
    let num = |p: &serde_json::Value| p["number"].as_u64().map(|n| n as u32);
    v.iter()
        .filter(|p| p["state"] == "OPEN")
        .filter_map(num)
        .max()
        .or_else(|| v.iter().filter_map(num).max())
}

/// See [`Cmd::Complete`]: `word:description` lines for zsh's `_describe`
/// (colons in words escaped). Branches say whether they have a worktree.
/// Errors give no candidates.
fn complete(kind: &str, word: Option<&str>) -> Vec<String> {
    let esc = |w: &str| w.replace(':', "\\:");
    let repo_for = |w: &str| -> Option<Repo> {
        match repos::alias_slug(w) {
            Some(slug) => Repo::at(&slug).ok(),
            None => find_repo(w).ok().flatten(),
        }
    };
    let branches = |r: Option<Repo>| -> Vec<String> {
        let Some(r) = r.or_else(|| here_or_default().ok()) else {
            return Vec::new();
        };
        let wts: std::collections::BTreeSet<String> = r
            .worktrees()
            .unwrap_or_default()
            .into_iter()
            .map(|(_, b)| b)
            .collect();
        r.branches()
            .unwrap_or_default()
            .into_iter()
            .map(|b| {
                let what = if wts.contains(&b) {
                    "worktree"
                } else {
                    "branch"
                };
                format!("{}:{what}", esc(&b))
            })
            .collect()
    };
    match kind {
        "target" => {
            let mut v: Vec<String> = repos::manifest()
                .map(|m| {
                    m.aliases
                        .into_iter()
                        .map(|(k, spec)| format!("{}:alias {}", esc(&k), esc(&spec)))
                        .collect()
                })
                .unwrap_or_default();
            for r in repos::all().unwrap_or_default() {
                // owner/name: short, and unique in practice.
                let short = r
                    .slug
                    .split_once('/')
                    .map_or(r.slug.clone(), |(_, t)| t.to_owned());
                v.push(format!("{}:repo", esc(&short)));
            }
            v.extend(branches(None));
            v
        }
        "branch" => branches(word.and_then(repo_for)),
        _ => Vec::new(),
    }
}

pub fn run(a: Args) -> Result<()> {
    match a.cmd {
        Cmd::Sync { dry_run } => repos::sync(dry_run),
        Cmd::Clone { url } => {
            println!("{}", repos::clone_add(&url)?.display());
            Ok(())
        }
        Cmd::Reconcile { dry_run } => {
            for line in repos::reconcile(dry_run)? {
                println!("{line}");
            }
            Ok(())
        }
        Cmd::Watch { off: true } => {
            crate::cmd::daemon::set_part("repos", false)?;
            println!("repo watch off");
            Ok(())
        }
        Cmd::Watch { off: false } => {
            crate::cmd::daemon::ensure("repos")?;
            println!("watching {} folders", repos::watch_dirs()?.len());
            Ok(())
        }
        Cmd::List { raw: true } => crate::cmd::repo::print_raw(),
        Cmd::Pick(a) => crate::cmd::repo::pick(a),
        Cmd::List { raw: false } => {
            for r in repos::all()? {
                let def = r.default_branch().unwrap_or_default();
                let wts: Vec<String> = r.worktrees()?.into_iter().map(|(_, b)| b).collect();
                let others: Vec<String> = wts.iter().filter(|b| **b != def).cloned().collect();
                let prs = match gh_repo(&r) {
                    Some(g) if !others.is_empty() => prs_of(g, &others),
                    _ => Default::default(),
                };
                let shown: Vec<String> = wts
                    .iter()
                    .map(|b| match prs.get(b) {
                        Some(n) => format!("{b}(#{n})"),
                        None => b.clone(),
                    })
                    .collect();
                println!("{}\t{def}\t{}", r.slug, shown.join(" "));
            }
            Ok(())
        }
        Cmd::Take { branch, repo } => {
            let r = pick_repo(repo.as_deref())?;
            let b = branch_arg(&r, &branch)?;
            println!("{}", r.take(&b)?.display());
            Ok(())
        }
        Cmd::Narrow { repo, all, run } => {
            let list = if all {
                repos::all()?
            } else {
                vec![pick_repo(repo.as_deref())?]
            };
            let w = if run { "" } else { "would: " };
            for r in list {
                match repos::narrow(&r, !run) {
                    Ok(n) if n.remote.is_empty() && n.local.is_empty() => {}
                    Ok(n) => println!(
                        "{w}{}: drop {} remote-tracking refs, {} local branches",
                        r.slug,
                        n.remote.len(),
                        n.local.len()
                    ),
                    Err(e) => crate::warn!("{}: {e:#}", r.slug),
                }
            }
            if !run {
                println!("report only; run again with --run");
            }
            Ok(())
        }
        Cmd::Path {
            target,
            branch,
            base,
            new,
        } => {
            let p = match new {
                Some(n) => new_branch(target.as_deref(), &n)?,
                None => path(target.as_deref(), branch.as_deref(), base.as_deref())?,
            };
            println!("{}", p.display());
            Ok(())
        }
        Cmd::Fold { branch, repo } => {
            let (r, b) = match branch {
                Some(b) => {
                    let r = pick_repo(repo.as_deref())?;
                    let b = branch_arg(&r, &b)?;
                    (r, b)
                }
                None => {
                    let cwd = std::env::current_dir()?;
                    let (r, b, _) = repos::locate(&cwd).context("not inside a worktree")?;
                    (r, b)
                }
            };
            r.fold(&b)?;
            println!("folded {} {b}", r.slug);
            Ok(())
        }
        Cmd::Gc { idle_days, dry_run } => {
            for line in repos::gc(Duration::from_secs(idle_days * 86_400), dry_run)? {
                println!("{line}");
            }
            Ok(())
        }
        Cmd::Complete { kind, word } => {
            for c in complete(&kind, word.as_deref()) {
                println!("{c}");
            }
            Ok(())
        }
        Cmd::Ensure { path } => {
            if repos::ensure(Path::new(&path))? {
                eprintln!("valet: recreated the worktree for {}", path.display());
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr_numbers_and_heads() {
        assert_eq!(pr_arg("123"), Some(123));
        assert_eq!(pr_arg("#123"), Some(123));
        for s in ["", "#", "12a", "me/123", "-1", "99999999999"] {
            assert_eq!(pr_arg(s), None, "{s:?}");
        }
        assert_eq!(
            pr_head(r#"{"headRefName":"alice/fix","isCrossRepository":false}"#).unwrap(),
            "alice/fix"
        );
        assert!(pr_head(r#"{"headRefName":"main","isCrossRepository":true}"#).is_err());
        assert!(pr_head(r#"{"headRefName":"../x","isCrossRepository":false}"#).is_err());
        assert_eq!(
            pick_pr(r#"[{"number":3,"state":"MERGED"},{"number":2,"state":"OPEN"}]"#),
            Some(2)
        );
        assert_eq!(
            pick_pr(r#"[{"number":3,"state":"CLOSED"},{"number":5,"state":"MERGED"}]"#),
            Some(5)
        );
        assert_eq!(pick_pr("[]"), None);
    }
}
