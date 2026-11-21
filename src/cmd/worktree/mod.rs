//! Review: `valet git review ...` (local `.worktree-review` threads written by
//! the Git Worktree Diff VS Code extension; `sync`: GitHub PR review threads).
//!
//! Contract: `Cmd` is a clap Subcommand enum flattened into its parent, so
//! each variant becomes a command name. `run` dispatches. `SHELL_ZSH` /
//! `SHELL_BASH` are snippets appended to `valet shell-init` output.

mod json;
mod pr_sync;
mod review;
mod store;

use anyhow::Result;

pub const SHELL_ZSH: &str = "";
pub const SHELL_BASH: &str = "";

#[derive(clap::Subcommand)]
pub enum Cmd {
    /// Local review threads in <worktree>/.worktree-review (Git Worktree Diff).
    ///
    /// Agent-facing: list, show, reply to, add, and set the status of threads.
    /// Only the reviewer may set "resolved"; agents use addressed / rejected / noted.
    /// `sync fetch|push`: GitHub PR review threads <-> local markdown.
    Review(review::Args),
}

pub fn run(cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::Review(a) => review::run(a),
    }
}
