//! `claude.session-name`: name Claude Code sessions and their herdr tabs.
//!
//! UserPromptSubmit and SessionStart hook. The session title is kebab-case
//! (`valet-cutover`, at most [`TITLE_MAX`] chars). Until the user names the
//! session, valet sets it from Claude's latest `ai-title` transcript record
//! through the hook's `sessionTitle` output. A title valet did not set
//! (`/rename`, `--name`) is the user's: auto-naming stops for that session,
//! and the title is only normalized to kebab-case. Claude records hook and
//! user titles alike as `custom-title`, so per-session state under valet's
//! State dir (`session-names/<session_id>.json`) remembers which titles
//! valet set.
//!
//! Claude generates no `ai-title` when nonessential traffic is off (the
//! org's managed settings turn it off), so an unnamed session is named on
//! its first prompt from the prompt's key words ([`slug`]); an `ai-title`,
//! when Claude writes one, still wins.
//!
//! herdr tab labels follow session titles through the `tabs` part of
//! `valet daemon` ([`crate::cmd::cc::herdr::tabs_part`]), not here: Claude
//! runs hooks in background worker processes whose herdr environment can
//! name another tab.
//!
//! Never blocks the prompt: any error means no output and exit 0.

use super::skill_source::{emit, read_stdin};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const TITLE_MAX: usize = 40;
/// Titles valet set, kept per session to tell them from user renames.
const KEEP: usize = 8;
/// Transcript bytes scanned from the end for title records.
const SCAN_MAX: u64 = 64 << 20;
const CHUNK: u64 = 1 << 20;
/// Title records are short; a longer partial line is dropped while scanning.
const LINE_MAX: usize = 64 << 10;

#[derive(Default, Serialize, Deserialize, PartialEq, Debug)]
struct State {
    /// Titles valet set or adopted, newest last.
    #[serde(default)]
    set: Vec<String>,
    /// The user named the session: no more auto-naming.
    #[serde(default)]
    user: bool,
}

impl State {
    fn remember(&mut self, t: &str) {
        self.set.retain(|s| s != t);
        self.set.push(t.to_owned());
        let n = self.set.len().saturating_sub(KEEP);
        self.set.drain(..n);
    }
}

/// What the hook sees on one run.
struct Seen<'a> {
    /// Claude applies `sessionTitle` from this run.
    can_title: bool,
    /// The session's current title (custom-title).
    current: Option<&'a str>,
    /// Claude's latest generated title.
    ai: Option<&'a str>,
    /// The prompt being submitted (UserPromptSubmit).
    prompt: Option<&'a str>,
}

pub fn run() {
    let Some(input) = read_stdin() else { return };
    let Ok(dir) = crate::cmd::apps::self_dir("State") else {
        return;
    };
    if let Some(out) = handle(&input, &dir.join("session-names")) {
        emit(&out);
    }
}

/// One hook run with state in `dir`. Returns the hook output, if any.
fn handle(input: &Value, dir: &Path) -> Option<Value> {
    let event = input["hook_event_name"].as_str()?;
    let can_title = match event {
        "UserPromptSubmit" => true,
        // Claude ignores a SessionStart sessionTitle after clear and compact.
        "SessionStart" => matches!(
            input["source"].as_str(),
            Some("startup" | "resume" | "fork") | None
        ),
        _ => return None,
    };
    let sid = input["session_id"].as_str().filter(|s| safe_id(s))?;
    let file = dir.join(format!("{sid}.json"));
    let mut state: State = fs::read_to_string(&file)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();

    // Claude passes the current title; older versions do not.
    let given = input.get("session_title").map(|v| v.as_str());
    let mut current = given.flatten().map(str::to_owned);
    let need_custom = given.is_none();
    let need_ai =
        can_title && !state.user && current.as_ref().is_none_or(|c| state.set.contains(c));
    let mut ai = None;
    if (need_custom || need_ai)
        && let Some(path) = input["transcript_path"].as_str()
        && let Ok(mut f) = fs::File::open(path)
    {
        let (a, c) = scan(&mut f, need_ai, need_custom);
        ai = a;
        if need_custom {
            current = c;
        }
    }

    let before = format!("{state:?}");
    let seen = Seen {
        can_title,
        current: current.as_deref(),
        ai: ai.as_deref(),
        prompt: input["prompt"].as_str(),
    };
    let title = step(&seen, &mut state);
    if format!("{state:?}") != before {
        save(&file, &state);
    }
    title.map(|t| json!({"hookSpecificOutput": {"hookEventName": event, "sessionTitle": t}}))
}

