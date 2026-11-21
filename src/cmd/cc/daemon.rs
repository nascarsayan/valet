//! The `valet cc` daemon: one loop that tails attached sessions' transcripts into
//! their Slack threads, polls the DM for replies, and drives the panes.

use super::live::{self, Live};
use super::render;
use super::slack::{self, Slack, ts_gt};
use super::transcript;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

macro_rules! log {
    ($($t:tt)*) => { super::log(&format!($($t)*)) };
}
macro_rules! warn {
    ($($t:tt)*) => { super::warn(&format!($($t)*)) };
}
macro_rules! debug {
    ($($t:tt)*) => { if super::debug_on() { super::debug(&format!($($t)*)) } };
}

const TEXT_LIMIT: usize = 3500;
const POLL_MS: u64 = 2000;
const IDLE_POLL_MS: u64 = 6000;

const THREAD_HELP: &str = "*In a session thread*
• `text` anything else goes to Claude
• `1` `2` … picks a dialog option
• `esc` `enter` `up` `down` `tab` `space` `ctrlc` keys
• `keys K1 K2` raw keys
• `screen` terminal snapshot
• `detach` stop mirroring
• `help` this list
_Main DM: `help` for the rest_";

#[derive(Serialize, Deserialize, Default, Clone)]
#[serde(default)]
pub(super) struct Sess {
    pane: String,
    name: String,
    session_id: String,
    transcript: String,
    offset: u64,
    pub(super) root_ts: String,
    thread_seen: String,
    /// attached automatically by AFK mode (detached + deleted on AFK off)
    auto: bool,
    activity_ts: String,
    activity: Vec<String>,
    activity_total: usize,
    #[serde(skip)]
    activity_dirty: bool,
    pending_q: Option<String>,
    suppress_dialog: bool,
    last_dialog: String,
    cand_dialog: String,
    last_status: String,
    pending_acks: Vec<String>,
    injected: Vec<String>,
    recent_human: Vec<String>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub(super) struct State {
    pub(super) afk: bool,
    /// Mirror verbosity (see [`super::MAX_VERBOSE`]); None = the default.
    pub(super) verbose: Option<u8>,
    pub(super) sessions: BTreeMap<String, Sess>,
    active: Option<String>,
    hist_seen: String,
    listing: Vec<String>,
    /// bot messages outside session threads (command replies, notices) to remove on cleanup
    misc_ts: Vec<String>,
    /// roots of ended / detached threads, kept until cleanup
    ended_roots: Vec<String>,
    delete_roots: Vec<String>,
    delete_msgs: Vec<String>,
}

pub(super) struct Cx {
    pub channel: String,
    pub user_id: String,
    /// The bot. valet cc never uses the operator's own Slack session: their
    /// messages are cleared on their PC (the "Clear DM" bookmark).
    pub slack: Slack,
    pub io: live::Io,
    pub bot_user: String,
    /// Mirror verbosity, 0..=MAX_VERBOSE: 0 questions/plans/dialogs only;
    /// 1 + Claude's text and typed prompts; 2 + tool calls and errors;
    /// 3 + the first line of every tool result.
    pub verbose: std::cell::Cell<u8>,
}

impl Cx {
    fn v(&self) -> u8 {
        self.verbose.get()
    }
}

impl State {
    pub(super) fn level(&self) -> u8 {
        self.verbose.unwrap_or(super::DEFAULT_VERBOSE)
    }
}

impl Sess {
    fn say(&mut self, cx: &Cx, text: &str) {
        self.flush_activity(cx);
        self.activity_ts.clear();
        self.activity.clear();
        self.activity_total = 0;
        for c in render::chunk(text, TEXT_LIMIT) {
            if let Err(e) = cx.slack.post(&cx.channel, Some(&self.root_ts), &c) {
                warn!("post to {}: {e}", self.pane);
            }
        }
    }

    /// Like [`Self::say`], for Claude's markdown: sent as Block Kit, so
    /// Slack renders tables, code and lists natively.
    fn say_md(&mut self, cx: &Cx, markdown: &str) {
        self.flush_activity(cx);
        self.activity_ts.clear();
        self.activity.clear();
        self.activity_total = 0;
        if let Err(e) = cx
            .slack
            .post_md(&cx.channel, Some(&self.root_ts), markdown, true)
        {
            warn!("post to {}: {e}", self.pane);
        }
    }

    fn add_activity(&mut self, line: String) {
        self.activity.push(line);
        self.activity_total += 1;
        if self.activity.len() > 12 {
            self.activity.remove(0);
        }
        self.activity_dirty = true;
    }

    fn flush_activity(&mut self, cx: &Cx) {
        if !self.activity_dirty {
            return;
        }
        self.activity_dirty = false;
        let hidden = self.activity_total.saturating_sub(self.activity.len());
        let mut text = String::new();
        if hidden > 0 {
            text.push_str(&format!("_…{hidden} earlier_\n"));
        }
        text.push_str(&self.activity.join("\n"));
        if self.activity_ts.is_empty() {
            match cx.slack.post(&cx.channel, Some(&self.root_ts), &text) {
                Ok(ts) => self.activity_ts = ts,
                Err(e) => warn!("activity post: {e}"),
            }
        } else if let Err(e) = cx.slack.update(&cx.channel, &self.activity_ts, &text) {
            warn!("activity update: {e}");
        }
    }

