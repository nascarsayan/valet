//! `valet hook claude canonical-paths`: a Claude Code PreToolUse hook that
//! keeps the agent on one spelling of this host's folders.
//!
//! A folder often has several names: a home-dir link to the ws folder
//! (`~/ws`), or a mount that shows a local disk under another prefix. Claude
//! Code treats each spelling as a different place: it asks again for each,
//! and refuses a worktree whose printed path is not the one git records. So
//! this hook denies a file-tool path (`file_path`, `notebook_path`, `path`,
//! Glob's `pattern`) or a Bash command that uses another spelling, and the
//! reason names this host's one.
//!
//! The other spellings:
//!
//! ```text
//! ~/<link>/X, $HOME/<link>/X, ${HOME}/<link>/X, <home>/<link>/X
//!     (any home-dir link to the ws folder; computed)
//! <from>/X -> <to>/X
//!     (each [[paths.alias]] in config.toml; {host} and {home} expand)
//! ```
//!
//! Other paths under the home dir (`<home>/.ssh`) are fine. Bash matches whole
//! path words, anywhere but in quoted here-documents (`<<'EOF'`: data, such as
//! a commit message) and the rest of an `ssh` command (it runs on another
//! host, which has its own spelling). Nothing is stat'ed, so the hook cannot
//! itself trigger a mount.

use super::skill_source::{emit, read_stdin};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Characters that end a path word in shell text.
const END: &str = r#"\s'"`;|&<>(){}"#;

/// config.toml `[paths]`.
#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    alias: Vec<Alias>,
}

/// One `[[paths.alias]]`: paths under `from` are spelled under `to` here.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Alias {
    from: String,
    to: String,
}

/// Parse config.toml TEXT's `[paths]` (for `valet config` checks).
pub fn validate(text: &str) -> anyhow::Result<()> {
    let c: Config = crate::config::section_of(text, "paths")?;
    for a in &c.alias {
        if !a.from.starts_with('/') || a.from.len() < 2 {
            anyhow::bail!(
                "config.toml: [[paths.alias]] from = {:?}: an absolute path below /",
                a.from
            );
        }
        if !a.to.is_empty() && !a.to.starts_with('/') {
            anyhow::bail!(
                "config.toml: [[paths.alias]] to = {:?}: an absolute path or \"\" (/)",
                a.to
            );
        }
    }
    Ok(())
}

/// The configured aliases as (written prefix, this host's spelling), with
/// `{host}` and `{home}` expanded; an alias that needs an unknown value or
/// expands to `/` is skipped.
fn configured(c: &Config, home: &str, host: &str) -> Vec<(String, String)> {
    let expand = |s: &str| {
        if (s.contains("{host}") && host.is_empty()) || (s.contains("{home}") && home.is_empty()) {
            return None;
        }
        Some(s.replace("{host}", host).replace("{home}", home))
    };
    c.alias
        .iter()
        .filter_map(|a| {
            let from = expand(&a.from)?.trim_end_matches('/').to_owned();
            let to = expand(&a.to)?.trim_end_matches('/').to_owned();
            (!from.is_empty() && from != to).then_some((from, to))
        })
        .collect()
}

/// The other spellings of this host's folders.
struct Spellings {
    /// (written prefix, this host's spelling; "" is /), longest prefix first.
    aliases: Vec<(String, String)>,
}

impl Spellings {
    /// HOME's links `links` (name, canonical target) that lead to the ws
    /// folder WS name it four ways each; `extra` adds configured aliases.
    fn new(
        home: &Path,
        links: &[(String, PathBuf)],
        ws: &Path,
        extra: Vec<(String, String)>,
    ) -> Self {
        let mut aliases = extra;
        for (name, target) in links {
            if target != ws {
                continue;
            }
            let to = ws.display().to_string();
            for written in [
                format!("~/{name}"),
                format!("$HOME/{name}"),
                format!("${{HOME}}/{name}"),
                home.join(name).display().to_string(),
            ] {
                aliases.push((written, to.clone()));
            }
        }
        aliases.sort_by_key(|(w, _)| std::cmp::Reverse(w.len()));
        Spellings { aliases }
    }

