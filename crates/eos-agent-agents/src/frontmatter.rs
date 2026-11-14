//! The YAML subset agent files use: `key: value` lines with plain, quoted and
//! block (`|`, `>`) scalars, flow lists (`[a, b]`) and block lists (`- a`).
//! Nested mappings (hooks, inline MCP servers) are skipped, not parsed.

use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Value {
    Scalar(String),
    List(Vec<String>),
    /// A nested mapping or anything else outside the subset.
    Other,
}

/// Splits a document into its frontmatter fields and body. A file without a
/// leading `---` block has no fields.
pub(crate) fn split(text: &str) -> (HashMap<String, Value>, &str) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = text.strip_prefix("---").and_then(|r| r.strip_prefix('\n').or_else(|| r.strip_prefix("\r\n")))
    else {
        return (HashMap::new(), text);
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            let body = &rest[offset + line.len()..];
            return (parse(&rest[..offset]), body);
        }
        offset += line.len();
    }
    (HashMap::new(), text)
}

fn parse(yaml: &str) -> HashMap<String, Value> {
    let lines: Vec<&str> = yaml.lines().map(|l| l.trim_end_matches('\r')).collect();
    let mut fields = HashMap::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        if line.trim().is_empty() || line.trim_start().starts_with('#') || line.starts_with([' ', '\t']) {
            continue;
        }
        let Some((key, raw)) = line.split_once(':') else { continue };
        let raw = raw.trim();
        let block_end = lines[i..].iter().position(|l| !l.trim().is_empty() && !l.starts_with([' ', '\t', '-']));
        let block = &lines[i..i + block_end.unwrap_or(lines.len() - i)];
        let value = if raw.is_empty() {
            i += block.len();
            block_value(block)
        } else if raw.starts_with(['|', '>']) {
            i += block.len();
            Value::Scalar(block_scalar(block, raw.starts_with('|'), !raw.contains('-')))
        } else if raw.starts_with('[') {
            Value::List(flow_list(raw))
        } else if raw.starts_with('{') {
            Value::Other
        } else {
            Value::Scalar(scalar(raw))
        };
        fields.insert(key.trim().to_owned(), value);
    }
    fields
}

/// The indented lines under `key:` — a block list, or a mapping we skip.
fn block_value(block: &[&str]) -> Value {
    let items: Vec<&str> = block.iter().map(|l| l.trim()).filter(|l| !l.is_empty() && !l.starts_with('#')).collect();
    if items.is_empty() {
        return Value::Scalar(String::new());
    }
    if !items[0].starts_with('-') {
        return Value::Other;
    }
    let indent = |l: &&str| l.len() - l.trim_start().len();
    let item_indent = block.iter().find(|l| l.trim_start().starts_with('-')).map(indent).unwrap_or(0);
    let list = block
        .iter()
        .filter(|l| indent(l) == item_indent)
        .filter_map(|l| l.trim().strip_prefix('-'))
        .map(str::trim)
        .filter(|item| !is_mapping_entry(item))
        .map(scalar)
        .collect();
    Value::List(list)
}

fn is_mapping_entry(item: &str) -> bool {
    !item.starts_with(['"', '\'']) && (item.contains(": ") || item.ends_with(':'))
}

fn block_scalar(block: &[&str], literal: bool, keep_newline: bool) -> String {
    let indent =
        block.iter().filter(|l| !l.trim().is_empty()).map(|l| l.len() - l.trim_start().len()).min().unwrap_or(0);
    let lines: Vec<&str> = block.iter().map(|l| l.get(indent..).unwrap_or("")).collect();
    let text = if literal { lines.join("\n") } else { fold(&lines) };
    let text = text.trim_end_matches('\n');
    if keep_newline { format!("{text}\n") } else { text.to_owned() }
}

/// `>` scalars: lines join with spaces, blank lines become newlines.
fn fold(lines: &[&str]) -> String {
    let mut out = String::new();
    for line in lines {
        if line.is_empty() {
            out.push('\n');
        } else {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push(' ');
            }
            out.push_str(line);
        }
    }
    out
}

fn flow_list(raw: &str) -> Vec<String> {
    let inner = raw.trim().trim_start_matches('[').trim_end_matches(']');
    let mut items = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    for c in inner.chars() {
        match (quote, c) {
            (None, '"' | '\'') => {
                quote = Some(c);
                current.push(c);
            }
            (Some(q), _) if c == q => {
                quote = None;
                current.push(c);
            }
            (None, ',') => items.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    items.push(current);
    items.iter().map(|s| s.trim()).filter(|s| !s.is_empty()).map(scalar).collect()
}

fn scalar(raw: &str) -> String {
    let raw = raw.trim();
    if let Some(inner) = raw.strip_prefix('"').and_then(|r| r.get(..closing(r, '"')?)) {
        return unescape_double(inner);
    }
    if let Some(inner) = raw.strip_prefix('\'').and_then(|r| r.get(..closing(r, '\'')?)) {
        return inner.replace("''", "'");
    }
    match raw.find(" #") {
        Some(comment) => raw[..comment].trim_end().to_owned(),
        None => raw.to_owned(),
    }
}

/// Where a quoted scalar ends (anything after it is a comment): `\"` escapes
/// in double quotes, `''` in single quotes.
fn closing(s: &str, quote: char) -> Option<usize> {
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if quote == '"' && c == '\\' {
            chars.next();
        } else if c == quote {
            if quote == '\'' && chars.peek().is_some_and(|&(_, n)| n == '\'') {
                chars.next();
            } else {
                return Some(i);
            }
        }
    }
    None
}

fn unescape_double(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Value {
        Value::Scalar(v.to_owned())
    }

    #[test]
    fn scalars_lists_and_body() {
        let doc = "---\nname: meta-agent\ndescription: \"Says \\\"hi\\\": twice\"\nnote: 'it''s' # trailing\n\
                   tools: [Read, \"Bash(git log, x)\"]\nskills:\n  - one\n  - 'two'\nhooks:\n  PreToolUse: []\n\
                   mcpServers:\n  - github\n  - local:\n      type: stdio\nprompt: |\n  line 1\n\n  line 2\n\
                   folded: >-\n  a\n  b\nflat:\n- x\n- y\n---\n\nBody here.\n";
        let (fields, body) = split(doc);
        assert_eq!(body, "\nBody here.\n");
        assert_eq!(fields["name"], s("meta-agent"));
        assert_eq!(fields["description"], s("Says \"hi\": twice"));
        assert_eq!(fields["note"], s("it's"));
        assert_eq!(fields["tools"], Value::List(vec!["Read".into(), "Bash(git log, x)".into()]));
        assert_eq!(fields["skills"], Value::List(vec!["one".into(), "two".into()]));
        assert_eq!(fields["hooks"], Value::Other);
        assert_eq!(fields["mcpServers"], Value::List(vec!["github".into()]));
        assert_eq!(fields["prompt"], s("line 1\n\nline 2\n"));
        assert_eq!(fields["folded"], s("a b"));
        assert_eq!(fields["flat"], Value::List(vec!["x".into(), "y".into()]));
    }

    #[test]
    fn no_frontmatter() {
        let (fields, body) = split("just text\n");
        assert!(fields.is_empty());
        assert_eq!(body, "just text\n");
        let (fields, _) = split("---\nname: x\nno closing fence\n");
        assert!(fields.is_empty());
    }
}
