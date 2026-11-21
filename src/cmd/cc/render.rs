//! Formatting: GitHub markdown -> Slack mrkdwn, chunking, tool summaries, dialogs.
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

static RE_HEAD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*#{1,6}\s+(.*?)\s*#*\s*$").expect("valid regex"));
static RE_BULLET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\s*)[-*+]\s+").expect("valid regex"));
static RE_BOLD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\*\*(.+?)\*\*").expect("valid regex"));
static RE_BOLD2: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"__(.+?)__").expect("valid regex"));
static RE_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[([^\]]+)\]\(([^)\s]+)\)").expect("valid regex"));
static RE_SL_LABEL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<((?:https?|mailto):[^|>]+)\|[^>]*>").expect("valid regex"));
static RE_SL_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<((?:https?|mailto):[^>]+)>").expect("valid regex"));
static RE_OPTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(?:❯\s*)?(\d+)\.\s+(.*)$").expect("valid regex"));

/// (absolute prefix, "~/name") pairs: $HOME itself plus targets of top-level symlinks
/// in $HOME (e.g. ~/ws -> /mnt/data/ws), longest first.
static HOME_MAP: LazyLock<Vec<(String, String)>> = LazyLock::new(|| {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut m: Vec<(String, String)> = Vec::new();
    if home.is_empty() {
        return m;
    }
    if let Ok(rd) = std::fs::read_dir(&home) {
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_symlink() || !p.is_dir() {
                continue;
            }
            if let Ok(t) = std::fs::canonicalize(&p) {
                let t = t.to_string_lossy().to_string();
                if t.len() > 1 && !t.starts_with(&home) {
                    m.push((t, format!("~/{}", e.file_name().to_string_lossy())));
                }
            }
        }
    }
    m.push((home, "~".into()));
    m.sort_by_key(|(k, _)| std::cmp::Reverse(k.len()));
    m
});

/// Replace home / home-symlinked absolute paths with ~ notation.
pub fn tilde(s: &str) -> String {
    let mut out = s.to_string();
    for (abs, short) in HOME_MAP.iter() {
        if !out.contains(abs.as_str()) {
            continue;
        }
        // only at path boundaries: followed by '/', end, or a non-path char
        let mut res = String::with_capacity(out.len());
        let mut rest = out.as_str();
        while let Some(i) = rest.find(abs.as_str()) {
            let after = &rest[i + abs.len()..];
            let boundary = after.chars().next().is_none_or(|c| {
                c == '/' || !(c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
            });
            res.push_str(&rest[..i]);
            res.push_str(if boundary { short } else { abs });
            rest = after;
        }
        res.push_str(rest);
        out = res;
    }
    out
}

pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Slack message text -> plain text for the terminal.
pub fn unslack(s: &str) -> String {
    let s = RE_SL_LABEL.replace_all(s, "$1");
    let s = RE_SL_URL.replace_all(&s, "$1");
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

pub fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n).collect();
        t.push('…');
        t
    }
}

fn inline(line: &str) -> String {
    let l = esc(line);
    if let Some(c) = RE_HEAD.captures(&l) {
        return format!("*{}*", c[1].replace("**", ""));
    }
    let l = RE_BULLET.replace(&l, "$1• ");
    let l = RE_BOLD.replace_all(&l, "*$1*");
    let l = RE_BOLD2.replace_all(&l, "_${1}_");
    RE_LINK.replace_all(&l, "<$2|$1>").to_string()
}

