//! Markdown (CommonMark + GitHub tables, strikethrough, task lists; parsed
//! with pulldown-cmark, so any valid markdown works) -> Slack Block Kit.
//!
//! Headings -> `header`; rules -> `divider`; tables -> `table` (too big:
//! an aligned monospace grid); paragraphs, lists, quotes and short code ->
//! `rich_text` (code highlighted when Slack knows its language); code
//! longer than [`SNIPPET_LINES`] lines -> a collapsible file snippet
//! ([`Item::Snippet`]). [`messages`] splits the result per Slack's limits.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use serde_json::{Value, json};

/// Code blocks longer than this become file snippets.
pub const SNIPPET_LINES: usize = 10;
/// Per message: Slack's block cap; a text budget well under its limits;
/// one table.
const MAX_BLOCKS: usize = 50;
const MAX_CHARS: usize = 11_000;
/// Per rich_text block, so no one block nears Slack's text limits.
const MAX_RT_CHARS: usize = 3_000;
/// Table limits: rows, cells per row, characters.
const MAX_ROWS: usize = 100;
const MAX_COLS: usize = 20;
const MAX_TABLE_CHARS: usize = 9_000;
/// Slack's header text cap.
const MAX_HEADER: usize = 150;

/// One piece of a converted message.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    Block(Value),
    /// Long code: uploaded as a file snippet (collapsible in Slack).
    Snippet {
        lang: Option<String>,
        code: String,
    },
}

/// Slack's name for a code fence's language (```` ```rs ````), or None when
/// the fence names none or Slack would not know it (then: no highlighting).
pub fn language(info: &str) -> Option<&'static str> {
    let w = info.split_whitespace().next()?;
    let w = w.trim_matches(|c| c == '{' || c == '}' || c == '.');
    let w = w
        .strip_prefix("language-")
        .unwrap_or(w)
        .to_ascii_lowercase();
    Some(match w.as_str() {
        "rs" | "rust" => "rust",
        "py" | "python" | "python3" => "python",
        "js" | "jsx" | "mjs" | "javascript" => "javascript",
        "ts" | "tsx" | "typescript" => "typescript",
        "sh" | "bash" | "zsh" | "shell" | "console" => "bash",
        "json" | "jsonc" => "json",
        "yml" | "yaml" => "yaml",
        "toml" => "toml",
        "go" | "golang" => "go",
        "c" | "h" => "c",
        "cpp" | "c++" | "cc" | "hpp" => "cpp",
        "cs" | "csharp" => "csharp",
        "java" => "java",
        "kt" | "kotlin" => "kotlin",
        "rb" | "ruby" => "ruby",
        "php" => "php",
        "swift" => "swift",
        "scala" => "scala",
        "sql" => "sql",
        "html" | "xml" | "svg" => "html",
        "css" | "scss" => "css",
        "diff" | "patch" => "diff",
        "md" | "markdown" => "markdown",
        "dockerfile" | "docker" => "dockerfile",
        "makefile" | "make" => "makefile",
        "lua" => "lua",
        "perl" | "pl" => "perl",
        "r" => "r",
        "groovy" => "groovy",
        "proto" | "protobuf" => "protobuf",
        "ps1" | "powershell" => "powershell",
        "graphql" | "gql" => "graphql",
        "tf" | "hcl" | "terraform" => "hcl",
        _ => return None,
    })
}

/// File extension for a snippet in LANG (Slack picks the snippet type from it).
pub fn extension(lang: Option<&str>) -> &'static str {
    match lang.unwrap_or_default() {
        "rust" => "rs",
        "python" => "py",
        "javascript" => "js",
        "typescript" => "ts",
        "bash" => "sh",
        "json" => "json",
        "yaml" => "yaml",
        "toml" => "toml",
        "go" => "go",
        "c" => "c",
        "cpp" => "cpp",
        "csharp" => "cs",
        "java" => "java",
        "kotlin" => "kt",
        "ruby" => "rb",
        "php" => "php",
        "swift" => "swift",
        "scala" => "scala",
        "sql" => "sql",
        "html" => "html",
        "css" => "css",
        "diff" => "diff",
        "markdown" => "md",
        "dockerfile" => "dockerfile",
        "makefile" => "mk",
        "lua" => "lua",
        "perl" => "pl",
        "r" => "r",
        "groovy" => "groovy",
        "protobuf" => "proto",
        "powershell" => "ps1",
        "graphql" => "graphql",
        "hcl" => "tf",
        _ => "txt",
    }
}

