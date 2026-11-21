//! Git hooks: gitleaks pre-commit, published-PR guard on pre-push, and the
//! Git hooks (gitleaks, LFS); messages and exit codes match the old shell hooks.

use super::{exit, shell_code, strip_newlines};
use crate::{proc, tools};
use anyhow::{Context, Result};
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Hooks `valet hooks install` links; keep in sync with `HOOK_NAMES` in main.rs.
const HOOKS: [&str; 5] = [
    "pre-commit",
    "pre-push",
    "post-checkout",
    "post-commit",
    "post-merge",
];

const ZERO: &str = "0000000000000000000000000000000000000000";
const GH_TIMEOUT: Duration = Duration::from_secs(30);

/// `${NAME:-default}`.
/// `$GITLEAKS_BIN`, else `gitleaks` on PATH; an error (the commit is
/// refused) when neither exists.
fn gitleaks_bin() -> Result<PathBuf> {
    std::env::var_os("GITLEAKS_BIN")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| proc::which("gitleaks"))
        .context("gitleaks not found: set GITLEAKS_BIN or put it on PATH (`valet apps install`)")
}

/// The personal gitleaks config (`gitleaks.toml` in the valet repo, written
/// locked to the generated dir by `valet hooks install`).
fn personal_gitleaks() -> PathBuf {
    crate::cmd::apps::root()
        .map(|r| crate::cmd::apps::generated_dir(&r).join("gitleaks.toml"))
        .unwrap_or_default()
}

fn env_or(name: &str, default: PathBuf) -> PathBuf {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or(default)
}

/// Run git captured, passing its stderr through; returns (exit code, stdout).
fn git(args: &[&OsStr]) -> Result<(i32, String)> {
    let o = proc::Cmd::new("git").args(args).output()?;
    eprint!("{}", o.stderr);
    Ok((shell_code(o.status), strip_newlines(&o.stdout).to_owned()))
}

/// Exit code a shell uses when it cannot run a program.
fn spawn_failure_code(err: &anyhow::Error) -> i32 {
    match err.root_cause().downcast_ref::<std::io::Error>() {
        Some(e) if e.kind() == std::io::ErrorKind::NotFound => 127,
        _ => 126,
    }
}

/// Run `prog args` with the terminal inherited and exit with its status.
fn run_and_exit(hook: &str, cmd: proc::Cmd) -> ! {
    match cmd.status() {
        Ok(s) => exit(shell_code(s)),
        Err(e) => {
            eprintln!("{hook}: {e:#}");
            exit(spawn_failure_code(&e))
        }
    }
}

