//! `valet cc`: mirror running Claude Code sessions to a Slack bot's DM,
//! and drive them from Slack (prompts, AskUserQuestion answers, permission
//! dialogs). `valet cc afk on` mirrors every running session (and new ones);
//! `valet cc afk off` stops and deletes the mirrored threads.
//!
//! Sessions come from Claude Code's own registry (`~/.claude/sessions/<pid>.json`,
//! live = pid running with the recorded start time). Replies are typed into the
//! session's herdr or tmux pane; sessions in any other terminal are read-only.
//!
//! One daemon (the `cc` part of `valet daemon`) owns all state in
//! `$XDG_STATE_HOME/valet/cc/state.json`; CLI commands hand it request files in
//! `ctl/` and wait for the `.done` answer.
//!
//! Bot token: SLACK_BOT_TOKEN; DM: SLACK_CHANNEL; each from the environment,
//! else `valet vars` (`token.rs`), no defaults. The operator is the DM's other
//! member. valet cc only ever deletes the bot's messages, never the operator's.

mod blocks;
mod daemon;
pub(crate) mod herdr;
mod live;
mod render;
mod slack;
mod token;
mod transcript;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use slack::Slack;
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// This daemon's part of `valet daemon`.
const PART: &str = "cc";
/// Highest mirror verbosity; any larger or negative level means this.
pub(super) const MAX_VERBOSE: u8 = 3;
pub(super) const DEFAULT_VERBOSE: u8 = 1;
const REQUEST_WAIT: Duration = Duration::from_secs(90);

#[derive(clap::Args, Debug)]
pub struct Args {
    #[command(subcommand)]
    cmd: Sub,
}

/// `valet slack`: post to the bot's DM, outside the session mirror.
#[derive(clap::Args, Debug)]
pub struct SlackArgs {
    #[command(subcommand)]
    cmd: SlackSub,
}

#[derive(clap::Subcommand, Debug)]
enum SlackSub {
    /// Send markdown to the DM: TEXT, a file (--file), or stdin (TEXT absent or `-`).
    ///
    /// Any CommonMark/GitHub markdown becomes native Block Kit (headers,
    /// dividers, tables, rich text, lists, quotes, highlighted code); code
    /// over 10 lines goes as a collapsible file snippet; long text is split
    /// into several messages.
    /// Prints the first message's ts. Channel: SLACK_CHANNEL.
    Dm {
        text: Option<String>,
        /// Send this markdown file instead of TEXT.
        #[arg(short, long, value_name = "PATH", conflicts_with = "text")]
        file: Option<PathBuf>,
        /// Reply in this thread (a message ts).
        #[arg(long)]
        thread: Option<String>,
        /// Fail with Slack's reason instead of falling back to plain markdown.
        #[arg(long, hide = true)]
        no_fallback: bool,
        /// Post this file's JSON array of Block Kit blocks as-is (debugging).
        #[arg(long, hide = true, value_name = "PATH", conflicts_with_all = ["text", "file"])]
        raw_blocks: Option<PathBuf>,
    },
}

/// Largest stdin `valet slack dm` reads.
const DM_MAX_BYTES: u64 = 1 << 20;