#[derive(Default)]
struct Style {
    bold: u32,
    italic: u32,
    strike: u32,
}

struct ListCtx {
    ordered: bool,
    /// Number of the next item (ordered lists may start anywhere).
    next: u64,
    indent: usize,
    /// The list element being filled (several when nested lists interrupt).
    open: Option<Value>,
    /// The current item's text was already emitted (a nested list followed it).
    emitted: bool,
    /// The current item is a done task: its text is struck through.
    strike: bool,
}

struct TableCtx {
    aligns: Vec<Alignment>,
    rows: Vec<Vec<Vec<Value>>>,
    row: Vec<Vec<Value>>,
}

#[derive(Default)]
struct Conv {
    items: Vec<Item>,
    /// Elements of the rich_text block being built, and their text size.
    rt: Vec<Value>,
    rt_chars: usize,
    /// Inline elements of the current paragraph, item, heading or cell.
    inline: Vec<Value>,
    style: Style,
    link: Option<(String, Vec<Value>)>,
    lists: Vec<ListCtx>,
    quote: usize,
    quote_buf: Vec<Value>,
    code: Option<(Option<&'static str>, String)>,
    table: Option<TableCtx>,
}

fn text_of(e: &[Value]) -> String {
    e.iter()
        .map(|x| x["text"].as_str().or(x["url"].as_str()).unwrap_or_default())
        .collect()
}

fn elems(v: &Value) -> &[Value] {
    v.as_array().map_or(&[][..], Vec::as_slice)
}

fn chars(e: &[Value]) -> usize {
    text_of(e).chars().count()
}

/// Drop empty text elements (Slack refuses them); None when nothing is left.
fn nonempty(mut e: Vec<Value>) -> Option<Vec<Value>> {
    e.retain(|x| x["type"] != "text" || x["text"].as_str().is_some_and(|t| !t.is_empty()));
    (!e.is_empty()).then_some(e)
}

fn space() -> Vec<Value> {
    vec![json!({ "type": "text", "text": " " })]
}

impl Conv {
    fn push_text(&mut self, t: &str, code: bool) {
        if t.is_empty() {
            return;
        }
        let mut st = serde_json::Map::new();
        if self.style.bold > 0 {
            st.insert("bold".into(), json!(true));
        }
        if self.style.italic > 0 {
            st.insert("italic".into(), json!(true));
        }
        if self.style.strike > 0 {
            st.insert("strike".into(), json!(true));
        }
        if code {
            st.insert("code".into(), json!(true));
        }
        let style = (!st.is_empty()).then_some(Value::Object(st));
        let buf = match &mut self.link {
            Some((_, b)) => b,
            None => &mut self.inline,
        };
        // Merge with the previous run of the same style.
        if let Some(last) = buf.last_mut()
            && last["type"] == "text"
            && last.get("style") == style.as_ref()
        {
            let joined = format!("{}{t}", last["text"].as_str().unwrap_or_default());
            last["text"] = json!(joined);
            return;
        }
        let mut e = json!({ "type": "text", "text": t });
        if let Some(s) = style {
            e["style"] = s;
        }
        buf.push(e);
    }

    fn push_rt(&mut self, e: Value) {
        let n = if e["type"] == "rich_text_list" {
            elems(&e["elements"])
                .iter()
                .map(|x| chars(elems(&x["elements"])))
                .sum()
        } else {
            chars(elems(&e["elements"]))
        };
        if self.rt_chars + n > MAX_RT_CHARS {
            self.flush_rt();
        }
        self.rt_chars += n;
        self.rt.push(e);
    }

    fn flush_rt(&mut self) {
        if !self.rt.is_empty() {
            let els = std::mem::take(&mut self.rt);
            self.items
                .push(Item::Block(json!({ "type": "rich_text", "elements": els })));
        }
        self.rt_chars = 0;
    }

    /// Close every open list element (into rt), keeping the list contexts.
    fn close_lists(&mut self) {
        let open: Vec<Value> = self
            .lists
            .iter_mut()
            .filter_map(|l| l.open.take())
            .collect();
        for l in open {
            self.push_rt(l);
        }
    }

