//! Repository layout (replaces ghq/gwq). Every git checkout lives under
//! `<Userland>/Repos`:
//!
//! ```text
//! Repos/<host>/<owner>/<name>/
//!   .git/                     bare repository (all branches, objects, config)
//!   tree/<branch>/            one checkout per branch; `a/b` nests as a/b (like GitHub's /tree/<branch> URLs)
//!   .parked/<branch>/         notes kept while a branch is folded (see PARK)
//! ```
//!
//! A branch is either a worktree or folded: only a branch in `.git`. `add`
//! makes the worktree (from the local branch, else `origin/<branch>`, else a
//! new branch off the default), `fold` removes a clean worktree and keeps the
//! branch. Idle worktrees are folded by the daily job; the shell and Claude
//! hooks mark activity and recreate folded worktrees on use.

use crate::proc::Cmd;
use anyhow::{Context, Result, bail};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const GIT_TIMEOUT: Duration = Duration::from_secs(120);
const NET_TIMEOUT: Duration = Duration::from_secs(1800);
/// Gitignored notes that survive a fold (moved to `.parked/<branch>/`).
const PARK: &[&str] = &[".worktree-review", ".docs"];
/// Touched in the worktree's git dir when the shell or Claude uses it.
const ACTIVE: &str = "valet-active";
/// Worktrees live in `<repo>/tree/<branch>`.
pub const TREE: &str = "tree";

/// `<Userland>/Repos`.
pub fn root() -> Result<PathBuf> {
    Ok(crate::cmd::apps::root()?.join("Repos"))
}

/// One repository: `<root>/<host>/<owner>/<name>`.
#[derive(Clone, Debug, PartialEq)]
pub struct Repo {
    pub dir: PathBuf,
    /// `host/owner/name`.
    pub slug: String,
}

fn git(dir: &Path) -> Cmd {
    Cmd::new("git").arg("-C").arg(dir)
}

impl Repo {
    pub fn at(slug: &str) -> Result<Repo> {
        let parts: Vec<&str> = slug.split('/').collect();
        if parts.len() != 3 || parts.iter().any(|p| !safe_part(p)) {
            bail!("repo {slug:?}: want host/owner/name");
        }
        Ok(Repo {
            dir: root()?.join(slug),
            slug: slug.to_owned(),
        })
    }

    pub fn git_dir(&self) -> PathBuf {
        self.dir.join(".git")
    }

    pub fn worktree(&self, branch: &str) -> PathBuf {
        self.dir.join(TREE).join(branch)
    }

    fn git(&self) -> Cmd {
        Cmd::new("git").arg("--git-dir").arg(self.git_dir())
    }

    pub fn exists(&self) -> bool {
        self.git_dir().join("HEAD").is_file()
    }

    /// `origin/HEAD`'s branch, else the bare repo's HEAD.
    pub fn default_branch(&self) -> Result<String> {
        let r = self
            .git()
            .args(["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
            .timeout(GIT_TIMEOUT)
            .output()?;
        let s = r.stdout.trim().to_owned();
        if r.status.success()
            && let Some(b) = s.strip_prefix("origin/")
        {
            return Ok(b.to_owned());
        }
        Ok(self
            .git()
            .args(["symbolic-ref", "--short", "HEAD"])
            .timeout(GIT_TIMEOUT)
            .checked()?
            .trim()
            .to_owned())
    }

    fn has_ref(&self, r: &str) -> bool {
        self.git()
            .args(["rev-parse", "--verify", "--quiet", r])
            .timeout(GIT_TIMEOUT)
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// Local branch names.
    pub fn branches(&self) -> Result<Vec<String>> {
        let out = self
            .git()
            .args(["for-each-ref", "--format=%(refname:short)", "refs/heads"])
            .timeout(GIT_TIMEOUT)
            .checked()?;
        Ok(out.lines().map(str::to_owned).collect())
    }

    /// Worktrees (path, branch) from `git worktree list` (bare entry dropped).
    pub fn worktrees(&self) -> Result<Vec<(PathBuf, String)>> {
        let out = self
            .git()
            .args(["worktree", "list", "--porcelain"])
            .timeout(GIT_TIMEOUT)
            .checked()?;
        let mut v = Vec::new();
        let mut path = None;
        for line in out.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(p));
            } else if let Some(b) = line.strip_prefix("branch refs/heads/")
                && let Some(p) = path.take()
            {
                v.push((p, b.to_owned()));
            }
        }
        Ok(v)
    }

    /// The worktree for `branch`, made when missing: from the local branch,
    /// else `origin/<branch>` (tracking it), else a new branch off `base`
    /// (default: `origin/<default branch>`).
    pub fn add(&self, branch: &str, base: Option<&str>) -> Result<PathBuf> {
        if !branch_ok(branch) {
            bail!("bad branch name {branch:?}");
        }
        let wt = self.worktree(branch);
        if wt.join(".git").exists() {
            return Ok(wt);
        }
        self.check_worktree_allowed(branch)?;
        fs::create_dir_all(wt.parent().context("worktree parent")?)?;
        // Relative links: the worktree works under every host's spelling of
        // the disk (a direct path here, an automount path elsewhere).
        let mut c = self
            .git()
            .args(["worktree", "add", "--quiet", "--relative-paths"]);
        if self.has_ref(&format!("refs/heads/{branch}")) {
            c = c.arg(&wt).arg(branch);
        } else if self.has_ref(&format!("refs/remotes/origin/{branch}")) {
            c = c
                .args(["--track", "-b", branch])
                .arg(&wt)
                .arg(format!("origin/{branch}"));
        } else if !self.fetches(branch) && self.remote_has(branch) {
            // Someone else's branch the narrow refspecs skip: take it over
            // rather than make a new branch of that name.
            return self.take(branch);
        } else {
            let user = std::env::var("USER").unwrap_or_default();
            if let Err(why) = new_branch_name_ok(&user, branch) {
                bail!("new branch {branch:?}: {why}");
            }
            let base = match base {
                Some(b) => b.to_owned(),
                None => self.newest_default()?,
            };
            c = c.args(["--no-track", "-b", branch]).arg(&wt).arg(base);
        }
        // Children print to stderr: callers print the worktree path on
        // stdout for a shell to cd to.
        c.timeout(NET_TIMEOUT).stdout_to_stderr().run()?;
        if wt.join(".gitmodules").is_file() {
            git(&wt)
                .args(["submodule", "update", "--init", "--recursive", "--quiet"])
                .timeout(NET_TIMEOUT)
                .stdout_to_stderr()
                .run()?;
            absolutize_submodules(&wt)?;
        }
        self.unpark(branch, &wt)?;
        if let Err(e) = self.copy_includes(&wt) {
            crate::warn!("{}: copying [repos] include files: {e:#}", self.slug);
        }
        self.run_setup(&wt);
        mark_active(&wt);
        Ok(wt)
    }

    /// Run the repo's `setup` valet subcommands in WT; failures only warn.
    fn run_setup(&self, wt: &Path) {
        let Some(e) = self.entry() else { return };
        let Ok(exe) = crate::cmd::apps::self_bin() else {
            return;
        };
        for line in &e.setup {
            eprintln!(
                "valet: setup {}: {} {line}",
                self.slug,
                crate::generated::name()
            );
            let r = Cmd::new(&exe)
                .args(line.split_whitespace())
                .cwd(wt)
                .env("PWD", wt)
                .timeout(Duration::from_secs(30 * 60))
                .stdout_to_stderr()
                .run();
            if let Err(err) = r {
                crate::warn!("{}: setup `{line}` failed: {err:#}", self.slug);
            }
        }
    }