pub fn slack_cmd(a: SlackArgs) -> Result<()> {
    let SlackSub::Dm {
        text,
        file,
        thread,
        no_fallback,
        raw_blocks,
    } = a.cmd;
    if let Some(p) = raw_blocks {
        let blocks: Value = serde_json::from_str(
            &std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?,
        )
        .with_context(|| format!("{} is not JSON", p.display()))?;
        let slack = Slack::new(token::bot_token()?);
        let ts = slack.post_raw_blocks(&token::default_channel()?, thread.as_deref(), &blocks)?;
        println!("{ts}");
        return Ok(());
    }
    let text = match (&file, text.as_deref()) {
        (Some(f), _) => {
            let len = std::fs::metadata(f)
                .with_context(|| format!("read {}", f.display()))?
                .len();
            if len > DM_MAX_BYTES {
                bail!("{} is larger than {DM_MAX_BYTES} bytes", f.display());
            }
            std::fs::read_to_string(f).with_context(|| format!("read {}", f.display()))?
        }
        (None, t) => match t {
            None | Some("-") => {
                use std::io::Read;
                let mut s = String::new();
                std::io::stdin()
                    .take(DM_MAX_BYTES + 1)
                    .read_to_string(&mut s)
                    .context("read stdin")?;
                if s.len() as u64 > DM_MAX_BYTES {
                    bail!("stdin is larger than {DM_MAX_BYTES} bytes");
                }
                s
            }
            Some(t) => t.to_owned(),
        },
    };
    if text.trim().is_empty() {
        bail!("nothing to send");
    }
    let slack = Slack::new(token::bot_token()?);
    let channel = token::default_channel()?;
    println!(
        "{}",
        slack.post_md(&channel, thread.as_deref(), &text, !no_fallback)?
    );
    Ok(())
}

// The command tree, shared by `valet cc` and the Slack DM: Slack parses each
// message with it, so help works at every level there too. First doc lines
// stay short: they are the one-line help on a phone.
#[derive(clap::Subcommand, Debug)]
enum Sub {
    /// Mirror sessions here (on) or stop (off); `on v2` sets verbosity.
    ///
    /// No argument: show the mode. `on` again changes the verbosity:
    /// v0 questions, plans and dialogs only; v1 (default) + Claude's text and
    /// typed prompts; v2 + tool calls and errors; v3 + each tool result's
    /// first line. Higher or negative = the most (v3).
    Afk {
        #[arg(value_parser = ["on", "off"])]
        state: Option<String>,
        /// Verbosity as `v2`, `v=2` or `2`.
        level: Option<String>,
        /// Verbosity (same as the positional level).
        #[arg(short = 'v', long, allow_hyphen_values = true)]
        verbose: Option<String>,
    },
    /// Running Claude sessions: ls, attach, detach, screen.
    #[command(subcommand, visible_alias = "s")]
    Session(SessionCmd),
    /// Wipe this DM (mirrored threads stay).
    ///
    /// Every bot message and thread reply. Your own messages stay.
    Clear,
    /// The background daemon: status, restart, stop.
    #[command(subcommand)]
    Daemon(DaemonCmd),
    /// Run the daemon in the foreground (what the systemd unit runs).
    #[command(hide = true)]
    Serve,
}

#[derive(clap::Subcommand, Debug)]
enum SessionCmd {
    /// List them, with status and where replies go.
    #[command(visible_alias = "list")]
    Ls,
    /// Mirror one to its own thread.
    Attach {
        /// Number from `ls`, name or pid.
        ///
        /// Also a session-id prefix. From a terminal, defaults to the Claude
        /// session the command runs in.
        session: Option<String>,
    },
    /// Stop mirroring one.
    Detach {
        /// Number from `ls`, name or pid.
        ///
        /// Also a session-id prefix. From a terminal, defaults to the Claude
        /// session the command runs in.
        session: Option<String>,
        /// Also delete its thread.
        #[arg(long)]
        delete: bool,
    },
    /// Snapshot of its terminal.
    Screen {
        /// Number from `ls`, name or pid.
        session: String,
    },
}

#[derive(clap::Subcommand, Debug)]
enum DaemonCmd {
    /// State and mirrored sessions.
    Status,
    /// Restart it (e.g. after `valet upgrade`).
    Restart,
    /// Stop it and disable it at login (terminal only).
    Stop,
    /// Its recent log lines.
    ///
    /// Set VALET_CC_DEBUG=1 in the unit (`systemctl --user edit valet-daemon`) for per-event detail.
    Logs {
        /// How many lines.
        #[arg(default_value_t = 30)]
        lines: usize,
    },
}

/// Shown under the Slack help: what is not a command.
const SLACK_AFTER_HELP: &str =
    "In a session thread: text goes to Claude, a number answers its dialog, `help` lists the keys.";

