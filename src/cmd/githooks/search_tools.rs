//! `valet hook claude search-tools`: a Claude Code PreToolUse hook for Bash
//! that steers file searches to the fast tools and keeps them inside the
//! workspace.
//!
//! Denied: `grep` (use `rg`); `find` (use `fd`); `fd` or `rg` whose search
//! root lies outside the ws folder (the Userland root's parent): elsewhere
//! (network mounts, the whole disk) a walk can take ages (`rg` on a single
//! file or a pipe is fine). A command that really needs `find`, or a search
//! outside ws, says why:
//! `FIND_REASON="<why>" <command>`. A heuristic on the command text, like
//! no-tmp.

use super::no_tmp::strip_heredocs;
use super::skill_source::{emit, lexical, read_stdin};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Leading assignment that allows `find`, or `fd` outside ws, for one command.
const REASON_ENV: &str = "FIND_REASON";

const NAME: &str = "valet hook claude search-tools";

/// The command sets a non-empty [`REASON_ENV`].
fn has_reason(cmd: &str) -> bool {
    let re = regex::Regex::new(&format!(
        r#"(?:^|[\s;&|(])(?:export\s+)?{REASON_ENV}=(?:"[^"]+"|'[^']+'|[^\s"';&|]+)"#
    ))
    .expect("valid regex");
    re.is_match(cmd)
}

/// Folders searches may walk freely: the ws folder, as written and resolved.
fn allowed_roots() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(ws) = crate::cmd::apps::root()
        .ok()
        .and_then(|r| r.parent().map(Path::to_path_buf))
    {
        if let Ok(c) = ws.canonicalize() {
            v.push(c);
        }
        v.push(ws);
    }
    v
}

/// `word` as an absolute path (`~`, `$HOME` expanded; relative to `cwd`).
fn absolute(word: &str, cwd: Option<&Path>) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok();
    let w = match (word.strip_prefix('~'), word.strip_prefix("$HOME"), &home) {
        (Some(rest), _, Some(h)) if rest.is_empty() || rest.starts_with('/') => {
            format!("{h}{rest}")
        }
        (_, Some(rest), Some(h)) if rest.is_empty() || rest.starts_with('/') => {
            format!("{h}{rest}")
        }
        _ => word.to_owned(),
    };
    let p = Path::new(&w);
    if p.is_absolute() {
        Some(lexical(p))
    } else {
        cwd.map(|c| lexical(&c.join(p)))
    }
}

/// Inside one of ROOTS, lexically or once resolved.
fn inside(p: &Path, roots: &[PathBuf]) -> bool {
    let hit = |p: &Path| roots.iter().any(|r| p.starts_with(r));
    hit(p) || p.canonicalize().is_ok_and(|c| hit(&c))
}

/// `fd`'s search roots: path-like arguments (and `--search-path`,
/// `--base-directory` values), else the working directory. Stops at
/// `-x`/`-X` (the rest is the command to run).
fn fd_roots(args: &[String], cwd: Option<&Path>) -> Vec<Option<PathBuf>> {
    let mut roots = Vec::new();
    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        let a = a.as_str();
        if matches!(a, "-x" | "-X" | "--exec" | "--exec-batch") {
            break;
        }
        let value = match a {
            "--search-path" | "--base-directory" => it.next().map(String::as_str),
            _ => a
                .strip_prefix("--search-path=")
                .or_else(|| a.strip_prefix("--base-directory=")),
        };
        if let Some(v) = value {
            roots.push(absolute(v, cwd));
        } else if path_like(a) {
            roots.push(absolute(a, cwd));
        }
    }
    if roots.is_empty() {
        roots.push(cwd.map(Path::to_path_buf));
    }
    roots
}

/// Shell words of each simple command in `cmd`, quote-aware: split on
/// unquoted ; | & ( ) ` and newlines; quotes removed; leading env
/// assignments, sudo, env, command and exec skipped. The flag: the command
/// reads a pipe (`a | b`, not `a || b`).
fn commands(cmd: &str) -> Vec<(Vec<String>, bool)> {
    let mut out = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut piped = false;
    let mut prev = ' ';
    let flush_cmd = |words: &mut Vec<String>, out: &mut Vec<(Vec<String>, bool)>, piped: bool| {
        let skip = words
            .iter()
            .take_while(|w| {
                matches!(w.as_str(), "sudo" | "env" | "command" | "exec")
                    || w.contains('=') && !w.starts_with('-')
            })
            .count();
        if skip < words.len() {
            out.push((words[skip..].to_vec(), piped));
        }
        words.clear();
    };
    for c in cmd.chars() {
        let last = prev;
        if !c.is_whitespace() {
            prev = c;
        }
        let prev = last;
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => word.push(c),
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    in_word = true;
                }
                ';' | '|' | '&' | '(' | ')' | '`' | '\n' => {
                    if in_word {
                        words.push(std::mem::take(&mut word));
                        in_word = false;
                    }
                    flush_cmd(&mut words, &mut out, piped);
                    piped = c == '|' && prev != '|';
                }
                c if c.is_whitespace() => {
                    if in_word {
                        words.push(std::mem::take(&mut word));
                        in_word = false;
                    }
                }
                c => {
                    word.push(c);
                    in_word = true;
                }
            },
        }
    }
    if in_word {
        words.push(word);
    }
    flush_cmd(&mut words, &mut out, piped);
    out
}

