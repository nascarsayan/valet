//! `valet git review sync`: GitHub PR discussion <-> local markdown.
//!
//! `fetch` writes `<dir>/.local/ghub/<branch>/PR-<N>-{human,bot}-{pending,resolved}.md`
//! (a small local git repo). `push` posts the filled reply blocks in
//! `PR-<N>-human-pending.md`. Block format, per unresolved thread:
//!
//! ```text
//! <!-- <gh-login>
//! [ ] resolve ; [ ] resolve with 👍
//! reply text (or empty)
//! -->
//! ```
//!
//! text + no box: reply; `[x] resolve` + text: reply and resolve;
//! `[x] resolve with 👍`: reply 👍 and resolve; empty + no box: skip.

use crate::{proc, tools};
use anyhow::{Context, Result, bail};
use regex::Regex;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

/// `gh api --paginate` can take several requests.
const PAGINATE_TIMEOUT: Duration = Duration::from_secs(120);
/// Stop paginating review threads after this many pages (100 threads each).
const MAX_THREAD_PAGES: usize = 100;

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    cmd: SyncCmd,
}

#[derive(clap::Subcommand)]
enum SyncCmd {
    /// Pull reviews and threads into .local/ghub/<branch>/PR-<N>-*.md.
    Fetch(Target),
    /// Post filled reply blocks to GitHub; refreshes from GitHub first.
    Push(PushArgs),
}

#[derive(clap::Args, Debug)]
struct Target {
    /// GitHub repository, owner/name.
    #[arg(value_parser = parse_repo)]
    repo: String,
    /// Pull request number.
    #[arg(value_parser = clap::value_parser!(u64).range(1..))]
    pr: u64,
    /// Checkout that holds .local/ghub [default: current directory].
    #[arg(long)]
    dir: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
struct PushArgs {
    #[command(flatten)]
    target: Target,
    /// Show what would be posted (gh commands and JSON bodies); write nothing.
    #[arg(long)]
    dry_run: bool,
}

pub fn run(s: Args) -> Result<()> {
    match s.cmd {
        SyncCmd::Fetch(t) => {
            let gh = Gh(tools::resolve("gh")?);
            let plan = plan_fetch(&gh, &t.repo, t.pr, &checkout_dir(&t)?, None, true)?;
            apply_fetch(&plan)
        }
        SyncCmd::Push(p) => push(&p),
    }
}

static REPO_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$").expect("valid regex"));

fn parse_repo(s: &str) -> Result<String, String> {
    if REPO_RE.is_match(s) && !s.split('/').any(|p| p == "." || p == "..") {
        Ok(s.to_owned())
    } else {
        Err(format!("expected owner/name, got {s:?}"))
    }
}

fn checkout_dir(t: &Target) -> Result<PathBuf> {
    let d = t.dir.clone().unwrap_or_else(|| PathBuf::from("."));
    std::path::absolute(&d).with_context(|| format!("bad --dir {}", d.display()))
}

// ── Python string semantics ──────────────────────────────────────────────────

/// Python `str.isspace`.
fn py_isspace(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// Python `str.strip()`.
fn py_strip(s: &str) -> &str {
    s.trim_matches(py_isspace)
}

/// Python `str.splitlines()`.
fn py_splitlines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut it = s.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        let brk = matches!(
            c,
            '\n' | '\r'
                | '\u{0b}'
                | '\u{0c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if brk {
            out.push(&s[start..i]);
            let mut end = i + c.len_utf8();
            if c == '\r' && it.peek().is_some_and(|(_, n)| *n == '\n') {
                it.next();
                end += 1;
            }
            start = end;
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// Python `Path.read_text()` newline translation (`\r\n` and `\r` -> `\n`).
fn universal_newlines(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

fn read_text(p: &Path) -> Result<String> {
    let s = std::fs::read_to_string(p).with_context(|| format!("cannot read {}", p.display()))?;
    Ok(universal_newlines(&s))
}

/// Python truthiness of a JSON value.
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// Python `f"{d.get(key, default)}"`.
fn pystr(v: Option<&Value>, default: &str) -> String {
    match v {
        None => default.to_owned(),
        Some(Value::Null) => "None".into(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => if *b { "True" } else { "False" }.into(),
        Some(other) => other.to_string(),
    }
}

/// Python `(d.get(key) or "").strip()`.
fn body_of(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(s)) => py_strip(s).to_owned(),
        _ => String::new(),
    }
}

fn int(v: &Value, key: &str) -> Result<i64> {
    v.get(key)
        .and_then(Value::as_i64)
        .with_context(|| format!("GitHub object has no integer {key:?}"))
}

fn login(v: &Value) -> Result<&str> {
    v.get("user")
        .and_then(|u| u.get("login"))
        .and_then(Value::as_str)
        .context("GitHub object has no user.login")
}

// ── GitHub via gh ────────────────────────────────────────────────────────────

struct Gh(PathBuf);

/// A mutating `gh api` call; the JSON body goes on stdin, never argv.
#[derive(Debug, PartialEq)]
struct ApiCall {
    endpoint: String,
    body: Value,
}

impl ApiCall {
    fn reply(repo: &str, tid: &str, text: &str) -> Self {
        ApiCall {
            endpoint: format!("repos/{repo}/pulls/comments/{tid}/replies"),
            body: json!({ "body": text }),
        }
    }

    fn graphql(query: &str, variables: Value) -> Self {
        ApiCall {
            endpoint: "graphql".into(),
            body: json!({ "query": query, "variables": variables }),
        }
    }

    fn resolve(thread_node_id: &str) -> Self {
        Self::graphql(RESOLVE_MUTATION, json!({ "threadId": thread_node_id }))
    }

    fn args(&self) -> Vec<&str> {
        vec![
            "api",
            "-X",
            "POST",
            &self.endpoint,
            "-H",
            "Content-Type: application/json",
            "--input",
            "-",
        ]
    }

    fn cmd(&self, gh: &Path) -> proc::Cmd {
        proc::Cmd::new(gh)
            .args(self.args())
            .stdin(self.body.to_string())
            .timeout(proc::NET_TIMEOUT)
    }

    /// Dry-run description: the command and its stdin.
    fn describe(&self) -> String {
        format!(
            "  would run: {}\n  stdin: {}",
            proc::Cmd::new("gh").args(self.args()).display(),
            self.body
        )
    }
}

const THREADS_QUERY: &str = "query($owner: String!, $name: String!, $pr: Int!, $after: String) { repository(owner: $owner, name: $name) { pullRequest(number: $pr) { reviewThreads(first: 100, after: $after) { pageInfo { hasNextPage endCursor } nodes { id isResolved comments(first: 1) { nodes { databaseId } } } } } } }";
const RESOLVE_MUTATION: &str = "mutation($threadId: ID!) { resolveReviewThread(input: {threadId: $threadId}) { thread { isResolved } } }";

impl Gh {
    fn run(
        &self,
        args: &[&str],
        stdin: Option<String>,
        timeout: Duration,
        what: &str,
    ) -> Result<String> {
        let mut c = proc::Cmd::new(&self.0).args(args).timeout(timeout);
        if let Some(s) = stdin {
            c = c.stdin(s);
        }
        let o = c.output()?;
        if !o.status.success() {
            bail!("{what}: {}", o.stderr.trim());
        }
        Ok(o.stdout)
    }

    fn get_one(&self, endpoint: &str) -> Result<Value> {
        let out = self.run(
            &["api", endpoint],
            None,
            proc::NET_TIMEOUT,
            &format!("gh api GET {endpoint}"),
        )?;
        serde_json::from_str(&out).with_context(|| format!("gh api GET {endpoint}: bad JSON"))
    }

    fn get_list(&self, endpoint: &str) -> Result<Vec<Value>> {
        let out = self.run(
            &["api", "--paginate", "--jq", ".[]", endpoint],
            None,
            PAGINATE_TIMEOUT,
            &format!("gh api GET {endpoint}"),
        )?;
        out.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                serde_json::from_str(l).with_context(|| format!("gh api GET {endpoint}: bad JSON"))
            })
            .collect()
    }

    fn call(&self, c: &ApiCall) -> Result<Value> {
        let o = c.cmd(&self.0).output()?;
        if !o.status.success() {
            let what = if c.endpoint == "graphql" {
                "gh graphql".to_owned()
            } else {
                format!("gh api POST {}", c.endpoint)
            };
            bail!("{what}: {}", o.stderr.trim());
        }
        serde_json::from_str(&o.stdout).context("gh api: bad JSON")
    }

    /// Logged-in user, or "me" if gh cannot tell (as pr_sync.py).
    fn current_user(&self) -> String {
        self.run(
            &["api", "user", "--jq", ".login"],
            None,
            proc::NET_TIMEOUT,
            "gh api user",
        )
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|_| "me".into())
    }

