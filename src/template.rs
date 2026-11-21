//! Renders the Jinja templates valet ships (`shell/rc/*.j2`) and the managed
//! home files, only the subset they use:
//!
//! - `{{ name }}`: a variable; undefined is an error.
//! - `{{ "text" }}` / `{{ 'text' }}`: a literal (how `{{` is written).
//! - `{% include "path" %}`: another file, relative to the including file's
//!   dir; it must stay inside the root. With [`render_files`], the name of
//!   another entry of the table.
//! - `{% if name %}...{% endif %}`: true when the variable is non-empty;
//!   undefined is an error.
//!
//! Anything else (other tags, filters, comments, whitespace control) is an
//! error, so a template change valet does not understand fails instead of
//! rendering wrong. As in Jinja's defaults, one trailing newline of each file
//! is dropped and block tags keep the text around them unchanged.

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::Path;

/// Template variables.
pub type Vars = BTreeMap<&'static str, String>;

/// Nested includes deeper than this are an error (catches include loops).
const MAX_DEPTH: usize = 8;

/// Render `file`; includes must resolve inside `root`.
pub fn render(file: &Path, root: &Path, vars: &Vars) -> Result<String> {
    let root = root
        .canonicalize()
        .with_context(|| format!("template root {}", root.display()))?;
    render_at(&Source::Dir(&root), &file.to_string_lossy(), vars, 0)
}

/// Render `name` from `files` (`(name, text)`, usually embedded with
/// `include_str!`); an include names another entry of `files`.
pub fn render_files(name: &str, files: &[(&str, &str)], vars: &Vars) -> Result<String> {
    render_at(&Source::Files(files), name, vars, 0)
}

/// Where templates and their includes come from.
enum Source<'a> {
    /// A directory; includes are relative to the including file.
    Dir(&'a Path),
    /// A fixed table; includes are table names.
    Files(&'a [(&'a str, &'a str)]),
}

impl Source<'_> {
    /// (key, text) of `file` (a path or table name).
    fn load(&self, file: &str) -> Result<(String, String)> {
        match self {
            Source::Dir(root) => {
                let path = Path::new(file)
                    .canonicalize()
                    .with_context(|| format!("template {file}"))?;
                if !path.starts_with(root) {
                    bail!("{} is outside {}", path.display(), root.display());
                }
                let src = std::fs::read_to_string(&path)
                    .with_context(|| format!("read {}", path.display()))?;
                Ok((path.to_string_lossy().into_owned(), src))
            }
            Source::Files(files) => files
                .iter()
                .find(|(n, _)| *n == file)
                .map(|(n, t)| ((*n).to_owned(), (*t).to_owned()))
                .with_context(|| format!("no template {file}")),
        }
    }

    /// The file an `include` of `rel` in `cur` names.
    fn resolve(&self, cur: &str, rel: &str) -> Result<String> {
        if Path::new(rel).is_absolute() {
            bail!("include path must be relative: {rel}");
        }
        match self {
            Source::Dir(_) => {
                let dir = Path::new(cur)
                    .parent()
                    .context("template has no parent dir")?;
                Ok(dir.join(rel).to_string_lossy().into_owned())
            }
            Source::Files(_) => Ok(rel.to_owned()),
        }
    }
}

fn render_at(src: &Source<'_>, file: &str, vars: &Vars, depth: usize) -> Result<String> {
    if depth > MAX_DEPTH {
        bail!("includes nested deeper than {MAX_DEPTH}");
    }
    let (key, text) = src.load(file)?;
    let text = text.strip_suffix('\n').unwrap_or(&text);
    let toks = lex(text).with_context(|| key.clone())?;
    let ctx = Ctx {
        src,
        cur: &key,
        vars,
        depth,
    };
    let mut out = String::new();
    let mut i = 0;
    render_seq(&toks, &mut i, &ctx, &mut out, false).with_context(|| key.clone())?;
    Ok(out)
}

struct Ctx<'a> {
    src: &'a Source<'a>,
    cur: &'a str,
    vars: &'a Vars,
    depth: usize,
}

