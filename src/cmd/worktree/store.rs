//! Review threads for one worktree, stored at
//! `<worktree>/.worktree-review/threads.json` (port of the Git Worktree Diff
//! extension's `review-store.js`). The store is excluded via the gitdir's
//! `info/exclude`, so the discussion never lands in a commit.
//!
//! Data stays an order-preserving [`J`] tree, as in the JS original, so
//! fields this port does not know about survive a rewrite untouched.

use super::json::{J, js_opt, parse};
use anyhow::{Result, anyhow, bail};
use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const STORE_DIR: &str = ".worktree-review";
const STORE_FILE: &str = "threads.json";
const DISCUSSION_FILE: &str = "DISCUSSION.md";
const VERSION: i64 = 1;

/// Statuses, and whether an agent may set each. Only the human resolves.
pub const STATUSES: [(&str, bool); 5] = [
    ("open", true),
    ("addressed", true),
    ("rejected", true),
    ("noted", true),
    ("resolved", false),
];

/// What an agent did about a comment, shown as a badge on its reply.
const ACTIONS: [&str; 5] = ["accepted", "rejected", "noted", "question", "review"];

pub fn status_names() -> Vec<&'static str> {
    STATUSES.iter().map(|(s, _)| *s).collect()
}

pub fn store_dir(wt: &Path) -> PathBuf {
    wt.join(STORE_DIR)
}
pub fn store_path(wt: &Path) -> PathBuf {
    store_dir(wt).join(STORE_FILE)
}
pub fn discussion_path(wt: &Path) -> PathBuf {
    store_dir(wt).join(DISCUSSION_FILE)
}

pub fn empty_store() -> J {
    J::obj()
        .with("version", J::int(VERSION))
        .with("threads", J::Arr(Vec::new()))
}

fn parse_store(raw: &str) -> Result<J> {
    let mut parsed = parse(raw).map_err(|e| anyhow!("{e}"))?;
    if !matches!(parsed, J::Obj(_)) || parsed.get("threads").and_then(J::as_arr).is_none() {
        return Ok(empty_store());
    }
    if !parsed.get("version").is_some_and(J::truthy) {
        parsed.set("version", J::int(VERSION));
    }
    Ok(parsed)
}

pub fn load(wt: &Path) -> Result<J> {
    let p = store_path(wt);
    match fs::read(&p) {
        Ok(bytes) => parse_store(&String::from_utf8_lossy(&bytes))
            .map_err(|e| anyhow!("unreadable review store at {}: {e}", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(empty_store()),
        Err(e) => bail!("unreadable review store at {}: {e}", p.display()),
    }
}

pub fn save(wt: &Path, store: &J) -> Result<()> {
    fs::create_dir_all(store_dir(wt))?;
    fs::write(store_path(wt), store.to_pretty() + "\n")?;
    ensure_local_ignore(wt);
    Ok(())
}

/// Add `/.worktree-review/` to `<gitdir>/info/exclude` once. A linked
/// worktree's `.git` is a file pointing at its gitdir. Best effort.
pub fn ensure_local_ignore(wt: &Path) -> bool {
    let line = format!("/{STORE_DIR}/");
    let dot_git = wt.join(".git");
    let Ok(meta) = fs::metadata(&dot_git) else {
        return false;
    };
    let git_dir = if meta.is_file() {
        let Ok(content) = fs::read_to_string(&dot_git) else {
            return false;
        };
        let Some(target) = content
            .lines()
            .find_map(|l| l.strip_prefix("gitdir:"))
            .map(|r| r.trim_start())
            .filter(|r| !r.is_empty())
        else {
            return false;
        };
        let target = Path::new(target.trim());
        if target.is_absolute() {
            target.to_owned()
        } else {
            wt.join(target)
        }
    } else {
        dot_git
    };
    let info = git_dir.join("info");
    let exclude = info.join("exclude");
    let existing = fs::read_to_string(&exclude).unwrap_or_default();
    if existing.split('\n').any(|l| js_trim(l) == line) {
        return false;
    }
    if fs::create_dir_all(&info).is_err() {
        return false;
    }
    let sep = if !existing.is_empty() && !existing.ends_with('\n') {
        "\n"
    } else {
        ""
    };
    use std::io::Write;
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&exclude)
        .and_then(|mut f| f.write_all(format!("{sep}{line}\n").as_bytes()))
        .is_ok()
}

/// `String.prototype.trim`: Rust's whitespace minus U+0085, plus U+FEFF.
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}')
}

