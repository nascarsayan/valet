//! Claude Code hooks that keep skill edits in git.
//!
//! Skills are symlinks in `~/.claude/skills` to directories in git clones
//! (`valet skills`). An edit through a link still lands in a repo, but the agent
//! cannot see which one, so it forgets to commit, or commits on whatever
//! branch is checked out. These hooks make it work in the repo itself.
//!
//! `skill-source` (PreToolUse on Edit, Write, MultiEdit, NotebookEdit, Bash):
//!
//! - a path through `~/.claude/skills/<name>/` is denied, naming the source;
//! - a third-party skill source is denied: `valet skills apply --update` pulls it;
//! - a team skill source (an owner in config.toml `[repos] team`) on its
//!   default branch is denied until the agent creates a branch;
//! - an own or team skill source is allowed, with the repo and branch as
//!   context and a reminder to commit there.
//!
//! Bash is checked best effort: a write (redirect, `sed -i`, `tee`, `rm`,
//! `cp`/`mv` destination, ...) whose target names `~/.claude/skills` is
//! denied.
//!
//! `skill-commit` (Stop): blocks the stop once (not again while
//! `stop_hook_active`) while an own or team skill source has uncommitted
//! changes to a file this session's Edit, Write, MultiEdit or NotebookEdit
//! calls wrote (read from the transcript).
//!
//! Malformed input or a failed lookup produces no output and exit 0, so the
//! tool call proceeds as if the hook were absent.

use crate::proc::Cmd;
use serde_json::{Value, json};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

const MAX_INPUT: u64 = 64 << 20;
/// Only the tail of a larger transcript is searched.
const MAX_TRANSCRIPT: u64 = 256 << 20;
const GIT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq, Debug)]
enum Class {
    Own,
    Team,
    ThirdParty,
}

/// Owner prefixes from config.toml: (owned, team).
fn owners() -> (Vec<String>, Vec<String>) {
    crate::repos::manifest()
        .map(|m| (m.owned, m.team))
        .unwrap_or_default()
}

fn class_of(repo: Option<&str>, owners: &(Vec<String>, Vec<String>)) -> Class {
    let Some(owner) = repo.and_then(|r| r.split('/').nth(1)) else {
        return Class::Own; // `~/` sources
    };
    let has = |v: &[String]| v.iter().any(|o| o.rsplit('/').next() == Some(owner));
    if has(&owners.0) {
        Class::Own
    } else if has(&owners.1) {
        Class::Team
    } else {
        Class::ThirdParty
    }
}

struct Src {
    name: String,
    repo: String,
    class: Class,
    dir: PathBuf,
    repo_dir: PathBuf,
}

/// Current and default branch of a repo, if git can tell.
type BranchFn = fn(&Path) -> Option<(String, String)>;

struct Ctx {
    /// `~/.claude/skills`, as spelled under HOME and canonical.
    links: Vec<PathBuf>,
    sources: Vec<Src>,
    branch: BranchFn,
}

#[derive(Debug, PartialEq)]
enum Verdict {
    Deny(String),
    Note(String),
    Pass,
}

impl Ctx {
    fn from_process() -> anyhow::Result<Ctx> {
        let repos = crate::repos::root()?;
        let claude = super::super::claude_dir()?;
        let links_dir = claude.join("skills");
        let mut links = vec![links_dir.clone()];
        if let Ok(c) = links_dir.canonicalize()
            && c != links_dir
        {
            links.push(c);
        }
        let owners = owners();
        let sources = crate::cmd::skills::sources()?
            .into_iter()
            .map(|s| {
                let repo = s.repo.clone().unwrap_or_default();
                let repo_dir = if repo.is_empty() {
                    s.dir.clone()
                } else {
                    repos.join(&repo)
                };
                Src {
                    class: class_of(s.repo.as_deref(), &owners),
                    name: s.name,
                    repo,
                    dir: norm(&s.dir),
                    repo_dir: norm(&repo_dir),
                }
            })
            .collect();
        Ok(Ctx {
            links,
            sources,
            branch: git_branch,
        })
    }

