//! `valet hooks`: one switchboard for every hook `valet` runs.
//!
//! Hooks fire automatically once installed; this module only decides which
//! are enabled (state in `$XDG_CONFIG_HOME/valet/hooks.json`) and wires them
//! into their callers: the shell rc file, git's `core.hooksPath`, and Claude
//! Code's `settings.json`. Every change made by `install` is undone by
//! `uninstall`.

use crate::proc::Cmd;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub const LONG_ABOUT: &str = "\
Manage every hook valet runs: list, enable, disable, install, uninstall.

Hooks run automatically after `valet hooks install`:
  shell.secrets            shell start   export ALL_CAPS pass entries as env variables
                                         (one decrypt; skipped while gpg-agent is locked)
  shell.env                every cd      set/unset variables by directory and host (config.toml [env])
  git.pre-commit           git commit    gitleaks on staged changes, then the repo's hook
  git.ascii                git commit    refuse invisible chars, and ASCII lookalikes in code (after gitleaks;
                                         off in one repo: git config valet.ascii false)
  git.pre-push             git push      refuse rewriting branches of published PRs; git lfs
  git.post-checkout        git checkout  git lfs
  git.post-commit          git commit    git lfs
  git.post-merge           git merge     git lfs
  claude.canonical-paths   Claude Code   deny other spellings of this host's folders (home-dir links to
                                         the ws folder, config.toml [[paths.alias]]) in file tools
                                         and Bash; the reason names this host's spelling
  claude.skill-source      Claude Code   keep skill edits in their git source (deny links,
                                         third-party, team default branch)
  claude.skill-commit      Claude stop   block once while this session's skill edits are uncommitted
  claude.no-tmp            Claude Code   deny creating files/dirs under /tmp or /var/tmp (reads allowed)
  claude.search-tools      Claude Code   deny grep/find (use rg/fd) and fd/rg walks outside the ws folder, unless FIND_REASON
  claude.worktree          Claude Code   recreate a folded worktree the session uses; mark it active
  claude.worktree-create   Claude Code   make --worktree checkouts at <repo>/tree/<user>/<name> (else .claude/worktrees/)
  claude.worktree-remove   Claude Code   fold such a checkout (branch kept); else git worktree remove
  claude.session-name      Claude prompt kebab-case session title from Claude's ai-title (user renames win);
                                         herdr tab label in PascalCase (also at session start)

`install` wires all of them (shell rc block, git core.hooksPath, Claude Code
settings.json); `uninstall` reverts exactly those changes. `enable`/`disable` take effect on the next run (shell
hooks: next cd or new shell). Disabled git.pre-push still runs git lfs;
other disabled hooks do nothing. `list` shows state, wiring, and hooks owned
by others (org-installed Claude hooks), which valet never changes.";

const BLOCK_BEGIN: &str = "# >>> valet shell-init (managed by `valet hooks install`) >>>";
const BLOCK_END: &str = "# <<< valet shell-init <<<";

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Shell,
    Git,
    Claude,
}

struct Hook {
    id: &'static str,
    kind: Kind,
    when: &'static str,
    what: &'static str,
}

