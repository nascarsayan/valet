//! `valet vars`: named values exported into every shell,
//! secret or plain, grouped in folders.
//!
//! One store, `<Userland>/Applications/Valet/Data/Vars` (0700):
//!
//!   <group>/<NAME>.gpg   secret: encrypted with `gpg` to the keys in `recipients`
//!   <group>/<NAME>.txt   plain value: a 0600 file
//!   recipients           GPG key ids, one per line
//!
//! NAME is an ALL_CAPS variable name, unique across the store, exported
//! under that name; its first line is the value. A group is one optional
//! folder level (lowercase) that only organises: `acme/ACME_EMAIL`.
//!
//! gpg-agent caches the unlocked key (see ~/.gnupg/gpg-agent.conf), so the
//! passphrase is asked once per agent lifetime; `valet vars lock` forgets it.
//! Plain values export even while it is locked; secrets wait for an unlock.
//! Decrypted values are cached in RAM ($XDG_RUNTIME_DIR) for fast shell
//! start. Secret values go only to stdout (`get`, `env`) or a child's
//! environment (`exec`), never to argv or logs.

use crate::proc::{self, Cmd};
use anyhow::{Context, Result, bail};
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

pub const SHELL_ZSH: &str = "# pinentry needs to know the terminal\nexport GPG_TTY=$TTY";
pub const SHELL_BASH: &str = "# pinentry needs to know the terminal\nexport GPG_TTY=$(tty)";

#[derive(clap::Args)]
#[command(after_help = "\
Examples:
  valet vars set acme/ACME_API_TOKEN                 secret (hidden prompt, asked twice)
  valet vars set --plain acme/ACME_EMAIL alice@example.com
  gh auth token | valet vars set github/GH_TOKEN
  valet vars ls                                      every value, by group
  valet vars mv SSHPASS ssh                          regroup (`.` = no group)
  valet vars exec PW=SSHPASS -- sshpass -e ssh host
New shells export every value; in the current one: eval \"$(valet vars env)\"")]
pub struct Args {
    #[command(subcommand)]
    cmd: Sub,
}

#[derive(clap::Subcommand)]
enum Sub {
    /// List values by group; plain values are shown, secrets are not.
    Ls {
        #[arg(id = "vars_group", value_name = "GROUP")]
        group: Option<String>,
    },
    /// Print a value (a secret's whole decrypted entry).
    Get {
        #[arg(id = "vars_name", value_name = "NAME")]
        name: String,
    },
    /// Add or update [GROUP/]NAME (ALL_CAPS). A secret is read from a hidden
    /// prompt or stdin, never the command line; --plain takes VALUE too.
    ///
    /// An existing NAME keeps its group unless GROUP is given; switching
    /// between secret and plain replaces the old file.
    Set {
        #[arg(id = "vars_name", value_name = "[GROUP/]NAME")]
        spec: String,
        /// Plain value (with --plain only).
        value: Option<String>,
        /// Store as a plain 0600 file instead of encrypting.
        #[arg(long)]
        plain: bool,
    },
    /// Move NAME into GROUP (`.` = no group).
    Mv {
        #[arg(id = "vars_name", value_name = "NAME")]
        name: String,
        #[arg(id = "vars_group", value_name = "GROUP")]
        group: String,
    },
    /// Delete NAME (asks for confirmation unless --force).
    Rm {
        #[arg(id = "vars_name", value_name = "NAME")]
        name: String,
        #[arg(short, long)]
        force: bool,
    },
    /// Run a command with values in its environment under other names.
    ///
    /// Example: valet vars exec SSHPASS=HOST_ROOT_PASSWORD -- sshpass -e ssh host
    Exec {
        /// VAR=NAME pairs.
        #[arg(required = true, value_name = "VAR=NAME")]
        pairs: Vec<String>,
        /// Command and arguments, after `--`.
        #[arg(last = true, required = true, value_name = "CMD")]
        cmd: Vec<String>,
    },
    /// Print `export NAME=value` for every value, for `eval`.
    Env {
        /// If gpg-agent is locked, export only plain values instead of
        /// prompting (shell start).
        #[arg(long)]
        if_unlocked: bool,
        /// Print only the variable names.
        #[arg(long, conflicts_with = "plain")]
        names: bool,
        /// Print raw `NAME=value` lines (no `export`, no quoting), e.g. for a .env file.
        #[arg(long)]
        plain: bool,
    },
    /// Unlock now (one passphrase prompt) so new shells get the secrets.
    Unlock,
    /// Show whether gpg-agent holds the unlocked key, and every name.
    Status,
    /// Forget the cached unlock now; the next access asks for the passphrase.
    Lock,
    /// Completion candidates (`names` or `groups`), as `word:description`.
    #[command(hide = true)]
    Complete { kind: String },
}