    /// Copy untracked, ignored files matching `include` (config.toml: the
    /// `[repos]` list plus the repo's own) from the home-branch worktree into
    /// the new worktree WT: local settings and generated files git does not
    /// carry. Existing files are kept.
    fn copy_includes(&self, wt: &Path) -> Result<()> {
        let Ok(m) = manifest() else { return Ok(()) };
        let mut pats = m.include.clone();
        if let Some(e) = self.entry() {
            pats.extend(e.include);
        }
        if pats.is_empty() {
            return Ok(());
        }
        let mut sources = Vec::new();
        for b in [self.home_branch().ok(), self.default_branch().ok()]
            .into_iter()
            .flatten()
        {
            sources.push(self.worktree(&b));
        }
        let Some(src) = sources
            .into_iter()
            .find(|s| s != wt && s.join(".git").exists())
        else {
            return Ok(());
        };
        let mut c = git(&src).args([
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "-z",
            "--",
        ]);
        for p in &pats {
            c = c.arg(format!(":(glob){p}"));
        }
        let out = c.timeout(GIT_TIMEOUT).checked()?;
        let mut n = 0;
        for rel in out.split('\0').filter(|r| !r.is_empty()) {
            let (from, to) = (src.join(rel), wt.join(rel));
            if fs::symlink_metadata(&to).is_ok() {
                continue;
            }
            if let Some(d) = to.parent() {
                fs::create_dir_all(d)?;
            }
            match fs::read_link(&from) {
                Ok(t) => std::os::unix::fs::symlink(t, &to)?,
                Err(_) => {
                    fs::copy(&from, &to).with_context(|| format!("copy {}", from.display()))?;
                }
            }
            n += 1;
        }
        if n > 0 {
            eprintln!("valet: copied {n} include file(s) from {}", src.display());
        }
        Ok(())
    }

    /// The branch valet opens when none is named: the first listed branch of a
    /// `worktrees = false` repo (e.g. a shared repo with one personal branch), else the default.
    pub fn home_branch(&self) -> Result<String> {
        match self.entry() {
            Some(e) if !e.worktrees && !e.branches.is_empty() => Ok(e.branches[0].clone()),
            _ => self.default_branch(),
        }
    }

    /// This repo's manifest entry.
    fn entry(&self) -> Option<Entry> {
        manifest()
            .ok()?
            .repos
            .into_iter()
            .find(|e| slug_of_url(&e.url).as_deref() == Some(self.slug.as_str()))
    }

    /// Refuse a worktree the manifest does not allow (`worktrees = false`).
    fn check_worktree_allowed(&self, branch: &str) -> Result<()> {
        let Some(e) = self.entry() else {
            return Ok(());
        };
        let listed = if e.branches.is_empty() {
            self.default_branch().is_ok_and(|d| d == branch)
        } else {
            e.branches.iter().any(|b| b == branch)
        };
        if e.worktrees
            || listed
            || std::env::var_os("WORKTREE_REASON").is_some_and(|r| !r.is_empty())
        {
            return Ok(());
        }
        bail!(
            "{}: no worktrees besides {} (config.toml `worktrees = false`); \
             set WORKTREE_REASON=<why> to override",
            self.slug,
            e.branches.join(", ")
        )
    }

    fn parked(&self, branch: &str) -> PathBuf {
        self.dir.join(".parked").join(branch)
    }

    fn unpark(&self, branch: &str, wt: &Path) -> Result<()> {
        let p = self.parked(branch);
        for name in PARK {
            let from = p.join(name);
            if from.exists() && !wt.join(name).exists() {
                fs::rename(&from, wt.join(name))?;
            }
        }
        let _ = fs::remove_dir(&p);
        Ok(())
    }

    /// Remove `branch`'s worktree, keeping the branch. Refuses the default
    /// branch and anything with changes (tracked, untracked or in a
    /// submodule); `PARK` notes are kept for the next `add`.
    pub fn fold(&self, branch: &str) -> Result<()> {
        if branch == self.default_branch()? {
            bail!("{}: the default branch stays checked out", self.slug);
        }
        self.fold_at(&self.worktree(branch))
    }

    /// [`Repo::fold`] for the worktree directory WT under `tree/` (also
    /// a detached one; its path below `tree/` keys the parked notes).
    /// The checkout is renamed into [`Repo::trash`] first, so a failed
    /// delete never leaves a half-deleted worktree; the clean job removes
    /// what is left there.
    pub fn fold_at(&self, wt: &Path) -> Result<()> {
        let top = self.dir.join(TREE);
        let key = wt
            .strip_prefix(&top)
            .ok()
            .filter(|k| !k.as_os_str().is_empty())
            .with_context(|| format!("{}: not under {}", wt.display(), top.display()))?
            .to_string_lossy()
            .into_owned();
        if !wt.exists() {
            return Ok(());
        }
        if let Some(why) = unclean(wt)? {
            bail!("{}: not folded: {why}", wt.display());
        }
        if self.has_ref("refs/stash") {
            // A stash is repo-wide; it does not block, but say so.
            crate::warn!("{}: the repository has stashes", self.slug);
        }
        let p = self.parked(&key);
        for name in PARK {
            if wt.join(name).exists() {
                fs::create_dir_all(&p)?;
                fs::rename(wt.join(name), p.join(name))?;
            }
        }
        // Same filesystem as tree/, so the rename is atomic and instant.
        // `git worktree remove` refuses worktrees with submodules even when
        // clean; the tree is verified clean above.
        let trash = self.trash();
        fs::create_dir_all(&trash)?;
        let stamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let dest = trash.join(format!("{}-{stamp}", key.replace('/', "-")));
        fs::rename(wt, &dest)
            .with_context(|| format!("move {} to {}", wt.display(), dest.display()))?;
        self.git()
            .args(["worktree", "prune"])
            .timeout(GIT_TIMEOUT)
            .run()?;
        // Drop now-empty parents up to tree/ (branch `a/b`).
        let mut d = wt.parent();
        while let Some(x) = d {
            if x == top || fs::remove_dir(x).is_err() {
                break;
            }
            d = x.parent();
        }
        if let Err(e) = remove_tree(&dest) {
            crate::warn!("{e:#}; `valet clean worktrees --run` removes it later");
        }
        Ok(())
    }

    /// New branch BRANCH off the latest default branch (fetched now; see
    /// [`Self::newest_default`]) in its worktree. Refused when BRANCH
    /// exists here or on origin.
    pub fn branch_off_latest(&self, branch: &str) -> Result<PathBuf> {
        if self.has_ref(&format!("refs/heads/{branch}"))
            || self.has_ref(&format!("refs/remotes/origin/{branch}"))
            || self.remote_has(branch)
        {
            bail!("branch {branch:?} exists; open it with `valet repos path {branch}`");
        }
        let def = self.default_branch()?;
        self.git()
            .args(["fetch", "--quiet", "origin", &def])
            .timeout(NET_TIMEOUT)
            .stdout_to_stderr()
            .run()
            .with_context(|| format!("fetch origin {def}"))?;
        self.add(branch, Some(&self.newest_default()?))
    }

    /// Base for a new branch: the local default branch when it holds all of
    /// origin's (it may be ahead, unpushed), else `origin/<default>`.
    pub fn newest_default(&self) -> Result<String> {
        let def = self.default_branch()?;
        let origin = format!("origin/{def}");
        let ahead = git(&self.git_dir())
            .args(["merge-base", "--is-ancestor", &origin, &def])
            .timeout(Duration::from_secs(30))
            .status()
            .is_ok_and(|s| s.success());
        Ok(if ahead { def } else { origin })
    }

