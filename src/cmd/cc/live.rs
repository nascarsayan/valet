//! Running Claude Code sessions, from Claude's own registry
//! (`~/.claude/sessions/<pid>.json`), and how to type into each one.
//!
//! A registry entry is live when its pid exists and `/proc/<pid>/stat`'s start
//! time equals the recorded `procStart` (guards against pid reuse). Input goes
//! through the terminal hosting the session: a herdr pane (matched by session
//! id) or a tmux pane (matched by tty). Anything else is mirrored read-only.

use super::herdr::{self, Herdr};
use crate::proc::Cmd;
use anyhow::{Result, bail};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

const TMUX_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq)]
pub enum Term {
    Herdr(String),
    Tmux(String),
    None,
}

#[derive(Clone, Debug)]
pub struct Live {
    /// Stable key: the Claude process id.
    pub pane: String,
    pub session_id: String,
    pub name: String,
    pub cwd: String,
    /// idle | working | blocked | done
    pub status: String,
    pub tty: String,
    pub term: Term,
}

impl Live {
    pub fn display(&self) -> String {
        if !self.name.is_empty() {
            return self.name.clone();
        }
        let base = self.cwd.rsplit('/').next().unwrap_or("");
        let sid: String = self.session_id.chars().take(8).collect();
        format!("{base} ({sid})")
    }

    /// Where input goes, for humans.
    pub fn route(&self) -> String {
        match &self.term {
            Term::Herdr(p) => format!("herdr {p}"),
            Term::Tmux(p) => format!("tmux {p}"),
            Term::None => format!("read-only ({})", self.tty.trim_start_matches("/dev/")),
        }
    }
}

/// Registry status -> the lifecycle words the daemon uses.
fn norm_status(s: &str) -> &str {
    match s {
        "busy" | "running" | "working" => "working",
        "waiting" | "blocked" => "blocked",
        "" => "unknown",
        s => s,
    }
}

/// Start time (field 22) from `/proc/<pid>/stat`; the comm field may contain spaces.
pub fn proc_start(stat: &str) -> Option<&str> {
    stat.rsplit_once(')')?.1.split_whitespace().nth(19)
}

fn alive(pid: u64, proc_start_rec: &str) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    proc_start_rec.is_empty() || proc_start(&stat) == Some(proc_start_rec)
}

/// Parse one registry entry; None for non-interactive or malformed entries.
pub fn parse_entry(v: &Value) -> Option<(u64, Live, String)> {
    if v["kind"].as_str() != Some("interactive") {
        return None;
    }
    let pid = v["pid"].as_u64()?;
    let s = |k: &str| v[k].as_str().unwrap_or_default().to_owned();
    let live = Live {
        pane: pid.to_string(),
        session_id: s("sessionId"),
        name: s("name"),
        cwd: s("cwd"),
        status: norm_status(v["status"].as_str().unwrap_or("")).to_owned(),
        tty: String::new(),
        term: Term::None,
    };
    Some((pid, live, s("procStart")))
}

pub struct Io {
    herdr: Option<Herdr>,
    tmux: Option<PathBuf>,
}

impl Io {
    pub fn new() -> Self {
        Io {
            herdr: herdr::resolve().ok().map(|bin| Herdr { bin }),
            tmux: crate::proc::which("tmux"),
        }
    }

    /// Live interactive sessions, oldest first.
    pub fn discover(&self) -> Result<Vec<Live>> {
        let dir = crate::cmd::claude_dir()?.join("sessions");
        let mut out: Vec<(u64, Live)> = Vec::new();
        for e in std::fs::read_dir(&dir)?.flatten() {
            let p = e.path();
            if p.extension().is_none_or(|x| x != "json") {
                continue;
            }
            let Some(v) = std::fs::read_to_string(&p)
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            else {
                continue;
            };
            let Some((pid, mut l, start)) = parse_entry(&v) else {
                continue;
            };
            if !alive(pid, &start) {
                continue;
            }
            l.tty = std::fs::read_link(format!("/proc/{pid}/fd/0"))
                .map(|t| t.to_string_lossy().into_owned())
                .unwrap_or_default();
            out.push((pid, l));
        }
        out.sort_by_key(|(pid, _)| *pid);
        let herdr_by_session: HashMap<String, (String, String)> = match &self.herdr {
            Some(h) => h
                .agents()
                .unwrap_or_default()
                .into_iter()
                .filter(|a| !a.session_id.is_empty())
                .map(|a| (a.session_id, (a.pane, a.status)))
                .collect(),
            None => HashMap::new(),
        };
        let tmux_by_tty = self.tmux_panes();
        Ok(out
            .into_iter()
            .map(|(_, mut l)| {
                if let Some((pane, status)) = herdr_by_session.get(&l.session_id) {
                    l.term = Term::Herdr(pane.clone());
                    // herdr reads the screen, so it also sees dialogs.
                    l.status = status.clone();
                } else if let Some(p) = tmux_by_tty.get(&l.tty) {
                    l.term = Term::Tmux(p.clone());
                }
                l
            })
            .collect())
    }