pub fn run(args: Args) -> Result<()> {
    let changes = matches!(
        args.cmd,
        Sub::Set { .. } | Sub::Mv { .. } | Sub::Rm { .. } | Sub::Lock | Sub::Unlock
    );
    if changes {
        drop_cache();
    }
    let result = run_sub(args.cmd);
    if changes {
        mark_changed();
    }
    result
}

fn run_sub(cmd: Sub) -> Result<()> {
    match cmd {
        Sub::Ls { group } => ls(group.as_deref()),
        Sub::Get { name } => {
            print!("{}", content(&find(&name)?)?);
            Ok(())
        }
        Sub::Set { spec, value, plain } => set(&spec, value, plain),
        Sub::Mv { name, group } => {
            let e = find(&name)?;
            let group = (group != ".").then_some(group);
            if let Some(g) = &group {
                valid_group(g)?;
            }
            let to = Entry { group, ..e.clone() };
            if to.group == e.group {
                println!("{} is already there", e.rel());
                return Ok(());
            }
            let store = store_dir()?;
            make_dir(&to.file(&store))?;
            std::fs::rename(e.file(&store), to.file(&store))?;
            remove_empty_group(&store, &e);
            println!("moved {} -> {}", e.rel(), to.rel());
            Ok(())
        }
        Sub::Rm { name, force } => {
            let e = find(&name)?;
            if !force && !confirm(&format!("delete {} ({})?", e.rel(), e.kind()))? {
                bail!("not deleted");
            }
            let store = store_dir()?;
            std::fs::remove_file(e.file(&store))?;
            remove_empty_group(&store, &e);
            println!("deleted {}", e.rel());
            Ok(())
        }
        Sub::Exec { pairs, cmd } => exec(&pairs, &cmd),
        Sub::Env {
            if_unlocked,
            names,
            plain,
        } => env(if_unlocked, names, plain),
        Sub::Unlock => {
            let first = entries()?
                .into_iter()
                .find(|e| e.secret)
                .context("no secrets to unlock with")?;
            content(&first)?;
            println!(
                "unlocked; new shells will export {} value(s)",
                entries()?.len()
            );
            Ok(())
        }
        Sub::Status => status(),
        Sub::Lock => {
            gnupg("gpgconf")?
                .args(["--reload", "gpg-agent"])
                .timeout(proc::NET_TIMEOUT)
                .run()?;
            println!("gpg-agent cache cleared; next access asks for the passphrase");
            Ok(())
        }
        Sub::Complete { kind } => {
            let es = entries()?;
            match kind.as_str() {
                "names" => {
                    for e in &es {
                        println!(
                            "{}:{} {}",
                            e.name,
                            e.kind(),
                            e.group.as_deref().unwrap_or("")
                        );
                    }
                }
                "groups" => {
                    let mut gs: Vec<&str> = es.iter().filter_map(|e| e.group.as_deref()).collect();
                    gs.sort();
                    gs.dedup();
                    for g in gs {
                        println!("{g}:group");
                    }
                }
                _ => bail!("complete: names or groups"),
            }
            Ok(())
        }
    }
}

// ---------- entries ----------

#[derive(Clone, Debug, PartialEq)]
struct Entry {
    group: Option<String>,
    name: String,
    secret: bool,
}

impl Entry {
    fn rel(&self) -> String {
        match &self.group {
            Some(g) => format!("{g}/{}", self.name),
            None => self.name.clone(),
        }
    }
    fn file(&self, store: &Path) -> PathBuf {
        let ext = if self.secret { "gpg" } else { "txt" };
        store.join(format!("{}.{ext}", self.rel()))
    }
    fn kind(&self) -> &'static str {
        if self.secret { "secret" } else { "plain" }
    }
}

/// `<Userland>/Applications/Valet/Data/Vars`.
fn store_dir() -> Result<PathBuf> {
    Ok(crate::cmd::apps::self_dir("Data")?.join("Vars"))
}