/// `test -x`: executable by this process.
fn is_executable(p: &Path) -> bool {
    unsafe extern "C" {
        fn access(path: *const std::ffi::c_char, mode: i32) -> i32;
    }
    const X_OK: i32 = 1;
    let Ok(c) = std::ffi::CString::new(p.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: c is a valid NUL-terminated path.
    unsafe { access(c.as_ptr(), X_OK) == 0 }
}

/// True if `p` resolves to the running `valet` binary (a hooks dir of `valet`
/// symlinks chaining to itself would loop forever).
fn is_self(p: &Path) -> bool {
    match (std::env::current_exe(), p.canonicalize()) {
        (Ok(me), Ok(target)) => me.canonicalize().is_ok_and(|me| me == target),
        _ => false,
    }
}

/// 1-based numbers of the lines of `text` gitleaks flags (personal config, as
/// for commits). `text` is scanned as a file in a private dir under
/// `$XDG_RUNTIME_DIR` (tmpfs), removed afterwards. Fails closed: an error
/// when gitleaks cannot run or its report cannot be read.
fn gitleaks_lines(text: &str) -> Result<std::collections::BTreeSet<usize>> {
    use std::os::unix::fs::DirBuilderExt;
    #[derive(serde::Deserialize)]
    struct Finding {
        #[serde(rename = "StartLine")]
        start: usize,
        #[serde(rename = "EndLine")]
        end: usize,
    }
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join(format!("valet-gitleaks-{}", std::process::id()));
    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    let scan = (|| -> Result<std::collections::BTreeSet<usize>> {
        let file = dir.join("scan.txt");
        let report = dir.join("report.json");
        std::fs::write(&file, text)?;
        let mut cmd = proc::Cmd::new(gitleaks_bin()?)
            .args(["dir", "--no-banner", "--redact", "--exit-code", "0"])
            .args(["--report-format", "json", "--report-path"])
            .arg(&report);
        let personal = env_or("GITLEAKS_CONFIG", personal_gitleaks());
        if personal.is_file() {
            let mut flag = OsString::from("--config=");
            flag.push(&personal);
            cmd = cmd.arg(flag);
        }
        let out = cmd.arg(&file).timeout(Duration::from_secs(300)).output()?;
        if !out.status.success() {
            anyhow::bail!("gitleaks failed: {}", out.stderr.trim());
        }
        let found: Vec<Finding> =
            serde_json::from_str(&std::fs::read_to_string(&report).context("gitleaks report")?)
                .context("gitleaks report")?;
        Ok(found
            .iter()
            .flat_map(|f| f.start..=f.end.max(f.start))
            .collect())
    })();
    let _ = std::fs::remove_dir_all(&dir);
    scan
}

/// Scan staged changes with gitleaks, then chain to the repo's own hook.
/// gitleaks over the staged changes of the repository at `top`, secrets
/// redacted from its report. Personal config: upstream default ruleset plus
/// narrow, documented allowlists for verified false-positive shapes. A
/// repository that ships its own .gitleaks.toml wins: gitleaks auto-detects
/// it, so don't override.
fn gitleaks_staged(top: &Path) -> Result<proc::Cmd> {
    let mut config = None;
    if !top.join(".gitleaks.toml").exists() {
        let personal = env_or("GITLEAKS_CONFIG", personal_gitleaks());
        if personal.is_file() {
            let mut flag = OsString::from("--config=");
            flag.push(&personal);
            config = Some(flag);
        }
    }
    let gitleaks = gitleaks_bin()?;
    Ok(proc::Cmd::new(&gitleaks)
        .args(["git", "--pre-commit", "--redact", "--staged", "--verbose"])
        .args(config)
        .arg(top))
}

/// The single commit a merge in progress brings in (`MERGE_HEAD`), or None when
/// no merge is in progress or it merges several heads (octopus).
fn merge_head() -> Result<Option<String>> {
    let (code, path) = git(&[
        "rev-parse".as_ref(),
        "--path-format=absolute".as_ref(),
        "--git-path".as_ref(),
        "MERGE_HEAD".as_ref(),
    ])?;
    if code != 0 {
        return Ok(None);
    }
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    let heads: Vec<&str> = text.split_whitespace().collect();
    Ok(match heads.as_slice() {
        [one] => Some((*one).to_owned()),
        _ => None,
    })
}

/// Lines a unified diff adds: (path, line number in the new file, text).
fn added_lines(patch: &str) -> Vec<(String, usize, String)> {
    let mut out = Vec::new();
    let (mut path, mut line) = (String::new(), 0usize);
    for l in patch.lines() {
        if let Some(p) = l.strip_prefix("+++ ") {
            path = p.strip_prefix("b/").unwrap_or(p).to_owned();
        } else if l.starts_with("--- ") || l.starts_with("diff ") {
        } else if let Some(h) = l.strip_prefix("@@ ") {
            // @@ -a,b +c,d @@: the next new-file line is c.
            line = h
                .split_whitespace()
                .find_map(|f| f.strip_prefix('+'))
                .and_then(|f| f.split(',').next())
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
        } else if let Some(t) = l.strip_prefix('+') {
            out.push((path.clone(), line, t.to_owned()));
            line += 1;
        } else if l.starts_with(' ') {
            line += 1;
        }
    }
    out
}

/// Merge commit: scan only what it adds over the merged-in branch (this
/// branch's own changes plus any conflict resolution). The rest is that
/// branch's history, already in the repository; scanning it against HEAD would
/// flag other people's commits. Fails closed: nothing is committed past a
/// finding, and an error when the diff or gitleaks cannot run.
fn scan_merge(merge_head: &str) -> Result<()> {
    let out = proc::Cmd::new("git")
        .args([
            "diff",
            "--cached",
            "--no-color",
            "--no-ext-diff",
            "-U0",
            merge_head,
        ])
        .timeout(Duration::from_secs(300))
        .output()?;
    if !out.status.success() {
        anyhow::bail!(
            "git diff --cached {merge_head} failed: {}",
            out.stderr.trim()
        );
    }
    let added = added_lines(&out.stdout);
    if added.is_empty() {
        return Ok(());
    }
    let text: Vec<&str> = added.iter().map(|(_, _, t)| t.as_str()).collect();
    let flagged = gitleaks_lines(&text.join("\n"))?;
    if flagged.is_empty() {
        eprintln!(
            "pre-commit: merge with {}: gitleaks scanned the {} line(s) it adds over that branch; no leaks",
            &merge_head[..merge_head.len().min(10)],
            added.len()
        );
        return Ok(());
    }
    eprintln!(
        "pre-commit: gitleaks flags {} line(s) this merge adds over {} (values not shown):",
        flagged.len(),
        &merge_head[..merge_head.len().min(10)]
    );
    for n in flagged {
        if let Some((path, line, _)) = added.get(n - 1) {
            eprintln!("  {path}:{line}");
        }
    }
    exit(1)
}

pub fn pre_commit(args: Vec<OsString>) -> Result<()> {
    let (_, top) = git(&["rev-parse".as_ref(), "--show-toplevel".as_ref()])?;
    if let Some(head) = merge_head()? {
        if let Err(e) = scan_merge(&head) {
            eprintln!("pre-commit: {e:#}");
            exit(1)
        }
    } else {
        let scan = gitleaks_staged(Path::new(&top))?;
        match scan.status() {
            Ok(s) if s.success() => {}
            Ok(s) => exit(shell_code(s)),
            Err(e) => {
                eprintln!("pre-commit: {e:#}");
                exit(spawn_failure_code(&e))
            }
        }
    }

    if crate::cmd::hooks::enabled("git.ascii") {
        let code = super::ascii::staged()?;
        if code != 0 {
            exit(code);
        }
    }

    // Run a repository's pre-existing hook after the global Gitleaks check.
    let (code, common_dir) = git(&[
        "rev-parse".as_ref(),
        "--path-format=absolute".as_ref(),
        "--git-common-dir".as_ref(),
    ])?;
    if code != 0 {
        exit(code);
    }
    let repo_hook = PathBuf::from(format!("{common_dir}/hooks/pre-commit"));
    let global = env_or("GLOBAL_GIT_HOOKS_DIR", crate::cmd::hooks::git_hooks_dir()?);
    let mut global_prefix = global.into_os_string();
    global_prefix.push("/");
    if repo_hook
        .as_os_str()
        .as_bytes()
        .starts_with(global_prefix.as_bytes())
        || is_self(&repo_hook)
    {
        exit(0);
    }
    if is_executable(&repo_hook) {
        let err = proc::Cmd::new(&repo_hook).args(&args).exec().unwrap_err();
        eprintln!("pre-commit: {err:#}");
        exit(spawn_failure_code(&err));
    }
    exit(0)
}

/// owner/repo of a github.com remote, or empty for any other host.
fn github_repo(url: &str) -> &str {
    let url = url.strip_suffix(".git").unwrap_or(url);
    [
        "git@github.com:",
        "ssh://git@github.com/",
        "https://github.com/",
    ]
    .iter()
    .find_map(|p| url.strip_prefix(p))
    .unwrap_or("")
}

/// One pre-push stdin line: `<local ref> <local sha> <remote ref> <remote sha>`.
#[derive(Debug, PartialEq)]
struct Update<'a> {
    local_sha: &'a str,
    remote_ref: &'a str,
    remote_sha: &'a str,
}

