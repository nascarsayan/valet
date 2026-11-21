//! Dotfiles kept as templates in a git checkout: alias `dotfiles` in
//! config.toml (`<host>/<owner>/<name>[:<subdir>]`).
//!
//! `files.txt` there lists files by their path under `$HOME`; each is stored
//! as `files/<path>` with this machine's paths turned into variables
//! (`{{ home }}`, `{{ ws }}`), so the copy holds no hardcoded path. `capture`
//! (the daily `home` job) copies live files into the repo; `apply` renders
//! them back, keeping the replaced file as `<file>.valet-bak`.

use crate::proc::Cmd;
use crate::template::{self, Vars};
use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    cmd: Sub,
}

#[derive(clap::Subcommand)]
enum Sub {
    /// The managed files and whether each matches its template.
    Diff,
    /// Copy the live files into the dotfiles checkout as templates.
    Capture,
    /// Write the rendered templates over the live files (old copy kept as
    /// `<file>.valet-bak`).
    Apply {
        /// Show which files would change; write nothing.
        #[arg(long)]
        dry_run: bool,
    },
}

pub fn run(a: Args) -> Result<()> {
    match a.cmd {
        Sub::Diff => diff(),
        Sub::Capture => {
            for l in capture()? {
                println!("{l}");
            }
            Ok(())
        }
        Sub::Apply { dry_run } => apply(dry_run),
    }
}

/// The dotfiles checkout (alias `dotfiles`).
fn root() -> Result<PathBuf> {
    crate::repos::alias("dotfiles", None)
}

/// Managed paths under `$HOME`: one per line, `#` comments; no absolute
/// paths or `..`.
fn manifest(root: &Path) -> Result<Vec<String>> {
    let p = root.join("files.txt");
    let text = fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
    let mut v = Vec::new();
    for l in text.lines().map(str::trim) {
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        if l.starts_with('/') || l.split('/').any(|c| c == ".." || c.is_empty()) {
            bail!("{}: bad path {l:?}", p.display());
        }
        v.push(l.to_owned());
    }
    Ok(v)
}

/// This machine's values: `ws` (where `~/ws` really lives) and `home`.
fn vars() -> Result<Vars> {
    let home = crate::cmd::home()?;
    let mut v = Vars::new();
    if let Ok(ws) = home.join("ws").canonicalize() {
        v.insert("ws", ws.display().to_string());
    }
    v.insert("home", home.display().to_string());
    Ok(v)
}

/// Template text for live `text`: `{{`, `{%`, `{#` written as literals, then
/// each value (longest first) replaced by its variable. Rendering drops one
/// trailing newline, so one is added when the file ends in one.
pub fn detemplate(text: &str, vars: &Vars) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, chars.peek()) {
            ('{', Some(&n)) if matches!(n, '{' | '%' | '#') => {
                chars.next();
                out.push_str(&format!("{{{{ \"{{{n}\" }}}}"));
            }
            _ => out.push(c),
        }
    }
    let mut vals: Vec<(&&str, &String)> = vars.iter().filter(|(_, v)| v.len() >= 4).collect();
    vals.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
    for (k, v) in vals {
        out = out.replace(v.as_str(), &format!("{{{{ {k} }}}}"));
    }
    if out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn render(root: &Path, rel: &str, vars: &Vars) -> Result<String> {
    template::render(&root.join("files").join(rel), root, vars)
}

/// The `home` job: live files into the repo. Returns what changed.
pub fn capture() -> Result<Vec<String>> {
    let (root, vars) = (root()?, vars()?);
    let home = crate::cmd::home()?;
    let mut rep = Vec::new();
    for rel in manifest(&root)? {
        let live = home.join(&rel);
        let Ok(text) = fs::read_to_string(&live) else {
            rep.push(format!("~/{rel}: missing here; kept the stored copy"));
            continue;
        };
        let t = detemplate(&text, &vars);
        let stored = root.join("files").join(&rel);
        if fs::read_to_string(&stored).ok().as_deref() == Some(t.as_str()) {
            continue;
        }
        fs::create_dir_all(stored.parent().context("no parent")?)?;
        fs::write(&stored, &t)?;
        if render(&root, &rel, &vars)? != text {
            bail!("~/{rel}: the stored template does not render back to the file");
        }
        rep.push(format!("~/{rel}: captured"));
    }
    if rep.is_empty() {
        rep.push("all files unchanged".into());
    }
    Ok(rep)
}