/// The store's GPG recipients (`recipients`: one key id per line).
fn recipients(store: &Path) -> Result<Vec<String>> {
    let p = store.join("recipients");
    let ids: Vec<String> = std::fs::read_to_string(&p)
        .with_context(|| format!("read {} (GPG key ids, one per line)", p.display()))?
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect();
    if ids.is_empty() {
        bail!("{} lists no GPG key", p.display());
    }
    Ok(ids)
}

/// Push DIR's `NAME.gpg` / `NAME.txt` entries (ALL_CAPS names) onto OUT;
/// returns its subfolder names.
fn entries_in(dir: &Path, group: Option<&str>, out: &mut Vec<Entry>) -> Vec<String> {
    let mut subdirs = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let Some(f) = e.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if f.starts_with('.') {
            continue;
        }
        let Ok(t) = e.file_type() else { continue };
        if t.is_dir() {
            subdirs.push(f);
            continue;
        }
        let (stem, secret) = match (f.strip_suffix(".gpg"), f.strip_suffix(".txt")) {
            (Some(s), _) => (s, true),
            (_, Some(s)) => (s, false),
            _ => continue,
        };
        if is_env_name(stem) {
            out.push(Entry {
                group: group.map(str::to_owned),
                name: stem.to_owned(),
                secret,
            });
        }
    }
    subdirs
}

/// Every value in STORE (top level and one group level), sorted by name. A
/// NAME found twice is reported and the first (by path) kept.
fn entries_at(store: &Path) -> Vec<Entry> {
    let mut v = Vec::new();
    let mut groups = entries_in(store, None, &mut v);
    groups.sort();
    for g in groups.iter().filter(|g| valid_group(g).is_ok()) {
        entries_in(&store.join(g), Some(g), &mut v);
    }
    v.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.rel().cmp(&b.rel())));
    let mut out: Vec<Entry> = Vec::with_capacity(v.len());
    for e in v {
        if let Some(prev) = out.last()
            && prev.name == e.name
        {
            crate::warn!("{} duplicates {}; ignored", e.rel(), prev.rel());
            continue;
        }
        out.push(e);
    }
    out
}

fn entries() -> Result<Vec<Entry>> {
    Ok(entries_at(&store_dir()?))
}

/// The entry for NAME or GROUP/NAME.
fn find(spec: &str) -> Result<Entry> {
    let (group, name) = parse_spec(spec)?;
    entries()?
        .into_iter()
        .find(|e| e.name == name && (group.is_none() || e.group.as_deref() == group))
        .with_context(|| format!("no value {spec} (`valet vars ls`)"))
}

/// `[GROUP/]NAME` -> (GROUP, NAME), both validated.
fn parse_spec(spec: &str) -> Result<(Option<&str>, &str)> {
    let (group, name) = match spec.split_once('/') {
        Some((g, n)) => (Some(g), n),
        None => (None, spec),
    };
    if let Some(g) = group {
        valid_group(g)?;
    }
    if !is_env_name(name) {
        bail!(
            "{name:?}: names are ALL_CAPS variable names, e.g. {}",
            caps_name(name).unwrap_or_else(|| "GITHUB_TOKEN".into())
        );
    }
    Ok((group, name))
}

/// Group folders: `[a-z0-9][a-z0-9._-]*`.
fn valid_group(g: &str) -> Result<()> {
    let ok = g.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && g.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c));
    if !ok {
        bail!("invalid group {g:?}: lowercase letters, digits, . _ -");
    }
    Ok(())
}

fn valid_var(var: &str) -> Result<()> {
    let ok = !var.is_empty()
        && !var.starts_with(|c: char| c.is_ascii_digit())
        && var.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !ok {
        bail!("invalid environment variable name {var:?}");
    }
    Ok(())
}

/// ALL_CAPS variable name: `[A-Z_][A-Z0-9_]*`.
fn is_env_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Flat ALL_CAPS form of a name (`a/b-c.d` -> `A_B_C_D`), if valid.
fn caps_name(name: &str) -> Option<String> {
    let n: String = name
        .chars()
        .map(|c| {
            if matches!(c, '-' | '.' | '/') {
                '_'
            } else {
                c.to_ascii_uppercase()
            }
        })
        .collect();
    is_env_name(&n).then_some(n)
}

// ---------- reading and writing ----------

