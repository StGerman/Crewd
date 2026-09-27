//! Converts between Atlassian Document Format (ADF) and Markdown.
//!
//! Jira Cloud REST v3 returns issue descriptions, and takes comment bodies, as ADF JSON, but the
//! daemon's agent prompt is Markdown throughout: `src/worker/prompt.rs`'s builder and its
//! HTML-comment stripping both assume the issue body is Markdown text, never a rich-text node
//! tree. `render` bridges the read side; `encode` bridges comments the agent writes back.
//!
//! No crate does this: `jc-adf` (0.2) and `atlassian-markdown-converter` (0.1) are both early
//! 0.x releases from a single maintainer, and pulling one in would let its Markdown dialect
//! shape the agent prompt instead of this repository choosing it (coding-guidelines: a
//! hand-rolled version over roughly fifty lines needs this recorded). `render` and `encode`
//! below stay well short of a general ADF or Markdown implementation on purpose — no arbitrary
//! mark nesting beyond code and link, no nested-table support, and `encode` recognises three
//! constructs only.
//!
//! Unknown node types keep rendering their `content` or `text` rather than being dropped, so a
//! Jira feature this module has not been taught about degrades to visible text in the prompt
//! instead of a silent gap.

// Nothing calls this module yet; the `#[allow]` goes when the tracker calls it (#99).
#![cfg_attr(not(test), allow(dead_code))]

use serde_json::{Value, json};
use time::OffsetDateTime;

// ---- ADF -> Markdown --------------------------------------------------------

/// Renders an ADF document as Markdown for the agent prompt. Never panics: a missing or
/// malformed field degrades to empty or plain text rather than stopping the conversion.
pub(crate) fn render(doc: &Value) -> String {
    render_blocks(children(doc))
}

/// Renders a sequence of block-level nodes, one blank line apart.
fn render_blocks(nodes: &[Value]) -> String {
    nodes.iter().map(render_node).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n\n")
}

/// Renders a sequence of inline nodes back to back, with no separator.
fn render_inline(nodes: &[Value]) -> String {
    nodes.iter().map(render_node).collect()
}

/// The single dispatch point over every ADF node type this module knows about. One small
/// function per node family keeps this under the line and parameter limits; an unrecognized
/// type falls through to [`render_unknown`] so its text is never silently dropped.
fn render_node(node: &Value) -> String {
    match node_type(node) {
        "paragraph" => render_inline(children(node)),
        "heading" => render_heading(node),
        "bulletList" => render_list(node, ListKind::Bullet),
        "orderedList" => render_list(node, ListKind::Ordered),
        "taskList" => render_list(node, ListKind::Task),
        "codeBlock" => render_code_block(node),
        "blockquote" | "panel" => render_quote(node),
        "rule" => "---".to_string(),
        "table" => render_table(node),
        "mediaSingle" | "mediaGroup" | "media" | "mediaInline" => render_media(node),
        "expand" | "nestedExpand" => render_expand(node),
        "text" => render_text(node),
        "hardBreak" => "\n".to_string(),
        "mention" => render_mention(node),
        "emoji" => {
            attr_str(node, "text").or_else(|| attr_str(node, "shortName")).unwrap_or_default()
        }
        "inlineCard" | "blockCard" | "embedCard" => attr_str(node, "url").unwrap_or_default(),
        "date" => render_date(node),
        "status" => format!("[{}]", attr_str(node, "text").unwrap_or_default()),
        _ => render_unknown(node),
    }
}

/// An unrecognized node keeps its descendants' text: block children first, then a plain `text`
/// field, so a Jira feature this module has not been taught about never disappears silently.
fn render_unknown(node: &Value) -> String {
    match node.get("content").and_then(Value::as_array) {
        Some(content) => render_inline(content),
        None => node.get("text").and_then(Value::as_str).unwrap_or_default().to_string(),
    }
}

fn render_heading(node: &Value) -> String {
    let level = attr_u64(node, "level").unwrap_or(1).clamp(1, 6);
    format!("{} {}", "#".repeat(level as usize), render_inline(children(node)))
}

