//! External CLI tools (fzf, gh, jq, ...): found on PATH, or installed on demand.
//!
//! `valet` never bundles these. [`resolve`] looks on PATH first, then in the
//! managed bin dir, and only then downloads the pinned release below, checks
//! its SHA-256, and installs it into the managed dir. `valet shell-init` appends
//! the managed dir to PATH, so installed tools work outside `valet` too.
//!
//! Bootstrap prerequisites on a vanilla Linux box: curl, sha256sum, tar
//! (unzip only for zip releases).

use crate::proc;
use anyhow::{Context, Result, bail};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Overrides the managed bin dir.
pub const DIR_ENV: &str = "VALET_TOOLS_DIR";
/// Set to 1 to fail instead of downloading a missing tool.
pub const NO_INSTALL_ENV: &str = "VALET_NO_AUTO_INSTALL";

const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);

pub struct Tool {
    pub name: &'static str,
    pub version: &'static str,
    /// Per-architecture release asset.
    assets: &'static [Asset],
}

struct Asset {
    arch: &'static str,
    url: &'static str,
    sha256: &'static str,
    /// Path of the binary inside the archive; `None` for a bare binary.
    member: Option<&'static str>,
}

/// Pinned releases. Digests are GitHub's release-asset `digest` values.
/// To bump: update version/URLs, take digests from
/// `gh api repos/<owner>/<repo>/releases/tags/<tag> --jq '.assets[]|[.name,.digest]'`,
/// then run `valet tools install --force <name>` to verify.
pub const TOOLS: &[Tool] = &[
    Tool {
        name: "fzf",
        version: "0.75.0",
        assets: &[
            Asset {
                arch: "x86_64",
                url: "https://github.com/junegunn/fzf/releases/download/v0.75.0/fzf-0.75.0-linux_amd64.tar.gz",
                sha256: "0d6640f83582d991cf429754baad64b07e3a4e439a6019670f4b91da1314141c",
                member: Some("fzf"),
            },
            Asset {
                arch: "aarch64",
                url: "https://github.com/junegunn/fzf/releases/download/v0.75.0/fzf-0.75.0-linux_arm64.tar.gz",
                sha256: "bd0717badb6a5620387e014bb94209aac4d9eab6ab77b5348eabb0833aeb4d59",
                member: Some("fzf"),
            },
        ],
    },
    Tool {
        name: "gh",
        version: "2.103.3",
        assets: &[
            Asset {
                arch: "x86_64",
                url: "https://github.com/cli/cli/releases/download/v2.103.3/gh_2.103.3_linux_amd64.tar.gz",
                sha256: "4c806978ffce1cec70853302a56d70ac6da4f19bae7adca4e1a8ab00633dd91d",
                member: Some("gh_2.103.3_linux_amd64/bin/gh"),
            },
            Asset {
                arch: "aarch64",
                url: "https://github.com/cli/cli/releases/download/v2.103.3/gh_2.103.3_linux_arm64.tar.gz",
                sha256: "ee32df2323c8847f5fac6cb6b4b69ca75828e9329a8f22f677021f7b3306ce63",
                member: Some("gh_2.103.3_linux_arm64/bin/gh"),
            },
        ],
    },
    Tool {
        name: "jq",
        version: "1.8.2",
        assets: &[
            Asset {
                arch: "x86_64",
                url: "https://github.com/jqlang/jq/releases/download/jq-1.8.2/jq-linux-amd64",
                sha256: "b1c22172dd303f3be49e935aa56aa48a8b7a46e0bc838b4997d3bb451495870f",
                member: None,
            },
            Asset {
                arch: "aarch64",
                url: "https://github.com/jqlang/jq/releases/download/jq-1.8.2/jq-linux-arm64",
                sha256: "8b85c817833814ddca00a144c33705546355afccf0cf39b188f3cdb48b852309",
                member: None,
            },
        ],
    },
    Tool {
        name: "yq",
        version: "4.54.3",
        assets: &[
            Asset {
                arch: "x86_64",
                url: "https://github.com/mikefarah/yq/releases/download/v4.54.3/yq_linux_amd64",
                sha256: "129534601871538efe0ba4c5e62a34fb1d5f469bcb975c4fddeb467d82974973",
                member: None,
            },
            Asset {
                arch: "aarch64",
                url: "https://github.com/mikefarah/yq/releases/download/v4.54.3/yq_linux_arm64",
                sha256: "89f69310b61b68d1245065a1241d1fc843f7a36cd6103ffd77ad987613bd1b67",
                member: None,
            },
        ],
    },
];

