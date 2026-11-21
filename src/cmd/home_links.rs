//! `[home] links` (folders) and `[home] files` in config.toml: dot-paths in
//! `$HOME` that tools hard-code (instead of honouring XDG_* or a variable of
//! their own) become links into Userland, so their files still land on the
//! fast disk in a named place. `valet hooks install` applies them.
//!
//! For each link: a folder already there (real, or the target of an older
//! link) is merged into the target (newer files win, sockets skipped), then
//! moved to `<ws>/.Trash/<date>/home-links/`; a file is moved into place when
//! the target has none. Placeholders: `{userland}`.

use crate::proc::Cmd;
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

const RSYNC_TIMEOUT: Duration = Duration::from_secs(1800);

#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    links: BTreeMap<String, String>,
    #[serde(default)]
    files: BTreeMap<String, String>,
}

fn parse(text: &str) -> Result<Config> {
    let c: Config = crate::config::section_of(text, "home")?;
    for (name, target) in c.links.iter().chain(&c.files) {
        if c.links.contains_key(name) && c.files.contains_key(name) {
            bail!("config.toml [home]: {name} is in both links and files");
        }
        let p = Path::new(name);
        let ok = !name.is_empty()
            && p.components().all(|c| matches!(c, Component::Normal(_)))
            && name.starts_with('.');
        if !ok {
            bail!("config.toml [home] links: {name:?} must be a relative dot-path in $HOME");
        }
        if !target.starts_with("{userland}/") {
            bail!("config.toml [home] links: {name} target must start with {{userland}}/");
        }
    }
    Ok(c)
}

/// Check `[home]` of config TEXT.
pub fn validate(text: &str) -> Result<()> {
    parse(text).map(drop)
}

/// (link path, target path, target is a file) for every entry.
fn links(text: &str, home: &Path, userland: &Path) -> Result<Vec<(PathBuf, PathBuf, bool)>> {
    let c = parse(text)?;
    let dirs = c.links.into_iter().map(|e| (e, false));
    let files = c.files.into_iter().map(|e| (e, true));
    Ok(dirs
        .chain(files)
        .map(|((name, t), file)| {
            let t = t.replace("{userland}", &userland.display().to_string());
            (home.join(name), PathBuf::from(t), file)
        })
        .collect())
}

/// What `install` does to one entry.
#[derive(Debug, PartialEq)]
enum Step {
    Ok,
    /// Nothing at the link path: make the target and the link.
    Link,
    /// A folder (real, or behind an older link) to merge first.
    MergeDir(PathBuf),
    /// A file to move to the (missing) target.
    MoveFile,
    /// Both the old file and the target exist: left alone.
    Conflict(String),
}

fn plan(link: &Path, target: &Path) -> Step {
    let meta = std::fs::symlink_metadata(link);
    let Ok(m) = meta else { return Step::Link };
    if m.file_type().is_symlink() {
        let now = std::fs::read_link(link).unwrap_or_default();
        if now == crate::cmd::apps::link_value(target, link) {
            return Step::Ok;
        }
        // The same folder under another spelling (an automount path and the direct one, an
        // absolute link that is now stored relative) only needs relinking:
        // compare by device and inode, never by name.
        use std::os::unix::fs::MetadataExt;
        let same = |a: &Path, b: &Path| match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
            _ => false,
        };
        let resolved = link.canonicalize().ok();
        return match resolved {
            Some(r) if r.is_dir() && !same(&r, target) => Step::MergeDir(r),
            _ => Step::Link,
        };
    }
    if m.is_dir() {
        return Step::MergeDir(link.to_owned());
    }
    if target.exists() {
        Step::Conflict(format!(
            "{} and {} both exist",
            link.display(),
            target.display()
        ))
    } else {
        Step::MoveFile
    }
}

fn trash_dir(userland: &Path) -> Result<PathBuf> {
    let date = Cmd::new("date").arg("+%F").checked()?;
    Ok(userland
        .parent()
        .context("Userland has no parent")?
        .join(".Trash")
        .join(date.trim())
        .join("home-links"))
}

