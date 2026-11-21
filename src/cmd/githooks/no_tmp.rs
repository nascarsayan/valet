//! `valet hook claude no-tmp`: a Claude Code PreToolUse hook that keeps the
//! agent from creating files or directories under /tmp or /var/tmp (shared,
//! small, wiped at reboot, readable by others). Reading there stays allowed.
//!
//! Denied: Write/Edit/MultiEdit/NotebookEdit of a path there, and Bash that
//! writes there (redirection; tee, touch, mkdir, cp/mv destination, sed -i,
//! ...; curl -o, wget -O, tar -C, unzip -d, git clone), `cd` into it, and
//! `mktemp` without a directory outside it. A heuristic: it catches the usual
//! forms, not every program that can write a file.
//!
//! Throw-away files go in `<Userland>/Scratch/<task>/` ([`crate::cmd::apps::scratch`]),
//! which the daily clean job empties. A Bash command that really needs
//! /tmp says why: `TMP_WRITE_REASON="<why>" <command>`.

use super::skill_source::{emit, lexical, read_stdin, write_target_by};
use serde_json::{Value, json};
use std::path::Path;

/// Places the hook keeps the agent out of, and `$TMPDIR` (usually /tmp).
const TMP: &[&str] = &["/tmp", "/var/tmp", "$TMPDIR", "${TMPDIR}"];

/// Leading assignment that lets one Bash command write to /tmp.
const REASON_ENV: &str = "TMP_WRITE_REASON";

/// Deny message, naming `<Userland>/Scratch` as the place to use.
fn advice() -> String {
    let place = crate::cmd::apps::scratch().map_or_else(
        |_| "$USERLAND/Scratch".to_owned(),
        |d| d.display().to_string(),
    );
    let days = crate::cmd::apps::SCRATCH_DAYS;
    format!(
        "valet hook claude no-tmp: do not create files or directories under /tmp or /var/tmp (shared, small, wiped). Put throw-away files in {place}/<short-task-name>/ (mkdir -p it; deleted {days} days after its last change); $XDG_RUNTIME_DIR for anything secret; for mktemp pass -p <dir>. Reading /tmp is fine. If a Bash command truly needs /tmp, prefix it with {REASON_ENV}=\"<why>\"."
    )
}

/// The command sets a non-empty [`REASON_ENV`] (outside here-documents).
fn has_reason(cmd: &str) -> bool {
    let re = regex::Regex::new(&format!(
        r#"(?:^|[\s;&|(])(?:export\s+)?{REASON_ENV}=(?:"[^"]+"|'[^']+'|[^\s"';&|]+)"#
    ))
    .expect("valid regex");
    re.is_match(cmd)
}

/// `word` (quotes trimmed) names /tmp, /var/tmp or $TMPDIR, or a path in them.
pub fn in_tmp(word: &str) -> bool {
    let w = word.trim_matches(['\'', '"']);
    let w = if w.starts_with('/') {
        lexical(Path::new(w)).display().to_string()
    } else {
        w.to_owned()
    };
    TMP.iter()
        .any(|t| w == *t || w.strip_prefix(t).is_some_and(|rest| rest.starts_with('/')))
}