    fn block(&mut self, b: Value) {
        self.close_lists();
        self.flush_rt();
        self.items.push(Item::Block(b));
    }

    /// End of a paragraph (or HTML block, footnote) outside tables.
    fn end_para(&mut self) {
        let inl = std::mem::take(&mut self.inline);
        if self.quote > 0 {
            if !self.quote_buf.is_empty() {
                self.quote_buf.push(json!({ "type": "text", "text": "\n" }));
            }
            self.quote_buf.extend(inl);
            return;
        }
        if !self.lists.is_empty() {
            // A loose list item's paragraphs stay in the item, on new lines.
            self.inline = inl;
            self.push_text("\n", false);
            return;
        }
        let Some(mut inl) = nonempty(inl) else { return };
        // A blank line between consecutive paragraphs.
        if self
            .rt
            .last()
            .is_some_and(|l| l["type"] == "rich_text_section")
        {
            inl.insert(0, json!({ "type": "text", "text": "\n" }));
        }
        self.push_rt(json!({ "type": "rich_text_section", "elements": inl }));
    }

    /// The current list item's text, into its list element.
    fn emit_item(&mut self) {
        let mut inl = std::mem::take(&mut self.inline);
        while inl.last().is_some_and(|l| {
            l["type"] == "text" && l["text"].as_str().is_some_and(|t| t.trim().is_empty())
        }) {
            inl.pop();
        }
        if let Some(last) = inl.last_mut()
            && last["type"] == "text"
        {
            let t = last["text"]
                .as_str()
                .unwrap_or_default()
                .trim_end()
                .to_owned();
            last["text"] = json!(t);
        }
        let Some(l) = self.lists.last_mut() else {
            return;
        };
        let inl = nonempty(inl).unwrap_or_else(space);
        let (ordered, next, indent) = (l.ordered, l.next, l.indent);
        let el = l.open.get_or_insert_with(|| {
            let mut v = json!({
                "type": "rich_text_list",
                "style": if ordered { "ordered" } else { "bullet" },
                "indent": indent.min(8),
                "elements": [],
            });
            if ordered && next > 1 {
                v["offset"] = json!(next - 1);
            }
            v
        });
        el["elements"]
            .as_array_mut()
            .expect("array")
            .push(json!({ "type": "rich_text_section", "elements": inl }));
        l.next += 1;
    }

    fn code_end(&mut self) {
        let Some((lang, mut code)) = self.code.take() else {
            return;
        };
        while code.ends_with('\n') {
            code.pop();
        }
        if self.quote > 0 || self.table.is_some() {
            self.push_text(&format!("{code}\n"), true);
            return;
        }
        self.close_lists();
        if code.lines().count() > SNIPPET_LINES {
            self.flush_rt();
            self.items.push(Item::Snippet {
                lang: lang.map(str::to_owned),
                code,
            });
            return;
        }
        let body = if code.is_empty() {
            " ".to_owned()
        } else {
            code
        };
        let mut pre = json!({
            "type": "rich_text_preformatted",
            "elements": [{ "type": "text", "text": body }],
        });
        if let Some(l) = lang {
            pre["language"] = json!(l);
        }
        self.push_rt(pre);
    }

    fn table_end(&mut self) {
        let Some(t) = self.table.take() else { return };
        let n = t.rows.iter().map(Vec::len).max().unwrap_or(0);
        if n == 0 {
            return;
        }
        let size: usize = t.rows.iter().flatten().map(|c| chars(c)).sum();
        if t.rows.len() > MAX_ROWS || n > MAX_COLS || size > MAX_TABLE_CHARS {
            // Too big for a table block: an aligned monospace grid.
            let rows: Vec<Vec<String>> = t
                .rows
                .iter()
                .map(|r| r.iter().map(|c| text_of(c)).collect())
                .collect();
            let grid = super::render::grid(&rows, true).join("\n");
            self.flush_rt();
            self.push_rt(json!({
                "type": "rich_text_preformatted",
                "elements": [{ "type": "text", "text": grid }],
            }));
            return;
        }
        let rows: Vec<Value> = t
            .rows
            .iter()
            .map(|r| {
                (0..n)
                    .map(|i| {
                        let c = r.get(i).cloned().and_then(nonempty).unwrap_or_else(space);
                        json!({ "type": "rich_text", "elements": [{ "type": "rich_text_section", "elements": c }] })
                    })
                    .collect()
            })
            .collect();
        let settings: Vec<Value> = (0..n)
            .map(|i| {
                let a = match t.aligns.get(i) {
                    Some(Alignment::Center) => "center",
                    Some(Alignment::Right) => "right",
                    _ => "left",
                };
                json!({ "align": a, "is_wrapped": true })
            })
            .collect();
        self.block(json!({ "type": "table", "rows": rows, "column_settings": settings }));
    }