/// The command tree as the Slack shell sees it.
fn slack_tree() -> clap::Command {
    use clap::Subcommand;
    Sub::augment_subcommands(
        clap::Command::new("valet cc")
            .no_binary_name(true)
            .subcommand_required(true)
            .arg_required_else_help(true)
            .disable_version_flag(true)
            .disable_help_subcommand(true),
    )
}

/// First sentence, no trailing period: one short line on a phone.
fn short(s: &str) -> String {
    let s = s.split(". ").next().unwrap_or(s).trim();
    s.lines()
        .next()
        .unwrap_or("")
        .trim_end_matches('.')
        .to_owned()
}

/// Help for one node of the tree: one bullet per line, no alignment, no `--`.
fn slack_help(cmd: &clap::Command, path: &str) -> String {
    let about =
        |c: &clap::Command| short(&c.get_about().map(|a| a.to_string()).unwrap_or_default());
    // The top level is just the command list; deeper levels get a title line.
    let mut out = Vec::new();
    if !path.is_empty() {
        out.push(format!("*{path}* {}", about(cmd)));
    }
    let subs: Vec<&clap::Command> = cmd.get_subcommands().filter(|c| !c.is_hide_set()).collect();
    for c in &subs {
        let name = if path.is_empty() {
            c.get_name().to_owned()
        } else {
            format!("{path} {}", c.get_name())
        };
        out.push(format!("• `{name}` {}", about(c)));
    }
    for a in cmd.get_arguments().filter(|a| {
        let id = a.get_id().as_str();
        !a.is_hide_set() && id != "help"
    }) {
        let vals: Vec<String> = a
            .get_possible_values()
            .iter()
            .map(|v| v.get_name().to_owned())
            .collect();
        let name = match a.get_long() {
            Some(l) => l.to_owned(),
            None if !vals.is_empty() => vals.join("|"),
            None => a.get_id().as_str().to_uppercase(),
        };
        let help = short(&a.get_help().map(|h| h.to_string()).unwrap_or_default());
        out.push(format!("• `{name}` {help}"));
    }
    if path.is_empty() {
        out.push(format!("_{SLACK_AFTER_HELP}_"));
    } else if !subs.is_empty() {
        out.push(format!("_`{path} CMD help` for more_"));
    }
    out.join("\n")
}

/// Slack words -> argv: no dashes needed. `help` anywhere (or `?`) asks for
/// help at that level; a bare word naming a flag of the command (`delete`)
/// is that flag.
fn slack_argv(tree: &clap::Command, text: &str) -> Vec<String> {
    let mut words: Vec<&str> = text.split_whitespace().collect();
    if matches!(words.first(), Some(&"help" | &"?")) {
        words.remove(0);
        words.push("help");
    }
    let mut node = tree;
    let mut in_path = true;
    let mut out = Vec::new();
    for w in words {
        if in_path {
            if let Some(c) = node.find_subcommand(w).filter(|c| !c.is_hide_set()) {
                node = c;
                out.push(w.to_owned());
                continue;
            }
            in_path = false;
        }
        let low = w.to_lowercase();
        let flag = node.get_arguments().find(|a| {
            a.get_long()
                .is_some_and(|l| l == low || l.replace('-', "") == low)
        });
        out.push(match (low.as_str(), flag) {
            ("help" | "?", _) => "--help".to_owned(),
            (_, Some(a)) => format!("--{}", a.get_long().unwrap_or_default()),
            _ => w.to_owned(),
        });
    }
    out
}