/// PROGRAM (a GnuPG tool) with the GnuPG app's environment (GNUPGHOME).
fn gnupg(program: &str) -> Result<Cmd> {
    let mut c = Cmd::new(program);
    for (k, v) in crate::cmd::apps::app_env("GnuPG")? {
        c = c.env(k, v);
    }
    Ok(c)
}

/// `gpg` with the GnuPG app's environment (GNUPGHOME) and GPG_TTY set when
/// we know the terminal, so pinentry can prompt even from shells that did
/// not export it.
fn gpg() -> Result<Cmd> {
    let mut c = gnupg("gpg")?.args([
        "--quiet",
        "--yes",
        "--compress-algo=none",
        "--no-encrypt-to",
    ]);
    if std::env::var_os("GPG_TTY").is_none()
        && let Some(tty) = terminal()
    {
        c = c.env("GPG_TTY", tty);
    }
    Ok(c)
}

/// Encrypt BODY to the store's recipients into FILE (atomically, 0600).
fn encrypt(store: &Path, file: &Path, body: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    make_dir(file)?;
    let name = file.file_name().context("no file name")?.to_string_lossy();
    let tmp = file.with_file_name(format!(".{name}.{}", std::process::id()));
    let mut c = gpg()?.args(["--batch", "--encrypt"]);
    for r in recipients(store)? {
        c = c.arg("--recipient").arg(r);
    }
    let out = c.arg("--output").arg(&tmp).stdin(body).output()?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&tmp);
        bail!("gpg --encrypt failed: {}", out.stderr.trim());
    }
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&tmp, file)?;
    Ok(())
}

/// The controlling terminal's device path, if stdin/stderr is one.
fn terminal() -> Option<String> {
    ["/proc/self/fd/0", "/proc/self/fd/2"]
        .iter()
        .filter_map(|fd| std::fs::read_link(fd).ok())
        .map(|p| p.to_string_lossy().into_owned())
        .find(|p| p.starts_with("/dev/pts/") || p.starts_with("/dev/tty"))
}

/// The entry's whole content (a passphrase prompt may appear for a secret).
fn content(e: &Entry) -> Result<String> {
    if !e.secret {
        let p = e.file(&store_dir()?);
        return std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()));
    }
    let out = gpg()?
        .arg("--decrypt")
        .arg(e.file(&store_dir()?))
        .output()?;
    if !out.status.success() {
        bail!("gpg --decrypt {} failed: {}", e.rel(), out.stderr.trim());
    }
    Ok(out.stdout)
}

/// First line of an entry: the exported value.
fn first_line(content: &str) -> &str {
    content.lines().next().unwrap_or_default()
}

/// The value of NAME (first line), for other valet commands. Never log it.
pub fn value(name: &str) -> Result<String> {
    Ok(first_line(&content(&find(name)?)?).to_owned())
}

/// Whether the store has NAME (no decryption).
pub fn has(name: &str) -> bool {
    find(name).is_ok()
}

fn set(spec: &str, value: Option<String>, plain: bool) -> Result<()> {
    let (group, name) = parse_spec(spec)?;
    if value.is_some() && !plain {
        bail!(
            "secret values never go on the command line: omit VALUE to be prompted, or pipe it in"
        );
    }
    let old = entries()?.into_iter().find(|e| e.name == name);
    let new = Entry {
        group: match group {
            Some(g) => Some(g.to_owned()),
            None => old.as_ref().and_then(|e| e.group.clone()),
        },
        name: name.to_owned(),
        secret: !plain,
    };
    let store = store_dir()?;
    if plain {
        let v = match value {
            Some(v) => v,
            None => read_plain(name)?,
        };
        if v.is_empty() || v.contains('\n') {
            bail!("a plain value is one non-empty line");
        }
        write_private(&new.file(&store), &format!("{v}\n"))?;
    } else {
        let v = read_secret(name)?;
        if v.trim().is_empty() {
            bail!("empty secret; nothing stored");
        }
        encrypt(&store, &new.file(&store), &v)?;
    }
    if let Some(o) = &old
        && o != &new
    {
        std::fs::remove_file(o.file(&store))?;
        remove_empty_group(&store, o);
    }
    println!(
        "{} {} ({}; exported in your shells at the next prompt)",
        if old.is_some() { "updated" } else { "added" },
        new.rel(),
        new.kind()
    );
    Ok(())
}