    /// Folded checkouts waiting to be deleted (`.git/valet-trash/`).
    pub fn trash(&self) -> PathBuf {
        self.git_dir().join("valet-trash")
    }

    /// Seconds since the worktree was last used (git activity or a mark).
    pub fn idle_secs(&self, wt: &Path) -> Option<u64> {
        SystemTime::now()
            .duration_since(last_used(wt)?)
            .ok()
            .map(|d| d.as_secs())
    }

    /// Branches whose worktree stays: the default, the home branch and the
    /// manifest's `branches` (`repos sync` recreates those).
    pub fn pinned_branches(&self) -> Vec<String> {
        let mut v: Vec<String> = [self.default_branch().ok(), self.home_branch().ok()]
            .into_iter()
            .flatten()
            .collect();
        if let Some(e) = self.entry() {
            v.extend(e.branches);
        }
        v
    }
}

/// When the worktree WT was last used: git activity or a [`mark_active`].
pub fn last_used(wt: &Path) -> Option<SystemTime> {
    let gd = git_dir_of(wt)?;
    ["index", "HEAD", "logs/HEAD", ACTIVE]
        .iter()
        .filter_map(|f| fs::metadata(gd.join(f)).and_then(|m| m.modified()).ok())
        .max()
}

/// Why `wt` cannot be folded, if it cannot (changes: tracked, untracked or
/// in a submodule).
pub fn unclean(wt: &Path) -> Result<Option<String>> {
    let st = git(wt)
        .args([
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ])
        .timeout(GIT_TIMEOUT)
        .checked()?;
    if !st.trim().is_empty() {
        return Ok(Some(format!("{} changed paths", st.lines().count())));
    }
    Ok(None)
}

/// `rm -rf DIR`, also through read-only directories (a Go module cache has
/// no u+w): on a permission error, `chmod -R u+w` and try again.
pub fn remove_tree(dir: &Path) -> Result<()> {
    match fs::remove_dir_all(dir) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => {}
    }
    // Never follows a symlink: -R without -L, and DIR itself is a real dir.
    if fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir()) {
        let _ = Cmd::new("chmod")
            .args(["-R", "u+w", "--"])
            .arg(dir)
            .timeout(Duration::from_secs(1800))
            .output();
    }
    fs::remove_dir_all(dir).with_context(|| format!("remove {}", dir.display()))
}

/// The worktree's own git dir (`.git` file -> `gitdir:`).
fn git_dir_of(wt: &Path) -> Option<PathBuf> {
    let t = fs::read_to_string(wt.join(".git")).ok()?;
    let p = PathBuf::from(t.strip_prefix("gitdir:")?.trim());
    Some(if p.is_absolute() { p } else { wt.join(p) })
}

/// Record use of the worktree holding `path` (for idle folding).
pub fn mark_active(path: &Path) {
    if let Some((_, _, wt)) = locate(path)
        && let Some(gd) = git_dir_of(&wt)
    {
        let _ = fs::write(gd.join(ACTIVE), b"");
    }
}

fn safe_part(p: &str) -> bool {
    !p.is_empty()
        && p != "."
        && p != ".."
        && p.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

/// A branch name usable as a path under tree/ (git's rules, plus no
/// `.`/`..` parts).
/// Names for NEW branches: `claude/<slug>` (an agent's throw-away worktree)
/// or `<user>/<slug>` / `<user>/<TICKET-123>[-<slug>]` (work meant for a PR).
/// slug: lowercase words joined by `-`, at most 50 characters.
pub fn new_branch_name_ok(user: &str, b: &str) -> std::result::Result<(), String> {
    let want = format!(
        "want claude/<short-description> (throw-away agent work) or {user}/<short-description> \
         or {user}/<TICKET-123>[-<short-description>] (PR work); description: lowercase words joined by -"
    );
    let slug = |s: &str| {
        !s.is_empty()
            && s.len() <= 50
            && s.split('-').all(|w| {
                !w.is_empty()
                    && w.chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            })
    };
    let ticket = |s: &str| {
        let (key, rest) = match s.split_once('-') {
            Some(x) => x,
            None => return false,
        };
        let (num, desc) = match rest.split_once('-') {
            Some((n, d)) => (n, Some(d)),
            None => (rest, None),
        };
        key.len() >= 2
            && key
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
            && key.starts_with(|c: char| c.is_ascii_uppercase())
            && !num.is_empty()
            && num.chars().all(|c| c.is_ascii_digit())
            && desc.is_none_or(slug)
    };
    match b.split_once('/') {
        Some(("claude", rest)) if slug(rest) => Ok(()),
        Some((owner, rest))
            if !user.is_empty() && owner == user && (slug(rest) || ticket(rest)) =>
        {
            Ok(())
        }
        _ => Err(want),
    }
}

pub fn branch_ok(b: &str) -> bool {
    !b.is_empty()
        && !b.starts_with('-')
        && !b.ends_with('/')
        && !b.ends_with(".lock")
        && b.split('/').all(|p| {
            !p.is_empty()
                && p != "."
                && p != ".."
                && !p.starts_with('.')
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._-+".contains(c))
        })
}

/// `host/owner/name` from a clone URL (https, ssh, scp-like).
pub fn slug_of_url(url: &str) -> Option<String> {
    let s = url.trim().trim_end_matches('/');
    let s = s.strip_suffix(".git").unwrap_or(s);
    let rest = if let Some(r) = s.split_once("://") {
        r.1.rsplit_once('@').map_or(r.1, |x| x.1).to_owned()
    } else {
        // git@host:owner/name
        let (h, p) = s.split_once(':')?;
        format!("{}/{p}", h.rsplit_once('@').map_or(h, |x| x.1))
    };
    let parts: Vec<&str> = rest.split('/').collect();
    if parts.len() != 3 || parts.iter().any(|p| !safe_part(p)) {
        return None;
    }
    Some(parts.join("/"))
}

/// Clone `url` as a bare repo fetching only [`fetch_specs`] (the default
/// branch and your own; all of an `owned` repo), then check out its home
/// branch under tree/.
pub fn clone(url: &str) -> Result<Repo> {
    let slug = slug_of_url(url).with_context(|| format!("cannot place {url}"))?;
    let r = Repo::at(&slug)?;
    if r.exists() {
        return Ok(r);
    }
    fs::create_dir_all(&r.dir)?;
    Cmd::new("git")
        .args(["init", "--bare", "--quiet"])
        .arg(r.git_dir())
        .timeout(GIT_TIMEOUT)
        .run()?;
    r.git()
        .args(["config", "worktree.useRelativePaths", "true"])
        .timeout(GIT_TIMEOUT)
        .run()?;
    r.git()
        .args(["remote", "add", "origin", url])
        .timeout(GIT_TIMEOUT)
        .run()?;
    let head = r
        .git()
        .args(["ls-remote", "--symref", "origin", "HEAD"])
        .timeout(NET_TIMEOUT)
        .checked()?;
    let def = symref_head(&head).with_context(|| format!("{url}: no default branch"))?;
    r.git()
        .args(["symbolic-ref", "HEAD", &format!("refs/heads/{def}")])
        .timeout(GIT_TIMEOUT)
        .run()?;
    setup_bare(&r)?;
    r.git()
        .args(["fetch", "--quiet", "origin"])
        .timeout(NET_TIMEOUT)
        .run()?;
    let b = r.home_branch()?;
    r.add(&b, None)?;
    Ok(r)
}

/// The branch of `git ls-remote --symref <remote> HEAD` output.
fn symref_head(out: &str) -> Option<String> {
    out.lines().find_map(|l| {
        let (r, name) = l.strip_prefix("ref: ")?.split_once('\t')?;
        (name == "HEAD").then(|| r.strip_prefix("refs/heads/").map(str::to_owned))?
    })
}

/// Remote-tracking refs and origin/HEAD for a bare repo.
fn setup_bare(r: &Repo) -> Result<()> {
    set_fetch_specs(r)?;
    let _ = r
        .git()
        .args(["remote", "set-head", "origin", "--auto"])
        .timeout(NET_TIMEOUT)
        .output();
    Ok(())
}

/// Write [`fetch_specs`] to `remote.origin.fetch` when it differs.
fn set_fetch_specs(r: &Repo) -> Result<()> {
    let want = fetch_specs(r);
    let have = r
        .git()
        .args(["config", "--get-all", "remote.origin.fetch"])
        .timeout(GIT_TIMEOUT)
        .checked()
        .unwrap_or_default();
    if have.lines().collect::<Vec<_>>() != want.iter().map(String::as_str).collect::<Vec<_>>() {
        let _ = r
            .git()
            .args(["config", "--unset-all", "remote.origin.fetch"])
            .timeout(GIT_TIMEOUT)
            .output();
        for s in &want {
            r.git()
                .args(["config", "--add", "remote.origin.fetch", s])
                .timeout(GIT_TIMEOUT)
                .run()?;
        }
    }
    Ok(())
}

fn one_spec(b: &str) -> String {
    format!("+refs/heads/{b}:refs/remotes/origin/{b}")
}

/// Narrow fetch refspecs: the default branch, USER's own (`USER/*`), and
/// EXTRA (listed and taken branches). All branches without a default.
fn specs_for(def: &str, user: &str, extra: &[String]) -> Vec<String> {
    if def.is_empty() {
        return vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()];
    }
    let mut v = vec![one_spec(def)];
    if !user.is_empty() {
        v.push(format!("+refs/heads/{user}/*:refs/remotes/origin/{user}/*"));
    }
    for b in extra {
        let s = one_spec(b);
        if !v.contains(&s) && !spec_covers(&v, &format!("refs/remotes/origin/{b}")) {
            v.push(s);
        }
    }
    v
}

