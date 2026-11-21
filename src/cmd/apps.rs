//! `valet apps`: Userland, one home for every custom tool, per CPU architecture.
//!
//! ```text
//! ~/ws/Userland/
//!   Applications/<App>/   Binaries/<Arch>/ Settings/ Data/ Caches/ State/ Logs/
//!   Commands/<Arch>/      generated links and launchers: the PATH dir for tools
//!   Scripts/              architecture-independent scripts
//! ```
//!
//! `[apps.*]` in config.toml are the manifest. Binaries come from
//! mise (versions pinned, checksums in mise.lock), installed into each app's
//! own `Binaries/<Arch>/` (mise's data dir for that app alone; the download
//! cache is shared): an app is self-contained, and removing its folder
//! removes it. Folders other than Binaries exist only when used. The home dir is
//! shared between machines, so installing for another architecture runs mise
//! on a host of that architecture over ssh and links from here.

use crate::proc::Cmd as Proc;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The Userland root when this binary is not installed in one (a build in a
/// checkout); else `~/ws/Userland`.
const ROOT_ENV: &str = "USERLAND";
const ARCHES: &[&str] = &["Amd64", "Arm64"];
const INSTALL_TIMEOUT: Duration = Duration::from_secs(1800);
const QUERY_TIMEOUT: Duration = Duration::from_secs(120);
/// Folders every app gets, in placeholder form `{Settings}` etc.
const FOLDERS: &[&str] = &["Settings", "Data", "Caches", "State", "Logs"];

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    cmd: Sub,
}