    /// (resolved comment ids, comment id -> thread node id), keyed by each
    /// thread's first comment. Read-only GraphQL query, paginated.
    fn thread_info(&self, repo: &str, pr: u64) -> Result<(HashSet<i64>, HashMap<i64, String>)> {
        let (owner, name) = repo.split_once('/').context("repo is not owner/name")?;
        let mut resolved = HashSet::new();
        let mut node_ids = HashMap::new();
        let mut after = Value::Null;
        for _ in 0..MAX_THREAD_PAGES {
            let q = ApiCall::graphql(
                THREADS_QUERY,
                json!({"owner": owner, "name": name, "pr": pr, "after": after}),
            );
            let data = self.call(&q)?;
            let rt = descend(
                &data,
                &["data", "repository", "pullRequest", "reviewThreads"],
            )?;
            for t in rt
                .get("nodes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let comments = descend(t, &["comments"])?;
                for c in comments
                    .get("nodes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let db_id = int(c, "databaseId")?;
                    let tid = t
                        .get("id")
                        .and_then(Value::as_str)
                        .context("thread without id")?;
                    node_ids.insert(db_id, tid.to_owned());
                    if truthy(t.get("isResolved")) {
                        resolved.insert(db_id);
                    }
                }
            }
            let page = rt.get("pageInfo");
            if !truthy(page.and_then(|p| p.get("hasNextPage"))) {
                return Ok((resolved, node_ids));
            }
            after = page
                .and_then(|p| p.get("endCursor"))
                .cloned()
                .context("no endCursor")?;
        }
        bail!("more than {MAX_THREAD_PAGES} pages of review threads")
    }
}

/// Python's `d.get(k, {})` chain: a missing key is empty, `null` is an error.
fn descend<'a>(v: &'a Value, keys: &[&str]) -> Result<&'a Value> {
    static EMPTY: LazyLock<Value> = LazyLock::new(|| json!({}));
    let mut cur = v;
    for k in keys {
        cur = match cur.get(*k) {
            None => &EMPTY,
            Some(Value::Object(_)) => &cur[*k],
            Some(_) => bail!("unexpected GraphQL response at {k:?}"),
        };
    }
    Ok(cur)
}

// ── Reply blocks ─────────────────────────────────────────────────────────────

/// `<!-- USERNAME ... -->` blocks (posted or pending). Group 2: lines after the first.
fn reply_block_re(username: &str) -> Regex {
    Regex::new(&format!(
        r"(?s)(<!-- {}(?:[^\n]*)?\n(.*?)-->)",
        regex::escape(username)
    ))
    .expect("valid regex")
}

static META_BLOCK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<!-- meta\n(.*?)-->").expect("valid regex"));
static CHECKBOX_LINE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(\[[ x]\]) resolve\s*;\s*(\[[ x]\]) resolve with 👍").expect("valid regex")
});
static POSTED_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^<!-- \S+ posted=").expect("valid regex"));
static POSTED_INFO_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^<!-- \S+ posted=(\S+) by=(\S+) at=([^\n]+)").expect("valid regex")
});
static SECTION_END_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\n###|\n---\n|\z").expect("valid regex"));

const FRESH_CHECKBOX: &str = "[ ] resolve ; [ ] resolve with 👍";

fn parse_meta(body: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for line in py_splitlines(body) {
        if let Some((k, v)) = line.split_once(": ") {
            m.insert(py_strip(k).to_owned(), py_strip(v).to_owned());
        }
    }
    m
}

