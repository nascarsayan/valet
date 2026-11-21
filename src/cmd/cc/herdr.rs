//! The `herdr` CLI as an input backend: map Claude sessions to panes, read
//! screens, send keys and prompts.

use crate::proc::Cmd;
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(20);
/// Overrides where `herdr` is found (the systemd unit sets it).
pub const HERDR_ENV: &str = "VALET_CC_HERDR";

#[derive(Clone, Debug, Default)]
pub struct Agent {
    pub pane: String,
    pub status: String,
    pub session_id: String,
}

pub fn resolve() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os(HERDR_ENV).filter(|p| !p.is_empty()) {
        return Ok(p.into());
    }
    if let Some(p) = crate::proc::which("herdr") {
        return Ok(p);
    }
    let local = crate::cmd::home()?.join(".local/bin/herdr");
    local
        .exists()
        .then_some(local)
        .context("herdr not found (PATH, ~/.local/bin, or VALET_CC_HERDR)")
}

pub struct Herdr {
    pub bin: PathBuf,
}

fn run(bin: &PathBuf, args: &[&str]) -> Result<String> {
    let out = Cmd::new(bin).args(args).timeout(TIMEOUT).output()?;
    if !out.status.success() {
        bail!(
            "herdr {}: {}",
            args.iter().take(2).copied().collect::<Vec<_>>().join(" "),
            out.stderr.trim()
        );
    }
    Ok(out.stdout)
}

/// Claude agents from `herdr agent list` JSON.
pub fn parse_agents(json: &str) -> Result<Vec<Agent>> {
    let v: Value = serde_json::from_str(json).context("herdr agent list: not JSON")?;
    let s = |a: &Value, k: &str| a[k].as_str().unwrap_or_default().to_owned();
    Ok(v["result"]["agents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|a| a["agent"].as_str() == Some("claude"))
        .map(|a| Agent {
            pane: s(a, "pane_id"),
            status: s(a, "agent_status"),
            session_id: a["agent_session"]["value"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        })
        .collect())
}

impl Herdr {
    pub fn agents(&self) -> Result<Vec<Agent>> {
        parse_agents(&run(&self.bin, &["agent", "list"])?)
    }

    pub fn screen(&self, pane: &str) -> Result<String> {
        run(
            &self.bin,
            &["pane", "read", pane, "--source", "visible", "--lines", "80"],
        )
    }

    pub fn keys(&self, pane: &str, keys: &[&str]) -> Result<()> {
        let mut args = vec!["pane", "send-keys", pane];
        args.extend_from_slice(keys);
        run(&self.bin, &args).map(|_| ())
    }

    pub fn text(&self, pane: &str, text: &str) -> Result<()> {
        run(&self.bin, &["pane", "send-text", pane, text]).map(|_| ())
    }

    /// Submit a prompt (honours bracketed paste). In a thread: herdr can wait
    /// several seconds for the agent's lifecycle change.
    pub fn prompt_async(&self, pane: &str, text: &str) {
        let (bin, pane, text) = (self.bin.clone(), pane.to_owned(), text.to_owned());
        std::thread::spawn(move || {
            if let Err(e) = run(&bin, &["agent", "prompt", &pane, &text]) {
                let msg = e.to_string();
                // "stalled" = no lifecycle change within 5s (e.g. queued while working);
                // the text was still delivered.
                if !msg.contains("agent_prompt_stalled") {
                    super::warn(&format!("herdr prompt to {pane} failed: {msg}"));
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_claude_agents_only() {
        let j = r#"{"result":{"agents":[
            {"agent":"claude","pane_id":"w1:p2","agent_status":"idle","cwd":"/x/ws",
             "agent_session":{"value":"76911f3c-c227"},"terminal_title_stripped":"✳ gpu-jam"},
            {"agent":"codex","pane_id":"w1:p3","agent_status":"idle"},
            {"agent":"claude","pane_id":"w1:p4","agent_status":"blocked","cwd":"/x/ws",
             "agent_session":{"value":"2b46d7ec-aaaa"},"terminal_title_stripped":"Claude Code"}]}}"#;
        let a = parse_agents(j).unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].session_id, "76911f3c-c227");
        assert_eq!(a[1].pane, "w1:p4");
        assert_eq!(a[1].status, "blocked");
    }
}

/// Claude's terminal title before the session has a name.
const DEFAULT_TITLE: &str = "Claude Code";
/// How often the `tabs` part asks herdr for panes and tabs (herdr has no
/// title-change event).
const TABS_EVERY: Duration = Duration::from_secs(2);
/// Host-name words that say nothing about which machine it is.
const HOST_NOISE: &[&str] = &[
    "agent", "cloud", "dev", "host", "node", "prod", "server", "srv",
];
/// Longest host part of a tab label (room for a `-N` suffix in 15).
const HOST_MAX: usize = 12;

/// One herdr tab: id, workspace, 1-based position (herdr's default label),
/// current label, and the agent kind and terminal title of its first pane.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TabView {
    pub tab: String,
    pub ws: String,
    pub pos: usize,
    pub label: String,
    pub agent: String,
    pub title: String,
}

/// The short host a shell put in its terminal title: a bare host name (the
/// valet shell rc) or `user@host:dir` (common distro default).
fn title_host(title: &str) -> Option<&str> {
    let host_ok = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
    };
    let host = match title.split_once('@') {
        Some((user, rest)) => {
            let (host, _) = rest.split_once(':')?;
            let user_ok = !user.is_empty()
                && user
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b));
            if !user_ok {
                return None;
            }
            host
        }
        None => title,
    };
    host_ok(host).then(|| host.split('.').next().unwrap_or(host))
}