#[derive(clap::Subcommand)]
enum Sub {
    /// Apps in the manifest and whether each command is linked for an architecture.
    List {
        /// Amd64 or Arm64 (default: this machine).
        #[arg(long)]
        arch: Option<String>,
    },
    /// Install the manifest's apps and link their commands.
    ///
    /// For another architecture, mise runs over ssh on the host named under
    /// [hosts] in config.toml; the home dir is shared, so links are made here.
    Install {
        /// Amd64, Arm64 or all (default: this machine).
        #[arg(long)]
        arch: Option<String>,
    },
    /// Relink Commands/<arch> from what is installed, without downloading:
    /// adds missing links, removes links of removed apps and dangling ones.
    Link {
        /// Amd64, Arm64 or all (default: this machine).
        #[arg(long)]
        arch: Option<String>,
    },
    /// Write the generated files (mise config, environment, launchers) without installing.
    Render,
    /// Move apps to their latest releases: rewrite `version` in config.toml,
    /// then install for every architecture and drop the old versions.
    Update {
        /// Apps to update (default: all).
        apps: Vec<String>,
        /// Show old -> new; change nothing.
        #[arg(short = 'n', long)]
        dry_run: bool,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    /// Architecture -> ssh host used to install for it.
    #[serde(default)]
    hosts: BTreeMap<String, String>,
    /// Passed through to mise's [settings].
    #[serde(default)]
    settings: toml::Table,
    apps: BTreeMap<String, App>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct App {
    /// mise tool id (`aqua:owner/repo`, `github:owner/repo`, `core:node`, ...).
    tool: Option<String>,
    version: Option<String>,
    /// Extra mise tool options (asset_pattern, bin_path, ...).
    #[serde(default)]
    options: toml::Table,
    /// Commands put on PATH: `name`, or `name=binary` to link under another name.
    #[serde(default)]
    commands: Vec<String>,
    /// Architectures the app exists for (default: all). Arch-only tools go to
    /// mise's `config.<Arch>.toml`, read when MISE_ENV=<Arch>.
    #[serde(default)]
    arches: Vec<String>,
    /// Exported in every shell; values may use {Settings} {Data} {Caches}
    /// {State} {Logs} {Arch} {App} {Host}. {Host} (`$USERLAND_HOST`) keeps
    /// SQLite state per host: hosts may share Userland over a network mount,
    /// and two hosts writing one SQLite file corrupt it.
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// Run through a launcher that points XDG_* at the app's folders, for tools
    /// with no variable of their own. Children inherit them (zsh resets them in
    /// its zshenv).
    #[serde(default)]
    xdg: bool,
    /// A binary the OS provides (no mise `tool`): its `commands` wrap it.
    system: Option<String>,
    /// Arguments every command passes first, for tools whose config path is
    /// only a flag (placeholders as in `env`), e.g. `["-f", "{Settings}/tmux.conf"]`.
    #[serde(default)]
    args: Vec<String>,
    /// A command (on PATH) that runs each command's script, for npm tools
    /// whose `#!/usr/bin/env node` the OS's node is too old for: `"bun"`.
    via: Option<String>,
    /// Files in the app folder that are links to files in the valet repo:
    /// `"Settings/tmux.conf" = "tmux/tmux.conf"`.
    #[serde(default)]
    files: BTreeMap<String, String>,
}

/// The Userland root, computed from where this binary is installed:
/// `<root>/Applications/<App>/Binaries/<Arch>/<name>`, in this host's
/// [`canonical`] spelling. Moving the whole folder (or naming it differently)
/// needs no setting; rerun `hooks install` and `apps render` to regenerate
/// the files that name it. A binary outside that layout (a build in a
/// checkout) uses `$USERLAND`, else `~/ws/Userland`.
pub fn root() -> Result<PathBuf> {
    static ROOT: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    if let Some(r) = ROOT.get_or_init(|| {
        let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
        installed_root(&exe).map(|r| canonical(&r))
    }) {
        return Ok(r.clone());
    }
    let r = match std::env::var_os(ROOT_ENV).filter(|v| !v.is_empty()) {
        Some(r) => PathBuf::from(r),
        None => crate::cmd::home()?.join("ws/Userland"),
    };
    Ok(canonical(&r))
}

/// `<Userland>/Scratch`: throw-away files of any task, one folder per task;
/// the daily clean job deletes an entry [`SCRATCH_DAYS`] after its last change.
pub fn scratch() -> Result<PathBuf> {
    Ok(root()?.join("Scratch"))
}

/// Days an entry of [`scratch`] lives after its last change.
pub const SCRATCH_DAYS: u32 = 2;

/// This machine's short host name (`/proc/sys/kernel/hostname` up to the first dot).
pub fn host() -> String {
    let h = fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
    h.trim().split('.').next().unwrap_or_default().to_owned()
}

/// The one spelling of PATH: symlinks resolved. A path that does not exist
/// yet keeps its missing tail after its nearest existing ancestor's
/// spelling. (`_host` is unused; kept until the callers drop it.)
pub fn canonical(path: &Path) -> PathBuf {
    path.ancestors()
        .find_map(|a| {
            let r = a.canonicalize().ok()?;
            let rest = path.strip_prefix(a).ok()?;
            Some(if rest.as_os_str().is_empty() {
                r
            } else {
                r.join(rest)
            })
        })
        .unwrap_or_else(|| path.to_owned())
}

/// Directory symlinks directly in HOME and their [`canonical`] targets.
pub fn home_dir_links(home: &Path) -> Vec<(PathBuf, PathBuf)> {
    let mut v = Vec::new();
    for e in fs::read_dir(home).into_iter().flatten().flatten() {
        if !e.file_type().is_ok_and(|t| t.is_symlink()) {
            continue;
        }
        let link = e.path();
        let t = canonical(&link);
        if t != link && t.is_dir() {
            v.push((link, t));
        }
    }
    v
}

/// PATH spelled through a directory symlink in `$HOME` when one leads there
/// (`<home>/ws/Userland/X`), else PATH. For files every host reads (the home
/// dir's rc files, git's global config, links in the home dir): the link
/// resolves on each host to that host's spelling.
pub fn portable(path: &Path) -> PathBuf {
    let Ok(home) = crate::cmd::home() else {
        return path.to_path_buf();
    };
    // The link to the highest folder (~/ws, not ~/.config into it): a link
    // never spells itself.
    home_dir_links(&home)
        .into_iter()
        .filter(|(_, t)| path.starts_with(t))
        .min_by_key(|(_, t)| t.as_os_str().len())
        .map(|(link, t)| match path.strip_prefix(&t) {
            Ok(rest) if !rest.as_os_str().is_empty() => link.join(rest),
            _ => link,
        })
        .unwrap_or_else(|| path.to_path_buf())
}

/// [`portable`] as `~/...` when it is under `$HOME` (git expands `~/` in
/// path settings).
pub fn portable_tilde(path: &Path) -> String {
    let p = portable(path);
    let home = crate::cmd::home().unwrap_or_default();
    match p.strip_prefix(&home) {
        Ok(rest) if !home.as_os_str().is_empty() => format!("~/{}", rest.display()),
        _ => p.display().to_string(),
    }
}

/// TARGET relative to directory BASE, by name (both absolute, no `..`).
pub fn relative(target: &Path, base: &Path) -> PathBuf {
    let t: Vec<_> = target.components().collect();
    let b: Vec<_> = base.components().collect();
    let common = t.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut out = PathBuf::new();
    for _ in common..b.len() {
        out.push("..");
    }
    for c in &t[common..] {
        out.push(c);
    }
    out
}

/// What a symlink at LINK pointing to TARGET stores. A target in Userland
/// gets a relative value, which every host resolves to its own spelling:
/// from a link in Userland directly, from a link in the home dir through the
/// home dir's link to Userland ([`portable`]). Anything else stays as given.
pub fn link_value(target: &Path, link: &Path) -> PathBuf {
    let (Ok(root), Some(parent)) = (root(), link.parent()) else {
        return target.to_path_buf();
    };
    let dir = canonical(parent);
    // The target's own name stays: it may be a link whose name matters.
    let t = match (target.parent(), target.file_name()) {
        (Some(p), Some(n)) => canonical(p).join(n),
        _ => target.to_path_buf(),
    };
    if !t.starts_with(&root) {
        return target.to_path_buf();
    }
    if dir.starts_with(&root) {
        return relative(&t, &dir);
    }
    let p = portable(&t);
    match crate::cmd::home() {
        Ok(h) if p != t && dir.starts_with(&h) && p.starts_with(&h) => relative(&p, &dir),
        _ => target.to_path_buf(),
    }
}

/// LINK is a symlink that stores [`link_value`] for TARGET.
pub fn link_is(link: &Path, target: &Path) -> bool {
    fs::read_link(link).is_ok_and(|t| t == link_value(target, link))
}

/// Make (or replace) the symlink LINK -> TARGET, stored as [`link_value`].
pub fn symlink(target: &Path, link: &Path) -> Result<()> {
    if link_is(link, target) {
        return Ok(());
    }
    crate::generated::remove(link)?;
    let v = link_value(target, link);
    std::os::unix::fs::symlink(&v, link)
        .with_context(|| format!("link {} -> {}", link.display(), v.display()))
}

/// `<root>` of `<root>/Applications/<App>/Binaries/<Arch>/<binary>`.
fn installed_root(exe: &Path) -> Option<PathBuf> {
    let arch = exe.parent()?;
    let bins = arch.parent()?;
    let app = bins.parent()?;
    let apps = app.parent()?;
    (ARCHES
        .iter()
        .any(|a| arch.file_name().is_some_and(|n| n == *a))
        && bins.file_name()? == "Binaries"
        && apps.file_name()? == "Applications")
        .then(|| apps.parent().map(Path::to_path_buf))
        .flatten()
}

/// This machine's architecture folder name.
pub fn this_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "Arm64",
        _ => "Amd64",
    }
}