/// Fields of the last complete `<!-- meta -->` block before byte `pos`.
fn meta_before(content: &str, pos: usize) -> HashMap<String, String> {
    META_BLOCK_RE
        .captures_iter(&content[..pos])
        .last()
        .map(|c| parse_meta(&c[1]))
        .unwrap_or_default()
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Reply,
    Resolve,
    Thumbsup,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Reply => "reply",
            Kind::Resolve => "resolve",
            Kind::Thumbsup => "thumbsup",
        }
    }

    fn checkbox(self) -> &'static str {
        match self {
            Kind::Reply => FRESH_CHECKBOX,
            Kind::Resolve => "[x] resolve ; [ ] resolve with 👍",
            Kind::Thumbsup => "[ ] resolve ; [x] resolve with 👍",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Action {
    kind: Kind,
    tid: String,
    node_id: String,
    text: String,
}

/// Filled, not-yet-posted blocks, in file order.
fn extract_pending(content: &str, username: &str) -> Vec<Action> {
    let mut actions = Vec::new();
    for m in reply_block_re(username).captures_iter(content) {
        let full = m.get(1).expect("group 1");
        if POSTED_RE.is_match(full.as_str()) {
            continue;
        }
        let lines: Vec<&str> = m[2].split('\n').collect();
        let Some(cb) = CHECKBOX_LINE_RE.captures(py_strip(lines[0])) else {
            continue;
        };
        let resolve_x = &cb[1] == "[x]";
        let thumbsup_x = &cb[2] == "[x]";
        let text = py_strip(&lines[1..].join("\n")).to_owned();
        let meta = meta_before(content, full.start());
        let tid = meta.get("id").cloned().unwrap_or_default();
        let node_id = meta.get("thread").cloned().unwrap_or_default();
        if tid.is_empty() {
            continue;
        }
        let kind = if thumbsup_x {
            Kind::Thumbsup
        } else if !text.is_empty() {
            if resolve_x {
                Kind::Resolve
            } else {
                Kind::Reply
            }
        } else {
            continue;
        };
        let text = if kind == Kind::Thumbsup {
            String::new()
        } else {
            text
        };
        actions.push(Action {
            kind,
            tid,
            node_id,
            text,
        });
    }
    actions
}

/// Count `↳ **` reply lines in the thread section whose meta has `id: TID`.
fn reply_count_in_thread(content: &str, tid: &str) -> usize {
    let re = Regex::new(&format!(
        r"(?s)<!-- meta\b[^>]*?id:\s*{}\b",
        regex::escape(tid)
    ))
    .expect("valid regex");
    let Some(m) = re.find(content) else {
        return 0;
    };
    let rest = &content[m.end()..];
    let end = SECTION_END_RE.find(rest).map_or(rest.len(), |e| e.start());
    rest[..end].matches("↳ **").count()
}

/// Byte range of the reply block of thread `tid`: the first `<!-- USERNAME`
/// block after its meta and before the next thread's meta.
fn thread_block(content: &str, tid: &str, username: &str) -> Option<(usize, usize)> {
    let meta = Regex::new(&format!(
        r"(?s)<!-- meta\b[^>]*?\nid:\s*{}(?:\s|$)",
        regex::escape(tid)
    ))
    .expect("valid regex");
    let mm = meta.find(content)?;
    let section_end = content[mm.end()..]
        .find("<!-- meta")
        .map_or(content.len(), |i| mm.end() + i);
    let fm = reply_block_re(username).find_at(content, mm.end())?;
    (fm.end() <= section_end).then_some((fm.start(), fm.end()))
}

/// Fill checkboxes and text into a fresh (empty) reply block.
fn apply_action_to_block(block: &str, kind: Kind, text: &str) -> String {
    match kind {
        Kind::Thumbsup => block.replace("[ ] resolve with 👍", "[x] resolve with 👍"),
        Kind::Resolve => insert_text(&block.replace("[ ] resolve ;", "[x] resolve ;"), text),
        Kind::Reply => insert_text(block, text),
    }
}

fn insert_text(block: &str, text: &str) -> String {
    if let Some(head) = block.strip_suffix("\n\n-->") {
        format!("{head}\n{text}\n-->")
    } else if block.contains("-->") {
        let t = block.trim_end_matches(py_isspace);
        let cut = t.char_indices().rev().nth(2).map_or(0, |(i, _)| i);
        format!("{}\n{text}\n-->", &t[..cut])
    } else {
        block.to_owned()
    }
}

/// Re-inject filled blocks from the old file into freshly fetched content.
/// Returns the merged content and the thread ids that conflict.
fn inject_pending(
    new: &str,
    old: &str,
    pending: &[Action],
    username: &str,
) -> (String, Vec<String>) {
    let mut merged = new.to_owned();
    let mut conflicts: Vec<String> = Vec::new();
    for a in pending {
        if reply_count_in_thread(new, &a.tid) > reply_count_in_thread(old, &a.tid) {
            conflicts.push(a.tid.clone());
        }
        let fresh = thread_block(&merged, &a.tid, username)
            .filter(|(s, e)| !POSTED_RE.is_match(&merged[*s..*e]));
        match fresh {
            Some((s, e)) => {
                let nb = apply_action_to_block(&merged[s..e], a.kind, &a.text);
                merged.replace_range(s..e, &nb);
            }
            None => {
                if !conflicts.contains(&a.tid) {
                    conflicts.push(a.tid.clone());
                }
            }
        }
    }
    (merged, conflicts)
}

/// Replace thread `tid`'s block with the posted stamp.
fn stamp(
    content: &str,
    a: &Action,
    username: &str,
    posted_id: &str,
    by: &str,
    at: &str,
) -> Option<String> {
    let (s, e) = thread_block(content, &a.tid, username)?;
    let body = if a.text.is_empty() {
        a.kind.checkbox().to_owned()
    } else {
        format!("{}\n{}", a.kind.checkbox(), a.text)
    };
    let block = format!("<!-- {username} posted={posted_id} by={by} at={at}\n{body}\n-->");
    let mut out = content.to_owned();
    out.replace_range(s..e, &block);
    Some(out)
}

#[derive(Debug, Clone, PartialEq)]
struct Posted {
    posted_id: String,
    by: String,
    at: String,
    kind: &'static str,
    body: String,
}

/// Posted stamps in an existing human-pending file, by thread id.
fn posted_state(content: &str, username: &str) -> HashMap<String, Posted> {
    let mut out = HashMap::new();
    for m in reply_block_re(username).captures_iter(content) {
        let full = m.get(1).expect("group 1");
        if !POSTED_RE.is_match(full.as_str()) {
            continue;
        }
        let meta = meta_before(content, full.start());
        let Some(tid) = meta.get("id").filter(|t| !t.is_empty()) else {
            continue;
        };
        let lines: Vec<&str> = m[2].split('\n').collect();
        let text = py_strip(&lines[1..].join("\n")).to_owned();
        let kind = match CHECKBOX_LINE_RE.captures(py_strip(lines[0])) {
            Some(cb) if &cb[2] == "[x]" => "thumbsup",
            Some(cb) if &cb[1] == "[x]" => "resolve",
            _ => "reply",
        };
        if let Some(pm) = POSTED_INFO_RE.captures(full.as_str()) {
            out.insert(
                tid.clone(),
                Posted {
                    posted_id: pm[1].to_owned(),
                    by: pm[2].to_owned(),
                    at: py_strip(&pm[3]).to_owned(),
                    kind,
                    body: text,
                },
            );
        }
    }
    out
}

// ── Rendering ────────────────────────────────────────────────────────────────

const KEYS: [&str; 4] = [
    "human-pending",
    "human-resolved",
    "bot-pending",
    "bot-resolved",
];
const LABELS: [&str; 4] = [
    "Human Pending",
    "Human Resolved",
    "Bot Pending",
    "Bot Resolved",
];
const HUMAN_PENDING: usize = 0;
const BOT_PENDING: usize = 2;

fn is_bot(login: &str) -> bool {
    ["[bot]", "-bot", "bot]"].iter().any(|s| login.ends_with(s)) || login.contains("[bot]")
}

struct Thread<'a> {
    resolved: &'a HashSet<i64>,
    node_ids: &'a HashMap<i64, String>,
    replies: &'a HashMap<i64, Vec<&'a Value>>,
    posted: &'a HashMap<String, Posted>,
    username: &'a str,
}