    /// Verdict for a file tool writing `raw`.
    fn judge_path(&self, raw: &Path) -> Verdict {
        let clean = lexical(raw);
        for links in &self.links {
            let Ok(rest) = clean.strip_prefix(links) else {
                continue;
            };
            let mut parts = rest.components();
            let Some(Component::Normal(name)) = parts.next() else {
                return Verdict::Deny(format!(
                    "`{}` is the generated skills directory (`valet skills apply` owns it); \
                     change config.toml [skills] in the valet repo instead.",
                    raw.display()
                ));
            };
            let link = links.join(name);
            let Ok(target) = fs::read_link(&link) else {
                return Verdict::Pass;
            };
            let target = if target.is_absolute() {
                target
            } else {
                links.join(target)
            };
            let source = norm(&target.join(parts.as_path()));
            return match self.judge_real(&source) {
                Verdict::Deny(m) => Verdict::Deny(m),
                v => Verdict::Deny(format!(
                    "`{}` goes through the skill link {}. Edit the source file instead: `{}`.{}",
                    raw.display(),
                    link.display(),
                    source.display(),
                    match v {
                        Verdict::Note(n) => format!(" {n}"),
                        _ => String::new(),
                    }
                )),
            };
        }
        self.judge_real(&norm(&clean))
    }

    /// Verdict for a resolved path.
    fn judge_real(&self, p: &Path) -> Verdict {
        let Some(s) = self
            .sources
            .iter()
            .filter(|s| p.starts_with(&s.dir))
            .max_by_key(|s| s.dir.as_os_str().len())
        else {
            return Verdict::Pass;
        };
        if s.class == Class::ThirdParty {
            return Verdict::Deny(format!(
                "Skill `{}` comes from the third-party repo {}, which `valet skills apply \
                 --update` pulls, so local edits get in its way. Fork it, or copy the skill \
                 to the `skills` alias's repo ({}/{}) and point its [skills] entry there \
                 (`valet upgrade && valet skills apply`).",
                s.name,
                s.repo,
                crate::repos::alias_path("skills")
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                s.name
            ));
        }
        let branch = (self.branch)(&s.repo_dir);
        let repo = s.repo_dir.display();
        match (s.class, branch) {
            (Class::Team, Some((cur, default))) if cur == default || cur == "HEAD" => {
                Verdict::Deny(format!(
                    "Skill `{}` is in the team repo {repo}, which is on `{cur}`. Create a \
                     branch first (`git -C {repo} switch -c skill-update/{}-<yyyymmdd>` from \
                     an up-to-date origin/{default}), then edit, commit, push and open a PR.",
                    s.name, s.name
                ))
            }
            (Class::Team, b) => Verdict::Note(format!(
                "Skill `{}` source is in the team repo {repo}{}. Commit there; push and open \
                 a PR when the user agrees.",
                s.name,
                on_branch(b)
            )),
            (_, b) => Verdict::Note(format!(
                "Skill `{}` source is in the git repo {repo}{}. Commit there when done; push \
                 only when the user says so.",
                s.name,
                on_branch(b)
            )),
        }
    }

    /// Verdict for a Bash command (best effort).
    fn judge_bash(&self, cmd: &str) -> Verdict {
        if !guarded(cmd) {
            return Verdict::Pass;
        }
        match write_target(cmd) {
            Some(t) => Verdict::Deny(format!(
                "This command writes `{t}`, under ~/.claude/skills. \
                 Edit the skill's source file in its git repo with the file tools instead \
                 (`valet skills list` shows each link's source)."
            )),
            None => Verdict::Pass,
        }
    }

    /// Dirty files in own and team skill sources that the session's file
    /// tools wrote (`edited`, normalized).
    fn uncommitted(&self, edited: &[PathBuf]) -> Vec<String> {
        let mut out = Vec::new();
        let mut repos: Vec<&PathBuf> = Vec::new();
        for s in &self.sources {
            if s.class != Class::ThirdParty && !repos.contains(&&s.repo_dir) {
                repos.push(&s.repo_dir);
            }
        }
        for repo in repos {
            let dirs: Vec<String> = self
                .sources
                .iter()
                .filter(|s| &s.repo_dir == repo && s.class != Class::ThirdParty)
                .filter_map(|s| s.dir.strip_prefix(repo).ok())
                .map(|d| {
                    let d = d.to_string_lossy();
                    if d.is_empty() {
                        ".".into()
                    } else {
                        d.into_owned()
                    }
                })
                .collect();
            for f in dirty(repo, &dirs) {
                if edited.contains(&norm(&repo.join(&f))) {
                    out.push(format!("{}/{f}", repo.display()));
                }
            }
        }
        out
    }
}