const HOOKS: &[Hook] = &[
    Hook {
        id: "shell.secrets",
        kind: Kind::Shell,
        when: "shell start",
        what: "export ALL_CAPS pass entries as env variables",
    },
    Hook {
        id: "shell.env",
        kind: Kind::Shell,
        when: "every cd",
        what: "set/unset variables by directory and host (config.toml [env])",
    },
    Hook {
        id: "git.pre-commit",
        kind: Kind::Git,
        when: "git commit",
        what: "gitleaks on staged changes, then repo hook",
    },
    Hook {
        id: "git.ascii",
        kind: Kind::Git,
        when: "git commit",
        what: "refuse invisible chars, and ASCII lookalikes in code (after gitleaks)",
    },
    Hook {
        id: "git.pre-push",
        kind: Kind::Git,
        when: "git push",
        what: "guard published-PR branches; git lfs",
    },
    Hook {
        id: "git.post-checkout",
        kind: Kind::Git,
        when: "git checkout",
        what: "git lfs",
    },
    Hook {
        id: "git.post-commit",
        kind: Kind::Git,
        when: "git commit",
        what: "git lfs",
    },
    Hook {
        id: "git.post-merge",
        kind: Kind::Git,
        when: "git merge",
        what: "git lfs",
    },
    Hook {
        id: "claude.canonical-paths",
        kind: Kind::Claude,
        when: "Claude PreToolUse",
        what: "deny other spellings of this host's paths",
    },
    Hook {
        id: "claude.skill-source",
        kind: Kind::Claude,
        when: "Claude PreToolUse",
        what: "keep skill edits in their git source",
    },
    Hook {
        id: "claude.skill-commit",
        kind: Kind::Claude,
        when: "Claude Stop",
        what: "block while skill edits are uncommitted",
    },
    Hook {
        id: "claude.worktree",
        kind: Kind::Claude,
        when: "Claude PreToolUse",
        what: "recreate a folded worktree the session uses; mark it active",
    },
    Hook {
        id: "claude.worktree-create",
        kind: Kind::Claude,
        when: "Claude WorktreeCreate",
        what: "make worktrees at <repo>/tree/<user>/<name>",
    },
    Hook {
        id: "claude.worktree-remove",
        kind: Kind::Claude,
        when: "Claude WorktreeRemove",
        what: "fold such a worktree (branch kept)",
    },
    Hook {
        id: "claude.session-name",
        kind: Kind::Claude,
        when: "Claude prompt/start",
        what: "kebab-case session title; herdr tab label",
    },
    Hook {
        id: "claude.no-tmp",
        kind: Kind::Claude,
        when: "Claude PreToolUse",
        what: "deny creating files/dirs under /tmp",
    },
    Hook {
        id: "claude.search-tools",
        kind: Kind::Claude,
        when: "Claude PreToolUse",
        what: "deny grep/find (use rg/fd) and fd/rg walks outside ws",
    },
];

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    cmd: Sub,
}

#[derive(clap::Subcommand)]
enum Sub {
    /// Show every hook: enabled, wired, and hooks owned by others.
    List,
    /// Enable hooks (ids from `valet hooks list`, or `all`).
    Enable {
        #[arg(required = true)]
        ids: Vec<String>,
    },
    /// Disable hooks (ids from `valet hooks list`, or `all`).
    Disable {
        #[arg(required = true)]
        ids: Vec<String>,
    },
    /// Wire all hooks into the shell rc, git and Claude Code (idempotent).
    Install {
        /// Show what would change without changing anything.
        #[arg(short = 'n', long)]
        dry_run: bool,
    },
    /// Undo everything `install` changed.
    Uninstall {
        #[arg(short = 'n', long)]
        dry_run: bool,
    },
    /// Run the hooks for an event and print shell code to eval (used by shell-init).
    #[command(hide = true)]
    Run { event: String },
}

pub fn run(args: Args) -> Result<()> {
    match args.cmd {
        Sub::List => list(),
        Sub::Enable { ids } => set_enabled(&ids, true),
        Sub::Disable { ids } => set_enabled(&ids, false),
        Sub::Install { dry_run } => install(dry_run),
        Sub::Uninstall { dry_run } => uninstall(dry_run),
        Sub::Run { event } => match event.as_str() {
            "chpwd" => {
                // Keeps this worktree out of the idle fold.
                if let Ok(cwd) = std::env::current_dir() {
                    crate::repos::mark_active(&cwd);
                }
                if enabled("shell.env") {
                    crate::cmd::envrules::chpwd()?;
                }
                Ok(())
            }
            other => bail!("unknown hook event {other:?}"),
        },
    }
}

// ---------- state ----------

#[derive(Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    disabled: Vec<String>,
    /// What `install` replaced, so `uninstall` can restore it.
    #[serde(default)]
    installed: Installed,
}

#[derive(Default, Serialize, Deserialize)]
struct Installed {
    git_hooks_path_before: Option<String>,
    #[serde(default)]
    excludes_file_before: Option<String>,
    #[serde(default)]
    rc_files: Vec<PathBuf>,
}

fn config_dir() -> Result<PathBuf> {
    crate::cmd::apps::self_dir("Settings")
}

fn home() -> Result<PathBuf> {
    Ok(PathBuf::from(
        std::env::var_os("HOME").context("HOME is not set")?,
    ))
}

fn state_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("hooks.json"))
}

