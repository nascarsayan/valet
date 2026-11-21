//! `git.ascii` pre-commit hook: refuse newly added characters that hide or
//! change what code does. Other non-ASCII text (prose, emoji, arrows,
//! accented names, box drawing) passes.
//!
//! - Invisible: bidi controls (Trojan Source), zero-width characters, odd
//!   spaces such as U+00A0. Checked in every file, comments included.
//! - Lookalikes of ASCII syntax: smart quotes, dashes and minus for `-`,
//!   fullwidth ASCII, Greek question mark. Checked in code files only,
//!   outside comments and `"..."` strings, where they break a command or
//!   a parser; markup (Markdown, HTML, Astro) gets the invisible check only.

use super::unicode_names;
use crate::proc;
use anyhow::{Result, bail};
use std::collections::HashSet;

const CODE_SUFFIXES: &[&str] = &[
    ".bash", ".c", ".cc", ".cfg", ".conf", ".cpp", ".css", ".go", ".h", ".ini", ".java", ".js",
    ".json", ".jsonnet", ".jsx", ".jinja2", ".lua", ".pl", ".py", ".rb", ".rs", ".sh", ".sql",
    ".tf", ".toml", ".tpl", ".ts", ".tsx", ".yaml", ".yml",
];
const CODE_NAMES: &[&str] = &["Dockerfile", "Makefile", "Chart.yaml"];
const J2_SUFFIXES: [&str; 3] = [".sh.j2", ".yaml.j2", ".yml.j2"];

/// Invisible or direction-changing characters (dangerous anywhere).
fn invisible(c: char) -> bool {
    matches!(c,
        '\u{00A0}' | '\u{00AD}' | '\u{061C}' | '\u{1680}' | '\u{180E}'
        | '\u{2000}'..='\u{200F}' | '\u{2028}'..='\u{202F}'
        | '\u{205F}'..='\u{206F}' | '\u{3000}' | '\u{FEFF}')
}

/// Characters that pass for ASCII syntax (quotes, `-`, fullwidth forms).
fn lookalike(c: char) -> bool {
    matches!(c,
        '\u{00B4}' | '\u{02BC}' | '\u{037E}'
        | '\u{2010}'..='\u{2013}' | '\u{2018}'..='\u{201F}'
        | '\u{2032}'..='\u{2037}' | '\u{2044}' | '\u{2212}' | '\u{2215}'
        | '\u{FE63}' | '\u{FF01}'..='\u{FF5E}')
}

/// Comment openers for a code file, by suffix (`#` when unknown).
fn comment_markers(path: &str) -> &'static [&'static str] {
    let (_, suffix) = name_suffix(path);
    match suffix.to_lowercase().as_str() {
        ".c" | ".cc" | ".cpp" | ".h" | ".go" | ".java" | ".js" | ".jsx" | ".ts" | ".tsx"
        | ".rs" | ".jsonnet" | ".css" | ".json" => &["//", "/*"],
        ".sql" | ".lua" => &["--"],
        ".jinja2" | ".j2" => &["#", "{#"],
        _ => &["#"],
    }
}

/// String delimiters for a code file: `"`; `'` and backtick template
/// literals in JavaScript and TypeScript; `'` in Python. Shell keeps only
/// `"` (single-quoted text there is often a command), Rust too (lifetimes).
fn quotes(path: &str) -> &'static [char] {
    let (_, suffix) = name_suffix(path);
    match suffix.to_lowercase().as_str() {
        ".js" | ".jsx" | ".ts" | ".tsx" | ".mjs" | ".cjs" => &['"', '`', '\''],
        ".py" => &['"', '\''],
        _ => &['"'],
    }
}