/// Split like `read -r a b c d`: whitespace-separated, the last field takes
/// the rest of the line.
fn parse_update(line: &str) -> Update<'_> {
    let mut rest = line.trim_matches([' ', '\t']);
    let mut field = || {
        let end = rest.find([' ', '\t']).unwrap_or(rest.len());
        let f = &rest[..end];
        rest = rest[end..].trim_start_matches([' ', '\t']);
        f
    };
    let _local_ref = field();
    let local_sha = field();
    let remote_ref = field();
    Update {
        local_sha,
        remote_ref,
        remote_sha: rest,
    }
}

/// Decide whether a push may proceed. `rewrites(local, remote)` says whether
/// the update replaces the remote ref with something that does not contain
/// it; `lookup(branch)` returns the published PRs heading the branch (empty
/// if none) or the lookup error. Err is the refusal message.
fn check_updates(
    repo: &str,
    updates: &str,
    mut rewrites: impl FnMut(&str, &str) -> bool,
    mut lookup: impl FnMut(&str) -> Result<String, String>,
) -> Result<(), String> {
    for line in updates.split('\n') {
        let u = parse_update(line);
        if repo.is_empty() || !u.remote_ref.starts_with("refs/heads/") {
            continue;
        }
        if !rewrites(u.local_sha, u.remote_sha) {
            continue;
        }
        let branch = &u.remote_ref["refs/heads/".len()..];
        // Fail closed: if GitHub cannot say the PR is a draft, refuse.
        match lookup(branch) {
            Err(out) => {
                return Err(format!(
                    "pre-push: refusing to force-push or delete {branch}: could not look up its PRs on {repo}:\n{out}\n"
                ));
            }
            Ok(published) if !published.is_empty() => {
                return Err(format!(
                    "pre-push: refusing to force-push or delete {branch}: it heads published PR {published} on {repo}.\n\
                     Add a commit (or merge) instead, or convert the PR to draft first.\n"
                ));
            }
            Ok(_) => {}
        }
    }
    Ok(())
}

