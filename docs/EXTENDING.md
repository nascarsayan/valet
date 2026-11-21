# Extending valet

Where each kind of change goes. Follow the rules in [AGENTS.md](../AGENTS.md)
and run the checks there after every change. See
[ARCHITECTURE.md](ARCHITECTURE.md) for how the parts fit.

## A command

1. Create `src/cmd/<name>.rs` with `pub struct Args` (clap, usually with a
   `#[command(subcommand)]` enum) and `pub fn run(a: Args) -> anyhow::Result<()>`.
   For a group of subcommands, use a folder `src/cmd/<name>/mod.rs`.
2. Add `pub mod <name>;` to `src/cmd/mod.rs`.
3. In `src/main.rs`, add a variant to `Command` with a doc comment (it becomes
   `--help`) and a dispatch arm in `main`. Use `#[command(hide = true)]` for
   commands only hooks or scripts call.
4. To add several top-level commands from one module, give it a
   `#[derive(clap::Subcommand)] pub enum Cmd` and flatten it into `Command`
   (`#[command(flatten)]`), as `cleanup::Cmd` and `githooks::Cmd` do.
5. Configuration goes in a new section of `config.toml`; read it with
   `crate::config::section::<T>("<section>")` into a serde struct.
6. Shell code the command needs (a function, a completion helper) goes in a
   `pub const SHELL_ZSH` / `SHELL_BASH` in the module, listed in
   `shell_snippets()` in `src/cmd/mod.rs`.
7. Unit tests in the module. The test `cli_definition_is_valid` in
   `src/main.rs` checks the clap definition.

Run programs with `crate::proc::Cmd` (argument array, `.timeout(..)`), HTTP
with `crate::net`, managed CLIs with `crate::tools::resolve("<tool>")`.
Write generated files with `crate::generated::write` and a
`generated::header`.

## A daemon part

A long-running process supervised by `valet daemon`.

1. In `src/cmd/daemon.rs`, add a `Part { name, what, cmd }` to `PARTS`.
   `cmd` returns the `proc::Cmd` to start; most parts run the binary itself
   via `self_cmd(&["daemon", "part", "<name>"])`.
2. For an in-process part, add a match arm in `Sub::Part` in `daemon::run`
   that calls your loop function (see `repos_part`, `job_part`, and
   `cc::herdr::tabs_part`). The loop never returns in normal operation; if it
   exits or errors, the supervisor restarts it with backoff.
3. Log with `crate::info!` / `crate::warn!`; the supervisor adds `[<name>]`.
4. The command that needs the part turns it on with
   `crate::cmd::daemon::ensure("<name>")` and off with
   `crate::cmd::daemon::set_part("<name>", false)`. Users can also run
   `valet daemon on|off <name>`.
5. `valet daemon install` (also run by `valet upgrade`) rewrites the unit; the
   unit's description lists the parts.

## A daily job

1. In `src/cmd/job/mod.rs`, add `("<name>", "<what it does>")` to `JOBS`.
   The position is the run order.
2. Add a match arm in `run_job` that returns `Result<Outcome>`:
   `Outcome::Done(lines)` with one short report line per thing done. Honour
   `dry` (`valet job run -n`): report what would happen, change nothing.
3. Larger jobs go in their own file under `src/cmd/job/` (see `repos.rs`,
   `upstream.rs`) with `mod <name>;` in `job/mod.rs`.
4. Update the test of `plan` in `job/mod.rs` if it lists every job.

Jobs never force-push or discard local work. A job that fails is reported in
`valet job status`; the run continues with the next job.

For one-off background work that should outlive the shell, use an ad-hoc
job (`src/cmd/job/adhoc.rs`): a transient systemd unit
`valet-job-<id>.service` with its own log.

## A hook

Every hook has an entry in `HOOKS` in `src/cmd/hooks.rs`
(`id`, `kind`, `when`, `what`) and a line in `LONG_ABOUT` there. The id is
what `valet hooks enable|disable` takes. Hook bodies check
`crate::cmd::hooks::enabled("<id>")` before doing anything.

### Git hook

1. Body: a function in `src/cmd/githooks/hooks.rs` (or a new file in
   `src/cmd/githooks/`), a variant in `HookCmd` and a match arm in
   `githooks::run` in `src/cmd/githooks/mod.rs`.