/// A secret: typed twice without echo on a terminal (one line), else all of
/// stdin (up to 64 KiB, may be multi-line).
fn read_secret(name: &str) -> Result<String> {
    if !std::io::stdin().is_terminal() {
        let mut s = String::new();
        std::io::stdin().take(64 * 1024).read_to_string(&mut s)?;
        return Ok(format!("{}\n", s.trim_end_matches(['\n', '\r'])));
    }
    let ask = |q: &str| -> Result<String> {
        eprint!("{q}");
        std::io::stderr().flush()?;
        Cmd::new("stty").arg("-echo").status()?;
        let mut s = String::new();
        let r = std::io::stdin().read_line(&mut s);
        let _ = Cmd::new("stty").arg("echo").status();
        eprintln!();
        r?;
        Ok(s.trim_end_matches(['\n', '\r']).to_owned())
    };
    let a = ask(&format!("{name} (secret): "))?;
    if ask(&format!("{name} again: "))? != a {
        bail!("the two entries differ; nothing stored");
    }
    Ok(format!("{a}\n"))
}

/// One visible line from the terminal, else stdin (up to 64 KiB).
fn read_plain(name: &str) -> Result<String> {
    let mut s = String::new();
    if std::io::stdin().is_terminal() {
        eprint!("{name}: ");
        std::io::stderr().flush()?;
        std::io::stdin().read_line(&mut s)?;
    } else {
        std::io::stdin().take(64 * 1024).read_to_string(&mut s)?;
    }
    Ok(s.trim_end_matches(['\n', '\r']).to_owned())
}

fn make_dir(file: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let dir = file.parent().context("no parent dir")?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("create {}", dir.display()))
}