/// `new Date().toISOString()`.
pub fn now_iso() -> String {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = crate::log::iso_from_unix(d.as_secs() as i64);
    format!("{}.{:03}Z", secs.trim_end_matches('Z'), d.subsec_millis())
}

pub fn threads(store: &J) -> &[J] {
    store
        .get("threads")
        .and_then(J::as_arr)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn threads_mut(store: &mut J) -> &mut Vec<J> {
    if store.get("threads").and_then(J::as_arr).is_none() {
        store.set("threads", J::Arr(Vec::new()));
    }
    store
        .get_mut("threads")
        .and_then(J::as_arr_mut)
        .expect("threads array")
}

pub fn comments(t: &J) -> &[J] {
    t.get("comments")
        .and_then(J::as_arr)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn next_id(store: &J) -> String {
    let used: Vec<&str> = threads(store)
        .iter()
        .filter_map(|t| t.get("id").and_then(J::as_str))
        .collect();
    (1..)
        .map(|i| format!("t{i}"))
        .find(|id| !used.contains(&id.as_str()))
        .expect("unbounded")
}

pub fn find_thread(store: &J, id: &str) -> Result<usize> {
    let ts = threads(store);
    ts.iter()
        .position(|t| t.get("id").and_then(J::as_str) == Some(id))
        .ok_or_else(|| {
            let have: Vec<String> = ts
                .iter()
                .map(|t| match t.get("id") {
                    None | Some(J::Null) => String::new(),
                    Some(v) => v.js(),
                })
                .collect();
            let have = have.join(", ");
            anyhow!(
                "no thread {id} (have: {})",
                if have.is_empty() { "none" } else { &have }
            )
        })
}

/// `normalizeAuthor`.
pub fn normalize_author(a: &J) -> J {
    let bot = a.get("kind").and_then(J::as_str) == Some("bot");
    let name = match a.get("name") {
        Some(n) if n.truthy() => n.clone(),
        _ => J::str(if bot { "agent" } else { "reviewer" }),
    };
    let mut out = J::obj()
        .with("name", name)
        .with("kind", J::str(if bot { "bot" } else { "human" }));
    for k in ["agent", "model"] {
        if let Some(v) = a.get(k).filter(|v| v.truthy()) {
            out.set(k, v.clone());
        }
    }
    out
}

pub struct NewThread<'a> {
    pub file: &'a str,
    pub start_line: i64,
    pub end_line: i64,
    pub body: &'a str,
    pub author: &'a J,
    pub anchor_text: &'a str,
    pub action: Option<&'a str>,
}

/// `addThread`: returns the new thread's index.
pub fn add_thread(store: &mut J, n: NewThread) -> Result<usize> {
    if n.file.is_empty() {
        bail!("addThread needs a file");
    }
    let stamp = now_iso();
    // `Math.max(1, startLine || 1)` / `Math.max(1, endLine || startLine || 1)`
    let start = n.start_line.max(1);
    let end = [n.end_line, n.start_line, 1]
        .into_iter()
        .find(|v| *v != 0)
        .unwrap_or(1)
        .max(1);
    let thread = J::obj()
        .with("id", J::str(next_id(store)))
        .with("file", J::str(n.file))
        .with("startLine", J::int(start))
        .with("endLine", J::int(end))
        .with("anchorText", J::str(n.anchor_text))
        .with("status", J::str("open"))
        .with("createdAt", J::str(&stamp))
        .with("updatedAt", J::str(&stamp))
        .with("comments", J::Arr(Vec::new()));
    let ts = threads_mut(store);
    ts.push(thread);
    let idx = ts.len() - 1;
    add_comment_at(store, idx, n.body, n.author, n.action, &stamp)?;
    Ok(idx)
}

/// `addComment`: returns the new comment.
pub fn add_comment(
    store: &mut J,
    id: &str,
    body: &str,
    author: &J,
    action: Option<&str>,
) -> Result<J> {
    let idx = find_thread(store, id)?;
    add_comment_at(store, idx, body, author, action, &now_iso())
}

fn add_comment_at(
    store: &mut J,
    idx: usize,
    body: &str,
    author: &J,
    action: Option<&str>,
    stamp: &str,
) -> Result<J> {
    let body = js_trim(body);
    if body.is_empty() {
        bail!("a comment needs a body");
    }
    let thread = &mut threads_mut(store)[idx];
    let tid = js_opt(thread.get("id"));
    let n = comments(thread).len() + 1;
    let mut comment = J::obj()
        .with("id", J::str(format!("{tid}.c{n}")))
        .with("author", normalize_author(author))
        .with("body", J::str(body))
        .with("createdAt", J::str(stamp));
    if let Some(a) = action.filter(|a| ACTIONS.contains(a)) {
        comment.set("action", J::str(a));
    }
    if thread.get("comments").and_then(J::as_arr).is_none() {
        thread.set("comments", J::Arr(Vec::new()));
    }
    thread
        .get_mut("comments")
        .and_then(J::as_arr_mut)
        .expect("comments array")
        .push(comment.clone());
    thread.set("updatedAt", J::str(stamp));
    Ok(comment)
}

/// `setStatus`.
pub fn set_status(store: &mut J, id: &str, status: &str, by_agent: bool) -> Result<()> {
    let Some((_, agent_ok)) = STATUSES.iter().find(|(s, _)| *s == status) else {
        bail!(
            "unknown status \"{status}\" (want one of {})",
            status_names().join(", ")
        );
    };
    if by_agent && !agent_ok {
        bail!("an agent may not set \"{status}\" — that is the reviewer's call");
    }
    let idx = find_thread(store, id)?;
    let thread = &mut threads_mut(store)[idx];
    thread.set("status", J::str(status));
    thread.set("updatedAt", J::str(now_iso()));
    Ok(())
}

pub fn is_open(t: &J) -> bool {
    t.get("status").and_then(J::as_str) != Some("resolved")
}

fn is_bot(author: Option<&J>) -> bool {
    author.and_then(|a| a.get("kind")).and_then(J::as_str) == Some("bot")
}

/// Threads an agent still has to answer: the last comment is not from a bot.
pub fn needs_reply(t: &J) -> bool {
    is_open(t) && comments(t).last().is_some_and(|c| !is_bot(c.get("author")))
}

#[derive(Debug, PartialEq)]
pub struct Counts {
    pub threads: usize,
    pub open: usize,
    pub resolved: usize,
    pub awaiting_agent: usize,
}

impl Counts {
    pub fn to_json(&self) -> J {
        let n = |v: usize| J::int(v as i64);
        J::obj()
            .with("threads", n(self.threads))
            .with("open", n(self.open))
            .with("resolved", n(self.resolved))
            .with("awaitingAgent", n(self.awaiting_agent))
    }
}

pub fn counts(store: &J) -> Counts {
    let ts = threads(store);
    let open = ts.iter().filter(|t| is_open(t)).count();
    Counts {
        threads: ts.len(),
        open,
        resolved: ts.len() - open,
        awaiting_agent: ts.iter().filter(|t| needs_reply(t)).count(),
    }
}

/// `${t.startLine}` or `${t.startLine}-${t.endLine}`.
pub fn line_span(t: &J) -> String {
    let (s, e) = (t.get("startLine"), t.get("endLine"));
    let wider = match (s.and_then(J::as_f64), e.and_then(J::as_f64)) {
        (Some(s), Some(e)) => e > s,
        _ => false,
    };
    if wider {
        format!("{}-{}", js_opt(s), js_opt(e))
    } else {
        js_opt(s)
    }
}

fn author_name(c: &J) -> String {
    js_opt(c.get("author").and_then(|a| a.get("name")))
}

fn who(author: Option<&J>) -> String {
    let get = |k: &str| author.and_then(|a| a.get(k));
    if !is_bot(author) {
        return js_opt(get("name"));
    }
    let name = get("name");
    let mut detail = Vec::new();
    if let Some(a) = get("agent").filter(|v| v.truthy()) {
        detail.push(a.js());
    }
    if let Some(m) = get("model").filter(|v| v.truthy() && Some(*v) != name) {
        detail.push(m.js());
    }
    let detail = detail.join(" · ");
    format!(
        "{} _({})_",
        js_opt(name),
        if detail.is_empty() { "bot" } else { &detail }
    )
}

/// `iso.replace('T', ' ').replace(/\..*$/, ' UTC')`.
fn when(iso: Option<&J>) -> String {
    let Some(s) = iso.and_then(J::as_str) else {
        return String::new();
    };
    let s = s.replacen('T', " ", 1);
    let line_end = |c: char| matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}');
    // First '.' from which no line terminator follows (`.` never matches one).
    let cut = s
        .char_indices()
        .filter(|(_, c)| *c == '.')
        .map(|(i, _)| i)
        .find(|&i| !s[i..].contains(line_end));
    match cut {
        Some(i) => format!("{} UTC", &s[..i]),
        None => s,
    }
}