2. For a new git hook name (for example `commit-msg`), add it to
   `HOOK_NAMES` in `src/main.rs` (argv0 dispatch) and to `HOOKS` in
   `src/cmd/githooks/hooks.rs` (the symlinks `valet hooks install` makes).
3. Run the repo's own hook too, if your hook replaces one git would have run
   (see how `pre_commit` calls `.git/hooks/pre-commit`).

### Claude Code hook

1. Body: a new file in `src/cmd/githooks/` with a function that reads the
   hook JSON from stdin. A PreToolUse guard prints a decision only when it
   denies and always exits 0, so a bug never blocks the session. Copy the
   shape of `no_tmp.rs` or `search_tools.rs`.
2. A variant in `ClaudeCmd` and a match arm in `githooks::run`
   (`src/cmd/githooks/mod.rs`).
3. Wiring: add `(id, event, matcher, subcommand)` to `CLAUDE_ADDED` in
   `src/cmd/hooks.rs`. `valet hooks install` adds a group to Claude's
   `settings.json` that runs `valet hook claude <subcommand>`;
   `uninstall` removes it.

### Shell hook

1. The shell side is a snippet in `src/cmd/hooks.rs` (see `CHPWD_ZSH`,
   `SECRETS_ZSH`), added in `shell_snippet()` when the hook is enabled.
   Keep it fast: it runs at every prompt or `cd`.
2. Snippets call back into the binary through `valet hooks run <event>`;
   add the event to the `Sub::Run` match in `hooks::run`. For `chpwd`,
   add your step next to the `shell.env` one there.
3. Snippets reach shells through the shell-init cache; `valet hooks install`
   rewrites it.

## A managed tool

A CLI valet calls that may be missing on a fresh machine.

1. Add a `Tool` to `TOOLS` in `src/tools.rs`: name, exact version, and one
   `Asset` per architecture (`x86_64`, `aarch64`) with the URL, SHA-256 and
   the binary's path inside the archive (`member`, `None` for a bare binary).
2. Take the digest from GitHub:
   `gh api repos/<owner>/<repo>/releases/tags/<tag> --jq '.assets[]|[.name,.digest]'`.
   Never guess or compute it from an unverified download.
3. `valet tools install --force <name>` to verify.
4. Call it with `crate::tools::resolve("<name>")`, which uses PATH first,
   then the managed dir, then downloads.

A tool the system must provide (git, curl) goes in `PREREQS` in
`src/cmd/doctor.rs` instead.

## An app

A tool for the user (on PATH in every shell), installed into Userland.

1. Add `[apps.<App>]` to `config.toml`: `tool` (mise id such as
   `aqua:owner/repo` or `github:owner/repo`), `version` (exact), `commands`
   (names to link into `Commands/<Arch>/`). Optional keys, documented on
   `struct App` in `src/cmd/apps.rs`: `env` (placeholders `{Settings}`,
   `{Data}`, `{Caches}`, `{State}`, `{Logs}`, `{Arch}`, `{App}`, `{Host}`),
   `xdg`, `arches`, `args`, `via`, `system`, `files`, and mise `options`
   (for example `options.platforms.linux-x64.asset_pattern`).
2. `valet apps install` (locks checksums for both architectures, installs,
   links). `valet apps list` shows what is linked.
3. Point the tool's state into its app folder with `env`, so nothing lands in
   `$HOME`. If the tool hard-codes a dot-folder, add it to `[home.links]`.

## A skill

Skills are folders with a `SKILL.md`, linked into Claude's `skills` folder.

1. Put the folder in a git repo valet manages (listed in `[repos]`), or
   under `~/`.
2. Add `<name> = "<host>/<owner>/<repo>/<dir>"` (or `"~/..."`) to `[skills]`
   in `config.toml`.
3. `valet skills apply` (`-n` to preview). Edits to a linked `SKILL.md` are
   live; adding or removing one needs `apply`.

Skills call `valet ...` for anything scripted instead of shipping scripts.
The skill for valet itself is [`skill/SKILL.md`](../skill/SKILL.md).

## A config section

Each module owns one top-level section of `config.toml` and reads it with
`crate::config::section`. Comment the section in `config.toml` itself: what
each key does, placeholders, defaults. `valet config check` parses the file.
Keep ids, tokens and host names out of it; read them from the environment or
`valet vars`.