/// Write atomically with mode 0600 (new dirs 0700).
fn write_private(path: &Path, body: &str) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    make_dir(path)?;
    let dir = path.parent().context("no parent dir")?;
    let name = path.file_name().context("no file name")?.to_string_lossy();
    let tmp = dir.join(format!(".{name}.{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(body.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Remove E's group folder once it holds nothing.
fn remove_empty_group(store: &Path, e: &Entry) {
    if let Some(g) = &e.group {
        let _ = std::fs::remove_dir(store.join(g));
    }
}

fn confirm(q: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!("{q} needs a terminal to confirm; pass --force");
    }
    eprint!("{q} [y/N] ");
    std::io::stderr().flush()?;
    let mut a = String::new();
    std::io::stdin().read_line(&mut a)?;
    Ok(matches!(a.trim(), "y" | "Y" | "yes"))
}

// ---------- commands ----------

fn ls(group: Option<&str>) -> Result<()> {
    if let Some(g) = group {
        valid_group(g)?;
    }
    let mut es: Vec<Entry> = entries()?
        .into_iter()
        .filter(|e| group.is_none() || e.group.as_deref() == group)
        .collect();
    es.sort_by(|a, b| a.group.cmp(&b.group).then_with(|| a.name.cmp(&b.name)));
    let width = es.iter().map(|e| e.name.len()).max().unwrap_or(0);
    let store = store_dir()?;
    let mut current: Option<Option<String>> = None;
    for e in &es {
        if current.as_ref() != Some(&e.group) {
            println!("{}", e.group.as_deref().unwrap_or("(no group)"));
            current = Some(e.group.clone());
        }
        let shown = if e.secret {
            String::new()
        } else {
            std::fs::read_to_string(e.file(&store))
                .map(|c| first_line(&c).to_owned())
                .unwrap_or_default()
        };
        println!("  {:width$}  {:6}  {shown}", e.name, e.kind());
    }
    Ok(())
}

fn exec(pairs: &[String], cmd: &[String]) -> Result<()> {
    if pairs.is_empty() || cmd.is_empty() {
        bail!("usage: valet vars exec VAR=NAME... -- CMD [ARGS]");
    }
    let mut c = Cmd::new(&cmd[0]).args(&cmd[1..]);
    for p in pairs {
        let (var, name) = parse_pair(p)?;
        // Trailing newlines dropped, like $(valet vars get NAME).
        let v = content(&find(name)?)?;
        c = c.env(var, v.trim_end_matches('\n'));
    }
    match c.exec()? {}
}

fn parse_pair(p: &str) -> Result<(&str, &str)> {
    let (var, name) = p
        .split_once('=')
        .with_context(|| format!("expected VAR=NAME, got {p:?}"))?;
    valid_var(var)?;
    parse_spec(name)?;
    Ok((var, name))
}

fn unlocked() -> Result<bool> {
    let out = gnupg("gpg-connect-agent")?
        .args(["keyinfo --list", "/bye"])
        .timeout(proc::NET_TIMEOUT)
        .checked()?;
    Ok(cached_keys(&out) > 0)
}

fn env(if_unlocked: bool, names_only: bool, plain: bool) -> Result<()> {
    let es = entries()?;
    if names_only {
        for e in &es {
            println!("{}", e.name);
        }
        return Ok(());
    }
    let store = store_dir()?;
    let mut vars: Vec<(String, String)> = es
        .iter()
        .filter(|e| !e.secret)
        .filter_map(|e| {
            let c = std::fs::read_to_string(e.file(&store)).ok()?;
            Some((e.name.clone(), first_line(&c).to_owned()))
        })
        .collect();
    let secrets: Vec<Entry> = es.iter().filter(|e| e.secret).cloned().collect();
    // Locked at shell start: plain values only, silently (output here breaks
    // p10k's instant prompt; `valet vars status` tells).
    let is_unlocked = !secrets.is_empty() && unlocked().unwrap_or(false);
    if !secrets.is_empty() && (is_unlocked || !if_unlocked) {
        let cache = cache_path();
        let cached = match (&cache, is_unlocked) {
            (Some(c), true) => read_cache(c, &store),
            _ => None,
        };
        let got = match cached {
            Some(v) => v,
            None => {
                let got = decrypt_all(&secrets);
                if let Some(c) = &cache
                    && got.len() == secrets.len()
                {
                    // Best effort: a failed cache write only costs speed.
                    let _ = write_cache(c, &got);
                }
                got
            }
        };
        vars.extend(got);
    }
    vars.sort();
    // Every stored name, exported or not: a locked secret is not unset (a
    // shell may have inherited it); only names gone from the store are.
    let names: Vec<String> = es.iter().map(|e| e.name.clone()).collect();
    if !plain {
        for gone in removed_names(
            &std::env::var("VALET_SECRET_NAMES").unwrap_or_default(),
            &names,
        ) {
            println!("unset {gone}");
        }
        println!(
            "export VALET_SECRET_NAMES={}",
            proc::shell_words(&names.join(" "))
        );
    }
    for (n, v) in vars {
        if plain {
            println!("{n}={v}");
        } else {
            println!("export {n}={}", proc::shell_words(&v));
        }
    }
    Ok(())
}

/// Names exported earlier (`$VALET_SECRET_NAMES`) that are no longer in the
/// store; only valid variable names, so the output is safe to eval.
fn removed_names<'a>(previous: &'a str, current: &[String]) -> Vec<&'a str> {
    previous
        .split_whitespace()
        .filter(|n| is_env_name(n) && !current.iter().any(|c| c == n))
        .collect()
}

/// Decrypt every secret: the first alone (it may prompt), the rest in
/// parallel. Failures are reported and skipped.
fn decrypt_all(es: &[Entry]) -> Vec<(String, String)> {
    let one = |e: &Entry| content(e).map(|c| first_line(&c).to_owned());
    let mut results = vec![one(&es[0])];
    std::thread::scope(|s| {
        let handles: Vec<_> = es[1..].iter().map(|e| s.spawn(move || one(e))).collect();
        results.extend(
            handles
                .into_iter()
                .map(|h| h.join().expect("decrypt thread")),
        );
    });
    es.iter()
        .zip(results)
        .filter_map(|(e, r)| match r {
            Ok(v) => Some((e.name.clone(), v)),
            Err(err) => {
                eprintln!("valet: skipping {}: {err:#}", e.rel());
                None
            }
        })
        .collect()
}

