//! The `upstream` job: keep clones of repos you do not own (outside
//! config.toml `owned`) current with origin. Per repo: fetch the configured
//! refspecs, fast-forward the default branch's worktree to origin, and rebase
//! every other worktree onto its upstream when it tracks one (so a pushed
//! branch never needs a force-push), else onto `origin/<default>`. Local
//! changes, untracked files included, are stashed around the update and
//! re-applied. On a conflict the worktree is put back as it was and a WARN
//! line says what to do. Nothing is pushed.

use super::{git, git_ok};
use crate::repos::Repo;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

const FETCH_SECS: u64 = 600;
const STEP_SECS: u64 = 300;
const QUICK_SECS: u64 = 30;

#[derive(Debug, Default)]
struct Report {
    done: Vec<String>,
    warn: Vec<String>,
    current: usize,
}

/// What updating one worktree did.
enum Step {
    Current,
    Done(String),
    Warn(String),
}

/// A worktree used more recently than this (git activity or the
/// `claude.worktree` hook's mark) is left for the next run.
const IN_USE_SECS: u64 = 3600;

pub fn run(dry: bool) -> Result<Vec<String>> {
    let owned = crate::repos::manifest()?.owned;
    let mut rep = Report::default();
    for r in not_owned(crate::repos::all()?, &owned) {
        if let Err(e) = sync_repo(&r, dry, IN_USE_SECS, &mut rep) {
            rep.warn.push(format!("{}: {e:#}; skipped", r.slug));
        }
    }
    for w in &rep.warn {
        super::log("WARN", &format!("upstream: {w}"));
    }
    let mut out = rep.done;
    out.push(format!("{} worktrees already current", rep.current));
    if !rep.warn.is_empty() {
        out.push(format!(
            "{} left as they were (WARN lines in `valet job log`)",
            rep.warn.len()
        ));
    }
    Ok(out)
}

/// REPOS outside the OWNED prefixes.
fn not_owned(repos: Vec<Repo>, owned: &[String]) -> Vec<Repo> {
    repos
        .into_iter()
        .filter(|r| !crate::repos::is_owned(&r.slug, owned))
        .collect()
}

/// (path, branch) of each worktree in `git worktree list --porcelain` OUT;
/// branch `None` when detached. The bare entry and prunable worktrees are
/// left out.
fn worktrees(out: &str) -> Vec<(PathBuf, Option<String>)> {
    out.split("\n\n")
        .filter_map(|blk| {
            let (mut path, mut branch) = (None, None);
            for l in blk.lines() {
                if let Some(p) = l.strip_prefix("worktree ") {
                    path = Some(PathBuf::from(p));
                } else if let Some(b) = l.strip_prefix("branch refs/heads/") {
                    branch = Some(b.to_owned());
                } else if l == "bare" || l.starts_with("prunable") {
                    return None;
                }
            }
            path.map(|p| (p, branch))
        })
        .collect()
}