/// A word that names a path (absolute, home-relative, or `.`/`..`-relative).
fn path_like(a: &str) -> bool {
    !a.starts_with('-')
        && (a.starts_with('/')
            || a.starts_with('~')
            || a.starts_with("$HOME")
            || a == "."
            || a == ".."
            || a.starts_with("./")
            || a.starts_with("../"))
}

/// `rg`'s explicit search paths: path-like positionals after the pattern
/// (the first positional, unless -e/-f give it); values of flags that take
/// one are skipped.
fn rg_paths(args: &[String], cwd: Option<&Path>) -> Vec<Option<PathBuf>> {
    const TAKES_VALUE: &[&str] = &[
        "-e",
        "--regexp",
        "-f",
        "--file",
        "-g",
        "--glob",
        "--iglob",
        "-t",
        "--type",
        "-T",
        "--type-not",
        "-r",
        "--replace",
        "-A",
        "-B",
        "-C",
        "-m",
        "--max-count",
        "-M",
        "--max-columns",
        "-d",
        "--max-depth",
        "-j",
        "--threads",
        "--type-add",
    ];
    let mut pattern_given = args
        .iter()
        .any(|a| matches!(a.as_str(), "-e" | "--regexp" | "-f" | "--file"));
    let mut paths = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let a = a.as_str();
        if a == "--" {
            continue;
        }
        if TAKES_VALUE.contains(&a) {
            it.next();
            continue;
        }
        if a.starts_with('-') {
            continue;
        }
        if !pattern_given {
            pattern_given = true;
            continue;
        }
        if path_like(a) {
            paths.push(absolute(a, cwd));
        }
    }
    paths
}

/// Why `cmd` (run in `cwd`) is refused, if it is.
fn judge_bash(cmd: &str, cwd: Option<&Path>, roots: &[PathBuf]) -> Option<String> {
    if !["grep", "find", "fd", "rg"].iter().any(|w| cmd.contains(w)) {
        return None; // fast path
    }
    let cmd = &strip_heredocs(cmd);
    let reason = has_reason(cmd);
    for (w, piped) in commands(cmd) {
        // `xargs grep ...`, `time find ...`: judge the command they run.
        let start = w
            .iter()
            .position(|x| !matches!(x.as_str(), "xargs" | "time" | "nice" | "nohup"))
            .unwrap_or(0);
        let w = &w[start..];
        let prog = w[0].rsplit('/').next().unwrap_or(&w[0]);
        match prog {
            "grep" | "egrep" | "fgrep" | "zgrep" => {
                return Some(format!(
                    "{NAME}: use rg, not {prog} (recursive by default, skips .gitignore'd files, much faster; -n -i -l -w -F -A/-B/-C work the same, -z for compressed files). (refused: `{}`)",
                    w.join(" ")
                ));
            }
            "find" if !reason => {
                return Some(format!(
                    "{NAME}: use fd, not find (e.g. `fd -t f NAME DIR`, `fd -e rs`, `fd -H -I` to include hidden and ignored files, `-d N` to bound depth). find is slow on large trees; if find is really needed, prefix the command with {REASON_ENV}=\"<why>\". (refused: `{}`)",
                    w.join(" ")
                ));
            }
            "fd" | "fdfind" if !reason => {
                for r in fd_roots(&w[1..], cwd) {
                    let outside = match &r {
                        Some(p) => !inside(p, roots),
                        None => false,
                    };
                    if outside {
                        let p = r.map(|p| p.display().to_string()).unwrap_or_default();
                        return Some(format!(
                            "{NAME}: fd outside the ws folder ({p}) can walk a huge tree and take ages. Search inside the ws folder; if you must search there, bound it (-d N, a narrow DIR) and prefix the command with {REASON_ENV}=\"<why>\". (refused: `{}`)",
                            w.join(" ")
                        ));
                    }
                }
            }
            "rg" if !reason => {
                let mut paths = rg_paths(&w[1..], cwd);
                if paths.is_empty() && !piped {
                    paths.push(cwd.map(Path::to_path_buf));
                }
                // A single file is a read, not a walk.
                if let Some(p) = paths
                    .into_iter()
                    .flatten()
                    .find(|p| !inside(p, roots) && !p.is_file())
                {
                    return Some(format!(
                        "{NAME}: rg outside the ws folder ({}) can walk a huge tree and take ages. Search inside the ws folder, a single file, or a pipe; if you must search there, narrow it (a small DIR, -g, --max-depth) and prefix the command with {REASON_ENV}=\"<why>\". (refused: `{}`)",
                        p.display(),
                        w.join(" ")
                    ));
                }
            }
            _ => {}
        }
    }
    None
}