/// Decide the title to set (returned); updates `state`.
fn step(seen: &Seen, state: &mut State) -> Option<String> {
    if !seen.can_title {
        return None;
    }
    let cur = seen.current.filter(|c| !c.trim().is_empty());
    match cur {
        Some(c) if !state.set.iter().any(|s| s == c) => {
            // Named by the user: keep it, in kebab-case.
            state.user = true;
            let k = kebab(c);
            if !k.is_empty() && k != c {
                state.remember(&k);
                return Some(k);
            }
            state.remember(c);
            None
        }
        _ if state.user => None,
        _ => {
            // Claude's own title, else (unnamed only) the first prompt's words.
            let k = match seen.ai.map(kebab).filter(|k| !k.is_empty()) {
                Some(k) => k,
                None if cur.is_none() => slug(seen.prompt?)?,
                None => return None,
            };
            if Some(k.as_str()) == cur {
                return None;
            }
            state.remember(&k);
            Some(k)
        }
    }
}

/// Words that say little about a prompt's topic.
const STOP: &[&str] = &[
    // Request verbs: they say nothing about the topic.
    "describe", "explain", "give", "show", "tell", "answer", "brief", "line", "quick", "sentence",
    "short", "a", "about", "after", "again", "all", "also", "am", "an", "and", "any", "are", "as",
    "at", "be", "been", "but", "by", "can", "could", "did", "do", "does", "for", "from", "get",
    "got", "had", "has", "have", "hello", "help", "hey", "hi", "how", "i", "if", "im", "in",
    "into", "is", "it", "its", "just", "let", "lets", "like", "me", "my", "need", "no", "not",
    "now", "of", "ok", "okay", "on", "one", "or", "our", "out", "please", "pls", "should", "so",
    "some", "that", "the", "then", "there", "these", "this", "those", "to", "u", "up", "us",
    "want", "was", "we", "were", "what", "when", "where", "which", "who", "why", "will", "with",
    "would", "yes", "you", "your",
];

/// A kebab-case session title from a prompt: its first four topic words
/// (URLs, paths, inline code and stop words dropped). None when nothing is left.
fn slug(prompt: &str) -> Option<String> {
    let head: String = prompt.chars().take(400).collect();
    let mut words = Vec::new();
    let mut in_code = false;
    for tok in head.split_whitespace() {
        let ticks = tok.matches('`').count();
        let was_code = in_code;
        if ticks % 2 == 1 {
            in_code = !in_code;
        }
        if was_code || ticks > 0 || tok.contains("://") || tok.contains('/') || tok.starts_with('-')
        {
            continue;
        }
        for w in tok.split(|c: char| !c.is_ascii_alphanumeric()) {
            let w = w.to_ascii_lowercase();
            if w.len() >= 2 && !STOP.contains(&w.as_str()) && !w.bytes().all(|b| b.is_ascii_digit())
            {
                words.push(w);
            }
        }
        if words.len() >= 4 {
            break;
        }
    }
    words.truncate(4);
    let k = kebab(&words.join(" "));
    (!k.is_empty()).then_some(k)
}