fn sync_repo(r: &Repo, dry: bool, in_use: u64, rep: &mut Report) -> Result<()> {
    let gd = r.git_dir();
    git_ok(&gd, &["fetch", "--quiet", "origin"], FETCH_SECS).context("fetch")?;
    let def = r.default_branch()?;
    let origin_def = format!("refs/remotes/origin/{def}");
    let mut wts = worktrees(&git_ok(
        &gd,
        &["worktree", "list", "--porcelain"],
        QUICK_SECS,
    )?);
    // The default branch first (stable sort keeps the rest in git's order).
    wts.sort_by_key(|(_, b)| b.as_deref() != Some(def.as_str()));
    for (wt, branch) in wts {
        let name = format!("{} ({})", r.slug, branch.as_deref().unwrap_or("detached"));
        if let Some(op) = super::repos::in_progress(&wt)? {
            rep.warn.push(format!(
                "{name}: {op} in progress in {}; skipped (finish or abort it)",
                wt.display()
            ));
            continue;
        }
        let Some(branch) = branch else {
            rep.warn.push(format!(
                "{name}: {} has a detached HEAD; skipped (check out a branch)",
                wt.display()
            ));
            continue;
        };
        // Only the repos layout's own checkouts (`tree/<branch>`), and none a
        // session used within the hour: never rebase files under an editor.
        if crate::cmd::apps::canonical(&wt) != crate::cmd::apps::canonical(&r.worktree(&branch)) {
            continue;
        }
        if r.idle_secs(&wt).is_some_and(|s| s < in_use) {
            rep.done
                .push(format!("{name}: used within the hour; skipped"));
            continue;
        }
        let (onto, ff) = if branch == def {
            (origin_def.clone(), true)
        } else {
            match onto_of(&wt, &branch, &origin_def)? {
                Some(u) => (u, false),
                None => {
                    rep.warn.push(format!(
                        "{name}: its upstream is gone; skipped (set one with `git branch -u`, or unset it)"
                    ));
                    continue;
                }
            }
        };
        match update(&wt, &branch, &onto, ff, dry) {
            Ok(Step::Current) => rep.current += 1,
            Ok(Step::Done(s)) => rep.done.push(format!("{name}: {s}")),
            Ok(Step::Warn(s)) => rep.warn.push(format!("{name}: {s}")),
            Err(e) => rep.warn.push(format!(
                "{name}: {e:#}; check `git status` and `git stash list` in {}",
                wt.display()
            )),
        }
    }
    Ok(())
}

/// What BRANCH rebases onto: its upstream's ref when it tracks one, else
/// ORIGIN_DEF. `None` when it tracks an upstream that no longer resolves.
fn onto_of(wt: &Path, branch: &str, origin_def: &str) -> Result<Option<String>> {
    let tracks = git(
        wt,
        &["config", "--get", &format!("branch.{branch}.merge")],
        QUICK_SECS,
    )?;
    if !tracks.status.success() {
        return Ok(Some(origin_def.to_owned()));
    }
    let u = git(
        wt,
        &[
            "rev-parse",
            "--symbolic-full-name",
            &format!("{branch}@{{upstream}}"),
        ],
        QUICK_SECS,
    )?;
    Ok((u.status.success() && !u.stdout.trim().is_empty()).then(|| u.stdout.trim().to_owned()))
}

fn rev(wt: &Path, r: &str) -> Result<String> {
    git_ok(
        wt,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{r}^{{commit}}"),
        ],
        QUICK_SECS,
    )
    .with_context(|| format!("{r} does not resolve"))
}

fn stash_tip(wt: &Path) -> Option<String> {
    git_ok(
        wt,
        &["rev-parse", "--verify", "--quiet", "refs/stash"],
        QUICK_SECS,
    )
    .ok()
}

fn is_ancestor(wt: &Path, a: &str, b: &str) -> Result<bool> {
    Ok(git(wt, &["merge-base", "--is-ancestor", a, b], QUICK_SECS)?
        .status
        .success())
}