    /// This host's spelling of the whole path P and what P used instead,
    /// when P spells it another way.
    fn fix(&self, p: &str) -> Option<(String, String)> {
        for (w, to) in &self.aliases {
            if let Some(rest) = p.strip_prefix(w.as_str())
                && (rest.is_empty() || rest.starts_with('/'))
            {
                let fixed = format!("{to}{rest}");
                let fixed = if fixed.is_empty() {
                    "/".to_owned()
                } else {
                    fixed
                };
                return Some((fixed, w.clone()));
            }
        }
        None
    }

    /// The first path word in shell text CMD that [`Self::fix`] corrects:
    /// (word, its fix, what it used).
    fn scan(&self, cmd: &str) -> Option<(String, String, String)> {
        if self.aliases.is_empty() {
            return None;
        }
        let alts: Vec<String> = self.aliases.iter().map(|(w, _)| regex::escape(w)).collect();
        let re = regex::Regex::new(&format!(
            r#"(?:^|[{END}=:,])((?:{})(?:/[^{END}:]*)?)"#,
            alts.join("|")
        ))
        .expect("valid regex");
        let cmd = strip_remote(&strip_quoted_heredocs(cmd));
        for c in re.captures_iter(&cmd) {
            let m = c.get(1).expect("group");
            // A whole word: `~/wsx` is another name.
            let next = cmd[m.end()..].chars().next();
            if next.is_some_and(|ch| !(ch.is_whitespace() || r#"'"`;|&<>(){},:"#.contains(ch))) {
                continue;
            }
            let word = m.as_str().trim_end_matches(',');
            if let Some((fixed, used)) = self.fix(word) {
                return Some((word.to_owned(), fixed, used));
            }
        }
        None
    }

    /// The deny reason for one tool call, if it is refused.
    fn judge(&self, tool: &str, input: &Value) -> Option<String> {
        let (word, fixed, used) = if tool == "Bash" {
            self.scan(input["command"].as_str()?)?
        } else {
            let mut keys = vec!["file_path", "notebook_path", "path"];
            if tool == "Glob" {
                keys.push("pattern");
            }
            keys.iter().find_map(|k| {
                let p = input[*k].as_str()?;
                self.fix(p).map(|(f, u)| (p.to_owned(), f, u))
            })?
        };
        Some(format!(
            "valet hook claude canonical-paths: use {fixed} (this host's path for {used}), \
             not {word}. One spelling per host: other spellings of the same folder are \
             refused (Claude Code treats them as different places). $USERLAND is this \
             host's spelling of Userland."
        ))
    }
}

/// CMD without the bodies of quoted here-documents (`<<'EOF'`, `<<"EOF"`,
/// `<<\EOF`): literal data, such as a commit message or a file's text.
fn strip_quoted_heredocs(cmd: &str) -> String {
    let start = regex::Regex::new(r#"<<-?\s*(?:'([A-Za-z_][A-Za-z0-9_]*)'|"([A-Za-z_][A-Za-z0-9_]*)"|\\([A-Za-z_][A-Za-z0-9_]*))"#)
        .expect("valid regex");
    let mut out = String::new();
    let mut end: Option<String> = None;
    for line in cmd.lines() {
        if let Some(d) = &end {
            if line.trim() == d {
                end = None;
            } else {
                continue;
            }
        } else if let Some(c) = start.captures(line) {
            end = c
                .iter()
                .skip(1)
                .flatten()
                .next()
                .map(|m| m.as_str().to_owned());
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// CMD with the rest of each `ssh` command (`ssh`, `valet ssh ...`) and of
/// `herdr pane run` / `herdr agent prompt` (text for another pane) blanked
/// out up to an unquoted `;`, `|`, `&`, redirect or line end: options, host and remote
/// command run on (or name) the other host. An unquoted here-document fed to
/// it is blanked too.
fn strip_remote(cmd: &str) -> String {
    let heredoc =
        regex::Regex::new(r#"<<-?\s*['"\\]?([A-Za-z_][A-Za-z0-9_]*)"#).expect("valid regex");
    let mut out = String::with_capacity(cmd.len());
    let mut lines = cmd.split_inclusive('\n');
    while let Some(line) = lines.next() {
        let b: Vec<char> = line.chars().collect();
        let mut i = 0;
        let mut quote: Option<char> = None;
        let mut remote = false;
        let mut skipped = String::new();
        while i < b.len() {
            let c = b[i];
            if remote {
                match (quote, c) {
                    (None, ';' | '|' | '&' | '>' | '\n') => remote = false,
                    // `<file` is local input; `<<EOF` feeds the remote side.
                    (None, '<') if b.get(i + 1) != Some(&'<') && (i == 0 || b[i - 1] != '<') => {
                        remote = false
                    }
                    (None, '\'' | '"') => quote = Some(c),
                    (Some(q), _) if q == c => quote = None,
                    _ => {}
                }
                if remote {
                    skipped.push(c);
                    out.push(if c == '\n' { c } else { ' ' });
                    i += 1;
                    continue;
                }
            } else {
                match (quote, c) {
                    (None, '\'' | '"') => quote = Some(c),
                    (Some(q), _) if q == c => quote = None,
                    (None, 's')
                        if b[i..].starts_with(&['s', 's', 'h'])
                            && (i == 0 || " \t;|&(".contains(b[i - 1]))
                            && b.get(i + 3).is_none_or(|n| n.is_whitespace()) =>
                    {
                        remote = true;
                    }
                    // Text typed into another herdr pane runs in that pane's shell.
                    (None, 'h')
                        if (i == 0 || " \t;|&(".contains(b[i - 1]))
                            && ["herdr pane run ", "herdr agent prompt "]
                                .iter()
                                .any(|p| b[i..].iter().take(p.len()).copied().eq(p.chars())) =>
                    {
                        remote = true;
                    }
                    _ => {}
                }
            }
            out.push(c);
            i += 1;
        }
        if let Some(d) = heredoc.captures(&skipped).map(|c| c[1].to_owned()) {
            for body in lines.by_ref() {
                out.push('\n');
                if body.trim() == d {
                    break;
                }
            }
        }
    }
    out
}

/// Text that may name another spelling: cheap test before reading $HOME's
/// links. EXTRA: the configured aliases.
fn may_match(input: &Value, home: &str, extra: &[(String, String)]) -> bool {
    let s = input.to_string();
    s.contains("~/")
        || s.contains("HOME")
        || (!home.is_empty() && s.contains(home))
        || extra.iter().any(|(w, _)| s.contains(w.as_str()))
}

/// `valet hook claude canonical-paths`: prints a deny decision only when
/// there is one; always exits 0.
pub fn pre_tool_use() {
    let Some(input) = read_stdin() else { return };
    let tool = input["tool_name"].as_str().unwrap_or_default();
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let host = crate::cmd::apps::host();
    // A broken config must not block every tool call: no configured aliases.
    let c: Config = crate::config::section("paths").unwrap_or_default();
    let extra = configured(&c, &home.display().to_string(), &host);
    if !may_match(&input["tool_input"], &home.display().to_string(), &extra) {
        return;
    }
    let ws = crate::cmd::apps::root()
        .ok()
        .and_then(|r| r.parent().map(Path::to_path_buf));
    let links: Vec<(String, PathBuf)> = match &ws {
        Some(_) if !home.as_os_str().is_empty() => crate::cmd::apps::home_dir_links(&home)
            .into_iter()
            .filter_map(|(l, t)| Some((l.file_name()?.to_str()?.to_owned(), t)))
            .collect(),
        _ => Vec::new(),
    };
    let s = Spellings::new(&home, &links, ws.as_deref().unwrap_or(Path::new("")), extra);
    if let Some(reason) = s.judge(tool, &input["tool_input"]) {
        emit(&json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }}));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WS: &str = "/data/ws";

    /// Home `/home/u` with `ws` -> WS and `.ssh` elsewhere; config aliases
    /// `/mnt/{host}` -> `/data` and `/mnt/a` -> `/data/a`, host `vm`.
    fn spellings() -> Spellings {
        let links = [
            ("ws".to_owned(), PathBuf::from(WS)),
            (".ssh".to_owned(), PathBuf::from("/data/ws/Userland/Ssh")),
        ];
        let c: Config = toml::from_str(
            r#"
            [[alias]]
            from = "/mnt/{host}"
            to = "/data"
            [[alias]]
            from = "/mnt/a"
            to = "/data/a"
            [[alias]]
            from = "/remote/{host}/"
            to = ""
            "#,
        )
        .unwrap();
        Spellings::new(
            Path::new("/home/u"),
            &links,
            Path::new(WS),
            configured(&c, "/home/u", "vm"),
        )
    }

    fn judge(tool: &str, input: Value) -> Option<String> {
        spellings().judge(tool, &input)
    }

    fn bash(c: &str) -> Option<String> {
        judge("Bash", json!({ "command": c }))
    }

    #[test]
    fn file_tools_with_another_spelling_are_refused() {
        for (tool, key, p) in [
            ("Read", "file_path", "/home/u/ws/Userland/x.rs"),
            ("Edit", "file_path", "~/ws/a"),
            ("Write", "file_path", "/mnt/vm/ws/new.txt"),
            ("Write", "file_path", "/mnt/a/new.txt"),
            ("NotebookEdit", "notebook_path", "$HOME/ws/n.ipynb"),
            ("Grep", "path", "/home/u/ws"),
            ("Glob", "path", "${HOME}/ws/src"),
            ("Glob", "pattern", "/home/u/ws/**/*.rs"),
        ] {
            assert!(judge(tool, json!({ key: p })).is_some(), "{tool} {p}");
        }
        let r = judge("Read", json!({"file_path": "/home/u/ws/Userland/x"})).unwrap();
        assert!(
            r.contains("use /data/ws/Userland/x (this host's path for /home/u/ws), not /home/u/ws/Userland/x"),
            "{r}"
        );
        let r = judge("Read", json!({"file_path": "/mnt/a/b"})).unwrap();
        assert!(
            r.contains("use /data/a/b (this host's path for /mnt/a)"),
            "{r}"
        );
        let r = judge("Read", json!({"file_path": "/remote/vm/srv/x"})).unwrap();
        assert!(
            r.contains("use /srv/x (this host's path for /remote/vm)"),
            "{r}"
        );
        let r = judge("Read", json!({"file_path": "/remote/vm"})).unwrap();
        assert!(r.contains("use / ("), "{r}");
    }

    #[test]
    fn file_tools_with_this_hosts_spelling_pass() {
        for p in [
            "/data/ws/Userland/x.rs",
            "/home/u/.ssh/config",
            "/home/u/wsx/a",
            "/home/u/notes.md",
            "/mnt/other-host/x",
            "/mnt/ab/x",
            "/remote/vm2/x",
        ] {
            assert_eq!(judge("Read", json!({ "file_path": p })), None, "{p}");
        }
        assert_eq!(
            judge("Grep", json!({ "pattern": "~/ws" })),
            None,
            "Grep's pattern is a regex"
        );
    }

    #[test]
    fn bash_with_another_spelling_is_refused() {
        for c in [
            "cd ~/ws/Userland",
            "ls ~/ws",
            "cat \"$HOME/ws/a b\"",
            "rg foo ${HOME}/ws/src",
            "cargo test --manifest-path=/home/u/ws/x/Cargo.toml",
            "x=$(ls /mnt/vm/ws)",
            "echo hi > /mnt/a/f; ls",
            "cat <<EOF\n$HOME/ws/x\nEOF",
            "PATH=/home/u/ws/bin:$PATH foo",
            "(cd ~/ws && make)",
            "ssh arm true; ls /mnt/a",
            "ssh arm 'cat f' > ~/ws/out",
            "ssh arm 'x' | tee ~/ws/out",
            "ssh arm sh < ~/ws/script",
        ] {
            assert!(bash(c).is_some(), "{c}");
        }
        let r = bash("cd ~/ws/Userland && ls").unwrap();
        assert!(
            r.contains("use /data/ws/Userland (this host's path for ~/ws), not ~/ws/Userland."),
            "{r}"
        );
    }

    #[test]
    fn bash_with_this_hosts_spelling_or_other_paths_passes() {
        for c in [
            "cd /data/ws/Userland && cargo test",
            "ls ~/notws ~/wsx ~/.ssh /home/u/.ssh/config",
            "ls /srv/ws/x ~/Documents/ws",
            "cat /mnt/other/ws/x",
            "ls /mnt/ab",
            "echo $USERLAND/Repos",
            "git commit -F - <<'EOF'\nuse ~/ws/x, not /mnt/a/x\nEOF",
            "cat > f.md <<\"EOF\"\nsee $HOME/ws\nEOF",
            "ssh -o BatchMode=yes arm 'ls -d /mnt/a'",
            "ssh -J jump host1 -- cat ~/ws/x",
            "ssh arm bash <<EOF\nls /mnt/a/x\nEOF",
            "herdr pane run w1:p2 'cd ~/ws && claude'",
            "herdr agent prompt a \"ls ~/ws\" --wait",
        ] {
            assert_eq!(bash(c), None, "{c}");
        }
    }

    #[test]
    fn no_ws_link_leaves_only_configured_aliases() {
        let s = Spellings::new(
            Path::new("/h"),
            &[],
            Path::new(WS),
            vec![("/mnt/a".to_owned(), "/data/a".to_owned())],
        );
        assert!(s.judge("Bash", &json!({"command": "ls ~/ws"})).is_none());
        assert!(
            s.judge("Bash", &json!({"command": "ls /mnt/a/x"}))
                .is_some()
        );
        let none = Spellings::new(Path::new("/h"), &[], Path::new(WS), Vec::new());
        assert!(
            none.judge("Bash", &json!({"command": "ls /mnt/a/x"}))
                .is_none()
        );
    }

    #[test]
    fn placeholders_need_values() {
        let c: Config = toml::from_str(
            "[[alias]]\nfrom = \"/mnt/{host}\"\nto = \"{home}/m\"\n[[alias]]\nfrom = \"/x\"\nto = \"/x/\"\n",
        )
        .unwrap();
        assert_eq!(
            configured(&c, "/h", "vm"),
            [("/mnt/vm".to_owned(), "/h/m".to_owned())]
        );
        assert!(configured(&c, "/h", "").is_empty());
        assert!(configured(&c, "", "vm").is_empty());
    }

    #[test]
    fn config_is_validated() {
        assert!(validate("").is_ok());
        assert!(validate("[[paths.alias]]\nfrom = \"/mnt/a\"\nto = \"/data/a\"\n").is_ok());
        assert!(validate("[[paths.alias]]\nfrom = \"/r/{host}\"\nto = \"\"\n").is_ok());
        assert!(validate("[[paths.alias]]\nfrom = \"mnt\"\nto = \"/d\"\n").is_err());
        assert!(validate("[[paths.alias]]\nfrom = \"/\"\nto = \"/d\"\n").is_err());
        assert!(validate("[[paths.alias]]\nfrom = \"/m\"\nto = \"d\"\n").is_err());
        assert!(validate("[[paths.alias]]\nfrom = \"/m\"\nto = \"/d\"\nx = 1\n").is_err());
    }

    #[test]
    fn quoted_heredoc_bodies_are_dropped() {
        assert_eq!(
            strip_quoted_heredocs("a <<'E'\nbody\nE\nb <<X\nkept\nX\n"),
            "a <<'E'\nE\nb <<X\nkept\nX\n"
        );
    }
}