enum ListKind {
    Bullet,
    Ordered,
    Task,
}

/// `bulletList`, `orderedList` and `taskList` differ only in each item's marker, so one function
/// covers the family; [`render_list_item`] handles the shared indentation for nested content.
fn render_list(node: &Value, kind: ListKind) -> String {
    let mut order = attr_i64(node, "order").unwrap_or(1);
    children(node)
        .iter()
        .map(|item| {
            let marker = match kind {
                ListKind::Bullet => "- ".to_string(),
                ListKind::Ordered => {
                    let marker = format!("{order}. ");
                    order += 1;
                    marker
                }
                ListKind::Task => task_marker(item),
            };
            render_list_item(item, &marker)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn task_marker(item: &Value) -> String {
    if attr_str(item, "state").as_deref() == Some("DONE") {
        "- [x] ".to_string()
    } else {
        "- [ ] ".to_string()
    }
}

/// The item's own marker replaces the first line; every following line — including a nested
/// list's own markers — is indented by the marker's width, so nesting composes without tracking
/// depth explicitly.
fn render_list_item(item: &Value, marker: &str) -> String {
    let body = render_blocks(children(item));
    if body.is_empty() {
        return marker.trim_end().to_string();
    }
    let indent = " ".repeat(marker.chars().count());
    let mut out = String::new();
    for (i, line) in body.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if i == 0 {
            out.push_str(marker);
            out.push_str(line);
        } else if !line.is_empty() {
            out.push_str(&indent);
            out.push_str(line);
        }
    }
    out
}

fn render_code_block(node: &Value) -> String {
    let lang = attr_str(node, "language").unwrap_or_default();
    let text: String =
        children(node).iter().filter_map(|n| n.get("text").and_then(Value::as_str)).collect();
    format!("```{lang}\n{text}\n```")
}

fn render_quote(node: &Value) -> String {
    render_blocks(children(node))
        .lines()
        .map(|l| if l.is_empty() { ">".to_string() } else { format!("> {l}") })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A pipe table: the first row is always the header row, whether its cells are `tableHeader` or
/// `tableCell` — this module never inspects the cell's own type, only its content.
fn render_table(node: &Value) -> String {
    let rows: Vec<Vec<String>> =
        children(node).iter().map(|row| children(row).iter().map(render_cell).collect()).collect();
    let Some(header) = rows.first() else {
        return String::new();
    };
    let cols = header.len();
    let mut lines = vec![pipe_row(header, cols), pipe_row(&vec!["---".to_string(); cols], cols)];
    lines.extend(rows[1..].iter().map(|r| pipe_row(r, cols)));
    lines.join("\n")
}

fn pipe_row(cells: &[String], cols: usize) -> String {
    let mut padded: Vec<&str> = cells.iter().map(String::as_str).collect();
    let blank = String::new();
    while padded.len() < cols {
        padded.push(&blank);
    }
    format!("| {} |", padded.join(" | "))
}

/// Flattened to one line — every newline in the cell's own rendering becomes a space — with `|`
/// escaped, since a raw pipe or a line break would otherwise split or misalign the row.
fn render_cell(cell: &Value) -> String {
    render_blocks(children(cell)).replace('\n', " ").replace('|', "\\|")
}

fn render_media(node: &Value) -> String {
    match find_alt(node) {
        Some(alt) if !alt.is_empty() => format!("[attachment: {alt}]"),
        _ => "[attachment]".to_string(),
    }
}

/// `mediaSingle`/`mediaGroup` wrap a `media`/`mediaInline` child that carries `attrs.alt`, so the
/// alt text is searched through the node's own children rather than assumed to sit on top.
fn find_alt(node: &Value) -> Option<String> {
    if let Some(alt) = attr_str(node, "alt") {
        return Some(alt);
    }
    children(node).iter().find_map(find_alt)
}

fn render_expand(node: &Value) -> String {
    let title = attr_str(node, "title").unwrap_or_default();
    let body = render_blocks(children(node));
    match (title.is_empty(), body.is_empty()) {
        (true, _) => body,
        (false, true) => format!("**{title}**"),
        (false, false) => format!("**{title}**\n\n{body}"),
    }
}

fn render_text(node: &Value) -> String {
    let text = node.get("text").and_then(Value::as_str).unwrap_or_default();
    let empty = Vec::new();
    let marks = node.get("marks").and_then(Value::as_array).unwrap_or(&empty);
    apply_marks(text.to_string(), marks)
}

/// Code applies innermost and link outermost; the marks in between (strong, em, strike) apply in
/// the order Jira sent them. A mark this module does not know — underline, `textColor`,
/// subsup — leaves the text plain rather than emitting Markdown that does not mean that mark.
fn apply_marks(text: String, marks: &[Value]) -> String {
    let mut out = text;
    if marks.iter().any(|m| mark_type(m) == "code") {
        out = format!("`{out}`");
    }
    for m in marks {
        match mark_type(m) {
            "strong" => out = format!("**{out}**"),
            "em" => out = format!("*{out}*"),
            "strike" => out = format!("~~{out}~~"),
            _ => {}
        }
    }
    if let Some(link) = marks.iter().find(|m| mark_type(m) == "link") {
        out = format!("[{out}]({})", attr_str(link, "href").unwrap_or_default());
    }
    out
}

fn render_mention(node: &Value) -> String {
    match attr_str(node, "text") {
        Some(text) => text,
        None => attr_str(node, "id").map(|id| format!("@{id}")).unwrap_or_default(),
    }
}

fn render_date(node: &Value) -> String {
    let Some(raw) = node.get("attrs").and_then(|a| a.get("timestamp")) else {
        return String::new();
    };
    value_as_millis(raw).and_then(format_millis).unwrap_or_else(|| raw_value_string(raw))
}

fn value_as_millis(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

fn format_millis(millis: i64) -> Option<String> {
    let dt = OffsetDateTime::from_unix_timestamp(millis.div_euclid(1000)).ok()?;
    Some(format!("{:04}-{:02}-{:02}", dt.year(), u8::from(dt.month()), dt.day()))
}

fn raw_value_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

// ---- node helpers ------------------------------------------------------------

fn node_type(node: &Value) -> &str {
    node.get("type").and_then(Value::as_str).unwrap_or("")
}

fn mark_type(mark: &Value) -> &str {
    node_type(mark)
}

fn children(node: &Value) -> &[Value] {
    node.get("content").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

fn attr_str(node: &Value, key: &str) -> Option<String> {
    node.get("attrs")?.get(key)?.as_str().map(str::to_string)
}

fn attr_u64(node: &Value, key: &str) -> Option<u64> {
    node.get("attrs")?.get(key)?.as_u64()
}

fn attr_i64(node: &Value, key: &str) -> Option<i64> {
    node.get("attrs")?.get(key)?.as_i64()
}

// ---- Markdown -> ADF --------------------------------------------------------

/// Encodes an agent-written comment as ADF. Deliberately not a Markdown parser: it exists only
/// to keep the text readable once Jira renders it, so it recognises paragraphs, line breaks and
/// fenced code blocks and copies everything else through as plain text. A heading, a list or a
/// table written in the comment stays literal text with its punctuation intact — good enough for
/// a comment, unlike a description Jira itself renders as the source of truth.
pub(crate) fn encode(markdown: &str) -> Value {
    json!({"type": "doc", "version": 1, "content": encode_blocks(markdown)})
}

fn encode_blocks(markdown: &str) -> Vec<Value> {
    let mut blocks = Vec::new();
    let mut paragraph: Vec<&str> = Vec::new();
    let mut lines = markdown.lines();
    while let Some(line) = lines.next() {
        if let Some(lang) = line.strip_prefix("```") {
            flush_paragraph(&mut paragraph, &mut blocks);
            blocks.push(encode_code_block(lang.trim(), &mut lines));
        } else if line.trim().is_empty() {
            flush_paragraph(&mut paragraph, &mut blocks);
        } else {
            paragraph.push(line);
        }
    }
    flush_paragraph(&mut paragraph, &mut blocks);
    blocks
}

fn encode_code_block<'a>(lang: &str, lines: &mut std::str::Lines<'a>) -> Value {
    let mut code = Vec::new();
    for line in lines.by_ref() {
        if line.starts_with("```") {
            break;
        }
        code.push(line);
    }
    let content = if code.is_empty() {
        Vec::new()
    } else {
        vec![json!({"type": "text", "text": code.join("\n")})]
    };
    let mut node = json!({"type": "codeBlock", "content": content});
    if !lang.is_empty() {
        node["attrs"] = json!({"language": lang});
    }
    node
}

/// A paragraph's lines join with `hardBreak`; a line that is only whitespace never happens here
/// (it already ended the paragraph in [`encode_blocks`]), so every text node pushed is non-empty
/// — ADF rejects an empty one.
fn flush_paragraph(lines: &mut Vec<&str>, blocks: &mut Vec<Value>) {
    if lines.is_empty() {
        return;
    }
    let mut content = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            content.push(json!({"type": "hardBreak"}));
        }
        content.push(json!({"type": "text", "text": line}));
    }
    blocks.push(json!({"type": "paragraph", "content": content}));
    lines.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_description_with_headings_lists_code_and_a_table_renders_as_markdown() {
        let doc = json!({
            "type": "doc",
            "version": 1,
            "content": [
                {"type": "heading", "attrs": {"level": 2}, "content": [{"type": "text", "text": "Summary"}]},
                {"type": "paragraph", "content": [
                    {"type": "text", "text": "This is "},
                    {"type": "text", "text": "important", "marks": [{"type": "strong"}]},
                    {"type": "text", "text": ", see "},
                    {"type": "text", "text": "the code", "marks": [{"type": "code"}]},
                    {"type": "text", "text": " and "},
                    {"type": "text", "text": "the docs", "marks": [{"type": "link", "attrs": {"href": "https://example.com"}}]},
                    {"type": "text", "text": "."}
                ]},
                {"type": "bulletList", "content": [
                    {"type": "listItem", "content": [
                        {"type": "paragraph", "content": [{"type": "text", "text": "first"}]}
                    ]},
                    {"type": "listItem", "content": [
                        {"type": "paragraph", "content": [{"type": "text", "text": "second"}]},
                        {"type": "orderedList", "attrs": {"order": 1}, "content": [
                            {"type": "listItem", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "nested one"}]}]},
                            {"type": "listItem", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "nested two"}]}]}
                        ]}
                    ]}
                ]},
                {"type": "codeBlock", "attrs": {"language": "rust"}, "content": [
                    {"type": "text", "text": "fn main() {}"}
                ]},
                {"type": "table", "content": [
                    {"type": "tableRow", "content": [
                        {"type": "tableHeader", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "Name"}]}]},
                        {"type": "tableHeader", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "Role"}]}]},
                        {"type": "tableHeader", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "Team"}]}]}
                    ]},
                    {"type": "tableRow", "content": [
                        {"type": "tableCell", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "Jane"}]}]},
                        {"type": "tableCell", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "Lead"}]}]},
                        {"type": "tableCell", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "Core"}]}]}
                    ]}
                ]},
                {"type": "rule"}
            ]
        });
        insta::assert_snapshot!(render(&doc));
    }

    #[test]
    fn inline_nodes_render_as_their_readable_text() {
        let doc = json!({"type": "doc", "version": 1, "content": [
            {"type": "paragraph", "content": [
                {"type": "mention", "attrs": {"id": "abc", "text": "@Jane"}},
                {"type": "text", "text": " reacted "},
                {"type": "emoji", "attrs": {"shortName": ":thumbsup:", "text": "\u{1F44D}"}},
                {"type": "hardBreak"},
                {"type": "inlineCard", "attrs": {"url": "https://example.com/PROJ-1"}},
                {"type": "text", "text": " due "},
                {"type": "date", "attrs": {"timestamp": "1700000000000"}},
                {"type": "text", "text": " status "},
                {"type": "status", "attrs": {"text": "Done", "color": "green"}}
            ]}
        ]});
        insta::assert_snapshot!(render(&doc));
    }

    #[test]
    fn panels_expands_and_quotes_render_as_quoted_or_titled_blocks() {
        let doc = json!({"type": "doc", "version": 1, "content": [
            {"type": "blockquote", "content": [
                {"type": "paragraph", "content": [{"type": "text", "text": "quoted line one"}]},
                {"type": "paragraph", "content": [{"type": "text", "text": "quoted line two"}]}
            ]},
            {"type": "panel", "attrs": {"panelType": "warning"}, "content": [
                {"type": "paragraph", "content": [{"type": "text", "text": "careful here"}]}
            ]},
            {"type": "expand", "attrs": {"title": "Details"}, "content": [
                {"type": "paragraph", "content": [{"type": "text", "text": "hidden text"}]}
            ]}
        ]});
        insta::assert_snapshot!(render(&doc));
    }

    #[test]
    fn a_table_cell_with_a_pipe_or_a_newline_stays_on_one_row() {
        let doc = json!({"type": "doc", "version": 1, "content": [
            {"type": "table", "content": [
                {"type": "tableRow", "content": [
                    {"type": "tableHeader", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "A"}]}]},
                    {"type": "tableHeader", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "B"}]}]}
                ]},
                {"type": "tableRow", "content": [
                    {"type": "tableCell", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "a|b"}]}]},
                    {"type": "tableCell", "content": [
                        {"type": "paragraph", "content": [{"type": "text", "text": "line one"}]},
                        {"type": "paragraph", "content": [{"type": "text", "text": "line two"}]}
                    ]}
                ]}
            ]}
        ]});
        insta::assert_snapshot!(render(&doc));
    }

    #[test]
    fn task_items_render_as_checkboxes() {
        let doc = json!({"type": "doc", "version": 1, "content": [
            {"type": "taskList", "content": [
                {"type": "taskItem", "attrs": {"state": "DONE"}, "content": [
                    {"type": "paragraph", "content": [{"type": "text", "text": "shipped"}]}
                ]},
                {"type": "taskItem", "attrs": {"state": "TODO"}, "content": [
                    {"type": "paragraph", "content": [{"type": "text", "text": "still open"}]}
                ]}
            ]}
        ]});
        insta::assert_snapshot!(render(&doc));
    }

    #[test]
    fn an_unknown_adf_node_keeps_its_text_rather_than_being_dropped() {
        let doc = json!({"type": "doc", "version": 1, "content": [
            {"type": "paragraph", "content": [
                {"type": "text", "text": "before "},
                {"type": "fancyWidget", "content": [{"type": "text", "text": "widget text"}]},
                {"type": "text", "text": " after"}
            ]}
        ]});
        insta::assert_snapshot!(render(&doc));
    }

    #[test]
    fn malformed_adf_renders_without_panicking() {
        assert_eq!(render(&json!("just a string")), "");
        assert_eq!(render(&json!(null)), "");
        assert_eq!(render(&json!({"type": "doc"})), "");
        assert_eq!(render(&json!({"type": "doc", "content": "not an array"})), "");
        let odd = json!({"type": "doc", "content": [
            {"type": "heading", "attrs": {"level": "two"}, "content": [{"type": "text", "text": "oops"}]},
            {"type": "paragraph", "content": [{"type": "text", "marks": {"not": "an array"}}]},
            {"type": "text"}
        ]});
        insta::assert_snapshot!(render(&odd));
    }

    #[test]
    fn a_comment_with_paragraphs_line_breaks_and_a_fence_encodes_to_adf() {
        let markdown = "first paragraph\nstill first paragraph\n\n```rust\nfn main() {}\n```\n\nlast paragraph";
        insta::assert_snapshot!(serde_json::to_string_pretty(&encode(markdown)).unwrap());
    }

    #[test]
    fn encoding_never_emits_an_empty_text_node() {
        let doc = encode("\n\n```\n```\n\nonly line\n\n\n");
        let dump = serde_json::to_string(&doc).unwrap();
        assert!(!dump.contains("\"text\":\"\""));
    }
}