/// Parse one top-level DM message as a `valet cc` command line. `Err` is the
/// reply instead: help for the level typed, after a one-line error if any.
fn parse_slack(text: &str) -> std::result::Result<Sub, String> {
    use clap::FromArgMatches;
    use clap::error::ErrorKind;
    let mut tree = slack_tree();
    let argv = slack_argv(&tree, text);
    let err = match tree.try_get_matches_from_mut(&argv) {
        Ok(m) => return Sub::from_arg_matches(&m).map_err(|e| e.to_string()),
        Err(e) => e,
    };
    // The deepest command the words name (`session attach help` -> attach).
    let mut node = &tree;
    let mut path: Vec<&str> = Vec::new();
    for w in &argv {
        match node.find_subcommand(w) {
            Some(c) if !c.is_hide_set() => {
                path.push(c.get_name());
                node = c;
            }
            _ => break,
        }
    }
    let help = slack_help(node, &path.join(" "));
    Err(match err.kind() {
        ErrorKind::DisplayHelp | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => help,
        ErrorKind::InvalidSubcommand | ErrorKind::UnknownArgument if path.is_empty() => {
            format!("Unknown command `{}`.\n\n{help}", text.trim())
        }
        _ => {
            let first = err.to_string();
            let first = first
                .lines()
                .next()
                .unwrap_or("")
                .trim_start_matches("error: ")
                .replace("'--", "'")
                .to_owned();
            format!("{first}\n\n{help}")
        }
    })
}

/// Verbosity from `v2` / `v=2` / `2` (`-v` wins over the positional form):
/// clamped to MAX_VERBOSE, negative = MAX_VERBOSE; None when not given.
pub(super) fn afk_level(level: Option<&str>, verbose: Option<&str>) -> Result<Option<u8>> {
    let Some(raw) = verbose.or(level) else {
        return Ok(None);
    };
    let t = raw.trim();
    let t = t
        .strip_prefix("v=")
        .or_else(|| t.strip_prefix('v'))
        .unwrap_or(t);
    let n: i64 = t
        .parse()
        .map_err(|_| anyhow::anyhow!("verbosity {raw:?}: want v0..v{MAX_VERBOSE} (or a number)"))?;
    Ok(Some(if n < 0 || n > MAX_VERBOSE as i64 {
        MAX_VERBOSE
    } else {
        n as u8
    }))
}

pub fn run(a: Args) -> Result<()> {
    match a.cmd {
        Sub::Afk { state: None, .. } => {
            let st = load_state();
            let on = if st.afk { "on" } else { "off" };
            println!("AFK is {on} (v={})", st.level());
            Ok(())
        }
        Sub::Afk {
            state: Some(s),
            level,
            verbose,
        } => {
            let v = afk_level(level.as_deref(), verbose.as_deref())?;
            if s == "off" && v.is_some() {
                bail!("verbosity goes with `afk on`");
            }
            let cmd = if s == "on" { "afk-on" } else { "afk-off" };
            println!("{}", request(json!({ "cmd": cmd, "verbose": v }))?);
            Ok(())
        }
        Sub::Session(SessionCmd::Ls) => list(),
        Sub::Session(SessionCmd::Attach { session }) => {
            let s = session_or_current(session)?;
            println!("{}", request(json!({ "cmd": "attach", "pane": s }))?);
            Ok(())
        }
        Sub::Session(SessionCmd::Detach { session, delete }) => {
            let s = session_or_current(session)?;
            let r = request(json!({ "cmd": "detach", "pane": s, "delete": delete }))?;
            println!("{r}");
            Ok(())
        }
        Sub::Session(SessionCmd::Screen { session }) => {
            let io = live::Io::new();
            let all = io.discover()?;
            let s = all
                .iter()
                .find(|l| {
                    l.pane == session || l.name == session || l.session_id.starts_with(&session)
                })
                .with_context(|| {
                    format!("no running Claude session matches {session:?} (`valet cc session ls`)")
                })?;
            print!("{}", io.screen(&s.term)?);
            Ok(())
        }
        Sub::Clear => clear(),
        Sub::Daemon(DaemonCmd::Status) => {
            if crate::cmd::daemon::part_running(PART) {
                print!("{}", request(json!({ "cmd": "status" }))?);
            } else {
                println!(
                    "daemon not running (AFK flag: {}); `valet cc afk on` starts it",
                    load_state().afk
                );
            }
            Ok(())
        }
        Sub::Daemon(DaemonCmd::Restart) => {
            // On, so it comes back after a reboot (linger keeps the user
            // manager running without a login).
            crate::cmd::daemon::ensure(PART)?;
            crate::cmd::daemon::restart_part(PART)
        }
        Sub::Daemon(DaemonCmd::Stop) => {
            crate::cmd::daemon::set_part(PART, false)?;
            println!("cc daemon off; `valet cc afk on` starts it");
            Ok(())
        }
        Sub::Daemon(DaemonCmd::Logs { lines }) => {
            for l in journal_tail(lines)? {
                println!("{l}");
            }
            Ok(())
        }
        Sub::Serve => run_daemon(),
    }
}