/// Apply every link (`dry`: print the plan only).
pub fn install(dry: bool) -> Result<()> {
    let home = crate::cmd::home()?;
    let userland = crate::cmd::apps::root()?;
    let say = |m: String| println!("{}{m}", if dry { "would: " } else { "" });
    for (link, target, file) in links(crate::config::text()?, &home, &userland)? {
        let step = plan(&link, &target);
        if step == Step::Ok {
            continue;
        }
        if let Step::Conflict(why) = &step {
            crate::warn!("home link {}: {why}; left alone", link.display());
            continue;
        }
        say(format!("link {} -> {}", link.display(), target.display()));
        if dry {
            continue;
        }
        let trash = trash_dir(&userland)?;
        std::fs::create_dir_all(&trash)?;
        let name = link
            .strip_prefix(&home)
            .unwrap_or(&link)
            .to_string_lossy()
            .replace('/', "_");
        match &step {
            Step::MergeDir(src) => {
                std::fs::create_dir_all(&target)?;
                Cmd::new("rsync")
                    .args(["-a", "--update", "--no-specials", "--no-devices"])
                    .arg(format!("{}/", src.display()))
                    .arg(format!("{}/", target.display()))
                    .timeout(RSYNC_TIMEOUT)
                    .checked()
                    .with_context(|| {
                        format!("merge {} into {}", src.display(), target.display())
                    })?;
                // The old folder (and an older link to it) go to the trash.
                if src != &link {
                    std::fs::rename(src, trash.join(format!("{name}.old-target"))).or_else(
                        |_| move_cross_fs(src, &trash.join(format!("{name}.old-target"))),
                    )?;
                    std::fs::remove_file(&link)?;
                } else {
                    move_cross_fs(&link, &trash.join(&name))?;
                }
            }
            Step::MoveFile => {
                if let Some(d) = target.parent() {
                    std::fs::create_dir_all(d)?;
                }
                move_cross_fs(&link, &target)?;
            }
            Step::Link => {
                // A file target is left for the tool to create through the link.
                if file {
                    if let Some(d) = target.parent() {
                        std::fs::create_dir_all(d)?;
                    }
                } else {
                    std::fs::create_dir_all(&target)?;
                }
                if std::fs::symlink_metadata(&link).is_ok() {
                    std::fs::remove_file(&link)?;
                }
            }
            Step::Ok | Step::Conflict(_) => unreachable!(),
        }
        if let Some(d) = link.parent() {
            std::fs::create_dir_all(d)?;
        }
        // Relative, through the home dir's link to Userland: every host
        // reads the home dir and resolves it to its own spelling.
        let value = crate::cmd::apps::link_value(&target, &link);
        std::os::unix::fs::symlink(&value, &link)
            .with_context(|| format!("link {} -> {}", link.display(), value.display()))?;
    }
    Ok(())
}

/// `mv` (works across filesystems, unlike rename).
fn move_cross_fs(from: &Path, to: &Path) -> Result<()> {
    Cmd::new("mv")
        .arg("-n")
        .arg("--")
        .arg(from)
        .arg(to)
        .timeout(RSYNC_TIMEOUT)
        .checked()
        .with_context(|| format!("move {} to {}", from.display(), to.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_are_validated() {
        assert!(parse("[home.links]\n\".cache\" = \"{userland}/Library/Caches\"\n").is_ok());
        for bad in [
            "[home.links]\n\"cache\" = \"{userland}/x\"\n",
            "[home.links]\n\".a/../b\" = \"{userland}/x\"\n",
            "[home.links]\n\"/etc\" = \"{userland}/x\"\n",
            "[home.links]\n\".cache\" = \"/tmp/x\"\n",
            "[home.links]\n\".a\" = \"{userland}/x\"\n[home.files]\n\".a\" = \"{userland}/y\"\n",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn plans_each_case() {
        let d = std::env::temp_dir().join(format!("valet-test-{}-homelinks", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("t/a")).unwrap();
        std::fs::create_dir_all(d.join("realdir")).unwrap();
        std::fs::create_dir_all(d.join("old")).unwrap();
        std::fs::write(d.join("file"), "x").unwrap();
        std::os::unix::fs::symlink(d.join("t/a"), d.join("good")).unwrap();
        std::os::unix::fs::symlink(d.join("old"), d.join("stale")).unwrap();
        let t = d.join("t/a");
        assert_eq!(plan(&d.join("good"), &t), Step::Ok);
        assert_eq!(plan(&d.join("none"), &t), Step::Link);
        assert_eq!(
            plan(&d.join("realdir"), &t),
            Step::MergeDir(d.join("realdir"))
        );
        assert_eq!(
            plan(&d.join("stale"), &t),
            Step::MergeDir(d.join("old").canonicalize().unwrap())
        );
        // The same folder by another name: relinked, never merged into itself.
        std::os::unix::fs::symlink(d.join("t"), d.join("alias")).unwrap();
        std::os::unix::fs::symlink(d.join("alias/a"), d.join("respelled")).unwrap();
        assert_eq!(plan(&d.join("respelled"), &t), Step::Link);
        assert_eq!(plan(&d.join("file"), &d.join("t/missing")), Step::MoveFile);
        assert!(matches!(plan(&d.join("file"), &t), Step::Conflict(_)));
        let _ = std::fs::remove_dir_all(&d);
    }
}
