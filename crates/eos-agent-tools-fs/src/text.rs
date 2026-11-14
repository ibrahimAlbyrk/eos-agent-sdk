//! Text encodings the file tools read and write: UTF-8 and UTF-16LE behind its
//! byte-order mark. Anything else is "not text" and never written.

pub(crate) const BOM: char = '\u{feff}';

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Encoding {
    Utf8,
    Utf16Le,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotText {
    NotUtf8,
    Nul,
    Utf16Be,
}

impl NotText {
    fn what(self) -> &'static str {
        match self {
            Self::NotUtf8 => {
                "is not valid UTF-8: it uses a legacy encoding such as Windows-1252, Shift-JIS or GBK, or it is binary"
            }
            Self::Nul => {
                "has NUL bytes in its first 8 KB: binary data, UTF-16/UTF-32 without a byte-order mark, or text with embedded NULs"
            }
            Self::Utf16Be => "is UTF-16 big-endian (it starts FE FF), which these tools cannot write",
        }
    }

    /// Why Edit/Write refuse the file.
    pub(crate) fn refusal(self) -> String {
        format!(
            "File {}. These tools write UTF-8 (or UTF-16LE behind its byte-order mark) and never convert a file's encoding on their own, so nothing was written. If the file is text, make the change with a shell command that reads and writes it in its own encoding (iconv, or PowerShell Get-Content/Set-Content -Encoding), or ask the user whether to convert it to UTF-8 first. To replace the whole file with new UTF-8 text anyway, delete it first, then Write it.",
            self.what()
        )
    }

    /// The note Read appends after the content.
    pub(crate) fn read_note(self) -> String {
        let extra = if self == Self::NotUtf8 { " Its undecodable bytes show above as U+FFFD." } else { "" };
        format!(
            "[This file {}.{extra} Edit and Write will refuse it: change it with a shell command that keeps its encoding, or ask the user whether to convert it to UTF-8.]",
            self.what()
        )
    }
}

/// Sniffs the encoding from the head of a file.
pub(crate) fn sniff(head: &[u8]) -> (Encoding, Option<NotText>) {
    if head.starts_with(&[0xff, 0xfe]) {
        return (Encoding::Utf16Le, None);
    }
    if head.starts_with(&[0xfe, 0xff]) {
        return (Encoding::Utf8, Some(NotText::Utf16Be));
    }
    if head[..head.len().min(8192)].contains(&0) {
        return (Encoding::Utf8, Some(NotText::Nul));
    }
    (Encoding::Utf8, None)
}

/// A whole file as text. A BOM stays in the text, line endings untouched.
#[derive(Debug)]
pub(crate) struct Decoded {
    pub(crate) text: String,
    pub(crate) encoding: Encoding,
    pub(crate) not_text: Option<NotText>,
}

pub(crate) fn decode(bytes: &[u8]) -> Decoded {
    let (encoding, mut not_text) = sniff(bytes);
    let text = match encoding {
        Encoding::Utf16Le => {
            let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
            String::from_utf16_lossy(&units)
        }
        Encoding::Utf8 => match std::str::from_utf8(bytes) {
            Ok(s) => s.to_owned(),
            Err(_) => {
                not_text.get_or_insert(NotText::NotUtf8);
                String::from_utf8_lossy(bytes).into_owned()
            }
        },
    };
    Decoded { text, encoding, not_text }
}

pub(crate) fn encode(text: &str, encoding: Encoding) -> Vec<u8> {
    match encoding {
        Encoding::Utf8 => text.as_bytes().to_vec(),
        Encoding::Utf16Le => text.encode_utf16().flat_map(u16::to_le_bytes).collect(),
    }
}

/// CRLF when CRLF outnumbers bare LF in the first 4096 characters.
pub(crate) fn is_crlf(text: &str) -> bool {
    let head = match text.char_indices().nth(4096) {
        Some((i, _)) => &text[..i],
        None => text,
    };
    let crlf = head.matches("\r\n").count();
    let lf = head.matches('\n').count() - crlf;
    crlf > lf
}

/// BOM dropped, CRLF folded to LF: the text the model sees and edits.
pub(crate) fn normalize(text: &str) -> String {
    text.strip_prefix(BOM).unwrap_or(text).replace("\r\n", "\n")
}

/// Every line break as CRLF, bare LFs included.
pub(crate) fn to_crlf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16le_round_trips() {
        let bytes = encode("\u{feff}héllo\n", Encoding::Utf16Le);
        let d = decode(&bytes);
        assert_eq!(d.encoding, Encoding::Utf16Le);
        assert_eq!(d.text, "\u{feff}héllo\n");
        assert!(d.not_text.is_none());
    }

    #[test]
    fn not_text_is_flagged() {
        assert_eq!(decode(b"a\0b").not_text, Some(NotText::Nul));
        assert_eq!(decode(&[0xfe, 0xff, 0, 65]).not_text, Some(NotText::Utf16Be));
        assert_eq!(decode(&[0x61, 0xff, 0x62]).not_text, Some(NotText::NotUtf8));
    }

    #[test]
    fn line_endings() {
        assert!(is_crlf("a\r\nb\r\nc\n"));
        assert!(!is_crlf("a\nb\r\n"));
        assert_eq!(to_crlf("a\nb\r\nc"), "a\r\nb\r\nc");
        assert_eq!(normalize("\u{feff}a\r\nb"), "a\nb");
    }
}
