//! `valet repos list --raw`, `valet repos pick` and `valet cd`: list and
//! fuzzy-pick repositories, worktrees and folded branches under
//! `<Userland>/Repos` (see [`crate::repos`]).
//!
//! Branch names come from reading `.git`/`HEAD` directly rather than running
//! `git` per repo, which keeps the picker instant with hundreds of worktrees.

use crate::tools;
use anyhow::{Context, Result, bail};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(clap::Args)]
pub struct PickArgs {
    /// A place name (an alias from config.toml, claude-skills, memory) jumps
    /// straight there; anything else is the initial fzf query,
    /// and a unique match is selected without prompting.
    pub query: Option<String>,
    /// What to list: worktrees; + your branches ($USER/...); + everything
    /// (local and remote branches). In fzf: alt-w, alt-m, alt-a.
    #[arg(long, value_enum, default_value_t = Scope::Worktrees)]
    pub scope: Scope,
    /// Print the rows for --scope and exit (fzf's reload).
    #[arg(long, hide = true)]
    pub list: bool,
}

#[derive(clap::ValueEnum, Clone, Copy, PartialEq, PartialOrd, Debug)]
pub enum Scope {
    Worktrees,
    Mine,
    All,
}

/// Named entry points, offered before repos: every alias in config.toml, plus
/// these.
fn places() -> Vec<(String, PathBuf)> {
    let mut v: Vec<(String, PathBuf)> = crate::repos::manifest()
        .map(|m| m.aliases.into_keys().collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|n| crate::repos::alias_path(&n).map(|p| (n, p)))
        .collect();
    if let Ok(h) = super::claude_dir() {
        v.push(("claude-skills".into(), h.join("skills")));
        // Claude keys project dirs by launch directory: the newest memory dir.
        let newest = fs::read_dir(h.join("projects"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path().join("memory"))
            .filter(|p| p.is_dir())
            .max_by_key(|p| fs::metadata(p).and_then(|m| m.modified()).ok());
        if let Some(m) = newest {
            v.push(("memory".into(), m));
        }
    }
    v
}

/// `valet repos list --raw`: `<name>\t<branch>\t<path>` for every repo and worktree.
pub fn print_raw() -> Result<()> {
    for r in repos_list()? {
        println!("{}\t{}\t{}", r.name, r.branch, r.path.display());
    }
    Ok(())
}

struct Repo {
    name: String,
    /// Shown: branch plus what it is (`(branch)`, `(remote)`).
    branch: String,
    path: PathBuf,
}

/// Every repo's worktrees (for `valet repos list --raw`).
fn repos_list() -> Result<Vec<Repo>> {
    rows_for(Scope::Worktrees, None)
}

/// Rows for SCOPE, ONLY one repo when given: worktrees first, then your
/// branches (`$USER/...`), then the rest (local, then remote-only).
fn rows_for(scope: Scope, only: Option<&crate::repos::Repo>) -> Result<Vec<Repo>> {
    let me = format!("{}/", std::env::var("USER").unwrap_or_default());
    let repos: Vec<crate::repos::Repo> = match only {
        Some(r) => vec![r.clone()],
        None => crate::repos::all()?,
    };
    let mut tiers: [Vec<Repo>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    for r in &repos {
        let name = r
            .slug
            .split_once('/')
            .map_or(r.slug.as_str(), |x| x.1)
            .to_owned();
        let top = r.dir.join(crate::repos::TREE);
        let mut checked = Vec::new();
        worktree_dirs(&top, &top, &mut checked);
        checked.sort();
        for b in &checked {
            tiers[0].push(Repo {
                name: name.clone(),
                branch: b.clone(),
                path: top.join(b),
            });
        }
        if scope == Scope::Worktrees {
            continue;
        }
        let local = local_branches(&r.git_dir());
        for b in local.iter().filter(|b| !checked.contains(b)) {
            let row = Repo {
                name: name.clone(),
                branch: format!("{b} (branch)"),
                path: top.join(b),
            };
            if b.starts_with(&me) {
                tiers[1].push(row);
            } else if scope == Scope::All {
                tiers[2].push(row);
            }
        }
        for b in remote_branches(&r.git_dir())
            .iter()
            .filter(|b| !checked.contains(b) && !local.contains(b))
        {
            let row = Repo {
                name: name.clone(),
                branch: format!("{b} (remote)"),
                path: top.join(b),
            };
            if b.starts_with(&me) {
                tiers[1].push(row);
            } else if scope == Scope::All {
                tiers[2].push(row);
            }
        }
    }
    Ok(tiers.into_iter().flatten().collect())
}

/// origin's branch names from refs/remotes/origin and packed-refs.
fn remote_branches(git_dir: &Path) -> Vec<String> {
    let mut v = Vec::new();
    let base = git_dir.join("refs/remotes/origin");
    fn walk(base: &Path, d: &Path, v: &mut Vec<String>) {
        for e in fs::read_dir(d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(base, &p, v);
            } else if let Ok(rel) = p.strip_prefix(base) {
                v.push(rel.to_string_lossy().into_owned());
            }
        }
    }
    walk(&base, &base, &mut v);
    for line in fs::read_to_string(git_dir.join("packed-refs"))
        .unwrap_or_default()
        .lines()
    {
        if let Some((_, r)) = line.split_once(' ')
            && let Some(b) = r.strip_prefix("refs/remotes/origin/")
            && !v.iter().any(|x| x == b)
        {
            v.push(b.to_owned());
        }
    }
    v.retain(|b| b != "HEAD");
    v.sort();
    v
}

/// Branch paths (relative to `top`) of worktree dirs (holding a `.git` file).
fn worktree_dirs(top: &Path, dir: &Path, out: &mut Vec<String>) {
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        if p.join(".git").is_file() {
            if let Ok(rel) = p.strip_prefix(top) {
                out.push(rel.to_string_lossy().into_owned());
            }
        } else {
            worktree_dirs(top, &p, out);
        }
    }
}