/// Dry run of [`capture`]: which files it would capture.
pub fn changed() -> Result<Vec<String>> {
    let (root, vars) = (root()?, vars()?);
    let home = crate::cmd::home()?;
    let mut rep = Vec::new();
    for rel in manifest(&root)? {
        let Ok(text) = fs::read_to_string(home.join(&rel)) else {
            continue;
        };
        let stored = fs::read_to_string(root.join("files").join(&rel)).ok();
        if stored.as_deref() != Some(detemplate(&text, &vars).as_str()) {
            rep.push(format!("~/{rel}: would capture"));
        }
    }
    if rep.is_empty() {
        rep.push("all files unchanged".into());
    }
    Ok(rep)
}

fn diff() -> Result<()> {
    let (root, vars) = (root()?, vars()?);
    let home = crate::cmd::home()?;
    for rel in manifest(&root)? {
        let want = render(&root, &rel, &vars)?;
        let live = home.join(&rel);
        match fs::read_to_string(&live) {
            Ok(t) if t == want => println!("same     ~/{rel}"),
            Ok(_) => {
                println!("differs  ~/{rel}");
                let out = Cmd::new("diff")
                    .args(["-u", "--label", "live", "--label", "template"])
                    .arg(&live)
                    .arg("-")
                    .stdin(want)
                    .timeout(Duration::from_secs(30))
                    .output()?;
                print!("{}", out.stdout);
            }
            Err(_) => println!("missing  ~/{rel}"),
        }
    }
    Ok(())
}

fn apply(dry_run: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let (root, vars) = (root()?, vars()?);
    let home = crate::cmd::home()?;
    for rel in manifest(&root)? {
        let want = render(&root, &rel, &vars)?;
        let live = home.join(&rel);
        let old = fs::read_to_string(&live).ok();
        if old.as_deref() == Some(want.as_str()) {
            continue;
        }
        println!(
            "{} ~/{rel}",
            match (dry_run, old.is_some()) {
                (true, _) => "would write",
                (false, true) => "write (old copy: .valet-bak)",
                (false, false) => "create",
            }
        );
        if dry_run {
            continue;
        }
        let mode = match fs::metadata(&live) {
            Ok(m) => m.permissions().mode() & 0o777,
            Err(_) if rel.starts_with(".ssh/") => 0o600,
            Err(_) => 0o644,
        };
        let dir = live.parent().context("no parent")?;
        fs::create_dir_all(dir)?;
        if old.is_some() {
            let mut bak = live.clone().into_os_string();
            bak.push(".valet-bak");
            fs::copy(&live, &bak)?;
        }
        let tmp = dir.join(format!(".valet-home.{}", std::process::id()));
        fs::write(&tmp, &want)?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
        fs::rename(&tmp, &live)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detemplate_round_trips_through_the_renderer() {
        let d = std::env::temp_dir().join(format!("valet-home-{}", std::process::id()));
        fs::create_dir_all(d.join("files")).unwrap();
        let mut v = Vars::new();
        v.insert("home", "/home/u".into());
        v.insert("ws", "/data/ws".into());
        for text in [
            "Include /home/u/.ssh/extra\nIdentityFile /data/ws/k\n",
            "a {{ b }} {% c %} {# d #} {{{ e\n\n",
            "no newline /home/u",
            "",
        ] {
            let t = detemplate(text, &v);
            assert!(!t.contains("/home/u"), "{t}");
            fs::write(d.join("files/x"), &t).unwrap();
            assert_eq!(render(&d, "x", &v).unwrap(), text);
        }
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn manifest_rejects_escaping_paths() {
        let d = std::env::temp_dir().join(format!("valet-home-m-{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("files.txt"), "# c\n.ssh/config\n\n.gitconfig\n").unwrap();
        assert_eq!(manifest(&d).unwrap(), [".ssh/config", ".gitconfig"]);
        for bad in ["/etc/x", "../x", "a//b"] {
            fs::write(d.join("files.txt"), bad).unwrap();
            assert!(manifest(&d).is_err(), "{bad}");
        }
        fs::remove_dir_all(&d).unwrap();
    }
}
