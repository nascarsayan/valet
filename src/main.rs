//! `valet` — one entry point for day-to-day development utilities.
//!
//! Each top-level command lives in its own module under `cmd/`. Add a command
//! by adding a module, a variant to [`Command`], and a dispatch arm in `main`.

mod cmd;
mod config;
mod generated;
mod log;
mod net;
mod proc;
mod repos;
mod template;
mod tools;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "valet",
    version,
    about,
    propagate_version = true,
    after_help = "Internal commands (run by shell startup, hooks and jobs) are hidden; `valet [COMMAND...] --help-all` shows them."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the path of a place or picked repo/worktree (the `wt` shell function cds there).
    #[command(hide = true)]
    Cd(cmd::repo::PickArgs),
    /// Claude Code <-> Slack: mirror running Claude sessions to a Slack bot DM and drive them from Slack (afk, session, dm, daemon).
    Cc(cmd::cc::Args),
    /// Slack: send markdown to your Slack bot DM (`dm`); tables and code render natively.
    Slack(cmd::cc::SlackArgs),
    /// Userland apps (<Userland>, beside the valet binary): per-architecture tools from mise, one folder per app.
    Apps(cmd::apps::Args),
    /// valet's one config file (config.toml in the valet repo): path, edit, check, show.
    Config(cmd::config::Args),
    /// Run a command with the environment `cd` gives this directory (env rules, repo build dirs); for shells that skip the cd hook.
    Run(cmd::run::Args),
    /// Project folders from templates (macOS layout, README with rules): list, create.
    Tpl(cmd::tpl::Args),
    /// The `sandbox` alias's checkout: new topic folder (`valet tpl create` there).
    Sandbox(cmd::tpl::SandboxArgs),
    /// Git checkouts under <Userland>/Repos: sync from config.toml, worktrees per branch, fold idle ones, list and pick.
    Repos(cmd::repos::Args),
    /// GitHub: review worktrees and threads, the current branch's PR.
    Git(cmd::git::Args),
    /// Managed external CLI tools (fzf, gh, jq, yq): install, path.
    Tools(cmd::tools::Args),
    /// Check prerequisites and list the managed tools (version, where each resolves).
    Doctor,
    /// Print shell integration: PATH, `wt`, hooks, completions.
    ///
    /// Add to ~/.zshrc: eval "$(command valet shell-init zsh)"
    #[command(hide = true)]
    ShellInit(cmd::shell::InitArgs),
    /// Every hook valet runs (shell cd/startup, git, Claude Code): list, enable, disable, install.
    #[command(long_about = cmd::hooks::LONG_ABOUT)]
    Hooks(cmd::hooks::Args),
    /// Agent skills: ~/.claude/skills generated from config.toml [skills] (list, apply).
    Skills(cmd::skills::Args),
    /// Pull, check, rebuild and replace this binary from its source repo.
    ///
    /// `git pull --ff-only` first (skipped with a note when the tree is dirty
    /// or the branch has no upstream), then fmt/clippy/test, release build,
    /// and replace the `valet` on PATH.
    Upgrade(cmd::upgrade::Args),
    /// Values exported into every shell, secret (`pass`) or plain, in groups: ls, set, get, mv, rm, exec, lock.
    Vars(cmd::vars::Args),
    /// Background jobs on a daily timer: upgrade, dotfiles, repos, upstream repos, tool pins, disk cleanup (list, status, run, enable, disable).
    Job(cmd::job::Args),
    /// One user unit for every long-running part (cc, herdr tabs, repos watch, daily jobs): install, status, on, off, restart, logs.
    Daemon(cmd::daemon::Args),
    #[command(flatten)]
    Cleanup(cmd::cleanup::Cmd),
    #[command(flatten)]
    Githooks(cmd::githooks::Cmd),
}

/// Git hook names `valet` answers to when invoked through a symlink
/// (core.hooksPath dir of symlinks to the binary): `pre-push ARGS` runs as
/// `valet hook pre-push ARGS`.
const HOOK_NAMES: &[&str] = &[
    "pre-commit",
    "pre-push",
    "post-checkout",
    "post-commit",
    "post-merge",
];

fn argv() -> Vec<std::ffi::OsString> {
    let mut args: Vec<_> = std::env::args_os().collect();
    let invoked = args
        .first()
        .and_then(|a| std::path::Path::new(a).file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if HOOK_NAMES.contains(&invoked.as_str()) {
        args.splice(
            0..1,
            [env!("CARGO_BIN_NAME").into(), "hook".into(), invoked.into()],
        );
    }
    args
}

/// `valet [COMMAND...] --help-all`: that command's help with hidden (internal)
/// commands and flags shown.
fn help_all(args: &[std::ffi::OsString]) -> Option<String> {
    use clap::CommandFactory;
    fn unhide(c: clap::Command) -> clap::Command {
        c.hide(false)
            .mut_args(|a| a.hide(false))
            .mut_subcommands(unhide)
    }
    if !args.iter().any(|a| a == "--help-all") {
        return None;
    }
    let mut cmd = unhide(Cli::command());
    cmd.build();
    for w in args.iter().skip(1).filter(|a| *a != "--help-all") {
        match cmd.find_subcommand(w) {
            Some(s) => cmd = s.clone(),
            None => break,
        }
    }
    Some(cmd.render_long_help().to_string())
}

/// Rust ignores SIGPIPE, so `valet ... | head` would panic on the closed pipe.
/// Restore the default so `valet` exits quietly like any other CLI.
fn reset_sigpipe() {
    unsafe extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
    }
    const SIGPIPE: i32 = 13;
    const SIG_DFL: usize = 0;
    // SAFETY: called once at startup, before any threads exist.
    unsafe {
        signal(SIGPIPE, SIG_DFL);
    }
}

fn main() -> std::process::ExitCode {
    reset_sigpipe();
    let args = argv();
    if let Some(help) = help_all(&args) {
        print!("{help}");
        return std::process::ExitCode::SUCCESS;
    }
    let cli = Cli::parse_from(args);
    let result = match cli.command {
        Command::Cd(args) => cmd::repo::pick(args),
        Command::Cc(args) => cmd::cc::run(args),
        Command::Slack(args) => cmd::cc::slack_cmd(args),
        Command::Git(args) => cmd::git::run(args),
        Command::Apps(args) => cmd::apps::run(args),
        Command::Repos(args) => cmd::repos::run(args),
        Command::Config(args) => cmd::config::run(args),
        Command::Run(args) => cmd::run::run(args),
        Command::Tools(args) => cmd::tools::run(args),
        Command::Doctor => cmd::doctor::run(),
        Command::ShellInit(args) => cmd::shell::init(args),
        Command::Hooks(args) => cmd::hooks::run(args),
        Command::Vars(args) => cmd::vars::run(args),
        Command::Skills(args) => cmd::skills::run(args),
        Command::Tpl(args) => cmd::tpl::run(args),
        Command::Sandbox(args) => cmd::tpl::run_sandbox(args),
        Command::Upgrade(args) => cmd::upgrade::run(args),
        Command::Job(args) => cmd::job::run(args),
        Command::Daemon(args) => cmd::daemon::run(args),
        Command::Cleanup(c) => cmd::cleanup::run(c),
        Command::Githooks(c) => cmd::githooks::run(c),
    };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("valet: {err:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