fn load() -> Result<State> {
    let p = state_path()?;
    match fs::read_to_string(&p) {
        Ok(s) => serde_json::from_str(&s).with_context(|| format!("parse {}", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(e) => Err(e).with_context(|| format!("read {}", p.display())),
    }
}

fn save(s: &State) -> Result<()> {
    let p = state_path()?;
    fs::create_dir_all(p.parent().expect("config dir"))?;
    write_atomic(&p, &(serde_json::to_string_pretty(s)? + "\n"))
}

/// Whether hook `id` is enabled. Unreadable state counts as enabled so a
/// broken config never silently drops the security hooks.
pub fn enabled(id: &str) -> bool {
    load()
        .map(|s| !s.disabled.iter().any(|d| d == id))
        .unwrap_or(true)
}

fn set_enabled(ids: &[String], on: bool) -> Result<()> {
    let ids: Vec<&str> = if ids.iter().any(|i| i == "all") {
        HOOKS.iter().map(|h| h.id).collect()
    } else {
        for id in ids {
            if !HOOKS.iter().any(|h| h.id == id) {
                bail!("unknown hook {id:?}; see `valet hooks list`");
            }
        }
        ids.iter().map(String::as_str).collect()
    };
    let mut s = load()?;
    s.disabled.retain(|d| !ids.contains(&d.as_str()));
    if !on {
        s.disabled.extend(ids.iter().map(|i| i.to_string()));
        s.disabled.sort();
    }
    save(&s)?;
    // The cached shell-init reflects which hooks are on.
    if let Err(e) = crate::cmd::shell::write_cache() {
        crate::warn!("shell cache not refreshed: {e:#}");
    }
    for id in ids {
        println!("{} {id}", if on { "enabled " } else { "disabled" });
        if !on && (id == "git.pre-commit" || id == "git.pre-push") {
            eprintln!("valet: note: {id} is a safety check (secrets / published PRs)");
        }
    }
    Ok(())
}

// ---------- shell integration ----------

/// Snippet for `valet shell-init`: only enabled shell hooks are registered.
pub fn shell_snippet(zsh: bool) -> String {
    let mut out = String::new();
    if enabled("shell.secrets") {
        out.push_str(if zsh { SECRETS_ZSH } else { SECRETS_BASH });
    }
    if enabled("shell.env") {
        out.push_str(if zsh { CHPWD_ZSH } else { CHPWD_BASH });
    }
    out
}

/// Export `valet vars` values at shell start, and again at a prompt after
/// `valet vars set/mv/rm/lock/unlock` changed the marker. `read` is a builtin,
/// so an unchanged prompt forks nothing.
const SECRETS_ZSH: &str = r#"__valet_secrets_mark=
__valet_secrets_sync() {
  local m=none
  [[ -n ${XDG_RUNTIME_DIR:-} && -r $XDG_RUNTIME_DIR/valet/secrets.changed ]] &&
    read -r m < "$XDG_RUNTIME_DIR/valet/secrets.changed"
  [[ $m == "$__valet_secrets_mark" ]] && return
  __valet_secrets_mark=$m
  eval "$(command valet vars env --if-unlocked)"
}
__valet_secrets_sync
autoload -Uz add-zsh-hook
add-zsh-hook precmd __valet_secrets_sync
"#;
const SECRETS_BASH: &str = r#"__valet_secrets_mark=
__valet_secrets_sync() {
  local m=none
  [ -n "${XDG_RUNTIME_DIR:-}" ] && [ -r "$XDG_RUNTIME_DIR/valet/secrets.changed" ] &&
    read -r m < "$XDG_RUNTIME_DIR/valet/secrets.changed"
  [ "$m" = "$__valet_secrets_mark" ] && return
  __valet_secrets_mark=$m
  eval "$(command valet vars env --if-unlocked)"
}
__valet_secrets_sync
PROMPT_COMMAND="__valet_secrets_sync${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
"#;

const CHPWD_ZSH: &str = r#"__valet_hooks_chpwd() {
  setopt local_options null_glob
  local __valet_out
  __valet_out="$(VALET_ENV_STATE="${__valet_env_state-}" command valet hooks run chpwd)" || return
  [ -n "$__valet_out" ] && eval "$__valet_out"
}
autoload -Uz add-zsh-hook
add-zsh-hook chpwd __valet_hooks_chpwd
__valet_hooks_chpwd
"#;

const CHPWD_BASH: &str = r#"__valet_hooks_chpwd() {
  [ "$PWD" = "${__valet_hooks_pwd-}" ] && return
  __valet_hooks_pwd=$PWD
  local __valet_out
  __valet_out="$(VALET_ENV_STATE="${__valet_env_state-}" command valet hooks run chpwd)" || return
  [ -n "$__valet_out" ] && eval "$__valet_out"
}
PROMPT_COMMAND="__valet_hooks_chpwd${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
"#;