/// One of SPECS fetches into the remote-tracking ref RREF.
fn spec_covers(specs: &[String], rref: &str) -> bool {
    specs.iter().any(|s| {
        let dst = s.rsplit_once(':').map_or(s.as_str(), |x| x.1);
        match dst.strip_suffix('*') {
            Some(pre) => rref.len() > pre.len() && rref.starts_with(pre),
            None => rref == dst,
        }
    })
}

/// Fetch refspecs: only the listed branches for a `worktrees = false` repo
/// (a shared repo may have hundreds of other people's branches); every
/// branch of an `owned` repo; else [`specs_for`] the default branch, $USER's
/// branches, the listed ones and the taken ones (`valet repos take`).
fn fetch_specs(r: &Repo) -> Vec<String> {
    let e = r.entry();
    if let Some(e) = &e
        && !e.worktrees
        && !e.branches.is_empty()
    {
        return e.branches.iter().map(|b| one_spec(b)).collect();
    }
    if r.owned() {
        return vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()];
    }
    let mut extra = e.map(|e| e.branches).unwrap_or_default();
    extra.extend(r.taken());
    let user = std::env::var("USER").unwrap_or_default();
    specs_for(&r.default_branch().unwrap_or_default(), &user, &extra)
}

/// SLUG (`host/owner/name`) is under one of the OWNED prefixes (`host/owner`).
pub fn is_owned(slug: &str, owned: &[String]) -> bool {
    owned.iter().any(|o| slug.starts_with(&format!("{o}/")))
}

impl Repo {
    /// Under a config.toml `owned` owner: every branch is yours.
    fn owned(&self) -> bool {
        manifest().is_ok_and(|m| is_owned(&self.slug, &m.owned))
    }

    /// Branches taken over with `valet repos take` (`valet.take` in the repo's config).
    pub fn taken(&self) -> Vec<String> {
        self.git()
            .args(["config", "--get-all", "valet.take"])
            .timeout(GIT_TIMEOUT)
            .output()
            .map(|o| o.stdout.lines().map(str::to_owned).collect())
            .unwrap_or_default()
    }

    /// The fetch refspecs bring BRANCH in.
    pub fn fetches(&self, branch: &str) -> bool {
        spec_covers(&fetch_specs(self), &format!("refs/remotes/origin/{branch}"))
    }

    /// Take over BRANCH (someone else's): fetch it from now on (`valet.take`),
    /// fetch it now, and make its worktree.
    pub fn take(&self, branch: &str) -> Result<PathBuf> {
        if !branch_ok(branch) {
            bail!("bad branch name {branch:?}");
        }
        if !self.fetches(branch) {
            self.git()
                .args(["config", "--add", "valet.take", branch])
                .timeout(GIT_TIMEOUT)
                .run()?;
            set_fetch_specs(self)?;
        }
        self.git()
            .args(["fetch", "--quiet", "origin", &one_spec(branch)])
            .timeout(NET_TIMEOUT)
            .stdout_to_stderr()
            .run()?;
        self.add(branch, None)
    }

    /// Stop taking BRANCH (drop its `valet.take` line and refspec).
    pub fn untake(&self, branch: &str) -> Result<()> {
        if !self.taken().iter().any(|b| b == branch) {
            return Ok(());
        }
        self.git()
            .args([
                "config",
                "--unset-all",
                "--fixed-value",
                "valet.take",
                branch,
            ])
            .timeout(GIT_TIMEOUT)
            .run()?;
        set_fetch_specs(self)
    }

    /// Whether origin has BRANCH (network).
    fn remote_has(&self, branch: &str) -> bool {
        self.git()
            .args(["ls-remote", "--exit-code", "--heads", "origin"])
            .arg(format!("refs/heads/{branch}"))
            .timeout(crate::proc::NET_TIMEOUT)
            .output()
            .is_ok_and(|o| o.status.success())
    }
}

/// What [`narrow`] removes.
#[derive(Debug, Default, PartialEq)]
pub struct Narrowing {
    /// Remote-tracking refs outside the fetch refspecs.
    pub remote: Vec<String>,
    /// Local branches outside them, checked out nowhere, holding nothing
    /// the remote lacks (tip equals origin's, or merged into the default).
    pub local: Vec<String>,
}

/// The [`Narrowing`] for SPECS over REFS (`(refname, sha)` of refs/heads and
/// refs/remotes/origin), keeping KEEP branches (checked out, default, home)
/// and treating MERGED (local refnames merged into origin/<default>) as safe.
fn narrow_plan(
    specs: &[String],
    refs: &[(String, String)],
    keep: &[String],
    merged: &std::collections::HashSet<String>,
) -> Narrowing {
    let tip: std::collections::HashMap<&str, &str> =
        refs.iter().map(|(r, s)| (r.as_str(), s.as_str())).collect();
    let mut n = Narrowing::default();
    for (r, sha) in refs {
        if let Some(b) = r.strip_prefix("refs/remotes/origin/") {
            if b != "HEAD" && !spec_covers(specs, r) {
                n.remote.push(r.clone());
            }
        } else if let Some(b) = r.strip_prefix("refs/heads/") {
            let rref = format!("refs/remotes/origin/{b}");
            let pushed = tip.get(rref.as_str()) == Some(&sha.as_str());
            if !spec_covers(specs, &rref)
                && !keep.iter().any(|k| k == b)
                && (pushed || merged.contains(r))
            {
                n.local.push(b.to_owned());
            }
        }
    }
    n
}