/// A few telling characters of HOST: generic words dropped
/// (`build-agent-web3` -> `build-web3`).
pub fn host_label(host: &str) -> String {
    let out = host
        .split('-')
        .filter(|w| !w.is_empty() && !HOST_NOISE.contains(&w.to_ascii_lowercase().as_str()))
        .collect::<Vec<_>>()
        .join("-");
    let out: String = if out.is_empty() { host.to_owned() } else { out };
    out.chars()
        .take(HOST_MAX)
        .collect::<String>()
        .trim_end_matches('-')
        .to_owned()
}

/// The label each tab should have: a named Claude session's title in
/// PascalCase; a shell's host, local or remote (same host in one workspace:
/// `-1`, `-2`, ... by position); otherwise the tab's position. ME is this host.
pub fn desired_labels(tabs: &[TabView], me: &str) -> Vec<(String, String)> {
    let base = |t: &TabView| -> (bool, String) {
        let title = t.title.trim();
        // Until Claude sets its title the pane still shows the shell's (this
        // host, or `user@host:dir`), which is no session name.
        let shell_title = title.eq_ignore_ascii_case(me) || title.contains('@');
        if t.agent == "claude" && !title.is_empty() && title != DEFAULT_TITLE && !shell_title {
            let l = crate::cmd::githooks::session_name::pascal(
                &crate::cmd::githooks::session_name::kebab(title),
            );
            if !l.is_empty() {
                return (false, l);
            }
        }
        match title_host(title) {
            Some(h) => (true, host_label(h)),
            None => (false, t.pos.to_string()),
        }
    };
    let bases: Vec<(bool, String)> = tabs.iter().map(base).collect();
    tabs.iter()
        .zip(&bases)
        .map(|(t, (host, b))| {
            if !host {
                return (t.tab.clone(), b.clone());
            }
            let same: Vec<&TabView> = tabs
                .iter()
                .zip(&bases)
                .filter(|(o, (h, ob))| *h && ob == b && o.ws == t.ws)
                .map(|(o, _)| o)
                .collect();
            if same.len() < 2 {
                return (t.tab.clone(), b.clone());
            }
            let n = same.iter().position(|o| o.tab == t.tab).unwrap_or(0) + 1;
            (t.tab.clone(), format!("{b}-{n}"))
        })
        .collect()
}