impl Thread<'_> {
    fn render(&self, idx: usize, root: &Value) -> Result<Vec<String>> {
        let cid = int(root, "id")?;
        let path = pystr(root.get("path"), "");
        let line = if truthy(root.get("line")) {
            pystr(root.get("line"), "")
        } else if truthy(root.get("original_line")) {
            pystr(root.get("original_line"), "")
        } else {
            "?".into()
        };
        let is_res = self.resolved.contains(&cid);
        let badge = if is_res { "✅" } else { "🔴" };
        let filename = if path.contains('/') {
            path.rsplit('/').next().unwrap_or_default().to_owned()
        } else if path.is_empty() {
            "?".into()
        } else {
            path.clone()
        };
        let mut out = vec![
            format!("### {idx}. {badge} `{filename}`"),
            String::new(),
            "<!-- meta".into(),
            format!("filepath: {path}:L{line}"),
            format!("link: {}", pystr(root.get("html_url"), "")),
            format!("id: {cid}"),
        ];
        if let Some(n) = self.node_ids.get(&cid).filter(|n| !n.is_empty()) {
            out.push(format!("thread: {n}"));
        }
        out.extend(["-->".into(), String::new()]);

        if let Some(Value::String(hunk)) = root.get("diff_hunk").filter(|h| truthy(Some(h))) {
            let lines = py_splitlines(hunk);
            let tail = lines[lines.len().saturating_sub(12)..].join("\n");
            out.extend(["```diff".into(), tail, "```".into(), String::new()]);
        }

        out.extend([
            format!("**{}:** {}", login(root)?, body_of(root, "body")),
            String::new(),
        ]);
        for rep in self.replies.get(&cid).into_iter().flatten() {
            out.extend([
                format!(
                    "↳ **{}** (id={}): {}",
                    login(rep)?,
                    int(rep, "id")?,
                    body_of(rep, "body")
                ),
                format!("  {}", pystr(rep.get("html_url"), "")),
                String::new(),
            ]);
        }

        if !is_res {
            match self.posted.get(&cid.to_string()) {
                Some(p) => out.extend([
                    format!(
                        "<!-- {} posted={} by={} at={}",
                        self.username, p.posted_id, p.by, p.at
                    ),
                    format!("[x] {}", p.kind),
                    p.body.clone(),
                    "-->".into(),
                    String::new(),
                ]),
                None => out.extend([
                    format!("<!-- {}", self.username),
                    FRESH_CHECKBOX.into(),
                    String::new(),
                    "-->".into(),
                    String::new(),
                ]),
            }
        }
        Ok(out)
    }
}

/// GitHub data for one PR, as returned by the REST API.
struct Discussion {
    reviews: Vec<Value>,
    comments: Vec<Value>,
    issue_comments: Vec<Value>,
    resolved: HashSet<i64>,
    node_ids: HashMap<i64, String>,
}

struct Rendered {
    buckets: [Vec<String>; 4],
    kept: usize,
    skipped: usize,
    roots: usize,
    resolved: usize,
}

fn render(d: &Discussion, posted: &HashMap<String, Posted>, username: &str) -> Result<Rendered> {
    let mut replies: HashMap<i64, Vec<&Value>> = HashMap::new();
    let mut roots: Vec<&Value> = Vec::new();
    for c in &d.comments {
        match c
            .get("in_reply_to_id")
            .filter(|p| truthy(Some(p)))
            .and_then(Value::as_i64)
        {
            Some(parent) => replies.entry(parent).or_default().push(c),
            None => roots.push(c),
        }
    }
    let mut by_review: HashMap<Option<i64>, Vec<&Value>> = HashMap::new();
    for c in &roots {
        by_review
            .entry(c.get("pull_request_review_id").and_then(Value::as_i64))
            .or_default()
            .push(c);
    }

    let mut reviews: Vec<&Value> = d.reviews.iter().collect();
    let key = |r: &Value| {
        r.get("submitted_at")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    reviews.sort_by_key(|r| std::cmp::Reverse(key(r)));

    let th = Thread {
        resolved: &d.resolved,
        node_ids: &d.node_ids,
        replies: &replies,
        posted,
        username,
    };
    let mut buckets: [Vec<String>; 4] = Default::default();
    let mut counters = [0usize; 4];
    let mut skipped = 0;
    for review in &reviews {
        let rev_id = int(review, "id")?;
        let author = login(review)?;
        let body = body_of(review, "body");
        let mut threads: Vec<&Value> = by_review.get(&Some(rev_id)).cloned().unwrap_or_default();
        if body.is_empty() && threads.is_empty() {
            skipped += 1;
            continue;
        }
        let state = review.get("state").and_then(Value::as_str);
        let all_resolved = if threads.is_empty() {
            matches!(state, Some("APPROVED" | "DISMISSED"))
        } else {
            threads
                .iter()
                .all(|t| int(t, "id").is_ok_and(|id| d.resolved.contains(&id)))
        };
        let k = usize::from(is_bot(author)) * 2 + usize::from(all_resolved);
        let out = &mut buckets[k];
        out.extend([
            "---".into(),
            String::new(),
            format!(
                "## Review {rev_id} — {author} [{}] — {}",
                pystr(review.get("state"), ""),
                pystr(review.get("submitted_at"), "")
            ),
        ]);
        if !body.is_empty() {
            out.extend([String::new(), format!("> {body}")]);
        }
        out.push(String::new());
        threads.sort_by_key(|c| {
            let line = c.get("original_line").and_then(Value::as_i64).unwrap_or(0);
            (
                c.get("path")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                line,
            )
        });
        for root in threads {
            counters[k] += 1;
            out.extend(th.render(counters[k], root)?);
        }
    }

    let mut human = Vec::new();
    let mut bots = Vec::new();
    for ic in &d.issue_comments {
        if is_bot(login(ic)?) {
            bots.push(ic)
        } else {
            human.push(ic)
        }
    }
    if !human.is_empty() {
        let b = &mut buckets[HUMAN_PENDING];
        b.extend([
            "---".into(),
            String::new(),
            "## General Comments".into(),
            String::new(),
        ]);
        for ic in human {
            b.extend([
                format!(
                    "**{}** ({}) id={}:",
                    login(ic)?,
                    pystr(ic.get("created_at"), ""),
                    pystr(ic.get("id"), "")
                ),
                String::new(),
                body_of(ic, "body"),
                String::new(),
            ]);
        }
    }
    if !bots.is_empty() {
        let b = &mut buckets[BOT_PENDING];
        b.extend([
            "---".into(),
            String::new(),
            "## General Bot Comments".into(),
            String::new(),
        ]);
        for ic in bots {
            let body: String = body_of(ic, "body").chars().take(300).collect();
            b.extend([
                format!(
                    "**{}** ({}): {body}",
                    login(ic)?,
                    pystr(ic.get("created_at"), "")
                ),
                String::new(),
            ]);
        }
    }
    Ok(Rendered {
        buckets,
        kept: reviews.len() - skipped,
        skipped,
        roots: roots.len(),
        resolved: d.resolved.len(),
    })
}

/// File contents per bucket (`None`: empty bucket, file is removed).
fn file_contents(header: &[String], buckets: &[Vec<String>; 4]) -> [Option<String>; 4] {
    std::array::from_fn(|i| {
        (!buckets[i].is_empty()).then(|| {
            let mut all = header.to_vec();
            all.extend([format!("*({})*", LABELS[i]), String::new()]);
            all.extend(buckets[i].iter().cloned());
            all.join("\n")
        })
    })
}

/// Python `datetime.now(timezone.utc).isoformat()`.
fn utc_isoformat() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let base = crate::log::iso_from_unix(d.as_secs() as i64);
    let base = base.trim_end_matches('Z');
    match d.subsec_micros() {
        0 => format!("{base}+00:00"),
        us => format!("{base}.{us:06}+00:00"),
    }
}

