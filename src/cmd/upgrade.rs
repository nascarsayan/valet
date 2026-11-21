//! `valet upgrade`: update, rebuild and deploy this binary from its source repo.

use crate::proc::Cmd;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Source checkout this binary was built from (the fallback for [`repo`]).
pub const REPO: &str = env!("CARGO_MANIFEST_DIR");

/// The source checkout: the one the live config.toml link points into (set
/// by the last upgrade), else the one this binary was built from.
pub fn repo() -> PathBuf {
    crate::config::source()
        .ok()
        .and_then(|f| f.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from(REPO))
}
/// `git pull` may fetch submodules or many objects.
const PULL_TIMEOUT: Duration = Duration::from_secs(120);

/// `git pull --ff-only` in the source repo (unless the tree is dirty or the
/// branch has no upstream), fmt/clippy/test, release build, then replace the
/// `valet` binary on PATH.
#[derive(clap::Args)]
pub struct Args {
    /// Source checkout to build (default: the one config.toml is linked
    /// from, else the one this binary was built from).
    #[arg(long)]
    repo: Option<PathBuf>,
    /// Skip `git pull --ff-only`.
    #[arg(long)]
    no_pull: bool,
    /// Skip fmt/clippy/test.
    #[arg(long)]
    no_check: bool,
}

/// Cargo's output dir: `$CARGO_TARGET_DIR` (the cd hook sets it inside
/// repos, see config.toml [env.repo]), else `<repo>/target`.
fn target_dir(repo: &Path) -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .filter(|v| !v.is_empty())
        .map_or_else(|| repo.join("target"), PathBuf::from)
}

pub fn run(a: Args) -> Result<()> {
    let repo = &a.repo.unwrap_or_else(repo);
    if !a.no_pull {
        pull(repo)?;
    }
    let cargo = |args: &[&str]| Cmd::new("cargo").args(args).cwd(repo).run();
    if !a.no_check {
        cargo(&["fmt", "--check"])?;
        cargo(&["clippy", "--all-targets", "--", "-D", "warnings"])?;
        cargo(&["test", "--quiet"])?;
    }
    cargo(&["build", "--release", "--locked"])?;
    let root = crate::cmd::apps::root()?;
    let here = crate::cmd::apps::this_arch();
    install(&target_dir(repo).join("release/valet"), &root, here)?;
    // From Amd64, also build the Arm64 binary (static musl, Rust's own linker):
    // the home dir is shared with ARM hosts.
    if here == "Amd64" {
        match cross_build(repo) {
            Ok(built) => install(&built, &root, "Arm64")?,
            Err(e) => crate::warn!("Arm64 build skipped: {e:#}"),
        }
    }
    // valet reads config.toml live through this link.
    let checkout = crate::cmd::apps::canonical(repo);
    if let Err(e) = crate::config::link(&checkout) {
        crate::warn!("cannot link config.toml: {e:#}");
    }
    // The new binary rewrites the shell config and caches (its shell-init and
    // completions may have changed).
    if let Err(e) = Cmd::new(crate::cmd::apps::valet_bin(&root, here))
        .args(["hooks", "install"])
        .timeout(Duration::from_secs(300))
        .run()
    {
        crate::warn!("`valet hooks install` after upgrade failed: {e:#}");
    }
    // The daemon's parts run the new binary.
    crate::cmd::daemon::after_upgrade();
    Ok(())
}

const ARM_TARGET: &str = "aarch64-unknown-linux-musl";

fn cross_build(repo: &Path) -> Result<PathBuf> {
    Cmd::new("rustup")
        .args(["target", "add", ARM_TARGET])
        .timeout(PULL_TIMEOUT)
        .checked()?;
    Cmd::new("cargo")
        .args(["build", "--release", "--locked", "--target", ARM_TARGET])
        .env("CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER", "rust-lld")
        .cwd(repo)
        .run()?;
    Ok(target_dir(repo).join(ARM_TARGET).join("release/valet"))
}

/// Fast-forward the checkout, or say in one line why not.
fn pull(repo: &Path) -> Result<()> {
    let git = |args: &[&str]| Cmd::new("git").args(args).cwd(repo);
    let dirty = git(&["status", "--porcelain"])
        .timeout(Duration::from_secs(30))
        .checked()?;
    let upstream = git(&[
        "rev-parse",
        "--abbrev-ref",
        "--symbolic-full-name",
        "@{upstream}",
    ])
    .timeout(Duration::from_secs(10))
    .output()?
    .status
    .success();
    if let Some(why) = skip_pull(&dirty, upstream) {
        println!("not pulling: {why}");
        return Ok(());
    }
    git(&["pull", "--ff-only"])
        .timeout(PULL_TIMEOUT)
        .run()
        .with_context(|| format!("git pull --ff-only in {} failed", repo.display()))
}

/// Why the pull is skipped, if it is: dirty tree or no upstream.
fn skip_pull(porcelain: &str, has_upstream: bool) -> Option<&'static str> {
    if !porcelain.trim().is_empty() {
        Some("working tree has uncommitted changes")
    } else if !has_upstream {
        Some("branch has no upstream")
    } else {
        None
    }
}

/// Put `built` at the Valet app's binary for `arch` (copy beside, then rename:
/// a running binary keeps its inode) and link it into Commands/<arch>.
fn install(built: &Path, root: &Path, arch: &str) -> Result<()> {
    let exe = crate::cmd::apps::valet_bin(root, arch);
    std::fs::create_dir_all(exe.parent().unwrap_or(root))?;
    let tmp = exe.with_extension("valet-new");
    std::fs::copy(built, &tmp).with_context(|| format!("copy {}", built.display()))?;
    std::fs::rename(&tmp, &exe).with_context(|| format!("replace {}", exe.display()))?;
    crate::cmd::apps::link_self(root, arch)?;
    println!("installed {}", exe.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct T {
        #[command(flatten)]
        a: Args,
    }

    #[test]
    fn pull_is_skipped_on_dirty_tree_or_missing_upstream() {
        assert_eq!(skip_pull("", true), None);
        assert_eq!(
            skip_pull(" M src/main.rs\n", true),
            Some("working tree has uncommitted changes")
        );
        assert_eq!(skip_pull("", false), Some("branch has no upstream"));
    }

    #[test]
    fn flags() {
        let t = T::try_parse_from(["upgrade", "--no-pull", "--no-check"]).unwrap();
        assert!(t.a.no_pull && t.a.no_check);
        assert_eq!(t.a.repo, None);
        let t = T::try_parse_from(["upgrade", "--repo", "/src/valet"]).unwrap();
        assert_eq!(t.a.repo.as_deref(), Some(Path::new("/src/valet")));
        assert!(T::try_parse_from(["upgrade", "install"]).is_err());
    }
}