#[derive(Debug, PartialEq)]
enum Tok<'a> {
    Text(&'a str),
    /// Inside `{{ }}`, trimmed.
    Expr(&'a str),
    /// Inside `{% %}`, trimmed.
    Stmt(&'a str),
}

fn lex(src: &str) -> Result<Vec<Tok<'_>>> {
    let mut toks = Vec::new();
    let mut rest = src;
    while let Some(at) = rest.find('{') {
        let open = &rest[at..];
        let close = if open.starts_with("{{") {
            "}}"
        } else if open.starts_with("{%") {
            "%}"
        } else if open.starts_with("{#") {
            bail!("comments ({{# #}}) are not supported");
        } else {
            // A lone `{` is text; keep scanning after it.
            let (head, tail) = rest.split_at(at + 1);
            push_text(&mut toks, head);
            rest = tail;
            continue;
        };
        push_text(&mut toks, &rest[..at]);
        let body = &open[2..];
        let end = find_close(body, close).with_context(|| format!("unclosed {}", &open[..2]))?;
        let inner = &body[..end];
        if inner.starts_with(['-', '+']) || inner.ends_with(['-', '+']) {
            bail!("whitespace control (`-`/`+`) is not supported");
        }
        let inner = inner.trim();
        toks.push(if close == "}}" {
            Tok::Expr(inner)
        } else {
            Tok::Stmt(inner)
        });
        rest = &body[end + 2..];
    }
    push_text(&mut toks, rest);
    Ok(toks)
}

/// Add a text token unless empty.
fn push_text<'a>(toks: &mut Vec<Tok<'a>>, s: &'a str) {
    if s.is_empty() {
        return;
    }
    toks.push(Tok::Text(s));
}

/// Index of `close` in `body`, skipping quoted strings (`{{ "}}" }}`).
fn find_close(body: &str, close: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (i, c) in body.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if body[i..].starts_with(close) => return Some(i),
            None => {}
        }
    }
    None
}