/// Local branch names from refs/heads and packed-refs (no git process).
fn local_branches(git_dir: &Path) -> Vec<String> {
    let mut v = Vec::new();
    let heads = git_dir.join("refs/heads");
    fn walk(base: &Path, d: &Path, v: &mut Vec<String>) {
        for e in fs::read_dir(d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(base, &p, v);
            } else if let Ok(rel) = p.strip_prefix(base) {
                v.push(rel.to_string_lossy().into_owned());
            }
        }
    }
    walk(&heads, &heads, &mut v);
    for line in fs::read_to_string(git_dir.join("packed-refs"))
        .unwrap_or_default()
        .lines()
    {
        if let Some((_, r)) = line.split_once(' ')
            && let Some(b) = r.strip_prefix("refs/heads/")
            && !v.iter().any(|x| x == b)
        {
            v.push(b.to_owned());
        }
    }
    v.sort();
    v
}

/// Widest repo column; longer names lose characters on the left (`…`).
const NAME_MAX: usize = 20;

/// `name` cut to `w` characters from the left (`…` + its tail).
fn left_cut(name: &str, w: usize) -> String {
    let n = name.chars().count();
    if n <= w {
        return name.to_owned();
    }
    let tail: String = name.chars().skip(n - (w - 1)).collect();
    format!("…{tail}")
}

/// fzf rows: `<name> <branch>` shown (name column at most [`NAME_MAX`]
/// wide, one space after it; no name column when every row is one repo),
/// full path hidden in field 2.
fn rows(repos: &[Repo]) -> String {
    let one_repo = repos.windows(2).all(|w| w[0].name == w[1].name)
        && repos.first().is_some_and(|r| !r.name.starts_with('@'));
    let width = repos
        .iter()
        .map(|r| r.name.chars().count())
        .max()
        .unwrap_or(0)
        .min(NAME_MAX);
    repos
        .iter()
        .map(|r| {
            if one_repo {
                format!("{}\t{}\n", r.branch, r.path.display())
            } else {
                let name = left_cut(&r.name, width);
                format!("{name:width$} {}\t{}\n", r.branch, r.path.display())
            }
        })
        .collect()
}

/// Path of a named place.
pub fn place(name: &str) -> Result<PathBuf> {
    if crate::repos::manifest()?.aliases.contains_key(name) {
        return crate::repos::alias(name, None);
    }
    places()
        .into_iter()
        .find(|(n, _)| n == name)
        .map(|(_, p)| p)
        .with_context(|| format!("no place {name}"))
}

