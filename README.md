# valet

One Rust CLI for a developer's daily machine. It replaces a pile of shell
functions and scripts with `valet <command> <subcommand> [flags]`.

It covers:

- **Repos and worktrees**: every checkout under `<Userland>/Repos/<host>/<owner>/<name>/`
  as a bare `.git` plus one `tree/<branch>/` per branch; idle worktrees fold
  away and come back on use; `wt` jumps to any of them.
- **Agent hooks** for Claude Code: guard rails (search tools,
  `/tmp`, path spelling, skill sources), worktree create/remove, session names.
- **Git hooks**: gitleaks on commit, invisible-character check, no rewriting
  of published PR branches, git lfs.
- **herdr tabs**: tab labels follow the Claude session or ssh host in each tab.
- **Background daemon**: one systemd user unit that supervises long-running
  parts, plus daily jobs (self-upgrade, repo sync, tool pin report, cleanup).
- **Cleanup**: `valet clean` reports, and with `--run` deletes, regenerable files.
- **Userland**: one folder per app, per CPU architecture, installed by mise
  from pinned versions.
- **Shell integration**: zsh/bash config rendered from templates, completions,
  per-directory environment rules.
- **Vars and secrets**: values exported into every shell; secrets stored
  gpg-encrypted.
- **Skills**: `~/.claude/skills` generated from a list in `config.toml`.
- **Self-upgrade**: `valet upgrade` pulls, checks, rebuilds and replaces itself.
- **Slack mirroring** (optional): mirror running Claude sessions to a Slack
  bot DM and answer them from there.

Linux only (systemd user units, `/proc`).

## Install