/// Every pinned download: (name, version, release URL), one per tool.
pub fn pins() -> Vec<(&'static str, &'static str, &'static str)> {
    TOOLS
        .iter()
        .filter_map(|t| Some((t.name, t.version, t.assets.first()?.url)))
        .collect()
}

/// `owner/repo` and tag of a GitHub release download URL.
pub fn release_of(url: &str) -> Option<(&str, &str)> {
    const MID: &str = "/releases/download/";
    let rest = url.strip_prefix("https://github.com/")?;
    let i = rest.find(MID)?;
    let tag = rest[i + MID.len()..].split('/').next()?;
    Some((&rest[..i], tag))
}

pub fn get(name: &str) -> Option<&'static Tool> {
    TOOLS.iter().find(|t| t.name == name)
}

/// Where `valet` installs tools: $VALET_TOOLS_DIR, else valet's `Data/Tools`.
pub fn managed_dir() -> Result<PathBuf> {
    if let Some(d) = std::env::var_os(DIR_ENV).filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(d));
    }
    Ok(crate::cmd::apps::self_dir("Data")?.join("Tools"))
}

pub enum Source {
    Path(PathBuf),
    Managed(PathBuf),
    Missing,
}

/// Where `name` currently comes from, without installing anything.
pub fn locate(name: &str) -> Result<Source> {
    if let Some(p) = proc::which(name) {
        return Ok(Source::Path(p));
    }
    let managed = managed_dir()?.join(name);
    Ok(if managed.is_file() {
        Source::Managed(managed)
    } else {
        Source::Missing
    })
}

/// Path to an executable `name`, installing the pinned release if needed.
/// Unknown (unmanaged) tools must already be on PATH.
pub fn resolve(name: &str) -> Result<PathBuf> {
    match locate(name)? {
        Source::Path(p) | Source::Managed(p) => Ok(p),
        Source::Missing => {
            let tool = get(name).with_context(|| format!("{name} not found on PATH"))?;
            if std::env::var(NO_INSTALL_ENV).is_ok_and(|v| v == "1") {
                bail!(
                    "{name} not found on PATH and {NO_INSTALL_ENV}=1; run `valet tools install {name}`"
                );
            }
            eprintln!(
                "valet: {name} not found; installing {name} {}",
                tool.version
            );
            install(tool)
        }
    }
}

/// Download, verify, and install `tool` into the managed dir; returns its path.
pub fn install(tool: &Tool) -> Result<PathBuf> {
    let arch = std::env::consts::ARCH;
    if std::env::consts::OS != "linux" {
        bail!(
            "auto-install supports Linux only; install {} yourself",
            tool.name
        );
    }
    let asset = tool
        .assets
        .iter()
        .find(|a| a.arch == arch)
        .with_context(|| format!("no pinned {} release for {arch}", tool.name))?;

    let dir = managed_dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    // Stage next to the target so the final rename is atomic (same filesystem).
    let stage = dir.join(format!(".stage-{}-{}", tool.name, std::process::id()));
    let _ = fs::remove_dir_all(&stage);
    fs::create_dir(&stage)?;
    let result = install_staged(tool, asset, &stage, &dir);
    let _ = fs::remove_dir_all(&stage);
    result
}

fn install_staged(tool: &Tool, asset: &Asset, stage: &Path, dir: &Path) -> Result<PathBuf> {
    let download = stage.join("download");
    fetch_verified(asset.url, asset.sha256, &download)?;
    let binary = match asset.member {
        None => download,
        Some(member) => extract_member(&download, asset.url, member, &stage.join("x"))?,
    };
    // Only the one named member is extracted; still refuse anything but a file.
    if !fs::symlink_metadata(&binary)?.is_file() {
        bail!("{} in {} is not a regular file", tool.name, asset.url);
    }
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))?;
    let target = dir.join(tool.name);
    fs::rename(&binary, &target).with_context(|| format!("install {}", target.display()))?;
    eprintln!(
        "valet: installed {} {} -> {}",
        tool.name,
        tool.version,
        target.display()
    );
    Ok(target)
}

