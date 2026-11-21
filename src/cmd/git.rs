//! `valet git`: GitHub-side commands in one group: review worktrees, local
//! review threads and the current branch's PR. Local checkouts are
//! `valet repos`.

use anyhow::Result;

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    cmd: Sub,
}

#[derive(clap::Subcommand)]
enum Sub {
    #[command(flatten)]
    Worktree(super::worktree::Cmd),
    /// The GitHub PR of the current git branch (number, state, URL).
    ///
    /// The zsh prompt shows it through the p10k `valet_pr` segment, from a
    /// cache refreshed in the background every 10 minutes.
    Pr(super::pr::Args),
}

pub fn run(a: Args) -> Result<()> {
    match a.cmd {
        Sub::Worktree(w) => super::worktree::run(w),
        Sub::Pr(p) => super::pr::run(p),
    }
}