/// Bring WT (on BRANCH) up to ONTO: fast-forward when FF, else rebase.
fn update(wt: &Path, branch: &str, onto: &str, ff: bool, dry: bool) -> Result<Step> {
    let short = onto
        .strip_prefix("refs/remotes/")
        .or_else(|| onto.strip_prefix("refs/heads/"))
        .unwrap_or(onto);
    let to = rev(wt, onto)?;
    if is_ancestor(wt, &to, "HEAD")? {
        return Ok(Step::Current);
    }
    let old = rev(wt, "HEAD")?;
    if ff && !is_ancestor(wt, &old, &to)? {
        return Ok(Step::Warn(format!(
            "has commits not on {short}; not fast-forwarded (rebase or reset it by hand)"
        )));
    }
    let n = git_ok(
        wt,
        &["rev-list", "--count", &format!("{old}..{to}")],
        QUICK_SECS,
    )?;
    let what = if ff {
        format!("fast-forward {n} commits")
    } else {
        format!("rebase onto {short} ({n} new commits)")
    };
    let dirty = !git_ok(
        wt,
        &["status", "--porcelain", "--untracked-files=all"],
        STEP_SECS,
    )?
    .is_empty();
    if dry {
        let st = if dirty {
            ", stashing local changes"
        } else {
            ""
        };
        return Ok(Step::Done(format!("would {what}{st}")));
    }
    let stash = if dirty {
        let before = stash_tip(wt);
        let msg = format!(
            "valet upstream: {branch} before {what}, {}",
            crate::log::now_iso()
        );
        git_ok(
            wt,
            &[
                "stash",
                "push",
                "--include-untracked",
                "--quiet",
                "-m",
                &msg,
            ],
            STEP_SECS,
        )?;
        // Unchanged: nothing git can stash (submodule changes only).
        stash_tip(wt).filter(|t| Some(t) != before.as_ref())
    } else {
        None
    };
    let up = if ff {
        git(wt, &["merge", "--ff-only", "--quiet", &to], STEP_SECS)?
    } else {
        git(
            wt,
            &[
                "rebase",
                "--quiet",
                "--no-autostash",
                "--no-update-refs",
                &to,
            ],
            STEP_SECS,
        )?
    };
    if !up.status.success() {
        if !ff {
            let _ = git(wt, &["rebase", "--abort"], STEP_SECS);
        }
        let kept = restore(wt, stash.as_deref())?;
        let how = if ff { "merge --ff-only" } else { "rebase" };
        return Ok(Step::Warn(format!(
            "{what} failed ({}); left as it was{kept}. By hand: cd {} && git {how} {short}",
            last_line(&up.stderr),
            wt.display()
        )));
    }
    if let Some(s) = &stash
        && !apply(wt, s)?
    {
        // The changes conflict with the new base: back to OLD, where they
        // apply as they were stashed.
        git_ok(wt, &["reset", "--hard", "--quiet", &old], STEP_SECS)?;
        remove_stashed_untracked(wt, s)?;
        let kept = restore(wt, Some(s))?;
        return Ok(Step::Warn(format!(
            "local changes conflict with {short}; not updated, left as it was{kept}. \
             Commit them, then: cd {} && git rebase {short}",
            wt.display()
        )));
    }
    if let Some(s) = &stash {
        drop_stash(wt, s)?;
    }
    Ok(Step::Done(if ff {
        format!("fast-forwarded {n} commits")
    } else {
        format!("rebased onto {short} ({n} new commits)")
    }))
}