/// Check config.toml TEXT the way [`parse`] reads it.
pub fn validate(text: &str) -> Result<()> {
    let m = parse(text)?;
    for (name, app) in &m.apps {
        if app.tool.is_some() && app.system.is_some() {
            bail!("config.toml [apps.{name}]: `tool` or `system`, not both");
        }
        for (rel, src) in &app.files {
            for p in [rel, src] {
                let ok = !p.is_empty()
                    && Path::new(p)
                        .components()
                        .all(|c| matches!(c, std::path::Component::Normal(_)));
                if !ok {
                    bail!("config.toml [apps.{name}] files: {p:?} must be a relative path");
                }
            }
        }
    }
    Ok(())
}

/// The apps part of config.toml TEXT: `[hosts]`, `[settings]`, `[apps.*]`.
fn parse(text: &str) -> Result<Manifest> {
    crate::config::keys_of(text, &["hosts", "settings", "apps"])
}

fn load(_root: &Path) -> Result<Manifest> {
    parse(crate::config::text()?)
}

fn arches(arg: Option<&str>) -> Result<Vec<&'static str>> {
    match arg {
        None => Ok(vec![this_arch()]),
        Some("all") => Ok(ARCHES.to_vec()),
        Some(a) => ARCHES
            .iter()
            .find(|x| x.eq_ignore_ascii_case(a))
            .map(|x| vec![*x])
            .with_context(|| format!("unknown architecture {a} (Amd64, Arm64, all)")),
    }
}

/// APP's `env` from config.toml for this host (placeholders filled in), for
/// valet's own child processes: they must not depend on how fresh the calling
/// shell's environment is.
pub fn app_env(app: &str) -> Result<Vec<(String, String)>> {
    let root = root()?;
    let m = load(&root)?;
    let Some(a) = m.apps.get(app) else {
        return Ok(Vec::new());
    };
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
    Ok(a.env
        .iter()
        .map(|(k, v)| {
            let v = expand(v, &root, app, this_arch()).replace("${USERLAND_HOST}", host.trim());
            (k.clone(), v)
        })
        .collect())
}

/// Userland's root in generated shell text that every host reads: the
/// variable each shell sets at start to its own host's spelling.
const ROOT_VAR: &str = "${USERLAND}";

