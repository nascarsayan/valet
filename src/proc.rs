//! Running external programs. Always argument arrays, never a shell string.
//! Anything that talks to the network gets a timeout.
//!
//! ```ignore
//! let out = Cmd::new("git").args(["status", "--short"]).cwd(repo).checked()?;
//! Cmd::new("gh").args(["api", "--input", "-", "repos/acme/widgets/issues"]).stdin(body).timeout(NET_TIMEOUT).checked()?;
//! Cmd::new("pytest").args(tests).env("PYTHONPATH", p).run()?; // inherits the terminal
//! ```

use anyhow::{Context, Result, bail};
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Default timeout for network-bound tools such as `gh`, `git`, `curl`.
pub const NET_TIMEOUT: Duration = Duration::from_secs(30);

/// Captured result of a finished program.
pub struct Output {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

/// Builder for one program invocation.
pub struct Cmd {
    program: OsString,
    args: Vec<OsString>,
    envs: Vec<(OsString, OsString)>,
    cwd: Option<PathBuf>,
    stdin: Option<Vec<u8>>,
    timeout: Option<Duration>,
    /// [`Cmd::status`]/[`Cmd::run`]: the child's stdout goes to our stderr.
    quiet_stdout: bool,
}

impl Cmd {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Cmd {
            program: program.as_ref().to_owned(),
            args: Vec::new(),
            envs: Vec::new(),
            cwd: None,
            stdin: None,
            timeout: None,
            quiet_stdout: false,
        }
    }

    /// Send the child's stdout to stderr, for commands run while ours prints
    /// a result to stdout (a path a shell function will `cd` to).
    pub fn stdout_to_stderr(mut self) -> Self {
        self.quiet_stdout = true;
        self
    }

