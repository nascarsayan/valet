//! `valet tpl`: project folders from templates (templates/project/), with the
//! macOS-style layout. `valet sandbox new` is the short form for a folder in
//! the checkout named by the `sandbox` alias.
//!
//! Kept and committed: README.md (guidelines; edit it), Sources/, Results/.
//! Throw-away (gitignored, reaped by `valet clean scratch` after 3 idle days):
//! Builds/, Caches/, Downloads/, Logs/, Scratch/, made only when used.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

const README: &str = include_str!("../../templates/project/README.md");
const RUST: &[(&str, &str)] = &[
    (
        "Sources/Cargo.toml",
        include_str!("../../templates/project/rust/Cargo.toml"),
    ),
    (
        "Sources/.cargo/config.toml",
        include_str!("../../templates/project/rust/.cargo/config.toml"),
    ),
    (
        "Sources/src/main.rs",
        include_str!("../../templates/project/rust/src/main.rs"),
    ),
];
const RUST_RUN: &str = "
## Run

```sh
cd Sources && cargo run --release
```
";

/// Templates: name, what `create` makes.
const TEMPLATES: &[(&str, &str)] = &[
    (
        "rust",
        "README, Results/, and a Rust crate in Sources/ building into Builds/",
    ),
    ("topic", "README, Sources/, Results/ only"),
];

#[derive(clap::Args)]
#[command(after_help = "\
Examples:
  valet tpl list
  valet tpl create rust ~/ws/x/my-tool    a new folder at that path
  valet sandbox new parser-benchmark      rust template, in the sandbox")]
pub struct Args {
    #[command(subcommand)]
    cmd: Sub,
}

#[derive(clap::Subcommand)]
enum Sub {
    /// The templates and what each makes.
    List,
    /// Make DIR (must not exist) from TEMPLATE; its README names the layout
    /// and rules, edit it next.
    Create {
        #[arg(value_parser = clap::builder::PossibleValuesParser::new(TEMPLATES.iter().map(|t| t.0)))]
        template: String,
        /// New folder; its name must be lowercase letters, digits and `-`.
        dir: PathBuf,
    },
}

/// `valet sandbox`: project folders in the checkout the `sandbox` alias names.
#[derive(clap::Args)]
pub struct SandboxArgs {
    #[command(subcommand)]
    cmd: SandboxSub,
}

#[derive(clap::Subcommand)]
enum SandboxSub {
    /// New topic folder in the sandbox: `valet tpl create <TEMPLATE> <sandbox>/NAME`.
    New {
        /// Folder name: lowercase letters, digits and `-` (e.g. parser-benchmark).
        name: String,
        #[arg(short, long, default_value = "rust",
              value_parser = clap::builder::PossibleValuesParser::new(TEMPLATES.iter().map(|t| t.0)))]
        template: String,
    },
}

pub fn run(a: Args) -> Result<()> {
    match a.cmd {
        Sub::List => {
            for (n, what) in TEMPLATES {
                println!("{n:6} {what}");
            }
            Ok(())
        }
        Sub::Create { template, dir } => create(&template, &dir),
    }
}

pub fn run_sandbox(a: SandboxArgs) -> Result<()> {
    match a.cmd {
        SandboxSub::New { name, template } => {
            valid_name(&name)?;
            let root = crate::repos::alias_path("sandbox")
                .context("no sandbox checkout (alias `sandbox` in config.toml)")?;
            create(&template, &root.join(name))
        }
    }
}

fn create(template: &str, dir: &Path) -> Result<()> {
    let name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| format!("{}: no folder name", dir.display()))?;
    valid_name(name)?;
    if dir.exists() {
        bail!("{} exists", dir.display());
    }
    for (rel, body) in files(name, template == "rust") {
        write(&dir.join(rel), &body)?;
    }
    println!("created {} ({template})", dir.display());
    println!("next: edit {}", dir.join("README.md").display());
    Ok(())
}

/// Relative path and content of every file the layout starts with.
fn files(name: &str, rust: bool) -> Vec<(String, String)> {
    let fill = |s: &str| {
        s.replace("{{name}}", name)
            .replace("{{run}}", if rust { RUST_RUN } else { "" })
    };
    let mut v = vec![
        ("README.md".to_owned(), fill(README)),
        ("Sources/.gitkeep".to_owned(), String::new()),
        ("Results/.gitkeep".to_owned(), String::new()),
    ];
    if rust {
        v.retain(|(p, _)| p != "Sources/.gitkeep");
        v.extend(RUST.iter().map(|(p, s)| ((*p).to_owned(), fill(s))));
    }
    v
}

fn valid_name(n: &str) -> Result<()> {
    let ok = n.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && n.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !ok {
        bail!("{n:?}: lowercase letters, digits and `-` only (e.g. parser-benchmark)");
    }
    Ok(())
}

fn write(path: &Path, body: &str) -> Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).with_context(|| format!("create {}", d.display()))?;
    }
    std::fs::write(path, body).with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_layout_is_filled_in() {
        let f = files("my-tool", true);
        let paths: Vec<&str> = f.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            [
                "README.md",
                "Results/.gitkeep",
                "Sources/Cargo.toml",
                "Sources/.cargo/config.toml",
                "Sources/src/main.rs"
            ]
        );
        assert!(f.iter().all(|(_, s)| !s.contains("{{")));
        assert!(f[0].1.starts_with("# my-tool\n") && f[0].1.contains("cargo run"));
        assert!(f[2].1.contains("name = \"my-tool\""));
    }

    #[test]
    fn topic_has_no_crate() {
        let f = files("t", false);
        assert_eq!(f.len(), 3);
        assert!(!f[0].1.contains("cargo run"));
    }

    #[test]
    fn names_are_checked() {
        assert!(valid_name("parser-benchmark-2").is_ok());
        for bad in ["", "-x", "X", "a b", "a/b", "../x", "a_b"] {
            assert!(valid_name(bad).is_err(), "{bad:?}");
        }
    }
}
