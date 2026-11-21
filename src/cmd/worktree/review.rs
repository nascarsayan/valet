//! `valet git review`: agent-facing CLI over a worktree's local review store, so an agent never
//! hand-edits `threads.json`. `valet git review sync` syncs GitHub PR review
//! threads with local markdown (see `pr_sync`). Thread-command errors print `review: <message>` and exit 1.

use super::json::J;
use super::store::{self, NewThread};
use anyhow::{Result, anyhow, bail};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

#[derive(clap::Args)]
pub struct Args {
    /// Worktree holding .worktree-review. Default: the worktree containing the cwd.
    #[arg(long, global = true, value_name = "WORKTREE")]
    dir: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Sub,
}

#[derive(clap::Args)]
struct Who {
    /// Author name. Default: --model, else "agent" (with --human: $USER, else "reviewer").
    #[arg(long)]
    author: Option<String>,
    /// Agent harness of a bot author, e.g. claude-code.
    #[arg(long)]
    agent: Option<String>,
    /// Model id of a bot author.
    #[arg(long)]
    model: Option<String>,
    /// Act as the human reviewer (may set "resolved").
    #[arg(long)]
    human: bool,
}

#[derive(clap::Args)]
struct Body {
    /// Comment text; `-` (or no value) reads it from stdin, the safe way to pass markdown.
    #[arg(long, num_args = 0..=1, default_missing_value = "-", value_name = "TEXT|-")]
    body: Option<String>,
}

#[derive(clap::Subcommand)]
enum Sub {
    #[command(flatten)]
    Thread(Thread),
    /// GitHub PR review threads <-> local markdown: fetch, then push replies.
    Sync(super::pr_sync::Args),
}