/// Shell words of each simple command in `cmd` (split on ; | & newlines,
/// parentheses and `$(`), leading env assignments, sudo, env and command
/// skipped.
pub(super) fn commands(cmd: &str) -> Vec<Vec<String>> {
    cmd.split(['\n', ';', '|', '&', '(', ')', '`'])
        .map(|seg| {
            let words: Vec<String> = seg
                .split_whitespace()
                .map(|w| w.trim_matches(['\'', '"']).to_owned())
                .collect();
            let skip = words
                .iter()
                .take_while(|w| {
                    matches!(w.as_str(), "sudo" | "env" | "command" | "exec" | "$")
                        || w.contains('=') && !w.starts_with('-')
                })
                .count();
            words[skip..].to_vec()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

/// `cmd` without here-document bodies (`<<EOF` ... `EOF`): they are data,
/// such as a commit message that mentions /tmp, not commands.
pub(super) fn strip_heredocs(cmd: &str) -> String {
    let start =
        regex::Regex::new(r#"<<-?\s*['"]?([A-Za-z_][A-Za-z0-9_]*)['"]?"#).expect("valid regex");
    let mut out = String::new();
    let mut end: Option<String> = None;
    for line in cmd.lines() {
        if let Some(d) = &end {
            if line.trim() == d {
                end = None;
            }
            continue;
        }
        if let Some(c) = start.captures(line) {
            end = Some(c[1].to_owned());
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Why a writing command in `cmd` is refused, if one is.
fn judge_bash(cmd: &str) -> Option<String> {
    if !TMP.iter().any(|t| cmd.contains(t.trim_start_matches('$'))) && !cmd.contains("mktemp") {
        return None; // fast path
    }
    let cmd = &strip_heredocs(cmd);
    if has_reason(cmd) {
        return None;
    }
    if let Some(p) = write_target_by(cmd, in_tmp) {
        return Some(format!("writes {p}"));
    }
    for w in commands(cmd) {
        let prog = w[0].rsplit('/').next().unwrap_or(&w[0]);
        let args = &w[1..];
        let hit = match prog {
            "cd" | "pushd" => args.first().is_some_and(|a| in_tmp(a)),
            "git" => args.first().is_some_and(|a| a == "clone") && args.iter().any(|a| in_tmp(a)),
            "curl" | "wget" | "tar" | "unzip" | "7z" | "bsdtar" => args
                .iter()
                .any(|a| in_tmp(a) || a.split_once('=').is_some_and(|(_, v)| in_tmp(v))),
            "mktemp" => mktemp_in_tmp(args),
            _ => false,
        };
        if hit {
            return Some(format!("`{}`", w.join(" ")));
        }
    }
    None
}

/// mktemp creates in $TMPDIR (/tmp) unless given `-p DIR`, `--tmpdir=DIR`
/// or a template with a directory; true when it would land in /tmp.
fn mktemp_in_tmp(args: &[String]) -> bool {
    let mut dir: Option<&str> = None;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "-p" || a == "--tmpdir" {
            dir = args.get(i + 1).map(String::as_str);
            i += 1;
        } else if let Some(d) = a.strip_prefix("--tmpdir=") {
            dir = Some(d);
        } else if let Some(d) = a.strip_prefix("-p").filter(|d| !d.is_empty()) {
            dir = Some(d);
        } else if !a.starts_with('-') && a.contains('/') {
            return in_tmp(a);
        }
        i += 1;
    }
    dir.is_none_or(in_tmp)
}

/// The deny reason for one tool call, if it is refused.
pub fn judge(tool: &str, input: &Value) -> Option<String> {
    let path = match tool {
        "Write" | "Edit" | "MultiEdit" => input["file_path"].as_str(),
        "NotebookEdit" => input["notebook_path"].as_str(),
        "Bash" => {
            return judge_bash(input["command"].as_str()?)
                .map(|w| format!("{} (refused: {w})", advice()));
        }
        _ => return None,
    }?;
    in_tmp(path).then(|| format!("{} (refused: {path})", advice()))
}

/// `valet hook claude no-tmp`: prints a deny decision only when there is one;
/// always exits 0.
pub fn pre_tool_use() {
    let Some(input) = read_stdin() else { return };
    let tool = input["tool_name"].as_str().unwrap_or_default();
    if let Some(reason) = judge(tool, &input["tool_input"]) {
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

    fn bash(c: &str) -> Option<String> {
        judge("Bash", &json!({ "command": c }))
    }

    #[test]
    fn file_tools_under_tmp_are_refused() {
        for p in ["/tmp/x.md", "/var/tmp/a/b", "/tmp", "/home/u/../../tmp/x"] {
            assert!(judge("Write", &json!({ "file_path": p })).is_some(), "{p}");
        }
        assert!(judge("NotebookEdit", &json!({ "notebook_path": "/tmp/n.ipynb" })).is_some());
        for p in ["/tmpfoo/x", "/home/u/tmp/x", "/srv/tmp.txt"] {
            assert!(judge("Edit", &json!({ "file_path": p })).is_none(), "{p}");
        }
        assert!(judge("Read", &json!({ "file_path": "/tmp/x" })).is_none());
    }

    #[test]
    fn bash_writes_to_tmp_are_refused() {
        for c in [
            "echo hi > /tmp/x",
            "make 2>/tmp/err.log",
            "mkdir -p /tmp/work && cd /tmp/work",
            "cd /tmp && git clone https://x/y",
            "touch \"/tmp/a b\"",
            "cp a.txt /tmp/",
            "tee /var/tmp/log < in",
            "curl -fsSL -o /tmp/f https://x",
            "tar -C /tmp -xzf a.tgz",
            "d=$(mktemp -d)",
            "f=$(mktemp)",
            "mktemp -p /tmp",
            "mktemp -t x.XXXX",
            "echo > $TMPDIR/x",
            "sed -i s/a/b/ /tmp/f",
            "cat > /tmp/x <<'EOF'\nhi\nEOF",
            "git clone https://x/y /tmp/y",
        ] {
            assert!(bash(c).is_some(), "{c}");
        }
    }

    #[test]
    fn bash_reads_and_other_dirs_pass() {
        for c in [
            "cat /tmp/x.log",
            "ls -la /tmp",
            "tail -f /tmp/claude-2538/tasks/x.output",
            "grep -r foo /tmp/logs",
            "cp /tmp/x ~/ws/projects/alice/t/",
            "mktemp -d -p ~/ws/projects/alice/t",
            "mktemp --tmpdir=/srv/x",
            "mktemp ~/ws/projects/alice/t/f.XXXX",
            "git commit -m \"note: cd /tmp is refused\"",
            "git commit -F - <<'EOF'\nruns `mktemp` without -p; echo > /tmp/x\nEOF",
            "python3 - <<'PY'\nopen('/tmp/x','w')\nPY\ncargo test",
            "echo hi > ~/ws/x",
            "cargo test",
            "TMP_WRITE_REASON=\"tool hardcodes /tmp\" foo > /tmp/x",
            "export TMP_WRITE_REASON='needs tmpfs'; mkdir /tmp/y",
        ] {
            assert_eq!(bash(c), None, "{c}");
        }
    }

    #[test]
    fn empty_or_heredoc_reason_does_not_bypass() {
        for c in [
            "TMP_WRITE_REASON= echo > /tmp/x",
            "TMP_WRITE_REASON=\"\" echo > /tmp/x",
            "cat <<EOF\nTMP_WRITE_REASON=x\nEOF\necho > /tmp/x",
            "MY_TMP_WRITE_REASON=x echo > /tmp/x",
        ] {
            assert!(bash(c).is_some(), "{c}");
        }
    }

    #[test]
    fn advice_names_userland_scratch() {
        let r = judge("Write", &json!({ "file_path": "/tmp/x" })).unwrap();
        assert!(r.contains("/Scratch/<short-task-name>/"), "{r}");
        assert!(r.contains("TMP_WRITE_REASON"), "{r}");
    }
}