    fn human(&mut self, cx: &Cx, text: &str) {
        let n = render::norm(text);
        if n.is_empty() {
            return;
        }
        if let Some(i) = self
            .injected
            .iter()
            .position(|x| *x == n || n.contains(x.as_str()))
        {
            self.injected.remove(i);
            debug!(
                "session {}: prompt from Slack reached the transcript",
                self.pane
            );
            self.remember_human(n);
            return;
        }
        if self.recent_human.contains(&n) {
            return;
        }
        debug!(
            "session {}: prompt typed in the terminal, {} chars",
            self.pane,
            n.len()
        );
        self.remember_human(n);
        if cx.v() < 1 {
            return;
        }
        if let Some(cmd) = command_text(text) {
            self.say(cx, &format!("⌨️ _terminal:_ `{}`", render::esc(&cmd)));
        } else {
            let quoted: Vec<String> = render::md(text).lines().map(|l| format!("> {l}")).collect();
            self.say(
                cx,
                &format!("⌨️ _typed in terminal:_\n{}", quoted.join("\n")),
            );
        }
    }

    fn remember_human(&mut self, n: String) {
        self.recent_human.push(n);
        if self.recent_human.len() > 30 {
            self.recent_human.remove(0);
        }
    }

    fn handle_event(&mut self, cx: &Cx, ev: &Value) {
        if ev["isSidechain"].as_bool() == Some(true) {
            return;
        }
        match ev["type"].as_str().unwrap_or("") {
            "assistant" => {
                for b in ev["message"]["content"].as_array().into_iter().flatten() {
                    match b["type"].as_str().unwrap_or("") {
                        "text" => {
                            let t = b["text"].as_str().unwrap_or("");
                            if t.trim().is_empty() {
                                continue;
                            }
                            let pre = if ev["isApiErrorMessage"].as_bool() == Some(true) {
                                "⚠️ "
                            } else {
                                ""
                            };
                            debug!("session {}: assistant text, {} chars", self.pane, t.len());
                            if !pre.is_empty() {
                                warn!("session {}: Claude reported an API error", self.pane);
                            }
                            if cx.v() >= 1 || !pre.is_empty() {
                                self.say_md(cx, &format!("{pre}{t}"));
                            }
                        }
                        "tool_use" => {
                            let name = b["name"].as_str().unwrap_or("");
                            let input = &b["input"];
                            debug!("session {}: tool_use {name}", self.pane);
                            match name {
                                "AskUserQuestion" => {
                                    let n = input["questions"].as_array().map_or(0, Vec::len);
                                    log!(
                                        "session {}: question posted ({n} question(s))",
                                        self.pane
                                    );
                                    self.say(cx, &render::question(input));
                                    self.pending_q = b["id"].as_str().map(str::to_string);
                                    self.suppress_dialog = true;
                                }
                                "ExitPlanMode" => {
                                    log!("session {}: plan posted for approval", self.pane);
                                    let plan = input["plan"].as_str().unwrap_or("");
                                    self.say(
                                        cx,
                                        &format!("📋 *Plan for approval:*\n{}", render::md(plan)),
                                    );
                                }
                                _ if cx.v() >= 2 => {
                                    self.add_activity(render::tool_line(name, input))
                                }
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                }
            }
            "user" => {
                if ev["isMeta"].as_bool() == Some(true) {
                    return;
                }
                let human = ev["origin"]["kind"] == "human" || ev["promptSource"].is_string();
                let content = &ev["message"]["content"];
                if let Some(t) = content.as_str() {
                    if human || command_text(t).is_some() {
                        self.human(cx, t);
                    }
                    return;
                }
                let arr = content.as_array().cloned().unwrap_or_default();
                let has_result = arr.iter().any(|b| b["type"] == "tool_result");
                for b in &arr {
                    match b["type"].as_str().unwrap_or("") {
                        "tool_result" => {
                            let id = b["tool_use_id"].as_str().map(str::to_string);
                            let text = result_text(b);
                            if id.is_some() && id == self.pending_q {
                                self.pending_q = None;
                                self.suppress_dialog = false;
                                let ans = text
                                    .split_once("answered: ")
                                    .map(|(_, r)| {
                                        let r = r.split(". You can now").next().unwrap_or(r);
                                        r.split(". Read the answers")
                                            .next()
                                            .unwrap_or(r)
                                            .to_string()
                                    })
                                    .unwrap_or_else(|| render::trunc(&text, 300));
                                self.say(cx, &format!("☑️ {}", render::esc(&ans)));
                            } else if b["is_error"].as_bool() == Some(true) && cx.v() >= 2 {
                                let first = text.lines().next().unwrap_or("");
                                self.add_activity(format!(
                                    "  ↳ ❌ {}",
                                    render::esc(&render::trunc(first, 160))
                                ));
                            } else if cx.v() >= 3 {
                                let first =
                                    text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
                                if !first.is_empty() {
                                    self.add_activity(format!(
                                        "  ↳ {}",
                                        render::esc(&render::trunc(first, 160))
                                    ));
                                }
                            }
                        }
                        "text" => {
                            let t = b["text"].as_str().unwrap_or("");
                            if t.starts_with("[Request interrupted") {
                                if cx.v() >= 1 {
                                    self.say(cx, "⏹ _Interrupted._");
                                }
                            } else if human && !has_result {
                                self.human(cx, t);
                            }
                        }
                        _ => {}
                    }
                }
            }
            "queue-operation" if ev["operation"] == "enqueue" => {
                if let Some(t) = ev["content"].as_str() {
                    self.human(cx, t);
                }
            }
            "system" if ev["subtype"] == "compact_boundary" && cx.v() >= 1 => {
                self.say(cx, "🗜 _Context compacted._")
            }
            _ => {}
        }
    }
}

/// The bot's messages a DM wipe deletes, thread replies before their root.
/// Threads in `keep` (mirrored sessions) are skipped. The operator's own
/// messages are left alone: their PC clears those (the "Clear DM" bookmark).
pub(super) fn clear_targets(cx: &Cx, keep: &HashSet<String>) -> Result<Vec<String>> {
    let ch = cx.channel.as_str();
    let is_bot =
        |m: &Value| m["user"].as_str() == Some(cx.bot_user.as_str()) || !m["bot_id"].is_null();
    let mut targets: Vec<String> = Vec::new();
    let mut add = |m: &Value| {
        if is_bot(m) {
            targets.push(m["ts"].as_str().unwrap_or_default().to_owned());
        }
    };
    let mut cursor = String::new();
    loop {
        let mut p = vec![("channel", ch), ("limit", "200")];
        if !cursor.is_empty() {
            p.push(("cursor", cursor.as_str()));
        }
        let v = cx.slack.call("conversations.history", &p)?;
        for m in v["messages"].as_array().into_iter().flatten() {
            let ts = m["ts"].as_str().unwrap_or_default();
            if keep.contains(ts) {
                continue;
            }
            if m["reply_count"].as_u64().unwrap_or(0) > 0 {
                for r in cx.slack.replies(ch, ts, None)?.iter().skip(1) {
                    add(r);
                }
            }
            add(m);
        }
        let next = v["response_metadata"]["next_cursor"].as_str().unwrap_or("");
        if v["has_more"].as_bool() != Some(true) || next.is_empty() {
            break;
        }
        cursor = next.to_owned();
    }
    Ok(targets)
}

fn result_text(b: &Value) -> String {
    match &b["content"] {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .filter_map(|x| x["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// "<command-name>/foo</command-name><command-args>x</command-args>" -> "/foo x"
fn command_text(t: &str) -> Option<String> {
    let name = t
        .split_once("<command-name>")?
        .1
        .split_once("</command-name>")?
        .0;
    let args = t
        .split_once("<command-args>")
        .and_then(|(_, r)| r.split_once("</command-args>"))
        .map(|(a, _)| a)
        .unwrap_or("");
    Some(
        format!("{} {}", name.trim(), args.trim())
            .trim()
            .to_string(),
    )
}

pub(super) struct Daemon {
    cx: Cx,
    st: State,
    agents: Vec<Live>,
    tick: u64,
}

enum Outcome {
    Keep,
    Ended,
}

impl Daemon {
    pub(super) fn new(cx: Cx, st: State) -> Self {
        cx.verbose.set(st.level());
        Daemon {
            cx,
            st,
            agents: vec![],
            tick: 0,
        }
    }

    fn agent(&self, pane: &str) -> Option<Live> {
        self.agents.iter().find(|a| a.pane == pane).cloned()
    }

    fn top(&mut self, thread: Option<&str>, text: &str) {
        for c in render::chunk(text, TEXT_LIMIT) {
            match self.cx.slack.post(&self.cx.channel, thread, &c) {
                // Tracked for `afk off` / `clear`, including replies under your
                // commands (session threads are deleted whole anyway).
                Ok(ts) => self.st.misc_ts.push(ts),
                Err(e) => warn!("post: {e}"),
            }
        }
    }

    fn attach(&mut self, a: &Live, auto: bool) -> Result<String> {
        if self.st.sessions.contains_key(&a.pane) {
            return Ok(format!("{} already attached", a.display()));
        }
        let tp = transcript::find(&a.session_id).unwrap_or_default();
        let offset = if tp.is_empty() {
            0
        } else {
            transcript::len(&tp)
        };
        let header = format!(
            "🟢 *{}* — Claude Code\n`{}` · pid `{}` · {} · {}\n_Reply in this thread to talk to Claude · `help` for keys & commands_",
            render::esc(&a.display()),
            render::esc(&render::tilde(&a.cwd)),
            a.pane,
            a.route(),
            a.status
        );
        let root = self.cx.slack.post(&self.cx.channel, None, &header)?;
        let mut s = Sess {
            pane: a.pane.clone(),
            name: a.display(),
            session_id: a.session_id.clone(),
            transcript: tp.clone(),
            offset,
            root_ts: root.clone(),
            thread_seen: root,
            auto,
            last_status: a.status.clone(),
            ..Default::default()
        };
        if !tp.is_empty()
            && let Some(t) = transcript::last_assistant_text(&tp)
        {
            s.say(&self.cx, &format!("_Last message:_\n{}", render::md(&t)));
        }
        log!("attached {} ({}) auto={auto}", a.pane, s.name);
        let name = s.name.clone();
        self.st.sessions.insert(a.pane.clone(), s);
        if self.st.active.is_none() || !auto {
            self.st.active = Some(a.pane.clone());
        }
        Ok(format!("attached {name}"))
    }

    fn detach(&mut self, pane: &str, delete: bool) -> String {
        let Some(mut s) = self.st.sessions.remove(pane) else {
            return format!("{pane} not attached");
        };
        log!("detach {pane} ({}): delete thread={delete}", s.name);
        if delete {
            self.st.delete_roots.push(s.root_ts.clone());
        } else {
            s.say(&self.cx, "⚪️ _Detached — no longer mirrored._");
            self.st.ended_roots.push(s.root_ts.clone());
        }
        if self.st.active.as_deref() == Some(pane) {
            self.st.active = self.st.sessions.keys().next().cloned();
        }
        format!("detached {}", s.name)
    }

    /// `verbose`: a new mirror verbosity (with `on`); `on` while on only
    /// changes the verbosity.
    fn set_afk(&mut self, on: bool, announce: bool, verbose: Option<u8>) -> String {
        log!("afk {}", if on { "on" } else { "off" });
        if let (true, Some(v)) = (on, verbose) {
            self.st.verbose = Some(v);
            self.cx.verbose.set(v);
        }
        if on && self.st.afk {
            return format!(
                "AFK already on: {} sessions mirrored, verbosity v{}",
                self.st.sessions.len(),
                self.st.level()
            );
        }
        if on {
            self.st.afk = true;
            let agents = self.agents.clone();
            let mut n = 0;
            for a in &agents {
                if !self.st.sessions.contains_key(&a.pane) && self.attach(a, true).is_ok() {
                    n += 1;
                }
            }
            if announce {
                let msg = format!(
                    "🟠 *AFK mode on* (v{}) — mirroring {} Claude session(s). Type `help` for commands.",
                    self.st.level(),
                    self.st.sessions.len()
                );
                self.top(None, &msg);
            }
            format!(
                "AFK on (v{}): {} sessions mirrored ({n} newly attached)",
                self.st.level(),
                self.st.sessions.len()
            )
        } else {
            self.st.afk = false;
            let autos: Vec<String> = self
                .st
                .sessions
                .iter()
                .filter(|(_, s)| s.auto)
                .map(|(p, _)| p.clone())
                .collect();
            for p in &autos {
                self.detach(p, true);
            }
            let ended = std::mem::take(&mut self.st.ended_roots);
            self.st.delete_roots.extend(ended);
            let misc = std::mem::take(&mut self.st.misc_ts);
            self.st.delete_msgs.extend(misc);
            format!(
                "AFK off: stopped mirroring {} session(s); deleting {} thread(s) in the background",
                autos.len(),
                self.st.delete_roots.len()
            )
        }
    }

    /// `clear`: delete every bot message and thread reply in the DM now.
    /// Mirrored threads are kept; the operator's own messages are cleared on
    /// their PC. Returns a note only when something was left behind.
    fn clear_dm(&mut self) -> Option<String> {
        let keep: HashSet<String> = self
            .st
            .sessions
            .values()
            .map(|s| s.root_ts.clone())
            .collect();
        let targets = match clear_targets(&self.cx, &keep) {
            Ok(t) => t,
            Err(e) => return Some(format!("clear failed: {e}")),
        };
        let ch = self.cx.channel.clone();
        let mut failed = 0;
        for ts in &targets {
            if let Err(e) = self.cx.slack.delete(&ch, ts) {
                failed += 1;
                warn!("clear {ts}: {e}");
            }
        }
        log!(
            "clear: deleted {} of {}",
            targets.len() - failed,
            targets.len()
        );
        self.st.misc_ts.clear();
        self.st.ended_roots.clear();
        self.st.delete_msgs.clear();
        let mut notes = Vec::new();
        if failed > 0 {
            notes.push(format!(
                "{failed} message(s) could not be deleted (journalctl --user -u valet-cc)."
            ));
        }
        if !keep.is_empty() {
            notes.push(format!(
                "Kept {} mirrored thread(s); `afk off` removes them.",
                keep.len()
            ));
        }
        (!notes.is_empty()).then(|| format!("🧹 {}", notes.join("\n")))
    }

    fn cleanup(&mut self) -> String {
        let ended = std::mem::take(&mut self.st.ended_roots);
        let n = ended.len();
        self.st.delete_roots.extend(ended);
        let misc = std::mem::take(&mut self.st.misc_ts);
        let m = misc.len();
        self.st.delete_msgs.extend(misc);
        format!("queued {n} ended thread(s) and {m} notice(s) for deletion")
    }

    fn status_text(&self) -> String {
        let mut out = format!(
            "AFK: {} · mirrored: {} · pending deletes: {} threads/{} msgs\n",
            if self.st.afk { "on" } else { "off" },
            self.st.sessions.len(),
            self.st.delete_roots.len(),
            self.st.delete_msgs.len(),
        );
        for (p, s) in &self.st.sessions {
            let st = self
                .agent(p)
                .map(|a| format!("{} · {}", a.status, a.route()))
                .unwrap_or_else(|| "gone".into());
            let act = if self.st.active.as_deref() == Some(p) {
                " ← active"
            } else {
                ""
            };
            out.push_str(&format!(
                "  {p}  {}  [{st}]{}{act}\n",
                s.name,
                if s.auto { " (afk)" } else { "" }
            ));
        }
        out
    }

    fn tick_session(&mut self, s: &mut Sess) -> Outcome {
        let cx = &self.cx;
        let Some(a) = self.agents.iter().find(|a| a.pane == s.pane).cloned() else {
            log!(
                "session {} ({}): ended (no longer in the registry)",
                s.pane,
                s.name
            );
            s.say(cx, "🔴 _Session ended (Claude exited)._");
            return Outcome::Ended;
        };
        if !a.session_id.is_empty() && a.session_id != s.session_id {
            log!(
                "session {}: new conversation {} (was {})",
                s.pane,
                a.session_id,
                s.session_id
            );
            s.session_id = a.session_id.clone();
            s.transcript = transcript::find(&s.session_id).unwrap_or_default();
            let len = transcript::len(&s.transcript);
            s.offset = if len < 100_000 { 0 } else { len };
            s.pending_q = None;
            s.suppress_dialog = false;
            s.say(cx, "🔄 _New conversation in this pane._");
        }
        if s.transcript.is_empty()
            && let Some(p) = transcript::find(&s.session_id)
        {
            log!("session {}: transcript found {p}", s.pane);
            s.transcript = p;
            s.offset = 0;
        }
        if !s.transcript.is_empty() {
            let path = s.transcript.clone();
            let evs = transcript::read_new(&path, &mut s.offset);
            if !evs.is_empty() {
                debug!(
                    "session {}: {} transcript event(s), offset {}",
                    s.pane,
                    evs.len(),
                    s.offset
                );
            }
            for ev in evs {
                s.handle_event(cx, &ev);
            }
        }
        if a.status == "blocked" {
            let screen = cx.io.screen(&a.term);
            if let Err(e) = &screen {
                debug!(
                    "session {}: blocked but no screen ({}): {e}",
                    s.pane,
                    a.route()
                );
            }
            if let Ok(scr) = screen {
                let d = render::dialog(&scr);
                if !d.is_empty() && d == s.cand_dialog && d != s.last_dialog {
                    let first = d.lines().next().unwrap_or("").trim().to_owned();
                    if s.suppress_dialog {
                        debug!("session {}: dialog is the posted question: {first}", s.pane);
                        s.suppress_dialog = false;
                    } else {
                        log!(
                            "session {}: dialog posted: {}",
                            s.pane,
                            render::trunc(&first, 60)
                        );
                        s.say(
                            cx,
                            &format!(
                                "⚠️ *Claude is waiting for you:*\n```\n{}\n```\n_Reply with an option number, free text, or `esc`._",
                                render::esc(&d)
                            ),
                        );
                    }
                    s.last_dialog = d.clone();
                }
                s.cand_dialog = d;
            }
        } else {
            s.cand_dialog.clear();
            s.last_dialog.clear();
        }
        s.flush_activity(cx);
        let prev = std::mem::replace(&mut s.last_status, a.status.clone());
        if prev != a.status {
            log!("session {}: {prev} -> {} ({})", s.pane, a.status, a.route());
        }
        if prev == "working" && (a.status == "idle" || a.status == "done") {
            for ts in s.pending_acks.drain(..) {
                cx.slack.react(&cx.channel, &ts, "white_check_mark");
            }
        }
        Outcome::Keep
    }

    fn deliver(&mut self, pane: &str, text: &str, ts: &str) {
        let Some(a) = self.agent(pane) else {
            self.top(None, "That session is gone. `ls` lists the running ones.");
            return;
        };
        self.st.active = Some(pane.to_string());
        if a.term == live::Term::None {
            log!("deliver {pane}: refused, read-only ({})", a.route());
            let root = self.st.sessions.get(pane).map(|s| s.root_ts.clone());
            self.top(
                root.as_deref(),
                &format!(
                    "📵 _Read-only: this session runs in a plain terminal (`{}`), not a herdr or tmux pane, so I can't type into it._",
                    render::esc(&a.tty)
                ),
            );
            return;
        }
        self.cx.slack.react(&self.cx.channel, ts, "eyes");
        let (io, term) = (&self.cx.io, &a.term);
        let Some(s) = self.st.sessions.get_mut(pane) else {
            return;
        };
        s.pending_acks.push(ts.to_string());
        let t = text.trim();
        let route = a.route();
        // Input errors are logged, not shown: the screen / next dialog tells the story.
        let check = |what: &str, r: Result<()>| {
            if let Err(e) = r {
                warn!("deliver {pane}: {what} failed ({route}): {e}");
            }
        };
        if a.status == "blocked" {
            let digits: String = t
                .chars()
                .filter(|c| !c.is_whitespace() && *c != ',')
                .collect();
            if !digits.is_empty() && digits.len() <= 3 && digits.chars().all(|c| c.is_ascii_digit())
            {
                log!("deliver {pane}: dialog option key(s) {digits} via {route}");
                for c in digits.chars() {
                    let k = c.to_string();
                    check("key", io.keys(term, &[k.as_str()]));
                    std::thread::sleep(Duration::from_millis(250));
                }
                return;
            }
            let dialog = if s.last_dialog.is_empty() {
                io.screen(term)
                    .map(|x| render::dialog(&x))
                    .unwrap_or_default()
            } else {
                s.last_dialog.clone()
            };
            if let Some(n) = render::type_something_option(&dialog) {
                log!(
                    "deliver {pane}: free-text answer via option {n} ({} chars, {route})",
                    t.len()
                );
                check("option key", io.keys(term, &[n.as_str()]));
                std::thread::sleep(Duration::from_millis(400));
                check("text", io.text(term, t));
                std::thread::sleep(Duration::from_millis(300));
                check("enter", io.keys(term, &["enter"]));
                return;
            }
            // Permission or other dialog: decline it and send the text as guidance.
            log!("deliver {pane}: text while a dialog is open: esc, then prompt ({route})");
            check("esc", io.keys(term, &["esc"]));
            std::thread::sleep(Duration::from_millis(1200));
        } else {
            log!(
                "deliver {pane}: prompt, {} chars, session {} ({route})",
                t.len(),
                a.status
            );
        }
        s.injected.push(render::norm(t));
        if s.injected.len() > 20 {
            s.injected.remove(0);
        }
        check("prompt", io.prompt(term, t));
    }

    /// pid, exact name, session-id prefix, `ls` list number, or name substring.
    fn resolve_target(&self, arg: &str) -> Option<Live> {
        let arg = arg.trim();
        if arg.is_empty() {
            return None;
        }
        if let Some(a) = self.agents.iter().find(|a| {
            a.pane == arg || a.name == arg || (arg.len() >= 6 && a.session_id.starts_with(arg))
        }) {
            return Some(a.clone());
        }
        if let Ok(n) = arg.parse::<usize>() {
            let pane = self.st.listing.get(n.wrapping_sub(1))?;
            return self.agent(pane);
        }
        let low = arg.to_lowercase();
        self.agents
            .iter()
            .find(|a| a.display().to_lowercase().contains(&low))
            .cloned()
    }

    /// A top-level DM message: one `valet cc` command line, answered in a thread
    /// under it.
    fn shell(&mut self, text: &str, thread: &str) {
        match super::parse_slack(text) {
            Ok(sub) => self.run_sub(sub, thread),
            // Already Slack mrkdwn (bullets, `code`): post as is, not in a code block.
            Err(help) => self.top(Some(thread), help.trim_end()),
        }
    }

    fn run_sub(&mut self, sub: super::Sub, thread: &str) {
        use super::{DaemonCmd, SessionCmd, Sub};
        let th = Some(thread);
        let which = "Which session? `session ls` lists them; then use its number, name or pid.";
        match sub {
            Sub::Afk { state: None, .. } => {
                let on = if self.st.afk { "on" } else { "off" };
                let v = self.st.level();
                self.top(
                    th,
                    &format!("AFK is *{on}* (v{v})  ·  `afk on [v0-v3]` / `afk off`"),
                );
            }
            Sub::Afk {
                state: Some(s),
                level,
                verbose,
            } if s == "on" => match super::afk_level(level.as_deref(), verbose.as_deref()) {
                Ok(v) => {
                    let r = self.set_afk(true, false, v);
                    self.top(th, &format!("🟠 {r}"));
                }
                Err(e) => self.top(th, &format!("{e:#}")),
            },
            Sub::Afk { .. } => {
                // Its threads (and, with your token, this message) are being
                // deleted; a reply would only linger. Acknowledge with a reaction.
                self.set_afk(false, false, None);
                self.cx.slack.react(&self.cx.channel, thread, "white_check_mark");
            }
            Sub::Session(SessionCmd::Ls) => self.list_sessions(th),
            Sub::Session(SessionCmd::Attach { session }) => {
                match session.and_then(|s| self.resolve_target(&s)) {
                    Some(a) => {
                        let r = self.attach(&a, self.st.afk).unwrap_or_else(|e| e.to_string());
                        self.top(th, &r);
                    }
                    None => self.top(th, which),
                }
            }
            Sub::Session(SessionCmd::Detach { session, delete }) => {
                match session.and_then(|s| self.resolve_target(&s)) {
                    Some(a) => {
                        let r = self.detach(&a.pane, delete);
                        self.top(th, &r);
                    }
                    None => self.top(th, which),
                }
            }
            Sub::Session(SessionCmd::Screen { session }) => match self.resolve_target(&session) {
                Some(a) => self.screen(&a.pane, th),
                None => self.top(th, which),
            },
            Sub::Clear => {
                if let Some(note) = self.clear_dm() {
                    // The command's own thread is gone after a wipe: post at top level.
                    self.top(None, &note);
                }
            }
            Sub::Daemon(DaemonCmd::Status) => {
                let s = self.status_text();
                self.top(th, &format!("```\n{}\n```", render::esc(&s)));
            }
            Sub::Daemon(DaemonCmd::Restart) => {
                self.top(th, "♻️ _Restarting the valet cc daemon…_");
                let _ = super::save_state(&self.st);
                log!("restart requested from Slack");
                std::process::exit(0); // `valet daemon` starts it again
            }
            Sub::Daemon(DaemonCmd::Logs { lines }) => {
                let body = match super::journal_tail(lines.clamp(1, 200)) {
                    Ok(l) if l.is_empty() => "(no log lines)".to_owned(),
                    Ok(l) => l.join("\n"),
                    Err(e) => format!("journalctl failed: {e}"),
                };
                self.top(th, &format!("```\n{}\n```", render::esc(&body)));
            }
            Sub::Daemon(DaemonCmd::Stop) | Sub::Serve => self.top(
                th,
                "Only from a terminal (`valet cc daemon stop`): stopping from here would cut this chat off.",
            ),
        }
    }

    /// Inside a session thread, a message that is exactly one of these words
    /// acts on the session; anything else goes to Claude.
    fn thread_command(&mut self, text: &str, pane: &str, root: &str) -> bool {
        let t = text.trim().to_lowercase();
        let words: Vec<&str> = t.split_whitespace().collect();
        let keys: Vec<&str> = match words.as_slice() {
            ["esc"] => vec!["esc"],
            ["enter"] => vec!["enter"],
            ["up"] => vec!["up"],
            ["down"] => vec!["down"],
            ["tab"] => vec!["tab"],
            ["space"] => vec!["space"],
            ["ctrlc"] => vec!["ctrl+c"],
            ["keys", rest @ ..] if !rest.is_empty() => rest.to_vec(),
            ["screen"] => {
                self.screen(pane, Some(root));
                return true;
            }
            ["detach"] => {
                self.detach(pane, false);
                return true;
            }
            ["help"] => {
                self.top(Some(root), THREAD_HELP);
                return true;
            }
            _ => return false,
        };
        let r = self
            .agent(pane)
            .ok_or_else(|| anyhow::anyhow!("session is gone"))
            .and_then(|a| self.cx.io.keys(&a.term, &keys));
        match r {
            Ok(()) => {
                if let Some(s) = self.st.sessions.get_mut(pane) {
                    s.last_dialog.clear();
                }
            }
            Err(e) => self.top(Some(root), &format!("keys failed: {e}")),
        }
        true
    }

    fn list_sessions(&mut self, th: Option<&str>) {
        self.st.listing = self.agents.iter().map(|a| a.pane.clone()).collect();
        let mut out = String::from("*Running Claude sessions*\n");
        for (i, a) in self.agents.iter().enumerate() {
            let att = if self.st.sessions.contains_key(&a.pane) {
                "  ✅"
            } else {
                ""
            };
            out.push_str(&format!(
                "`{}`  *{}*{att}\n        {} · {} · `{}`\n",
                i + 1,
                render::esc(&a.display()),
                a.status,
                a.route(),
                render::esc(&render::tilde(&a.cwd))
            ));
        }
        if self.agents.is_empty() {
            out.push_str("_none_\n");
        }
        out.push_str("_✅ mirrored  ·  `session attach N`  ·  `afk on` mirrors all_");
        self.top(th, &out);
    }

    fn screen(&mut self, pane: &str, th: Option<&str>) {
        let scr = match self.agent(pane) {
            Some(a) => self.cx.io.screen(&a.term).unwrap_or_else(|e| e.to_string()),
            None => "session is gone".into(),
        };
        let lines: Vec<&str> = scr.lines().map(|l| l.trim_end()).collect();
        let tail = lines[lines.len().saturating_sub(40)..].join("\n");
        self.top(
            th,
            &format!(
                "```\n{}\n```",
                render::esc(&render::tilde(tail.trim_matches('\n')))
            ),
        );
    }

    /// Top level is the `valet cc` shell; a session thread talks to Claude.
    fn on_user_msg(&mut self, m: &Value, root: Option<&str>) {
        let ts = m["ts"].as_str().unwrap_or("").to_string();
        let text = render::unslack(m["text"].as_str().unwrap_or(""))
            .trim()
            .to_string();
        if text.is_empty() {
            return;
        }
        let Some(r) = root else {
            log!("slack command ts={ts}: {}", render::trunc(&text, 80));
            self.shell(&text, &ts);
            return;
        };
        let Some(p) = self
            .st
            .sessions
            .iter()
            .find(|(_, s)| s.root_ts == r)
            .map(|(p, _)| p.clone())
        else {
            return;
        };
        log!(
            "slack msg ts={ts} session={p}: {}",
            render::trunc(&text, 80)
        );
        if !self.thread_command(&text, &p, r) {
            self.deliver(&p, &text, &ts);
        }
    }

    fn is_operator(&self, m: &Value) -> bool {
        m["user"].as_str() == Some(self.cx.user_id.as_str())
            && m["bot_id"].is_null()
            && matches!(m["subtype"].as_str(), None | Some("file_share"))
    }

    fn poll_slack(&mut self) -> Result<()> {
        let ch = self.cx.channel.clone();
        let h = self.cx.slack.call(
            "conversations.history",
            &[("channel", &ch), ("limit", "100")],
        )?;
        let msgs = h["messages"].as_array().cloned().unwrap_or_default();
        let newest = msgs
            .first()
            .and_then(|m| m["ts"].as_str())
            .unwrap_or("0")
            .to_string();
        if self.st.hist_seen.is_empty() {
            self.st.hist_seen = newest;
            return Ok(());
        }
        let mut fresh = Vec::new();
        let mut fetch = Vec::new();
        let mut seen_roots = HashSet::new();
        for m in &msgs {
            let ts = m["ts"].as_str().unwrap_or("");
            let thread_ts = m["thread_ts"].as_str().unwrap_or(ts);
            if thread_ts != ts {
                continue; // broadcast reply; handled via the thread
            }
            if ts_gt(ts, &self.st.hist_seen) && self.is_operator(m) {
                fresh.push(m.clone());
            }
            if let Some((p, s)) = self.st.sessions.iter().find(|(_, s)| s.root_ts == ts) {
                seen_roots.insert(p.clone());
                if let Some(lr) = m["latest_reply"].as_str()
                    && ts_gt(lr, &s.thread_seen)
                {
                    fetch.push(p.clone());
                }
            }
        }
        if ts_gt(&newest, &self.st.hist_seen) {
            self.st.hist_seen = newest;
        }
        // history's latest_reply can lag by several seconds; poll the active thread directly
        if let Some(p) = self.st.active.clone()
            && self.st.sessions.contains_key(&p)
            && !fetch.contains(&p)
        {
            fetch.push(p);
        }
        if self.tick.is_multiple_of(15) {
            for p in self.st.sessions.keys() {
                if !seen_roots.contains(p) && !fetch.contains(p) {
                    fetch.push(p.clone());
                }
            }
        }
        for m in fresh.iter().rev() {
            self.on_user_msg(m, None);
        }
        for p in fetch {
            let Some(s) = self.st.sessions.get(&p) else {
                continue;
            };
            let (root, seen) = (s.root_ts.clone(), s.thread_seen.clone());
            let reps = match self.cx.slack.replies(&ch, &root, Some(&seen)) {
                Ok(r) => r,
                Err(e) => {
                    warn!("replies {root}: {e}");
                    continue;
                }
            };
            let mut max = seen.clone();
            let mut mine = Vec::new();
            for r in reps {
                let ts = r["ts"].as_str().unwrap_or("").to_string();
                if !ts_gt(&ts, &seen) {
                    continue;
                }
                if ts_gt(&ts, &max) {
                    max = ts.clone();
                }
                if self.is_operator(&r) {
                    mine.push(r);
                }
            }
            if let Some(s) = self.st.sessions.get_mut(&p) {
                s.thread_seen = max;
            }
            for m in mine {
                self.on_user_msg(&m, Some(&root));
            }
        }
        Ok(())
    }

    fn process_deletes(&mut self) {
        let ch = self.cx.channel.clone();
        if self.st.delete_msgs.is_empty()
            && let Some(root) = self.st.delete_roots.pop()
        {
            match self.cx.slack.replies(&ch, &root, None) {
                Ok(msgs) => {
                    let mut ts: Vec<String> = msgs
                        .iter()
                        .filter(|m| self.deletable(m))
                        .filter_map(|m| m["ts"].as_str().map(str::to_string))
                        .collect();
                    // replies first, root last
                    ts.sort_by_key(|t| std::cmp::Reverse(slack::ts_key(t)));
                    self.st.delete_msgs.extend(ts);
                }
                Err(e) if e.to_string().contains("thread_not_found") => {}
                Err(e) => {
                    warn!("list thread {root}: {e}");
                    self.st.delete_roots.insert(0, root);
                }
            }
        }
        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline {
            let Some(ts) = self.st.delete_msgs.first().cloned() else {
                break;
            };
            if let Err(e) = self.cx.slack.delete(&ch, &ts) {
                warn!("delete {ts}: {e}");
            }
            self.st.delete_msgs.remove(0);
        }
        if self.st.delete_msgs.is_empty() && self.st.delete_roots.is_empty() {
            log!("delete queue drained");
        }
    }

    fn deletable(&self, m: &Value) -> bool {
        m["user"].as_str() == Some(self.cx.bot_user.as_str()) || !m["bot_id"].is_null()
    }

    fn process_ctl(&mut self) {
        let Ok(rd) = std::fs::read_dir(super::ctl_dir()) else {
            return;
        };
        let mut reqs: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "req"))
            .collect();
        reqs.sort();
        for p in reqs {
            let req: Value = std::fs::read_to_string(&p)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
            let cmd = req["cmd"].as_str().unwrap_or("");
            let pane = req["pane"].as_str().unwrap_or("").to_string();
            let res = match cmd {
                "afk-on" => {
                    let v = req["verbose"]
                        .as_u64()
                        .map(|v| v.min(super::MAX_VERBOSE as u64) as u8);
                    self.set_afk(true, true, v)
                }
                "afk-off" => self.set_afk(false, true, None),
                "attach" => match self.resolve_target(&pane) {
                    Some(a) => self
                        .attach(&a, false)
                        .unwrap_or_else(|e| format!("error: {e}")),
                    None => format!("no running Claude session matches {pane:?} (`session ls`)"),
                },
                "detach" => {
                    let key = self
                        .resolve_target(&pane)
                        .map(|a| a.pane)
                        .unwrap_or(pane.clone());
                    self.detach(&key, req["delete"].as_bool() == Some(true))
                }
                "cleanup" => self.cleanup(),
                "status" => self.status_text(),
                _ => format!("unknown request {cmd}"),
            };
            log!("ctl {cmd} {pane}: {res}");
            let _ = std::fs::write(p.with_extension("done"), &res);
            let _ = std::fs::remove_file(&p);
        }
    }

    pub(super) fn run(&mut self) -> ! {
        let mut last_saved = String::new();
        let mut last_poll = Instant::now() - Duration::from_secs(60);
        loop {
            self.tick += 1;
            let fresh = self.cx.io.discover();
            if let Ok(a) = &fresh {
                self.agents = a.clone();
            }
            self.process_ctl();
            match fresh {
                Ok(_) => {
                    if self.st.afk {
                        let new: Vec<Live> = self
                            .agents
                            .iter()
                            .filter(|a| !self.st.sessions.contains_key(&a.pane))
                            .cloned()
                            .collect();
                        for a in new {
                            if let Err(e) = self.attach(&a, true) {
                                warn!("auto-attach {}: {e}", a.pane);
                            }
                        }
                    }
                    let panes: Vec<String> = self.st.sessions.keys().cloned().collect();
                    for p in panes {
                        let Some(mut s) = self.st.sessions.remove(&p) else {
                            continue;
                        };
                        match self.tick_session(&mut s) {
                            Outcome::Keep => {
                                self.st.sessions.insert(p, s);
                            }
                            Outcome::Ended => {
                                if s.auto {
                                    self.st.ended_roots.push(s.root_ts);
                                }
                                if self.st.active.as_deref() == Some(p.as_str()) {
                                    self.st.active = self.st.sessions.keys().next().cloned();
                                }
                            }
                        }
                    }
                }
                Err(e) => warn!("session discovery: {e}"),
            }
            let busy = self.st.afk || !self.st.sessions.is_empty();
            let every = Duration::from_millis(if busy { POLL_MS } else { IDLE_POLL_MS });
            if last_poll.elapsed() >= every {
                last_poll = Instant::now();
                if let Err(e) = self.poll_slack() {
                    warn!("slack poll: {e}");
                }
            }
            if !self.st.delete_roots.is_empty() || !self.st.delete_msgs.is_empty() {
                self.process_deletes();
            }
            if let Ok(s) = serde_json::to_string(&self.st)
                && s != last_saved
            {
                if let Err(e) = super::save_state(&self.st) {
                    warn!("save state: {e}");
                }
                last_saved = s;
            }
            std::thread::sleep(Duration::from_millis(700));
        }
    }
}
