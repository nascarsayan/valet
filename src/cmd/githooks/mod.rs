//! Git hooks and agent (Claude Code) hooks:
//! `valet hook`, run through `valet hooks install`.
//!
//! Git runs the hooks through a directory of symlinks to the `valet` binary
//! (`valet hooks install` creates it); `main` rewrites argv0 `pre-push` into
//! `valet hook pre-push`. `valet hook` is hidden from `--help`: git and Claude
//! Code call it, people use `valet hooks`.

mod ascii;
mod canonical_paths;
/// Parse config.toml `[paths]` (canonical-paths hook aliases); for `crate::config::check`.
pub use canonical_paths::validate as validate_paths;
mod hooks;
mod no_tmp;
mod search_tools;
pub(crate) mod session_name;
mod skill_source;
mod unicode_names;
mod worktree;

use anyhow::Result;
use std::ffi::OsString;
use std::io::Write;
use std::process::ExitStatus;

pub const SHELL_ZSH: &str = "";
pub const SHELL_BASH: &str = "";

#[derive(clap::Subcommand)]
pub enum Cmd {
    /// Git hooks (run by git through `valet hooks install` symlinks) and agent hooks.
    #[command(subcommand, hide = true)]
    Hook(HookCmd),
}

/// Arguments git passes to a hook, forwarded unchanged.
#[derive(clap::Args)]
pub struct HookArgs {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<OsString>,
}

#[derive(clap::Subcommand)]
pub enum HookCmd {
    /// Scan staged changes with gitleaks (--redact), then run the repo's own
    /// .git/hooks/pre-commit.
    ///
    /// Env: GITLEAKS_BIN (default: gitleaks on PATH), GITLEAKS_CONFIG
    /// (default: gitleaks.toml from the valet repo; ignored when the repo has its
    /// own .gitleaks.toml), GLOBAL_GIT_HOOKS_DIR (default: valet's git hooks dir).
    PreCommit(HookArgs),
    /// Refuse to force-push or delete a branch that heads a published
    /// (open, non-draft) GitHub PR, then run `git lfs pre-push`.
    ///
    /// Fails closed: if the PR lookup (gh, 30s timeout) fails, the push is refused.
    PrePush(HookArgs),
    /// Run `git lfs post-checkout`.
    PostCheckout(HookArgs),
    /// Run `git lfs post-commit`.
    PostCommit(HookArgs),
    /// Run `git lfs post-merge`.
    PostMerge(HookArgs),
    /// Claude Code hooks.
    #[command(subcommand)]
    Claude(ClaudeCmd),
}

#[derive(clap::Subcommand)]
pub enum ClaudeCmd {
    /// PreToolUse hook: deny file-tool paths and Bash commands that spell
    /// this host's folders another way (home-dir links to the ws folder,
    /// config.toml [[paths.alias]] prefixes); the reason names this host's
    /// spelling.
    ///
    /// Reads the hook JSON on stdin; prints a decision only when it denies;
    /// always exits 0.
    CanonicalPaths,
    /// PreToolUse hook: keep skill edits in their git source. Deny edits
    /// through ~/.claude/skills links, in the org sync's clone, in third-party
    /// skill sources, and in team skill repos on their default branch; note
    /// the repo and branch on own and team sources.
    ///
    /// Reads the hook JSON on stdin; prints a decision only when it has one;
    /// always exits 0.
    SkillSource,
    /// Stop hook: block the stop once while own or team skill sources have
    /// uncommitted changes to files this session's transcript names.
    SkillCommit,
    /// PreToolUse hook: deny creating files or directories under /tmp or
    /// /var/tmp (file tools, and Bash that writes there, cds there, or runs
    /// mktemp without a directory elsewhere). Reading /tmp stays allowed.
    ///
    /// Reads the hook JSON on stdin; prints a decision only when it denies;
    /// always exits 0.
    NoTmp,
    /// PreToolUse hook (Bash): deny grep (use rg), find (use fd), and fd/rg
    /// walks outside the ws folder, unless FIND_REASON is set.
    ///
    /// Reads the hook JSON on stdin; prints a decision only when it denies;
    /// always exits 0.
    SearchTools,
    /// PreToolUse hook: recreate a folded worktree the session's cwd or a
    /// file-tool path points into, and mark it active. Prints nothing.
    Worktree,
    /// WorktreeCreate hook (`claude --worktree`): make branch `$USER/<name>`
    /// off the session's HEAD at `<repo>/tree/$USER/<name>` in a managed repo,
    /// else `git worktree add <top>/.claude/worktrees/<name>`.
    ///
    /// Reads the hook JSON on stdin; prints only the worktree's absolute
    /// path; exits non-zero when it cannot make one.
    WorktreeCreate,
    /// WorktreeRemove hook: fold a managed worktree (the branch stays), else
    /// `git worktree remove` (never --force). Exits non-zero when refused.
    WorktreeRemove,
    /// UserPromptSubmit and SessionStart hook: kebab-case session title from
    /// Claude's ai-title until the user renames it (then only normalized);
    /// in herdr, the tab label is the title in PascalCase (15 chars max).
    ///
    /// Reads the hook JSON on stdin; prints sessionTitle JSON only when the
    /// title changes; always exits 0.
    SessionName,
}