/// `[A-Za-z0-9_-]`, 1 to 128 chars.
fn safe_id(s: &str) -> bool {
    (1..=128).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn save(file: &Path, state: &State) {
    let Some(dir) = file.parent() else { return };
    if fs::create_dir_all(dir).is_err() {
        return;
    }
    let Ok(text) = serde_json::to_string(state) else {
        return;
    };
    let tmp = file.with_extension(format!("tmp{}", std::process::id()));
    if fs::write(&tmp, text).is_err() || fs::rename(&tmp, file).is_err() {
        let _ = fs::remove_file(&tmp);
    }
}

/// Lowercase ASCII words joined by `-`, at most [`TITLE_MAX`] chars, cut at a
/// word boundary (a longer first word is cut). Apostrophes join words.
pub(crate) fn kebab(s: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    let mut w = String::new();
    for c in s.chars() {
        match c {
            '\'' | '\u{2019}' => {}
            c if c.is_ascii_alphanumeric() => w.push(c.to_ascii_lowercase()),
            _ if !w.is_empty() => words.push(std::mem::take(&mut w)),
            _ => {}
        }
    }
    if !w.is_empty() {
        words.push(w);
    }
    let mut out = String::new();
    for w in words {
        if out.is_empty() {
            out = w.chars().take(TITLE_MAX).collect();
        } else if out.len() + 1 + w.len() <= TITLE_MAX {
            out.push('-');
            out.push_str(&w);
        } else {
            break;
        }
    }
    out
}

/// herdr tab labels: at most this many chars.
pub(crate) const LABEL_MAX: usize = 15;

/// `valet-cutover` -> `ValetCutover`, at most [`LABEL_MAX`] chars: whole
/// trailing words dropped to fit (a longer first word is cut).
pub(crate) fn pascal(kebab: &str) -> String {
    let mut out = String::new();
    for w in kebab.split('-').filter(|w| !w.is_empty()) {
        let mut cs = w.chars();
        let first = cs.next().map(|c| c.to_ascii_uppercase());
        let word: String = first.into_iter().chain(cs).collect();
        if out.is_empty() {
            out = word.chars().take(LABEL_MAX).collect();
        } else if out.len() + word.len() <= LABEL_MAX {
            out.push_str(&word);
        } else {
            break;
        }
    }
    out
}

/// Latest `ai-title` and `custom-title` from a JSONL transcript, read
/// backwards from the end (at most [`SCAN_MAX`] bytes) until the wanted ones
/// are found. Only lines naming the record type are parsed.
fn scan<R: Read + Seek>(
    f: &mut R,
    want_ai: bool,
    want_custom: bool,
) -> (Option<String>, Option<String>) {
    let (mut ai, mut custom) = (None, None);
    let Ok(len) = f.seek(SeekFrom::End(0)) else {
        return (ai, custom);
    };
    let floor = len.saturating_sub(SCAN_MAX);
    let mut pos = len;
    let mut carry: Vec<u8> = Vec::new();
    let done = |ai: &Option<String>, c: &Option<String>| {
        (!want_ai || ai.is_some()) && (!want_custom || c.is_some())
    };
    while pos > floor && !done(&ai, &custom) {
        let n = CHUNK.min(pos - floor);
        pos -= n;
        let mut buf = vec![0; n as usize];
        if f.seek(SeekFrom::Start(pos)).is_err() || f.read_exact(&mut buf).is_err() {
            break;
        }
        buf.extend_from_slice(&carry);
        // Bytes before the first newline belong to a line that starts earlier.
        let (head, lines) = match buf.iter().position(|&b| b == b'\n') {
            Some(i) if pos > 0 => (buf[..i].to_vec(), &buf[i + 1..]),
            _ if pos > 0 => (buf.clone(), &buf[..0]),
            _ => (Vec::new(), &buf[..]),
        };
        for line in lines.rsplit(|&b| b == b'\n') {
            if want_ai && ai.is_none() {
                ai = record(line, "ai-title", "aiTitle");
            }
            if want_custom && custom.is_none() {
                custom = record(line, "custom-title", "customTitle");
            }
            if done(&ai, &custom) {
                break;
            }
        }
        carry = if head.len() > LINE_MAX {
            Vec::new()
        } else {
            head
        };
    }
    (ai, custom)
}

/// `line`'s `key` when it is a `{"type": kind}` record with a non-empty value.
fn record(line: &[u8], kind: &str, key: &str) -> Option<String> {
    let marker = format!("\"type\":\"{kind}\"");
    if line.len() > LINE_MAX || !line.windows(marker.len()).any(|w| w == marker.as_bytes()) {
        return None;
    }
    let v: Value = serde_json::from_slice(line).ok()?;
    if v["type"].as_str() != Some(kind) {
        return None;
    }
    v[key]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const NON_LATIN: &str = "\u{65e5}\u{672c}\u{8a9e}";

    #[test]
    fn kebab_case() {
        assert_eq!(kebab("Valet cutover"), "valet-cutover");
        assert_eq!(
            kebab("  Fix PR #123: Widget GPU taint!  "),
            "fix-pr-123-widget-gpu-taint"
        );
        assert_eq!(
            kebab("Don't break server\u{2019}s CI"),
            "dont-break-servers-ci"
        );
        assert_eq!(kebab("valet-cutover"), "valet-cutover");
        assert_eq!(kebab("\u{dc}n\u{ef}code only \u{2603}"), "n-code-only");
        assert_eq!(kebab(NON_LATIN), "");
        // At most 40 chars, cut at a word boundary.
        let k = kebab("Widget server Helm conversion PR three with Argo follow up");
        assert_eq!(k, "widget-server-helm-conversion-pr-three");
        assert!(k.len() <= TITLE_MAX);
        assert_eq!(kebab(&"a".repeat(50)), "a".repeat(40));
        assert_eq!(
            kebab(&kebab("Some Title, again")),
            kebab("Some Title, again")
        );
    }

    #[test]
    fn pascal_case_fits_15() {
        assert_eq!(pascal("valet-cutover"), "ValetCutover");
        assert_eq!(pascal("gpu-site-learn"), "GpuSiteLearn");
        // Whole trailing words dropped.
        assert_eq!(pascal("widget-server-helm"), "WidgetServer");
        assert_eq!(pascal("fix-pr-1234-gadgets"), "FixPr1234");
        // Exactly 15 fits.
        assert_eq!(pascal("abcdefg-hijklmno"), "AbcdefgHijklmno");
        // A single long word is cut.
        assert_eq!(pascal("supercalifragilistic-x"), "Supercalifragil");
        assert_eq!(pascal(""), "");
    }

    fn seen<'a>(
        current: Option<&'a str>,
        ai: Option<&'a str>,
        prompt: Option<&'a str>,
    ) -> Seen<'a> {
        Seen {
            can_title: true,
            current,
            ai,
            prompt,
        }
    }

    #[test]
    fn names_from_the_first_prompt() {
        let mut s = State::default();
        // No ai-title: the first prompt's topic words.
        let out = step(
            &seen(
                None,
                None,
                Some("How do I list files sorted by size in bash? One line answer."),
            ),
            &mut s,
        );
        assert_eq!(out.as_deref(), Some("list-files-sorted-size"));
        assert!(!s.user);
        // Applied: later prompts leave it alone.
        let out = step(
            &seen(Some("list-files-sorted-size"), None, Some("now in zsh")),
            &mut s,
        );
        assert_eq!(out, None);
        // An ai-title, when Claude writes one, still wins.
        let out = step(
            &seen(
                Some("list-files-sorted-size"),
                Some("Sort files by size"),
                None,
            ),
            &mut s,
        );
        assert_eq!(out.as_deref(), Some("sort-files-by-size"));
        // Nothing to name from: nothing.
        let mut s = State::default();
        assert_eq!(
            step(&seen(None, None, Some("hi, please help")), &mut s),
            None
        );
        assert_eq!(step(&seen(None, None, None), &mut s), None);
    }

    #[test]
    fn slugs_skip_noise() {
        assert_eq!(
            slug("fix the herdr tab naming bug").as_deref(),
            Some("fix-herdr-tab-naming")
        );
        assert_eq!(
            slug("Explain how kubernetes pod disruption budgets work").as_deref(),
            Some("kubernetes-pod-disruption-budgets")
        );
        assert_eq!(
            slug("check https://x.y/z and `valet repos` in /srv/a/b for node42 GPU nodes")
                .as_deref(),
            Some("check-node42-gpu-nodes")
        );
        assert_eq!(
            slug("```\ncode here\n``` summarize it").as_deref(),
            Some("summarize")
        );
        assert_eq!(slug("12 34 ?!").as_deref(), None);
        // Non-ASCII letters split words; one-letter pieces drop.
        assert_eq!(
            slug("ünïcode words only").as_deref(),
            Some("code-words-only")
        );
    }

    #[test]
    fn user_rename_wins() {
        let mut s = State::default();
        step(&seen(None, Some("Valet Cutover"), None), &mut s);
        // The user ran `/rename My Big Plan`: normalize it, stop auto-naming.
        let out = step(
            &seen(Some("My Big Plan"), Some("Valet Cutover"), None),
            &mut s,
        );
        assert_eq!(out.as_deref(), Some("my-big-plan"));
        assert!(s.user);
        // A new ai-title or prompt no longer applies.
        assert_eq!(
            step(
                &seen(Some("my-big-plan"), Some("Something else"), Some("x y")),
                &mut s
            ),
            None
        );
        // Later renames are followed.
        let out = step(&seen(Some("GPU Site v2"), None, None), &mut s);
        assert_eq!(out.as_deref(), Some("gpu-site-v2"));
        // A title that cannot be kebab-cased stays, and is not touched again.
        assert_eq!(step(&seen(Some(NON_LATIN), None, None), &mut s), None);
        assert_eq!(s.set.last().map(String::as_str), Some(NON_LATIN));
    }

    #[test]
    fn existing_title_is_the_users() {
        // A session valet never saw, already named: treated as a user title.
        let mut s = State::default();
        assert_eq!(
            step(
                &seen(Some("gpu-site-learn"), Some("GPU things"), None),
                &mut s
            ),
            None
        );
        assert!(s.user);
        assert_eq!(s.set, vec!["gpu-site-learn".to_owned()]);
    }

    #[test]
    fn remembers_only_recent_titles() {
        let mut s = State::default();
        for i in 0..20 {
            s.remember(&format!("t{i}"));
        }
        assert_eq!(s.set.len(), KEEP);
        assert_eq!(s.set.last().map(String::as_str), Some("t19"));
    }

    #[test]
    fn no_title_where_claude_ignores_it() {
        // After compact Claude ignores sessionTitle: state left alone.
        let mut s = State::default();
        let compact = Seen {
            can_title: false,
            current: Some("Big Plan"),
            ai: Some("Other"),
            prompt: None,
        };
        assert_eq!(step(&compact, &mut s), None);
        assert!(!s.user && s.set.is_empty());
    }

    fn transcript(lines: &[&str]) -> Cursor<Vec<u8>> {
        Cursor::new(lines.join("\n").into_bytes())
    }

    #[test]
    fn scans_latest_titles() {
        let big = format!(
            r#"{{"type":"user","message":{{"content":"{}"}}}}"#,
            "x".repeat(3 << 20)
        );
        let mut t = transcript(&[
            r#"{"type":"ai-title","aiTitle":"First","sessionId":"s"}"#,
            r#"{"type":"custom-title","customTitle":"mine","sessionId":"s"}"#,
            &big,
            // Escaped inside a tool result: not a record.
            r#"{"type":"user","message":{"content":"{\"type\":\"ai-title\",\"aiTitle\":\"Fake\"}"}}"#,
            r#"{"type":"ai-title","aiTitle":"Second","sessionId":"s"}"#,
            r#"{"type":"assistant","message":{}}"#,
        ]);
        assert_eq!(
            scan(&mut t, true, true),
            (Some("Second".into()), Some("mine".into()))
        );
        assert_eq!(scan(&mut t, true, false), (Some("Second".into()), None));
        let mut t = transcript(&[
            r#"{"type":"user"}"#,
            r#"{"type":"custom-title","customTitle":"only"}"#,
        ]);
        assert_eq!(scan(&mut t, true, true), (None, Some("only".into())));
        // First line, no trailing newline, empty file.
        let mut t = transcript(&[r#"{"type":"ai-title","aiTitle":"Top"}"#]);
        assert_eq!(scan(&mut t, true, false).0.as_deref(), Some("Top"));
        assert_eq!(scan(&mut transcript(&[]), true, true), (None, None));
    }

    #[test]
    fn scan_crosses_chunk_boundaries() {
        // A title record split across the 1 MiB chunk boundary.
        let rec = r#"{"type":"ai-title","aiTitle":"Split","sessionId":"s"}"#;
        let pad = "y".repeat(CHUNK as usize - 20);
        let mut t = transcript(&[rec, &pad]);
        assert_eq!(scan(&mut t, true, false).0.as_deref(), Some("Split"));
        let filler = format!(
            r#"{{"type":"user","c":"{}"}}"#,
            "z".repeat(CHUNK as usize - 40)
        );
        let mut t = transcript(&[&filler, rec, &pad]);
        assert_eq!(scan(&mut t, true, false).0.as_deref(), Some("Split"));
    }

    #[test]
    fn hook_runs_end_to_end() {
        // Scratch dir in target/ (no /tmp).
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("test-session-name-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let tr = dir.join("t.jsonl");
        fs::write(&tr, "{\"type\":\"user\"}\n").unwrap();
        let state = dir.join("state");
        let base = |event: &str| json!({"session_id": "s-1", "transcript_path": tr, "hook_event_name": event});

        // First prompt, no title: named from the prompt.
        let mut i = base("UserPromptSubmit");
        i["prompt"] = json!("Fix the herdr tab naming bug");
        assert_eq!(
            handle(&i, &state),
            Some(
                json!({"hookSpecificOutput": {"hookEventName": "UserPromptSubmit", "sessionTitle": "fix-herdr-tab-naming"}})
            )
        );
        assert!(state.join("s-1.json").exists());

        // Resume after `/rename Big Plan`: SessionStart sets the kebab title.
        let mut i = base("SessionStart");
        i["source"] = json!("resume");
        i["session_title"] = json!("Big Plan");
        assert_eq!(
            handle(&i, &state),
            Some(
                json!({"hookSpecificOutput": {"hookEventName": "SessionStart", "sessionTitle": "big-plan"}})
            )
        );

        // Title from the transcript when the input has none (older Claude).
        fs::write(
            &tr,
            "{\"type\":\"custom-title\",\"customTitle\":\"big-plan\"}\n",
        )
        .unwrap();
        assert_eq!(handle(&base("UserPromptSubmit"), &state), None);

        // Other events and bad session ids: nothing.
        assert_eq!(handle(&base("Stop"), &state), None);
        let mut i = base("UserPromptSubmit");
        i["session_id"] = json!("../x");
        assert_eq!(handle(&i, &state), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ids_are_checked() {
        assert!(safe_id("2af2e099-b6c7-44f5-b205-d78955b316dd"));
        assert!(!safe_id("../x"));
        assert!(!safe_id("w1:t1"));
        assert!(!safe_id(""));
    }
}
