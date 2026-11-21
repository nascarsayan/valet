---
name: valet
description: The `valet` CLI (Rust) that runs this machine's developer setup. Use for anything it covers: git checkouts and worktrees, PRs and review threads, shell variables and secrets, per-directory environment, Userland apps and tools, hooks, agent skills, background jobs, disk cleanup, Claude/Slack mirroring. Also use when changing valet itself or adding, updating, or removing a skill.
---

# valet

Use valet's commands; don't redo by hand what they do. `valet <cmd> --help` is the reference (`--help-all` shows hidden commands). If valet lacks a capability or gets one wrong, change valet instead of writing a workaround or a skill.

## What it covers

- `repos`: checkouts under `$USERLAND/Repos/<host>/<owner>/<name>`, one worktree per branch at `<repo>/tree/<branch>`. `path <alias|repo> [branch|PR]`, `path -b <new>` (off the latest default branch), `sync`, `fold`, `take`, `list`, `pick`. Never hardcode a checkout path. `cd`: print a place's path (the `wt` shell function cds there).
- `git`: `pr` for the current branch; `review` worktrees and threads.
- `vars`: values exported into every shell, plain or secret (`pass`). `run`: a command with the environment `cd` gives its directory (config.toml `[env]`).
- `apps`, `tools`, `doctor`: Userland apps installed per architecture from pinned versions; managed CLI tools; prerequisite check.
- `hooks`: list, enable, disable, install and uninstall every shell, git and Claude Code hook valet runs.
- `skills`: `~/.claude/skills` generated from config.toml `[skills]` (`list`, `apply`).
- `config`: valet's one config file (`path`, `edit`, `check`, `show`).
- `tpl`: project folders from templates.
- `job`, `daemon`: daily background jobs and the one user unit that supervises long-running parts.
- `cleanup`: report, and with `--run` delete, regenerable files.
- `cc`, `slack`: optional mirroring of Claude sessions to a Slack bot DM, and sending markdown there (configured in config.toml).
- `upgrade`: pull, check, rebuild and replace valet.

Throw-away files go in `$USERLAND/Scratch/<task>/`, which is emptied a few days after the last change. Do not write under `/tmp`.

## Change valet

Repo: `valet repos path valet`; read its `AGENTS.md` first. Rust only, `proc::Cmd` with argument arrays and timeouts, no new crate without asking. Make the smallest change, test it with `target/release/valet ...`, then `valet upgrade` (fmt, clippy, tests, install). Run the changed command for real, update `README.md`, commit, and push only when the user says so.

## Skills

The manifest is `[skills]` in the valet repo's `config.toml`. `valet skills apply` syncs `~/.claude/skills`, and `valet skills list` shows every skill as `ok`. Edit a skill at its source repo (`valet skills list` shows it), never through `~/.claude/skills`. Skills in a team repo (an owner in `[repos] team`): branch, commit, push, PR. Keep skills thin: point at valet commands, and don't describe valet's internal files.
