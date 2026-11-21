//! Directory and host environment rules (`[env]` in config.toml), the
//! `shell.env` hook. At shell start and on every cd, `valet hooks run chpwd`
//! prints the exports/unsets that take the shell from the rules it had to
//! the rules that match now.
//!
//! The shell keeps the state in the unexported `__valet_env_state` and hands
//! it over as `VALET_ENV_STATE`: the active rule names, and for each variable
//! the rules manage, the value the shell had before (to restore on leaving).
//! When the matching rules are unchanged nothing is printed, so values
//! changed by hand inside a rule stay.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

pub const STATE_ENV: &str = "VALET_ENV_STATE";
const STATE_VAR: &str = "__valet_env_state";

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Rules {
    #[serde(default)]
    default: Rule,
    #[serde(default, rename = "rule")]
    rules: Vec<Rule>,
    /// Set while the shell is inside any repo under `<Userland>/Repos`;
    /// values may also use {derived} (`<Userland>/DerivedData/<owner>/<repo>`):
    /// build output shared by the repo's worktrees, outside them.
    #[serde(default)]
    repo: BTreeMap<String, String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Rule {
    name: Option<String>,
    #[serde(default)]
    path: Vec<String>,
    /// `<host>/<owner>/<name>`: applies inside any checkout of that repo,
    /// whichever path spells it; `{root}` = the repo folder.
    repo: Option<String>,
    host: Option<String>,
    #[serde(default)]
    set: BTreeMap<String, String>,
    #[serde(default)]
    unset: Vec<String>,
}

/// What the rules want: `Some` = export, `None` = unset.
type Want = BTreeMap<String, Option<String>>;

/// Where the shell is.
struct Here<'a> {
    /// The shell's `$PWD` and the physical cwd (a rule may name either).
    dirs: Vec<PathBuf>,
    host: &'a str,
    home: &'a Path,
    /// Userland root ([`crate::cmd::apps::root`]).
    userland: &'a Path,
    arch: &'a str,
}

/// Check config.toml TEXT the way [`parse`] reads it.
pub fn validate(text: &str) -> Result<()> {
    parse(text).map(drop)
}

/// The `[env]` section of config.toml TEXT.
fn parse(text: &str) -> Result<Rules> {
    check(crate::config::section_of(text, "env")?)
}

fn check(r: Rules) -> Result<Rules> {
    if !r.default.path.is_empty() || r.default.host.is_some() {
        bail!("config.toml [env]: [default] takes no `path` or `host`");
    }
    for (i, rule) in r.rules.iter().enumerate() {
        if rule.path.is_empty() && rule.repo.is_none() && rule.host.is_none() {
            bail!(
                "config.toml [env]: rule {} needs `path`, `repo` or `host`",
                i + 1
            );
        }
        if !rule.path.is_empty() && rule.repo.is_some() {
            bail!(
                "config.toml [env]: rule {} takes `path` or `repo`, not both",
                i + 1
            );
        }
    }
    for rule in std::iter::once(&r.default).chain(&r.rules) {
        for k in rule.set.keys().chain(&rule.unset) {
            if !is_name(k) {
                bail!("config.toml [env]: bad variable name {k:?}");
            }
        }
    }
    Ok(r)
}