/// Bring the repo in line with [`fetch_specs`]: write them, and (unless DRY)
/// delete the remote-tracking refs and the safe local branches outside them
/// ([`narrow_plan`]). Objects go with git's own auto gc.
pub fn narrow(r: &Repo, dry: bool) -> Result<Narrowing> {
    let specs = fetch_specs(r);
    let out = r
        .git()
        .args([
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/heads",
            "refs/remotes/origin",
        ])
        .timeout(GIT_TIMEOUT)
        .checked()?;
    let refs: Vec<(String, String)> = out
        .lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(a, b)| (a.to_owned(), b.to_owned()))
        .collect();
    let def = r.default_branch().unwrap_or_default();
    let merged: std::collections::HashSet<String> = r
        .git()
        .args(["for-each-ref", "--format=%(refname)", "--merged"])
        .arg(format!("refs/remotes/origin/{def}"))
        .arg("refs/heads")
        .timeout(GIT_TIMEOUT)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| o.stdout.lines().map(str::to_owned).collect())
        .unwrap_or_default();
    let mut keep: Vec<String> = r.worktrees()?.into_iter().map(|(_, b)| b).collect();
    keep.extend(r.pinned_branches());
    let n = narrow_plan(&specs, &refs, &keep, &merged);
    if dry {
        return Ok(n);
    }
    set_fetch_specs(r)?;
    if n.remote.is_empty() && n.local.is_empty() {
        return Ok(n);
    }
    // origin/HEAD pointing at a dropped ref would dangle.
    if let Ok(o) = r
        .git()
        .args(["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"])
        .timeout(GIT_TIMEOUT)
        .checked()
        && n.remote.contains(&o)
    {
        let _ = r
            .git()
            .args(["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"])
            .timeout(GIT_TIMEOUT)
            .output();
    }
    let tip: std::collections::HashMap<&str, &str> =
        refs.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    let mut stdin = String::new();
    for rf in n
        .remote
        .iter()
        .cloned()
        .chain(n.local.iter().map(|b| format!("refs/heads/{b}")))
    {
        // Only if unchanged since the scan.
        let _ = writeln!(stdin, "delete {rf} {}", tip.get(rf.as_str()).unwrap_or(&""));
    }
    r.git()
        .args(["update-ref", "--stdin"])
        .stdin(stdin.into_bytes())
        .timeout(GIT_TIMEOUT)
        .run()?;
    r.drop_branch_config(&n.local);
    Ok(n)
}

impl Repo {
    /// Remove `branch.<b>.*` of deleted local branches.
    pub fn drop_branch_config(&self, gone: &[String]) {
        let Ok(o) = self
            .git()
            .args(["config", "--name-only", "--get-regexp", r"^branch\."])
            .timeout(GIT_TIMEOUT)
            .output()
        else {
            return;
        };
        let gone: std::collections::HashSet<&str> = gone.iter().map(String::as_str).collect();
        let mut done = std::collections::HashSet::new();
        for key in o.stdout.lines() {
            let Some(b) = key
                .strip_prefix("branch.")
                .and_then(|k| k.rsplit_once('.'))
                .map(|x| x.0)
            else {
                continue;
            };
            if gone.contains(b) && done.insert(b.to_owned()) {
                let _ = self
                    .git()
                    .args(["config", "--remove-section", &format!("branch.{b}")])
                    .timeout(GIT_TIMEOUT)
                    .output();
            }
        }
    }
}

/// Every repo under [`root`] (`<host>/<owner>/<name>/.git`).
pub fn all() -> Result<Vec<Repo>> {
    Ok(all_in(&root()?))
}

/// Every repo under ROOT (`<host>/<owner>/<name>/.git`).
pub fn all_in(root: &Path) -> Vec<Repo> {
    let mut v = Vec::new();
    let dirs = |p: &Path| -> Vec<PathBuf> {
        fs::read_dir(p)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_dir()
                    && !p
                        .file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with('.'))
            })
            .collect()
    };
    for h in dirs(root) {
        for o in dirs(&h) {
            for n in dirs(&o) {
                if n.join(".git/HEAD").is_file()
                    && let Ok(rel) = n.strip_prefix(root)
                {
                    v.push(Repo {
                        slug: rel.to_string_lossy().into_owned(),
                        dir: n,
                    });
                }
            }
        }
    }
    v.sort_by(|a, b| a.slug.cmp(&b.slug));
    v
}

/// PATH's components below the root, in either spelling: the root's (`~/...`)
/// or the physical one (an automount or bind-mount path).
fn rel_parts(path: &Path) -> Option<(PathBuf, Vec<String>)> {
    let root = root().ok()?;
    let rel = path
        .strip_prefix(&root)
        .ok()
        .map(Path::to_path_buf)
        .or_else(|| rel_by_inode(&root, path))?;
    let parts = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    Some((root, parts))
}

/// PATH relative to ROOT when one of PATH's ancestors is ROOT's directory
/// under another name: a symlink, or a bind mount (a local path and its
/// automount path are one directory), matched by device and inode.
fn rel_by_inode(root: &Path, path: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let r = std::fs::metadata(root).ok()?;
    let path = path.canonicalize().ok()?;
    path.ancestors()
        .find(|a| std::fs::metadata(a).is_ok_and(|m| m.dev() == r.dev() && m.ino() == r.ino()))
        .and_then(|a| path.strip_prefix(a).ok().map(Path::to_path_buf))
}

/// P with `.` and `..` resolved by name (no filesystem access).
fn lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// Rewrite DIR's submodule `.git` files and their `core.worktree` to
/// absolute, resolved paths, recursively. git writes them relative, and a
/// relative link made through one view of a path (a symlink, a bind mount)
/// breaks when the checkout is reached through another. Returns how many
/// links it rewrote.
pub fn absolutize_submodules(dir: &Path) -> Result<usize> {
    if !dir.join(".gitmodules").is_file() {
        return Ok(0);
    }
    let out = Cmd::new("git")
        .args([
            "config",
            "--file",
            ".gitmodules",
            "--get-regexp",
            r"^submodule\..*\.path$",
        ])
        .cwd(dir)
        .output()?;
    let mut fixed = 0;
    for line in out.stdout.lines() {
        let Some((_, rel)) = line.split_once(' ') else {
            continue;
        };
        let sub = dir.join(rel);
        let gitfile = sub.join(".git");
        let Ok(text) = std::fs::read_to_string(&gitfile) else {
            continue; // not initialised, or a real .git dir
        };
        let Some(target) = text.trim().strip_prefix("gitdir: ") else {
            continue;
        };
        let gitdir = if Path::new(target).is_absolute() {
            PathBuf::from(target)
        } else {
            let canon_sub = sub.canonicalize().unwrap_or_else(|_| sub.clone());
            let found = [sub.join(target), canon_sub.join(target)]
                .into_iter()
                .map(|p| lexical(&p))
                .find(|p| p.join("HEAD").is_file())
                .with_context(|| {
                    format!("{}: gitdir {target} does not resolve", gitfile.display())
                })?;
            let abs = found.canonicalize()?;
            std::fs::write(&gitfile, format!("gitdir: {}\n", abs.display()))?;
            fixed += 1;
            abs
        };
        let cfg = gitdir.join("config");
        let wt = Cmd::new("git")
            .arg("config")
            .arg("--file")
            .arg(&cfg)
            .args(["--get", "core.worktree"])
            .output()?;
        let cur = wt.stdout.trim();
        if !cur.is_empty() && !Path::new(cur).is_absolute() {
            let abs = sub.canonicalize()?;
            Cmd::new("git")
                .arg("config")
                .arg("--file")
                .arg(&cfg)
                .arg("core.worktree")
                .arg(&abs)
                .checked()?;
            fixed += 1;
        }
        fixed += absolutize_submodules(&sub)?;
    }
    Ok(fixed)
}

