//! `valet skills`: `~/.claude/skills` is generated from `[skills]` in config.toml (read live
//! at build time). Each skill is a symlink to its source directory in a ghq
//! clone; edits to a SKILL.md are live, adding or removing one is a manifest
//! change plus `valet upgrade && valet skills apply`.

use crate::proc;
use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::time::Duration;

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// Show every manifest skill and its state, plus unmanaged entries.
    List,
    /// Make ~/.claude/skills match the manifest: clone missing source repos,
    /// link new skills, fix wrong links, remove links no longer listed.
    /// Real directories are never deleted, only reported.
    Apply {
        /// Print the plan only.
        #[arg(short = 'n', long)]
        dry_run: bool,
        /// Also pull every source repo (default branch, fast-forward only).
        #[arg(short, long)]
        update: bool,
    },
}

pub fn run(args: Args) -> Result<()> {
    let owned = parse(crate::config::text()?)?;
    let skills: Vec<(&str, &str)> = owned
        .iter()
        .map(|(n, s)| (n.as_str(), s.as_str()))
        .collect();
    let dir = super::claude_dir()?.join("skills");
    match args.cmd {
        Cmd::List => {
            for (name, src) in &skills {
                let want = super::expand(src)?;
                println!("{:<30} {:<8} {src}", name, state(&dir.join(name), &want));
            }
            for (name, what) in extras(&dir, &skills)? {
                println!("{name:<30} {what:<8} (not in manifest)");
            }
            Ok(())
        }
        Cmd::Apply { dry_run, update } => apply(&dir, &skills, dry_run, update),
    }
}

/// One manifest skill: its name, the `host/owner/repo` it lives in (None for
/// `~/` sources) and its source directory on disk.
pub struct Source {
    pub name: String,
    pub repo: Option<String>,
    pub dir: std::path::PathBuf,
}

/// Every manifest skill with its source directory expanded.
pub fn sources() -> Result<Vec<Source>> {
    parse(crate::config::text()?)?
        .into_iter()
        .map(|(name, src)| {
            Ok(Source {
                repo: repo_of(&src),
                dir: super::expand(&src)?,
                name,
            })
        })
        .collect()
}

/// `(name, source)` pairs from the `[skills]` section of config.toml TEXT.
pub fn parse(text: &str) -> Result<Vec<(String, String)>> {
    let t: std::collections::BTreeMap<String, String> = crate::config::section_of(text, "skills")?;
    for (n, src) in &t {
        let ok = !n.is_empty()
            && n.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !ok || src.contains("..") {
            bail!("config.toml [skills]: bad entry {n} = {src:?}");
        }
    }
    Ok(t.into_iter().collect())
}

/// `host/owner/repo` of a repo source, None for `~/` paths.
fn repo_of(src: &str) -> Option<String> {
    if src.starts_with("~/") {
        return None;
    }
    let parts: Vec<&str> = src.splitn(4, '/').collect();
    (parts.len() >= 3).then(|| parts[..3].join("/"))
}

fn state(link: &Path, want: &Path) -> &'static str {
    match fs::symlink_metadata(link) {
        Err(_) => "missing",
        Ok(m) if !m.file_type().is_symlink() => "real-dir",
        Ok(_) if crate::cmd::apps::link_is(link, want) => {
            if want.join("SKILL.md").is_file() {
                "ok"
            } else {
                "broken"
            }
        }
        Ok(_) => "relink",
    }
}

/// Entries in the skills dir that the manifest does not list.
fn extras(dir: &Path, skills: &[(&str, &str)]) -> Result<Vec<(String, &'static str)>> {
    let listed: BTreeSet<&str> = skills.iter().map(|(n, _)| *n).collect();
    let mut out = Vec::new();
    for e in fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        if listed.contains(name.as_str()) {
            continue;
        }
        if e.file_type()?.is_symlink() {
            out.push((name, "remove"));
        } else {
            out.push((name, "real-dir"));
        }
    }
    out.sort();
    Ok(out)
}

fn apply(dir: &Path, skills: &[(&str, &str)], dry: bool, update: bool) -> Result<()> {
    let repos: BTreeSet<String> = skills.iter().filter_map(|(_, s)| repo_of(s)).collect();
    for slug in &repos {
        let repo = crate::repos::Repo::at(slug)?;
        let present = repo.exists();
        if present && !update {
            // Links point into the default branch's worktree: make it.
            if !dry {
                repo.add(&repo.default_branch()?, None)?;
            }
            continue;
        }
        println!("{} {slug}", if present { "pull" } else { "clone" });
        if dry {
            continue;
        }
        if !present {
            crate::repos::clone(&format!("https://{slug}"))?;
        } else {
            let wt = repo.add(&repo.default_branch()?, None)?;
            proc::Cmd::new("git")
                .arg("-C")
                .arg(&wt)
                .args(["pull", "--ff-only", "--quiet"])
                .timeout(Duration::from_secs(300))
                .run()?;
        }
    }
    let mut problems = Vec::new();
    for (name, src) in skills {
        let link = dir.join(name);
        let want = super::expand(src)?;
        match state(&link, &want) {
            "ok" => continue,
            "real-dir" => problems.push(format!(
                "{name}: real directory in the way; move it out, then re-run"
            )),
            "broken" => problems.push(format!("{name}: {} has no SKILL.md", want.display())),
            s => {
                if !dry && !want.join("SKILL.md").is_file() {
                    problems.push(format!("{name}: {} has no SKILL.md", want.display()));
                    continue;
                }
                println!(
                    "{} {name} -> {}",
                    if s == "missing" { "link" } else { "relink" },
                    want.display()
                );
                if !dry {
                    // Relative within Userland: every host reads the link.
                    crate::cmd::apps::symlink(&want, &link)?;
                }
            }
        }
    }
    for (name, what) in extras(dir, skills)? {
        if what == "remove" {
            println!("unlink {name}");
            if !dry {
                fs::remove_file(dir.join(&name))?;
            }
        } else {
            problems.push(format!(
                "{name}: real directory not in manifest; left alone"
            ));
        }
    }
    for p in &problems {
        crate::warn!("{p}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parses() {
        parse(crate::config::EMBEDDED).unwrap();
    }

    #[test]
    fn parse_rejects_bad_lines() {
        assert!(parse("[skills]\nA = \"x\"").is_err());
        assert!(parse("[skills]\na = \"../x\"").is_err());
        assert!(parse("[skills]\na = 1").is_err());
        assert_eq!(
            parse("[skills]\na = \"x\" # note\n").unwrap(),
            vec![("a".to_owned(), "x".to_owned())]
        );
    }

    #[test]
    fn repo_of_takes_three_parts() {
        assert_eq!(
            repo_of("github.com/o/r/skills/x").as_deref(),
            Some("github.com/o/r")
        );
        assert_eq!(repo_of("github.com/o/r").as_deref(), Some("github.com/o/r"));
        assert_eq!(repo_of("~/x/y"), None);
    }
}