    fn event(&mut self, ev: Event) {
        if let Some((_, code)) = &mut self.code {
            match ev {
                Event::Text(t) => code.push_str(&t),
                Event::End(TagEnd::CodeBlock) => self.code_end(),
                _ => {}
            }
            return;
        }
        match ev {
            Event::Text(t) => self.push_text(&t, false),
            Event::Code(t) | Event::InlineMath(t) | Event::DisplayMath(t) => {
                self.push_text(&t, true)
            }
            Event::Html(t) | Event::InlineHtml(t) => self.push_text(&t, false),
            Event::SoftBreak => self.push_text(" ", false),
            Event::HardBreak => self.push_text("\n", false),
            // Task items: plain bullets, done ones struck through (survives
            // copy, edit and forward, unlike Slack's markdown checkboxes).
            Event::TaskListMarker(done) => {
                if done && let Some(l) = self.lists.last_mut() {
                    l.strike = true;
                    self.style.strike += 1;
                }
            }
            Event::FootnoteReference(l) => self.push_text(&format!("[{l}]"), false),
            Event::Rule => self.block(json!({ "type": "divider" })),
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::BlockQuote(_) => {
                if self.quote == 0 {
                    self.close_lists();
                }
                self.quote += 1;
            }
            Tag::CodeBlock(kind) => {
                let lang = match &kind {
                    CodeBlockKind::Fenced(info) => language(info),
                    CodeBlockKind::Indented => None,
                };
                if self.quote == 0 && !self.lists.is_empty() && !self.inline.is_empty() {
                    // Code inside a list item: the item's text goes first.
                    self.emit_item();
                    if let Some(l) = self.lists.last_mut() {
                        l.emitted = true;
                    }
                }
                self.code = Some((lang, String::new()));
            }
            Tag::List(start) => {
                if self.quote > 0 {
                    return;
                }
                if let Some(p) = self.lists.last_mut()
                    && std::mem::take(&mut p.strike)
                {
                    self.style.strike = self.style.strike.saturating_sub(1);
                }
                if let Some(parent) = self.lists.last()
                    && !parent.emitted
                {
                    // A nested list: the parent item's text comes first.
                    self.emit_item();
                    if let Some(p) = self.lists.last_mut() {
                        p.emitted = true;
                    }
                }
                self.close_lists();
                let indent = self.lists.len();
                self.lists.push(ListCtx {
                    ordered: start.is_some(),
                    next: start.unwrap_or(1),
                    indent,
                    open: None,
                    emitted: false,
                    strike: false,
                });
            }
            Tag::Item => {
                if self.quote > 0 {
                    if !self.quote_buf.is_empty() || !self.inline.is_empty() {
                        self.push_text("\n", false);
                    }
                    self.push_text("• ", false);
                } else if let Some(l) = self.lists.last_mut() {
                    l.emitted = false;
                }
            }
            Tag::Table(aligns) => {
                self.close_lists();
                self.table = Some(TableCtx {
                    aligns,
                    rows: Vec::new(),
                    row: Vec::new(),
                });
            }
            Tag::Emphasis => self.style.italic += 1,
            Tag::Strong => self.style.bold += 1,
            Tag::Strikethrough => self.style.strike += 1,
            Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                self.link = Some((dest_url.to_string(), Vec::new()));
            }
            Tag::FootnoteDefinition(l) => self.push_text(&format!("[{l}]: "), false),
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::HtmlBlock | TagEnd::FootnoteDefinition => {
                if self.table.is_none() {
                    self.end_para();
                }
            }
            TagEnd::Heading(_) => {
                let t: String = text_of(&std::mem::take(&mut self.inline))
                    .trim()
                    .chars()
                    .take(MAX_HEADER)
                    .collect();
                if !t.is_empty() {
                    self.block(json!({ "type": "header", "text": { "type": "plain_text", "text": t, "emoji": true } }));
                }
            }
            TagEnd::BlockQuote(_) => {
                self.quote = self.quote.saturating_sub(1);
                if self.quote == 0 {
                    let mut q = std::mem::take(&mut self.quote_buf);
                    q.extend(std::mem::take(&mut self.inline));
                    if let Some(q) = nonempty(q) {
                        self.push_rt(json!({ "type": "rich_text_quote", "elements": q }));
                    }
                }
            }
            TagEnd::Item => {
                if let Some(l) = self.lists.last_mut()
                    && std::mem::take(&mut l.strike)
                {
                    self.style.strike = self.style.strike.saturating_sub(1);
                }
                if self.quote > 0 {
                    return;
                }
                let has_text = !text_of(&self.inline).trim().is_empty();
                let emitted = self.lists.last().is_some_and(|l| l.emitted);
                if has_text || !emitted {
                    self.emit_item();
                } else {
                    self.inline.clear();
                }
            }
            TagEnd::List(_) => {
                if self.quote > 0 {
                    return;
                }
                if let Some(open) = self.lists.pop().and_then(|mut l| l.open.take()) {
                    self.push_rt(open);
                }
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                if let Some(t) = &mut self.table {
                    let row = std::mem::take(&mut t.row);
                    t.rows.push(row);
                }
            }
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.inline);
                if let Some(t) = &mut self.table {
                    t.row.push(cell);
                }
            }
            TagEnd::Table => self.table_end(),
            TagEnd::Emphasis => self.style.italic = self.style.italic.saturating_sub(1),
            TagEnd::Strong => self.style.bold = self.style.bold.saturating_sub(1),
            TagEnd::Strikethrough => self.style.strike = self.style.strike.saturating_sub(1),
            TagEnd::Link | TagEnd::Image => {
                if let Some((url, inner)) = self.link.take() {
                    let label = text_of(&inner);
                    let mut e = json!({ "type": "link", "url": url });
                    if !label.is_empty() && label != url {
                        e["text"] = json!(label);
                    }
                    if let Some(st) = inner.first().and_then(|x| x.get("style")) {
                        e["style"] = st.clone();
                    }
                    self.inline.push(e);
                }
            }
            _ => {}
        }
    }
}