fn is_name(k: &str) -> bool {
    k.chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `*` within one string; linear (no backtracking blowup).
fn glob(pat: &str, s: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pat.chars().collect(), s.chars().collect());
    let (mut i, mut j, mut star, mut mark) = (0, 0, None, 0);
    while j < t.len() {
        if i < p.len() && p[i] != '*' && p[i] == t[j] {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == '*' {
            star = Some(i);
            mark = j;
            i += 1;
        } else if let Some(s) = star {
            i = s + 1;
            mark += 1;
            j = mark;
        } else {
            return false;
        }
    }
    p[i..].iter().all(|c| *c == '*')
}

/// The dir `pat` matches at or above `dir` (pattern parts against the
/// leading parts of `dir`).
fn match_dir(pat: &str, dir: &Path, home: &Path) -> Option<PathBuf> {
    let pat = match pat.strip_prefix("~") {
        Some(rest) => format!("{}{rest}", home.display()),
        None => pat.to_owned(),
    };
    let pp: Vec<_> = Path::new(&pat).components().collect();
    let dp: Vec<_> = dir.components().collect();
    if pp.len() > dp.len() {
        return None;
    }
    let mut root = PathBuf::new();
    for (a, b) in pp.iter().zip(&dp) {
        match (a, b) {
            (Component::Normal(a), Component::Normal(b)) => {
                if !glob(&a.to_string_lossy(), &b.to_string_lossy()) {
                    return None;
                }
            }
            (a, b) if a == b => {}
            _ => return None,
        }
        root.push(b);
    }
    Some(root)
}

/// Names of the matching rules (with `default` first) and what they want.
fn evaluate(r: &Rules, here: &Here) -> (Vec<String>, Want) {
    let mut names = vec!["default".to_owned()];
    let mut want = Want::new();
    apply(&r.default, None, here, &mut want);
    // [env.repo] before the rules, so a rule can override it for a path.
    if !r.repo.is_empty()
        && let Some(repo) = here.dirs.iter().find_map(|d| crate::repos::repo_of(d))
    {
        let owner_name = repo
            .slug
            .split_once('/')
            .map_or(repo.slug.as_str(), |(_, t)| t);
        let derived = here.userland.join("DerivedData").join(owner_name);
        let rule = Rule {
            set: r
                .repo
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        v.replace("{derived}", &derived.display().to_string()),
                    )
                })
                .collect(),
            ..Rule::default()
        };
        names.push(format!("repo:{}", repo.slug));
        apply(&rule, Some(&repo.dir), here, &mut want);
    }
    for (i, rule) in r.rules.iter().enumerate() {
        if let Some(h) = &rule.host
            && !glob(h, here.host)
        {
            continue;
        }
        let root = if let Some(slug) = &rule.repo {
            match here
                .dirs
                .iter()
                .find_map(|d| crate::repos::repo_of(d).filter(|r| &r.slug == slug))
            {
                Some(r) => Some(r.dir),
                None => continue,
            }
        } else if rule.path.is_empty() {
            None
        } else {
            let found = rule
                .path
                .iter()
                .find_map(|p| here.dirs.iter().find_map(|d| match_dir(p, d, here.home)));
            match found {
                Some(r) => Some(r),
                None => continue,
            }
        };
        names.push(
            rule.name
                .clone()
                .unwrap_or_else(|| format!("rule{}", i + 1)),
        );
        apply(rule, root.as_deref(), here, &mut want);
    }
    (names, want)
}

fn apply(rule: &Rule, root: Option<&Path>, here: &Here, want: &mut Want) {
    for k in &rule.unset {
        want.insert(k.clone(), None);
    }
    for (k, v) in &rule.set {
        let v = v
            .replace("{home}", &here.home.display().to_string())
            .replace("{userland}", &here.userland.display().to_string())
            .replace("{arch}", here.arch)
            .replace("{user}", &std::env::var("USER").unwrap_or_default())
            .replace(
                "{root}",
                &root.map(|r| r.display().to_string()).unwrap_or_default(),
            );
        want.insert(k.clone(), Some(v));
    }
}

/// Previous state: rule names, and per managed variable its value before
/// the rules (`None` = was unset). Lines: `rules a,b`, `var NAME HEX|-`.
struct State {
    rules: Vec<String>,
    orig: BTreeMap<String, Option<String>>,
}

fn hex(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<String> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect();
    String::from_utf8(bytes?).ok()
}