fn on_branch(b: Option<(String, String)>) -> String {
    b.map(|(cur, _)| format!(" on branch `{cur}`"))
        .unwrap_or_default()
}

fn git_branch(repo: &Path) -> Option<(String, String)> {
    let git = |args: &[&str]| {
        Cmd::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .timeout(GIT_TIMEOUT)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| o.stdout.trim().to_owned())
    };
    let cur = git(&["rev-parse", "--abbrev-ref", "HEAD"])?;
    let default = git(&["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        .and_then(|r| r.strip_prefix("origin/").map(str::to_owned))
        .unwrap_or_else(|| "main".into());
    Some((cur, default))
}

/// Repo-relative paths with uncommitted changes under `dirs`.
fn dirty(repo: &Path, dirs: &[String]) -> Vec<String> {
    let Ok(o) = Cmd::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--",
        ])
        .args(dirs)
        .timeout(GIT_TIMEOUT)
        .output()
    else {
        return Vec::new();
    };
    if !o.status.success() {
        return Vec::new();
    }
    parse_porcelain(&o.stdout)
}

/// Paths from `git status --porcelain=v1 -z` (rename sources skipped).
fn parse_porcelain(out: &str) -> Vec<String> {
    let mut v = Vec::new();
    let mut it = out.split('\0');
    while let Some(e) = it.next() {
        if e.len() < 4 {
            continue;
        }
        let (xy, path) = e.split_at(3);
        v.push(path.to_owned());
        if xy.starts_with('R') || xy.starts_with('C') {
            it.next();
        }
    }
    v
}

/// `.` and `..` resolved lexically; the path stays absolute.
pub(super) fn lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// The canonical path (nearest existing ancestor canonicalized, the rest
/// appended).
fn norm(p: &Path) -> PathBuf {
    let p = lexical(p);
    let mut base = p.as_path();
    let mut rest = Vec::new();
    let canon = loop {
        if let Ok(c) = base.canonicalize() {
            break c;
        }
        match (base.parent(), base.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                base = parent;
            }
            _ => return p,
        }
    };
    let mut out = canon;
    for r in rest.iter().rev() {
        out.push(r);
    }
    out
}

fn guarded(word: &str) -> bool {
    word.contains(".claude/skills")
}

/// The first written path in `cmd` that names a guarded place, if any.
fn write_target(cmd: &str) -> Option<String> {
    write_target_by(cmd, guarded)
}