/// A `.zshrc` valet renders (`shell/rc/`) sources drop-ins instead of carrying
/// a block. It contains this glob.
const ZSH_DROPIN_MARKER: &str = "zshrc.d/*.zsh(N)";

/// (path, content) of the zsh drop-ins, when ~/.zshrc is valet's.
fn zsh_dropins() -> Result<Option<[(PathBuf, String); 2]>> {
    let dir = crate::cmd::shell::zsh_dir()?;
    let zshrc = fs::read_to_string(dir.join(".zshrc")).unwrap_or_default();
    if !zshrc.contains(ZSH_DROPIN_MARKER) {
        return Ok(None);
    }
    let vars = crate::cmd::shell::dropin_vars()?;
    Ok(Some([
        (
            dir.join(format!("zshenv.d/{}.zsh", crate::generated::name())),
            crate::cmd::shell::render("zshenv-dropin.zsh", &vars)?,
        ),
        (
            dir.join(format!("zshrc.d/{}.zsh", crate::generated::name())),
            crate::cmd::shell::render("zshrc-dropin.zsh", &vars)?,
        ),
    ]))
}

fn rc_block(shell: &str) -> String {
    let n = crate::generated::name();
    format!(
        "{BLOCK_BEGIN}\ncommand -v {n} >/dev/null 2>&1 && eval \"$(command {n} shell-init {shell})\"\n{BLOCK_END}\n"
    )
}

/// rc files to manage: (path with symlinks resolved, shell).
fn rc_files() -> Result<Vec<(PathBuf, &'static str)>> {
    let h = home()?;
    let mut v = Vec::new();
    for (name, shell) in [(".zshrc", "zsh"), (".bashrc", "bash")] {
        let p = h.join(name);
        // Generated whole by `shell::install`: no block to manage.
        let generated = fs::read_to_string(&p).is_ok_and(|t| t.starts_with("# Generated by "));
        if p.exists() && !generated {
            v.push((fs::canonicalize(&p)?, shell));
        }
    }
    Ok(v)
}

/// rc text with the managed block appended.
fn rc_install(text: &str, shell: &str) -> String {
    let mut out: String = text.lines().map(|l| format!("{l}\n")).collect();
    if !text.contains(BLOCK_BEGIN) {
        if !out.is_empty() && !out.ends_with("\n\n") {
            out.push('\n');
        }
        out.push_str(&rc_block(shell));
    }
    out
}

/// Inverse of [`rc_install`].
fn rc_uninstall(text: &str) -> String {
    let mut out = String::new();
    let mut in_block = false;
    for l in text.lines() {
        if l == BLOCK_BEGIN {
            in_block = true;
            continue;
        }
        if in_block {
            in_block = l != BLOCK_END;
            continue;
        }
        out.push_str(l);
        out.push('\n');
    }
    // Drop the blank line install added before the block.
    while out.ends_with("\n\n") {
        out.pop();
    }
    out
}

// ---------- git ----------

pub(crate) fn git_hooks_dir() -> Result<PathBuf> {
    Ok(crate::tools::managed_dir()?
        .parent()
        .expect("data dir")
        .join("git-hooks"))
}

fn global_hooks_path() -> Option<String> {
    Cmd::new("git")
        .args(["config", "--global", "--get", "core.hooksPath"])
        .checked()
        .ok()
        .filter(|s| !s.is_empty())
}

/// `core.hooksPath` for DIR: `~/...` through the home dir's link, since
/// every host reads git's global config.
fn hooks_path_value(dir: &Path) -> String {
    crate::cmd::apps::portable_tilde(dir)
}

fn git_wired() -> bool {
    let (Ok(dir), Some(cur)) = (git_hooks_dir(), global_hooks_path()) else {
        return false;
    };
    cur == hooks_path_value(&dir)
        && crate::cmd::apps::launcher()
            .is_ok_and(|exe| crate::cmd::apps::link_is(&dir.join("pre-push"), &exe))
}

/// The hook command for settings files every host reads (Claude's
/// settings.json): valet by name, found on each host's
/// PATH (`Commands/<Arch>`), so no host's spelling or architecture is baked in.
fn hook_cmd(args: &str) -> String {
    format!("{} {args}", crate::generated::name())
}

// ---------- Claude Code ----------

fn claude_settings() -> Result<PathBuf> {
    Ok(super::claude_dir()?.join("settings.json"))
}