/// The repo PATH is in: its top folder, a worktree, or anything below.
pub fn repo_of(path: &Path) -> Option<Repo> {
    let (root, parts) = rel_parts(path)?;
    if parts.len() < 3 {
        return None;
    }
    let slug = parts[..3].join("/");
    let repo = Repo {
        dir: root.join(&slug),
        slug,
    };
    repo.exists().then_some(repo)
}

/// (repo, branch, worktree dir) for a path inside `Repos/.../tree/<b>`
/// (the worktree need not exist: its path names the branch). The branch is
/// the longest existing-or-named prefix: an existing worktree dir (has
/// `.git`) wins, else the first part.
/// `None` at the repo's top folder.
pub fn locate(path: &Path) -> Option<(Repo, String, PathBuf)> {
    let (root, parts) = rel_parts(path)?;
    if parts.len() < 5 || parts[3] != TREE {
        return None;
    }
    let slug = parts[..3].join("/");
    let repo = Repo {
        dir: root.join(&slug),
        slug,
    };
    let rest = &parts[4..];
    for n in (1..=rest.len()).rev() {
        let b = rest[..n].join("/");
        if repo.worktree(&b).join(".git").exists() {
            let wt = repo.worktree(&b);
            return Some((repo, b, wt));
        }
    }
    // Folded: the longest prefix that is a local branch.
    for n in (1..=rest.len()).rev() {
        let b = rest[..n].join("/");
        if repo.has_ref(&format!("refs/heads/{b}")) {
            let wt = repo.worktree(&b);
            return Some((repo, b, wt));
        }
    }
    None
}

/// Recreate the folded worktree a path points into; true when made.
pub fn ensure(path: &Path) -> Result<bool> {
    let Some((repo, branch, wt)) = locate(path) else {
        return Ok(false);
    };
    if wt.join(".git").exists() {
        mark_active(&wt);
        return Ok(false);
    }
    repo.add(&branch, None)?;
    Ok(true)
}

// ---- manifest ([repos] in config.toml) ----

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub default: String,
    /// Owner prefixes (`host/owner`) the daily `repos` job syncs.
    #[serde(default)]
    pub owned: Vec<String>,
    /// Owner prefixes whose repos are shared with a team.
    #[serde(default)]
    pub team: Vec<String>,
    /// Untracked, ignored files (git glob pathspecs) copied into every new
    /// worktree from the repo's home-branch worktree.
    #[serde(default)]
    pub include: Vec<String>,
    /// Short names for checkouts: `<slug>[@<branch>][:<subdir>]`.
    #[serde(default)]
    pub aliases: std::collections::BTreeMap<String, String>,
    #[serde(default, rename = "repo")]
    pub repos: Vec<Entry>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub url: String,
    /// Checked out besides the default branch.
    #[serde(default)]
    pub branches: Vec<String>,
    /// `false`: only `branches` (else the default branch) get worktrees, and
    /// the first one is opened when no branch is named; others are refused
    /// unless `WORKTREE_REASON` says why.
    #[serde(default = "yes")]
    pub worktrees: bool,
    /// Like `[repos] include`, for this repo only.
    #[serde(default)]
    pub include: Vec<String>,
    /// valet subcommands run in every new worktree of this repo (after
    /// clone too), e.g. `run -- make bootstrap`; they reuse shared toolchains.
    #[serde(default)]
    pub setup: Vec<String>,
}

fn yes() -> bool {
    true
}

pub fn manifest() -> Result<Manifest> {
    parse_manifest(crate::config::text()?)
}

/// The `[repos]` section of config.toml TEXT.
pub fn parse_manifest(text: &str) -> Result<Manifest> {
    let m: Manifest = crate::config::section_of(text, "repos")?;
    for e in &m.repos {
        slug_of_url(&e.url).with_context(|| format!("[repos]: cannot place {}", e.url))?;
        for b in &e.branches {
            if !branch_ok(b) {
                bail!("[repos]: bad branch {b:?}");
            }
        }
    }
    Ok(m)
}

/// An alias split into (slug, branch, subdir); branch/subdir may be empty.
pub fn alias_spec(name: &str) -> Option<(String, String, String)> {
    let m = manifest().ok()?;
    let spec = m.aliases.get(name)?;
    let (rest, sub) = spec.split_once(':').unwrap_or((spec, ""));
    let (slug, branch) = rest.split_once('@').unwrap_or((rest, ""));
    Some((slug.to_owned(), branch.to_owned(), sub.to_owned()))
}

/// The repo slug of an alias.
pub fn alias_slug(name: &str) -> Option<String> {
    alias_spec(name).map(|(slug, _, _)| slug)
}

/// The default repo (`default` in config.toml).
pub fn default_repo() -> Result<Repo> {
    Repo::at(&manifest()?.default)
}

/// Path of an alias: its branch's worktree (made when folded), plus the
/// subdir. BRANCH overrides the alias's branch.
pub fn alias(name: &str, branch: Option<&str>) -> Result<PathBuf> {
    let (slug, b, sub) =
        alias_spec(name).with_context(|| format!("no alias {name:?} in config.toml"))?;
    let repo = Repo::at(&slug)?;
    if !repo.exists() {
        bail!("alias {name}: {slug} is not cloned (`valet repos sync`)");
    }
    let branch = match branch {
        Some(b) => b.to_owned(),
        None if b.is_empty() => repo.default_branch()?,
        None => b,
    };
    let wt = repo.add(&branch, None)?;
    Ok(if sub.is_empty() { wt } else { wt.join(sub) })
}

/// Alias path without touching git (for hot paths such as prompts): the
/// worktree dir is computed, not created; `None` when the repo is absent.
pub fn alias_path(name: &str) -> Option<PathBuf> {
    let (slug, b, sub) = alias_spec(name)?;
    let repo = Repo::at(&slug).ok()?;
    let branch = if b.is_empty() {
        repo.default_branch().ok()?
    } else {
        b
    };
    let wt = repo.worktree(&branch);
    Some(if sub.is_empty() { wt } else { wt.join(sub) })
}

/// Clone every manifest repo that is missing and check out its branches.
pub fn sync(dry: bool) -> Result<()> {
    fs::create_dir_all(root()?)?;
    for line in reconcile(dry)? {
        println!("{line}");
    }
    for e in manifest_text(&std::fs::read_to_string(crate::config::source()?)?)?.repos {
        let slug = slug_of_url(&e.url).unwrap_or_default();
        let r = Repo::at(&slug)?;
        if !r.exists() {
            println!("{}clone {}", if dry { "would: " } else { "" }, e.url);
            if dry {
                continue;
            }
            clone(&e.url)?;
        }
        if r.exists() && !dry {
            setup_bare(&r)?;
        }
        // `worktrees = false` repos stay narrowed to `branches`; others
        // narrow on `valet repos narrow --run`.
        if !e.worktrees && r.exists() {
            let n = narrow(&r, dry)?;
            if !n.remote.is_empty() || !n.local.is_empty() {
                println!(
                    "{}{slug}: drop {} remote and {} local branches outside `branches`",
                    if dry { "would: " } else { "" },
                    n.remote.len(),
                    n.local.len()
                );
            }
        }
        for b in &e.branches {
            if !r.worktree(b).join(".git").exists() {
                println!("{}worktree {slug} {b}", if dry { "would: " } else { "" });
                if !dry {
                    r.add(b, None)?;
                }
            }
        }
    }
    Ok(())
}