/// Used by `valet hooks install`.
pub fn install_links(dir: &std::path::Path) -> Result<()> {
    hooks::install_dir(dir.to_path_buf())
}

/// `valet hooks disable` switch; disabled pre-push still runs git lfs.
fn on(id: &str) -> bool {
    crate::cmd::hooks::enabled(id)
}

pub fn run(cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::Hook(h) => match h {
            HookCmd::PreCommit(_) if !on("git.pre-commit") => Ok(()),
            HookCmd::PreCommit(a) => hooks::pre_commit(a.args),
            HookCmd::PrePush(a) if !on("git.pre-push") => hooks::lfs("pre-push", a.args),
            HookCmd::PrePush(a) => hooks::pre_push(a.args),
            HookCmd::PostCheckout(_) if !on("git.post-checkout") => Ok(()),
            HookCmd::PostCheckout(a) => hooks::lfs("post-checkout", a.args),
            HookCmd::PostCommit(_) if !on("git.post-commit") => Ok(()),
            HookCmd::PostCommit(a) => hooks::lfs("post-commit", a.args),
            HookCmd::PostMerge(_) if !on("git.post-merge") => Ok(()),
            HookCmd::PostMerge(a) => hooks::lfs("post-merge", a.args),
            HookCmd::Claude(ClaudeCmd::CanonicalPaths) if !on("claude.canonical-paths") => Ok(()),
            HookCmd::Claude(ClaudeCmd::CanonicalPaths) => {
                canonical_paths::pre_tool_use();
                Ok(())
            }
            HookCmd::Claude(ClaudeCmd::SkillSource) if !on("claude.skill-source") => Ok(()),
            HookCmd::Claude(ClaudeCmd::SkillSource) => {
                skill_source::pre_tool_use();
                Ok(())
            }
            HookCmd::Claude(ClaudeCmd::SkillCommit) if !on("claude.skill-commit") => Ok(()),
            HookCmd::Claude(ClaudeCmd::SkillCommit) => {
                skill_source::stop();
                Ok(())
            }
            HookCmd::Claude(ClaudeCmd::NoTmp) if !on("claude.no-tmp") => Ok(()),
            HookCmd::Claude(ClaudeCmd::NoTmp) => {
                no_tmp::pre_tool_use();
                Ok(())
            }
            HookCmd::Claude(ClaudeCmd::SearchTools) if !on("claude.search-tools") => Ok(()),
            HookCmd::Claude(ClaudeCmd::SearchTools) => {
                search_tools::pre_tool_use();
                Ok(())
            }
            HookCmd::Claude(ClaudeCmd::SessionName) if !on("claude.session-name") => Ok(()),
            HookCmd::Claude(ClaudeCmd::SessionName) => {
                session_name::run();
                Ok(())
            }
            HookCmd::Claude(ClaudeCmd::Worktree) if !on("claude.worktree") => Ok(()),
            HookCmd::Claude(ClaudeCmd::Worktree) => {
                worktree::pre_tool_use();
                Ok(())
            }
            // Disabled: Claude still needs a worktree, made the plain-git way.
            HookCmd::Claude(ClaudeCmd::WorktreeCreate) => {
                exit(worktree::create(on("claude.worktree-create")))
            }
            HookCmd::Claude(ClaudeCmd::WorktreeRemove) => {
                exit(worktree::remove(on("claude.worktree-remove")))
            }
        },
    }
}

/// Flush stdout and exit with `code` (hooks and checks have exit-code contracts).
fn exit(code: i32) -> ! {
    let _ = std::io::stdout().flush();
    std::process::exit(code)
}

/// Exit code as a POSIX shell reports it: the code, or 128 + signal.
fn shell_code(s: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(0))
}

/// Text as `$(...)` captures it: trailing newlines removed.
fn strip_newlines(s: &str) -> &str {
    s.trim_end_matches('\n')
}
