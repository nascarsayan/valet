//! One module per command group. Groups with a flattened `Cmd` enum
//! contribute top-level commands directly.

pub mod apps;
pub mod cc;
pub mod claude_overlay;
pub mod cleanup;
pub mod config;
pub mod daemon;
pub mod doctor;
pub mod envrules;
pub mod git;
pub mod githooks;
pub mod gitignore;
pub mod home_links;
pub mod hooks;
pub mod job;
pub mod pr;
pub mod repo;
pub mod repos;
pub mod run;
pub mod shell;
pub mod skills;
pub mod systemd;
pub mod tools;
pub mod tpl;
pub mod upgrade;
pub mod vars;
pub mod worktree;

/// Shell snippets from every group, appended by `valet shell-init`.
pub fn shell_snippets(zsh: bool) -> Vec<&'static str> {
    let mut v: Vec<&str> = if zsh {
        vec![
            vars::SHELL_ZSH,
            cleanup::SHELL_ZSH,
            githooks::SHELL_ZSH,
            pr::SHELL_ZSH,
            worktree::SHELL_ZSH,
        ]
    } else {
        vec![
            vars::SHELL_BASH,
            cleanup::SHELL_BASH,
            githooks::SHELL_BASH,
            worktree::SHELL_BASH,
        ]
    };
    v.retain(|s| !s.is_empty());
    v
}

pub fn home() -> anyhow::Result<std::path::PathBuf> {
    use anyhow::Context;
    Ok(std::env::var_os("HOME").context("HOME is not set")?.into())
}

/// Claude Code's folder: `$CLAUDE_CONFIG_DIR` (the Claude app's Data), else `~/.claude`.
pub fn claude_dir() -> anyhow::Result<std::path::PathBuf> {
    env_dir("CLAUDE_CONFIG_DIR", ".claude")
}

/// Codex's folder: `$CODEX_HOME` (the Codex app's Data), else `~/.codex`.
pub fn codex_dir() -> anyhow::Result<std::path::PathBuf> {
    env_dir("CODEX_HOME", ".codex")
}

fn env_dir(var: &str, in_home: &str) -> anyhow::Result<std::path::PathBuf> {
    match std::env::var_os(var).filter(|v| !v.is_empty()) {
        Some(d) => Ok(d.into()),
        None => Ok(home()?.join(in_home)),
    }
}

/// A source path: `~/x` under HOME, else `<host>/<owner>/<name>[/<dir>]`
/// under that repo's default-branch worktree (see [`crate::repos`]).
pub fn expand(src: &str) -> anyhow::Result<std::path::PathBuf> {
    if let Some(rest) = src.strip_prefix("~/") {
        return Ok(home()?.join(rest));
    }
    let parts: Vec<&str> = src.splitn(4, '/').collect();
    anyhow::ensure!(
        parts.len() >= 3,
        "source {src:?}: want host/owner/name[/dir]"
    );
    let repo = crate::repos::Repo::at(&parts[..3].join("/"))?;
    let wt = repo.worktree(&repo.default_branch()?);
    Ok(match parts.get(3) {
        Some(dir) => wt.join(dir),
        None => wt,
    })
}
