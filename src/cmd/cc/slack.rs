//! Slack Web API calls for `valet cc`: form-encoded POSTs through `net` (curl),
//! so the token never appears in argv.

use crate::net;
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::time::Duration;

pub struct Slack {
    token: String,
}

/// application/x-www-form-urlencoded value encoding.
fn enc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                o.push(b as char)
            }
            _ => o.push_str(&format!("%{b:02X}")),
        }
    }
    o
}

pub fn form(params: &[(&str, &str)]) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
        .collect::<Vec<_>>()
        .join("&")
}

impl Slack {
    pub fn new(token: String) -> Self {
        Slack { token }
    }

    pub fn call(&self, method: &str, params: &[(&str, &str)]) -> Result<Value> {
        let body = form(params);
        for attempt in 0..6u64 {
            let req = net::post(&format!("https://slack.com/api/{method}"))
                .bearer(&self.token)
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(body.clone())
                .timeout(Duration::from_secs(25));
            let resp = match req.send() {
                Ok(r) => r,
                Err(e) if attempt < 2 => {
                    super::warn(&format!("slack {method}: request failed, retrying: {e}"));
                    std::thread::sleep(Duration::from_secs(1));
                    continue;
                }
                Err(e) => return Err(e),
            };
            if resp.status == 429 {
                super::warn(&format!(
                    "slack {method}: HTTP 429, retry {} in {}s",
                    attempt + 1,
                    5 * (attempt + 1)
                ));
                std::thread::sleep(Duration::from_secs(5 * (attempt + 1)));
                continue;
            }
            if !(200..300).contains(&resp.status) {
                bail!("slack {method}: HTTP {}", resp.status);
            }
            let v: Value = resp.json()?;
            if v["ok"].as_bool() == Some(true) {
                return Ok(v);
            }
            let err = v["error"].as_str().unwrap_or("unknown_error");
            if err == "ratelimited" {
                super::warn(&format!(
                    "slack {method}: ratelimited, retry {} in 3s",
                    attempt + 1
                ));
                std::thread::sleep(Duration::from_secs(3));
                continue;
            }
            // invalid_blocks and friends say what is wrong here.
            let why: Vec<&str> = v["response_metadata"]["messages"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            if why.is_empty() {
                bail!("slack {method}: {err}");
            }
            bail!("slack {method}: {err}: {}", why.join("; "));
        }
        bail!("slack {method}: rate-limit retries exhausted")
    }

    pub fn post(&self, channel: &str, thread: Option<&str>, text: &str) -> Result<String> {
        let mut p = vec![
            ("channel", channel),
            ("text", text),
            ("unfurl_links", "false"),
            ("unfurl_media", "false"),
        ];
        if let Some(t) = thread {
            p.push(("thread_ts", t));
        }
        let v = self.call("chat.postMessage", &p)?;
        Ok(v["ts"].as_str().unwrap_or_default().to_owned())
    }

    /// Post MARKDOWN as Block Kit ([`super::blocks`]): as many messages as
    /// Slack's limits need, long code as collapsible file snippets. Returns
    /// the first message's ts. If Slack refuses a message's blocks (and
    /// FALLBACK), it goes as mrkdwn text; a refused snippet goes inline.
    pub fn post_md(
        &self,
        channel: &str,
        thread: Option<&str>,
        markdown: &str,
        fallback: bool,
    ) -> Result<String> {
        use super::blocks::{Msg, fallback as notify, items, messages, to_mrkdwn};
        let mut first = None;
        for msg in messages(items(markdown)) {
            let ts = match msg {
                Msg::Blocks(b) => {
                    match self.post_blocks(channel, thread, &Value::Array(b.clone()), &notify(&b)) {
                        Ok(ts) => ts,
                        Err(e) if !fallback => return Err(e),
                        Err(e) => {
                            super::warn(&format!("slack blocks refused ({e}); sending as text"));
                            self.post_text(channel, thread, &to_mrkdwn(&b))?
                        }
                    }
                }
                Msg::Snippet { lang, code } => {
                    match self.snippet(channel, thread, lang.as_deref(), &code) {
                        Ok(ts) => ts,
                        Err(e) if !fallback => return Err(e),
                        Err(e) => {
                            super::warn(&format!("slack snippet refused ({e}); sending inline"));
                            self.inline_code(channel, thread, lang.as_deref(), &code)?
                        }
                    }
                }
            };
            first.get_or_insert(ts);
        }
        Ok(first.unwrap_or_default())
    }

    /// CODE as an inline code block; mrkdwn text if Slack refuses that too.
    fn inline_code(
        &self,
        channel: &str,
        thread: Option<&str>,
        lang: Option<&str>,
        code: &str,
    ) -> Result<String> {
        let mut pre = serde_json::json!({
            "type": "rich_text_preformatted",
            "elements": [{ "type": "text", "text": code }],
        });
        if let Some(l) = lang {
            pre["language"] = serde_json::json!(l);
        }
        let b = serde_json::json!([{ "type": "rich_text", "elements": [pre] }]);
        let head: String = code
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(100)
            .collect();
        self.post_blocks(channel, thread, &b, &head).or_else(|_| {
            self.post_text(
                channel,
                thread,
                &format!("```\n{}\n```", super::render::esc(code)),
            )
        })
    }

    /// TEXT (mrkdwn) in Slack-sized pieces; the first piece's ts.
    fn post_text(&self, channel: &str, thread: Option<&str>, text: &str) -> Result<String> {
        let mut first = None;
        for c in super::render::chunk(text, 3500) {
            first.get_or_insert(self.post(channel, thread, &c)?);
        }
        Ok(first.unwrap_or_default())
    }

    /// CODE as a file snippet (collapsible) in CHANNEL / THREAD, through
    /// Slack's external upload: get an upload URL, send the bytes,
    /// complete into the channel. Returns "" (a shared file has no message
    /// ts in the reply).
    fn snippet(
        &self,
        channel: &str,
        thread: Option<&str>,
        lang: Option<&str>,
        code: &str,
    ) -> Result<String> {
        let ext = super::blocks::extension(lang);
        let name = format!("snippet.{ext}");
        let len = code.len().to_string();
        let mut p = vec![("filename", name.as_str()), ("length", len.as_str())];
        if let Some(l) = lang {
            p.push(("snippet_type", l));
        }
        let up = self.call("files.getUploadURLExternal", &p)?;
        let url = up["upload_url"]
            .as_str()
            .filter(|u| u.starts_with("https://"))
            .context("no upload_url")?;
        let id = up["file_id"].as_str().context("no file_id")?;
        let r = net::post(url)
            .header("Content-Type", "application/octet-stream")
            .body(code.to_owned())
            .timeout(Duration::from_secs(60))
            .send()?;
        if !(200..300).contains(&r.status) {
            bail!("snippet upload: HTTP {}", r.status);
        }
        let lines = code.lines().count();
        let title = format!("{} · {lines} lines", lang.unwrap_or("code"));
        let files = serde_json::json!([{ "id": id, "title": title }]).to_string();
        let mut p = vec![("files", files.as_str()), ("channel_id", channel)];
        if let Some(t) = thread {
            p.push(("thread_ts", t));
        }
        self.call("files.completeUploadExternal", &p)?;
        Ok(String::new())
    }

    /// BLOCKS as given, with a generic notification text (debugging).
    pub fn post_raw_blocks(
        &self,
        channel: &str,
        thread: Option<&str>,
        blocks: &Value,
    ) -> Result<String> {
        self.post_blocks(channel, thread, blocks, "blocks")
    }

    fn post_blocks(
        &self,
        channel: &str,
        thread: Option<&str>,
        blocks: &Value,
        text: &str,
    ) -> Result<String> {
        let blocks = blocks.to_string();
        let mut p = vec![
            ("channel", channel),
            ("text", text),
            ("blocks", blocks.as_str()),
            ("unfurl_links", "false"),
            ("unfurl_media", "false"),
        ];
        if let Some(t) = thread {
            p.push(("thread_ts", t));
        }
        let v = self.call("chat.postMessage", &p)?;
        Ok(v["ts"].as_str().unwrap_or_default().to_owned())
    }

    pub fn update(&self, channel: &str, ts: &str, text: &str) -> Result<()> {
        self.call(
            "chat.update",
            &[("channel", channel), ("ts", ts), ("text", text)],
        )?;
        Ok(())
    }

    pub fn delete(&self, channel: &str, ts: &str) -> Result<()> {
        match self.call("chat.delete", &[("channel", channel), ("ts", ts)]) {
            Err(e) if e.to_string().contains("message_not_found") => Ok(()),
            r => r.map(|_| ()),
        }
    }

    /// Best effort; `already_reacted` and friends are ignored.
    pub fn react(&self, channel: &str, ts: &str, name: &str) {
        let _ = self.call(
            "reactions.add",
            &[("channel", channel), ("timestamp", ts), ("name", name)],
        );
    }

    /// All messages of a thread (root first), optionally only those after `oldest`.
    pub fn replies(&self, channel: &str, ts: &str, oldest: Option<&str>) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        let mut cursor = String::new();
        loop {
            let mut p = vec![("channel", channel), ("ts", ts), ("limit", "200")];
            if let Some(o) = oldest {
                p.push(("oldest", o));
            }
            if !cursor.is_empty() {
                p.push(("cursor", cursor.as_str()));
            }
            let v = self.call("conversations.replies", &p)?;
            out.extend(v["messages"].as_array().into_iter().flatten().cloned());
            let next = v["response_metadata"]["next_cursor"].as_str().unwrap_or("");
            if v["has_more"].as_bool() != Some(true) || next.is_empty() {
                break;
            }
            cursor = next.to_owned();
        }
        Ok(out)
    }
}

/// Slack timestamps ("1784033200.860599") as an orderable key without f64 precision loss.
pub fn ts_key(ts: &str) -> (u64, u64) {
    let (a, b) = ts.split_once('.').unwrap_or((ts, "0"));
    (a.parse().unwrap_or(0), b.parse().unwrap_or(0))
}

pub fn ts_gt(a: &str, b: &str) -> bool {
    ts_key(a) > ts_key(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_encoding() {
        assert_eq!(
            form(&[("text", "a b&c=é"), ("x", "1.2")]),
            "text=a%20b%26c%3D%C3%A9&x=1.2"
        );
    }

    #[test]
    fn ts_order() {
        assert!(ts_gt("1784033200.860600", "1784033200.860599"));
        assert!(ts_gt("1784033201.000001", "1784033200.999999"));
        assert!(!ts_gt("0", "1.1"));
    }
}