#[derive(clap::Subcommand)]
enum Thread {
    /// List threads: open ones by default.
    List {
        /// Unresolved threads (the default).
        #[arg(long)]
        open: bool,
        /// Threads whose last comment is not from a bot: they need an agent reply.
        #[arg(long)]
        awaiting: bool,
        /// Every thread, resolved included.
        #[arg(long)]
        all: bool,
        /// JSON summaries instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Show one thread with its full history.
    Show {
        /// Thread id, e.g. t3.
        id: String,
        /// The raw thread as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Reply on a thread, optionally setting its status in the same call.
    Reply {
        /// Thread id, e.g. t3.
        id: String,
        #[command(flatten)]
        body: Body,
        /// Badge on the reply: accepted, rejected, noted, question, or review.
        #[arg(long)]
        action: Option<String>,
        /// New thread status: addressed, rejected, or noted (resolved needs --human).
        #[arg(long)]
        status: Option<String>,
        #[command(flatten)]
        who: Who,
    },
    /// Start a new thread on a file line.
    Add {
        /// Path relative to the worktree.
        #[arg(long)]
        file: Option<String>,
        /// First line (1-based).
        #[arg(long)]
        line: Option<i64>,
        /// Last line. Default: --line.
        #[arg(long)]
        end: Option<i64>,
        #[command(flatten)]
        body: Body,
        /// Badge on the comment. Default for bots: review.
        #[arg(long)]
        action: Option<String>,
        #[command(flatten)]
        who: Who,
    },
    /// Set a thread's status: open, addressed, rejected, noted, or resolved.
    Status {
        /// Thread id, e.g. t3.
        id: String,
        /// New status.
        status: Option<String>,
        /// Act as the human reviewer (may set "resolved").
        #[arg(long)]
        human: bool,
    },
    /// Rewrite .worktree-review/DISCUSSION.md and print its path.
    Render {
        /// Print the markdown instead of writing it.
        #[arg(long)]
        stdout: bool,
    },
    /// Thread totals.
    Counts {
        /// Compact JSON.
        #[arg(long)]
        json: bool,
    },
}

pub fn run(a: Args) -> Result<()> {
    let t = match a.cmd {
        Sub::Sync(s) => return super::pr_sync::run(s),
        Sub::Thread(t) => t,
    };
    if let Err(e) = run_inner(a.dir.as_deref(), t) {
        eprintln!("review: {e}");
        std::process::exit(1);
    }
    Ok(())
}

/// `path.resolve`: absolute, `.`/`..` folded lexically, symlinks kept.
fn resolve(p: &Path) -> Result<PathBuf> {
    let abs = if p.is_absolute() {
        p.to_owned()
    } else {
        std::env::current_dir()?.join(p)
    };
    Ok(normalize(&abs))
}

fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

/// A path relative to the worktree that stays inside it.
fn inside(rel: &str) -> bool {
    let p = Path::new(rel);
    if rel.is_empty() || p.is_absolute() {
        return false;
    }
    let mut depth = 0i32;
    for c in p.components() {
        match c {
            Component::Normal(_) => depth += 1,
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            Component::CurDir => {}
            _ => return false,
        }
    }
    true
}

fn worktree_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    crate::proc::Cmd::new("git")
        .arg("-C")
        .arg(&cwd)
        .args(["rev-parse", "--show-toplevel"])
        .checked()
        .map(PathBuf::from)
        .map_err(|_| anyhow!("{} is not inside a git worktree", cwd.display()))
}

fn branch_of(wt: &Path) -> String {
    match crate::proc::Cmd::new("git")
        .arg("-C")
        .arg(wt)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
    {
        Ok(o) if o.status.success() => o.stdout.trim().to_owned(),
        Ok(o) => {
            eprint!("{}", o.stderr);
            base_name(wt)
        }
        Err(_) => base_name(wt),
    }
}

fn base_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn read_body(v: Option<&str>) -> Result<String> {
    match v {
        Some("-") => {
            let mut b = Vec::new();
            std::io::stdin().read_to_end(&mut b)?;
            Ok(store::js_trim(&String::from_utf8_lossy(&b)).to_owned())
        }
        Some(s) => Ok(s.to_owned()),
        None => Ok(String::new()),
    }
}

fn nonempty(v: &Option<String>) -> Option<&str> {
    v.as_deref().filter(|s| !s.is_empty())
}

/// `identity(flags)`.
fn identity(w: &Who) -> J {
    if w.human {
        let user = std::env::var("USER").ok().filter(|u| !u.is_empty());
        let name = nonempty(&w.author)
            .map(str::to_owned)
            .or(user)
            .unwrap_or_else(|| "reviewer".into());
        return J::obj()
            .with("name", J::str(name))
            .with("kind", J::str("human"));
    }
    let name = nonempty(&w.author)
        .or(nonempty(&w.model))
        .unwrap_or("agent");
    let mut o = J::obj()
        .with("name", J::str(name))
        .with("kind", J::str("bot"));
    if let Some(a) = nonempty(&w.agent) {
        o.set("agent", J::str(a));
    }
    if let Some(m) = nonempty(&w.model) {
        o.set("model", J::str(m));
    }
    o
}

/// `anchorFor`: the trimmed text of `line` in `file`, or "".
fn anchor_for(wt: &Path, file: &str, line: i64) -> String {
    let Ok(bytes) = std::fs::read(wt.join(file)) else {
        return String::new();
    };
    let text = String::from_utf8_lossy(&bytes);
    usize::try_from(line - 1)
        .ok()
        .and_then(|i| text.split('\n').nth(i))
        .map(|l| store::js_trim(l).to_owned())
        .unwrap_or_default()
}

fn write_discussion(wt: &Path, data: &J) -> Result<()> {
    std::fs::create_dir_all(store::store_dir(wt))?;
    std::fs::write(
        store::discussion_path(wt),
        store::render(data, &branch_of(wt), wt),
    )?;
    Ok(())
}

fn run_inner(dir: Option<&Path>, cmd: Thread) -> Result<()> {
    let wt = match dir {
        Some(d) => resolve(d)?,
        None => worktree_root()?,
    };
    let mut data = store::load(&wt)?;

    match cmd {
        Thread::List {
            open: _,
            awaiting,
            all,
            json,
        } => {
            let ts: Vec<&J> = store::threads(&data)
                .iter()
                .filter(|t| {
                    if awaiting {
                        store::needs_reply(t)
                    } else {
                        all || store::is_open(t)
                    }
                })
                .collect();
            if json {
                let out = J::obj()
                    .with("worktree", J::str(wt.to_string_lossy()))
                    .with("branch", J::str(branch_of(&wt)))
                    .with(
                        "threads",
                        J::Arr(ts.iter().map(|t| store::summary(t)).collect()),
                    );
                println!("{}", out.to_pretty());
            } else if ts.is_empty() {
                println!("no matching threads");
            } else {
                for t in ts {
                    print!("{}", store::print_thread(t));
                }
            }
        }
        Thread::Show { id, json } => {
            let t = &store::threads(&data)[store::find_thread(&data, &id)?];
            if json {
                println!("{}", t.to_pretty());
            } else {
                print!("{}", store::print_thread(t));
            }
        }
        Thread::Reply {
            id,
            body,
            action,
            status,
            who,
        } => {
            let text = read_body(body.body.as_deref())?;
            if text.is_empty() {
                bail!("reply needs --body <text|->");
            }
            let comment =
                store::add_comment(&mut data, &id, &text, &identity(&who), action.as_deref())?;
            let status = status.filter(|s| !s.is_empty());
            if let Some(s) = &status {
                store::set_status(&mut data, &id, s, !who.human)?;
            }
            store::save(&wt, &data)?;
            write_discussion(&wt, &data)?;
            println!(
                "replied on {id} as {}{}",
                super::json::js_opt(comment.get("author").and_then(|a| a.get("name"))),
                status.map(|s| format!(", status {s}")).unwrap_or_default()
            );
        }
        Thread::Add {
            file,
            line,
            end,
            body,
            action,
            who,
        } => {
            let Some(file) = file.filter(|f| !f.is_empty()) else {
                bail!("add needs --file <path relative to the worktree>");
            };
            if !inside(&file) {
                bail!("add --file must be a path inside the worktree: {file}");
            }
            let Some(start) = line else {
                bail!("add needs --line <n>");
            };
            let text = read_body(body.body.as_deref())?;
            if text.is_empty() {
                bail!("add needs --body <text|->");
            }
            let author = identity(&who);
            let action = nonempty(&action)
                .map(str::to_owned)
                .or_else(|| (!who.human).then(|| "review".to_owned()));
            let anchor = anchor_for(&wt, &file, start);
            let idx = store::add_thread(
                &mut data,
                NewThread {
                    file: &file,
                    start_line: start,
                    end_line: end.filter(|e| *e != 0).unwrap_or(start),
                    body: &text,
                    author: &author,
                    anchor_text: &anchor,
                    action: action.as_deref(),
                },
            )?;
            store::save(&wt, &data)?;
            write_discussion(&wt, &data)?;
            let t = &store::threads(&data)[idx];
            println!(
                "{} {}:{}",
                super::json::js_opt(t.get("id")),
                super::json::js_opt(t.get("file")),
                super::json::js_opt(t.get("startLine"))
            );
        }
        Thread::Status { id, status, human } => {
            let Some(next) = status.filter(|s| !s.is_empty()) else {
                bail!("status needs one of {}", store::status_names().join(", "));
            };
            store::set_status(&mut data, &id, &next, !human)?;
            store::save(&wt, &data)?;
            write_discussion(&wt, &data)?;
            println!("{id} -> {next}");
        }
        Thread::Render { stdout } => {
            if stdout {
                print!("{}", store::render(&data, &branch_of(&wt), &wt));
            } else {
                write_discussion(&wt, &data)?;
                println!("{}", store::discussion_path(&wt).display());
            }
        }
        Thread::Counts { json } => {
            let c = store::counts(&data);
            if json {
                println!("{}", c.to_json().to_compact());
            } else {
                println!(
                    "{} threads, {} open, {} awaiting an agent",
                    c.threads, c.open, c.awaiting_agent
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_must_stay_inside() {
        assert!(inside("src/a.rs"));
        assert!(inside("./a/../b"));
        assert!(!inside("../x"));
        assert!(!inside("a/../../x"));
        assert!(!inside("/etc/passwd"));
        assert!(!inside(""));
    }

    #[test]
    fn normalize_folds_dots() {
        assert_eq!(normalize(Path::new("/a/./b/../c")), PathBuf::from("/a/c"));
        assert_eq!(normalize(Path::new("/../a")), PathBuf::from("/a"));
    }

    #[test]
    fn identities() {
        let w = |author: Option<&str>, agent: Option<&str>, model: Option<&str>, human| Who {
            author: author.map(Into::into),
            agent: agent.map(Into::into),
            model: model.map(Into::into),
            human,
        };
        assert_eq!(
            identity(&w(None, Some("cc"), Some("opus"), false)).to_compact(),
            r#"{"name":"opus","kind":"bot","agent":"cc","model":"opus"}"#
        );
        assert_eq!(
            identity(&w(None, None, None, false)).to_compact(),
            r#"{"name":"agent","kind":"bot"}"#
        );
        assert_eq!(
            identity(&w(Some("alice"), Some("cc"), None, true)).to_compact(),
            r#"{"name":"alice","kind":"human"}"#
        );
    }

    #[test]
    fn body_flag_parsing() {
        use clap::Parser;
        #[derive(Parser)]
        struct T {
            #[command(subcommand)]
            c: Wrap,
        }
        #[derive(clap::Subcommand)]
        enum Wrap {
            Review(Args),
        }
        let parse = |v: &[&str]| {
            let Wrap::Review(a) = T::try_parse_from(v).unwrap().c;
            a
        };
        let a = parse(&["t", "review", "reply", "t1", "--body", "-", "--dir", "/x"]);
        assert_eq!(a.dir.as_deref(), Some(Path::new("/x")));
        match a.cmd {
            Sub::Thread(Thread::Reply { body, .. }) => assert_eq!(body.body.as_deref(), Some("-")),
            _ => panic!(),
        }
        let a = parse(&["t", "review", "reply", "t1", "--body", "--status", "noted"]);
        match a.cmd {
            Sub::Thread(Thread::Reply { body, status, .. }) => {
                assert_eq!(body.body.as_deref(), Some("-"), "bare --body reads stdin");
                assert_eq!(status.as_deref(), Some("noted"));
            }
            _ => panic!(),
        }
    }
}