/// Claude Code hook groups valet adds itself: (hook id, event, matcher, subcommand).
const CLAUDE_ADDED: &[(&str, &str, Option<&str>, &str)] = &[
    (
        "claude.canonical-paths",
        "PreToolUse",
        Some("Read|Edit|Write|MultiEdit|NotebookEdit|Glob|Grep|Bash"),
        "canonical-paths",
    ),
    (
        "claude.skill-source",
        "PreToolUse",
        Some("Edit|Write|MultiEdit|NotebookEdit|Bash"),
        "skill-source",
    ),
    ("claude.skill-commit", "Stop", None, "skill-commit"),
    (
        "claude.no-tmp",
        "PreToolUse",
        Some("Edit|Write|MultiEdit|NotebookEdit|Bash"),
        "no-tmp",
    ),
    (
        "claude.search-tools",
        "PreToolUse",
        Some("Bash"),
        "search-tools",
    ),
    ("claude.worktree", "PreToolUse", None, "worktree"),
    (
        "claude.worktree-create",
        "WorktreeCreate",
        None,
        "worktree-create",
    ),
    (
        "claude.worktree-remove",
        "WorktreeRemove",
        None,
        "worktree-remove",
    ),
    (
        "claude.session-name",
        "UserPromptSubmit",
        None,
        "session-name",
    ),
    ("claude.session-name", "SessionStart", None, "session-name"),
];

/// The settings.json group for an added hook, as one line of JSON.
fn claude_group(matcher: Option<&str>, cmd: &str) -> String {
    // Making a worktree can clone submodules and run the repo's setup.
    let timeout = if cmd.ends_with(" worktree-create") {
        1800
    } else {
        15
    };
    let hook = serde_json::json!({"type": "command", "command": cmd, "timeout": timeout});
    let hook = serde_json::to_string(&hook).expect("json");
    match matcher {
        Some(m) => format!(
            "{{\"matcher\": {}, \"hooks\": [{hook}]}}",
            serde_json::to_string(m).expect("json")
        ),
        None => format!("{{\"hooks\": [{hook}]}}"),
    }
}

/// A settings.json hook group that runs ` hook claude <sub>` (any binary
/// path, any key order or formatting).
fn is_own_group(g: &serde_json::Value, sub: &str) -> bool {
    let tail = format!(" hook claude {sub}");
    g["hooks"].as_array().is_some_and(|hs| {
        hs.iter()
            .any(|h| h["command"].as_str().is_some_and(|c| c.ends_with(&tail)))
    })
}

/// settings.json text with `group` first in `event`'s list and every other
/// copy of valet's `sub` group (old binary paths) removed. None if unchanged.
fn claude_add(text: &str, event: &str, sub: &str, group: &str) -> Result<Option<String>> {
    let mut v: serde_json::Value = serde_json::from_str(text).context("parse settings.json")?;
    let want: serde_json::Value = serde_json::from_str(group)?;
    let hooks = v
        .get_mut("hooks")
        .and_then(|h| h.as_object_mut())
        .context("settings.json has no \"hooks\" object; add `\"hooks\": {}` first")?;
    let list = hooks
        .entry(event)
        .or_insert_with(|| serde_json::Value::Array(Vec::new()))
        .as_array_mut()
        .context("hooks event is not a list")?;
    let before = list.clone();
    list.retain(|g| !is_own_group(g, sub));
    list.insert(0, want);
    if *list == before {
        return Ok(None);
    }
    Ok(Some(format!("{}\n", serde_json::to_string_pretty(&v)?)))
}

/// settings.json text without valet's group(s) for `sub`; None if absent.
fn claude_remove(text: &str, sub: &str) -> Result<Option<String>> {
    claude_remove_where(text, |g| is_own_group(g, sub))
}

fn claude_remove_where(
    text: &str,
    drop: impl Fn(&serde_json::Value) -> bool,
) -> Result<Option<String>> {
    let mut v: serde_json::Value = serde_json::from_str(text).context("parse settings.json")?;
    let mut changed = false;
    if let Some(events) = v.get_mut("hooks").and_then(|h| h.as_object_mut()) {
        for list in events.values_mut().filter_map(|l| l.as_array_mut()) {
            let n = list.len();
            list.retain(|g| !drop(g));
            changed |= list.len() != n;
        }
    }
    if !changed {
        return Ok(None);
    }
    Ok(Some(format!("{}\n", serde_json::to_string_pretty(&v)?)))
}

