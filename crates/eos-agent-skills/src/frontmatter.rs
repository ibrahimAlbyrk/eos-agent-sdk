//! The YAML frontmatter of skill and command files.
//!
//! Only the subset skill files use: top-level `key: value` scalars (plain,
//! quoted, `|`/`>` blocks), block and flow lists. Nested mappings (`hooks`,
//! `metadata`) are recognized and skipped. Like Claude Code's lenient
//! fallback, a plain value containing `: ` is kept as text, not rejected.

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Value {
    Str(String),
    List(Vec<String>),
    /// A nested mapping, not read.
    Map,
}

#[derive(Debug, Default)]
pub(crate) struct Frontmatter {
    fields: Vec<(String, Value)>,
}

impl Frontmatter {
    /// Keys compare lowercased with `_` read as `-` (`when_to_use` = `when-to-use`).
    pub(crate) fn get(&self, key: &str) -> Option<&Value> {
        self.fields.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// A non-empty scalar (`null` and `~` count as absent).
    pub(crate) fn str(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            Value::Str(s) if !s.trim().is_empty() && s != "null" && s != "~" => Some(s.as_str()),
            _ => None,
        }
    }

    pub(crate) fn bool(&self, key: &str) -> Option<bool> {
        match self.str(key)?.trim().to_ascii_lowercase().as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        }
    }

    /// A list, or a scalar split by `split`.
    pub(crate) fn list(&self, key: &str, split: fn(&str) -> Vec<String>) -> Vec<String> {
        match self.get(key) {
            Some(Value::List(items)) => items.iter().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect(),
            Some(Value::Str(s)) => split(s),
            _ => Vec::new(),
        }
    }
}

/// Splits a file into its frontmatter and body.
pub(crate) fn split(text: &str) -> (Frontmatter, &str) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    if lines.next().map(|l| l.trim_end()) != Some("---") {
        return (Frontmatter::default(), text);
    }
    let start = text.find('\n').map_or(text.len(), |i| i + 1);
    let mut at = start;
    for line in lines {
        if line.trim_end() == "---" {
            return (parse(&text[start..at]), &text[at + line.len()..]);
        }
        at += line.len();
    }
    (Frontmatter::default(), text)
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start_matches([' ', '\t']).len()
}

fn is_blank(line: &str) -> bool {
    let t = line.trim();
    t.is_empty() || t.starts_with('#')
}

pub(crate) fn parse(yaml: &str) -> Frontmatter {
    let lines: Vec<&str> = yaml.lines().map(|l| l.trim_end_matches('\r')).collect();
    let mut fields = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        if is_blank(line) || indent(line) > 0 {
            continue;
        }
        // A top-level line that is not `key: value` (`name:x`) makes the
        // document invalid YAML; Claude Code then ignores the frontmatter.
        let Some((key, rest)) = key_value(line) else { return Frontmatter::default() };
        let key = key.to_ascii_lowercase().replace('_', "-");
        // Lines that belong to this key: indented ones, and `- item` lines.
        let block_end = (i..lines.len())
            .find(|&j| !lines[j].trim().is_empty() && indent(lines[j]) == 0 && !lines[j].starts_with('-'))
            .unwrap_or(lines.len());
        let block = &lines[i..block_end];
        let rest = rest.trim();
        let (value, used) = if rest.is_empty() {
            (nested(block), block.len())
        } else if rest.starts_with('|') || rest.starts_with('>') {
            let n = block.iter().take_while(|l| l.trim().is_empty() || indent(l) > 0).count();
            (Value::Str(block_scalar(rest, &block[..n])), n)
        } else if rest.starts_with('[') {
            let (items, n) = flow_list(rest, block);
            (Value::List(items), n)
        } else if rest.starts_with('{') {
            (Value::Map, block.len())
        } else if rest.starts_with('"') || rest.starts_with('\'') {
            (Value::Str(quoted(rest)), 0)
        } else {
            let n = block.iter().take_while(|l| !l.trim().is_empty() && indent(l) > 0).count();
            let mut text = strip_comment(rest).to_owned();
            for more in &block[..n] {
                text.push(' ');
                text.push_str(strip_comment(more.trim()));
            }
            (Value::Str(text), n)
        };
        i += used;
        fields.push((key, value));
    }
    Frontmatter { fields }
}

fn key_value(line: &str) -> Option<(String, &str)> {
    let at =
        line.char_indices().find(|&(i, c)| c == ':' && line[i + 1..].chars().next().is_none_or(char::is_whitespace))?.0;
    let key = line[..at].trim();
    let key = key.trim_matches(|c| c == '"' || c == '\'');
    (!key.is_empty()).then(|| (key.to_owned(), &line[at + 1..]))
}