Needs a Rust toolchain, `git`, `curl`, `sha256sum`, `tar`, `unzip`, and
[mise](https://mise.jdx.dev) for `valet apps`.

```sh
cargo build --release
mkdir -p ~/.local/bin && cp target/release/valet ~/.local/bin/   # any dir on PATH
eval "$(valet shell-init zsh)"     # or bash; try it in the current shell first
valet doctor                       # prerequisites and managed tools
valet hooks install -n             # what install would change
valet hooks install                # wire shell, git and Claude Code hooks
```

After the first install, `valet upgrade` rebuilds from this checkout and puts
the binary at `<Userland>/Applications/Valet/Binaries/<Arch>/valet`, linked
from `<Userland>/Commands/<Arch>/`.

**Read `valet hooks install -n` before you run it.** It writes `~/.zshenv`,
`~/.bashrc`, `~/.bash_profile` and `~/.zprofile` from [`shell/rc/`](shell/rc/),
turns the dot-folders in `[home.links]` into links into Userland, sets git's
global `core.hooksPath` and `core.excludesFile`, and merges
[`claude/settings.json`](claude/settings.json) into Claude Code's settings.
`valet hooks uninstall` reverts the hook wiring.

Files valet generates start with a header naming the command that regenerates
them and are locked (`chattr +i` where possible, else mode 0444). Change the
source in this repo and rerun the command.

## Userland

`<Userland>` is computed from where the binary is installed
(`<Userland>/Applications/Valet/Binaries/<Arch>/valet`). A binary outside a
Userland uses `$USERLAND`, else `~/ws/Userland`.

```text
<Userland>/
  Applications/<App>/   Binaries/<Arch>/ Settings/ Data/ Caches/ State/ Logs/
  Commands/<Arch>/      links and launchers; first on PATH
  Scripts/              architecture-independent launchers (valet for this arch)
  Repos/<host>/<owner>/<name>/   .git (bare) + tree/<branch>/
  DerivedData/<owner>/<repo>/    build output shared by a repo's worktrees
  Library/              targets of [home.links] (Caches, Preferences, ...)
  Scratch/<task>/       throw-away files; deleted 2 days after last change
```

An app is self-contained: removing its folder removes it. Folders other than
`Binaries` exist only when used. valet's own files live in
`Applications/Valet/` (`Settings/config.toml`, `Data/Vars`, `Data/Generated`,
`State/daemon/<host>/parts.json`).

## config.toml

One file, [`config.toml`](config.toml), read live through
`<Userland>/Applications/Valet/Settings/config.toml` (a link `valet upgrade`
makes). A copy is built into the binary for a fresh machine.
`valet config edit` opens it and checks it when the editor closes.

| Section | What |
|---|---|
| `[repos]` | Repos to clone, aliases (`valet repos path <alias>`), `owned` owners the daily job commits and pushes for, files copied into new worktrees |
| `[env]` | Shell environment rules by directory, repo and host, applied on every `cd` |
| `[git]` | Global gitignore patterns |
| `[home]` | Dot-folders and files in `$HOME` turned into links into Userland |
| `[skills]` | Agent skills: `name = "<host>/<owner>/<repo>/<dir>"` or `~/...` |
| `[hosts]` | A host per extra architecture (`Arm64 = "<host>"`) for `valet apps install --arch` over ssh |
| `[settings]` | mise settings |
| `[apps.<App>]` | Userland apps: mise `tool`, `version`, `commands`, `env`, ... |

## Commands

`valet --help` lists them; `valet <command> --help` has details;
`valet --help-all` also shows internal commands (hook entry points, `cd`,
`shell-init`).

| Command | What |
|---|---|
| `wt [query]` (shell function) | Jump to a repo, worktree or alias (`valet cd` prints the path) |
| `valet repos` | `sync`, `list`, `pick`, `path`, `fold`, `take`, `narrow`, `gc` |
| `valet git` | `pr` (current branch's PR, cached for the prompt), `review` (PR review threads) |
| `valet apps` | `list`, `install`, `link`, `render`, `update` |
| `valet tools` | Managed external CLIs: `install`, `path` |
| `valet hooks` | `list`, `enable`, `disable`, `install`, `uninstall` |
| `valet daemon` | `install`, `status`, `on`/`off PART`, `restart`, `logs` |
| `valet job` | Daily jobs and ad-hoc background jobs: `list`, `status`, `run`, `enable`, `disable`, `log`, `stop` |
| `valet clean` | Report (or `--run` delete) builds, caches, core dumps, idle worktrees, old app versions |
| `valet vars` | `ls`, `set`, `get`, `mv`, `rm`, `exec`, `env`, `lock`, `unlock` |
| `valet skills` | `list`, `apply` |
| `valet tpl` / `valet sandbox` | Project folders from [`templates/project/`](templates/project/) |
| `valet run -- CMD` | Run CMD with the environment the `cd` hook would give this directory |
| `valet cc` / `valet slack` | Slack mirroring of Claude sessions; send markdown to the bot DM |
| `valet config` | `path`, `edit`, `check`, `show` |
| `valet upgrade` | Pull, fmt/clippy/test, build, install, restart the daemon |
| `valet doctor` | Check prerequisites, list managed tools |

## Vars and secrets

One store, `<Userland>/Applications/Valet/Data/Vars` (0700).
`<group>/<NAME>.gpg` is a secret (encrypted with `gpg` to the key ids in
`recipients`); `<group>/<NAME>.txt` is a plain value. The `shell.secrets`
hook exports them at shell start. Secrets are read from a hidden prompt or
stdin, never from arguments.

```sh
valet vars set acme/ACME_TOKEN                       # secret: prompt or stdin
valet vars set --plain acme/ACME_EMAIL alice@example.com
valet vars exec TOKEN=ACME_TOKEN -- some-tool         # TOKEN set in the child only
```

## Slack mirroring (optional)

Off until you turn it on. Needs a Slack bot token in `SLACK_BOT_TOKEN` and
the DM channel id in `SLACK_CHANNEL`, from the environment or `valet vars`.
`valet cc afk on` mirrors running Claude sessions to that DM; replies are
typed into the session's herdr or tmux pane.

## Fork it

valet is meant to be forked and changed with a coding agent.

1. Fork, clone, and point `[repos.aliases] valet` and the `valet` entry in
   `[skills]` at your fork.
2. Edit `config.toml`: your repos, owners, apps, skills, env rules.
3. Remove commands you do not want: delete the module, its `Command` variant
   and its dispatch arm.
4. Read [AGENTS.md](AGENTS.md) (the rules) and
   [docs/EXTENDING.md](docs/EXTENDING.md) (where each kind of change goes).
   [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) explains how the parts fit.

[`skill/SKILL.md`](skill/SKILL.md) is the agent skill for working on valet
and on skills.
