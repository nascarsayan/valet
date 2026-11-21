//! `valet git pr`: the GitHub PR of the current git branch, and the cache the zsh
//! prompt reads (`valet_pr` powerlevel10k segment).
//!
//! The prompt never waits on the network: it reads
//! `$XDG_CACHE_HOME/valet/pr/<key>` with zsh builtins and, when the file is
//! older than `VALET_PR_TTL` (600 s) or missing, starts `valet git pr refresh` in the
//! background. The new text shows from the next prompt on.

use crate::{proc, tools};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// p10k segment `valet_pr`, put after `vcs` in the left prompt. `$(<f)` and
/// `zstat` are builtins, so a cache hit costs no fork.
pub const SHELL_ZSH: &str = r#"# valet pr: PR of the current branch in the p10k prompt (`valet git pr --help`).
zmodload -F zsh/stat b:zstat 2>/dev/null
zmodload zsh/datetime 2>/dev/null
typeset -g VALET_PR_TTL=600
prompt_valet_pr() {
  local src=${VCS_STATUS_REMOTE_URL:-$VCS_STATUS_WORKDIR}
  local b=$VCS_STATUS_REMOTE_BRANCH
  [[ -z $b || $b == (master|main) ]] && b=$VCS_STATUS_LOCAL_BRANCH
  [[ -n $src && -n $b && $b != (master|main) ]] || return
  local f=__VALET_PR_CACHE__/${src//[^A-Za-z0-9._-]/_}__${b//[^A-Za-z0-9._-]/_}
  local -a m
  if ! zstat -A m +mtime -- $f 2>/dev/null || (( EPOCHSECONDS - m[1] > VALET_PR_TTL )); then
    command valet git pr refresh -- "$src" "$b" &>/dev/null &!
  fi
  [[ -s $f ]] && p10k segment -f 39 -t "$(<$f)"
}
if (( ${+POWERLEVEL9K_LEFT_PROMPT_ELEMENTS} && ! ${POWERLEVEL9K_LEFT_PROMPT_ELEMENTS[(Ie)valet_pr]} )); then
  () {
    local i=${POWERLEVEL9K_LEFT_PROMPT_ELEMENTS[(ie)vcs]}
    POWERLEVEL9K_LEFT_PROMPT_ELEMENTS[i+1,i]=(valet_pr)
  }
  (( $+functions[p10k] )) && p10k reload
fi
"#;

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    cmd: Option<Sub>,
}

#[derive(clap::Subcommand)]
enum Sub {
    /// Write the prompt text for <SOURCE> <BRANCH> to the cache (run by the
    /// zsh prompt in the background). SOURCE is a remote URL or a worktree
    /// directory (its origin is used).
    #[command(hide = true)]
    Refresh { source: String, branch: String },
}

/// A PR whose head is a given branch.
#[derive(Deserialize, Debug, Clone, PartialEq)]
struct BranchPr {
    number: u32,
    /// OPEN, MERGED or CLOSED.
    state: String,
    #[serde(rename = "isDraft")]
    draft: bool,
    #[serde(default)]
    url: String,
}

pub fn run(a: Args) -> Result<()> {
    match a.cmd {
        None => {
            let (repo, branch) = current()?;
            match for_branch(&repo, &branch)? {
                Some(p) => println!("{repo}#{} {} {}", p.number, label(&p), p.url),
                None => println!("no PR in {repo} for branch {branch}"),
            }
            Ok(())
        }
        Some(Sub::Refresh { source, branch }) => refresh(&source, &branch),
    }
}

/// `owner/name` of the current checkout's PR remote, and the branch name
/// there: the upstream when one is set, else origin and the local branch. A
/// branch tracking master/main is looked up by its local name.
fn current() -> Result<(String, String)> {
    let git = |args: &[&str]| proc::output("git", args, proc::NET_TIMEOUT);
    let local = git(&["symbolic-ref", "--short", "HEAD"])
        .context("not on a git branch (detached HEAD or not a repository)")?;
    let (remote, branch) = match git(&["rev-parse", "--abbrev-ref", "@{upstream}"]) {
        Ok(up) => match up.split_once('/') {
            Some((r, b)) if !is_default(b) => (r.to_owned(), b.to_owned()),
            Some((r, _)) => (r.to_owned(), local),
            None => ("origin".to_owned(), local),
        },
        Err(_) => ("origin".to_owned(), local),
    };
    if is_default(&branch) {
        bail!("on {branch}: no PR branch");
    }
    let url = git(&["remote", "get-url", &remote])?;
    let repo = repo_of_url(&url).with_context(|| format!("remote {remote} is not on GitHub"))?;
    Ok((repo, branch))
}

fn is_default(branch: &str) -> bool {
    matches!(branch, "master" | "main")
}

/// `owner/name` from a github.com remote URL (https, ssh:// or scp form).
fn repo_of_url(url: &str) -> Option<String> {
    let rest = url
        .trim()
        .trim_end_matches('/')
        .split_once("github.com")?
        .1
        .trim_start_matches([':', '/']);
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut parts = rest.split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    let ok = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    (ok(owner) && ok(name) && parts.next().is_none()).then(|| format!("{owner}/{name}"))
}