/// The first path `cmd` writes (redirection, or an argument of a writing
/// command: tee, touch, mkdir, cp/mv destination, sed -i, ...) for which
/// `hit` is true.
pub(super) fn write_target_by(cmd: &str, guarded: impl Fn(&str) -> bool) -> Option<String> {
    let redirect = regex::Regex::new(r#"\d*>>?\|?\s*(['"]?)([^\s;&|)'"]+)"#).ok()?;
    for c in redirect.captures_iter(cmd) {
        let t = &c[2];
        if guarded(t) {
            return Some(t.to_owned());
        }
    }
    for seg in cmd.split(['\n', ';', '|', '&', '(', ')']) {
        let words: Vec<&str> = seg
            .split_whitespace()
            .map(|w| w.trim_matches(['\'', '"']))
            .collect();
        let mut i = 0;
        while i < words.len()
            && (words[i] == "sudo"
                || words[i] == "env"
                || words[i] == "command"
                || words[i].contains('=') && !words[i].starts_with('-'))
        {
            i += 1;
        }
        let Some(&prog) = words.get(i) else {
            continue;
        };
        let args: Vec<&str> = words[i + 1..]
            .iter()
            .copied()
            .filter(|a| !a.contains('>'))
            .collect();
        let prog = prog.rsplit('/').next().unwrap_or(prog);
        let in_place = |flag: char| {
            args.iter()
                .any(|a| a.starts_with('-') && !a.starts_with("--") && a.contains(flag))
                || args.iter().any(|a| a.starts_with("--in-place"))
        };
        let any = || args.iter().find(|a| guarded(a)).map(|a| a.to_string());
        let last = || {
            args.iter()
                .rfind(|a| !a.starts_with('-'))
                .filter(|a| guarded(a))
                .map(|a| a.to_string())
        };
        let hit = match prog {
            "sed" if in_place('i') => any(),
            "perl" if in_place('i') => any(),
            "tee" | "rm" | "rmdir" | "truncate" | "touch" | "chmod" | "chown" | "patch"
            | "unlink" | "mkdir" => any(),
            "cp" | "mv" | "install" | "ln" | "rsync" => last(),
            _ => None,
        };
        if hit.is_some() {
            return hit;
        }
    }
    None
}

pub(super) fn read_stdin() -> Option<Value> {
    let mut buf = Vec::new();
    std::io::stdin()
        .lock()
        .take(MAX_INPUT + 1)
        .read_to_end(&mut buf)
        .ok()?;
    if buf.len() as u64 > MAX_INPUT {
        return None;
    }
    serde_json::from_slice(&buf).ok()
}

pub(super) fn emit(v: &Value) {
    let mut so = std::io::stdout().lock();
    let _ = writeln!(so, "{v}");
    let _ = so.flush();
}

fn pre_tool_output(v: &Verdict) -> Option<Value> {
    match v {
        Verdict::Deny(m) => Some(json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": m,
        }})),
        Verdict::Note(m) => Some(json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "additionalContext": m,
        }})),
        Verdict::Pass => None,
    }
}

/// `valet hook claude skill-source`.
pub fn pre_tool_use() {
    let Some(input) = read_stdin() else { return };
    let tool = input["tool_name"].as_str().unwrap_or_default();
    let ti = &input["tool_input"];
    let target = match tool {
        "Edit" | "Write" | "MultiEdit" => ti["file_path"].as_str(),
        "NotebookEdit" => ti["notebook_path"].as_str(),
        "Bash" => None,
        _ => return,
    };
    if tool == "Bash" && !ti["command"].as_str().is_some_and(guarded) {
        return; // fast path: no repo lookup
    }
    let Ok(ctx) = Ctx::from_process() else { return };
    let verdict = match (tool, target) {
        ("Bash", _) => ctx.judge_bash(ti["command"].as_str().unwrap_or_default()),
        (_, Some(p)) if Path::new(p).is_absolute() => ctx.judge_path(Path::new(p)),
        _ => return,
    };
    if let Some(out) = pre_tool_output(&verdict) {
        emit(&out);
    }
}

/// `valet hook claude skill-commit`.
pub fn stop() {
    let Some(input) = read_stdin() else { return };
    if input["stop_hook_active"].as_bool() == Some(true) {
        return;
    }
    let Some(path) = input["transcript_path"].as_str() else {
        return;
    };
    let Some(transcript) = read_tail(Path::new(path)) else {
        return;
    };
    let Ok(ctx) = Ctx::from_process() else { return };
    let edited = edited_paths(&transcript);
    if edited.is_empty() {
        return;
    }
    let files = ctx.uncommitted(&edited);
    if files.is_empty() {
        return;
    }
    emit(&json!({
        "decision": "block",
        "reason": format!(
            "Uncommitted skill changes from this session:\n{}\nCommit them in their repo \
             (team repos: on a branch, then push and open a PR when the user agrees), or tell \
             the user why they stay uncommitted.",
            files.iter().map(|f| format!("- {f}")).collect::<Vec<_>>().join("\n")
        ),
    }));
}

/// Paths the transcript's Edit, Write, MultiEdit and NotebookEdit calls
/// target, normalized; one JSON object per line, unparsable lines skipped.
fn edited_paths(transcript: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for line in transcript.lines() {
        if !line.contains("\"tool_use\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        for c in v["message"]["content"].as_array().into_iter().flatten() {
            if c["type"] != "tool_use" {
                continue;
            }
            let key = match c["name"].as_str() {
                Some("Edit" | "Write" | "MultiEdit") => "file_path",
                Some("NotebookEdit") => "notebook_path",
                _ => continue,
            };
            if let Some(p) = c["input"][key].as_str().filter(|p| p.starts_with('/')) {
                let p = norm(Path::new(p));
                if !out.contains(&p) {
                    out.push(p);
                }
            }
        }
    }
    out
}

