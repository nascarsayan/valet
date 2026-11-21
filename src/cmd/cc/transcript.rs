//! Locate and incrementally tail Claude Code transcript JSONL files.

use serde_json::Value;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

/// `~/.claude/projects/*/<session>.jsonl`
pub fn find(session_id: &str) -> Option<String> {
    if session_id.is_empty() || session_id.contains('/') {
        return None;
    }
    let root = crate::cmd::claude_dir().ok()?.join("projects");
    let name = format!("{session_id}.jsonl");
    std::fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|e| e.path().join(&name))
        .find(|p| p.is_file())
        .map(|p| p.to_string_lossy().into_owned())
}

pub fn len(path: &str) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Parse complete lines appended since `offset`; advances `offset` past them.
pub fn read_new(path: &str, offset: &mut u64) -> Vec<Value> {
    let Ok(mut f) = File::open(path) else {
        return Vec::new();
    };
    let size = f.metadata().map(|m| m.len()).unwrap_or(0);
    if size < *offset {
        *offset = 0; // truncated or rewritten
    }
    if size == *offset || f.seek(SeekFrom::Start(*offset)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    let _ = f.take(8 << 20).read_to_end(&mut buf);
    parse_complete(&buf, offset)
}

fn parse_complete(buf: &[u8], offset: &mut u64) -> Vec<Value> {
    let Some(last_nl) = buf.iter().rposition(|&b| b == b'\n') else {
        return Vec::new();
    };
    *offset += last_nl as u64 + 1;
    buf[..last_nl]
        .split(|&b| b == b'\n')
        .filter_map(|l| serde_json::from_slice(l).ok())
        .collect()
}

/// Last main-thread assistant text block (context when a session is attached).
pub fn last_assistant_text(path: &str) -> Option<String> {
    let mut f = File::open(path).ok()?;
    let size = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(size.saturating_sub(2 << 20))).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let mut last = None;
    for line in buf.split(|&b| b == b'\n') {
        let Ok(v) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        if v["type"] != "assistant" || v["isSidechain"].as_bool() == Some(true) {
            continue;
        }
        for b in v["message"]["content"].as_array().into_iter().flatten() {
            if b["type"] == "text"
                && let Some(t) = b["text"].as_str().filter(|t| !t.trim().is_empty())
            {
                last = Some(t.to_owned());
            }
        }
    }
    last
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_partial_line_for_next_read() {
        let mut off = 0;
        let v = parse_complete(b"{\"a\":1}\n{\"b\":2}\n{\"c\":", &mut off);
        assert_eq!(v.len(), 2);
        assert_eq!(off, 16);
    }
}