/// The PR in `repo` with head `branch`: the open one, else the newest.
fn for_branch(repo: &str, branch: &str) -> Result<Option<BranchPr>> {
    let gh = tools::resolve("gh")?;
    let out = proc::output(
        &gh,
        &[
            "pr",
            "list",
            "--repo",
            repo,
            "--head",
            branch,
            "--state",
            "all",
            "--limit",
            "20",
            "--json",
            "number,state,isDraft,url",
        ],
        proc::NET_TIMEOUT,
    )?;
    let prs: Vec<BranchPr> = serde_json::from_str(&out).context("parse gh pr list output")?;
    Ok(pick(prs))
}

fn pick(prs: Vec<BranchPr>) -> Option<BranchPr> {
    let newest = |s: Option<&str>| {
        prs.iter()
            .filter(|p| s.is_none_or(|s| p.state == s))
            .max_by_key(|p| p.number)
            .cloned()
    };
    newest(Some("OPEN")).or_else(|| newest(None))
}

fn label(p: &BranchPr) -> &'static str {
    match (p.state.as_str(), p.draft) {
        ("OPEN", true) => "draft",
        ("OPEN", false) => "open",
        ("MERGED", _) => "merged",
        _ => "closed",
    }
}

/// Prompt text: `#N`, `#N draft`, `#N merged`; nothing for closed or none.
fn prompt_text(p: Option<&BranchPr>) -> String {
    match p.map(|p| (p, label(p))) {
        Some((p, "open")) => format!("#{}", p.number),
        Some((p, l @ ("draft" | "merged"))) => format!("#{} {l}", p.number),
        _ => String::new(),
    }
}

/// Same key as the zsh segment: unsafe characters become `_`.
fn cache_key(source: &str, branch: &str) -> String {
    let clean = |s: &str| {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                    c
                } else {
                    '_'
                }
            })
            .collect::<String>()
    };
    format!("{}__{}", clean(source), clean(branch))
}

pub(crate) fn cache_dir() -> Result<PathBuf> {
    Ok(crate::cmd::apps::self_dir("Caches")?.join("pr"))
}

fn refresh(source: &str, branch: &str) -> Result<()> {
    let dir = cache_dir()?;
    std::fs::create_dir_all(&dir)?;
    let file = dir.join(cache_key(source, branch));
    // Mark the entry fresh first, so prompts drawn meanwhile do not start
    // more refreshes; a failed lookup keeps the old text until the next TTL.
    touch(&file)?;
    let url = if Path::new(source).is_absolute() {
        proc::output(
            "git",
            &["-C", source, "remote", "get-url", "origin"],
            proc::NET_TIMEOUT,
        )?
    } else {
        source.to_owned()
    };
    let Some(repo) = repo_of_url(&url) else {
        return write_atomic(&file, ""); // not GitHub: nothing to show
    };
    let text = prompt_text(for_branch(&repo, branch)?.as_ref());
    write_atomic(&file, &text)
}

fn touch(file: &Path) -> Result<()> {
    let f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)?;
    f.set_modified(std::time::SystemTime::now())?;
    Ok(())
}

fn write_atomic(file: &Path, text: &str) -> Result<()> {
    let tmp = file.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, file)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(number: u32, state: &str, draft: bool) -> BranchPr {
        BranchPr {
            number,
            state: state.into(),
            draft,
            url: String::new(),
        }
    }

    #[test]
    fn repo_from_remote_urls() {
        for u in [
            "https://github.com/acme/widgets.git",
            "https://github.com/acme/widgets",
            "git@github.com:acme/widgets.git",
            "ssh://git@github.com/acme/widgets.git",
            "https://github.com/acme/widgets/",
        ] {
            assert_eq!(repo_of_url(u).as_deref(), Some("acme/widgets"), "{u}");
        }
        for u in [
            "https://gitlab.com/a/b",
            "https://github.com/a",
            "https://github.com/a/b/c",
            "/tmp/x",
        ] {
            assert_eq!(repo_of_url(u), None, "{u}");
        }
    }

    #[test]
    fn open_pr_wins_over_newer_closed() {
        let got = pick(vec![pr(9, "CLOSED", false), pr(5, "OPEN", false)]);
        assert_eq!(got.map(|p| p.number), Some(5));
        let got = pick(vec![pr(3, "MERGED", false), pr(7, "CLOSED", false)]);
        assert_eq!(got.map(|p| p.number), Some(7));
        assert_eq!(pick(vec![]), None);
    }

    #[test]
    fn prompt_text_per_state() {
        assert_eq!(prompt_text(Some(&pr(12, "OPEN", false))), "#12");
        assert_eq!(prompt_text(Some(&pr(12, "OPEN", true))), "#12 draft");
        assert_eq!(prompt_text(Some(&pr(12, "MERGED", false))), "#12 merged");
        assert_eq!(prompt_text(Some(&pr(12, "CLOSED", false))), "");
        assert_eq!(prompt_text(None), "");
    }

    #[test]
    fn cache_key_matches_zsh_substitution() {
        // zsh: ${src//[^A-Za-z0-9._-]/_}__${b//[^A-Za-z0-9._-]/_}
        assert_eq!(
            cache_key("git@github.com:acme/widgets.git", "alice/fix-1"),
            "git_github.com_acme_widgets.git__alice_fix-1"
        );
    }
}