/// GitHub-flavoured markdown (what Claude writes) -> Slack mrkdwn. Tables
/// become aligned monospace blocks ([`table`]).
pub fn md(s: &str) -> String {
    let s = tilde(s);
    let mut out: Vec<String> = Vec::new();
    let mut fence = false;
    let mut rows: Vec<&str> = Vec::new();
    let flush = |rows: &mut Vec<&str>, out: &mut Vec<String>| {
        if !rows.is_empty() {
            out.push("```".into());
            out.extend(table(rows).iter().map(|l| esc(l)));
            out.push("```".into());
            rows.clear();
        }
    };
    for line in s.lines() {
        let t = line.trim_start();
        if !fence && t.starts_with('|') {
            rows.push(t);
            continue;
        }
        flush(&mut rows, &mut out);
        if t.starts_with("```") {
            fence = !fence;
            out.push("```".into());
            continue;
        }
        out.push(if fence { esc(line) } else { inline(line) });
    }
    flush(&mut rows, &mut out);
    if fence {
        out.push("```".into());
    }
    out.join("\n")
}

/// Widest a rendered table gets before its widest columns wrap (Slack's
/// code blocks wrap at about this many monospace characters on a laptop).
const TABLE_WIDTH: usize = 96;
/// Narrowest a wrapped column gets.
const MIN_COL: usize = 10;
const GAP: &str = "  ";

/// A markdown table's lines as aligned plain-text rows: cells cleaned of
/// markdown, the `|---|` row replaced by a rule under the header, columns
/// padded, and wide cells word-wrapped so the table fits [`TABLE_WIDTH`].
pub fn table(lines: &[&str]) -> Vec<String> {
    let is_sep = |c: &[String]| {
        c.iter()
            .all(|x| !x.is_empty() && x.chars().all(|ch| matches!(ch, '-' | ':' | ' ')))
    };
    let mut rows: Vec<Vec<String>> = lines.iter().map(|l| cells(l)).collect();
    let header = rows.len() > 1 && is_sep(&rows[1]);
    if header {
        rows.remove(1);
    }
    let rows: Vec<Vec<String>> = rows.into_iter().filter(|r| !is_sep(r)).collect();
    grid(&rows, header)
}

/// ROWS of plain cells as aligned monospace lines: a rule under the first
/// row when HEADER, wide cells word-wrapped to fit [`TABLE_WIDTH`].
pub fn grid(rows: &[Vec<String>], header: bool) -> Vec<String> {
    let n = rows.iter().map(Vec::len).max().unwrap_or(0);
    if n == 0 {
        return Vec::new();
    }
    let width = |s: &str| s.chars().count();
    let mut w: Vec<usize> = (0..n)
        .map(|i| {
            rows.iter()
                .map(|r| r.get(i).map_or(0, |c| width(c)))
                .max()
                .unwrap_or(0)
        })
        .collect();
    let total = |w: &[usize]| w.iter().sum::<usize>() + GAP.len() * (n - 1);
    // Shrink the widest column, one character at a time, until it fits.
    while total(&w) > TABLE_WIDTH {
        let (i, &m) = w
            .iter()
            .enumerate()
            .max_by_key(|(_, x)| **x)
            .expect("n > 0");
        if m <= MIN_COL {
            break;
        }
        w[i] = m - 1;
    }
    let mut out = Vec::new();
    for (ri, r) in rows.iter().enumerate() {
        let wrapped: Vec<Vec<String>> = (0..n)
            .map(|i| wrap(r.get(i).map_or("", String::as_str), w[i]))
            .collect();
        let h = wrapped.iter().map(Vec::len).max().unwrap_or(1);
        for k in 0..h {
            let line: Vec<String> = (0..n)
                .map(|i| {
                    let c = wrapped[i].get(k).map_or("", String::as_str);
                    format!("{c}{}", " ".repeat(w[i].saturating_sub(width(c))))
                })
                .collect();
            out.push(line.join(GAP).trim_end().to_owned());
        }
        if ri == 0 && header {
            let rule: Vec<String> = w.iter().map(|&x| "─".repeat(x)).collect();
            out.push(rule.join(GAP));
        }
    }
    out
}

/// A table line's cells, markdown removed: `**b**` and `` `c` `` lose their
/// markers, `[t](u)` keeps t, `\|` is a literal bar.
fn cells(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                cur.push('|');
                chars.next();
            }
            '|' => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out.iter()
        .map(|c| {
            let c = RE_LINK.replace_all(c.trim(), "$1");
            let c = RE_BOLD.replace_all(&c, "$1");
            let c = RE_BOLD2.replace_all(&c, "$1");
            c.replace('`', "")
        })
        .collect()
}