/// Renames for TABS. A tab is ours while its label is herdr's stock number
/// or the label we set last (OURS: tab -> that label); any other label was
/// set by the user or Claude and stays for the tab's life. Returns whether
/// OURS changed.
pub fn tab_renames(
    tabs: &[TabView],
    me: &str,
    ours: &mut std::collections::BTreeMap<String, String>,
) -> (Vec<(String, String)>, bool) {
    let before = ours.clone();
    ours.retain(|k, _| tabs.iter().any(|t| t.tab == *k));
    let mut out = Vec::new();
    for ((tab, want), t) in desired_labels(tabs, me).into_iter().zip(tabs) {
        let stock = !t.label.is_empty() && t.label.bytes().all(|b| b.is_ascii_digit());
        if !stock && ours.get(&tab) != Some(&t.label) {
            ours.remove(&tab);
            continue;
        }
        if t.label != want {
            out.push((tab.clone(), want.clone()));
        }
        ours.insert(tab, want);
    }
    let changed = *ours != before;
    (out, changed)
}

/// Labels the `tabs` part set (`<Valet>/State/tabs/<host>.json`), so a
/// daemon restart still tells its own labels from custom ones.
fn ours_file() -> Result<PathBuf> {
    let root = crate::cmd::apps::root()?;
    Ok(crate::cmd::apps::self_app(&root)
        .join("State/tabs")
        .join(format!("{}.json", crate::cmd::apps::host())))
}

fn save_ours(file: &Path, ours: &std::collections::BTreeMap<String, String>) -> Result<()> {
    if let Some(d) = file.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = file.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string(ours)?)?;
    std::fs::rename(&tmp, file)?;
    Ok(())
}