fn status() -> Result<()> {
    let out = gnupg("gpg-connect-agent")?
        .args(["keyinfo --list", "/bye"])
        .timeout(proc::NET_TIMEOUT)
        .checked()?;
    let cached = cached_keys(&out);
    let total = out.lines().filter(|l| l.starts_with("S KEYINFO ")).count();
    if cached > 0 {
        println!("unlocked: {cached} of {total} key(s) cached in gpg-agent");
    } else {
        println!(
            "locked: shells export plain values only; `valet vars unlock` ({total} key(s) known)"
        );
    }
    let es = entries()?;
    let names = |secret: bool| {
        es.iter()
            .filter(|e| e.secret == secret)
            .map(|e| e.name.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    };
    println!("secret: {}", names(true));
    println!("plain:  {}", names(false));
    Ok(())
}

/// Count `S KEYINFO <grip> <type> <serial> <idstr> <cached> ...` lines with cached=1.
fn cached_keys(keyinfo: &str) -> usize {
    keyinfo
        .lines()
        .filter_map(|l| l.strip_prefix("S KEYINFO "))
        .filter(|rest| rest.split_whitespace().nth(4) == Some("1"))
        .count()
}

// ---------- RAM cache ----------
//
// Decrypting costs ~0.25 s per entry (gpg-agent re-derives the key from the
// cached passphrase each time), so shells read decrypted secrets from
// $XDG_RUNTIME_DIR/valet/secrets.env: tmpfs, private to the user, wiped at
// reboot. Used only while gpg-agent is unlocked and only if newer than the
// store; `set`/`mv`/`rm`/`lock` delete it.

/// Cache file, if a private runtime dir exists (no cache otherwise).
fn cache_path() -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let dir = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty())?);
    let m = std::fs::metadata(&dir).ok()?;
    (m.is_dir() && m.uid() == uid()? && m.mode() & 0o077 == 0)
        .then(|| dir.join("valet/secrets.env"))
}