    pub fn arg(mut self, a: impl AsRef<OsStr>) -> Self {
        self.args.push(a.as_ref().to_owned());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|a| a.as_ref().to_owned()));
        self
    }

    pub fn env(mut self, k: impl AsRef<OsStr>, v: impl AsRef<OsStr>) -> Self {
        self.envs
            .push((k.as_ref().to_owned(), v.as_ref().to_owned()));
        self
    }

    pub fn cwd(mut self, dir: impl AsRef<Path>) -> Self {
        self.cwd = Some(dir.as_ref().to_owned());
        self
    }

    /// Bytes fed to the program's stdin. Use this, not argv, for secrets.
    pub fn stdin(mut self, data: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(data.into());
        self
    }

    pub fn timeout(mut self, t: Duration) -> Self {
        self.timeout = Some(t);
        self
    }

    /// Short program name for messages.
    pub fn name(&self) -> String {
        Path::new(&self.program)
            .file_name()
            .unwrap_or(&self.program)
            .to_string_lossy()
            .into_owned()
    }

    /// Human-readable command line for logs and dry runs. Never pass secrets in argv.
    pub fn display(&self) -> String {
        std::iter::once(&self.program)
            .chain(&self.args)
            .map(|a| shell_words(&a.to_string_lossy()))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn command(&self) -> Command {
        let mut c = Command::new(&self.program);
        c.args(&self.args);
        for (k, v) in &self.envs {
            c.env(k, v);
        }
        if let Some(d) = &self.cwd {
            c.current_dir(d);
        }
        c
    }

    fn spawn_err(&self) -> String {
        format!("cannot run {} (try `valet doctor`)", self.name())
    }

    /// Run with captured stdout/stderr. Does not fail on non-zero exit.
    pub fn output(&self) -> Result<Output> {
        let mut child = self
            .command()
            .stdin(if self.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| self.spawn_err())?;

        let stdin_t = self.stdin.clone().map(|data| {
            let mut w = child.stdin.take().expect("piped stdin");
            // A program that exits without reading stdin closes the pipe; ignore that.
            thread::spawn(move || {
                let _ = w.write_all(&data);
            })
        });
        // Drain pipes on threads so a chatty child cannot block on a full pipe.
        let mut out = child.stdout.take().expect("piped stdout");
        let mut err = child.stderr.take().expect("piped stderr");
        let out_t = thread::spawn(move || {
            let mut b = Vec::new();
            out.read_to_end(&mut b).map(|_| b)
        });
        let err_t = thread::spawn(move || {
            let mut b = Vec::new();
            err.read_to_end(&mut b).map(|_| b)
        });

        let status = self.wait(&mut child)?;
        if let Some(t) = stdin_t {
            let _ = t.join();
        }
        let stdout = out_t.join().expect("stdout reader")?;
        let stderr = err_t.join().expect("stderr reader")?;
        Ok(Output {
            status,
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        })
    }

    /// Run captured; fail on non-zero exit (stderr in the error); return trimmed stdout.
    pub fn checked(&self) -> Result<String> {
        let o = self.output()?;
        if !o.status.success() {
            bail!(
                "{} failed ({}): {}",
                self.display(),
                o.status,
                o.stderr.trim()
            );
        }
        Ok(o.stdout.trim().to_owned())
    }

    /// Run with the terminal inherited (streams output, allows prompts).
    /// Returns the exit status without failing on non-zero.
    pub fn status(&self) -> Result<ExitStatus> {
        let mut c = self.command();
        if self.quiet_stdout {
            c.stdout(Stdio::from(std::io::stderr()));
        }
        if self.stdin.is_some() {
            c.stdin(Stdio::piped());
        }
        let mut child = c.spawn().with_context(|| self.spawn_err())?;
        if let Some(data) = &self.stdin {
            let mut w = child.stdin.take().expect("piped stdin");
            let _ = w.write_all(data);
        }
        self.wait(&mut child)
    }

    /// Like [`Cmd::status`] but fails on non-zero exit.
    pub fn run(&self) -> Result<()> {
        let s = self.status()?;
        if !s.success() {
            bail!("{} failed ({s})", self.display());
        }
        Ok(())
    }

    /// Start a long-running program (a `valet daemon` part): stdout and
    /// stderr piped, no stdin, in its own process group so stopping it stops
    /// what it started. No timeout: the supervisor owns its lifetime.
    pub fn spawn_piped(&self) -> Result<std::process::Child> {
        use std::os::unix::process::CommandExt;
        let mut c = self.command();
        c.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        c.spawn().with_context(|| self.spawn_err())
    }

    /// Replace the current process (for pass-through commands like ssh).
    pub fn exec(&self) -> Result<std::convert::Infallible> {
        use std::os::unix::process::CommandExt;
        let err = self.command().exec();
        Err(err).with_context(|| self.spawn_err())
    }

    fn wait(&self, child: &mut std::process::Child) -> Result<ExitStatus> {
        let Some(timeout) = self.timeout else {
            return Ok(child.wait()?);
        };
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!("{} timed out after {}s", self.name(), timeout.as_secs());
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Run `program args...`, wait at most `timeout`, and return trimmed stdout.
/// Fails on non-zero exit (stderr included in the error) or on timeout.
pub fn output(program: impl AsRef<OsStr>, args: &[&str], timeout: Duration) -> Result<String> {
    Cmd::new(program).args(args).timeout(timeout).checked()
}

/// True if `program args...` exits 0 (any failure to run counts as false).
pub fn succeeds(program: impl AsRef<OsStr>, args: &[&str], timeout: Duration) -> bool {
    Cmd::new(program)
        .args(args)
        .timeout(timeout)
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Quote one word for display in a POSIX shell (dry runs, logs).
pub fn shell_words(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c))
    {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Full path of `program` on PATH, if any.
pub fn which(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        return None;
    }
    // Userland's commands first: a parent (Claude Code, systemd, an old
    // shell) may run us with a PATH that lacks them.
    if let Ok(root) = crate::cmd::apps::root() {
        let p = root
            .join("Commands")
            .join(crate::cmd::apps::this_arch())
            .join(program);
        if is_executable(&p) {
            return Some(p);
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_returns_trimmed_stdout() {
        assert_eq!(output("echo", &["hi"], NET_TIMEOUT).unwrap(), "hi");
    }

    #[test]
    fn output_fails_on_nonzero_exit() {
        assert!(output("false", &[], NET_TIMEOUT).is_err());
    }

    #[test]
    fn output_times_out() {
        let err = output("sleep", &["5"], Duration::from_millis(200)).unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
    }

    #[test]
    fn stdin_env_cwd() {
        assert_eq!(Cmd::new("cat").stdin("abc").checked().unwrap(), "abc");
        assert_eq!(
            Cmd::new("sh")
                .args(["-c", "echo $X"])
                .env("X", "y")
                .checked()
                .unwrap(),
            "y"
        );
        assert_eq!(Cmd::new("pwd").cwd("/").checked().unwrap(), "/");
    }

    #[test]
    fn succeeds_reports_exit() {
        assert!(succeeds("true", &[], NET_TIMEOUT));
        assert!(!succeeds("false", &[], NET_TIMEOUT));
        assert!(!succeeds(
            "definitely-not-a-real-binary-xyz",
            &[],
            NET_TIMEOUT
        ));
    }

    #[test]
    fn display_quotes() {
        let c = Cmd::new("git").args(["commit", "-m", "it's ok"]);
        assert_eq!(c.display(), r"git commit -m 'it'\''s ok'");
    }

    #[test]
    fn which_finds_sh() {
        assert!(which("sh").is_some());
        assert!(which("definitely-not-a-real-binary-xyz").is_none());
    }
}
