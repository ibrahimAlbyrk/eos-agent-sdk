//! Streams a text file once: picks the requested line window, counts every
//! line and hashes every byte, holding no more than the window in memory.

use std::fs::{File, Metadata};
use std::io::{BufRead, BufReader, Cursor, Read};
use std::path::Path;

use eos_agent_core::CancellationToken;

use crate::common::Fail;
use crate::fsio::Fnv;
use crate::text::{BOM, Encoding, NotText, decode, sniff};

/// Zero-based first line and how many lines to take.
#[derive(Debug, Clone, Copy)]
pub(super) struct Window {
    pub(super) start: u64,
    pub(super) limit: Option<u64>,
}

#[derive(Debug)]
pub(super) struct Scanned {
    /// The window's lines joined with `\n`, CRs and BOM dropped.
    pub(super) content: String,
    pub(super) lines: u64,
    pub(super) total_lines: u64,
    /// Hash of the whole file's raw bytes.
    pub(super) hash: u64,
    pub(super) meta: Metadata,
    pub(super) not_text: Option<NotText>,
}

#[derive(Debug)]
pub(super) enum ScanError {
    Fail(Fail),
    NotFound,
    /// Whole-file read over the byte cap (file size).
    TooLarge(u64),
    /// The window holds more than the selection cap (bytes so far).
    RangeTooLarge(u64),
}

impl From<std::io::Error> for ScanError {
    fn from(e: std::io::Error) -> Self {
        match e.kind() {
            std::io::ErrorKind::NotFound => Self::NotFound,
            _ => Self::Fail(Fail(e.to_string())),
        }
    }
}

const CHUNK: usize = 64 * 1024;

/// `max_size` caps the file when no limit is given; `max_selected` caps the
/// window's bytes when one is.
pub(super) fn scan(
    path: &Path,
    window: Window,
    max_size: u64,
    max_selected: u64,
    cancel: &CancellationToken,
) -> Result<Scanned, ScanError> {
    let file = File::open(path)?;
    let meta = file.metadata()?;
    if meta.is_dir() {
        let msg = format!("EISDIR: illegal operation on a directory, read '{}'", path.display());
        return Err(ScanError::Fail(Fail(msg)));
    }
    if window.limit.is_none() && meta.len() > max_size {
        return Err(ScanError::TooLarge(meta.len()));
    }
    let max_selected = if window.limit.is_some() && meta.len() > max_selected { Some(max_selected) } else { None };
    let mut reader = BufReader::with_capacity(CHUNK, file);
    let (encoding, mut not_text) = sniff(reader.fill_buf()?);
    let mut lines = Lines::new(window, max_selected);
    if encoding == Encoding::Utf16Le {
        let mut raw = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
        reader.read_to_end(&mut raw)?;
        let mut hash = Fnv::default();
        hash.update(&raw);
        let text = decode(&raw).text;
        drop(raw);
        lines.run(Cursor::new(text.as_bytes()), false, cancel)?;
        lines.hash = hash;
    } else {
        lines.run(&mut reader, true, cancel)?;
    }
    if lines.bad_utf8 {
        not_text.get_or_insert(NotText::NotUtf8);
    }
    Ok(Scanned {
        content: lines.content,
        lines: lines.taken,
        total_lines: lines.index,
        hash: lines.hash.finish(),
        meta,
        not_text,
    })
}

struct Lines {
    window: Window,
    max_selected: Option<u64>,
    content: String,
    line: Vec<u8>,
    index: u64,
    taken: u64,
    hash: Fnv,
    bad_utf8: bool,
}

impl Lines {
    fn new(window: Window, max_selected: Option<u64>) -> Self {
        Self {
            window,
            max_selected,
            content: String::new(),
            line: Vec::new(),
            index: 0,
            taken: 0,
            hash: Fnv::default(),
            bad_utf8: false,
        }
    }

    fn selected(&self) -> bool {
        self.index >= self.window.start && self.window.limit.is_none_or(|l| self.index < self.window.start + l)
    }

    /// `hash_raw`: hash the bytes as they stream by.
    fn run(&mut self, mut reader: impl BufRead, hash_raw: bool, cancel: &CancellationToken) -> Result<(), ScanError> {
        loop {
            if cancel.is_cancelled() {
                return Err(ScanError::Fail(Fail("The read was cancelled.".into())));
            }
            let chunk = reader.fill_buf()?;
            if chunk.is_empty() {
                break;
            }
            if hash_raw {
                self.hash.update(chunk);
            }
            let len = chunk.len();
            let mut rest = chunk;
            while let Some(pos) = rest.iter().position(|&b| b == b'\n') {
                if self.selected() {
                    self.line.extend_from_slice(&rest[..pos]);
                    self.take_line()?;
                }
                self.index += 1;
                rest = &rest[pos + 1..];
            }
            if self.selected() {
                self.line.extend_from_slice(rest);
                self.check_size(self.line.len())?;
            }
            reader.consume(len);
        }
        if self.selected() {
            self.take_line()?;
        }
        self.index += 1;
        Ok(())
    }

    fn take_line(&mut self) -> Result<(), ScanError> {
        let mut bytes = std::mem::take(&mut self.line);
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        let sep = usize::from(self.taken > 0);
        self.check_size(sep + bytes.len())?;
        if sep == 1 {
            self.content.push('\n');
        }
        match std::str::from_utf8(&bytes) {
            Ok(s) => self.content.push_str(s),
            Err(_) => {
                self.bad_utf8 = true;
                self.content.push_str(&String::from_utf8_lossy(&bytes));
            }
        }
        if self.index == 0 && self.content.starts_with(BOM) {
            self.content.drain(..BOM.len_utf8());
        }
        self.taken += 1;
        bytes.clear();
        self.line = bytes;
        Ok(())
    }

    fn check_size(&self, extra: usize) -> Result<(), ScanError> {
        let total = (self.content.len() + extra) as u64;
        match self.max_selected {
            Some(max) if total > max => Err(ScanError::RangeTooLarge(total)),
            _ => Ok(()),
        }
    }
}

/// `N\tline` per line, numbered from `start_line`.
pub(super) fn numbered(content: &str, start_line: u64) -> String {
    let mut out = String::with_capacity(content.len() + content.len() / 8 + 16);
    for (i, line) in content.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&(start_line + i as u64).to_string());
        out.push('\t');
        out.push_str(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines_of(data: &str, start: u64, limit: Option<u64>) -> (String, u64, u64) {
        let mut l = Lines::new(Window { start, limit }, None);
        l.run(Cursor::new(data.as_bytes()), true, &CancellationToken::new()).expect("scan");
        (l.content, l.taken, l.index)
    }

    #[test]
    fn counts_like_claude_code() {
        assert_eq!(lines_of("a\nb\n", 0, None), ("a\nb\n".into(), 3, 3));
        assert_eq!(lines_of("", 0, None), (String::new(), 1, 1));
        assert_eq!(lines_of("a\r\nb\r\nc", 1, Some(1)), ("b".into(), 1, 3));
        assert_eq!(lines_of("a\nb", 5, None), (String::new(), 0, 2));
    }

    #[test]
    fn numbers_without_padding() {
        assert_eq!(numbered("x\n\ty", 9), "9\tx\n10\t\ty");
    }
}