// ── Local store ──────────────────────────────────────────────────────────────

fn git(ghub: &Path, args: &[&str]) -> Result<()> {
    proc::Cmd::new("git").arg("-C").arg(ghub).args(args).run()
}

fn ensure_ghub_repo(ghub: &Path) -> Result<()> {
    std::fs::create_dir_all(ghub).with_context(|| format!("cannot create {}", ghub.display()))?;
    if !ghub.join(".git").exists() {
        git(ghub, &["init"])?;
        std::fs::write(
            ghub.join("README.md"),
            "# gh-pr-sync local store\n\n\
             GitHub PR discussion snapshots managed by `pr_sync.py`.\n\n\
             Edit `*-human-pending.md` files to draft replies, then run `push`.\n",
        )?;
        git(ghub, &["add", "README.md"])?;
        git(ghub, &["commit", "-m", "init: gh-pr-sync local store"])?;
        println!("Initialized gh-pr-sync repo at {}", ghub.display());
    }
    Ok(())
}

/// Branch name as one safe directory name (`/` -> `-`).
fn branch_dir(branch: &str) -> Result<String> {
    let name = branch.replace('/', "-");
    let mut comps = Path::new(&name).components();
    match (comps.next(), comps.next()) {
        (Some(std::path::Component::Normal(_)), None) if !name.contains('\0') => Ok(name),
        _ => bail!("unusable branch name for a directory: {branch:?}"),
    }
}

struct Store {
    ghub: PathBuf,
    files: [PathBuf; 4],
}

impl Store {
    fn new(dir: &Path, branch: &str, pr: u64, create: bool) -> Result<Self> {
        let ghub = dir.join(".local").join("ghub");
        let folder = ghub.join(branch_dir(branch)?);
        if create {
            ensure_ghub_repo(&ghub)?;
            std::fs::create_dir_all(&folder)
                .with_context(|| format!("cannot create {}", folder.display()))?;
        }
        let files = KEYS.map(|k| folder.join(format!("PR-{pr}-{k}.md")));
        Ok(Store { ghub, files })
    }

    fn pending(&self) -> &Path {
        &self.files[HUMAN_PENDING]
    }

    fn add(&self, p: &Path) -> Result<()> {
        git(&self.ghub, &["add", &p.to_string_lossy()])
    }
}

struct FetchPlan {
    store: Store,
    pr: u64,
    branch: String,
    first_create: bool,
    contents: [Option<String>; 4],
    rendered: Rendered,
}

/// Read GitHub and render the four files. Writes nothing to GitHub; with
/// `create`, initialises the local store (as pr_sync.py) but writes no file.
fn plan_fetch(
    gh: &Gh,
    repo: &str,
    pr: u64,
    dir: &Path,
    username: Option<&str>,
    create: bool,
) -> Result<FetchPlan> {
    let info = gh.get_one(&format!("repos/{repo}/pulls/{pr}"))?;
    let branch = info["head"]["ref"]
        .as_str()
        .context("PR has no head.ref")?
        .to_owned();
    let title = pystr(Some(&info["title"]), "");
    let pr_url = pystr(Some(&info["html_url"]), "");
    let username = match username {
        Some(u) => u.to_owned(),
        None => gh.current_user(),
    };
    let store = Store::new(dir, &branch, pr, create)?;
    let first_create = !store.files.iter().any(|f| f.exists());
    let posted = if store.pending().exists() {
        posted_state(&read_text(store.pending())?, &username)
    } else {
        HashMap::new()
    };

    let reviews = gh.get_list(&format!("repos/{repo}/pulls/{pr}/reviews"))?;
    let comments = gh.get_list(&format!("repos/{repo}/pulls/{pr}/comments"))?;
    let issue_comments = gh.get_list(&format!("repos/{repo}/issues/{pr}/comments"))?;
    let (resolved, node_ids) = gh.thread_info(repo, pr).unwrap_or_else(|e| {
        eprintln!("[warn] could not fetch thread info: {e:#}");
        Default::default()
    });
    let d = Discussion {
        reviews,
        comments,
        issue_comments,
        resolved,
        node_ids,
    };
    let rendered = render(&d, &posted, &username)?;
    let header = vec![
        format!(
            "<!-- pr_sync metadata pr={pr} repo={repo} branch={branch} fetched_at={} -->",
            utc_isoformat()
        ),
        String::new(),
        format!("# PR #{pr}: {title}"),
        format!("**Branch:** `{branch}`  **URL:** {pr_url}"),
        String::new(),
    ];
    let contents = file_contents(&header, &rendered.buckets);
    Ok(FetchPlan {
        store,
        pr,
        branch,
        first_create,
        contents,
        rendered,
    })
}