fn first_line(c: Option<&J>) -> String {
    let Some(c) = c else {
        return "(no comment)".into();
    };
    let body = c.get("body").map(J::js).unwrap_or_default();
    let line = body
        .split('\n')
        .find(|l| !js_trim(l).is_empty())
        .unwrap_or_default();
    // `.slice(0, 120)` counts UTF-16 code units.
    let mut units = 0;
    line.chars()
        .take_while(|ch| {
            units += ch.len_utf16();
            units <= 120
        })
        .collect()
}

/// Approximates `String.prototype.localeCompare` (ICU root collation) for
/// file paths: punctuation < digits < letters, case-insensitive first with
/// lowercase before uppercase as the tie-break.
pub fn locale_cmp(a: &str, b: &str) -> Ordering {
    const PUNCT: &str = "_-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";
    fn primary(c: char) -> (u8, u32) {
        if c.is_whitespace() || c.is_control() {
            (0, c as u32)
        } else if let Some(i) = PUNCT.find(c) {
            (1, i as u32)
        } else if c.is_ascii_digit() {
            (3, c as u32)
        } else if c.is_ascii_alphabetic() {
            (4, c.to_ascii_lowercase() as u32)
        } else {
            (5, c.to_lowercase().next().unwrap_or(c) as u32)
        }
    }
    let key = |s: &str| s.chars().map(primary).collect::<Vec<_>>();
    let tertiary = |s: &str| s.chars().map(char::is_uppercase).collect::<Vec<_>>();
    key(a)
        .cmp(&key(b))
        .then_with(|| tertiary(a).cmp(&tertiary(b)))
        .then_with(|| a.cmp(b))
}