/// Markdown S as Block Kit items.
pub fn items(s: &str) -> Vec<Item> {
    let s = super::render::tilde(s);
    let opts = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    let mut c = Conv::default();
    for ev in Parser::new_ext(&s, opts) {
        c.event(ev);
    }
    c.close_lists();
    c.flush_rt();
    c.items
}

/// What to send: messages of blocks, and snippets between them.
#[derive(Debug, PartialEq)]
pub enum Msg {
    Blocks(Vec<Value>),
    Snippet { lang: Option<String>, code: String },
}

fn block_chars(b: &Value) -> usize {
    let mut n = 0;
    let mut stack = vec![b];
    while let Some(v) = stack.pop() {
        match v {
            Value::String(s) => n += s.chars().count(),
            Value::Array(a) => stack.extend(a),
            Value::Object(o) => stack.extend(o.values()),
            _ => {}
        }
    }
    n
}

/// ITEMS grouped into messages within Slack's per-message limits.
pub fn messages(items: Vec<Item>) -> Vec<Msg> {
    let mut out = Vec::new();
    let (mut cur, mut size, mut tables): (Vec<Value>, usize, usize) = (Vec::new(), 0, 0);
    for it in items {
        match it {
            Item::Snippet { lang, code } => {
                if !cur.is_empty() {
                    out.push(Msg::Blocks(std::mem::take(&mut cur)));
                    (size, tables) = (0, 0);
                }
                out.push(Msg::Snippet { lang, code });
            }
            Item::Block(b) => {
                let n = block_chars(&b);
                let table = b["type"] == "table";
                if !cur.is_empty()
                    && (cur.len() >= MAX_BLOCKS || size + n > MAX_CHARS || (table && tables > 0))
                {
                    out.push(Msg::Blocks(std::mem::take(&mut cur)));
                    (size, tables) = (0, 0);
                }
                size += n;
                tables += usize::from(table);
                cur.push(b);
            }
        }
    }
    if !cur.is_empty() {
        out.push(Msg::Blocks(cur));
    }
    out
}

