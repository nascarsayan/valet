//! Claude Code worktree hooks.
//!
//! `valet hook claude worktree`: a PreToolUse hook that recreates a folded
//! worktree (see [`crate::repos`]) before a tool runs in it: the session's cwd
//! and file-tool paths are checked. It also marks the worktree active, so the
//! idle fold leaves it alone. Prints nothing; always exits 0.
//!
//! `worktree-create` / `worktree-remove`: the WorktreeCreate and
//! WorktreeRemove hooks, so `claude --worktree` checkouts follow the repos
//! layout (`<repo>/tree/$USER/<name>`) and fold back when done.

use crate::proc::Cmd;
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

const GIT: Duration = Duration::from_secs(600);

/// Paths in the hook input that may point into a worktree.
fn paths(input: &Value) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(c) = input["cwd"].as_str() {
        v.push(PathBuf::from(c));
    }
    let ti = &input["tool_input"];
    for k in ["file_path", "path", "notebook_path"] {
        if let Some(p) = ti[k].as_str().filter(|p| p.starts_with('/')) {
            v.push(PathBuf::from(p));
        }
    }
    v
}

pub fn pre_tool_use() {
    let Some(input) = stdin_json() else {
        return;
    };
    for p in paths(&input) {
        match crate::repos::ensure(&p) {
            Ok(true) => eprintln!("valet: recreated the worktree for {}", p.display()),
            Ok(false) => {}
            Err(e) => eprintln!("valet: worktree for {}: {e:#}", p.display()),
        }
    }
}

/// The hook JSON on stdin (at most 4 MiB).
fn stdin_json() -> Option<Value> {
    let mut s = String::new();
    std::io::stdin().take(4 << 20).read_to_string(&mut s).ok()?;
    serde_json::from_str(&s).ok()
}

/// WorktreeCreate input: (name, cwd). The name must be one path part that is
/// also a valid branch name; cwd absolute.
fn create_input(input: &Value) -> Option<(String, PathBuf)> {
    let name = input["name"].as_str()?;
    let cwd = PathBuf::from(input["cwd"].as_str()?);
    (crate::repos::branch_ok(name) && !name.contains('/') && cwd.is_absolute())
        .then(|| (name.to_owned(), cwd))
}

/// The branch for a Claude worktree: `<user>/<name>` (own branches carry the
/// user prefix), just NAME without a user.
/// Claude's own worktrees are throw-away: `claude/<name as a slug>`. Branches
/// meant for a PR are made on purpose (`valet repos path <user>/<desc>`).
fn branch_for(name: &str) -> String {
    let mut slug = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    let slug: String = slug.trim_end_matches('-').chars().take(50).collect();
    format!("claude/{}", slug.trim_end_matches('-'))
}

/// Absolute, without `.` or `..` parts (Claude Code rejects those).
fn plain_abs(p: &Path) -> bool {
    p.is_absolute()
        && p.to_string_lossy()
            .split('/')
            .all(|s| s != "." && s != "..")
}

/// Claude Code's own place for a worktree of the checkout TOP.
fn fallback_path(top: &Path, name: &str) -> PathBuf {
    top.join(".claude/worktrees").join(name)
}

fn git_out(dir: &Path, args: &[&str]) -> Result<String> {
    Cmd::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .timeout(GIT)
        .checked()
}

/// Make the worktree; its path.
fn make(name: &str, cwd: &Path, managed: bool) -> Result<PathBuf> {
    let head = git_out(cwd, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]).ok();
    if managed && let Some(repo) = crate::repos::repo_of(cwd) {
        let branch = branch_for(name);
        return repo.add(&branch, head.as_deref());
    }
    let top = PathBuf::from(git_out(cwd, &["rev-parse", "--show-toplevel"])?);
    let wt = fallback_path(&top, name);
    if !wt.join(".git").exists() {
        let o = Cmd::new("git")
            .arg("-C")
            .arg(&top)
            .args(["worktree", "add", "--quiet", "--relative-paths", "--"])
            .arg(&wt)
            .timeout(GIT)
            .output()?;
        eprint!("{}", o.stderr);
        if !o.status.success() {
            bail!("git worktree add {} failed", wt.display());
        }
    }
    Ok(wt)
}

