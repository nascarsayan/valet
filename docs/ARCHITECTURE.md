# Architecture

valet is one binary. Every feature is a module under `src/cmd/`, reached
through one clap enum in `src/main.rs`. State on disk is either the source
in this repo or files generated from it.

## Module map

Core (`src/`):

| File | What |
|---|---|
| `main.rs` | `Command` enum, dispatch, argv0 rewrite for git hooks, `--help-all` |
| `config.rs` | Reads `config.toml` (live link, else the copy built in); `section`, `keys_of` |
| `proc.rs` | `Cmd` builder and `output`: run programs with argument arrays and timeouts |
| `net.rs` | HTTP through `curl`, request on stdin so tokens stay out of argv |
| `tools.rs` | Managed external CLIs (`TOOLS`): find on PATH or download a pinned, digest-checked release |
| `repos.rs` | Repo layout under `<Userland>/Repos`: bare `.git`, `tree/<branch>`, fold/recreate, aliases, reconcile |
| `generated.rs` | Write generated files with a header, atomically, then lock them |
| `template.rs` | Minimal Jinja subset for `shell/rc/*.j2` |
| `log.rs` | `info!`/`warn!` timestamped stderr lines |

Commands (`src/cmd/`):

| Module | What |
|---|---|
| `repos.rs`, `repo.rs` | `valet repos`, `valet cd` and the `wt` picker |
| `git.rs`, `pr.rs`, `worktree/` | `valet git pr` (prompt cache), `valet git review` |
| `apps.rs` | Userland root, `[apps.*]` manifest, mise install, `Commands/<Arch>` links, launchers |
| `tools.rs`, `doctor.rs` | `valet tools`, `valet doctor` (`PREREQS`) |
| `hooks.rs` | `valet hooks`: hook registry (`HOOKS`), enable/disable state, install/uninstall wiring |
| `githooks/` | `valet hook ...` (hidden): the git and Claude Code hook bodies |
| `shell.rs` | `valet shell-init`, rendering of the zsh/bash rc files |
| `envrules.rs`, `run.rs` | `[env]` rules for the `cd` hook; `valet run` |
| `home_links.rs`, `gitignore.rs`, `claude_overlay.rs` | `[home]` links, `[git] ignore`, Claude `settings.json` merge |
| `skills.rs` | `valet skills`: `[skills]` to links in `~/.claude/skills` |
| `vars.rs` | `valet vars`: plain and gpg-encrypted values |
| `daemon.rs` | `valet daemon`: one user unit, supervisor, `PARTS` |
| `job/` | `valet job`: daily `JOBS`, ad-hoc jobs (`adhoc.rs`) as transient units |
| `systemd.rs` | User unit dir, install-on-change, `systemctl --user` |
| `cleanup/` | `valet clean` categories |
| `cc/` | `valet cc` / `valet slack`: Slack mirroring, herdr pane backend, `tabs` part |
| `tpl.rs` | `valet tpl`, `valet sandbox` from `templates/project/` |
| `upgrade.rs` | `valet upgrade` |
| `config.rs` | `valet config` |

`src/cmd/mod.rs` also holds shared helpers: `home()`, `claude_dir()`,
`codex_dir()`, `expand()` (a `<host>/<owner>/<repo>/<dir>` source to a path),
and `shell_snippets()`.

## Data flow

```text
config.toml, shell/, claude/settings.json, templates/, src/   (this repo)
        |
        |  valet upgrade: pull, fmt/clippy/test, build, install binary,
        |                 link Settings/config.toml -> repo, run hooks install,
        |                 restart the daemon
        v
valet hooks install  -> ~/.zshenv, ~/.bashrc, ZDOTDIR files, shell-init cache,
                        [home] links, global gitignore, git core.hooksPath,
                        Claude settings.json hooks + overlay
valet skills apply   -> one link per skill in Claude's skills folder
valet apps install   -> Applications/<App>/Binaries/<Arch>, Commands/<Arch>,
                        Data/Generated/environment.sh
valet daemon install -> valet-daemon.service (user unit)
```

`config.toml` is read live, so most edits apply at once. Generated files are
locked; rerun the command named in their header after changing the source.

## Userland

The root is computed from the binary's own path,
`<Userland>/Applications/Valet/Binaries/<Arch>/valet` (`cmd::apps::root()`).
A build outside a Userland uses `$USERLAND`, else `~/ws/Userland`.
The home folder and Userland may be shared between hosts of different
architectures, so per-host state goes in `<host>` subfolders and binaries in
`<Arch>` subfolders (`Amd64`, `Arm64`). `valet upgrade` on Amd64 also
cross-builds the Arm64 binary.

`<Userland>/Scripts/valet` is a launcher that runs the binary for the
current architecture and keeps `argv[0]`. Files read by every host (git
hook links, Claude hook commands) point at it or at `valet` on PATH, never
at one host's path.

## Hook dispatch

All hooks run the same binary.

- **Git**: `valet hooks install` makes a folder of symlinks named
  `pre-commit`, `pre-push`, `post-checkout`, `post-commit`, `post-merge`
  pointing at the launcher, and sets git's global `core.hooksPath` to it.
  When git runs `pre-push ARGS`, `main.rs` sees argv0 `pre-push` (in
  `HOOK_NAMES`) and rewrites it to `valet hook pre-push ARGS`. The pre-commit
  hook then runs the repo's own `.git/hooks/pre-commit`.
- **Claude Code**: `install` adds groups to `settings.json` that run
  `valet hook claude <sub>` for an event and matcher (`CLAUDE_ADDED` in
  `src/cmd/hooks.rs`). The hook reads Claude's JSON on stdin and prints a
  decision only when it has one.
- **Shell**: `valet shell-init` emits shell functions. The `cd` hook calls
  `valet hooks run chpwd` and evals its output (env rule changes); the
  secrets hook evals `valet vars env --if-unlocked` when a marker changes.

Each body checks `hooks::enabled(id)` first, so `valet hooks disable <id>`
takes effect on the next run without rewiring. State:
`$XDG_CONFIG_HOME/valet/hooks.json`.

## Daemon supervisor

`valet daemon install` writes one systemd user unit, `valet-daemon.service`.
Its `ExecStart` goes through zsh so it picks up this host's `$USERLAND` and
PATH, then runs `valet daemon run`.

`daemon run` is a supervisor (`Supervisor` in `src/cmd/daemon.rs`). Every
second it reads `<Userland>/Applications/Valet/State/daemon/<host>/parts.json`
(`{"cc": true, ...}`) and, for each entry in `PARTS`:

- starts a part that is on and not running, as a child process in its own
  process group (usually `valet daemon part <name>`);
- prefixes each output line with `[<part>]` in the journal;
- stops a part that was turned off (SIGTERM, SIGKILL after 10 s);
- restarts a part that exited, with backoff from 1 s doubling to 5 min,
  reset after 60 s of healthy running.

A `parts.json` that does not parse, or names an unknown part, stops every
part until it is fixed. Commands that need a part turn it on
(`daemon::ensure(part)`, which also installs the unit), for example `valet job enable` turns on `job`.

Current parts: `cc` (Slack mirroring), `tabs` (herdr tab labels), `repos`
(reconcile when repo folders change), `job` (start the daily run at about
03:30 local time).

## Jobs

The daily run executes `JOBS` in `src/cmd/job/mod.rs` in order, as an ad-hoc
job (transient unit `valet-job-<id>.service`) so an `upgrade` inside it can
restart the daemon. One run at a time (a lock file). Each job's last result
goes to `<valet Data>/job/state.json`, every step to `job/job.log`.