/// Every hook command in settings.json: (event, command).
fn claude_hooks(text: &str) -> Vec<(String, String)> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(events) = v.get("hooks").and_then(|h| h.as_object()) {
        for (event, groups) in events {
            for g in groups.as_array().into_iter().flatten() {
                for h in g
                    .get("hooks")
                    .and_then(|h| h.as_array())
                    .into_iter()
                    .flatten()
                {
                    if let Some(c) = h.get("command").and_then(|c| c.as_str()) {
                        out.push((event.clone(), c.to_owned()));
                    }
                }
            }
        }
    }
    out
}

// ---------- commands ----------

fn list() -> Result<()> {
    let state = load()?;
    let rc_ok = rc_files()?
        .iter()
        .any(|(p, _)| fs::read_to_string(p).is_ok_and(|t| t.contains(BLOCK_BEGIN)))
        || zsh_dropins()?.is_some_and(|d| d[1].0.exists());
    let settings = fs::read_to_string(claude_settings()?).unwrap_or_default();
    let git_ok = git_wired();
    println!(
        "{:<25} {:<8} {:<8} {:<18} WHAT",
        "HOOK", "ENABLED", "WIRED", "WHEN"
    );
    for h in HOOKS {
        let on = !state.disabled.iter().any(|d| d == h.id);
        let wired = match h.kind {
            Kind::Shell => rc_ok,
            Kind::Git => git_ok,
            Kind::Claude => CLAUDE_ADDED
                .iter()
                .find(|a| a.0 == h.id)
                .is_some_and(|(_, _, _, sub)| settings.contains(&format!(" hook claude {sub}\""))),
        };
        println!(
            "{:<25} {:<8} {:<8} {:<18} {}",
            h.id,
            if on { "yes" } else { "no" },
            if wired { "yes" } else { "no" },
            h.when,
            h.what
        );
    }
    let others: Vec<_> = claude_hooks(&settings)
        .into_iter()
        .filter(|(_, c)| !c.contains(" hook claude "))
        .collect();
    if !others.is_empty() {
        println!("\nowned by others (not managed): Claude Code settings.json");
        for (event, c) in others {
            let c: String = c.chars().take(70).collect();
            println!("  {event:<17} {c}");
        }
    }
    let added_ok = CLAUDE_ADDED
        .iter()
        .all(|(_, _, _, sub)| settings.contains(&format!(" hook claude {sub}\"")));
    if !(rc_ok && git_ok && added_ok) {
        println!("\nnot fully wired: run `valet hooks install` (preview with -n)");
    }
    Ok(())
}