fn list() -> Result<()> {
    let mirrored: HashSet<String> = load_state().sessions.into_keys().collect();
    let sessions = live::Io::new().discover()?;
    if sessions.is_empty() {
        println!("no running Claude sessions");
    }
    for s in sessions {
        println!(
            "{:>8}  {:<24} {:<8} {:<18} {}{}",
            s.pane,
            s.display(),
            s.status,
            s.route(),
            render::tilde(&s.cwd),
            if mirrored.contains(&s.pane) {
                "  [mirrored]"
            } else {
                ""
            }
        );
    }
    Ok(())
}

// ---------- shared with the daemon ----------

/// Daemon logging, one line per event to stderr (the journal):
/// `<iso> <LEVEL> <msg>`. Never pass secrets or message bodies beyond a short
/// preview. DEBUG lines appear only with VALET_CC_DEBUG=1.
const DEBUG_ENV: &str = "VALET_CC_DEBUG";

fn log_at(level: &str, msg: &str) {
    eprintln!("{} {level} {msg}", crate::log::now_iso());
}

/// Set by the daemon: INFO lines go to the journal; CLI runs show only WARN.
static IN_DAEMON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn log(msg: &str) {
    if IN_DAEMON.load(std::sync::atomic::Ordering::Relaxed) || debug_on() {
        log_at("INFO", msg);
    }
}

fn warn(msg: &str) {
    log_at("WARN", msg);
}

fn debug_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var(DEBUG_ENV).is_ok_and(|v| !v.is_empty() && v != "0"))
}

fn debug(msg: &str) {
    if debug_on() {
        log_at("DEBUG", msg);
    }
}

/// The cc part's journal lines for `daemon logs`, newest last, timestamps
/// shortened to HH:MM:SS so they fit a phone.
fn journal_tail(n: usize) -> Result<Vec<String>> {
    Ok(crate::cmd::daemon::log_lines(Some(PART), n)?
        .iter()
        .map(|l| shorten_log_line(l))
        .collect())
}

/// `2026-11-14T13:24:17Z INFO msg` -> `13:24:17 INFO msg`.
fn shorten_log_line(l: &str) -> String {
    match l.split_once(' ') {
        Some((ts, rest)) if ts.len() == 20 && ts.ends_with('Z') && ts.contains('T') => {
            format!("{} {rest}", &ts[11..19])
        }
        _ => l.to_owned(),
    }
}

fn state_dir() -> PathBuf {
    crate::cmd::apps::self_dir("State")
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("cc")
}

fn ctl_dir() -> PathBuf {
    state_dir().join("ctl")
}