fn last_line(s: &str) -> &str {
    s.lines()
        .rfind(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
}

/// `git stash apply --index STASH`; true when it applied cleanly.
fn apply(wt: &Path, stash: &str) -> Result<bool> {
    Ok(git(
        wt,
        &["stash", "apply", "--index", "--quiet", stash],
        STEP_SECS,
    )?
    .status
    .success())
}

/// Re-apply STASH (if any) on the original HEAD and drop it. Returns "" when
/// done, else where the changes are kept.
fn restore(wt: &Path, stash: Option<&str>) -> Result<String> {
    let Some(s) = stash else {
        return Ok(String::new());
    };
    if apply(wt, s)? {
        drop_stash(wt, s)?;
        return Ok(String::new());
    }
    Ok(format!(
        "; local changes kept in stash {s} (`git stash list`, `git stash apply {s}`)"
    ))
}

/// Drop the stash entry whose commit is STASH.
fn drop_stash(wt: &Path, stash: &str) -> Result<()> {
    let list = git_ok(wt, &["stash", "list", "--format=%H"], QUICK_SECS)?;
    if let Some(i) = list.lines().position(|l| l == stash) {
        git_ok(
            wt,
            &["stash", "drop", "--quiet", &format!("stash@{{{i}}}")],
            QUICK_SECS,
        )?;
    }
    Ok(())
}

/// Delete the untracked files a failed apply of STASH wrote back (its third
/// parent), so it applies again. They were stashed away, so any file at such
/// a path came from the apply; the stash keeps the content.
fn remove_stashed_untracked(wt: &Path, stash: &str) -> Result<()> {
    let u = format!("{stash}^3");
    if !git(wt, &["rev-parse", "--verify", "--quiet", &u], QUICK_SECS)?
        .status
        .success()
    {
        return Ok(());
    }
    let files = git_ok(wt, &["ls-tree", "-r", "-z", "--name-only", &u], QUICK_SECS)?;
    for f in files.split('\0').filter(|f| !f.is_empty()) {
        let p = wt.join(f);
        if std::fs::symlink_metadata(&p).is_ok() {
            std::fs::remove_file(&p).with_context(|| format!("remove {}", p.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn g(dir: &Path, args: &[&str]) -> String {
        let o = git(dir, args, 60).unwrap();
        assert!(o.status.success(), "git {args:?}: {}", o.stderr);
        o.stdout.trim().to_owned()
    }

    fn commit(dir: &Path, file: &str, text: &str) -> String {
        fs::write(dir.join(file), text).unwrap();
        g(dir, &["add", file]);
        g(dir, &["commit", "-qm", file]);
        g(dir, &["rev-parse", "HEAD"])
    }

    /// Hermetic identity, no hooks or signing (repo config beats global).
    fn local_config(dir: &Path, hooks: &Path) {
        for (k, v) in [
            ("user.name", "t"),
            ("user.email", "t@example.invalid"),
            ("commit.gpgsign", "false"),
            ("core.hooksPath", hooks.to_str().unwrap()),
        ] {
            g(dir, &["config", k, v]);
        }
    }

    struct Fx {
        root: PathBuf,
        up: PathBuf,
        repo: Repo,
    }

    impl Fx {
        fn wt(&self, b: &str) -> PathBuf {
            self.repo.worktree(b)
        }
    }

    impl Drop for Fx {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// An upstream repo on `main` (a.txt, b.txt) and a clone laid out like
    /// `valet repos`: bare `.git`, `tree/main`, and `tree/feat` (no upstream).
    fn fixture(tag: &str) -> Fx {
        let root =
            std::env::temp_dir().join(format!("valet-test-{}-upstream-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let hooks = root.join("hooks");
        let up = root.join("up");
        fs::create_dir_all(&hooks).unwrap();
        fs::create_dir_all(&up).unwrap();
        g(&up, &["init", "-q", "-b", "main"]);
        local_config(&up, &hooks);
        commit(&up, "a.txt", "a\n");
        commit(&up, "b.txt", "b\n");
        let repo = Repo {
            dir: root.join("Repos/github.com/other/proj"),
            slug: "github.com/other/proj".into(),
        };
        let gd = repo.git_dir();
        fs::create_dir_all(&gd).unwrap();
        g(&gd, &["init", "-q", "--bare", "-b", "main"]);
        local_config(&gd, &hooks);
        g(&gd, &["remote", "add", "origin", up.to_str().unwrap()]);
        g(&gd, &["fetch", "-q", "origin"]);
        g(
            &gd,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );
        g(&gd, &["branch", "-q", "--track", "main", "origin/main"]);
        let fx = Fx { root, up, repo };
        g(
            &gd,
            &[
                "worktree",
                "add",
                "-q",
                fx.wt("main").to_str().unwrap(),
                "main",
            ],
        );
        g(
            &gd,
            &[
                "worktree",
                "add",
                "-q",
                "--no-track",
                "-b",
                "feat",
                fx.wt("feat").to_str().unwrap(),
                "origin/main",
            ],
        );
        fx
    }

    fn read(p: PathBuf) -> String {
        fs::read_to_string(p).unwrap()
    }

    fn stashes(fx: &Fx) -> String {
        g(&fx.wt("main"), &["stash", "list"])
    }

    #[test]
    fn fast_forwards_default_and_rebases_dirty_branch_keeping_changes() {
        let fx = fixture("ff");
        let feat = fx.wt("feat");
        commit(&feat, "c.txt", "c\n");
        fs::write(feat.join("b.txt"), "b dirty\n").unwrap();
        fs::write(feat.join("u.txt"), "untracked\n").unwrap();
        let tip = commit(&fx.up, "a.txt", "a2\n");
        // A detached worktree is skipped with a warning.
        g(
            &fx.repo.git_dir(),
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                fx.wt("det").to_str().unwrap(),
                "origin/main",
            ],
        );

        let mut dry = Report::default();
        sync_repo(&fx.repo, true, 0, &mut dry).unwrap();
        assert_eq!(
            dry.done,
            [
                "github.com/other/proj (main): would fast-forward 1 commits",
                "github.com/other/proj (feat): would rebase onto origin/main (1 new commits), stashing local changes",
            ]
        );
        assert_ne!(g(&fx.wt("main"), &["rev-parse", "HEAD"]), tip);

        // Just used (the fixture made them now): left for the next run.
        let mut busy = Report::default();
        sync_repo(&fx.repo, false, IN_USE_SECS, &mut busy).unwrap();
        assert_eq!(busy.done.len(), 2, "{busy:?}");
        assert!(
            busy.done
                .iter()
                .all(|d| d.ends_with("used within the hour; skipped"))
        );
        assert_ne!(g(&fx.wt("main"), &["rev-parse", "HEAD"]), tip);

        let mut rep = Report::default();
        sync_repo(&fx.repo, false, 0, &mut rep).unwrap();
        assert_eq!(rep.done.len(), 2, "{rep:?}");
        assert_eq!(rep.warn.len(), 1, "{rep:?}");
        assert!(rep.warn[0].contains("(detached)") && rep.warn[0].contains("detached HEAD"));
        assert_eq!(g(&fx.wt("main"), &["rev-parse", "HEAD"]), tip);
        assert_eq!(g(&feat, &["rev-parse", "HEAD~1"]), tip);
        assert_eq!(read(feat.join("a.txt")), "a2\n");
        assert_eq!(read(feat.join("b.txt")), "b dirty\n");
        assert_eq!(read(feat.join("u.txt")), "untracked\n");
        assert_eq!(stashes(&fx), "");

        let mut again = Report::default();
        sync_repo(&fx.repo, false, 0, &mut again).unwrap();
        assert!(again.done.is_empty() && again.current == 2, "{again:?}");
    }

    #[test]
    fn rebase_conflict_leaves_branch_and_changes_as_they_were() {
        let fx = fixture("rebase");
        let feat = fx.wt("feat");
        let mine = commit(&feat, "a.txt", "mine\n");
        fs::write(feat.join("u.txt"), "untracked\n").unwrap();
        commit(&fx.up, "a.txt", "theirs\n");

        let mut rep = Report::default();
        sync_repo(&fx.repo, false, 0, &mut rep).unwrap();
        assert_eq!(rep.warn.len(), 1, "{rep:?}");
        let w = &rep.warn[0];
        assert!(
            w.starts_with("github.com/other/proj (feat): rebase onto origin/main"),
            "{w}"
        );
        assert!(
            w.contains("left as it was") && w.contains("git rebase origin/main"),
            "{w}"
        );
        assert_eq!(g(&feat, &["rev-parse", "HEAD"]), mine);
        assert_eq!(super::super::repos::in_progress(&feat).unwrap(), None);
        assert_eq!(read(feat.join("a.txt")), "mine\n");
        assert_eq!(read(feat.join("u.txt")), "untracked\n");
        assert_eq!(stashes(&fx), "");
        // The default branch still moved.
        assert_eq!(read(fx.wt("main").join("a.txt")), "theirs\n");
    }

    #[test]
    fn stash_conflict_puts_the_branch_back_with_its_changes() {
        let fx = fixture("pop");
        let feat = fx.wt("feat");
        let mine = commit(&feat, "c.txt", "c\n");
        fs::write(feat.join("a.txt"), "dirty\n").unwrap();
        fs::write(feat.join("u.txt"), "untracked\n").unwrap();
        commit(&fx.up, "a.txt", "theirs\n");

        let mut rep = Report::default();
        sync_repo(&fx.repo, false, 0, &mut rep).unwrap();
        assert_eq!(rep.warn.len(), 1, "{rep:?}");
        let w = &rep.warn[0];
        assert!(
            w.contains("(feat): local changes conflict with origin/main"),
            "{w}"
        );
        assert!(!w.contains("kept in stash"), "{w}");
        assert_eq!(g(&feat, &["rev-parse", "HEAD"]), mine);
        assert_eq!(read(feat.join("a.txt")), "dirty\n");
        assert_eq!(read(feat.join("u.txt")), "untracked\n");
        assert_eq!(g(&feat, &["status", "--porcelain"]), "M a.txt\n?? u.txt");
        assert_eq!(stashes(&fx), "");
    }

    #[test]
    fn tracked_branch_rebases_onto_its_own_upstream() {
        let fx = fixture("track");
        g(&fx.up, &["branch", "-q", "pr"]);
        let pr_tip = commit(&fx.up, "a.txt", "on main only\n");
        g(&fx.up, &["checkout", "-q", "pr"]);
        let pr_new = commit(&fx.up, "p.txt", "pr\n");
        g(&fx.up, &["checkout", "-q", "main"]);
        let gd = fx.repo.git_dir();
        g(&gd, &["fetch", "-q", "origin"]);
        g(&gd, &["branch", "-q", "pr", &format!("{pr_new}~1")]);
        g(&gd, &["branch", "-q", "-u", "origin/pr", "pr"]);
        g(
            &gd,
            &["worktree", "add", "-q", fx.wt("pr").to_str().unwrap(), "pr"],
        );

        let mut rep = Report::default();
        sync_repo(&fx.repo, false, 0, &mut rep).unwrap();
        assert!(rep.warn.is_empty(), "{rep:?}");
        assert!(
            rep.done
                .iter()
                .any(|d| d.contains("(pr): rebased onto origin/pr"))
        );
        assert_eq!(g(&fx.wt("pr"), &["rev-parse", "HEAD"]), pr_new);
        assert!(!is_ancestor(&fx.wt("pr"), &pr_tip, "HEAD").unwrap());
    }

    #[test]
    fn owned_repos_are_skipped() {
        let r = |s: &str| Repo {
            dir: PathBuf::from(s),
            slug: s.into(),
        };
        let owned = vec!["github.com/me".to_owned()];
        let kept = not_owned(
            vec![
                r("github.com/me/a"),
                r("github.com/other/b"),
                r("github.com/meow/c"),
            ],
            &owned,
        );
        let slugs: Vec<&str> = kept.iter().map(|r| r.slug.as_str()).collect();
        assert_eq!(slugs, ["github.com/other/b", "github.com/meow/c"]);
    }

    #[test]
    fn worktree_list_parses_branches_and_detached() {
        let out = "worktree /r/.git\nbare\n\nworktree /r/tree/main\nHEAD 1\nbranch refs/heads/main\n\n\
                   worktree /r/tree/x\nHEAD 2\ndetached\n\nworktree /r/tree/gone\nHEAD 3\nbranch refs/heads/gone\nprunable gitdir file points to non-existent location";
        assert_eq!(
            worktrees(out),
            [
                (PathBuf::from("/r/tree/main"), Some("main".to_owned())),
                (PathBuf::from("/r/tree/x"), None),
            ]
        );
    }
}