/// The deny reason for one tool call in `cwd`, if it is refused.
pub fn judge(tool: &str, input: &Value, cwd: Option<&Path>) -> Option<String> {
    if tool != "Bash" {
        return None;
    }
    judge_bash(input["command"].as_str()?, cwd, &allowed_roots())
}

/// `valet hook claude search-tools`: prints a deny decision only when there is
/// one; always exits 0.
pub fn pre_tool_use() {
    let Some(input) = read_stdin() else { return };
    let tool = input["tool_name"].as_str().unwrap_or_default();
    let cwd = input["cwd"].as_str().map(Path::new);
    if let Some(reason) = judge(tool, &input["tool_input"], cwd) {
        emit(&json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }}));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots() -> Vec<PathBuf> {
        vec![PathBuf::from("/w/ws")]
    }

    fn bash(c: &str, cwd: &str) -> Option<String> {
        judge_bash(c, Some(Path::new(cwd)), &roots())
    }

    #[test]
    fn grep_is_refused_everywhere() {
        for c in [
            "grep -rn foo .",
            "ps aux | grep x",
            "git log | egrep a",
            "fd -e rs | xargs grep TODO",
            "cd x && /usr/bin/grep y z",
        ] {
            assert!(
                bash(c, "/w/ws/repo").is_some_and(|r| r.contains("use rg")),
                "{c}"
            );
        }
        for c in [
            "rg foo",
            "git grep foo",
            "echo grep",
            "rg 'grep|find'",
            "rg \"a; find .\" src",
        ] {
            assert_eq!(bash(c, "/w/ws/repo"), None, "{c}");
        }
    }

    #[test]
    fn find_needs_a_reason() {
        assert!(bash("find . -name x", "/w/ws/repo").is_some_and(|r| r.contains("use fd")));
        assert!(bash("time find / -name x", "/w/ws").is_some());
        assert_eq!(
            bash("FIND_REASON=\"needs -perm\" find . -perm -u+s", "/w/ws"),
            None
        );
        assert!(bash("FIND_REASON= find .", "/w/ws").is_some());
    }

    #[test]
    fn fd_outside_ws_needs_a_reason() {
        assert_eq!(bash("fd -e rs", "/w/ws/repo"), None);
        assert_eq!(bash("fd foo /w/ws/x -d 3", "/"), None);
        assert_eq!(bash("fd foo ../other", "/w/ws/repo"), None);
        assert!(bash("fd -e rs", "/home/u").is_some_and(|r| r.contains("/home/u")));
        assert!(bash("fd foo /mnt/share/srv", "/w/ws").is_some());
        assert!(bash("fd foo ../..", "/w/ws/repo").is_some());
        assert!(bash("fd --search-path /mnt x", "/w/ws").is_some());
        assert!(bash("fd x --base-directory=/mnt", "/w/ws").is_some());
        assert_eq!(bash("fd x -x cat /etc/hosts", "/w/ws"), None);
        assert_eq!(bash("FIND_REASON='one file' fd -d 1 x /mnt", "/w/ws"), None);
    }

    #[test]
    fn rg_outside_ws_needs_a_reason() {
        assert_eq!(bash("rg foo", "/w/ws/repo"), None);
        assert_eq!(bash("rg '/api/v1' src", "/w/ws/repo"), None);
        assert_eq!(bash("rg -e x /w/ws/a ./b", "/w/ws"), None);
        assert_eq!(bash("dmesg | rg usb", "/"), None);
        assert_eq!(bash("rg root /etc/passwd", "/w/ws"), None);
        assert!(bash("rg foo", "/home/u").is_some_and(|r| r.contains("rg outside")));
        assert!(bash("rg -n foo /mnt/share/srv", "/w/ws").is_some());
        assert!(bash("rg -g '*.py' foo ~/..", "/w/ws").is_some());
        assert!(bash("false || rg foo", "/mnt").is_some());
        assert_eq!(bash("FIND_REASON=\"one dir\" rg foo /mnt/x", "/w/ws"), None);
    }

    #[test]
    fn heredoc_bodies_are_data() {
        let c = "git commit -F - <<'EOF'\nuse grep and find\nEOF";
        assert_eq!(bash(c, "/w/ws"), None);
    }

    #[test]
    fn other_tools_pass() {
        assert_eq!(
            judge("Read", &json!({ "file_path": "/x" }), Some(Path::new("/"))),
            None
        );
    }
}