fn load_state() -> daemon::State {
    std::fs::read_to_string(state_dir().join("state.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_state(st: &daemon::State) -> Result<()> {
    let s = serde_json::to_string_pretty(st)?;
    let tmp = state_dir().join("state.json.tmp");
    std::fs::write(&tmp, s)?;
    std::fs::rename(tmp, state_dir().join("state.json"))?;
    Ok(())
}

/// Bot client, DM channel, bot user id and the operator (the DM's other member).
fn connect() -> Result<daemon::Cx> {
    let slack = Slack::new(token::bot_token()?);
    let bot_user = slack.call("auth.test", &[])?["user_id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let channel = token::default_channel()?;
    let info = slack.call("conversations.info", &[("channel", &channel)])?;
    let user_id = info["channel"]["user"]
        .as_str()
        .filter(|u| !u.is_empty())
        .with_context(|| format!("{channel} is not a DM with the bot (set SLACK_CHANNEL)"))?
        .to_owned();
    Ok(daemon::Cx {
        channel,
        user_id,
        slack,
        io: live::Io::new(),
        bot_user,
        verbose: std::cell::Cell::new(DEFAULT_VERBOSE),
    })
}

fn run_daemon() -> Result<()> {
    IN_DAEMON.store(true, std::sync::atomic::Ordering::Relaxed);
    std::fs::create_dir_all(ctl_dir())?;
    let cx = connect()?;
    log(&format!(
        "daemon up: bot={} channel={} operator={}",
        cx.bot_user, cx.channel, cx.user_id
    ));
    daemon::Daemon::new(cx, load_state()).run()
}

// ---------- process management ----------

/// The cc part of `valet daemon` on, and the daemon installed and running.
fn ensure_daemon() -> Result<()> {
    crate::cmd::daemon::ensure(PART)
}

/// Hand the daemon a request and wait for its answer.
fn request(req: Value) -> Result<String> {
    ensure_daemon()?;
    std::fs::create_dir_all(ctl_dir())?;
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let p = ctl_dir().join(format!("{nanos}-{}.req", std::process::id()));
    std::fs::write(&p, req.to_string())?;
    let done = p.with_extension("done");
    let t0 = Instant::now();
    while t0.elapsed() < REQUEST_WAIT {
        if let Ok(s) = std::fs::read_to_string(&done) {
            let _ = std::fs::remove_file(&done);
            return Ok(s);
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    let _ = std::fs::remove_file(&p);
    bail!("daemon did not answer within {REQUEST_WAIT:?} (`valet daemon logs cc`)")
}

/// The given session, else the pid of the Claude session this command runs in.
fn session_or_current(session: Option<String>) -> Result<String> {
    match session {
        Some(s) => Ok(s),
        None => live::enclosing_pid()
            .map(|p| p.to_string())
            .context("not running inside a Claude session; name one (see `valet cc session ls`)"),
    }
}

// ---------- clear ----------

/// Delete every bot message and thread reply in the DM. Threads of mirrored
/// sessions and your own messages stay.
fn clear() -> Result<()> {
    let cx = connect()?;
    let keep: HashSet<String> = load_state()
        .sessions
        .values()
        .map(|s| s.root_ts.clone())
        .collect();
    let ch = cx.channel.as_str();
    let targets = daemon::clear_targets(&cx, &keep)?;
    let mut failed = 0;
    for ts in &targets {
        if let Err(e) = cx.slack.delete(ch, ts) {
            failed += 1;
            eprintln!("delete {ts}: {e}");
        }
    }
    println!(
        "deleted {} of {} bot message(s)",
        targets.len() - failed,
        targets.len()
    );
    if !keep.is_empty() {
        println!(
            "kept {} mirrored thread(s); `valet cc afk off` removes them",
            keep.len()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {

    #[test]
    fn afk_levels_clamp_and_parse_all_spellings() {
        let l = |p: Option<&str>, f: Option<&str>| afk_level(p, f).unwrap();
        assert_eq!(l(None, None), None);
        assert_eq!(l(Some("v2"), None), Some(2));
        assert_eq!(l(Some("v=0"), None), Some(0));
        assert_eq!(l(Some("1"), None), Some(1));
        assert_eq!(l(Some("v99"), None), Some(MAX_VERBOSE));
        assert_eq!(l(None, Some("-1")), Some(MAX_VERBOSE));
        assert_eq!(l(Some("v1"), Some("3")), Some(3));
        assert!(afk_level(Some("loud"), None).is_err());
    }
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct T {
        #[command(flatten)]
        a: Args,
    }

    fn parse(args: &[&str]) -> Result<Sub, clap::Error> {
        Ok(
            T::try_parse_from(std::iter::once("cc").chain(args.iter().copied()))?
                .a
                .cmd,
        )
    }

    #[test]
    fn parses_commands() {
        assert!(
            matches!(parse(&["afk", "on"]).unwrap(), Sub::Afk { state: Some(s), .. } if s == "on")
        );
        assert!(matches!(
            parse(&["afk"]).unwrap(),
            Sub::Afk { state: None, .. }
        ));
        assert!(parse(&["afk", "maybe"]).is_err());
        assert!(matches!(parse(&["afk", "on", "v2"]).unwrap(),
            Sub::Afk { level: Some(l), .. } if l == "v2"));
        assert!(matches!(parse(&["afk", "on", "-v", "-1"]).unwrap(),
            Sub::Afk { verbose: Some(v), .. } if v == "-1"));
        assert!(matches!(
            parse(&["session", "detach", "gpu-jam", "--delete"]).unwrap(),
            Sub::Session(SessionCmd::Detach { session: Some(s), delete: true }) if s == "gpu-jam"
        ));
        assert!(matches!(
            parse(&["s", "attach"]).unwrap(),
            Sub::Session(SessionCmd::Attach { session: None })
        ));
        assert!(matches!(
            parse(&["session", "list"]).unwrap(),
            Sub::Session(SessionCmd::Ls)
        ));
        assert!(matches!(parse(&["clear"]).unwrap(), Sub::Clear));
        assert!(parse(&["clear", "--dry-run"]).is_err());
        assert!(matches!(
            parse(&["daemon", "restart"]).unwrap(),
            Sub::Daemon(DaemonCmd::Restart)
        ));
    }

    #[test]
    fn log_lines_shortened_for_phones() {
        assert_eq!(
            shorten_log_line("2026-11-14T13:24:17Z INFO daemon up"),
            "13:24:17 INFO daemon up"
        );
        assert_eq!(shorten_log_line("-- No entries --"), "-- No entries --");
        assert!(matches!(
            parse_slack("daemon logs 50"),
            Ok(Sub::Daemon(DaemonCmd::Logs { lines: 50 }))
        ));
        assert!(matches!(
            parse_slack("daemon logs"),
            Ok(Sub::Daemon(DaemonCmd::Logs { lines: 30 }))
        ));
    }

    #[test]
    fn slack_shell_parses_and_helps() {
        assert!(matches!(parse_slack("afk on"), Ok(Sub::Afk { state: Some(s), .. }) if s == "on"));
        assert!(matches!(
            parse_slack("  session   attach  2 "),
            Ok(Sub::Session(SessionCmd::Attach { session: Some(s) })) if s == "2"
        ));
        assert!(matches!(parse_slack("clear"), Ok(Sub::Clear)));
        // help at every level: one bullet per line, no column padding, short lines
        for (t, want) in [
            ("help", "• `session` "),
            ("afk help", "*afk* "),
            ("clear help", "*clear* "),
            ("?", "• `daemon` "),
            ("session", "• `session attach` "),
            ("session help", "• `session ls` "),
            ("help clear", "*clear* "),
            ("session attach help", "• `SESSION` "),
            ("help session attach", "• `SESSION` "),
            ("help", "• `clear` "),
        ] {
            let h = parse_slack(t).unwrap_err();
            assert!(h.contains(want), "{t}: {h}");
            assert!(!h.contains("  "), "{t}: padded: {h}");
            assert!(
                h.lines().all(|l| l.chars().count() < 110),
                "{t}: long line: {h}"
            );
        }
        // no dashes anywhere: flags are plain words
        assert!(matches!(
            parse_slack("session detach 2 delete"),
            Ok(Sub::Session(SessionCmd::Detach { session: Some(s), delete: true })) if s == "2"
        ));
        for t in ["help", "session help", "clear help"] {
            assert!(!parse_slack(t).unwrap_err().contains("--"), "{t}");
        }
        let e = parse_slack("afk maybe").unwrap_err();
        assert!(e.contains("maybe") && e.contains("on|off"), "{e}");
        // unknown: one line, then the full command list
        let h = parse_slack("frobnicate now").unwrap_err();
        assert!(
            h.starts_with("Unknown command `frobnicate now`") && h.contains("• `session`"),
            "{h}"
        );
        assert!(parse_slack("help").unwrap_err().starts_with("• `afk`"));
        // the hidden daemon entry point is not advertised
        assert!(!parse_slack("help").unwrap_err().contains("serve"));
    }
}
