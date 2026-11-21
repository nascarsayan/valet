//! `valet config`: valet's one configuration file ([`crate::config`]).

use anyhow::{Context, Result};

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// Print the file's path (the default).
    Path,
    /// Open the file in $VISUAL / $EDITOR (else vi), then check it.
    Edit,
    /// Check that every section parses.
    Check,
    /// Print the file valet is reading (the baked-in copy when unlinked).
    Show,
}

pub fn run(a: Args) -> Result<()> {
    match a.cmd.unwrap_or(Cmd::Path) {
        Cmd::Path => {
            println!("{}", shown()?.display());
            Ok(())
        }
        Cmd::Edit => {
            let file = crate::config::source()?;
            let editor = ["VISUAL", "EDITOR"]
                .iter()
                .find_map(|v| std::env::var(v).ok().filter(|e| !e.trim().is_empty()))
                .unwrap_or_else(|| "vi".into());
            let mut words = editor.split_whitespace();
            let prog = words.next().context("empty $EDITOR")?;
            let st = crate::proc::Cmd::new(prog)
                .args(words)
                .arg(&file)
                .status()?;
            if !st.success() {
                anyhow::bail!("{prog} exited with {st}");
            }
            check(&file)
        }
        Cmd::Check => check(&crate::config::source()?),
        Cmd::Show => {
            print!("{}", crate::config::text()?);
            Ok(())
        }
    }
}

/// The checkout file when linked, else the link's path.
fn shown() -> Result<std::path::PathBuf> {
    crate::config::source().or_else(|_| crate::config::path())
}

fn check(file: &std::path::Path) -> Result<()> {
    let text = std::fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
    crate::config::check(&text).with_context(|| format!("{} is invalid", file.display()))?;
    println!("ok {}", file.display());
    Ok(())
}
