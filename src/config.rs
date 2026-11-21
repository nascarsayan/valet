//! valet's one configuration file, `config.toml` in the valet repo.
//!
//! Read live through `<Userland>/Applications/<App>/Settings/config.toml`, a
//! link to the checkout's file that `valet upgrade` makes; the copy baked in at
//! build time is used only while that link is missing (a fresh machine).
//! Each module reads its own section ([`section`], [`keys_of`]).

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The build-time copy.
pub const EMBEDDED: &str = include_str!("../config.toml");

/// Where valet reads the file: `<Userland>/Applications/<App>/Settings/config.toml`.
pub fn path() -> Result<PathBuf> {
    let root = crate::cmd::apps::root()?;
    Ok(crate::cmd::apps::self_app(&root)
        .join("Settings")
        .join("config.toml"))
}

/// The file's text: the live file when present, else [`EMBEDDED`].
pub fn text() -> Result<&'static str> {
    static TEXT: OnceLock<String> = OnceLock::new();
    if let Some(t) = TEXT.get() {
        return Ok(t);
    }
    let p = path()?;
    let t = match std::fs::read_to_string(&p) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => EMBEDDED.to_owned(),
        Err(e) => return Err(e).with_context(|| format!("read {}", p.display())),
    };
    Ok(TEXT.get_or_init(|| t))
}

fn table(text: &str) -> Result<toml::Table> {
    toml::from_str(text).context("parse config.toml")
}

/// Table KEY of TEXT (empty when absent) as T.
pub fn section_of<T: DeserializeOwned>(text: &str, key: &str) -> Result<T> {
    let v = table(text)?
        .remove(key)
        .unwrap_or_else(|| toml::Value::Table(toml::Table::new()));
    v.try_into()
        .with_context(|| format!("config.toml: [{key}]"))
}

/// Table KEY of the live file as T.
pub fn section<T: DeserializeOwned>(key: &str) -> Result<T> {
    section_of(text()?, key)
}

/// The top-level KEYS of TEXT, as one table, as T.
pub fn keys_of<T: DeserializeOwned>(text: &str, keys: &[&str]) -> Result<T> {
    let mut t = table(text)?;
    t.retain(|k, _| keys.contains(&k));
    toml::Value::Table(t)
        .try_into()
        .with_context(|| format!("config.toml: {}", keys.join(", ")))
}

/// The checkout file the live link points at (for writes: a rename through
/// the link would replace it).
pub fn source() -> Result<PathBuf> {
    let p = path()?;
    p.canonicalize()
        .with_context(|| format!("{} is missing (`valet upgrade` links it)", p.display()))?;
    Ok(crate::cmd::apps::canonical(&p))
}

/// Point the live link at CHECKOUT's `config.toml`.
pub fn link(checkout: &Path) -> Result<()> {
    let target = checkout.join("config.toml");
    if !target.is_file() {
        anyhow::bail!("{} is missing", target.display());
    }
    let p = path()?;
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    // Relative within Userland: every host reads the link.
    crate::cmd::apps::symlink(&target, &p)
}

/// Parse every section the way its module does.
pub fn check(text: &str) -> Result<()> {
    crate::repos::parse_manifest(text)?;
    crate::cmd::envrules::validate(text)?;
    crate::cmd::skills::parse(text)?;
    crate::cmd::apps::validate(text)?;
    crate::cmd::gitignore::validate(text)?;
    crate::cmd::home_links::validate(text)?;
    crate::cmd::githooks::validate_paths(text)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn embedded_config_checks() {
        super::check(super::EMBEDDED).expect("config.toml checks");
    }
}