impl Herdr {
    /// Every tab with its first pane's agent and terminal title.
    pub fn tabs(&self) -> Result<Vec<TabView>> {
        let panes: Value = serde_json::from_str(&run(&self.bin, &["pane", "list"])?)
            .context("herdr pane list: not JSON")?;
        let wss: Value = serde_json::from_str(&run(&self.bin, &["workspace", "list"])?)
            .context("herdr workspace list: not JSON")?;
        let s = |v: &Value, k: &str| v[k].as_str().unwrap_or_default().to_owned();
        let mut out = Vec::new();
        for w in wss["result"]["workspaces"].as_array().into_iter().flatten() {
            let ws = s(w, "workspace_id");
            let tl: Value =
                serde_json::from_str(&run(&self.bin, &["tab", "list", "--workspace", &ws])?)
                    .context("herdr tab list: not JSON")?;
            for (i, t) in tl["result"]["tabs"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
            {
                let id = s(t, "tab_id");
                let pane = panes["result"]["panes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|p| p["tab_id"].as_str() == Some(id.as_str()));
                out.push(TabView {
                    pos: i + 1,
                    label: s(t, "label"),
                    agent: pane.map(|p| s(p, "agent")).unwrap_or_default(),
                    title: pane
                        .map(|p| s(p, "terminal_title_stripped"))
                        .unwrap_or_default(),
                    ws: ws.clone(),
                    tab: id,
                });
            }
        }
        Ok(out)
    }
}

/// `valet daemon part tabs`: keep herdr tab labels on what each tab shows
/// ([`desired_labels`]). Pane titles come from herdr, so each tab follows
/// its own pane (hook environments can name another tab).
pub fn tabs_part() -> Result<()> {
    let h = Herdr { bin: resolve()? };
    let me = crate::cmd::apps::host();
    let file = ours_file()?;
    let mut ours: std::collections::BTreeMap<String, String> = std::fs::read_to_string(&file)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    // No file yet (first run): labels already equal to ours are ours.
    let mut seed = !file.exists();
    let mut failing = false;
    loop {
        match h.tabs() {
            Ok(tabs) => {
                failing = false;
                let seeded = std::mem::take(&mut seed);
                if seeded {
                    for ((tab, want), t) in desired_labels(&tabs, &me).into_iter().zip(&tabs) {
                        if t.label == want {
                            ours.insert(tab, want);
                        }
                    }
                }
                let (renames, changed) = tab_renames(&tabs, &me, &mut ours);
                for (tab, label) in renames {
                    match run(&h.bin, &["tab", "rename", &tab, &label]) {
                        Ok(_) => println!("{tab} -> {label}"),
                        Err(e) => eprintln!("rename {tab}: {e:#}"),
                    }
                }
                if (changed || seeded)
                    && let Err(e) = save_ours(&file, &ours)
                {
                    eprintln!("save {}: {e:#}", file.display());
                }
            }
            // herdr down: say so once, keep trying.
            Err(e) if !failing => {
                eprintln!("herdr: {e:#}");
                failing = true;
            }
            Err(_) => {}
        }
        std::thread::sleep(TABS_EVERY);
    }
}

#[cfg(test)]
mod tab_tests {
    use super::*;

    fn tab(id: &str, pos: usize, agent: &str, title: &str) -> TabView {
        TabView {
            tab: id.into(),
            ws: id.split(':').next().unwrap().into(),
            pos,
            label: String::new(),
            agent: agent.into(),
            title: title.into(),
        }
    }

    #[test]
    fn labels_follow_what_each_tab_shows() {
        let tabs = vec![
            tab("w8:t1", 1, "claude", "login-page-scale"),
            tab("w8:t3", 2, "", "alice@devbox:~/ws"),
            tab("w8:t4", 3, "", "alice@db-node-west2:~"),
            tab("w8:t5", 4, "claude", "Claude Code"),
            tab("w8:t6", 5, "claude", "alice@devbox:/tmp/x"),
            tab("w8:t7", 6, "claude", "devbox"),
            tab("w8:t8", 7, "", "web-frontend-eu1"),
            tab("w9:t1", 1, "", "alice@build-agent-web3:~"),
            tab("w9:t2", 2, "", "alice@build-agent-web3.example.com:/x"),
        ];
        let got: Vec<String> = desired_labels(&tabs, "devbox")
            .into_iter()
            .map(|(_, l)| l)
            .collect();
        assert_eq!(
            got,
            [
                "LoginPageScale",
                "devbox-1",
                "db-west2",
                "4",
                "devbox-2",
                "devbox-3",
                "web-frontend",
                "build-web3-1",
                "build-web3-2"
            ]
        );
    }

    #[test]
    fn host_labels_are_short() {
        assert_eq!(host_label("db-node-west2"), "db-west2");
        assert_eq!(host_label("build-agent-web3"), "build-web3");
        assert_eq!(host_label("web-frontend-eu1"), "web-frontend");
        assert_eq!(host_label("devbox"), "devbox");
        assert_eq!(host_label("server"), "server");
        assert_eq!(title_host("a b@c:d"), None);
        assert_eq!(title_host("u@h.x.y:~"), Some("h"));
        assert_eq!(title_host("build-agent-web3"), Some("build-agent-web3"));
        assert_eq!(title_host("Claude Code"), None);
    }

    #[test]
    fn custom_labels_stay_and_ours_follow_the_tab() {
        let mut ours = std::collections::BTreeMap::new();
        let mut tabs = vec![tab("w1:t1", 1, "claude", "api-access")];
        tabs[0].label = "1".into();
        let r = |tabs: &[TabView], o: &mut _| tab_renames(tabs, "me", o).0;
        assert_eq!(r(&tabs, &mut ours), [("w1:t1".into(), "ApiAccess".into())]);
        tabs[0].label = "ApiAccess".into();
        assert!(r(&tabs, &mut ours).is_empty());
        // Claude exits: our label follows, back to the position.
        tabs[0].agent.clear();
        tabs[0].title.clear();
        assert_eq!(r(&tabs, &mut ours), [("w1:t1".into(), "1".into())]);
        // Renamed by the user or Claude: never touched again.
        tabs[0].label = "Mine".into();
        tabs[0].title = "somehost".into();
        assert!(r(&tabs, &mut ours).is_empty());
        assert!(ours.is_empty());
        tabs[0].agent = "claude".into();
        tabs[0].title = "other-topic".into();
        assert!(r(&tabs, &mut ours).is_empty());
        // A tab created with a name is custom from the start.
        let named = vec![{
            let mut t = tab("w1:t2", 2, "", "me");
            t.label = "build".into();
            t
        }];
        assert!(r(&named, &mut ours).is_empty());
    }
}