    fn tmux_panes(&self) -> HashMap<String, String> {
        let Some(t) = &self.tmux else {
            return HashMap::new();
        };
        let Ok(o) = Cmd::new(t)
            .args(["list-panes", "-a", "-F", "#{pane_tty} #{pane_id}"])
            .timeout(TMUX_TIMEOUT)
            .output()
        else {
            return HashMap::new();
        };
        if !o.status.success() {
            return HashMap::new(); // no server
        }
        o.stdout
            .lines()
            .filter_map(|l| l.split_once(' '))
            .map(|(tty, id)| (tty.to_owned(), id.to_owned()))
            .collect()
    }

    fn tmux(&self, args: &[&str]) -> Result<String> {
        let Some(t) = &self.tmux else {
            bail!("tmux not found")
        };
        Cmd::new(t).args(args).timeout(TMUX_TIMEOUT).checked()
    }

    fn herdr(&self) -> Result<&Herdr> {
        self.herdr
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("herdr not found"))
    }

    pub fn screen(&self, t: &Term) -> Result<String> {
        match t {
            Term::Herdr(p) => self.herdr()?.screen(p),
            Term::Tmux(p) => self.tmux(&["capture-pane", "-p", "-t", p]),
            Term::None => bail!("read-only session: not in a herdr or tmux pane"),
        }
    }

    /// Logical keys (herdr names: esc, enter, up, ctrl+c, digits…).
    pub fn keys(&self, t: &Term, keys: &[&str]) -> Result<()> {
        match t {
            Term::Herdr(p) => self.herdr()?.keys(p, keys),
            Term::Tmux(p) => {
                let mut args = vec!["send-keys".to_owned(), "-t".to_owned(), p.clone()];
                args.extend(keys.iter().map(|k| tmux_key(k)));
                let refs: Vec<&str> = args.iter().map(String::as_str).collect();
                self.tmux(&refs).map(|_| ())
            }
            Term::None => bail!("read-only session: not in a herdr or tmux pane"),
        }
    }

    pub fn text(&self, t: &Term, text: &str) -> Result<()> {
        match t {
            Term::Herdr(p) => self.herdr()?.text(p, text),
            Term::Tmux(p) => self
                .tmux(&["send-keys", "-t", p, "-l", "--", text])
                .map(|_| ()),
            Term::None => bail!("read-only session: not in a herdr or tmux pane"),
        }
    }

    /// Submit a prompt. herdr: `agent prompt` (bracketed paste, in a thread);
    /// tmux: bracketed paste of a buffer, then Enter.
    pub fn prompt(&self, t: &Term, text: &str) -> Result<()> {
        match t {
            Term::Herdr(p) => {
                self.herdr()?.prompt_async(p, text);
                Ok(())
            }
            Term::Tmux(p) => {
                self.tmux(&["set-buffer", "-b", "valet-cc", "--", text])?;
                self.tmux(&["paste-buffer", "-p", "-d", "-b", "valet-cc", "-t", p])?;
                std::thread::sleep(Duration::from_millis(200));
                self.tmux(&["send-keys", "-t", p, "Enter"]).map(|_| ())
            }
            Term::None => bail!("read-only session: not in a herdr or tmux pane"),
        }
    }
}

fn tmux_key(k: &str) -> String {
    match k {
        "esc" | "escape" => "Escape".into(),
        "enter" => "Enter".into(),
        "up" => "Up".into(),
        "down" => "Down".into(),
        "left" => "Left".into(),
        "right" => "Right".into(),
        "tab" => "Tab".into(),
        "space" => "Space".into(),
        k if k.starts_with("ctrl+") => format!("C-{}", &k[5..]),
        k => k.to_owned(),
    }
}

/// The Claude session this process runs inside (walks the parent chain to a
/// pid that has a registry entry), e.g. for `!valet cc attach` from Claude.
pub fn enclosing_pid() -> Option<u64> {
    let dir = crate::cmd::claude_dir().ok()?.join("sessions");
    let mut pid = std::process::id() as u64;
    for _ in 0..32 {
        if dir.join(format!("{pid}.json")).exists() {
            return Some(pid);
        }
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        pid = stat
            .rsplit_once(')')?
            .1
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()?;
        if pid <= 1 {
            return None;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_start_skips_comm_with_spaces() {
        let stat = "2568743 (claude code) S 2568700 2568743 2568700 34825 2568743 4194560 1 2 3 4 5 6 7 8 20 0 12 0 316173865 1234 56";
        assert_eq!(proc_start(stat), Some("316173865"));
    }

    #[test]
    fn parses_interactive_entries_only() {
        let v: Value = serde_json::from_str(
            r#"{"pid":42,"sessionId":"2b46d7ec-27","cwd":"/x/ws","kind":"interactive","name":"ws-65","status":"waiting","procStart":"9"}"#,
        )
        .unwrap();
        let (pid, l, start) = parse_entry(&v).unwrap();
        assert_eq!(
            (pid, start.as_str(), l.status.as_str(), l.pane.as_str()),
            (42, "9", "blocked", "42")
        );
        let bg: Value = serde_json::from_str(r#"{"pid":43,"kind":"bg"}"#).unwrap();
        assert!(parse_entry(&bg).is_none());
    }

    #[test]
    fn tmux_key_names() {
        assert_eq!(tmux_key("esc"), "Escape");
        assert_eq!(tmux_key("ctrl+c"), "C-c");
        assert_eq!(tmux_key("2"), "2");
    }
}
