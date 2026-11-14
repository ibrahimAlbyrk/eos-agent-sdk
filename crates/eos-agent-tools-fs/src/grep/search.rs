//! Searching one file the way `rg` does for each output mode.

use std::io;
use std::path::Path;

use grep_matcher::Matcher;
use grep_regex::RegexMatcher;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};

use super::input::{Input, Mode};

/// One line of content-mode output, before the path is put in front.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Line {
    /// `--` between context groups.
    Break,
    /// `kind` is `:` for a match, `-` for context.
    Hit { number: u64, kind: char, text: String },
    /// A note standing for the file's lines (binary data).
    Note(String),
}

const MAX_COLUMNS: usize = 500;

pub(super) fn searcher(input: &Input) -> Searcher {
    let (before, after) = match input.context {
        Some(c) => (c, c),
        None => (input.before.unwrap_or(0), input.after.unwrap_or(0)),
    };
    let content = input.mode == Mode::Content;
    SearcherBuilder::new()
        .line_number(true)
        .multi_line(input.multiline)
        .binary_detection(BinaryDetection::quit(0))
        .before_context(if content { before as usize } else { 0 })
        .after_context(if content { after as usize } else { 0 })
        .build()
}

pub(super) fn context_breaks(input: &Input) -> bool {
    [input.context, input.before, input.after].iter().any(|c| c.is_some_and(|n| n > 0))
}

/// What one file produced. `lines` stops growing at `max_lines`.
#[derive(Debug, Default)]
pub(super) struct FileHits {
    pub(super) lines: Vec<Line>,
    pub(super) count: u64,
}

pub(super) fn search_file(
    searcher: &mut Searcher,
    matcher: &RegexMatcher,
    path: &Path,
    mode: Mode,
    only_matching: bool,
    breaks: bool,
    max_lines: usize,
) -> io::Result<FileHits> {
    let mut sink = Collect {
        matcher,
        mode,
        only_matching,
        breaks,
        max_lines,
        last_line: None,
        hits: FileHits::default(),
        binary_at: None,
    };
    searcher.search_path(matcher, path, &mut sink)?;
    let mut hits = sink.hits;
    if let (Some(offset), Mode::Content) = (sink.binary_at, mode) {
        hits.lines.retain(|l| !matches!(l, Line::Hit { text, .. } if text.contains('\0')));
        if !hits.lines.is_empty() {
            hits.lines.push(Line::Note(format!("binary file matches (found \"\\0\" byte around offset {offset})")));
        }
    }
    Ok(hits)
}

struct Collect<'m> {
    matcher: &'m RegexMatcher,
    mode: Mode,
    only_matching: bool,
    breaks: bool,
    max_lines: usize,
    last_line: Option<u64>,
    hits: FileHits,
    binary_at: Option<u64>,
}

fn clip(text: &[u8], kind: char) -> String {
    let text = String::from_utf8_lossy(text);
    let text = text.strip_suffix('\r').unwrap_or(&text);
    if text.len() > MAX_COLUMNS && text.chars().count() > MAX_COLUMNS {
        return if kind == ':' { "[Omitted long matching line]" } else { "[Omitted long context line]" }.into();
    }
    text.to_owned()
}

fn strip_eol(bytes: &[u8]) -> &[u8] {
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    bytes.strip_suffix(b"\r").unwrap_or(bytes)
}

impl Collect<'_> {
    fn push_lines(&mut self, first: u64, bytes: &[u8], kind: char) {
        if self.breaks && self.last_line.is_some_and(|last| first > last + 1) {
            self.hits.lines.push(Line::Break);
        }
        let mut n = first;
        for (i, line) in strip_eol(bytes).split(|&b| b == b'\n').enumerate() {
            n = first + i as u64;
            self.hits.lines.push(Line::Hit { number: n, kind, text: clip(line, kind) });
        }
        self.last_line = Some(n);
    }

    fn push_only_matching(&mut self, first: u64, bytes: &[u8]) {
        if self.breaks && self.last_line.is_some_and(|last| first > last + 1) {
            self.hits.lines.push(Line::Break);
        }
        let mut last = first;
        let _ = self.matcher.find_iter(bytes, |m| {
            if m.is_empty() {
                return true;
            }
            let start = first + bytes[..m.start()].iter().filter(|&&b| b == b'\n').count() as u64;
            for (i, piece) in bytes[m].split(|&b| b == b'\n').enumerate() {
                last = start + i as u64;
                self.hits.lines.push(Line::Hit { number: last, kind: ':', text: clip(piece, ':') });
            }
            true
        });
        self.last_line = Some(last);
    }

    fn full(&self) -> bool {
        self.hits.lines.len() > self.max_lines
    }
}

impl Sink for Collect<'_> {
    type Error = io::Error;

    fn matched(&mut self, _: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, io::Error> {
        self.hits.count += 1;
        match self.mode {
            Mode::Files => return Ok(false),
            Mode::Count => return Ok(true),
            Mode::Content => {}
        }
        let first = mat.line_number().unwrap_or(0);
        if self.only_matching {
            self.push_only_matching(first, mat.bytes());
        } else {
            self.push_lines(first, mat.bytes(), ':');
        }
        Ok(!self.full())
    }

    fn context(&mut self, _: &Searcher, ctx: &SinkContext<'_>) -> Result<bool, io::Error> {
        if self.mode != Mode::Content || self.only_matching {
            return Ok(true);
        }
        self.push_lines(ctx.line_number().unwrap_or(0), ctx.bytes(), '-');
        Ok(!self.full())
    }

    fn binary_data(&mut self, _: &Searcher, offset: u64) -> Result<bool, io::Error> {
        self.binary_at = Some(offset);
        Ok(false)
    }
}