fn nested(block: &[&str]) -> Value {
    match block.iter().find(|l| !is_blank(l)) {
        None => Value::Str(String::new()),
        Some(first) if first.trim_start().starts_with('-') => Value::List(
            block
                .iter()
                .filter_map(|l| l.trim_start().strip_prefix('-'))
                .map(|item| scalar(strip_comment(item.trim())))
                .filter(|s| !s.is_empty())
                .collect(),
        ),
        Some(_) => Value::Map,
    }
}

fn block_scalar(header: &str, lines: &[&str]) -> String {
    let folded = header.starts_with('>');
    let base = lines.iter().filter(|l| !l.trim().is_empty()).map(|l| indent(l)).min().unwrap_or(0);
    let body: Vec<&str> =
        lines.iter().map(|l| if l.trim().is_empty() { "" } else { &l[base.min(l.len())..] }).collect();
    let mut text = String::new();
    for (n, line) in body.iter().enumerate() {
        if n > 0 {
            let joins_paragraph = folded && !line.is_empty() && !body[n - 1].is_empty();
            text.push(if joins_paragraph { ' ' } else { '\n' });
        }
        text.push_str(line);
    }
    let trimmed = text.trim_end_matches('\n');
    if header.contains('-') || trimmed.is_empty() { trimmed.to_owned() } else { format!("{trimmed}\n") }
}

fn flow_list(first: &str, block: &[&str]) -> (Vec<String>, usize) {
    let mut text = first.to_owned();
    let mut used = 0;
    while !text.contains(']') && used < block.len() {
        text.push(' ');
        text.push_str(block[used].trim());
        used += 1;
    }
    let inner = text.trim_start_matches('[');
    let inner = inner.rfind(']').map_or(inner, |end| &inner[..end]);
    let mut items = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    for c in inner.chars() {
        match (quote, c) {
            (None, '"' | '\'') => {
                quote = Some(c);
                current.push(c);
            }
            (Some(q), c) if c == q => {
                quote = None;
                current.push(c);
            }
            (None, ',') => items.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    items.push(current);
    (items.into_iter().map(|s| scalar(s.trim())).filter(|s| !s.is_empty()).collect(), used)
}

fn scalar(text: &str) -> String {
    if text.starts_with('"') || text.starts_with('\'') { quoted(text) } else { text.to_owned() }
}

fn quoted(text: &str) -> String {
    let mut chars = text.chars();
    let Some(q) = chars.next() else { return String::new() };
    let mut out = String::new();
    while let Some(c) = chars.next() {
        match c {
            c if c == q && q == '\'' && chars.clone().next() == Some('\'') => {
                chars.next();
                out.push('\'');
            }
            c if c == q => return out,
            '\\' if q == '"' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('u') => {
                    let hex: String = chars.by_ref().take(4).collect();
                    out.extend(u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32));
                }
                Some(other) => out.push(other),
                None => {}
            },
            c => out.push(c),
        }
    }
    out
}

fn strip_comment(text: &str) -> &str {
    text.find(" #").map_or(text, |at| &text[..at]).trim_end()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_and_parses() {
        let file = "---\nname: deploy\ndescription: Ship it: carefully # note\nallowed-tools: [Bash(git:*), \"Read\"]\n\
                    arguments:\n  - env\n  - region\nwhen_to_use: >\n  When the user\n  asks to deploy.\n\
                    hooks:\n  PreToolUse:\n    - x\nuser-invocable: false\n---\nBody line\n";
        let (fm, body) = split(file);
        assert_eq!(body, "Body line\n");
        assert_eq!(fm.str("name"), Some("deploy"));
        assert_eq!(fm.str("description"), Some("Ship it: carefully"));
        assert_eq!(fm.get("allowed-tools"), Some(&Value::List(vec!["Bash(git:*)".into(), "Read".into()])));
        assert_eq!(fm.get("arguments"), Some(&Value::List(vec!["env".into(), "region".into()])));
        assert_eq!(fm.str("when-to-use"), Some("When the user asks to deploy.\n"));
        assert_eq!(fm.get("hooks"), Some(&Value::Map));
        assert_eq!(fm.bool("user-invocable"), Some(false));
    }

    #[test]
    fn literal_blocks_quotes_and_missing_frontmatter() {
        let (fm, _) = split("---\ndescription: |-\n  line one\n  line two\nmodel: 'it''s'\nhint: \"a\\\"b\"\n---\n");
        assert_eq!(fm.str("description"), Some("line one\nline two"));
        assert_eq!(fm.str("model"), Some("it's"));
        assert_eq!(fm.str("hint"), Some("a\"b"));
        let (fm, body) = split("# Title\ntext");
        assert!(fm.get("name").is_none());
        assert_eq!(body, "# Title\ntext");
    }

    #[test]
    fn invalid_yaml_drops_the_whole_frontmatter() {
        let (fm, body) = split("---\nname:ultrathink\ndisable-model-invocation: true\n---\n## Usage\n");
        assert!(fm.get("disable-model-invocation").is_none());
        assert_eq!(body, "## Usage\n");
    }
}
