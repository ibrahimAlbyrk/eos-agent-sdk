//! Collecting hits across walk threads, then Claude Code's result text and
//! `head_limit`/`offset` pagination.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::input::{Input, Mode};
use super::search::Line;
use crate::common::display_path;
use crate::walk::Merge;

pub(super) const DEFAULT_HEAD_LIMIT: u64 = 250;

/// Hits from every file. Content lines are kept only up to `need` (in path
/// order), enough to fill the requested page and tell whether more exist.
#[derive(Debug, Default)]
pub(super) struct Found {
    content: BTreeMap<PathBuf, Vec<Line>>,
    kept: usize,
    total_lines: usize,
    need: Option<usize>,
    breaks: bool,
    files: Vec<(PathBuf, u64)>,
    counts: Vec<(PathBuf, u64)>,
}

impl Found {
    pub(super) fn new(need: Option<usize>, breaks: bool) -> Self {
        Self { need, breaks, ..Self::default() }
    }

    pub(super) fn add_lines(&mut self, path: PathBuf, lines: Vec<Line>, need: Option<usize>, breaks: bool) {
        if lines.is_empty() {
            return;
        }
        self.need = need;
        self.breaks = breaks;
        let weight = lines.len() + usize::from(breaks);
        self.total_lines += weight;
        self.kept += weight;
        self.content.insert(path, lines);
        self.trim();
    }

    pub(super) fn add_file(&mut self, path: PathBuf, mtime: u64) {
        self.files.push((path, mtime));
    }

    pub(super) fn add_count(&mut self, path: PathBuf, count: u64) {
        self.counts.push((path, count));
    }

    fn trim(&mut self) {
        let Some(need) = self.need else { return };
        while let Some((_, last)) = self.content.last_key_value() {
            let weight = last.len() + usize::from(self.breaks);
            if self.kept - weight < need {
                break;
            }
            self.kept -= weight;
            self.content.pop_last();
        }
    }
}

impl Merge for Found {
    fn merge(&mut self, other: Self) {
        self.need = self.need.or(other.need);
        self.breaks |= other.breaks;
        self.total_lines += other.total_lines;
        self.kept += other.kept;
        self.content.extend(other.content);
        self.trim();
        self.files.extend(other.files);
        self.counts.extend(other.counts);
    }
}

/// The page of `items` and the limit to report (only when more remain).
fn paginate<T>(items: Vec<T>, head_limit: Option<u64>, offset: u64) -> (Vec<T>, Option<u64>) {
    let offset = offset as usize;
    let rest = items.into_iter().skip(offset);
    match head_limit {
        Some(0) => (rest.collect(), None),
        limit => {
            let limit = limit.unwrap_or(DEFAULT_HEAD_LIMIT);
            let mut rest = rest.peekable();
            let page: Vec<T> = rest.by_ref().take(limit as usize).collect();
            let more = rest.peek().is_some();
            (page, more.then_some(limit))
        }
    }
}

fn pagination(limit: Option<u64>, offset: u64) -> String {
    let mut parts = Vec::new();
    if let Some(l) = limit {
        parts.push(format!("limit: {l}"));
    }
    if offset > 0 {
        parts.push(format!("offset: {offset}"));
    }
    parts.join(", ")
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 { format!("{n} {word}") } else { format!("{n} {word}s") }
}

/// `searched_dir`: the search target was a directory (content lines then
/// carry their path).
pub(super) fn render(found: Found, input: &Input, cwd: &Path, searched_dir: bool) -> String {
    match input.mode {
        Mode::Content => render_content(found, input, cwd, searched_dir),
        Mode::Count => render_count(found, input, cwd),
        Mode::Files => render_files(found, input, cwd),
    }
}