/// Download `url` (https only) to `dest` and fail unless its SHA-256 is `sha256`.
pub fn fetch_verified(url: &str, sha256: &str, dest: &Path) -> Result<()> {
    let dest_s = dest.to_str().context("non-UTF-8 path")?;
    proc::output(
        "curl",
        &[
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--tlsv1.2",
            "--max-time",
            "240",
            "--output",
            dest_s,
            url,
        ],
        DOWNLOAD_TIMEOUT,
    )
    .with_context(|| format!("download {url}"))?;
    verify_sha256(dest, sha256).with_context(|| format!("verify {url}"))
}

/// Extract the one file `member` of `archive` (a zip when `url` ends in
/// `.zip`, else a tarball) into the new dir `out`; returns its path.
pub fn extract_member(archive: &Path, url: &str, member: &str, out: &Path) -> Result<PathBuf> {
    fs::create_dir(out)?;
    let archive_s = archive.to_str().context("non-UTF-8 path")?;
    let out_s = out.to_str().context("non-UTF-8 path")?;
    if url.ends_with(".zip") {
        proc::output(
            "unzip",
            &["-q", archive_s, member, "-d", out_s],
            DOWNLOAD_TIMEOUT,
        )?;
    } else {
        proc::output(
            "tar",
            &["-xf", archive_s, "-C", out_s, member],
            DOWNLOAD_TIMEOUT,
        )?;
    }
    Ok(out.join(member))
}

/// Fail unless the file's SHA-256 is `expected` (lowercase hex).
pub fn verify_sha256(file: &Path, expected: &str) -> Result<()> {
    let got = sha256_file(file)?;
    if got != expected {
        bail!("checksum mismatch: expected {expected}, got {got}; not installing");
    }
    Ok(())
}

/// SHA-256 of a file, lowercase hex.
pub fn sha256_file(file: &Path) -> Result<String> {
    let file_s = file.to_str().context("non-UTF-8 path")?;
    let sum = proc::output("sha256sum", &["--", file_s], DOWNLOAD_TIMEOUT)?;
    first_word(&sum)
}

fn first_word(sum: &str) -> Result<String> {
    let h = sum.split_whitespace().next().unwrap_or_default();
    if h.len() != 64 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("unexpected sha256sum output");
    }
    Ok(h.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_sha256_accepts_match_and_rejects_tampering() {
        let f = std::env::temp_dir().join(format!("valet-test-{}-sha", std::process::id()));
        fs::write(&f, "abc").unwrap();
        // SHA-256("abc"), FIPS 180-2 test vector.
        let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert!(verify_sha256(&f, abc).is_ok());
        fs::write(&f, "abd").unwrap();
        let err = verify_sha256(&f, abc).unwrap_err();
        assert!(format!("{err:#}").contains("checksum mismatch"), "{err:#}");
        let _ = fs::remove_file(&f);
    }

    #[test]
    fn every_tool_pins_both_arches_with_full_digests() {
        for t in TOOLS {
            for arch in ["x86_64", "aarch64"] {
                let a = t
                    .assets
                    .iter()
                    .find(|a| a.arch == arch)
                    .unwrap_or_else(|| panic!("{} {arch}", t.name));
                assert!(a.url.starts_with("https://github.com/"), "{}", a.url);
                assert!(a.url.contains(t.version), "{} url lacks version", t.name);
                assert_eq!(a.sha256.len(), 64, "{}", t.name);
                assert!(
                    a.sha256.chars().all(|c| c.is_ascii_hexdigit()),
                    "{}",
                    t.name
                );
            }
        }
    }

    #[test]
    fn get_finds_known_tools_only() {
        assert!(get("fzf").is_some());
        assert!(get("not-a-tool").is_none());
    }
}