/// Does the push replace the remote ref with something that does not contain it?
fn rewrites(local_sha: &str, remote_sha: &str) -> bool {
    if remote_sha == ZERO {
        return false; // new branch
    }
    if local_sha == ZERO {
        return true; // delete
    }
    let commit = format!("{remote_sha}^{{commit}}");
    if !proc::Cmd::new("git")
        .args(["cat-file", "-e", &commit])
        .output()
        .is_ok_and(|o| o.status.success())
    {
        return true;
    }
    match proc::Cmd::new("git")
        .args(["merge-base", "--is-ancestor", remote_sha, local_sha])
        .output()
    {
        Ok(o) => {
            eprint!("{}", o.stderr);
            !o.status.success()
        }
        Err(_) => true,
    }
}

/// Open, non-draft PRs heading `branch` as "#1 #2", via gh with a timeout.
/// Output is stdout then stderr, as the original's `2>&1` capture.
fn published_prs(repo: &str, branch: &str) -> Result<String, String> {
    let gh = tools::resolve("gh").map_err(|e| format!("{e:#}"))?;
    let o = proc::Cmd::new(gh)
        .args([
            "pr",
            "list",
            "--repo",
            repo,
            "--head",
            branch,
            "--state",
            "open",
            "--json",
            "number,isDraft",
            "--jq",
            r##"[.[] | select(.isDraft | not) | "#\(.number)"] | join(" ")"##,
        ])
        .timeout(GH_TIMEOUT)
        .output()
        .map_err(|e| format!("{e:#}"))?;
    let both = format!("{}{}", o.stdout, o.stderr);
    let both = strip_newlines(&both).to_owned();
    if o.status.success() {
        Ok(both)
    } else {
        Err(both)
    }
}