/// PATH as shell text: `${USERLAND}/...` when it is in Userland (in any
/// spelling), else PATH as given.
pub fn in_shell(path: &Path) -> String {
    let rel = root().ok().and_then(|r| {
        let under = |p: &Path| p.strip_prefix(&r).ok().map(Path::to_path_buf);
        under(path).or_else(|| under(&canonical(path)))
    });
    match rel {
        Some(rest) if rest.as_os_str().is_empty() => ROOT_VAR.to_owned(),
        Some(rest) => format!("{ROOT_VAR}/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// One of valet's own folders: `<Userland>/Applications/<App>/<KIND>`
/// (Settings, Data, Caches, State). valet never keys its state on XDG_*.
pub fn self_dir(kind: &str) -> Result<PathBuf> {
    Ok(self_app(&root()?).join(kind))
}

/// `value` with {Folder}, {Arch}, {App} and {Host} replaced ({Host} stays
/// the shell variable `$USERLAND_HOST`).
fn expand(value: &str, root: &Path, app: &str, arch: &str) -> String {
    let dir = root.join("Applications").join(app);
    let mut v = value
        .replace("{Arch}", arch)
        .replace("{Host}", "${USERLAND_HOST}")
        .replace("{App}", &dir.display().to_string());
    for f in FOLDERS {
        v = v.replace(&format!("{{{f}}}"), &dir.join(f).display().to_string());
    }
    v
}

fn mise_dir(root: &Path) -> PathBuf {
    root.join("Applications/Mise")
}

/// mise's cache dirs, one per architecture below it.
pub fn mise_caches(root: &Path) -> PathBuf {
    mise_dir(root).join("Caches")
}

/// Environment mise needs for `arch` (dirs inside the Mise app).
fn mise_env(root: &Path, arch: &str) -> Vec<(String, String)> {
    let m = mise_dir(root);
    let s = |p: PathBuf| p.display().to_string();
    vec![
        ("MISE_CONFIG_DIR".into(), s(m.join("Settings"))),
        ("MISE_DATA_DIR".into(), s(m.join("Data").join(arch))),
        ("MISE_CACHE_DIR".into(), s(m.join("Caches").join(arch))),
        ("MISE_STATE_DIR".into(), s(m.join("State").join(arch))),
        // Selects config.<Arch>.toml for architecture-only tools.
        ("MISE_ENV".into(), arch.to_owned()),
    ]
}

fn mise_bin(root: &Path, arch: &str) -> PathBuf {
    mise_dir(root).join("Binaries").join(arch).join("mise")
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

impl App {
    fn on(&self, arch: &str) -> bool {
        self.arches.is_empty() || self.arches.iter().any(|a| a == arch)
    }
    /// `(link name, binary name)` per command.
    fn links(&self) -> impl Iterator<Item = (&str, &str)> {
        self.commands
            .iter()
            .map(|c| c.split_once('=').unwrap_or((c.as_str(), c.as_str())))
    }
}

/// The mise config for the manifest's tools: `only = None` gives the shared
/// file (apps on every architecture, plus settings); `Some(arch)` the file
/// for apps limited to some architectures that include `arch`.
fn mise_config(m: &Manifest, only: Option<&str>) -> Result<String> {
    let mut tools = toml::Table::new();
    for (name, app) in &m.apps {
        let Some(tool) = &app.tool else { continue };
        let wanted = match only {
            None => app.arches.is_empty(),
            Some(a) => !app.arches.is_empty() && app.on(a),
        };
        if !wanted {
            continue;
        }
        let version = app
            .version
            .clone()
            .with_context(|| format!("app {name}: `tool` needs a pinned `version`"))?;
        let v = if app.options.is_empty() {
            toml::Value::String(version)
        } else {
            let mut t = app.options.clone();
            t.insert("version".into(), toml::Value::String(version));
            toml::Value::Table(t)
        };
        tools.insert(tool.clone(), v);
    }
    let mut doc = toml::Table::new();
    doc.insert("tools".into(), toml::Value::Table(tools));
    if only.is_none() {
        let mut settings = m.settings.clone();
        settings.insert("lockfile".into(), toml::Value::Boolean(true));
        doc.insert("settings".into(), toml::Value::Table(settings));
    }
    Ok(format!("{}{}", render_header(), toml::to_string(&doc)?))
}

/// Header of the files `valet apps render` writes.
fn render_header() -> String {
    crate::generated::header(
        "#",
        &format!(
            "{} (source: {})",
            crate::generated::cmd("apps render"),
            crate::generated::source("config.toml")
        ),
    )
}

/// `<self app>/Data/Generated`: shell files this tool generates (locked).
pub fn generated_dir(root: &Path) -> PathBuf {
    self_app(root).join("Data/Generated")
}

fn environment_path(root: &Path) -> PathBuf {
    generated_dir(root).join("environment.sh")
}

/// The README of [`generated_dir`].
pub fn generated_readme(root: &Path) -> Result<()> {
    let comp = format!("completions/_{}", crate::generated::name());
    crate::generated::readme(
        &generated_dir(root),
        "Generated shell files",
        &[
            (
                "environment.sh",
                "every app's environment; sourced by ~/.zshenv",
                "apps render",
            ),
            (
                "shell-init.zsh, shell-init.zsh.zwc",
                "zsh shell-init output, cached and compiled; sourced by ~/.zshrc",
                "hooks install",
            ),
            (&comp, "zsh completion (autoloaded)", "hooks install"),
            (
                "gitleaks.toml",
                "personal gitleaks config for the commit hook",
                "hooks install",
            ),
            (
                "init/*.zsh",
                "zoxide/atuin/broot/fzf init output, cached",
                "hooks install",
            ),
        ],
    )?;
    Ok(())
}

/// Shell code exporting every app's env (zshenv sources it after setting
/// USERLAND and USERLAND_ARCH); the root and `{Arch}` stay shell variables
/// so one file serves every host and architecture.
fn environment(m: &Manifest) -> String {
    let root = Path::new(ROOT_VAR);
    let mut out = render_header();
    for (k, v) in mise_env(root, "${USERLAND_ARCH}") {
        let _ = writeln!(out, "export {k}=\"{v}\"");
    }
    for (name, app) in &m.apps {
        for (k, v) in &app.env {
            let v = expand(v, root, name, "${USERLAND_ARCH}");
            let _ = writeln!(out, "export {k}=\"{}\"", v.replace('"', "\\\""));
        }
    }
    out
}

pub fn run(a: Args) -> Result<()> {
    let root = root()?;
    let m = load(&root)?;
    match a.cmd {
        Sub::Render => render(&m, &root),
        Sub::List { arch } => list(&m, &root, arches(arch.as_deref())?[0]),
        Sub::Update { apps, dry_run } => update(&m, &root, &apps, dry_run),
        Sub::Link { arch } => {
            render(&m, &root)?;
            for a in arches(arch.as_deref())? {
                link(&m, &root, a)?;
            }
            Ok(())
        }
        Sub::Install { arch } => {
            render(&m, &root)?;
            // Checksums for every platform, so each install is verified.
            crate::info!("mise lock (linux-x64, linux-arm64)");
            mise(
                &m,
                &root,
                this_arch(),
                &["lock", "--global", "--platform", "linux-x64,linux-arm64"],
                INSTALL_TIMEOUT,
            )?;
            for a in arches(arch.as_deref())? {
                install(&m, &root, a)?;
            }
            Ok(())
        }
    }
}

fn render(m: &Manifest, root: &Path) -> Result<()> {
    let settings = mise_dir(root).join("Settings");
    fs::create_dir_all(&settings)?;
    crate::generated::write(&settings.join("config.toml"), &mise_config(m, None)?)?;
    for a in ARCHES {
        crate::generated::write(
            &settings.join(format!("config.{a}.toml")),
            &mise_config(m, Some(a))?,
        )?;
    }
    let env_file = environment_path(root);
    crate::generated::write(&env_file, &environment(m))?;
    generated_readme(root)?;
    for (name, app) in m.apps.iter() {
        let dir = root.join("Applications").join(name);
        // Folders exist only when used: made here when the app's env names
        // one (a tool may expect it), else by the tool itself; empty ones go.
        for f in FOLDERS {
            let used = app.env.values().any(|v| v.contains(&format!("{{{f}}}")));
            if used {
                fs::create_dir_all(dir.join(f))?;
            } else {
                let _ = fs::remove_dir(dir.join(f)); // only succeeds when empty
            }
        }
        let _ = fs::remove_dir(&dir);
        if app.xdg && app.commands.is_empty() {
            bail!("app {name}: `xdg` needs `commands`");
        }
    }
    println!(
        "rendered {} and {}",
        settings.join("config.toml").display(),
        env_file.display()
    );
    Ok(())
}

fn write_if_changed(p: &Path, text: &str) -> Result<()> {
    if fs::read_to_string(p).is_ok_and(|t| t == text) {
        return Ok(());
    }
    let tmp = p.with_extension("tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, p).with_context(|| format!("write {}", p.display()))
}

/// Where an app's tool is installed for `arch`: mise's data dir for that
/// app alone, so the app folder holds its binaries and removing the folder
/// removes the app.
pub fn app_bin_dir(root: &Path, app: &str, arch: &str) -> PathBuf {
    root.join("Applications")
        .join(app)
        .join("Binaries")
        .join(arch)
}

/// Run mise for `arch`, here or on that architecture's host.
fn mise(m: &Manifest, root: &Path, arch: &str, args: &[&str], timeout: Duration) -> Result<String> {
    mise_in(m, root, arch, None, args, timeout)
}

/// [`mise`] with `app`'s own data dir ([`app_bin_dir`]).
fn mise_in(
    m: &Manifest,
    root: &Path,
    arch: &str,
    app: Option<&str>,
    args: &[&str],
    timeout: Duration,
) -> Result<String> {
    // mise and its plugins see the same app environment a shell would (the
    // Rust plugin, for one, installs into CARGO_HOME / RUSTUP_HOME).
    let mut env = mise_env(root, arch);
    if let Some(app) = app {
        let dir = app_bin_dir(root, app, arch).display().to_string();
        env.retain(|(k, _)| k != "MISE_DATA_DIR");
        env.push(("MISE_DATA_DIR".into(), dir));
        // Other apps' installs, read-only: a tool's install dependency (bun
        // for npm: tools) lives in its own app.
        let shared: Vec<String> = m
            .apps
            .keys()
            .filter(|n| n.as_str() != app)
            .map(|n| app_bin_dir(root, n, arch).join("installs"))
            .filter(|d| d.is_dir())
            .map(|d| d.display().to_string())
            .collect();
        if !shared.is_empty() {
            env.push(("MISE_SHARED_INSTALL_DIRS".into(), shared.join(":")));
        }
    }
    for (name, app) in &m.apps {
        for (k, v) in &app.env {
            env.push((k.clone(), expand(v, root, name, arch)));
        }
    }
    let bin = mise_bin(root, arch);
    if arch == this_arch() {
        let mut c = Proc::new(&bin).args(args).timeout(timeout);
        for (k, v) in &env {
            c = c.env(k, v);
        }
        return c.checked();
    }
    let host = m
        .hosts
        .get(arch)
        .with_context(|| format!("no [hosts] {arch} entry in config.toml to install for {arch}"))?;
    // One remote command line: `env K=V ... mise args` (no secrets in it).
    // Calls share one connection (install runs one per app). The other host
    // mounts the shared home dir at the same path, so paths pass unchanged.
    let mut line = String::from("env");
    for (k, v) in &env {
        let _ = write!(line, " {k}={}", sh_quote(v));
    }
    let _ = write!(line, " {}", sh_quote(&bin.display().to_string()));
    for a in args {
        let _ = write!(line, " {}", sh_quote(a));
    }
    let mut ssh = Proc::new("ssh").args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"]);
    if let Some(run) = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        let dir = PathBuf::from(run).join("valet/ssh");
        fs::create_dir_all(&dir)?;
        ssh = ssh.args([
            "-o".to_owned(),
            "ControlMaster=auto".to_owned(),
            "-o".to_owned(),
            format!("ControlPath={}/%C", dir.display()),
            "-o".to_owned(),
            "ControlPersist=60".to_owned(),
        ]);
    }
    ssh.args([host, &line]).timeout(timeout).checked()
}

/// Architectures apps are installed for: this machine's, plus those with a
/// `[hosts]` entry.
pub fn installed_arches() -> Result<Vec<&'static str>> {
    let m = load(&root()?)?;
    Ok(ARCHES
        .iter()
        .copied()
        .filter(|a| *a == this_arch() || m.hosts.contains_key(*a))
        .collect())
}

/// `mise ARGS` for each app with a tool on ARCH, in the app's own data dir:
/// (app, stdout). Stops at the first failure (an unreachable host).
pub fn mise_each_app(
    arch: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<Vec<(String, String)>> {
    let root = root()?;
    let m = load(&root)?;
    let mut out = Vec::new();
    for (name, a) in &m.apps {
        if a.tool.is_none() || !a.on(arch) {
            continue;
        }
        let o = mise_in(&m, &root, arch, Some(name), args, timeout)
            .with_context(|| format!("{arch}: {name}"))?;
        out.push((name.clone(), o));
    }
    Ok(out)
}

/// For every architecture, the Rust app's `RUSTUP_HOME` and its pinned
/// version: rustup keeps toolchains there that `mise prune` does not remove.
pub fn rustup_homes() -> Result<Vec<(PathBuf, String)>> {
    let root = root()?;
    let m = load(&root)?;
    let mut v = Vec::new();
    for (name, a) in &m.apps {
        if a.tool.as_deref() != Some("core:rust") {
            continue;
        }
        let (Some(ver), Some(home)) = (&a.version, a.env.get("RUSTUP_HOME")) else {
            continue;
        };
        for arch in ARCHES {
            if a.on(arch) {
                v.push((PathBuf::from(expand(home, &root, name, arch)), ver.clone()));
            }
        }
    }
    Ok(v)
}

fn install(m: &Manifest, root: &Path, arch: &str) -> Result<()> {
    crate::info!("{arch}: mise install, one app at a time");
    let apps: Vec<(&String, &str)> = m
        .apps
        .iter()
        .filter(|(_, a)| a.on(arch))
        .filter_map(|(n, a)| a.tool.as_deref().map(|t| (n, t)))
        .collect();
    // An app whose install dependency is another app (npm: tools need Bun)
    // can fail before that one is in; the second pass retries it.
    let mut failed = Vec::new();
    for pass in 0..2 {
        let todo: Vec<_> = if pass == 0 {
            apps.clone()
        } else {
            std::mem::take(&mut failed)
        };
        for (name, tool) in todo {
            let r = mise_in(
                m,
                root,
                arch,
                Some(name),
                &["install", "--yes", tool],
                INSTALL_TIMEOUT,
            )
            .and_then(|_| {
                // Versions this app no longer pins.
                mise_in(
                    m,
                    root,
                    arch,
                    Some(name),
                    &["prune", "--yes"],
                    INSTALL_TIMEOUT,
                )
            });
            match r {
                Ok(_) => {}
                Err(e) if pass == 0 => {
                    crate::info!("{arch}: {name}: retrying after the others ({e:#})");
                    failed.push((name, tool));
                }
                Err(e) => return Err(e.context(format!("{arch}: install {name}"))),
            }
        }
        if failed.is_empty() {
            break;
        }
    }
    link(m, root, arch)
}

/// Make `Commands/<arch>/<cmd>` for every app command: a link to the binary,
/// or a launcher for `xdg` apps. Removes stale entries.
fn link(m: &Manifest, root: &Path, arch: &str) -> Result<()> {
    let dir = root.join("Commands").join(arch);
    fs::create_dir_all(&dir)?;
    let mut wanted = std::collections::BTreeSet::new();
    // mise itself lives in the Mise app, outside mise's own installs.
    symlink(&mise_bin(root, arch), &dir.join("mise"))?;
    wanted.insert("mise".to_owned());
    if link_self(root, arch)? {
        wanted.insert(crate::generated::name().to_owned());
    }
    for (name, app) in &m.apps {
        if (app.tool.is_none() && app.system.is_none()) || !app.on(arch) {
            continue;
        }
        link_files(root, name, app)?;
        for (cmd, real) in app.links() {
            let bin = match &app.system {
                Some(sys) => sys.clone(),
                None => match mise_in(m, root, arch, Some(name), &["which", real], QUERY_TIMEOUT) {
                    Ok(p) if !p.trim().is_empty() => p.trim().to_owned(),
                    _ => {
                        crate::warn!("{arch}: {name}: no `{cmd}` after install; skipped");
                        continue;
                    }
                },
            };
            let target = dir.join(cmd);
            if app.xdg || !app.args.is_empty() || app.via.is_some() {
                crate::generated::remove(&target)?;
                crate::generated::write_mode(&target, &wrapper(app, name, arch, &bin), 0o555)?;
            } else {
                symlink(Path::new(&bin), &target)?;
            }
            wanted.insert(cmd.to_owned());
        }
    }
    for e in fs::read_dir(&dir)?.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if !wanted.contains(&n) {
            crate::generated::remove(&e.path())?;
        }
    }
    println!("{arch}: {} commands in {}", wanted.len(), dir.display());
    Ok(())
}

/// `$USERLAND` for a script in `<root>/<dir>/<script>` (DEPTH folders below
/// the root): the caller's, else found from the script's own path (git and
/// other tools may run it with an environment that lacks the variable).
fn root_from_script(depth: usize, resolve: &str) -> String {
    let up = vec![".."; depth].join("/");
    format!(
        "[ -n \"${{USERLAND:-}}\" ] || USERLAND=$(cd \"$(dirname {resolve})\"/{up} && pwd -P)\n"
    )
}

/// The launcher for APP's command BIN (`xdg`, `args` or `via`): shell text
/// that names Userland only through `$USERLAND`, so every host runs it.
fn wrapper(app: &App, name: &str, arch: &str, bin: &str) -> String {
    let root = Path::new(ROOT_VAR);
    let d = root.join("Applications").join(name);
    let mut s = String::from("#!/bin/sh\n# Keeps this tool's files in its app folder.\n");
    s.push_str(&crate::generated::header(
        "#",
        &crate::generated::cmd("apps link"),
    ));
    s.push_str(&root_from_script(2, "\"$0\""));
    if app.xdg {
        for (var, f) in [
            ("XDG_CONFIG_HOME", "Settings"),
            ("XDG_DATA_HOME", "Data"),
            ("XDG_CACHE_HOME", "Caches"),
            ("XDG_STATE_HOME", "State"),
        ] {
            let _ = writeln!(
                s,
                "export {var}={}",
                sh_quote_root(&d.join(f).display().to_string())
            );
        }
    }
    let args: String = app
        .args
        .iter()
        .map(|a| format!(" {}", sh_quote_root(&expand(a, root, name, arch))))
        .collect();
    let via = app
        .via
        .as_deref()
        .map_or(String::new(), |v| format!("{} ", sh_quote(v)));
    let bin = in_shell(Path::new(bin));
    let _ = writeln!(s, "exec {via}{}{args} \"$@\"", sh_quote_root(&bin));
    s
}

/// [`sh_quote`], with every `${USERLAND}` left to the shell.
fn sh_quote_root(s: &str) -> String {
    sh_quote(s).replace(ROOT_VAR, &format!("'\"{ROOT_VAR}\"'"))
}

/// APP's `files`: links from its app folder into the valet checkout.
fn link_files(root: &Path, name: &str, app: &App) -> Result<()> {
    if app.files.is_empty() {
        return Ok(());
    }
    let checkout =
        crate::repos::alias_path("valet").context("no valet checkout (alias `valet`)")?;
    for (rel, src) in &app.files {
        let at = root.join("Applications").join(name).join(rel);
        if let Some(d) = at.parent() {
            fs::create_dir_all(d)?;
        }
        symlink(&checkout.join(src), &at)?;
    }
    Ok(())
}

/// This tool's own app folder: its binary name, capitalised
/// (`valet` -> `Applications/Valet`).
pub fn self_app(root: &Path) -> PathBuf {
    let n = crate::generated::name();
    let mut c = n.chars();
    let cap: String = c
        .next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default();
    root.join("Applications").join(cap)
}

/// Where `upgrade` puts this tool's binary for `arch`.
pub fn valet_bin(root: &Path, arch: &str) -> PathBuf {
    self_app(root)
        .join("Binaries")
        .join(arch)
        .join(crate::generated::name())
}

/// `<Userland>/Scripts/<name>`: runs this tool for the machine's
/// architecture, so commands stored in files shared by every machine
/// (git hook links) work on Amd64 and Arm64 alike, on every host: it names
/// Userland only through `$USERLAND`, else its own resolved path. Keeps
/// `argv[0]`: git hooks dispatch on it.
pub fn launcher() -> Result<PathBuf> {
    let root = root()?;
    let name = crate::generated::name();
    let p = root.join("Scripts").join(name);
    let body = format!(
        "#!/bin/bash\n{}{}case \"$(uname -m)\" in aarch64|arm64) a=Arm64 ;; *) a=Amd64 ;; esac\n\
         exec -a \"${{0##*/}}\" \"$USERLAND/Commands/$a/{name}\" \"$@\"\n",
        crate::generated::header("#", &crate::generated::cmd("hooks install")),
        root_from_script(1, "\"$(readlink -f \"$0\")\""),
    );
    crate::generated::write_mode(&p, &body, 0o555)?;
    Ok(p)
}

/// The installed binary for this machine (after an upgrade, the new one).
pub fn self_bin() -> Result<PathBuf> {
    Ok(valet_bin(&root()?, this_arch()))
}

/// Link `Commands/<arch>/<name>` to this tool's binary when there is one.
pub fn link_self(root: &Path, arch: &str) -> Result<bool> {
    let bin = valet_bin(root, arch);
    if !bin.exists() {
        return Ok(false);
    }
    let dir = root.join("Commands").join(arch);
    fs::create_dir_all(&dir)?;
    symlink(&bin, &dir.join(crate::generated::name()))?;
    Ok(true)
}

/// Apps whose tool has a release index mise can ask for "latest".
fn has_index(tool: &str) -> bool {
    !tool.starts_with("http:")
}

fn update(m: &Manifest, root: &Path, only: &[String], dry_run: bool) -> Result<()> {
    for o in only {
        if !m.apps.contains_key(o) {
            bail!("no app {o} in config.toml");
        }
    }
    let here = this_arch();
    let path = crate::config::source()?;
    let mut text = fs::read_to_string(&path)
        .with_context(|| format!("read {} (needs the valet checkout)", path.display()))?;
    let mut changed = 0;
    for (name, app) in &m.apps {
        if !only.is_empty() && !only.contains(name) {
            continue;
        }
        let (Some(tool), Some(cur)) = (&app.tool, &app.version) else {
            continue;
        };
        if !has_index(tool) {
            println!("{name:20} {cur:14} (no release index: bump by hand)");
            continue;
        }
        let latest = match mise(m, root, here, &["latest", tool], QUERY_TIMEOUT) {
            Ok(v) => v.trim().lines().last().unwrap_or_default().to_owned(),
            Err(e) => {
                crate::warn!("{name}: cannot ask for the latest version: {e:#}");
                continue;
            }
        };
        if latest.is_empty() || latest == *cur {
            continue;
        }
        println!("{name:20} {cur:14} -> {latest}");
        text = set_version(&text, name, &latest)
            .with_context(|| format!("no `version` line for {name} in config.toml"))?;
        changed += 1;
    }
    if changed == 0 {
        println!("everything is at its latest release");
        return Ok(());
    }
    if dry_run {
        return Ok(());
    }
    write_if_changed(&path, &text)?;
    println!("updated {}; commit it in the valet repo", path.display());
    let m = parse(&text)?;
    render(&m, root)?;
    let arches: Vec<&str> = ARCHES
        .iter()
        .copied()
        .filter(|a| *a == here || m.hosts.contains_key(*a))
        .collect();
    for a in &arches {
        install(&m, root, a)?;
        mise(&m, root, a, &["prune", "--yes"], INSTALL_TIMEOUT)?;
    }
    println!("updated {changed} app(s) on {}", arches.join(", "));
    Ok(())
}

/// `text` with the first `version = "..."` of `[apps.<name>]` set to `v`;
/// comments and layout stay as they are.
fn set_version(text: &str, name: &str, v: &str) -> Option<String> {
    let header = format!("[apps.{name}]");
    let start = text.lines().position(|l| l.trim() == header)?;
    let mut out: Vec<String> = text.lines().map(str::to_owned).collect();
    for line in out.iter_mut().skip(start + 1) {
        let t = line.trim_start();
        if t.starts_with('[') {
            return None;
        }
        if let Some(rest) = t.strip_prefix("version") {
            let rest = rest.trim_start();
            if let Some(after) = rest.strip_prefix('=') {
                let after = after.trim_start();
                let q = after.strip_prefix('"')?;
                let end = q.find('"')?;
                *line = format!("version = \"{v}\"{}", &q[end + 1..]);
                let mut s = out.join("\n");
                if text.ends_with('\n') {
                    s.push('\n');
                }
                return Some(s);
            }
        }
    }
    None
}

fn list(m: &Manifest, root: &Path, arch: &str) -> Result<()> {
    let dir = root.join("Commands").join(arch);
    for (name, app) in &m.apps {
        if !app.on(arch) {
            continue;
        }
        let cmds: Vec<String> = app
            .links()
            .map(|(c, _)| {
                let ok = dir.join(c).exists();
                format!("{c}{}", if ok { "" } else { " (missing)" })
            })
            .collect();
        println!(
            "{name:16} {:10} {:42} {}",
            app.version.as_deref().unwrap_or("-"),
            app.tool.as_deref().unwrap_or("(not from mise)"),
            cmds.join(", ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_from_install_path() {
        assert_eq!(
            installed_root(Path::new("/w/Applications/Valet/Binaries/Arm64/valet")),
            Some(PathBuf::from("/w"))
        );
        assert_eq!(installed_root(Path::new("/r/target/release/valet")), None);
        assert_eq!(
            installed_root(Path::new("/w/Applications/Valet/Binaries/x86/valet")),
            None
        );
    }

    const SAMPLE: &str = r#"
[hosts]
Arm64 = "arm-host"

[apps.Ripgrep]
tool = "aqua:BurntSushi/ripgrep"
version = "15.2.0"
commands = ["rg"]

[apps.Atuin]
tool = "aqua:atuinsh/atuin"
version = "18.24.8"
commands = ["atuin"]
env = { ATUIN_CONFIG_DIR = "{Settings}", ATUIN_DB = "{Data}/{Arch}/history.db" }

[apps.Tre]
tool = "github:dduan/tre"
version = "0.4.0"
options = { bin_path = "tre" }
commands = ["tre"]
xdg = true
"#;

    fn sample() -> Manifest {
        toml::from_str(SAMPLE).unwrap()
    }

    #[test]
    fn manifest_parses_and_rejects_unknown_fields() {
        let m = sample();
        assert_eq!(m.hosts["Arm64"], "arm-host");
        assert_eq!(m.apps["Ripgrep"].commands, ["rg"]);
        assert!(toml::from_str::<Manifest>("[apps.X]\ntool='a'\nbogus=1\n").is_err());
    }

    #[test]
    fn mise_config_pins_and_keeps_options() {
        let c = mise_config(&sample(), None).unwrap();
        assert!(
            c.contains("\"aqua:BurntSushi/ripgrep\" = \"15.2.0\""),
            "{c}"
        );
        assert!(c.contains("bin_path = \"tre\""), "{c}");
        assert!(c.contains("lockfile = true"), "{c}");
        let unpinned: Manifest = toml::from_str("[apps.X]\ntool = \"aqua:a/b\"\n").unwrap();
        assert!(mise_config(&unpinned, None).is_err());
    }

    #[test]
    fn arch_only_apps_and_renamed_commands() {
        let m: Manifest = toml::from_str(
            "[settings]\nnpm = { package_manager = \"bun\" }\n\
             [apps.Chrome]\ntool = \"http:chs\"\nversion = \"1\"\narches = [\"Amd64\"]\ncommands = [\"chs\"]\n\
             [apps.MiniJinja]\ntool = \"github:m/j\"\nversion = \"2\"\ncommands = [\"jinja=minijinja-cli\"]\n",
        )
        .unwrap();
        let all = mise_config(&m, None).unwrap();
        assert!(
            all.contains("github:m/j") && !all.contains("http:chs"),
            "{all}"
        );
        assert!(all.contains("package_manager = \"bun\""), "{all}");
        assert!(mise_config(&m, Some("Amd64")).unwrap().contains("http:chs"));
        assert!(!mise_config(&m, Some("Arm64")).unwrap().contains("http:chs"));
        let links: Vec<_> = m.apps["MiniJinja"].links().collect();
        assert_eq!(links, [("jinja", "minijinja-cli")]);
    }

    #[test]
    fn environment_keeps_arch_as_a_shell_variable() {
        let e = environment(&sample());
        assert!(
            e.contains(
                "export MISE_DATA_DIR=\"${USERLAND}/Applications/Mise/Data/${USERLAND_ARCH}\""
            ),
            "{e}"
        );
        assert!(
            e.contains("export ATUIN_CONFIG_DIR=\"${USERLAND}/Applications/Atuin/Settings\""),
            "{e}"
        );
        assert!(
            e.contains(
                "export ATUIN_DB=\"${USERLAND}/Applications/Atuin/Data/${USERLAND_ARCH}/history.db\""
            ),
            "{e}"
        );
    }

    #[test]
    fn wrapper_names_userland_only_through_the_variable() {
        let m = sample();
        let w = wrapper(&m.apps["Tre"], "Tre", "Amd64", "/usr/bin/tre");
        assert!(
            w.contains(
                "[ -n \"${USERLAND:-}\" ] || USERLAND=$(cd \"$(dirname \"$0\")\"/../.. && pwd -P)"
            ),
            "{w}"
        );
        assert!(
            w.contains("export XDG_CONFIG_HOME=''\"${USERLAND}\"'/Applications/Tre/Settings'"),
            "{w}"
        );
        assert!(w.contains("exec '/usr/bin/tre' \"$@\""), "{w}");
        assert_eq!(sh_quote_root("${USERLAND}/a b"), "''\"${USERLAND}\"'/a b'");
    }

    #[test]
    fn canonical_resolves_links_and_keeps_missing_tails() {
        let d = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".tmp")
            .join(format!("valet-canon-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("real/sub")).unwrap();
        std::os::unix::fs::symlink(d.join("real"), d.join("link")).unwrap();
        let real = d.join("real").canonicalize().unwrap();
        assert_eq!(canonical(&d.join("link/sub")), real.join("sub"));
        assert_eq!(
            canonical(&d.join("link/new/file.txt")),
            real.join("new/file.txt")
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn relative_paths() {
        let r = |t: &str, b: &str| relative(Path::new(t), Path::new(b));
        assert_eq!(
            r("/u/Applications/A/x", "/u/Commands/Amd64"),
            Path::new("../../Applications/A/x")
        );
        assert_eq!(r("/h/ws/Userland/L/P", "/h"), Path::new("ws/Userland/L/P"));
        assert_eq!(r("/u/a", "/u/a/b"), Path::new(".."));
    }

    #[test]
    fn set_version_keeps_comments_and_other_apps() {
        let t =
            "[apps.A]\ntool = \"x\"\nversion = \"1.0\"   # pinned\n\n[apps.B]\nversion = \"1.0\"\n";
        let out = set_version(t, "A", "2.0").unwrap();
        assert_eq!(
            out,
            "[apps.A]\ntool = \"x\"\nversion = \"2.0\"   # pinned\n\n[apps.B]\nversion = \"1.0\"\n"
        );
        assert_eq!(set_version(t, "C", "2"), None);
        assert!(
            set_version(
                "[apps.A]\ntool = \"x\"\n[apps.B]\nversion = \"1\"\n",
                "A",
                "2"
            )
            .is_none()
        );
    }

    #[test]
    fn arch_names() {
        assert_eq!(arches(Some("arm64")).unwrap(), ["Arm64"]);
        assert_eq!(arches(Some("all")).unwrap(), ["Amd64", "Arm64"]);
        assert!(arches(Some("x86")).is_err());
    }
}