/// WorktreeCreate: print the new worktree's absolute path; the exit code.
/// MANAGED: the hook is enabled (else always plain git).
pub fn create(managed: bool) -> i32 {
    let Some((name, cwd)) = stdin_json().as_ref().and_then(create_input) else {
        eprintln!(
            "valet: WorktreeCreate: want JSON with a branch-safe `name` and an absolute `cwd`"
        );
        return 1;
    };
    // The path as git records it (symlinks resolved): Claude compares the
    // printed path with the worktree's own toplevel and refuses a mismatch.
    let made = make(&name, &cwd, managed).map(|p| {
        git_out(&p, &["rev-parse", "--show-toplevel"])
            .map(PathBuf::from)
            .unwrap_or(p)
    });
    match made {
        Ok(p) if plain_abs(&p) => {
            println!("{}", p.display());
            0
        }
        Ok(p) => {
            eprintln!(
                "valet: WorktreeCreate: {} is not a plain absolute path",
                p.display()
            );
            1
        }
        Err(e) => {
            eprintln!("valet: WorktreeCreate {name}: {e:#}");
            1
        }
    }
}

/// Fold a managed worktree, else `git worktree remove`.
fn discard(wt: &Path, managed: bool) -> Result<()> {
    if managed
        && let Some((repo, branch, root)) = crate::repos::locate(wt)
        && std::fs::canonicalize(&root).ok() == std::fs::canonicalize(wt).ok()
    {
        return repo.fold(&branch);
    }
    if !wt.exists() {
        return Ok(());
    }
    // From the common git dir: never "the current worktree".
    let common = git_out(
        wt,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    Cmd::new("git")
        .arg("--git-dir")
        .arg(&common)
        .args(["worktree", "remove", "--"])
        .arg(wt)
        .timeout(GIT)
        .checked()
        .map(drop)
        .context("git worktree remove")
}

/// WorktreeRemove: the exit code. MANAGED: the hook is enabled.
pub fn remove(managed: bool) -> i32 {
    let Some(wt) = stdin_json()
        .and_then(|v| v["worktree_path"].as_str().map(PathBuf::from))
        .filter(|p| p.is_absolute())
    else {
        eprintln!("valet: WorktreeRemove: want JSON with an absolute `worktree_path`");
        return 1;
    };
    match discard(&wt, managed) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("valet: WorktreeRemove {}: {e:#}", wt.display());
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn collects_cwd_and_absolute_file_paths() {
        let i = json!({"cwd": "/r/a", "tool_input": {"file_path": "/r/b/x", "path": "rel"}});
        assert_eq!(
            paths(&i),
            vec![PathBuf::from("/r/a"), PathBuf::from("/r/b/x")]
        );
    }

    #[test]
    fn worktree_create_input_and_paths() {
        let i = json!({"name": "agent-a1", "cwd": "/r/x", "session_id": "s"});
        assert_eq!(
            create_input(&i),
            Some(("agent-a1".to_owned(), PathBuf::from("/r/x")))
        );
        for bad in [
            json!({"name": "a/b", "cwd": "/r"}),
            json!({"name": "..", "cwd": "/r"}),
            json!({"name": "", "cwd": "/r"}),
            json!({"name": "-x", "cwd": "/r"}),
            json!({"name": "a", "cwd": "rel"}),
            json!({"cwd": "/r"}),
        ] {
            assert_eq!(create_input(&bad), None, "{bad}");
        }
        assert_eq!(branch_for("agent-a1"), "claude/agent-a1");
        assert_eq!(branch_for("Fix Login_bug!"), "claude/fix-login-bug");
        assert!(crate::repos::branch_ok(&branch_for("agent-a1")));
        assert!(crate::repos::new_branch_name_ok("me", &branch_for("agent-a1")).is_ok());
        assert_eq!(
            fallback_path(Path::new("/r/top"), "n"),
            PathBuf::from("/r/top/.claude/worktrees/n")
        );
        assert!(plain_abs(Path::new("/R/h/o/n/tree/me/a")));
        assert!(!plain_abs(Path::new("/R/h/./n")) && !plain_abs(Path::new("/R/../n")));
        assert!(!plain_abs(Path::new("rel/x")));
    }

    #[test]
    fn unmanaged_repo_uses_claude_place_and_git_worktree_remove() {
        let d = std::env::temp_dir().join(format!("valet-test-{}-cwt", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let d = d.canonicalize().unwrap();
        let git = |args: &[&str]| {
            Cmd::new("git")
                .arg("-C")
                .arg(&d)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .timeout(GIT)
                .checked()
                .unwrap()
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["commit", "-q", "--allow-empty", "-m", "a"]);
        // Outside <Userland>/Repos: the plain-git fallback even when enabled.
        let wt = make("n1", &d, true).unwrap();
        assert_eq!(wt, d.join(".claude/worktrees/n1"));
        assert!(wt.join(".git").is_file());
        // Again: the existing worktree is returned.
        assert_eq!(make("n1", &d, false).unwrap(), wt);
        discard(&wt, true).unwrap();
        assert!(!wt.exists());
        assert!(!git(&["worktree", "list"]).contains("n1"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
