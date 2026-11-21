//! Cleanup: `valet clean` frees disk space; the shell snippet keeps core
//! dumps from being written in the first place.

mod disk;

use anyhow::Result;

/// Soft limit only, so `ulimit -c unlimited` in one shell still allows a dump
/// for a debugging session. Children (Claude Code, builds, tests) inherit it.
const NO_CORES: &str = "# valet: no core dumps (core.<pid> files in the working directory) from this shell's\n# processes; for one debug session run: ulimit -c unlimited\nulimit -S -c 0 2>/dev/null\n";
pub const SHELL_ZSH: &str = NO_CORES;
pub const SHELL_BASH: &str = NO_CORES;

#[derive(clap::Subcommand)]
pub enum Cmd {
    /// Free disk space by removing regenerable or inactive files.
    ///
    /// Finds them on this machine's disks; reports only, unless --run is given.
    /// Never touched: Claude Code memory/, settings, credentials, anything
    /// open by a running process, the worktree or directory a process runs in.
    Clean(disk::Args),
}

pub fn run(cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::Clean(a) => disk::run(a),
    }
}