/// Rich-text ELEMENTS as mrkdwn.
fn mrkdwn(elements: &Value) -> String {
    let esc = super::render::esc;
    let mut s = String::new();
    for e in elems(elements) {
        if e["type"] == "emoji" {
            s.push_str(&format!(":{}:", e["name"].as_str().unwrap_or_default()));
            continue;
        }
        let t = e["text"].as_str().unwrap_or_default();
        if e["type"] == "link" {
            let u = e["url"].as_str().unwrap_or_default();
            s.push_str(&if t.is_empty() {
                format!("<{u}>")
            } else {
                format!("<{u}|{}>", esc(t))
            });
            continue;
        }
        let t = esc(t);
        let st = &e["style"];
        s.push_str(&if st["code"] == true {
            format!("`{t}`")
        } else if st["bold"] == true {
            format!("*{t}*")
        } else if st["italic"] == true {
            format!("_{t}_")
        } else if st["strike"] == true {
            format!("~{t}~")
        } else {
            t
        });
    }
    s
}

/// BLOCKS as mrkdwn text: the fallback when Slack refuses the blocks.
pub fn to_mrkdwn(blocks: &[Value]) -> String {
    let esc = super::render::esc;
    let mut out: Vec<String> = Vec::new();
    for b in blocks {
        match b["type"].as_str().unwrap_or_default() {
            "header" => out.push(format!(
                "*{}*",
                esc(b["text"]["text"].as_str().unwrap_or_default())
            )),
            "divider" => out.push("───".into()),
            "markdown" => out.push(super::render::md(b["text"].as_str().unwrap_or_default())),
            "table" => {
                let rows: Vec<Vec<String>> = elems(&b["rows"])
                    .iter()
                    .map(|r| {
                        elems(r)
                            .iter()
                            .map(|c| text_of(elems(&c["elements"][0]["elements"])))
                            .collect()
                    })
                    .collect();
                let g = super::render::grid(&rows, true);
                out.push(format!("```\n{}\n```", esc(&g.join("\n"))));
            }
            "rich_text" => {
                for e in elems(&b["elements"]) {
                    match e["type"].as_str().unwrap_or_default() {
                        "rich_text_preformatted" => out.push(format!(
                            "```\n{}\n```",
                            esc(&text_of(elems(&e["elements"])))
                        )),
                        "rich_text_quote" => {
                            for l in mrkdwn(&e["elements"]).split('\n') {
                                out.push(format!("> {l}"));
                            }
                        }
                        "rich_text_list" => {
                            let pad = "    ".repeat(e["indent"].as_u64().unwrap_or(0) as usize);
                            let first = e["offset"].as_u64().unwrap_or(0) + 1;
                            for (n, l) in elems(&e["elements"]).iter().enumerate() {
                                let mark = if e["style"] == "ordered" {
                                    format!("{}.", first + n as u64)
                                } else {
                                    "•".into()
                                };
                                out.push(format!("{pad}{mark} {}", mrkdwn(&l["elements"])));
                            }
                        }
                        _ => out.push(mrkdwn(&e["elements"]).trim_start_matches('\n').to_owned()),
                    }
                }
            }
            _ => {}
        }
    }
    out.join("\n")
}