fn lfs_missing(hook: &str) -> ! {
    eprint!(
        "\nThis repository is configured for Git LFS but 'git-lfs' was not found on your path. \
         If you no longer wish to use Git LFS, remove this hook by deleting the '{hook}' file \
         in the hooks directory (set by 'core.hookspath'; usually '.git/hooks').\n\n"
    );
    exit(2)
}

/// Global pre-push. Force pushes and branch deletes are fine while a branch's
/// PR is a draft; once the PR is published (open, not draft), reviewers'
/// comments and links point at its commits, so the hook refuses to rewrite or
/// delete the branch. Then it runs Git LFS.
pub fn pre_push(args: Vec<OsString>) -> Result<()> {
    let Some(remote_url) = args.get(1) else {
        eprintln!("pre-push: $2: unbound variable");
        exit(1)
    };
    let mut raw = Vec::new();
    std::io::stdin()
        .lock()
        .read_to_end(&mut raw)
        .context("reading pre-push stdin")?;
    // `$(cat)`: NUL bytes and trailing newlines dropped.
    raw.retain(|&b| b != 0);
    let updates = String::from_utf8_lossy(&raw);
    let updates = strip_newlines(&updates);

    let repo = github_repo(&remote_url.to_string_lossy()).to_owned();
    if let Err(msg) = check_updates(&repo, updates, rewrites, |b| published_prs(&repo, b)) {
        eprint!("{msg}");
        exit(1);
    }

    if proc::which("git-lfs").is_none() {
        lfs_missing("pre-push");
    }
    // `<<< "$updates"` appends one newline.
    let stdin = format!("{updates}\n");
    run_and_exit(
        "pre-push",
        proc::Cmd::new("git")
            .args(["lfs", "pre-push"])
            .args(&args)
            .stdin(stdin),
    )
}

/// post-checkout / post-commit / post-merge: hand over to `git lfs <hook>`.
pub fn lfs(hook: &str, args: Vec<OsString>) -> Result<()> {
    if proc::which("git-lfs").is_none() {
        lfs_missing(hook);
    }
    let err = proc::Cmd::new("git")
        .args(["lfs", hook])
        .args(&args)
        .exec()
        .unwrap_err();
    eprintln!("{hook}: {err:#}");
    exit(spawn_failure_code(&err))
}