/// Byte offset where a comment starts on `line`, if one does: a marker at
/// line start or after whitespace, outside strings; or a `*` line inside a
/// block comment.
fn comment_start(line: &str, markers: &[&str], quotes: &[char]) -> Option<usize> {
    let t = line.trim_start();
    if markers.contains(&"/*") && (t.starts_with("* ") || t == "*" || t.starts_with("*/")) {
        return Some(line.len() - t.len());
    }
    let mut open: Option<char> = None;
    let mut prev = ' ';
    for (i, c) in line.char_indices() {
        if quotes.contains(&c) && prev != '\\' && open.is_none_or(|q| q == c) {
            open = if open.is_some() { None } else { Some(c) };
        } else if open.is_none()
            && prev.is_whitespace()
            && markers.iter().any(|m| line[i..].starts_with(m))
        {
            return Some(i);
        }
        prev = c;
    }
    None
}

/// Byte offsets of `line` that are code: not in a comment or a string.
fn code_mask(line: &str, markers: &[&str], quotes: &[char]) -> Vec<bool> {
    let end = comment_start(line, markers, quotes).unwrap_or(line.len());
    let mut open: Option<char> = None;
    let mut prev = ' ';
    let mut mask = vec![false; line.len()];
    for (i, c) in line.char_indices() {
        if quotes.contains(&c) && prev != '\\' && open.is_none_or(|q| q == c) {
            open = if open.is_some() { None } else { Some(c) };
        } else if open.is_none() && i < end {
            mask[i] = true;
        }
        prev = c;
    }
    mask
}

/// A path as Python's `str(pathlib.PurePosixPath(p))` spells it.
fn py_path(p: &str) -> String {
    let root = if p.starts_with("//") && !p.starts_with("///") {
        "//"
    } else if p.starts_with('/') {
        "/"
    } else {
        ""
    };
    let parts: Vec<&str> = p
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    if root.is_empty() && parts.is_empty() {
        return ".".into();
    }
    format!("{root}{}", parts.join("/"))
}

/// `(name, suffix)` as pathlib defines them.
fn name_suffix(path: &str) -> (&str, &str) {
    let name = path.rsplit('/').next().unwrap_or("");
    let name = if name == "." { "" } else { name };
    let suffix = match name.rfind('.') {
        Some(i) if i > 0 && i < name.len() - 1 => &name[i..],
        _ => "",
    };
    (name, suffix)
}

fn code_name(path: &str) -> (bool, bool) {
    let (name, suffix) = name_suffix(path);
    let known = CODE_SUFFIXES.contains(&suffix.to_lowercase().as_str())
        || CODE_NAMES.contains(&name)
        || J2_SUFFIXES.iter().any(|s| name.ends_with(s));
    (known, suffix.is_empty())
}

/// Split like Python's `bytes.splitlines()`: on \n, \r and \r\n.
fn byte_lines(s: &str) -> Vec<&str> {
    split_lines(s, |c| c == '\n' || c == '\r')
}