fn read_tail(p: &Path) -> Option<String> {
    let mut f = fs::File::open(p).ok()?;
    let len = f.metadata().ok()?.len();
    if len > MAX_TRANSCRIPT {
        f.seek(SeekFrom::Start(len - MAX_TRANSCRIPT)).ok()?;
    }
    let mut buf = Vec::new();
    f.take(MAX_TRANSCRIPT).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_owners() -> (Vec<String>, Vec<String>) {
        (
            vec!["github.com/example-user".into()],
            vec!["github.com/acme".into()],
        )
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("valet-skill-src-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    fn on_main(_: &Path) -> Option<(String, String)> {
        Some(("main".into(), "main".into()))
    }

    fn on_topic(_: &Path) -> Option<(String, String)> {
        Some(("topic".into(), "main".into()))
    }

    /// home/.claude/skills/{own,team,third} linked into a fake ghq root.
    fn fixture(tag: &str, branch: BranchFn) -> (PathBuf, Ctx) {
        let root = tmp(tag);
        let links = root.join("home/.claude/skills");
        let repos = root.join("ghq");
        fs::create_dir_all(&links).unwrap();
        let mk = |rel: &str| {
            let d = repos.join(rel);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("SKILL.md"), "x").unwrap();
            d
        };
        let own = mk("github.com/example-user/skills/skills/own");
        let team = mk("github.com/acme/skills/skills/team");
        let third = mk("github.com/thirdparty/skills/skills/third");
        std::os::unix::fs::symlink(&own, links.join("own")).unwrap();
        std::os::unix::fs::symlink(&team, links.join("team")).unwrap();
        std::os::unix::fs::symlink(&third, links.join("third")).unwrap();
        let src = |name: &str, repo: &str, dir: &Path| Src {
            name: name.into(),
            repo: repo.into(),
            class: class_of(Some(repo), &test_owners()),
            dir: dir.to_path_buf(),
            repo_dir: repos.join(repo),
        };
        let ctx = Ctx {
            links: vec![links],
            sources: vec![
                src("own", "github.com/example-user/skills", &own),
                src("team", "github.com/acme/skills", &team),
                src("third", "github.com/thirdparty/skills", &third),
            ],
            branch,
        };
        (root, ctx)
    }

    fn deny(v: &Verdict) -> &str {
        match v {
            Verdict::Deny(m) => m,
            other => panic!("want deny, got {other:?}"),
        }
    }

    #[test]
    fn classes() {
        let o = test_owners();
        assert_eq!(
            class_of(Some("github.com/example-user/dev"), &o),
            Class::Own
        );
        assert_eq!(class_of(Some("github.com/acme/skills"), &o), Class::Team);
        assert_eq!(
            class_of(Some("github.com/someone/tool"), &o),
            Class::ThirdParty
        );
        assert_eq!(class_of(None, &o), Class::Own);
    }

    #[test]
    fn link_path_is_denied_with_the_source() {
        let (root, ctx) = fixture("link", on_topic);
        let p = root.join("home/.claude/skills/own/SKILL.md");
        let m = ctx.judge_path(&p).pipe(|v| deny(&v).to_owned());
        assert!(
            m.contains("github.com/example-user/skills/skills/own/SKILL.md"),
            "{m}"
        );
        assert!(m.contains("on branch `topic`"), "{m}");
        // `..` cannot hide the link.
        let sneaky = root.join("home/.claude/x/../skills/own/new.md");
        assert!(matches!(ctx.judge_path(&sneaky), Verdict::Deny(_)));
    }

    #[test]
    fn own_source_is_allowed_with_a_note() {
        let (root, ctx) = fixture("own", on_main);
        let p = root.join("ghq/github.com/example-user/skills/skills/own/SKILL.md");
        match ctx.judge_path(&p) {
            Verdict::Note(m) => assert!(m.contains("Commit there"), "{m}"),
            v => panic!("{v:?}"),
        }
        let elsewhere = root.join("ghq/github.com/example-user/skills/README.md");
        assert_eq!(ctx.judge_path(&elsewhere), Verdict::Pass);
    }

    #[test]
    fn team_source_needs_a_branch() {
        let (root, ctx) = fixture("team", on_main);
        let p = root.join("ghq/github.com/acme/skills/skills/team/SKILL.md");
        assert!(deny(&ctx.judge_path(&p)).contains("switch -c skill-update/team-"));
        let (root, ctx) = fixture("team2", on_topic);
        let p = root.join("ghq/github.com/acme/skills/skills/team/SKILL.md");
        assert!(matches!(ctx.judge_path(&p), Verdict::Note(_)));
    }

    #[test]
    fn third_party_is_denied() {
        let (root, ctx) = fixture("third", on_topic);
        let p = root.join("ghq/github.com/thirdparty/skills/skills/third/SKILL.md");
        assert!(deny(&ctx.judge_path(&p)).contains("third-party"));
        // Through the link: same verdict.
        let p = root.join("home/.claude/skills/third/SKILL.md");
        assert!(deny(&ctx.judge_path(&p)).contains("third-party"));
    }

    #[test]
    fn unrelated_paths_pass() {
        let (_root, ctx) = fixture("pass", on_main);
        assert_eq!(ctx.judge_path(Path::new("/tmp/x.md")), Verdict::Pass);
    }

    #[test]
    fn bash_writes_are_caught() {
        let (_root, ctx) = fixture("bash", on_main);
        for c in [
            "sed -i 's/a/b/' ~/.claude/skills/own/SKILL.md",
            "echo x > ~/.claude/skills/own/SKILL.md",
            "cat a >> \"$HOME/.claude/skills/own/SKILL.md\"",
            "cd /x && cp /tmp/a ~/.claude/skills/x/SKILL.md",
            "printf x | tee ~/.claude/skills/own/SKILL.md",
            "rm -rf ~/.claude/skills/own",
            "perl -pi -e 's/a/b/' ~/.claude/skills/own/SKILL.md",
        ] {
            assert!(matches!(ctx.judge_bash(c), Verdict::Deny(_)), "{c}");
        }
        for c in [
            "cat ~/.claude/skills/own/SKILL.md",
            "grep -rn x ~/.claude/skills 2>/dev/null | head",
            "cp ~/.claude/skills/own/SKILL.md /tmp/",
            "ls -la ~/.claude/skills > /tmp/list.txt",
            "cd ~/.claude/skills && ls -la",
            "sed -n 1,20p ~/.claude/skills/own/SKILL.md",
        ] {
            assert_eq!(ctx.judge_bash(c), Verdict::Pass, "{c}");
        }
    }

    #[test]
    fn porcelain_parses_renames() {
        let out = " M skills/a/SKILL.md\0R  skills/b/new.md\0skills/b/old.md\0?? skills/c/x\0";
        assert_eq!(
            parse_porcelain(out),
            vec!["skills/a/SKILL.md", "skills/b/new.md", "skills/c/x"]
        );
    }

    #[test]
    fn only_file_tool_writes_count_as_edited() {
        let t = [
            r#"{"message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/x/a/SKILL.md"}}]}}"#,
            r#"{"message":{"content":[{"type":"tool_use","name":"NotebookEdit","input":{"notebook_path":"/x/n.ipynb"}}]}}"#,
            r#"{"message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"/x/b/SKILL.md"}}]}}"#,
            r#"{"message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"git status /x/c/SKILL.md"}}]}}"#,
            r#"{"message":{"content":[{"type":"text","text":"edited /x/d/SKILL.md"}]}}"#,
            "not json \"tool_use\"",
        ]
        .join("\n");
        assert_eq!(
            edited_paths(&t),
            vec![PathBuf::from("/x/a/SKILL.md"), PathBuf::from("/x/n.ipynb")]
        );
    }

    #[test]
    fn lexical_resolves_dots() {
        assert_eq!(lexical(Path::new("/a/./b/../c")), PathBuf::from("/a/c"));
    }

    trait Pipe: Sized {
        fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
            f(self)
        }
    }
    impl<T> Pipe for T {}
}