/// Link every hook in `dir` to this binary, replacing stale entries
/// (used by `valet hooks install`).
pub fn install_dir(dir: PathBuf) -> Result<()> {
    // The per-arch launcher: the hooks dir is shared by every machine.
    let exe = crate::cmd::apps::launcher()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    for name in HOOKS {
        let link = dir.join(name);
        if crate::cmd::apps::link_is(&link, &exe) {
            println!("ok      {}", link.display());
            continue;
        }
        // Relative inside Userland: every host reads the hooks dir.
        crate::cmd::apps::symlink(&exe, &link)?;
        println!("linked  {} -> {}", link.display(), exe.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn added_lines_track_paths_and_new_line_numbers() {
        let patch = "diff --git a/x.sh b/x.sh\n--- a/x.sh\n+++ b/x.sh\n@@ -3,0 +4,2 @@\n+one\n+two\n\
                     @@ -10 +12 @@ ctx\n-old\n+new\ndiff --git a/n b/n\n--- /dev/null\n+++ b/n\n\
                     @@ -0,0 +1 @@\n++plus\n\\ No newline at end of file\n";
        assert_eq!(
            added_lines(patch),
            vec![
                ("x.sh".to_owned(), 4, "one".to_owned()),
                ("x.sh".to_owned(), 5, "two".to_owned()),
                ("x.sh".to_owned(), 12, "new".to_owned()),
                ("n".to_owned(), 1, "+plus".to_owned()),
            ]
        );
    }

    #[test]
    fn added_lines_counts_context_and_ignores_removals() {
        let patch = "+++ b/f\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n";
        assert_eq!(
            added_lines(patch),
            vec![("f".to_owned(), 2, "B".to_owned())]
        );
        assert!(added_lines("").is_empty());
    }

    #[test]
    fn github_repo_urls() {
        assert_eq!(
            github_repo("git@github.com:acme/widgets.git"),
            "acme/widgets"
        );
        assert_eq!(github_repo("ssh://git@github.com/o/r"), "o/r");
        assert_eq!(github_repo("https://github.com/o/r.git"), "o/r");
        assert_eq!(github_repo("https://gitlab.com/o/r.git"), "");
        assert_eq!(github_repo("/tmp/bare.git"), "");
        assert_eq!(github_repo("http://github.com/o/r"), "");
    }

    #[test]
    fn parses_update_lines_like_read() {
        let u = parse_update("refs/heads/a 1111 refs/heads/b 2222");
        assert_eq!(
            u,
            Update {
                local_sha: "1111",
                remote_ref: "refs/heads/b",
                remote_sha: "2222"
            }
        );
        assert_eq!(parse_update("  a\t b  c   d e ").remote_sha, "d e");
        assert_eq!(parse_update("").remote_ref, "");
    }

    const R: &str = "o/r";

    fn line(remote_ref: &str, local: &str, remote: &str) -> String {
        format!("refs/heads/x {local} {remote_ref} {remote}")
    }

    #[test]
    fn published_pr_refuses() {
        let up = line("refs/heads/feat", "aaa", "bbb");
        let err = check_updates(
            R,
            &up,
            |_, _| true,
            |b| {
                assert_eq!(b, "feat");
                Ok("#12".into())
            },
        )
        .unwrap_err();
        assert!(
            err.contains(
                "refusing to force-push or delete feat: it heads published PR #12 on o/r."
            )
        );
        assert!(err.contains("convert the PR to draft first"));
    }

    #[test]
    fn lookup_failure_refuses() {
        let up = line("refs/heads/feat", "aaa", "bbb");
        let err = check_updates(R, &up, |_, _| true, |_| Err("gh: boom".into())).unwrap_err();
        assert_eq!(
            err,
            "pre-push: refusing to force-push or delete feat: could not look up its PRs on o/r:\ngh: boom\n"
        );
    }

    #[test]
    fn draft_or_no_pr_allows() {
        let up = line("refs/heads/feat", "aaa", "bbb");
        assert!(check_updates(R, &up, |_, _| true, |_| Ok(String::new())).is_ok());
    }

    #[test]
    fn skips_without_lookup() {
        let no_lookup = |_: &str| -> Result<String, String> { panic!("lookup called") };
        // Fast-forward.
        let up = line("refs/heads/feat", "aaa", "bbb");
        assert!(check_updates(R, &up, |_, _| false, no_lookup).is_ok());
        // Tags and other refs.
        let up = line("refs/tags/v1", ZERO, "bbb");
        assert!(check_updates(R, &up, |_, _| true, no_lookup).is_ok());
        // Non-GitHub remote.
        let up = line("refs/heads/feat", ZERO, "bbb");
        assert!(check_updates("", &up, |_, _| true, no_lookup).is_ok());
        // Empty stdin.
        assert!(check_updates(R, "", |_, _| true, no_lookup).is_ok());
    }

    #[test]
    fn refuses_on_any_bad_line() {
        let up = format!(
            "{}\n{}",
            line("refs/heads/ok", "a", "b"),
            line("refs/heads/pub", "c", "d")
        );
        let err = check_updates(
            R,
            &up,
            |l, _| l == "c",
            |b| {
                Ok(if b == "pub" {
                    "#1 #2".into()
                } else {
                    String::new()
                })
            },
        )
        .unwrap_err();
        assert!(err.contains("delete pub: it heads published PR #1 #2"));
    }

    #[test]
    fn new_branch_and_delete() {
        assert!(!rewrites("aaa", ZERO));
        assert!(rewrites(ZERO, "bbb"));
    }
}
