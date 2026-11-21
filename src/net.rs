//! HTTP through the `curl` binary (no HTTP crate).
//!
//! URL, headers and body go to curl as a config file on stdin (`--config -`),
//! so tokens never appear in argv, `ps`, or shell history.

use crate::proc::{self, Cmd};
use anyhow::{Context, Result, bail};
use std::time::Duration;

pub struct Request {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Option<String>,
    timeout: Duration,
}

pub struct Response {
    pub status: u16,
    pub body: String,
}

impl Response {
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_str(&self.body).context("response is not the expected JSON")
    }
}

pub fn post(url: &str) -> Request {
    Request::new("POST", url)
}

impl Request {
    pub fn new(method: &str, url: &str) -> Self {
        Request {
            method: method.to_owned(),
            url: url.to_owned(),
            headers: Vec::new(),
            body: None,
            timeout: proc::NET_TIMEOUT,
        }
    }

    pub fn header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.to_owned(), v.to_owned()));
        self
    }

    pub fn bearer(self, token: &str) -> Self {
        self.header("Authorization", &format!("Bearer {token}"))
    }

    pub fn body(mut self, b: impl Into<String>) -> Self {
        self.body = Some(b.into());
        self
    }

    pub fn timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    fn config(&self) -> String {
        let mut c = String::new();
        let mut kv = |k: &str, v: &str| c.push_str(&format!("{k} = \"{}\"\n", quote(v)));
        kv("url", &self.url);
        kv("request", &self.method);
        for (k, v) in &self.headers {
            kv("header", &format!("{k}: {v}"));
        }
        if let Some(b) = &self.body {
            kv("data-binary", b);
        }
        c
    }

    pub fn send(&self) -> Result<Response> {
        let secs = self.timeout.as_secs().max(1).to_string();
        let args = [
            "--config",
            "-",
            "--silent",
            "--show-error",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--max-time",
            &secs,
            "--write-out",
            "\n%{http_code}",
        ];
        let out = Cmd::new("curl")
            .args(args)
            .stdin(self.config())
            .timeout(self.timeout + Duration::from_secs(5))
            .output()?;
        if !out.status.success() {
            // stderr from curl names the URL host but not headers/body.
            bail!(
                "{} {} failed: {}",
                self.method,
                redact_query(&self.url),
                out.stderr.trim()
            );
        }
        let (body, code) = out.stdout.rsplit_once('\n').unwrap_or(("", &out.stdout));
        let status = code.trim().parse().context("curl gave no HTTP status")?;
        Ok(Response {
            status,
            body: body.to_owned(),
        })
    }
}

/// Escape for a double-quoted curl config value.
fn quote(v: &str) -> String {
    let mut s = String::with_capacity(v.len());
    for ch in v.chars() {
        match ch {
            '\\' => s.push_str("\\\\"),
            '"' => s.push_str("\\\""),
            '\n' => s.push_str("\\n"),
            '\r' => s.push_str("\\r"),
            '\t' => s.push_str("\\t"),
            c => s.push(c),
        }
    }
    s
}

/// Drop the query string from a URL for error messages.
fn redact_query(url: &str) -> &str {
    url.split('?').next().unwrap_or(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_escapes_and_includes_headers() {
        let r = post("https://x.test/a")
            .bearer("t\"ok")
            .body("line1\nline2");
        let c = r.config();
        assert!(c.contains("url = \"https://x.test/a\"\n"));
        assert!(c.contains("request = \"POST\"\n"));
        assert!(c.contains("header = \"Authorization: Bearer t\\\"ok\"\n"));
        assert!(c.contains("data-binary = \"line1\\nline2\"\n"));
    }

    #[test]
    fn redacts_query() {
        assert_eq!(redact_query("https://h/p?token=abc"), "https://h/p");
    }

    #[test]
    fn plain_http_refused_by_default() {
        let err = post("http://127.0.0.1:9/")
            .timeout(Duration::from_secs(2))
            .send();
        assert!(err.is_err());
    }
}