fn read_state(text: &str) -> State {
    let mut st = State {
        rules: Vec::new(),
        orig: BTreeMap::new(),
    };
    for line in text.lines() {
        let f: Vec<&str> = line.split(' ').collect();
        match f.as_slice() {
            ["rules", list] => st.rules = list.split(',').map(str::to_owned).collect(),
            ["var", name, val] if is_name(name) => {
                let v = if *val == "-" { None } else { unhex(val) };
                st.orig.insert((*name).to_owned(), v);
            }
            _ => {}
        }
    }
    st
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn emit(out: &mut String, k: &str, v: &Option<String>) {
    match v {
        Some(v) => out.push_str(&format!("export {k}={}\n", quote(v))),
        None => out.push_str(&format!("unset {k}\n")),
    }
}

/// Shell code taking the shell from `state` to the rules matching `here`;
/// empty when they are the same. `env` reads the shell's current values.
fn transition(r: &Rules, here: &Here, state: &str, env: &dyn Fn(&str) -> Option<String>) -> String {
    let prev = read_state(state);
    let (names, want) = evaluate(r, here);
    if prev.rules == names {
        return String::new();
    }
    let mut out = String::new();
    for (k, o) in &prev.orig {
        if !want.contains_key(k) {
            emit(&mut out, k, o);
        }
    }
    let mut st = format!("rules {}\n", names.join(","));
    for (k, v) in &want {
        let orig = match prev.orig.get(k) {
            Some(o) => o.clone(),
            None => env(k),
        };
        st.push_str(&format!(
            "var {k} {}\n",
            orig.as_deref().map(hex).unwrap_or_else(|| "-".into())
        ));
        emit(&mut out, k, v);
    }
    out.push_str(&format!("{STATE_VAR}={}\n", quote(&st)));
    out
}

/// Body of the shell.env hook: print the transition for this shell.
pub fn chpwd() -> Result<()> {
    let r = check(crate::config::section("env")?)?;
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .context("HOME is not set")?;
    let mut dirs = Vec::new();
    if let Some(p) = std::env::var_os("PWD").map(PathBuf::from)
        && p.is_absolute()
    {
        dirs.push(p);
    }
    if let Ok(c) = std::env::current_dir() {
        dirs.push(c);
    }
    // `path = ["~/..."]` rules match this host's spelling too (/srv/.../ws/x
    // is ~/ws/x).
    let through_home: Vec<PathBuf> = dirs
        .iter()
        .map(|d| crate::cmd::apps::portable(&crate::cmd::apps::canonical(d)))
        .collect();
    dirs.extend(through_home);
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
    let host = host.trim().split('.').next().unwrap_or_default();
    let userland = crate::cmd::apps::root()?;
    let here = Here {
        dirs,
        host,
        home: &home,
        userland: &userland,
        arch: crate::cmd::apps::this_arch(),
    };
    let state = std::env::var(STATE_ENV).unwrap_or_default();
    print!(
        "{}",
        transition(&r, &here, &state, &|k| std::env::var(k).ok())
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &str = r#"
[default]
unset = ["PYTHONHOME"]
[[rule]]
name = "repo"
path = ["~/ws/repo*"]
set = { FOO = "{root}/x", PYTHONHOME = "/py" }
[[rule]]
name = "arm"
host = "arm-*"
set = { BAR = "{arch}" }
"#;

    fn here<'a>(dir: &str, host: &'a str, home: &'a Path) -> Here<'a> {
        Here {
            dirs: vec![PathBuf::from(dir)],
            host,
            home,
            userland: Path::new("/u"),
            arch: "Arm64",
        }
    }

    /// Apply printed code to a fake env; returns the new state text.
    fn run(r: &Rules, h: &Here, state: &str, env: &mut BTreeMap<String, String>) -> String {
        let snapshot = env.clone();
        let out = transition(r, h, state, &|k| snapshot.get(k).cloned());
        // The state assignment is last and spans lines.
        let at = out.find(&format!("{STATE_VAR}=")).unwrap_or(out.len());
        for line in out[..at].lines() {
            if let Some(rest) = line.strip_prefix("export ") {
                let (k, v) = rest.split_once('=').unwrap();
                env.insert(k.into(), v.trim_matches('\'').into());
            } else if let Some(k) = line.strip_prefix("unset ") {
                env.remove(k);
            }
        }
        match out[at..].split_once('=') {
            Some((_, v)) => v.trim_end().trim_matches('\'').to_owned(),
            None => state.to_owned(),
        }
    }

    fn rules(t: &str) -> Result<Rules> {
        check(toml::from_str(t)?)
    }

    #[test]
    fn rules_apply_and_restore() {
        let r = rules(T).unwrap();
        let home = Path::new("/h");
        let mut env = BTreeMap::from([("PYTHONHOME".to_owned(), "/stray".to_owned())]);
        let st = run(&r, &here("/h", "vm", home), "", &mut env);
        assert!(!env.contains_key("PYTHONHOME"), "default unsets");
        let st = run(&r, &here("/h/ws/repo=b/src", "vm", home), &st, &mut env);
        assert_eq!(env["FOO"], "/h/ws/repo=b/x");
        assert_eq!(env["PYTHONHOME"], "/py");
        // Same rules: nothing printed, a hand edit stays.
        env.insert("FOO".into(), "mine".into());
        let st2 = run(
            &r,
            &here("/h/ws/repo=b/src/deeper", "vm", home),
            &st,
            &mut env,
        );
        assert_eq!((st2.as_str(), env["FOO"].as_str()), (st.as_str(), "mine"));
        // Leaving restores what default left: FOO gone, PYTHONHOME unset.
        run(&r, &here("/h/other", "vm", home), &st, &mut env);
        assert!(
            !env.contains_key("FOO") && !env.contains_key("PYTHONHOME"),
            "{env:?}"
        );
    }

    #[test]
    fn host_rules_and_placeholders() {
        let r = rules(T).unwrap();
        let mut env = BTreeMap::new();
        run(&r, &here("/", "arm-7", Path::new("/h")), "", &mut env);
        assert_eq!(env["BAR"], "Arm64");
    }

    #[test]
    fn glob_and_validation() {
        assert!(glob("repo*", "repo=main") && glob("*a*b", "xaab") && !glob("a*c", "abd"));
        assert!(
            rules("[[rule]]\nset = { A = \"1\" }").is_err(),
            "needs path/host"
        );
        assert!(rules("[default]\nunset = [\"A-B\"]").is_err(), "bad name");
        assert!(rules("[default]\nbogus = 1").is_err(), "unknown field");
        parse(crate::config::EMBEDDED).expect("[env] parses");
    }

    #[test]
    fn state_values_survive_quotes_and_newlines() {
        let r = rules("[default]\nset = { A = \"1\" }").unwrap();
        let mut env = BTreeMap::from([("A".to_owned(), "it's\nx".to_owned())]);
        let h = here("/", "vm", Path::new("/h"));
        let st = run(&r, &h, "", &mut env);
        assert_eq!(read_state(&st).orig["A"].as_deref(), Some("it's\nx"));
    }
}