fn install(dry: bool) -> Result<()> {
    let mut state = load()?;
    let say = |msg: String| println!("{}{msg}", if dry { "would: " } else { "" });

    // config.toml read live: link it to the valet alias's checkout when unlinked.
    if !dry
        && !crate::config::path()?.exists()
        && let Some(checkout) = crate::repos::alias_path("valet")
        && let Err(e) = crate::config::link(&checkout)
    {
        crate::warn!("cannot link config.toml: {e:#}");
    }
    // zsh: ~/.zshrc and ~/.zshenv from shell/rc/, sourcing drop-ins; other
    // shells get a block.
    crate::cmd::shell::install(dry)?;
    // Hard-coded dot-folders in $HOME -> Userland ([home] links).
    crate::cmd::home_links::install(dry)?;
    let dropins = zsh_dropins()?;
    // Drop-ins this tool wrote under an older binary name.
    for (path, _) in dropins.iter().flatten() {
        let Some(dir) = path.parent() else { continue };
        for e in fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            let ours = fs::read_to_string(&p).is_ok_and(|t| {
                t.lines().next().is_some_and(|l| {
                    l.starts_with("# Generated by ") && l.contains(" hooks install`")
                })
            });
            if ours && &p != path {
                say(format!("remove {} (older name)", p.display()));
                if !dry {
                    crate::generated::remove(&p)?;
                }
            }
        }
    }
    for (path, text) in dropins.iter().flatten() {
        let body = format!(
            "{}{text}",
            crate::generated::header(
                "#",
                &format!(
                    "{} (source: {})",
                    crate::generated::cmd("hooks install"),
                    crate::generated::source("shell/")
                ),
            )
        );
        if fs::read_to_string(path).ok().as_deref() != Some(&body) {
            say(format!("write {}", path.display()));
            if !dry {
                crate::generated::write(path, &body)?;
            }
        }
    }
    for (path, shell) in rc_files()? {
        if shell == "zsh" && dropins.is_some() {
            continue;
        }
        let text = fs::read_to_string(&path)?;
        let new = rc_install(&text, shell);
        if new != text {
            say(format!("add valet shell-init block to {}", path.display()));
            if !dry {
                write_atomic(&path, &new)?;
                if !state.installed.rc_files.contains(&path) {
                    state.installed.rc_files.push(path);
                }
            }
        }
    }

    // Git hooks.
    let dir = git_hooks_dir()?;
    let before = global_hooks_path();
    if !git_wired() {
        let value = hooks_path_value(&dir);
        say(format!(
            "link git hooks in {} to {}",
            dir.display(),
            crate::cmd::apps::launcher()?.display()
        ));
        say(format!(
            "git config --global core.hooksPath {value} (was {})",
            before.as_deref().unwrap_or("unset")
        ));
        if !dry {
            crate::cmd::githooks::install_links(&dir)?;
            let ours = |b: &str| b == value || Path::new(b) == dir;
            if !before.as_deref().is_some_and(ours) {
                state.installed.git_hooks_path_before = before.clone();
            }
            Cmd::new("git")
                .args(["config", "--global", "core.hooksPath", &value])
                .checked()?;
        }
    }

    // Global gitignore.
    if let Some(before) = crate::cmd::gitignore::install(dry)?
        && !dry
    {
        state.installed.excludes_file_before.get_or_insert(before);
    }

    // Claude Code.
    let settings = claude_settings()?;
    if let Ok(mut text) = fs::read_to_string(&settings) {
        let mut changed = false;
        for (id, event, matcher, sub) in CLAUDE_ADDED {
            let cmd = hook_cmd(&format!("hook claude {sub}"));
            if let Some(new) = claude_add(&text, event, sub, &claude_group(*matcher, &cmd))? {
                say(format!("add Claude Code {event} hook {id}"));
                text = new;
                changed = true;
            }
        }
        let canon = |p: &Path| crate::cmd::apps::canonical(p).display().to_string();
        // Settings valet owns (claude/settings.json in the valet repo).
        if let Some(checkout) = crate::repos::alias_path("valet")
            && let Some((new, keys)) = crate::cmd::claude_overlay::apply(
                &text,
                // This host's spelling of each path (symlinks resolved).
                &canon(&checkout),
                &canon(&crate::cmd::claude_dir()?),
            )?
        {
            say(format!("set Claude Code settings {}", keys.join(", ")));
            text = new;
            changed = true;
        }
        if changed && !dry {
            write_atomic(&settings, &text)?;
        }
    }

    if !dry {
        save(&state)?;
        println!(
            "installed; open a new shell (or `exec zsh`) for shell hooks. Check with `valet hooks list`."
        );
    }
    Ok(())
}

fn uninstall(dry: bool) -> Result<()> {
    let mut state = load()?;
    let say = |msg: String| println!("{}{msg}", if dry { "would: " } else { "" });
    for (path, _) in zsh_dropins()?.iter().flatten() {
        if path.exists() {
            say(format!("remove {}", path.display()));
            if !dry {
                crate::generated::remove(path)?;
            }
        }
    }
    for (path, _) in rc_files()? {
        let text = fs::read_to_string(&path)?;
        let new = rc_uninstall(&text);
        if new != text {
            say(format!("remove valet block from {}", path.display()));
            if !dry {
                write_atomic(&path, &new)?;
            }
        }
    }
    if git_wired() {
        let before = state.installed.git_hooks_path_before.clone();
        say(format!(
            "restore core.hooksPath to {}",
            before.as_deref().unwrap_or("unset")
        ));
        if !dry {
            let c = Cmd::new("git").args(["config", "--global"]);
            match &before {
                Some(b) => c.args(["core.hooksPath", b]).checked()?,
                None => c.args(["--unset", "core.hooksPath"]).checked()?,
            };
        }
    }
    if let Some(before) = state.installed.excludes_file_before.clone()
        && crate::cmd::gitignore::current() == Some(crate::cmd::gitignore::value()?)
    {
        say(format!("restore core.excludesFile to {before}"));
        if !dry {
            Cmd::new("git")
                .args(["config", "--global", "core.excludesFile", &before])
                .checked()?;
        }
    }
    if let Ok(mut text) = fs::read_to_string(claude_settings()?) {
        let mut changed = false;
        for (id, _, _, sub) in CLAUDE_ADDED {
            if let Some(new) = claude_remove(&text, sub)? {
                say(format!("remove Claude Code hook {id}"));
                text = new;
                changed = true;
            }
        }
        if changed && !dry {
            write_atomic(&claude_settings()?, &text)?;
        }
    }
    if !dry {
        state.installed = Installed::default();
        save(&state)?;
        println!("uninstalled");
    }
    Ok(())
}