fn render_content(found: Found, input: &Input, cwd: &Path, searched_dir: bool) -> String {
    let mut items = Vec::with_capacity(found.kept);
    for (i, (path, lines)) in found.content.iter().enumerate() {
        if i > 0 && found.breaks {
            items.push("--".to_owned());
        }
        let shown = if searched_dir { display_path(cwd, path) } else { String::new() };
        for line in lines {
            items.push(format_line(line, &shown, searched_dir, input.line_numbers));
        }
    }
    let (page, limit) = paginate(items, input.head_limit, input.offset);
    let body = if page.is_empty() {
        if input.offset > 0 && found.total_lines > 0 {
            "No entries at this offset".into()
        } else {
            "No matches found".into()
        }
    } else {
        page.join("\n")
    };
    let pages = pagination(limit, input.offset);
    if pages.is_empty() { body } else { format!("{body}\n\n[Showing results with pagination = {pages}]") }
}

fn format_line(line: &Line, path: &str, with_path: bool, numbers: bool) -> String {
    let (kind, rest) = match line {
        Line::Break => return "--".into(),
        Line::Note(text) => (':', text.clone()),
        Line::Hit { number, kind, text } if numbers => (*kind, format!("{number}{kind}{text}")),
        Line::Hit { kind, text, .. } => (*kind, text.clone()),
    };
    if with_path { format!("{path}{kind}{rest}") } else { rest }
}

fn render_count(mut found: Found, input: &Input, cwd: &Path) -> String {
    found.counts.sort();
    let matches: u64 = found.counts.iter().map(|(_, n)| n).sum();
    let files = found.counts.len();
    let items: Vec<String> = found.counts.iter().map(|(p, n)| format!("{}:{n}", display_path(cwd, p))).collect();
    let (page, limit) = paginate(items, input.head_limit, input.offset);
    let body = if page.is_empty() {
        if matches > 0 { "No entries at this offset".into() } else { "No matches found".into() }
    } else {
        page.join("\n")
    };
    let pages = pagination(limit, input.offset);
    let occurrences = if matches == 1 { "occurrence" } else { "occurrences" };
    let paged = if pages.is_empty() { String::new() } else { format!(" with pagination = {pages}") };
    format!("{body}\n\nFound {matches} total {occurrences} across {}.{paged}", plural(files, "file"))
}

fn render_files(mut found: Found, input: &Input, cwd: &Path) -> String {
    found.files.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let total = found.files.len();
    let (page, limit) = paginate(found.files, input.head_limit, input.offset);
    let pages = pagination(limit, input.offset);
    if page.is_empty() {
        return if input.offset > 0 && total > 0 {
            format!("No entries at this offset. [Showing results with pagination = {pages}]")
        } else {
            "No files found".into()
        };
    }
    let paths: Vec<String> = page.iter().map(|(p, _)| display_path(cwd, p)).collect();
    let pages = if pages.is_empty() { String::new() } else { format!(" {pages}") };
    format!("Found {}{pages}\n{}", plural(paths.len(), "file"), paths.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_report_a_limit_only_when_more_remain() {
        assert_eq!(paginate((0..10).collect(), Some(5), 0), ((0..5).collect(), Some(5)));
        assert_eq!(paginate((0..10).collect(), Some(5), 5), ((5..10).collect(), None));
        assert_eq!(paginate((0..10).collect(), Some(0), 8), (vec![8, 9], None));
        assert_eq!(pagination(Some(250), 10), "limit: 250, offset: 10");
    }

    #[test]
    fn content_keeps_only_what_the_page_needs() {
        let hit = |n| Line::Hit { number: n, kind: ':', text: "x".into() };
        let mut found = Found::new(Some(3), false);
        found.add_lines("/b".into(), vec![hit(1), hit(2)], Some(3), false);
        found.add_lines("/c".into(), vec![hit(1), hit(2)], Some(3), false);
        found.add_lines("/a".into(), vec![hit(1), hit(2)], Some(3), false);
        assert_eq!(found.content.keys().collect::<Vec<_>>(), vec![Path::new("/a"), Path::new("/b")]);
        assert_eq!(found.total_lines, 6);
    }
}
