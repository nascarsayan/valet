//! `valet run -- CMD...`: run CMD with the environment the cd hook gives the
//! current directory (env rules, `[env.repo]`), for shells that never run
//! the hook: Claude Code's, scripts, `zsh -c`.

use anyhow::Result;

#[derive(clap::Args)]
pub struct Args {
    /// The command and its arguments.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
    cmd: Vec<String>,
}

pub fn run(a: Args) -> Result<()> {
    let exe = crate::cmd::apps::self_bin()?;
    let cwd = std::env::current_dir()?;
    // zsh -c reads .zshenv (PATH, app env); the hook output then adds what
    // `cd` here would. $0 is valet itself.
    let status = crate::proc::Cmd::new("zsh")
        .arg("-c")
        .arg(r#"eval "$("$0" hooks run chpwd)" && exec "$@""#)
        .arg(&exe)
        .args(&a.cmd)
        .env("PWD", &cwd)
        .status()?;
    use std::os::unix::process::ExitStatusExt;
    std::process::exit(
        status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)),
    )
}
