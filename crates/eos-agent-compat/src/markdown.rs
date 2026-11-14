//! The bits of Markdown handling CLAUDE.md loading needs: frontmatter,
//! block-level HTML comments, and `@path` imports outside code.

/// Splits YAML frontmatter (`---` lines) from the body.
pub(crate) fn split_frontmatter(text: &str) -> (Option<&str>, &str) {
    let Some(rest) = text.strip_prefix("---\n").or_else(|| text.strip_prefix("---\r\n")) else {
        return (None, text);
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return (Some(&rest[..offset]), &rest[offset + line.len()..]);
        }
        offset += line.len();
    }
    (None, text)
}

/// A rule file with `paths:` applies only while matching files are worked
/// on, so it is not part of the session's standing instructions.
pub(crate) fn is_conditional(frontmatter: &str) -> bool {
    frontmatter.lines().any(|l| l.trim_start().starts_with("paths:"))
}

/// Removes HTML comments that stand as their own block (outside code
/// fences); comments inside a paragraph stay, as in Claude Code.
pub(crate) fn strip_block_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut fence = Fence::default();
    let mut comment: Option<String> = None;
    for line in text.split_inclusive('\n') {
        if let Some(buf) = comment.as_mut() {
            buf.push_str(line);
            if line.contains("-->") {
                push_without_comments(&mut out, buf);
                comment = None;
            }
            continue;
        }
        if !fence.update(line) && line.trim_start().starts_with("<!--") {
            if line.contains("-->") {
                push_without_comments(&mut out, line);
            } else {
                comment = Some(line.to_owned());
            }
            continue;
        }
        out.push_str(line);
    }
    // An unclosed comment is not a comment block; keep it as written.
    if let Some(buf) = comment {
        out.push_str(&buf);
    }
    out
}

fn push_without_comments(out: &mut String, chunk: &str) {
    let mut rest = chunk;
    let mut kept = String::new();
    while let Some(start) = rest.find("<!--") {
        kept.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + 3..],
            None => {
                rest = &rest[start..];
                break;
            }
        }
    }
    kept.push_str(rest);
    if !kept.trim().is_empty() {
        out.push_str(&kept);
    }
}

/// The `@path` imports in `text`, outside code fences and inline code, with
/// `#fragment`s dropped and `\ ` unescaped. Paths are returned as written.
pub(crate) fn imports(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut fence = Fence::default();
    for line in text.lines() {
        if fence.update(line) {
            continue;
        }
        for segment in outside_inline_code(line) {
            scan_segment(segment, &mut found);
        }
    }
    found
}

fn scan_segment(segment: &str, found: &mut Vec<String>) {
    let bytes = segment.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'@' && (i == 0 || bytes[i - 1].is_ascii_whitespace()) {
            let start = i + 1;
            let mut end = start;
            while end < bytes.len() {
                if bytes[end] == b'\\' && bytes.get(end + 1) == Some(&b' ') {
                    end += 2;
                } else if bytes[end].is_ascii_whitespace() || bytes[end] == b'\\' {
                    break;
                } else {
                    end += 1;
                }
            }
            if let Some(path) = import_target(&segment[start..end])
                && !found.contains(&path)
            {
                found.push(path);
            }
            i = end.max(start);
        } else {
            i += 1;
        }
    }
}

fn import_target(raw: &str) -> Option<String> {
    let raw = raw.split('#').next().unwrap_or_default();
    let path = raw.replace("\\ ", " ");
    let first = path.chars().next()?;
    let plausible = path.starts_with("./")
        || path.starts_with("~/")
        || (path.starts_with('/') && path != "/")
        || (first.is_ascii_alphanumeric() || matches!(first, '.' | '_' | '-'));
    plausible.then_some(path)
}

/// The parts of a line outside backtick code spans.
fn outside_inline_code(line: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut rest = line;
    while let Some(open) = rest.find('`') {
        parts.push(&rest[..open]);
        let ticks = rest[open..].bytes().take_while(|b| *b == b'`').count();
        let after = &rest[open + ticks..];
        let close = "`".repeat(ticks);
        match after.find(&close) {
            Some(end) => rest = &after[end + ticks..],
            None => return parts,
        }
    }
    parts.push(rest);
    parts
}

#[derive(Default)]
struct Fence(Option<(u8, usize)>);

impl Fence {
    /// Feeds one line; true when the line is inside a fence or delimits one.
    fn update(&mut self, line: &str) -> bool {
        let trimmed = line.trim_start();
        let marker = trimmed.bytes().next().filter(|b| *b == b'`' || *b == b'~');
        let run = marker.map_or(0, |m| trimmed.bytes().take_while(|b| *b == m).count());
        match (self.0, marker) {
            (None, Some(m)) if run >= 3 => {
                self.0 = Some((m, run));
                true
            }
            (Some((m, n)), Some(c)) if c == m && run >= n && trimmed[run..].trim().is_empty() => {
                self.0 = None;
                true
            }
            (Some(_), _) => true,
            (None, _) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_is_split_off() {
        assert_eq!(split_frontmatter("---\npaths: src/**\n---\nbody\n"), (Some("paths: src/**\n"), "body\n"));
        assert_eq!(split_frontmatter("no front\n---\n"), (None, "no front\n---\n"));
        assert!(is_conditional("paths:\n  - a\n"));
    }

    #[test]
    fn block_comments_go_inline_ones_stay() {
        let text = "a\n<!-- hidden\nstill -->\nb <!-- kept --> c\n```\n<!-- code -->\n```\n<!-- x --> tail\n";
        assert_eq!(strip_block_comments(text), "a\nb <!-- kept --> c\n```\n<!-- code -->\n```\n tail\n");
    }

    #[test]
    fn imports_outside_code_only() {
        let text = "See @docs/a.md and @./b.md#part, @~/c.md\nmail me@example.com `@skip.md`\n```\n@fenced.md\n```\n\
@my\\ file.md @/abs/d.md @@x @#nope";
        assert_eq!(imports(text), vec!["docs/a.md", "./b.md", "~/c.md", "my file.md", "/abs/d.md"]);
    }
}