pub fn pick(args: PickArgs) -> Result<()> {
    let here = std::env::current_dir()
        .ok()
        .and_then(|d| crate::repos::repo_of(&d));
    let places = if here.is_some() { Vec::new() } else { places() };
    if let Some(q) = args.query.as_deref()
        && places.iter().any(|(n, _)| n == q)
    {
        println!("{}", place(q)?.display());
        return Ok(());
    }
    let mut repos: Vec<Repo> = places
        .into_iter()
        .map(|(n, p)| Repo {
            name: format!("@{n}"),
            branch: "place".into(),
            path: p,
        })
        .collect();
    repos.extend(rows_for(args.scope, here.as_ref())?);
    if args.list {
        print!("{}", rows(&repos));
        return Ok(());
    }
    let fzf = tools::resolve("fzf")?;
    let me = crate::generated::name();
    let reload = |s: &str| format!("reload({me} cd --list --scope {s})+change-prompt({s}> )");
    let scope_name = match args.scope {
        Scope::Worktrees => "worktrees",
        Scope::Mine => "mine",
        Scope::All => "all",
    };
    let mut fzf_args = vec![
        "--delimiter=\t".to_owned(),
        "--with-nth=1".to_owned(),
        "--no-sort".to_owned(),
        format!("--prompt={scope_name}> "),
        // One hint per line: a single line overflows narrow terminals.
        format!(
            "--header={}alt-w  worktrees\nalt-m  + {}/ branches\nalt-a  all branches",
            here.as_ref()
                .map(|r| format!("{}\n", r.slug))
                .unwrap_or_default(),
            std::env::var("USER").unwrap_or_default(),
        ),
        format!("--bind=alt-w:{}", reload("worktrees")),
        format!("--bind=alt-m:{}", reload("mine")),
        format!("--bind=alt-a:{}", reload("all")),
        // Places (`@name`): their path and contents; repos: the last commit.
        "--preview=case {1} in @*) echo {2}; echo; ls -A {2};; *) git -C {2} log -1 --stat --color=always 2>/dev/null;; esac".to_owned(),
        "--preview-window=right:30%:wrap".to_owned(),
    ];
    if let Some(q) = args.query {
        fzf_args.push(format!("--query={q}"));
        fzf_args.push("--select-1".to_owned());
    }
    // fzf draws on /dev/tty, so stdout stays clean for the chosen row.
    let mut child = Command::new(fzf)
        .args(&fzf_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("cannot run fzf")?;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(rows(&repos).as_bytes())?;
    let out = child.wait_with_output()?;
    match out.status.code() {
        Some(0) => {}
        // 1 = no match, 130 = cancelled: quiet, but non-zero so `cd` is skipped.
        Some(1 | 130) => std::process::exit(1),
        _ => bail!("fzf failed ({})", out.status),
    }
    let line = String::from_utf8(out.stdout)?;
    let path = PathBuf::from(line.trim_end().rsplit('\t').next().unwrap_or_default());
    // A branch without a worktree (local or remote): make it.
    if !path.exists()
        && let Some(r) = crate::repos::repo_of(&path)
        && let Ok(b) = path.strip_prefix(r.dir.join(crate::repos::TREE))
    {
        r.add(&b.to_string_lossy(), None)?;
    }
    println!("{}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_align_and_hide_path() {
        let r = |n: &str, b: &str, p: &str| Repo {
            name: n.into(),
            branch: b.into(),
            path: p.into(),
        };
        let out = rows(&[r("a", "main", "/x/a"), r("long", "dev", "/x/long")]);
        assert_eq!(out, "a    main\t/x/a\nlong dev\t/x/long\n");
        let one = rows(&[r("a", "main", "/x/a"), r("a", "dev", "/x/b")]);
        assert_eq!(one, "main\t/x/a\ndev\t/x/b\n");
        assert_eq!(left_cut("LaurentiuGabriel/learnscape", 10), "…earnscape");
        assert_eq!(left_cut("short", 10), "short");
    }
}