fn split_lines(s: &str, brk: impl Fn(char) -> bool) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut it = s.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        if brk(c) {
            out.push(&s[start..i]);
            start = i + c.len_utf8();
            if c == '\r' && it.peek().map(|p| p.1) == Some('\n') {
                it.next();
                start += 1;
            }
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// Refused characters in `line` of `path`: (1-based column, report).
fn findings(path: &str, line: &str) -> Vec<(usize, String)> {
    let (known, bare) = code_name(path);
    let code = (known || bare).then(|| code_mask(line, comment_markers(path), quotes(path)));
    line.char_indices()
        .enumerate()
        .filter(|&(col, (i, c))| {
            // A byte-order mark is fine as a file's first character.
            (invisible(c) && !(c == '\u{FEFF}' && col == 0))
                || lookalike(c) && code.as_ref().is_some_and(|m| m[i])
        })
        .map(|(col, (_, c))| {
            let name = unicode_names::name(c).unwrap_or_else(|| "UNKNOWN".into());
            (col + 1, format!("U+{:04X} {name}", c as u32))
        })
        .collect()
}

/// (path, new line number, text) for lines the index adds relative to `rev`
/// (default HEAD).
fn added_lines_against(rev: Option<&str>) -> Result<Vec<(String, usize, String)>> {
    let o = proc::Cmd::new("git")
        .args([
            "diff",
            "--cached",
            "--no-color",
            "--unified=0",
            "--find-renames",
            "--diff-filter=ACMR",
        ])
        .args(rev)
        .output()?;
    eprint!("{}", o.stderr);
    if !o.status.success() {
        bail!("git diff --cached failed ({})", o.status);
    }
    let mut out = Vec::new();
    let mut path: Option<String> = None;
    let mut is_text = false;
    let mut lineno = 0usize;
    for line in byte_lines(&o.stdout) {
        // `@@ -a,b +c,d @@`: the next added line is line c of the new file.
        if let Some(h) = line.strip_prefix("@@ ") {
            lineno = h
                .split_whitespace()
                .find_map(|w| w.strip_prefix('+'))
                .and_then(|w| w.split(',').next())
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            continue;
        }
        if let Some(p) = line.strip_prefix("+++ b/") {
            let p = py_path(p);
            is_text = true;
            path = Some(p);
            continue;
        }
        if line.starts_with("Binary files ") {
            is_text = false;
        }
        if !is_text || !line.starts_with('+') || line.starts_with("+++") {
            continue;
        }
        let p = path.clone().unwrap_or_default();
        out.push((p, lineno, line[1..].to_owned()));
        lineno += 1;
    }
    Ok(out)
}

/// The second parent's SHA mid-merge.
fn merge_head() -> Option<String> {
    let o = proc::Cmd::new("git")
        .args(["rev-parse", "--verify", "--quiet", "MERGE_HEAD"])
        .output()
        .ok()?;
    let sha = o.stdout.trim().to_owned();
    (!sha.is_empty()).then_some(sha)
}

/// Added source lines from the staged diff.
///
/// Mid-merge, `git diff --cached` is taken against HEAD alone, so every line
/// the other branch contributes reads as newly added. Keep only lines added
/// relative to *both* parents: what the merge itself introduces.
fn staged_added_lines() -> Result<Vec<(String, usize, String)>> {
    let ours = added_lines_against(None)?;
    let Some(other) = merge_head() else {
        return Ok(ours);
    };
    // Line numbers differ between the two diffs; match on (path, text).
    let theirs: HashSet<_> = added_lines_against(Some(&other))?
        .into_iter()
        .map(|(p, _, t)| (p, t))
        .collect();
    Ok(ours
        .into_iter()
        .filter(|(p, _, t)| theirs.contains(&(p.clone(), t.clone())))
        .collect())
}

/// Files under `paths` worth inspecting, in the order Python's
/// `Path.rglob("*")` yields them: each directory's entries in readdir order,
/// then its subdirectories (symlinks not followed).
/// Whether this repository opted out: `git config valet.ascii false` (local
/// config, so the check stays on in every other repository).
fn opted_out() -> bool {
    proc::Cmd::new("git")
        .args(["config", "--type=bool", "--get", "valet.ascii"])
        .output()
        .is_ok_and(|o| o.stdout.trim() == "false")
}

pub fn staged() -> Result<i32> {
    if opted_out() {
        return Ok(0);
    }
    let mut errors = 0;
    for (path, lineno, line) in staged_added_lines()? {
        for (col, what) in findings(&path, &line) {
            eprintln!("{path}:{lineno}:{col}: {what}");
            errors += 1;
        }
    }
    if errors > 0 {
        eprintln!(
            "pre-commit: {errors} invisible or lookalike character(s) added; replace them with ASCII (off in this repository: `git config valet.ascii false`; everywhere: `valet hooks disable git.ascii`)"
        );
    }
    Ok(i32::from(errors > 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols(path: &str, line: &str) -> Vec<usize> {
        findings(path, line).into_iter().map(|f| f.0).collect()
    }

    #[test]
    fn invisible_is_refused_everywhere() {
        assert_eq!(
            findings("a.md", "x\u{A0}y"),
            vec![(2, "U+00A0 NO-BREAK SPACE".to_owned())]
        );
        // Trojan Source: a bidi override inside a comment.
        assert_eq!(cols("a.rs", "let x = 1; // \u{202E}evil"), vec![15]);
        assert_eq!(cols("p.astro", "<p>zero\u{200B}width</p>"), vec![8]);
        assert_eq!(cols("a.rs", "\u{FEFF}fn main() {}"), Vec::<usize>::new());
        assert_eq!(cols("a.rs", "x\u{FEFF}"), vec![2]);
    }

    #[test]
    fn lookalikes_only_in_code_outside_comments_and_strings() {
        assert_eq!(cols("run.sh", "kubectl \u{2013}n kube-system"), vec![9]);
        assert_eq!(cols("a.py", "x = \u{201C}a\u{201D}"), vec![5, 7]);
        assert_eq!(cols("a.rs", "let y = a \u{2212} b;"), vec![11]);
        assert_eq!(cols("tool", "echo \u{FF02}x"), vec![6]);
        // Backticks are code in shell (command substitution), not a string.
        assert_eq!(cols("run.sh", "x=`kubectl \u{2013}n a`"), vec![12]);
        assert_eq!(cols("run.sh", "sh -c 'kubectl \u{2013}n a'"), vec![16]);
        for (p, l) in [
            ("a.rs", "// it\u{2019}s fine \u{2013} really"),
            ("a.rs", "let s = \"it\u{2019}s \u{2013} ok\";"),
            ("a.rs", "x(); /* \u{201C}quoted\u{201D} */"),
            ("a.rs", "   * doc \u{2018}line\u{2019}"),
            ("a.py", "x = 1  # \u{2018}note\u{2019}"),
            ("a.yaml", "key: v # \u{2013} why"),
            ("a.sql", "select 1 -- \u{2018}x\u{2019}"),
            ("README.md", "it\u{2019}s \u{201C}fine\u{201D}"),
            ("page.html", "<p>it\u{2019}s</p>"),
            (
                "a.rs",
                "// \u{2014} \u{2026} \u{2192} \u{2713} caf\u{E9} \u{2502}",
            ),
            ("a.rs", "let s = \"\u{1F680} done \u{2192}\";"),
            ("app.js", "t.textContent = `${a}\u{2013}${b} of ${n}`;"),
            ("a.js", "return c === 'Deleted' ? '\u{2212}' : '\u{B1}';"),
            ("a.py", "x = 'it\u{2019}s'  # ok"),
            ("a.ts", "const s = `it\u{2019}s \"fine\"`;"),
        ] {
            assert_eq!(cols(p, l), Vec::<usize>::new(), "{p}: {l}");
        }
    }

    #[test]
    fn comment_detection() {
        assert_eq!(comment_start("a # b", &["#"], &['"']), Some(2));
        assert_eq!(comment_start("echo ${#x}", &["#"], &['"']), None);
        assert_eq!(
            comment_start("u = \"http://x\" // c", &["//", "/*"], &['"']),
            Some(15)
        );
        assert_eq!(comment_start("s = \" # \"", &["#"], &['"']), None);
    }

    #[test]
    fn code_detection() {
        let code = |p| {
            let (k, b) = code_name(p);
            k || b
        };
        assert!(code("a/b.py") && code("x/Makefile") && code("bin/tool"));
        assert!(code("A.YML") && code("t.sh.j2"));
        assert!(!code("README.md") && !code("a.txt") && !code("p.astro"));
        assert!(code(".bashrc")); // pathlib: no suffix
    }

    #[test]
    fn python_paths_and_lines() {
        assert_eq!(py_path("./a//b/"), "a/b");
        assert_eq!(py_path("."), ".");
        assert_eq!(py_path("//x"), "//x");
        assert_eq!(py_path("///x"), "/x");
        assert_eq!(byte_lines("a\r\nb\rc\n"), vec!["a", "b", "c"]);
        assert_eq!(byte_lines("a\u{2028}b"), vec!["a\u{2028}b"]);
    }
}