// ---- lifecycle: clone, deletion, config upkeep ----

/// Slugs valet has seen cloned on this machine (`<Valet>/State/repos-seen`): a
/// listed repo that is missing but was seen has been deleted by the user,
/// one never seen (a fresh machine) is cloned.
fn seen_file() -> Result<PathBuf> {
    let root = crate::cmd::apps::root()?;
    Ok(crate::cmd::apps::self_app(&root).join("State/repos-seen"))
}

fn seen() -> std::collections::BTreeSet<String> {
    seen_file()
        .ok()
        .and_then(|f| fs::read_to_string(f).ok())
        .map(|t| t.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

fn save_seen(s: &std::collections::BTreeSet<String>) -> Result<()> {
    let f = seen_file()?;
    if let Some(d) = f.parent() {
        fs::create_dir_all(d)?;
    }
    let mut t: String = s.iter().map(|x| format!("{x}\n")).collect();
    if t.is_empty() {
        t.push('\n');
    }
    fs::write(&f, t)?;
    Ok(())
}

/// The manifest of config.toml TEXT (fresh from disk, not the cached copy).
fn manifest_text(text: &str) -> Result<Manifest> {
    parse_manifest(text)
}

/// TEXT with `[[repos.repo]] url` added after the last repo entry.
fn add_entry(text: &str, url: &str) -> Result<String> {
    let block = format!(
        "[[repos.repo]]\nurl = {}\n",
        toml::Value::String(url.to_owned())
    );
    let i = text
        .rfind("[[repos.repo]]")
        .context("config.toml has no [[repos.repo]] entry to append after")?;
    // End of that entry: the next table header or section rule.
    let rest = &text[i + 1..];
    let at = ["\n[", "\n# ==="]
        .iter()
        .filter_map(|m| rest.find(m))
        .min()
        .map_or(text.len(), |j| i + 1 + j + 1);
    let mut out = text.to_owned();
    out.insert_str(at, &block);
    Ok(out)
}

/// TEXT without the `[[repos.repo]]` entry for SLUG (`None`: not listed).
fn remove_entry(text: &str, slug: &str) -> Option<String> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() != "[[repos.repo]]" {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < lines.len() && !lines[j].starts_with('[') && !lines[j].starts_with("# ===") {
            j += 1;
        }
        let hit = lines[i + 1..j].iter().any(|l| {
            l.trim_start().starts_with("url")
                && l.split('"').nth(1).and_then(slug_of_url).as_deref() == Some(slug)
        });
        if hit {
            return Some(lines[..i].concat() + &lines[j..].concat());
        }
        i = j;
    }
    None
}

/// Rewrite config.toml with F, refusing a result that does not parse.
fn edit_config(f: impl FnOnce(&str) -> Result<Option<String>>) -> Result<bool> {
    let file = crate::config::source()?;
    let text = fs::read_to_string(&file)?;
    let Some(out) = f(&text)? else {
        return Ok(false);
    };
    crate::config::check(&out)?;
    fs::write(&file, out)?;
    Ok(true)
}

fn config_add(url: &str) -> Result<()> {
    edit_config(|t| add_entry(t, url).map(Some)).map(drop)
}

fn config_remove(slug: &str) -> Result<bool> {
    edit_config(|t| Ok(remove_entry(t, slug)))
}

/// Clone URL the valet way (bare + home-branch worktree), add it to
/// config.toml and record it; returns the worktree.
pub fn clone_add(url: &str) -> Result<PathBuf> {
    let r = clone(url)?;
    let listed = manifest_text(&fs::read_to_string(crate::config::source()?)?)?
        .repos
        .iter()
        .any(|e| slug_of_url(&e.url).as_deref() == Some(r.slug.as_str()));
    if !listed {
        config_add(url)?;
        eprintln!("valet: added {} to config.toml", r.slug);
    }
    let mut s = seen();
    s.insert(r.slug.clone());
    save_seen(&s)?;
    let wt = r.worktree(&r.home_branch()?);
    wt.canonicalize()?;
    Ok(crate::cmd::apps::canonical(&wt))
}

/// Bring config.toml and git in line with the folders: a listed repo deleted
/// from disk leaves config.toml; a repo folder that is not listed joins it;
/// worktrees deleted from disk are pruned (their branches stay). Returns what
/// changed.
pub fn reconcile(dry: bool) -> Result<Vec<String>> {
    let w = if dry { "would: " } else { "" };
    let mut out = Vec::new();
    let present: std::collections::BTreeMap<String, Repo> =
        all()?.into_iter().map(|r| (r.slug.clone(), r)).collect();
    let mut seen = seen();
    let m = manifest_text(&fs::read_to_string(crate::config::source()?)?)?;
    let listed: std::collections::BTreeSet<String> =
        m.repos.iter().filter_map(|e| slug_of_url(&e.url)).collect();
    for slug in listed.difference(&present.keys().cloned().collect()) {
        if seen.contains(slug) {
            out.push(format!("{w}config.toml: drop {slug} (deleted)"));
            if !dry {
                config_remove(slug)?;
                seen.remove(slug);
            }
        }
    }
    for (slug, r) in &present {
        if !listed.contains(slug)
            && let Ok(url) = r
                .git()
                .args(["config", "--get", "remote.origin.url"])
                .timeout(GIT_TIMEOUT)
                .checked()
        {
            out.push(format!("{w}config.toml: add {slug} ({url})"));
            if !dry {
                config_add(&url)?;
            }
        }
        let before = r.worktrees()?;
        if !dry {
            r.git()
                .args(["worktree", "prune"])
                .timeout(GIT_TIMEOUT)
                .run()?;
        }
        for (_, b) in before.iter().filter(|(p, _)| !p.join(".git").exists()) {
            out.push(format!("{w}{slug}: worktree {b} deleted; branch kept"));
        }
    }
    if !dry {
        seen.extend(present.keys().cloned());
        save_seen(&seen)?;
    }
    Ok(out)
}

/// Directories whose entries change when a repo or worktree is added or
/// deleted: owner dirs, each `tree/` and its nested branch dirs (the
/// `repos` part of `valet daemon` watches them).
pub fn watch_dirs() -> Result<Vec<PathBuf>> {
    let root = root()?;
    let mut v = Vec::new();
    for host in fs::read_dir(&root).into_iter().flatten().flatten() {
        if !host.path().is_dir() {
            continue;
        }
        v.push(host.path());
        for owner in fs::read_dir(host.path()).into_iter().flatten().flatten() {
            if owner.path().is_dir() {
                v.push(owner.path());
            }
        }
    }
    for r in all()? {
        let wts = r.dir.join(TREE);
        let mut stack = vec![wts];
        while let Some(d) = stack.pop() {
            if !d.is_dir() || d.join(".git").exists() {
                continue;
            }
            for e in fs::read_dir(&d).into_iter().flatten().flatten() {
                stack.push(e.path());
            }
            v.push(d);
        }
    }
    v.sort();
    Ok(v)
}

/// Fold clean worktrees idle longer than `idle`; returns what was folded.
pub fn gc(idle: Duration, dry: bool) -> Result<Vec<String>> {
    let mut done = Vec::new();
    for r in all()? {
        let def = r.default_branch().unwrap_or_default();
        let home = r.home_branch().unwrap_or_default();
        for (wt, b) in r.worktrees()? {
            // Older worktrees may be recorded under another spelling of the root.
            let under = crate::cmd::apps::canonical(&wt).starts_with(r.dir.join(TREE));
            if b == def || b == home || !under {
                continue;
            }
            let Some(secs) = r.idle_secs(&wt) else {
                continue;
            };
            if secs < idle.as_secs() {
                continue;
            }
            if dry {
                done.push(format!(
                    "would fold {} {b} (idle {}d)",
                    r.slug,
                    secs / 86_400
                ));
                continue;
            }
            match r.fold(&b) {
                Ok(()) => done.push(format!("folded {} {b}", r.slug)),
                Err(e) => crate::warn!("{e:#}"),
            }
        }
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    #[test]
    fn new_branch_names_follow_the_pattern() {
        for ok in [
            "claude/agent-a1",
            "claude/fix-login",
            "me/gpu-health-monitor",
            "me/ABC-42",
            "me/ABC-42-arm-build",
        ] {
            assert!(new_branch_name_ok("me", ok).is_ok(), "{ok}");
        }
        for bad in [
            "feature-x",
            "me/Fix_Login",
            "other/topic",
            "claude/ABC-42",
            "me/abc42_x",
            "me/",
            "claude/a--b",
        ] {
            assert!(new_branch_name_ok("me", bad).is_err(), "{bad}");
        }
    }

    use super::*;

    #[test]
    fn narrow_refspecs_and_plan() {
        let s = specs_for(
            "main",
            "me",
            &["bob/x".into(), "me/y".into(), "main".into()],
        );
        assert_eq!(
            s,
            [
                "+refs/heads/main:refs/remotes/origin/main",
                "+refs/heads/me/*:refs/remotes/origin/me/*",
                "+refs/heads/bob/x:refs/remotes/origin/bob/x",
            ]
        );
        assert!(spec_covers(&s, "refs/remotes/origin/me/a/b"));
        assert!(!spec_covers(&s, "refs/remotes/origin/me/"));
        assert!(!spec_covers(&s, "refs/remotes/origin/mex"));
        assert!(!spec_covers(&s, "refs/remotes/origin/bob/z"));
        assert_eq!(
            specs_for("", "me", &[]),
            ["+refs/heads/*:refs/remotes/origin/*"]
        );
        assert_eq!(
            symref_head("ref: refs/heads/master\tHEAD\nabc\tHEAD\n").as_deref(),
            Some("master")
        );
        assert_eq!(symref_head("abc\tHEAD\n"), None);

        let refs: Vec<(String, String)> = [
            ("refs/heads/main", "1"),
            ("refs/heads/me/a", "2"),
            ("refs/heads/old", "3"),    // pushed, same tip: goes
            ("refs/heads/local", "4"),  // only here: stays
            ("refs/heads/merged", "5"), // merged into origin/main: goes
            ("refs/heads/used", "6"),   // checked out: stays
            ("refs/heads/moved", "7"),  // origin moved on, not merged: stays
            ("refs/remotes/origin/HEAD", "1"),
            ("refs/remotes/origin/main", "1"),
            ("refs/remotes/origin/me/a", "2"),
            ("refs/remotes/origin/old", "3"),
            ("refs/remotes/origin/used", "6"),
            ("refs/remotes/origin/moved", "8"),
        ]
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
        let merged = ["refs/heads/merged".to_owned(), "refs/heads/main".to_owned()]
            .into_iter()
            .collect();
        let n = narrow_plan(&s, &refs, &["used".into(), "main".into()], &merged);
        assert_eq!(
            n.remote,
            [
                "refs/remotes/origin/old",
                "refs/remotes/origin/used",
                "refs/remotes/origin/moved"
            ]
        );
        assert_eq!(n.local, ["old", "merged"]);
    }

    #[test]
    fn rel_by_inode_maps_other_views() {
        let d = std::env::temp_dir().join(format!("valet-test-{}-views", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("real/a/b")).unwrap();
        std::os::unix::fs::symlink(d.join("real"), d.join("view")).unwrap();
        assert_eq!(
            rel_by_inode(&d.join("real"), &d.join("view/a/b")),
            Some(PathBuf::from("a/b"))
        );
        assert_eq!(rel_by_inode(&d.join("real/a"), &d.join("view")), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn submodule_links_become_absolute() {
        let d = std::env::temp_dir().join(format!("valet-test-{}-subm", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let (wt, gd) = (d.join("wt"), d.join("git/modules/sub"));
        std::fs::create_dir_all(wt.join("sub")).unwrap();
        std::fs::create_dir_all(&gd).unwrap();
        std::fs::write(gd.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(gd.join("config"), "[core]\n\tworktree = ../../../wt/sub\n").unwrap();
        std::fs::write(
            wt.join(".gitmodules"),
            "[submodule \"sub\"]\n\tpath = sub\n\turl = x\n",
        )
        .unwrap();
        std::fs::write(wt.join("sub/.git"), "gitdir: ../../git/modules/sub\n").unwrap();
        assert_eq!(absolutize_submodules(&wt).unwrap(), 2);
        let abs = gd.canonicalize().unwrap();
        assert_eq!(
            std::fs::read_to_string(wt.join("sub/.git")).unwrap(),
            format!("gitdir: {}\n", abs.display())
        );
        assert!(
            std::fs::read_to_string(gd.join("config"))
                .unwrap()
                .contains(&wt.join("sub").canonicalize().unwrap().display().to_string())
        );
        assert_eq!(absolutize_submodules(&wt).unwrap(), 0, "idempotent");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn urls_and_names() {
        for u in [
            "https://github.com/acme/widgets.git",
            "git@github.com:acme/widgets.git",
            "ssh://git@github.com/acme/widgets",
            "https://user@github.com/acme/widgets/",
        ] {
            assert_eq!(
                slug_of_url(u).as_deref(),
                Some("github.com/acme/widgets"),
                "{u}"
            );
        }
        assert!(slug_of_url("https://github.com/a/../b").is_none());
        assert!(branch_ok("alice/widget-server-3.1") && branch_ok("main"));
        let m = parse_manifest(crate::config::EMBEDDED).expect("config.toml parses");
        assert!(m.aliases.contains_key("valet"));
        for b in ["", "-x", "a/", "a/../b", ".hidden", "a.lock", "a b"] {
            assert!(!branch_ok(b), "{b:?}");
        }
    }

    #[test]
    fn config_entries_add_and_remove() {
        let t = "[repos]\ndefault = \"github.com/a/b\"\n\n[[repos.repo]]\nurl = \"https://github.com/a/b\"\n\
                 [[repos.repo]]\nurl = \"https://github.com/c/d\"\nbranches = [\"x\"]\n\n# === env\n[env.default]\n";
        let added = add_entry(t, "https://github.com/e/f").unwrap();
        assert!(added.contains(
            "branches = [\"x\"]\n\n[[repos.repo]]\nurl = \"https://github.com/e/f\"\n# === env"
        ));
        let removed = remove_entry(&added, "github.com/c/d").unwrap();
        assert!(!removed.contains("c/d") && !removed.contains("branches"));
        assert!(
            removed.contains("a/b") && removed.contains("e/f") && removed.contains("[env.default]")
        );
        assert!(remove_entry(t, "github.com/none/x").is_none());
    }
}