/// The whole discussion as markdown, in PR-review order (`render`).
pub fn render(store: &J, branch: &str, worktree: &Path) -> String {
    let c = counts(store);
    let title = if branch.is_empty() {
        worktree
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    } else {
        branch.to_owned()
    };
    let mut out: Vec<String> = vec![
        format!("# Review — {title}"),
        String::new(),
        format!(
            "{} thread{} · {} open · {} resolved{}",
            c.threads,
            if c.threads == 1 { "" } else { "s" },
            c.open,
            c.resolved,
            if c.awaiting_agent > 0 {
                format!(" · {} awaiting an agent", c.awaiting_agent)
            } else {
                String::new()
            }
        ),
        String::new(),
        "_Generated by Git Worktree Diff. Edit comments in the editor or through `bin/review`; this file is overwritten._".into(),
        String::new(),
    ];

    let ts = threads(store);
    let open: Vec<&J> = ts.iter().filter(|t| is_open(t)).collect();
    if !open.is_empty() {
        out.push("## Open items".into());
        out.push(String::new());
        for t in open {
            out.push(format!(
                "- [ ] **{}** `{}:{}` — {}{}: {}",
                js_opt(t.get("id")),
                js_opt(t.get("file")),
                js_opt(t.get("startLine")),
                js_opt(t.get("status")),
                if needs_reply(t) {
                    " · awaiting an agent"
                } else {
                    ""
                },
                first_line(comments(t).last())
            ));
        }
        out.push(String::new());
    }

    // Group by file in first-seen order, then sort files like localeCompare.
    let mut by_file: Vec<(String, Vec<&J>)> = Vec::new();
    for t in ts {
        let f = js_opt(t.get("file"));
        match by_file.iter_mut().find(|(k, _)| *k == f) {
            Some((_, v)) => v.push(t),
            None => by_file.push((f, vec![t])),
        }
    }
    by_file.sort_by(|a, b| locale_cmp(&a.0, &b.0));
    for (file, mut group) in by_file {
        out.push(format!("## `{file}`"));
        out.push(String::new());
        let line = |t: &J| t.get("startLine").and_then(J::as_f64).unwrap_or(f64::NAN);
        group.sort_by(|a, b| line(a).partial_cmp(&line(b)).unwrap_or(Ordering::Equal));
        for t in group {
            out.push(format!(
                "### {} · line {} · **{}**",
                js_opt(t.get("id")),
                line_span(t),
                js_opt(t.get("status"))
            ));
            out.push(String::new());
            if let Some(a) = t.get("anchorText").filter(|a| a.truthy()) {
                out.push("```".into());
                out.push(a.js());
                out.push("```".into());
                out.push(String::new());
            }
            for cm in comments(t) {
                let badge = match cm.get("action").filter(|a| a.truthy()) {
                    Some(a) => format!(" · `{}`", a.js()),
                    None => String::new(),
                };
                out.push(format!(
                    "**{}** · {}{badge}",
                    who(cm.get("author")),
                    when(cm.get("createdAt"))
                ));
                out.push(String::new());
                let body = cm.get("body").map(J::js).unwrap_or_default();
                out.push(
                    body.split('\n')
                        .map(|l| format!("> {l}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
                out.push(String::new());
            }
            out.push("---".into());
            out.push(String::new());
        }
    }
    if ts.is_empty() {
        out.push(
            "_No comments yet. Select code in a worktree file and add a review comment._".into(),
        );
        out.push(String::new());
    }
    out.join("\n")
}

/// `threadSummary` for `list --json`.
pub fn summary(t: &J) -> J {
    let mut o = J::obj();
    for k in ["id", "file", "startLine", "endLine", "status"] {
        if let Some(v) = t.get(k) {
            o.set(k, v.clone());
        }
    }
    o.set("awaitingAgent", J::Bool(needs_reply(t)));
    o.set("comments", J::int(comments(t).len() as i64));
    if let Some(last) = comments(t).last() {
        if let Some(n) = last.get("author").and_then(|a| a.get("name")) {
            o.set("lastAuthor", n.clone());
        }
        if let Some(b) = last.get("body") {
            o.set("lastBody", b.clone());
        }
    }
    o
}

/// `printThread` for the text listing.
pub fn print_thread(t: &J) -> String {
    let mut s = format!(
        "\n{}  {}:{}  [{}]{}\n",
        js_opt(t.get("id")),
        js_opt(t.get("file")),
        line_span(t),
        js_opt(t.get("status")),
        if needs_reply(t) {
            "  <- awaiting an agent"
        } else {
            ""
        }
    );
    if let Some(a) = t.get("anchorText").filter(|a| a.truthy()) {
        s.push_str(&format!("     | {}\n", a.js()));
    }
    for c in comments(t) {
        let author = c.get("author");
        let tag = if is_bot(author) {
            let agent = match author.and_then(|a| a.get("agent")).filter(|v| v.truthy()) {
                Some(a) => format!("/{}", a.js()),
                None => String::new(),
            };
            format!("{}{agent}", author_name(c))
        } else {
            author_name(c)
        };
        let action = match c.get("action").filter(|a| a.truthy()) {
            Some(a) => format!(" ({})", a.js()),
            None => String::new(),
        };
        let body = c.get("body").map(J::js).unwrap_or_default();
        s.push_str(&format!(
            "     {tag}{action}: {}\n",
            body.split('\n').collect::<Vec<_>>().join("\n       ")
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn human() -> J {
        J::obj()
            .with("name", J::str("alice"))
            .with("kind", J::str("human"))
    }
    fn bot() -> J {
        J::obj()
            .with("name", J::str("opus"))
            .with("kind", J::str("bot"))
            .with("agent", J::str("claude-code"))
            .with("model", J::str("opus"))
    }
    fn new<'a>(file: &'a str, line: i64, body: &'a str, author: &'a J) -> NewThread<'a> {
        NewThread {
            file,
            start_line: line,
            end_line: line,
            body,
            author,
            anchor_text: "",
            action: None,
        }
    }

    #[test]
    fn ids_and_awaiting() {
        let mut s = empty_store();
        let (h, b) = (human(), bot());
        add_thread(&mut s, new("a.rs", 3, " fix this \n", &h)).unwrap();
        add_thread(&mut s, new("b.rs", 0, "x", &h)).unwrap();
        let t = &threads(&s)[0];
        assert_eq!(t.get("id").unwrap().js(), "t1");
        assert_eq!(comments(t)[0].get("body").unwrap().js(), "fix this");
        assert_eq!(comments(t)[0].get("id").unwrap().js(), "t1.c1");
        assert_eq!(
            threads(&s)[1].get("startLine").unwrap().js(),
            "1",
            "clamped"
        );
        assert!(needs_reply(t));
        add_comment(&mut s, "t1", "done", &b, Some("accepted")).unwrap();
        assert!(!needs_reply(&threads(&s)[0]));
        assert_eq!(
            counts(&s),
            Counts {
                threads: 2,
                open: 2,
                resolved: 0,
                awaiting_agent: 1
            }
        );
        let c = &comments(&threads(&s)[0])[1];
        assert_eq!(c.get("id").unwrap().js(), "t1.c2");
        assert_eq!(c.get("action").unwrap().js(), "accepted");
    }

    #[test]
    fn next_id_fills_gaps() {
        let mut s = parse(r#"{"version":1,"threads":[{"id":"t2","comments":[]}]}"#).unwrap();
        let h = human();
        add_thread(&mut s, new("a", 1, "x", &h)).unwrap();
        assert_eq!(threads(&s)[1].get("id").unwrap().js(), "t1");
    }

    #[test]
    fn unknown_action_dropped_and_empty_body_rejected() {
        let mut s = empty_store();
        let h = human();
        add_thread(&mut s, new("a", 1, "x", &h)).unwrap();
        let c = add_comment(&mut s, "t1", "y", &h, Some("bogus")).unwrap();
        assert!(c.get("action").is_none());
        let e = add_comment(&mut s, "t1", " \n ", &h, None).unwrap_err();
        assert_eq!(e.to_string(), "a comment needs a body");
        let e = add_comment(&mut s, "t9", "y", &h, None).unwrap_err();
        assert_eq!(e.to_string(), "no thread t9 (have: t1)");
        let e = find_thread(&empty_store(), "t1").unwrap_err();
        assert_eq!(e.to_string(), "no thread t1 (have: none)");
    }

    #[test]
    fn status_rules() {
        let mut s = empty_store();
        let h = human();
        add_thread(&mut s, new("a", 1, "x", &h)).unwrap();
        let e = set_status(&mut s, "t1", "resolved", true).unwrap_err();
        assert!(e.to_string().contains("reviewer's call"), "{e}");
        let e = set_status(&mut s, "t1", "done", false).unwrap_err();
        assert_eq!(
            e.to_string(),
            "unknown status \"done\" (want one of open, addressed, rejected, noted, resolved)"
        );
        set_status(&mut s, "t1", "resolved", false).unwrap();
        assert!(!is_open(&threads(&s)[0]));
        assert!(!needs_reply(&threads(&s)[0]));
        assert_eq!(counts(&s).resolved, 1);
    }

    #[test]
    fn parse_store_shapes() {
        assert_eq!(parse_store("[]").unwrap(), empty_store());
        assert_eq!(parse_store(r#"{"threads":{}}"#).unwrap(), empty_store());
        // A missing version is appended, like `parsed.version = ... || 1`.
        assert_eq!(
            parse_store(r#"{"threads":[],"x":1}"#).unwrap().to_compact(),
            r#"{"threads":[],"x":1,"version":1}"#
        );
        assert!(parse_store("{").is_err());
    }

    #[test]
    fn author_normalisation() {
        let a = normalize_author(&J::obj().with("kind", J::str("bot")));
        assert_eq!(a.to_compact(), r#"{"name":"agent","kind":"bot"}"#);
        let a = normalize_author(&J::obj().with("name", J::str("")).with("agent", J::str("")));
        assert_eq!(a.to_compact(), r#"{"name":"reviewer","kind":"human"}"#);
    }

    #[test]
    fn who_and_when() {
        assert_eq!(who(Some(&bot())), "opus _(claude-code)_");
        let b = bot().with("name", J::str("me"));
        assert_eq!(who(Some(&b)), "me _(claude-code · opus)_");
        assert_eq!(
            who(Some(
                &J::obj()
                    .with("kind", J::str("bot"))
                    .with("name", J::str("x"))
            )),
            "x _(bot)_"
        );
        assert_eq!(who(Some(&human())), "alice");
        assert_eq!(
            when(Some(&J::str("2026-11-05T12:00:01.234Z"))),
            "2026-11-05 12:00:01 UTC"
        );
        assert_eq!(
            when(Some(&J::str("2026-11-05T12:00:01Z"))),
            "2026-11-05 12:00:01Z"
        );
        assert_eq!(when(None), "");
    }

    #[test]
    fn first_line_skips_blank_and_cuts_utf16() {
        let c = J::obj().with("body", J::str("\n  \nhello\nworld"));
        assert_eq!(first_line(Some(&c)), "hello");
        let long = "é".repeat(130);
        let c = J::obj().with("body", J::str(&long));
        assert_eq!(first_line(Some(&c)).chars().count(), 120);
        let astral = "😀".repeat(70);
        let c = J::obj().with("body", J::str(&astral));
        assert_eq!(first_line(Some(&c)).chars().count(), 60);
        assert_eq!(first_line(None), "(no comment)");
    }

    #[test]
    fn locale_order_matches_icu_for_paths() {
        // Order printed by node's localeCompare for these inputs.
        let want = [
            "a_rs",
            "a.rs",
            "a10",
            "a9",
            "b.rs",
            "README.md",
            "src/a_b.rs",
            "src/a-b.rs",
            "src/a.b",
            "src/a.rs",
            "src/A.rs",
            "src/a/b.rs",
            "src/ab.rs",
            "src/b.rs",
            "src2/x",
        ];
        let mut got = want.to_vec();
        got.reverse();
        got.sort_by(|a, b| locale_cmp(a, b));
        assert_eq!(got, want);
    }

    #[test]
    fn js_trim_set() {
        assert_eq!(js_trim("\u{feff} x \u{a0}"), "x");
        assert_eq!(js_trim("\u{85}x"), "\u{85}x");
    }

    #[test]
    fn local_ignore_once() {
        let d = std::env::temp_dir().join(format!("valet-wt-ignore-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("wt")).unwrap();
        fs::create_dir_all(d.join("gd/info")).unwrap();
        fs::write(d.join("gd/info/exclude"), "*.o").unwrap();
        fs::write(d.join("wt/.git"), "gitdir: ../gd\n").unwrap();
        assert!(ensure_local_ignore(&d.join("wt")));
        assert!(!ensure_local_ignore(&d.join("wt")));
        assert_eq!(
            fs::read_to_string(d.join("gd/info/exclude")).unwrap(),
            "*.o\n/.worktree-review/\n"
        );
        fs::remove_dir_all(&d).unwrap();
    }
}
