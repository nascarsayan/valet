//! `valet doctor`: report prerequisites and the managed tools (name, pinned
//! version, where each resolves: on PATH, managed dir, or missing).

use crate::{proc, tools};
use anyhow::{Result, bail};

/// System tools `valet` needs but does not install.
const PREREQS: &[(&str, &str)] = &[
    ("git", "repo, cd"),
    ("curl", "tool install"),
    ("sha256sum", "tool install"),
    ("tar", "tool install"),
    ("unzip", "tool install (broot)"),
    ("gzip", "tool install (tar.gz archives)"),
];

pub fn run() -> Result<()> {
    let mut missing = 0;
    println!("prerequisites:");
    for (tool, used_by) in PREREQS {
        match proc::which(tool) {
            Some(p) => println!("  ok       {tool:<9} {}", p.display()),
            None => {
                missing += 1;
                println!("  missing  {tool:<9} (needed by: {used_by})");
            }
        }
    }
    let dir = tools::managed_dir()?;
    println!(
        "managed tools (installed on demand into {}):",
        dir.display()
    );
    for t in tools::TOOLS {
        let (src, path) = match tools::locate(t.name)? {
            tools::Source::Path(p) => ("path", p.display().to_string()),
            tools::Source::Managed(p) => ("managed", p.display().to_string()),
            tools::Source::Missing => ("missing", "installs on first use".to_owned()),
        };
        println!("  {src:<8} {:<9} {:<9} {path}", t.name, t.version);
    }
    let on_path =
        std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d == dir));
    if !on_path {
        println!(
            "note: {} is not on PATH; add eval \"$(command valet shell-init zsh)\" to ~/.zshrc",
            dir.display()
        );
    }
    if missing > 0 {
        bail!("{missing} prerequisite(s) missing");
    }
    Ok(())
}
