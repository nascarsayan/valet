//! systemd user units: the user manager's unit dir, install-on-change, `systemctl --user`.

use crate::proc::Cmd;
use anyhow::Result;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

pub(crate) fn user_systemctl(a: &[&str]) -> Result<()> {
    Cmd::new("systemctl")
        .arg("--user")
        .args(a)
        .timeout(Duration::from_secs(60))
        .run()
}

/// Where the user manager looks, which may differ from this shell's XDG_CONFIG_HOME.
pub(crate) fn user_unit_dir() -> Result<PathBuf> {
    // The manager's own search path: its environment (show-environment) can
    // be changed after it started, and the search path does not follow.
    let unit_path = Cmd::new("systemctl")
        .args(["--user", "show", "-p", "UnitPath", "--value"])
        .timeout(Duration::from_secs(60))
        .checked()?;
    let home = crate::cmd::home()?;
    Ok(pick_unit_dir(&unit_path, &home).unwrap_or_else(|| home.join(".config/systemd/user")))
}

/// The first per-user config entry (`<under home>/systemd/user`) of UnitPath.
fn pick_unit_dir(unit_path: &str, home: &std::path::Path) -> Option<PathBuf> {
    unit_path
        .split_whitespace()
        .map(PathBuf::from)
        .find(|p| p.starts_with(home) && p.ends_with("systemd/user"))
}

/// Write a user unit when its text changed and reload; `restart` restarts it
/// on change. Returns whether it changed.
pub(crate) fn install_user_unit(name: &str, text: &str, restart: bool) -> Result<bool> {
    let path = user_unit_dir()?.join(name);
    if !changed(&path, text) {
        return Ok(false);
    }
    println!("update {}", path.display());
    fs::create_dir_all(path.parent().expect("unit dir"))?;
    fs::write(&path, text)?;
    user_systemctl(&["daemon-reload"])?;
    if restart {
        user_systemctl(&["restart", name])?;
    }
    Ok(true)
}

fn changed(path: &std::path::Path, text: &str) -> bool {
    fs::read_to_string(path).ok().as_deref() != Some(text)
}

#[cfg(test)]
mod tests {
    #[test]
    fn unit_dir_is_the_users_config_entry_of_unit_path() {
        let p = "/h/.config/systemd/user.control /run/user/1/systemd/user.control \
                 /run/user/1/systemd/transient /h/.config/systemd/user /etc/systemd/user \
                 /run/user/1/systemd/user";
        assert_eq!(
            super::pick_unit_dir(p, std::path::Path::new("/h")),
            Some(std::path::PathBuf::from("/h/.config/systemd/user"))
        );
        assert_eq!(
            super::pick_unit_dir("/etc/systemd/user", std::path::Path::new("/h")),
            None
        );
    }
}
