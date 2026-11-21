//! `valet tools`: install the external CLIs `valet` manages and find them
//! (`valet doctor` lists them).

use crate::tools::{self, Source};
use anyhow::{Context, Result, bail};

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// Install pinned releases into the managed dir (default: all missing).
    Install {
        /// Reinstall even if the tool is already available.
        #[arg(long)]
        force: bool,
        /// Tools to install; all managed tools if omitted.
        names: Vec<String>,
    },
    /// Print the path a tool resolves to, installing it if missing; with no
    /// name, print the managed bin dir.
    Path { name: Option<String> },
}

pub fn run(args: Args) -> Result<()> {
    match args.cmd {
        Cmd::Install { force, names } => install(force, &names),
        Cmd::Path { name: Some(name) } => {
            println!("{}", tools::resolve(&name)?.display());
            Ok(())
        }
        Cmd::Path { name: None } => {
            println!("{}", tools::managed_dir()?.display());
            Ok(())
        }
    }
}

fn install(force: bool, names: &[String]) -> Result<()> {
    let selected: Vec<_> = if names.is_empty() {
        tools::TOOLS.iter().collect()
    } else {
        names
            .iter()
            .map(|n| {
                tools::get(n)
                    .with_context(|| format!("{n} is not a managed tool (see `valet doctor`)"))
            })
            .collect::<Result<_>>()?
    };
    let mut failed = 0;
    for t in selected {
        if !force && !matches!(tools::locate(t.name)?, Source::Missing) {
            println!(
                "{}: already available, skipping (use --force to reinstall)",
                t.name
            );
            continue;
        }
        if let Err(e) = tools::install(t) {
            eprintln!("valet: {}: {e:#}", t.name);
            failed += 1;
        }
    }
    if failed > 0 {
        bail!("{failed} tool(s) failed to install");
    }
    Ok(())
}