/// Write via a temp file in the same dir + rename, keeping the file mode.
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path)
        .map(|m| m.permissions().mode())
        .unwrap_or(0o600);
    let tmp = path.with_extension(format!("valet-tmp-{}", std::process::id()));
    fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))
}

#[cfg(test)]
mod tests {

    use super::*;

    const RC: &str = "a\nexport EDITOR=vim\nb\n";

    #[test]
    fn rc_install_is_idempotent_and_reversible() {
        let once = rc_install(RC, "zsh");
        assert!(once.contains(BLOCK_BEGIN));
        assert!(once.starts_with(RC));
        assert!(once.contains("valet shell-init zsh"));
        assert_eq!(
            rc_install(&once, "zsh"),
            once,
            "second install changes nothing"
        );
        assert_eq!(rc_uninstall(&once), RC, "uninstall restores the original");
    }

    #[test]
    fn rc_install_keeps_other_lines() {
        let t = "  x=1\n# comment\n";
        assert_eq!(rc_uninstall(&rc_install(t, "bash")), t);
        assert!(rc_install(t, "bash").starts_with(t));
    }

    #[test]
    fn hook_cmd_names_the_binary() {
        assert_eq!(
            hook_cmd("hook claude no-tmp"),
            format!("{} hook claude no-tmp", crate::generated::name())
        );
    }

    #[test]
    fn claude_add_inserts_replaces_and_removes() {
        let s = "{\n  \"hooks\": {\n    \"Stop\": [\n      {\"hooks\": [{\"command\": \"a.sh\"}]}\n    ]\n  },\n  \"z\": 1\n}\n";
        let stop = |t: &str| -> Vec<String> {
            let v: serde_json::Value = serde_json::from_str(t).unwrap();
            v["hooks"]["Stop"]
                .as_array()
                .unwrap()
                .iter()
                .map(|g| g["hooks"][0]["command"].as_str().unwrap().to_owned())
                .collect()
        };
        let g = claude_group(None, "/x/dev hook claude skill-commit");
        let added = claude_add(s, "Stop", "skill-commit", &g).unwrap().unwrap();
        assert_eq!(stop(&added), ["/x/dev hook claude skill-commit", "a.sh"]);
        assert!(
            claude_add(&added, "Stop", "skill-commit", &g)
                .unwrap()
                .is_none()
        );
        // Another binary path, in another key order: replaced, not duplicated.
        let old = added.replace(
            r#""command": "/x/dev hook claude skill-commit""#,
            r#""type": "command", "command": "/old/dev hook claude skill-commit""#,
        );
        let g2 = claude_group(None, "/y/valet hook claude skill-commit");
        let moved = claude_add(&old, "Stop", "skill-commit", &g2)
            .unwrap()
            .unwrap();
        assert_eq!(stop(&moved), ["/y/valet hook claude skill-commit", "a.sh"]);
        let back = claude_remove(&moved, "skill-commit").unwrap().unwrap();
        assert_eq!(stop(&back), ["a.sh"]);
        // A missing event list is created; a matcher is kept.
        let g3 = claude_group(Some("Edit|Bash"), "/x/dev hook claude skill-source");
        let pre = claude_add(s, "PreToolUse", "skill-source", &g3)
            .unwrap()
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&pre).unwrap();
        assert_eq!(v["hooks"]["PreToolUse"][0]["matcher"], "Edit|Bash");
        assert!(claude_remove(&pre, "skill-source").unwrap().is_some());
        assert!(claude_add("{}", "Stop", "skill-commit", &g).is_err());
    }

    #[test]
    fn claude_hooks_lists_commands() {
        let s = r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"a.sh"}]}],"PreToolUse":[{"hooks":[{"command":"b"}]}]}}"#;
        let mut v = claude_hooks(s);
        v.sort();
        assert_eq!(
            v,
            vec![
                ("PreToolUse".into(), "b".into()),
                ("Stop".into(), "a.sh".into())
            ]
        );
    }

    #[test]
    fn registry_ids_are_unique_and_documented() {
        for (i, h) in HOOKS.iter().enumerate() {
            assert!(HOOKS[i + 1..].iter().all(|o| o.id != h.id), "{}", h.id);
            assert!(LONG_ABOUT.contains(h.id), "{} missing from help", h.id);
        }
    }
}