fn written_names(plan: &FetchPlan) -> String {
    let names: Vec<String> = plan
        .store
        .files
        .iter()
        .zip(&plan.contents)
        .filter(|(_, c)| c.is_some())
        .map(|(p, _)| {
            p.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    if names.is_empty() {
        "(none)".into()
    } else {
        names.join(", ")
    }
}

fn print_stats(plan: &FetchPlan) {
    let r = &plan.rendered;
    println!("Written:  {}", written_names(plan));
    println!("Reviews:  {} kept, {} skipped (empty)", r.kept, r.skipped);
    println!("Threads:  {}  ({} resolved)", r.roots, r.resolved);
}

/// Write the files, then commit them in the local store.
fn apply_fetch(plan: &FetchPlan) -> Result<()> {
    for (path, content) in plan.store.files.iter().zip(&plan.contents) {
        match content {
            Some(c) => std::fs::write(path, c)
                .with_context(|| format!("cannot write {}", path.display()))?,
            None if path.exists() => std::fs::remove_file(path)?,
            None => {}
        }
    }
    for path in plan.store.files.iter().filter(|p| p.exists()) {
        plan.store.add(path)?;
    }
    let staged = proc::Cmd::new("git")
        .arg("-C")
        .arg(&plan.store.ghub)
        .args(["diff", "--cached", "--quiet"])
        .output()?;
    if !staged.status.success() {
        let verb = if plan.first_create { "init" } else { "refresh" };
        let msg = format!("pr-sync: {verb} PR-{} ({})", plan.pr, plan.branch);
        git(&plan.store.ghub, &["commit", "-m", &msg])?;
        println!("Committed: {msg}");
    }
    print_stats(plan);
    Ok(())
}

fn push(p: &PushArgs) -> Result<()> {
    let t = &p.target;
    let dir = checkout_dir(t)?;
    let gh = Gh(tools::resolve("gh")?);
    let info = gh.get_one(&format!("repos/{}/pulls/{}", t.repo, t.pr))?;
    let branch = info["head"]["ref"]
        .as_str()
        .context("PR has no head.ref")?
        .to_owned();
    let username = gh.current_user();
    let store = Store::new(&dir, &branch, t.pr, !p.dry_run)?;
    let pending = store.pending().to_owned();
    if !pending.exists() {
        bail!("Not found: {} — run fetch first.", pending.display());
    }
    let old = read_text(&pending)?;
    let actions = extract_pending(&old, &username);
    if actions.is_empty() {
        println!("No filled reply blocks found.");
        return Ok(());
    }

    println!(
        "Found {} action(s). Refreshing from GitHub...",
        actions.len()
    );
    let new = if p.dry_run {
        println!("(dry run: refresh kept in memory; nothing written, committed or posted)");
        let plan = plan_fetch(&gh, &t.repo, t.pr, &dir, Some(&username), false)?;
        print_stats(&plan);
        let c = plan.contents[HUMAN_PENDING].as_deref();
        universal_newlines(c.context("refresh would remove the human-pending file")?)
    } else {
        apply_fetch(&plan_fetch(
            &gh,
            &t.repo,
            t.pr,
            &dir,
            Some(&username),
            true,
        )?)?;
        read_text(&pending)?
    };

    let (merged, conflicts) = inject_pending(&new, &old, &actions, &username);
    if !conflicts.is_empty() {
        if !p.dry_run {
            std::fs::write(&pending, &merged)?;
        }
        let list: Vec<String> = conflicts.iter().map(|c| format!("'{c}'")).collect();
        println!(
            "\n⚠  {} conflict(s) — new replies in thread(s): [{}]",
            conflicts.len(),
            list.join(", ")
        );
        println!("Review updated file, re-run push.");
        return Ok(());
    }

    if p.dry_run {
        for a in &actions {
            println!(
                "Posting [{}] to thread {} ... (dry run)",
                a.kind.name(),
                a.tid
            );
            let body = if a.kind == Kind::Thumbsup {
                "👍"
            } else {
                &a.text
            };
            println!("{}", ApiCall::reply(&t.repo, &a.tid, body).describe());
            if matches!(a.kind, Kind::Resolve | Kind::Thumbsup) && !a.node_id.is_empty() {
                println!("{}", ApiCall::resolve(&a.node_id).describe());
            }
        }
        println!("\nDry run: would post {} action(s).", actions.len());
        return Ok(());
    }

    std::fs::write(&pending, &merged)?;
    store.add(&pending)?;
    git(
        &store.ghub,
        &[
            "commit",
            "-m",
            &format!("pr-sync: pending replies PR-{} ({branch})", t.pr),
        ],
    )?;

    let mut content = merged;
    let mut posted_count = 0;
    for a in &actions {
        println!("Posting [{}] to thread {} ...", a.kind.name(), a.tid);
        match post_one(&gh, &t.repo, a) {
            Ok((id, by, at)) => {
                match stamp(&content, a, &username, &id, &by, &at) {
                    Some(c) => content = c,
                    None => println!("  (could not find the block for thread {} to stamp)", a.tid),
                }
                posted_count += 1;
                println!("  → posted id={id}");
            }
            Err(e) => println!("  ERROR: {e:#}"),
        }
    }
    if posted_count == 0 {
        return Ok(());
    }
    std::fs::write(&pending, &content)?;
    store.add(&pending)?;
    git(&store.ghub, &["commit", "--amend", "--no-edit"])?;
    println!("\nPosted {posted_count} action(s). Amended commit.");
    Ok(())
}

/// Post the reply (and resolve if asked). Returns (id, author, created_at).
fn post_one(gh: &Gh, repo: &str, a: &Action) -> Result<(String, String, String)> {
    let body = if a.kind == Kind::Thumbsup {
        "👍"
    } else {
        &a.text
    };
    let r = gh.call(&ApiCall::reply(repo, &a.tid, body))?;
    let id = pystr(Some(r.get("id").context("reply has no id")?), "");
    let at = py_strip(&pystr(r.get("created_at"), "")).to_owned();
    let by = login(&r)?.to_owned();
    if matches!(a.kind, Kind::Resolve | Kind::Thumbsup) && !a.node_id.is_empty() {
        gh.call(&ApiCall::resolve(&a.node_id))?;
        println!("  → resolved thread {}", a.node_id);
    }
    Ok((id, by, at))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct T {
        #[command(subcommand)]
        c: W,
    }

    #[derive(clap::Subcommand)]
    enum W {
        Sync(Args),
    }

    fn parse(args: &[&str]) -> Result<SyncCmd, clap::Error> {
        let t = T::try_parse_from(std::iter::once("review").chain(args.iter().copied()))?;
        let W::Sync(s) = t.c;
        Ok(s.cmd)
    }

    #[test]
    fn parses_commands() {
        match parse(&["sync", "fetch", "acme/widgets", "142"]).unwrap() {
            SyncCmd::Fetch(t) => {
                assert_eq!((t.repo.as_str(), t.pr), ("acme/widgets", 142));
                assert!(t.dir.is_none());
            }
            SyncCmd::Push(_) => panic!("expected fetch"),
        }
        match parse(&["sync", "push", "o/r", "7", "--dir", "/w", "--dry-run"]).unwrap() {
            SyncCmd::Push(p) => {
                assert!(p.dry_run);
                assert_eq!(p.target.dir.as_deref(), Some(Path::new("/w")));
            }
            SyncCmd::Fetch(_) => panic!("expected push"),
        }
        for bad in [
            &["sync", "fetch", "noslash", "1"][..],
            &["sync", "fetch", "o/r/x", "1"],
            &["sync", "fetch", "o/..", "1"],
            &["sync", "fetch", "o/r\"}", "1"],
            &["sync", "fetch", "o/r", "0"],
            &["sync", "fetch", "o/r", "-1"],
            &["sync", "fetch", "o/r", "x"],
            &["sync", "fetch", "o/r", "1", "--dry-run"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn python_string_helpers() {
        assert_eq!(
            py_splitlines("a\r\nb\rc\nd\u{2028}e\n"),
            ["a", "b", "c", "d", "e"]
        );
        assert_eq!(py_splitlines(""), Vec::<&str>::new());
        assert_eq!(py_splitlines("\n\n"), ["", ""]);
        assert_eq!(py_strip("\u{1c} x \u{3000}"), "x");
        assert_eq!(universal_newlines("a\r\nb\rc"), "a\nb\nc");
    }

    fn thread(id: &str, node: &str, block_body: &str) -> String {
        format!(
            "### 1. 🔴 `f.sh`\n\n<!-- meta\nfilepath: f.sh:L1\nlink: u\nid: {id}\nthread: {node}\n-->\n\n**rev:** hi\n\n<!-- me\n{block_body}\n-->\n"
        )
    }

    #[test]
    fn extract_pending_decisions() {
        let c = [
            thread("1", "N1", "[ ] resolve ; [ ] resolve with 👍\nplain reply"),
            thread("2", "N2", "[x] resolve ; [ ] resolve with 👍\n fix done \n"),
            thread("3", "N3", "[ ] resolve ; [x] resolve with 👍\nignored text"),
            thread("4", "N4", "[ ] resolve ; [ ] resolve with 👍\n"),
            thread("5", "N5", "[x] resolve ; [ ] resolve with 👍\n"),
            thread("6", "N6", "no checkbox line\ntext"),
        ]
        .concat()
            + "<!-- me posted=9 by=me at=t\n[x] reply\nold\n-->\n";
        let a = extract_pending(&c, "me");
        let got: Vec<_> = a
            .iter()
            .map(|a| (a.kind, a.tid.as_str(), a.node_id.as_str(), a.text.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                (Kind::Reply, "1", "N1", "plain reply"),
                (Kind::Resolve, "2", "N2", "fix done"),
                (Kind::Thumbsup, "3", "N3", ""),
            ]
        );
        assert!(extract_pending(&c, "someone-else").is_empty());
    }

    const FRESH: &str = "[ ] resolve ; [ ] resolve with 👍\n";

    #[test]
    fn inject_fills_each_block_once() {
        let new = [thread("1", "N1", FRESH), thread("2", "N2", FRESH)].concat();
        let old = new.clone();
        let acts = vec![
            Action {
                kind: Kind::Resolve,
                tid: "2".into(),
                node_id: "N2".into(),
                text: "done".into(),
            },
            Action {
                kind: Kind::Reply,
                tid: "1".into(),
                node_id: "N1".into(),
                text: "a\\1 b".into(),
            },
        ];
        let (merged, conflicts) = inject_pending(&new, &old, &acts, "me");
        assert!(conflicts.is_empty());
        let want = [
            thread("1", "N1", "[ ] resolve ; [ ] resolve with 👍\na\\1 b"),
            thread("2", "N2", "[x] resolve ; [ ] resolve with 👍\ndone"),
        ]
        .concat();
        assert_eq!(merged, want);
        assert_eq!(extract_pending(&merged, "me").len(), 2);
    }

    #[test]
    fn inject_reports_conflicts() {
        let old = [thread("1", "N1", FRESH), thread("2", "N2", FRESH)].concat();
        // A new reply in thread 1 since the last fetch.
        let new = old.replacen(
            "**rev:** hi\n",
            "**rev:** hi\n\n↳ **bob** (id=5): wait\n  u\n",
            1,
        );
        let a = |tid: &str| Action {
            kind: Kind::Reply,
            tid: tid.into(),
            node_id: String::new(),
            text: "x".into(),
        };
        let (_, c) = inject_pending(&new, &old, &[a("1"), a("2")], "me");
        assert_eq!(c, ["1"]);
        // Thread 1 resolved remotely: no block left in its section. Must not
        // fall through to thread 2's block.
        let resolved1 = thread("1", "N1", FRESH).replace(&format!("<!-- me\n{FRESH}\n-->\n"), "");
        let new = resolved1 + &thread("2", "N2", FRESH);
        let (merged, c) = inject_pending(&new, &old, &[a("1")], "me");
        assert_eq!(c, ["1"]);
        assert_eq!(merged, new);
        // Thread vanished entirely.
        let (_, c) = inject_pending(&thread("2", "N2", FRESH), &old, &[a("1")], "me");
        assert_eq!(c, ["1"]);
    }

    #[test]
    fn stamp_targets_its_own_thread() {
        let content = [
            thread("1", "N1", FRESH),
            thread("2", "N2", "[x] resolve ; [ ] resolve with 👍\ndone"),
        ]
        .concat();
        let a = Action {
            kind: Kind::Resolve,
            tid: "2".into(),
            node_id: "N2".into(),
            text: "done".into(),
        };
        let out = stamp(&content, &a, "me", "77", "me", "2026-01-01T00:00:00Z").unwrap();
        let want = [thread("1", "N1", FRESH), String::new()].concat()
            + &thread("2", "N2", "[x] resolve ; [ ] resolve with 👍\ndone").replace(
                "<!-- me\n",
                "<!-- me posted=77 by=me at=2026-01-01T00:00:00Z\n",
            );
        assert_eq!(out, want);
        assert_eq!(extract_pending(&out, "me").len(), 0);
        let ps = posted_state(&out, "me");
        assert_eq!(
            ps.get("2"),
            Some(&Posted {
                posted_id: "77".into(),
                by: "me".into(),
                at: "2026-01-01T00:00:00Z".into(),
                kind: "resolve",
                body: "done".into(),
            })
        );
        let thumbs = Action {
            kind: Kind::Thumbsup,
            tid: "1".into(),
            node_id: "N1".into(),
            text: String::new(),
        };
        let out = stamp(&out, &thumbs, "me", "78", "me", "t").unwrap();
        assert!(
            out.contains("<!-- me posted=78 by=me at=t\n[ ] resolve ; [x] resolve with 👍\n-->")
        );
        assert_eq!(posted_state(&out, "me")["1"].kind, "thumbsup");
    }

    #[test]
    fn apply_action_variants() {
        let fresh = "<!-- me\n[ ] resolve ; [ ] resolve with 👍\n\n-->";
        assert_eq!(
            apply_action_to_block(fresh, Kind::Reply, "hello"),
            "<!-- me\n[ ] resolve ; [ ] resolve with 👍\nhello\n-->"
        );
        assert_eq!(
            apply_action_to_block(fresh, Kind::Thumbsup, ""),
            "<!-- me\n[ ] resolve ; [x] resolve with 👍\n\n-->"
        );
        assert_eq!(
            apply_action_to_block(
                "<!-- me\n[ ] resolve ; [ ] resolve with 👍\nold-->  ",
                Kind::Resolve,
                "t"
            ),
            "<!-- me\n[x] resolve ; [ ] resolve with 👍\nold\nt\n-->"
        );
    }

    fn discussion() -> Discussion {
        let user = |l: &str| json!({ "login": l });
        Discussion {
            reviews: vec![
                json!({"id": 10, "user": user("alice"), "state": "COMMENTED", "submitted_at": "2026-01-01T00:00:00Z", "body": ""}),
                json!({"id": 11, "user": user("bob"), "state": "APPROVED", "submitted_at": "2026-01-03T00:00:00Z", "body": " lgtm "}),
                json!({"id": 12, "user": user("ci[bot]"), "state": "COMMENTED", "submitted_at": "2026-01-02T00:00:00Z", "body": "bot says"}),
                json!({"id": 13, "user": user("carol"), "state": "COMMENTED", "submitted_at": "2026-01-04T00:00:00Z", "body": null}),
            ],
            comments: vec![
                json!({"id": 100, "pull_request_review_id": 10, "path": "b/z.sh", "line": null, "original_line": 5, "html_url": "u100", "user": user("alice"), "body": "root z", "diff_hunk": "@@ x\r\n+1\n+2"}),
                json!({"id": 101, "pull_request_review_id": 10, "path": "a.sh", "line": 3, "html_url": "u101", "user": user("alice"), "body": "root a"}),
                json!({"id": 102, "pull_request_review_id": 10, "in_reply_to_id": 100, "path": "b/z.sh", "html_url": "u102", "user": user("me"), "body": "reply\r\n"}),
            ],
            issue_comments: vec![
                json!({"id": 200, "user": user("dave"), "created_at": "t1", "body": "general"}),
                json!({"id": 201, "user": user("dependabot[bot]"), "created_at": "t2", "body": "x".repeat(400)}),
            ],
            resolved: HashSet::from([101]),
            node_ids: HashMap::from([(100, "PRRT_a".to_owned()), (101, "PRRT_b".to_owned())]),
        }
    }

    #[test]
    fn render_buckets_and_counts() {
        let d = discussion();
        let r = render(&d, &HashMap::new(), "me").unwrap();
        assert_eq!((r.kept, r.skipped, r.roots, r.resolved), (3, 1, 2, 1));
        let hp = r.buckets[0].join("\n");
        // review 10 is pending (one unresolved thread); threads sorted by path.
        let a = hp.find("### 1. ✅ `a.sh`").unwrap();
        let z = hp.find("### 2. 🔴 `z.sh`").unwrap();
        assert!(a < z);
        assert!(hp.contains("filepath: b/z.sh:L5\nlink: u100\nid: 100\nthread: PRRT_a\n-->"));
        assert!(hp.contains("```diff\n@@ x\n+1\n+2\n```"));
        assert!(hp.contains("↳ **me** (id=102): reply\n  u102\n"));
        assert!(hp.contains("<!-- me\n[ ] resolve ; [ ] resolve with 👍\n\n-->"));
        assert_eq!(
            hp.matches("<!-- me\n").count(),
            1,
            "resolved thread has no block"
        );
        assert!(hp.contains("## General Comments\n\n**dave** (t1) id=200:\n\ngeneral\n"));
        // approved review with body and no threads: resolved.
        assert!(
            r.buckets[1]
                .join("\n")
                .contains("## Review 11 — bob [APPROVED] — 2026-01-03T00:00:00Z\n\n> lgtm\n")
        );
        let bp = r.buckets[2].join("\n");
        assert!(bp.contains("## Review 12 — ci[bot] [COMMENTED]"));
        assert!(bp.contains(&format!("**dependabot[bot]** (t2): {}\n", "x".repeat(300))));
        assert!(r.buckets[3].is_empty());
        // Newest review first within a bucket.
        let files = file_contents(&["H".into()], &r.buckets);
        assert!(
            files[0]
                .as_deref()
                .unwrap()
                .starts_with("H\n*(Human Pending)*\n\n---\n")
        );
        assert!(files[3].is_none());
    }

    #[test]
    fn render_keeps_posted_stamp() {
        let d = discussion();
        let posted = HashMap::from([(
            "100".to_owned(),
            Posted {
                posted_id: "9".into(),
                by: "me".into(),
                at: "t".into(),
                kind: "reply",
                body: "done".into(),
            },
        )]);
        let hp = render(&d, &posted, "me").unwrap().buckets[0].join("\n");
        assert!(hp.contains("<!-- me posted=9 by=me at=t\n[x] reply\ndone\n-->"));
        assert!(extract_pending(&hp, "me").is_empty());
    }

    #[test]
    fn api_calls_keep_bodies_off_argv() {
        let r = ApiCall::reply("o/r", "123", "it's \"quoted\"\n👍");
        assert_eq!(r.endpoint, "repos/o/r/pulls/comments/123/replies");
        assert_eq!(r.body, json!({"body": "it's \"quoted\"\n👍"}));
        assert!(!r.args().iter().any(|a| a.contains("quoted")));
        assert!(
            r.describe()
                .contains("would run: gh api -X POST repos/o/r/pulls/comments/123/replies")
        );
        let m = ApiCall::resolve("PRRT_x\"}");
        assert_eq!(m.endpoint, "graphql");
        assert_eq!(m.body["variables"], json!({"threadId": "PRRT_x\"}"}));
        assert!(!m.body["query"].as_str().unwrap().contains("PRRT_x"));
    }

    #[test]
    fn branch_dir_is_one_component() {
        assert_eq!(branch_dir("alice/feat/x").unwrap(), "alice-feat-x");
        for bad in ["", ".", ".."] {
            assert!(branch_dir(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn isoformat_shape() {
        let s = utc_isoformat();
        assert!(s.ends_with("+00:00") && s.len() >= 25, "{s}");
    }
}