/// Render tokens from `*i`; inside an `if`, stop at its `endif` (left at `*i`).
fn render_seq(
    toks: &[Tok<'_>],
    i: &mut usize,
    ctx: &Ctx<'_>,
    out: &mut String,
    in_if: bool,
) -> Result<()> {
    while *i < toks.len() {
        match &toks[*i] {
            Tok::Text(t) => out.push_str(t),
            Tok::Expr(e) => out.push_str(&expr(e, ctx.vars)?),
            Tok::Stmt(s) => {
                let (kw, arg) = s.split_once(char::is_whitespace).unwrap_or((s, ""));
                let arg = arg.trim();
                match kw {
                    "include" => {
                        let rel = literal(arg).context("include needs one quoted path")?;
                        let file = ctx.src.resolve(ctx.cur, rel)?;
                        out.push_str(&render_at(ctx.src, &file, ctx.vars, ctx.depth + 1)?);
                    }
                    "if" => {
                        let cond = !var(arg, ctx.vars)?.is_empty();
                        *i += 1;
                        let mut body = String::new();
                        render_seq(toks, i, ctx, &mut body, true)?;
                        if cond {
                            out.push_str(&body);
                        }
                    }
                    "endif" if arg.is_empty() => {
                        if in_if {
                            return Ok(());
                        }
                        bail!("`endif` without `if`");
                    }
                    _ => bail!("unsupported tag {{% {s} %}}"),
                }
            }
        }
        *i += 1;
    }
    if in_if {
        bail!("`if` without `endif`");
    }
    Ok(())
}

fn expr(e: &str, vars: &Vars) -> Result<String> {
    if let Some(l) = literal(e) {
        return Ok(l.to_owned());
    }
    var(e, vars).map(str::to_owned)
}

/// A quoted string with no escapes or quotes inside.
fn literal(s: &str) -> Option<&str> {
    let q = s.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    let body = s.strip_prefix(q)?.strip_suffix(q)?;
    (!body.contains(['"', '\'', '\\'])).then_some(body)
}

fn var<'a>(name: &str, vars: &'a Vars) -> Result<&'a str> {
    let ident = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !ident {
        bail!("unsupported expression {name:?}");
    }
    vars.get(name)
        .map(String::as_str)
        .with_context(|| format!("undefined variable `{name}`"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tmp(tag: &str, files: &[(&str, &str)]) -> Tmp {
        let d = std::env::temp_dir().join(format!("valet-tmpl-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for (rel, body) in files {
            let p = d.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        Tmp(d)
    }

    fn vars() -> Vars {
        Vars::from([("name", "v".to_owned()), ("empty", String::new())])
    }

    fn r(t: &Tmp, file: &str) -> Result<String> {
        render(&t.0.join(file), &t.0, &vars())
    }

    #[test]
    fn variables_literals_and_escaped_braces() {
        let t = tmp(
            "vars",
            &[(
                "a.j2",
                "x={{ name }} {{\"lit\"}} '{{ \"{{\" }}.Host{{ \"}}\" }}' {y}\n",
            )],
        );
        assert_eq!(r(&t, "a.j2").unwrap(), "x=v lit '{{.Host}}' {y}");
    }

    #[test]
    fn if_blocks_keep_surrounding_text() {
        let t = tmp(
            "if",
            &[(
                "a.j2",
                "p=(a{% if name %} \"{{ name }}/bin\"{% endif %}{% if empty %} no{% endif %} b)\n",
            )],
        );
        assert_eq!(r(&t, "a.j2").unwrap(), "p=(a \"v/bin\" b)");
    }

    #[test]
    fn include_is_relative_and_drops_one_trailing_newline() {
        let t = tmp(
            "inc",
            &[
                (
                    "portable/a.j2",
                    "top\n{% include \"../zsh/b.zsh\" %}\nend\n",
                ),
                ("zsh/b.zsh", "inc {{ name }}\n"),
            ],
        );
        assert_eq!(r(&t, "portable/a.j2").unwrap(), "top\ninc v\nend");
    }

    #[test]
    fn unknown_constructs_and_undefined_variables_fail_closed() {
        for (body, want) in [
            ("{{ nope }}", "undefined variable `nope`"),
            ("{% if nope %}x{% endif %}", "undefined variable `nope`"),
            ("{{ name | upper }}", "unsupported expression"),
            ("{% for x in y %}{% endfor %}", "unsupported tag"),
            ("{# c #}", "comments"),
            ("{{- name }}", "whitespace control"),
            ("{% if name %}x", "`if` without `endif`"),
            ("{% endif %}", "`endif` without `if`"),
            ("{{ name", "unclosed"),
            ("{% include x %}", "include needs one quoted path"),
            ("{% include \"/etc/hostname\" %}", "must be relative"),
        ] {
            let t = tmp("bad", &[("a.j2", body)]);
            let err = format!("{:#}", r(&t, "a.j2").unwrap_err());
            assert!(err.contains(want), "{body}: {err}");
        }
    }

    #[test]
    fn include_cannot_leave_the_root() {
        let t = tmp(
            "esc",
            &[
                ("root/a.j2", "{% include \"../outside.zsh\" %}"),
                ("outside.zsh", "x"),
            ],
        );
        let err = format!(
            "{:#}",
            render(&t.0.join("root/a.j2"), &t.0.join("root"), &vars()).unwrap_err()
        );
        assert!(err.contains("is outside"), "{err}");
        let t = tmp("loop", &[("a.j2", "{% include \"a.j2\" %}")]);
        let err = format!("{:#}", r(&t, "a.j2").unwrap_err());
        assert!(err.contains("nested deeper"), "{err}");
    }

    #[test]
    fn table_includes_name_entries() {
        let files = [
            ("a.j2", "top\n{% include \"b.zsh\" %}\n"),
            ("b.zsh", "b {{ name }}\n"),
        ];
        assert_eq!(render_files("a.j2", &files, &vars()).unwrap(), "top\nb v");
        let err = format!("{:#}", render_files("x", &files, &vars()).unwrap_err());
        assert!(err.contains("no template x"), "{err}");
    }
}