/// Real uid of this process, from /proc.
pub(crate) fn uid() -> Option<u32> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    s.lines()
        .find_map(|l| l.strip_prefix("Uid:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Newest mtime among the store dir, its group dirs and their entries.
fn store_mtime(store: &Path) -> Option<std::time::SystemTime> {
    let mut newest = std::fs::metadata(store).ok()?.modified().ok()?;
    for e in std::fs::read_dir(store).ok()?.filter_map(|e| e.ok()) {
        let Ok(m) = e.metadata() else { continue };
        if let Ok(t) = m.modified() {
            newest = newest.max(t);
        }
        if m.is_dir() && !e.file_name().to_string_lossy().starts_with('.') {
            for f in std::fs::read_dir(e.path()).into_iter().flatten().flatten() {
                if let Ok(t) = f.metadata().and_then(|m| m.modified()) {
                    newest = newest.max(t);
                }
            }
        }
    }
    Some(newest)
}

/// Cached vars if the file is ours, 0600, and newer than the store.
fn read_cache(path: &Path, store: &Path) -> Option<Vec<(String, String)>> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::symlink_metadata(path).ok()?;
    if !m.is_file() || m.uid() != uid()? || m.mode() & 0o777 != 0o600 {
        return None;
    }
    if m.modified().ok()? < store_mtime(store)? {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    parse_bundle(&text).ok()
}

fn write_cache(path: &Path, vars: &[(String, String)]) -> Result<()> {
    let body: String = vars.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
    write_private(path, &body)
}

/// Change marker the shell hook polls (with `read`, no fork) to reload the
/// exports in already-open shells after a change.
fn changed_path() -> Option<PathBuf> {
    cache_path().map(|c| c.with_file_name("secrets.changed"))
}

fn mark_changed() {
    let Some(p) = changed_path() else { return };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    // Best effort: without it, only new shells see the change.
    if make_dir(&p).is_ok() {
        let _ = std::fs::write(&p, format!("{stamp}\n"));
    }
}

fn drop_cache() {
    if let Some(c) = cache_path() {
        let _ = std::fs::remove_file(c);
    }
}

/// `NAME=value` per line; blank lines and `#` comments ignored.
fn parse_bundle(text: &str) -> Result<Vec<(String, String)>> {
    let mut vars = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let (k, v) = line
            .split_once('=')
            .with_context(|| format!("bundle line {} is not NAME=value", i + 1))?;
        valid_var(k)?;
        vars.push((k.to_owned(), v.to_owned()));
    }
    Ok(vars)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("valet-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn removed_names_only_valid_and_gone() {
        let cur = vec!["A_TOKEN".to_owned(), "SSHPASS".to_owned()];
        assert_eq!(
            removed_names("A_TOKEN OLD_KEY bad;name SSHPASS", &cur),
            vec!["OLD_KEY"]
        );
        assert!(removed_names("", &cur).is_empty());
    }

    #[test]
    fn specs_and_groups_are_validated() {
        assert_eq!(parse_spec("GH_TOKEN").unwrap(), (None, "GH_TOKEN"));
        assert_eq!(
            parse_spec("acme/ACME_EMAIL").unwrap(),
            (Some("acme"), "ACME_EMAIL")
        );
        for bad in [
            "", "gh_token", "a/b/C", "../X", "A/X", "-g/X", "g/", "/X", "g x/X",
        ] {
            assert!(parse_spec(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn env_names_are_all_caps() {
        for ok in ["GITHUB_TOKEN", "A", "_X1", "SSHPASS"] {
            assert!(is_env_name(ok), "{ok}");
        }
        for bad in ["", "github_token", "Mixed", "1X", "A-B", "env/shell", "A.B"] {
            assert!(!is_env_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn caps_names_for_set_hint() {
        assert_eq!(
            caps_name("acme-api-token").as_deref(),
            Some("ACME_API_TOKEN")
        );
        assert_eq!(caps_name("1abc"), None);
    }

    #[test]
    fn entries_cover_groups_kinds_and_duplicates() {
        let d = tmp("store");
        for dir in ["acme", "ssh", ".git", "Bad"] {
            std::fs::create_dir_all(d.join(dir)).unwrap();
        }
        for f in [
            "GH_TOKEN.gpg",
            "lower.gpg",
            "acme/ACME_EMAIL.txt",
            "acme/ACME_API_TOKEN.gpg",
            "ssh/SSHPASS.gpg",
            "ssh/GH_TOKEN.txt",
            "NOTES",
            ".git/X.gpg",
            "Bad/Y.gpg",
            ".gpg-id",
        ] {
            std::fs::write(d.join(f), "x").unwrap();
        }
        let es = entries_at(&d);
        let got: Vec<(String, bool)> = es.iter().map(|e| (e.rel(), e.secret)).collect();
        assert_eq!(
            got,
            vec![
                ("acme/ACME_API_TOKEN".into(), true),
                ("acme/ACME_EMAIL".into(), false),
                ("GH_TOKEN".into(), true),
                ("ssh/SSHPASS".into(), true),
            ]
        );
        assert_eq!(es[1].file(&d), d.join("acme/ACME_EMAIL.txt"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn bundle_parsing() {
        let v = parse_bundle("# c\nA=1\n\nB=x=y\n").unwrap();
        assert_eq!(
            v,
            vec![("A".into(), "1".into()), ("B".into(), "x=y".into())]
        );
        assert!(parse_bundle("nonsense\n").is_err());
        assert!(parse_bundle("1A=x\n").is_err());
    }

    #[test]
    fn first_line_is_the_value() {
        assert_eq!(first_line("s3cret\nuser: me\n"), "s3cret");
        assert_eq!(first_line(""), "");
    }

    #[test]
    fn pairs_are_parsed() {
        assert_eq!(
            parse_pair("SSHPASS=ssh/HOST").unwrap(),
            ("SSHPASS", "ssh/HOST")
        );
        for bad in ["SSHPASS", "=x", "1X=X", "A-B=X", "X=../Y", "X=lower"] {
            assert!(parse_pair(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn exec_requires_pairs_and_command() {
        assert!(exec(&["A=B".into()], &[]).is_err());
        assert!(exec(&[], &["true".into()]).is_err());
    }

    #[test]
    fn private_files_and_cache_staleness() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp("cache");
        let store = d.join("store");
        std::fs::create_dir_all(store.join("g")).unwrap();
        std::fs::write(store.join("g/A.gpg"), "x").unwrap();
        let cache = d.join("run/dev/secrets.env");
        let vars = vec![
            ("A".to_owned(), "v 1='x'".to_owned()),
            ("B".to_owned(), "=y".to_owned()),
        ];
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_cache(&cache, &vars).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&cache), 0o600);
        assert_eq!(mode(cache.parent().unwrap()), 0o700);
        assert_eq!(read_cache(&cache, &store), Some(vars.clone()));

        // Group/world-readable cache is ignored.
        std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(read_cache(&cache, &store), None);
        std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o600)).unwrap();

        // A newer entry inside a group makes the cache stale.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(store.join("g/B.gpg"), "x").unwrap();
        assert_eq!(read_cache(&cache, &store), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn keyinfo_cache_flag() {
        let s = "S KEYINFO AAAA D - - 1 P - - -\nS KEYINFO BBBB D - - - P - - -\nOK\n";
        assert_eq!(cached_keys(s), 1);
        assert_eq!(cached_keys("OK\n"), 0);
    }
}