/// S word-wrapped to lines of at most W characters (a longer word is cut).
fn wrap(s: &str, w: usize) -> Vec<String> {
    let mut lines = vec![String::new()];
    for word in s.split_whitespace() {
        let mut word: Vec<char> = word.chars().collect();
        loop {
            let last = lines.last_mut().expect("non-empty");
            let len = last.chars().count();
            let need = word.len() + usize::from(len > 0);
            if len + need <= w {
                if len > 0 {
                    last.push(' ');
                }
                last.extend(word.iter());
                break;
            }
            if len > 0 {
                lines.push(String::new());
                continue;
            }
            last.extend(word[..w].iter());
            word.drain(..w);
            lines.push(String::new());
        }
    }
    if lines.len() > 1 && lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

fn split_long(line: &str, n: usize) -> Vec<String> {
    let chars: Vec<char> = line.chars().collect();
    if chars.len() <= n {
        return vec![line.to_string()];
    }
    chars.chunks(n).map(|c| c.iter().collect()).collect()
}

/// Split into Slack-sized messages on line boundaries, keeping code fences balanced.
pub fn chunk(s: &str, limit: usize) -> Vec<String> {
    if s.len() <= limit {
        return vec![s.to_string()];
    }
    let mut chunks = Vec::new();
    let mut cur = String::new();
    let mut fence = false;
    for line in s.split('\n') {
        for p in split_long(line, limit / 2) {
            if !cur.is_empty() && cur.len() + p.len() + 8 > limit {
                if fence {
                    cur.push_str("\n```");
                }
                chunks.push(std::mem::take(&mut cur));
                if fence {
                    cur.push_str("```");
                }
            }
            if !cur.is_empty() {
                cur.push('\n');
            }
            cur.push_str(&p);
            if p.trim_start().starts_with("```") {
                fence = !fence;
            }
        }
    }
    if !cur.trim().is_empty() {
        chunks.push(cur);
    }
    chunks
}

fn first_str(v: &Value) -> String {
    v.as_object()
        .and_then(|o| o.values().find_map(|x| x.as_str().map(str::to_string)))
        .unwrap_or_default()
}

/// One compact line describing a tool call.
pub fn tool_line(name: &str, input: &Value) -> String {
    let s = |k: &str| input[k].as_str().unwrap_or_default().to_string();
    let detail = match name {
        "Bash" | "PowerShell" => s("command"),
        "Read" | "Write" | "Edit" | "MultiEdit" => s("file_path"),
        "NotebookEdit" => s("notebook_path"),
        "Grep" => format!("{} {}", s("pattern"), s("path")),
        "Glob" => s("pattern"),
        "WebFetch" => s("url"),
        "WebSearch" => s("query"),
        "Task" | "Agent" => s("description"),
        "Skill" => format!("{} {}", s("skill"), s("args")),
        "TodoWrite" => {
            let todos = input["todos"].as_array().cloned().unwrap_or_default();
            let done = todos.iter().filter(|t| t["status"] == "completed").count();
            let cur = todos
                .iter()
                .find(|t| t["status"] == "in_progress")
                .and_then(|t| t["activeForm"].as_str().or(t["content"].as_str()))
                .unwrap_or("");
            format!("{done}/{} done {cur}", todos.len())
        }
        _ => first_str(input),
    };
    let detail = trunc(&norm(&tilde(&detail)), 150).replace('`', "'");
    let icon = match name {
        "Bash" | "PowerShell" => "💻",
        "Read" => "📖",
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => "✏️",
        "Grep" | "Glob" => "🔎",
        "WebFetch" | "WebSearch" => "🌐",
        "Task" | "Agent" => "🤖",
        "TodoWrite" => "📝",
        _ => "🔧",
    };
    if detail.is_empty() {
        format!("{icon} *{name}*")
    } else {
        format!("{icon} *{name}* `{}`", esc(&detail))
    }
}

/// Render an AskUserQuestion tool input.
pub fn question(input: &Value) -> String {
    let qs = input["questions"].as_array().cloned().unwrap_or_default();
    let mut out = vec!["❓ *Claude is asking:*".to_string()];
    let many = qs.len() > 1;
    for (i, q) in qs.iter().enumerate() {
        let head = q["header"].as_str().unwrap_or("");
        let text = q["question"].as_str().unwrap_or("");
        let multi = q["multiSelect"].as_bool() == Some(true);
        out.push(String::new());
        let num = if many {
            format!("{}. ", i + 1)
        } else {
            String::new()
        };
        out.push(format!("*{num}{}* {}", esc(head), md(text)));
        let opts = q["options"].as_array().cloned().unwrap_or_default();
        for (j, o) in opts.iter().enumerate() {
            let label = o["label"].as_str().unwrap_or("");
            let desc = o["description"].as_str().unwrap_or("");
            if desc.is_empty() || desc == label {
                out.push(format!("   `{}`  {}", j + 1, esc(label)));
            } else {
                out.push(format!("   `{}`  *{}* — {}", j + 1, esc(label), esc(desc)));
            }
        }
        out.push(format!(
            "   `{}`  _Type something (reply with free text)_",
            opts.len() + 1
        ));
        if multi {
            out.push("   _multi-select: send the numbers to toggle, then `!enter`_".into());
        }
    }
    out.push(String::new());
    out.push(if many {
        "_Reply with an option number for the current question (questions are answered one at a time)._".into()
    } else {
        "_Reply with an option number, or just type your answer._".into()
    });
    out.join("\n")
}

fn is_rule(l: &str) -> bool {
    let t = l.trim();
    t.chars().count() >= 20 && t.chars().all(|c| c == '─' || c == '━')
}

/// Extract the interactive dialog region from a Claude Code screen snapshot.
pub fn dialog(screen: &str) -> String {
    let lines: Vec<&str> = screen.lines().map(|l| l.trim_end()).collect();
    let footer = lines.iter().rposition(|l| {
        l.contains("Esc to cancel")
            || l.contains("Enter to select")
            || l.contains("Enter to confirm")
    });
    let end = footer.map(|i| i + 1).unwrap_or(lines.len());
    let cursor = lines[..end]
        .iter()
        .rposition(|l| l.trim_start().starts_with('❯') && RE_OPTION.is_match(l))
        .or_else(|| {
            lines[..end]
                .iter()
                .rposition(|l| l.trim_start().starts_with('❯'))
        });
    let start = match cursor {
        Some(c) => lines[..c]
            .iter()
            .rposition(|l| is_rule(l))
            .map(|i| i + 1)
            .unwrap_or(c.saturating_sub(15)),
        None => end.saturating_sub(15),
    };
    let mut out: Vec<String> = Vec::new();
    let lines: Vec<String> = lines.iter().map(|l| tilde(l)).collect();
    for l in &lines[start..end] {
        if is_rule(l) || (!l.trim().is_empty() && l.trim().chars().all(|c| c == '╌')) {
            out.push("──────".into());
            continue;
        }
        out.push(l.to_string());
    }
    while out.first().is_some_and(|l| l.trim().is_empty()) {
        out.remove(0);
    }
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    // De-indent common leading whitespace.
    let ind = out
        .iter()
        .filter(|l| !l.trim().is_empty() && l.as_str() != "──────")
        .map(|l| l.len() - l.trim_start_matches(' ').len())
        .min()
        .unwrap_or(0);
    let out: Vec<String> = out
        .iter()
        .map(|l| {
            let lead = l.len() - l.trim_start_matches(' ').len();
            l[lead.min(ind)..].to_string()
        })
        .collect();
    out.into_iter().take(45).collect::<Vec<_>>().join("\n")
}

/// Option number of the "Type something" entry in a dialog, if any.
pub fn type_something_option(dialog: &str) -> Option<String> {
    dialog.lines().find_map(|l| {
        let c = RE_OPTION.captures(l)?;
        let label = c[2].to_lowercase();
        (label.starts_with("type something") || label.starts_with("other"))
            .then(|| c[1].to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md_conversion() {
        let s = md(
            "# Title\n**bold** and [x](http://a.b)\n- item\n```rust\nlet a = 1 < 2;\n```\n| a | b |\n|---|---|",
        );
        assert!(s.contains("*Title*"));
        assert!(s.contains("*bold*"));
        assert!(s.contains("<http://a.b|x>"));
        assert!(s.contains("• item"));
        assert!(s.contains("1 &lt; 2"));
        assert!(s.ends_with("```"));
    }

    #[test]
    fn tables_align_and_lose_markdown() {
        let t = table(&[
            "| Skill | Lines | What |",
            "|---|---:|---|",
            "| `cb-ssh` | 231 → 53 | **Rewritten** around [valet](http://x) |",
            "| journal | 62 | a \\| b |",
        ]);
        assert_eq!(
            t,
            [
                "Skill    Lines     What",
                "───────  ────────  ──────────────────────",
                "cb-ssh   231 → 53  Rewritten around valet",
                "journal  62        a | b",
            ]
        );
        let m = md("before\n| a | b |\n|---|---|\n| 1 | <2> |\nafter");
        assert_eq!(m, "before\n```\na  b\n─  ───\n1  &lt;2&gt;\n```\nafter");
    }

    #[test]
    fn wide_tables_wrap_inside_columns() {
        let long = "word ".repeat(40);
        let row = format!("| key | {long} |");
        let t = table(&["| k | v |", "|---|---|", &row]);
        assert!(t.iter().all(|l| l.chars().count() <= TABLE_WIDTH), "{t:?}");
        assert!(t.len() > 4, "{t:?}");
        assert!(t[2].starts_with("key  word"));
        assert!(t[3].starts_with("     word"));
    }

    #[test]
    fn tilde_home() {
        let h = std::env::var("HOME").unwrap();
        assert_eq!(tilde(&format!("cd {h}/src && ls {h}")), "cd ~/src && ls ~");
        assert_eq!(tilde(&format!("{h}x/y")), format!("{h}x/y"));
    }

    #[test]
    fn chunking_balances_fences() {
        let body = format!("intro\n```\n{}\n```\nend", "line of code\n".repeat(600));
        let parts = chunk(&body, 3000);
        assert!(parts.len() > 1);
        for p in &parts {
            assert!(p.len() <= 3000);
            assert_eq!(p.matches("```").count() % 2, 0, "unbalanced: {p}");
        }
    }

    #[test]
    fn dialog_ask() {
        let scr = "❯ do it\n────────────────────────────────────────\n ☐ Color\n\nWhat is your favorite color?\n\n❯ 1. Red\n     Red\n  2. Green\n  3. Blue\n  4. Type something.\n────────────────────────────────────────\n  5. Chat about this\n\nEnter to select · ↑/↓ to navigate · Esc to cancel\n";
        let d = dialog(scr);
        assert!(d.trim_start().starts_with("☐ Color"), "{d}");
        assert!(d.contains("5. Chat about this"));
        assert_eq!(type_something_option(&d).as_deref(), Some("4"));
    }

    #[test]
    fn dialog_permission() {
        let scr = "  ⎿  $ touch x\n\n────────────────────────────────────────\n Bash command\n Create marker\n╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌\n touch x\n╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌\n Do you want to proceed?\n ❯ 1. Yes\n   2. No\n\n Esc to cancel · Tab to amend\n";
        let d = dialog(scr);
        assert!(d.trim_start().starts_with("Bash command"), "{d}");
        assert!(d.contains("touch x"));
        assert!(d.ends_with("Esc to cancel · Tab to amend"));
    }
}