/// Short plain text of a message's blocks: Slack's notification text.
pub fn fallback(blocks: &[Value]) -> String {
    let mut s = String::new();
    for b in blocks {
        let t = match b["type"].as_str() {
            Some("header") => b["text"]["text"].as_str().unwrap_or_default().to_owned(),
            Some("table") => "(table)".to_owned(),
            Some("markdown") => b["text"].as_str().unwrap_or_default().to_owned(),
            Some("rich_text") => elems(&b["elements"])
                .iter()
                .map(|e| {
                    if e["type"] == "rich_text_list" {
                        elems(&e["elements"])
                            .iter()
                            .map(|x| text_of(elems(&x["elements"])))
                            .collect::<Vec<_>>()
                            .join(" · ")
                    } else {
                        text_of(elems(&e["elements"])).trim().to_owned()
                    }
                })
                .collect::<Vec<_>>()
                .join(" "),
            _ => continue,
        };
        if !s.is_empty() {
            s.push(' ');
        }
        s.push_str(t.trim());
        if s.chars().count() > 300 {
            break;
        }
    }
    let s: String = s.chars().take(300).collect();
    if s.trim().is_empty() {
        "message".to_owned()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocks(s: &str) -> Vec<Value> {
        items(s)
            .into_iter()
            .filter_map(|i| match i {
                Item::Block(b) => Some(b),
                Item::Snippet { .. } => None,
            })
            .collect()
    }

    fn types(b: &[Value]) -> Vec<String> {
        b.iter()
            .map(|x| x["type"].as_str().unwrap().to_owned())
            .collect()
    }

    /// Every rich_text block's element types, flattened.
    fn rt_types(b: &[Value]) -> Vec<String> {
        b.iter()
            .filter(|x| x["type"] == "rich_text")
            .flat_map(|x| {
                elems(&x["elements"])
                    .iter()
                    .map(|e| e["type"].as_str().unwrap().to_owned())
            })
            .collect()
    }

    #[test]
    fn headings_rules_paragraphs() {
        let b = blocks(
            "# Title **x**\n\nSetext\n======\n\nOne *two* `three` ~~four~~ [five](http://f) ![img](http://i.png)\n\n---\n\nPara two\nsoft line",
        );
        assert_eq!(
            types(&b),
            ["header", "header", "rich_text", "divider", "rich_text"]
        );
        assert_eq!(b[0]["text"]["text"], "Title x");
        assert_eq!(b[1]["text"]["text"], "Setext");
        let e = elems(&b[2]["elements"][0]["elements"]);
        let st = |i: usize, k: &str| e[i]["style"][k] == true;
        assert_eq!(e[0]["text"], "One ");
        assert!(st(1, "italic") && st(3, "code") && st(5, "strike"), "{e:?}");
        assert_eq!(e[7]["type"], "link");
        assert_eq!(e[7]["text"], "five");
        assert_eq!(e[9]["url"], "http://i.png");
        assert_eq!(
            b[4]["elements"][0]["elements"][0]["text"],
            "Para two soft line"
        );
    }

    #[test]
    fn lists_nested_ordered_tasks() {
        let b = blocks(
            "- one\n  - nested **b**\n- two\n\n3. three\n4. four\n\n<!-- -->\n\n- [ ] todo\n- [x] done",
        );
        let lists: Vec<&Value> = b
            .iter()
            .flat_map(|x| elems(&x["elements"]).iter())
            .filter(|e| e["type"] == "rich_text_list")
            .collect();
        let summary: Vec<(String, u64, usize)> = lists
            .iter()
            .map(|l| {
                (
                    l["style"].as_str().unwrap().to_owned(),
                    l["indent"].as_u64().unwrap(),
                    elems(&l["elements"]).len(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("bullet".into(), 0, 1),
                ("bullet".into(), 1, 1),
                ("bullet".into(), 0, 1),
                ("ordered".into(), 0, 2),
                ("bullet".into(), 0, 2),
            ]
        );
        assert_eq!(
            lists[1]["elements"][0]["elements"][1]["style"]["bold"],
            true
        );
        assert_eq!(lists[3]["offset"], 2);
        let todo = elems(&lists[4]["elements"][0]["elements"]);
        assert_eq!(todo, [json!({"type": "text", "text": "todo"})]);
        let done = elems(&lists[4]["elements"][1]["elements"]);
        assert_eq!(
            done,
            [json!({"type": "text", "text": "done", "style": {"strike": true}})]
        );
        // A nested list under a done item is not struck.
        let n = blocks("- [x] parent\n  - child");
        let ls = elems(&n[0]["elements"]);
        assert_eq!(ls[0]["elements"][0]["elements"][0]["style"]["strike"], true);
        assert!(ls[1]["elements"][0]["elements"][0].get("style").is_none());
    }

    #[test]
    fn code_quotes_tables() {
        let src = "```rs\nfn a() {}\n```\n\n~~~\nplain\n~~~\n\n    indented\n\n```weird-lang\nx\n```\n\n> quoted **b**\n> - item\n\na | b\n:-- | --:\n**x** | `y`\n";
        let b = blocks(src);
        assert_eq!(types(&b), ["rich_text", "table"]);
        let els = elems(&b[0]["elements"]);
        let pre: Vec<&Value> = els
            .iter()
            .filter(|e| e["type"] == "rich_text_preformatted")
            .collect();
        assert_eq!(pre.len(), 4);
        assert_eq!(pre[0]["language"], "rust");
        assert!(pre[1].get("language").is_none());
        assert_eq!(pre[2]["elements"][0]["text"], "indented");
        assert!(
            pre[3].get("language").is_none(),
            "unknown language: no highlighting"
        );
        let q = els.iter().find(|e| e["type"] == "rich_text_quote").unwrap();
        assert_eq!(text_of(elems(&q["elements"])), "quoted b\n• item");
        let t = &b[1];
        assert_eq!(t["column_settings"][1]["align"], "right");
        assert_eq!(
            t["rows"][1][0]["elements"][0]["elements"][0]["style"]["bold"],
            true
        );
        assert_eq!(
            t["rows"][1][1]["elements"][0]["elements"][0]["style"]["code"],
            true
        );
    }

    #[test]
    fn long_code_becomes_a_snippet() {
        let long: String = (1..=12).map(|i| format!("line {i}\n")).collect();
        let it = items(&format!("before\n\n```python\n{long}```\n\nafter"));
        assert_eq!(it.len(), 3, "{it:?}");
        assert!(
            matches!(&it[1], Item::Snippet { lang: Some(l), code } if l == "python" && code.lines().count() == 12)
        );
        let m = messages(it);
        assert_eq!(m.len(), 3);
        assert!(matches!(m[1], Msg::Snippet { .. }));
        let ten: String = (1..=10).map(|i| format!("l{i}\n")).collect();
        assert_eq!(
            rt_types(&blocks(&format!("```\n{ten}```"))),
            ["rich_text_preformatted"]
        );
    }

    #[test]
    fn limits_split_and_fallbacks() {
        let mut rows = vec!["| a |".to_owned(), "|---|".to_owned()];
        rows.extend((0..120).map(|i| format!("| {i} |")));
        let b = blocks(&rows.join("\n"));
        assert_eq!(
            rt_types(&b),
            ["rich_text_preformatted"],
            "too big: monospace grid"
        );
        let two = items("| a |\n|---|\n| 1 |\n\ntext\n\n| b |\n|---|\n| 2 |");
        assert_eq!(messages(two).len(), 2, "one table per message");
        let many: Vec<Item> = (0..120)
            .map(|_| Item::Block(json!({"type": "divider"})))
            .collect();
        let sizes: Vec<usize> = messages(many)
            .iter()
            .map(|m| match m {
                Msg::Blocks(b) => b.len(),
                Msg::Snippet { .. } => 0,
            })
            .collect();
        assert_eq!(sizes, [50, 50, 20]);
        let b = blocks("# Hi\n\nthere **b** 1<2\n\n- a\n- b\n\n| k | v |\n|---|---|\n| 1 | 2 |");
        assert_eq!(fallback(&b), "Hi there b 1<2 a · b (table)");
        let m = to_mrkdwn(&b);
        assert!(
            m.starts_with("*Hi*\nthere *b* 1&lt;2\n• a\n• b\n```\nk  v"),
            "{m}"
        );
    }

    #[test]
    fn no_empty_text_on_odd_input() {
        for s in [
            "",
            "   ",
            "<div>html</div>",
            "*",
            "**",
            "> ",
            "- ",
            "1.",
            "|",
            "```",
            "[^1]\n\n[^1]: note",
            "a\\*b\\_c",
            "***both***",
            "- a\n\n  para\n\n  ```\n  code\n  ```\n- b",
        ] {
            for b in blocks(s) {
                let j = b.to_string();
                assert!(!j.contains(r#""text":"""#), "{s:?}: {j}");
            }
        }
        let b = blocks("a\\*b\\_c ***both***");
        let e = elems(&b[0]["elements"][0]["elements"]);
        assert_eq!(e[0]["text"], "a*b_c ");
        assert_eq!(e[1]["style"], json!({"bold": true, "italic": true}));
    }
}
