//! Claude Code's `Zl`: how it makes text from or about an MCP server safe to
//! quote to the model (a failed server's name and error). NFKC, invisible
//! and control characters to spaces, quote and angle characters to spaces,
//! whitespace collapsed, secrets after `token=`, `Bearer` and the like
//! redacted, and the result cut (in UTF-16 units, as JavaScript counts).

use std::sync::LazyLock;

use fancy_regex::{Captures, Regex as FancyRegex};
use regex::Regex;
use unicode_normalization::UnicodeNormalization;

/// `Zl`'s default limit.
pub const QUOTE_LIMIT: usize = 200;
/// Text past this many UTF-16 units is cut before anything else (`qVn`).
const INPUT_LIMIT: usize = 4096;
/// What is looked at for secrets: at least this much (`k`)…
const SCAN_FLOOR: usize = 2000;
/// …or the limit plus this much (`oe`).
const SCAN_SLACK: usize = 512;

/// Invisible, control and separator characters, and spaces other than
/// U+0020 (`p`).
static INVISIBLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[\p{Cc}\p{Cf}\p{Zl}\p{Zp}\p{Default_Ignorable_Code_Point}\x{2800}]|[\p{Zs}--\x{20}]")
        .expect("valid pattern")
});

/// Quote and angle characters and their look-alikes (`ce`, over `YVn`).
const UNSAFE: &[char] = &[
    '<',
    '>',
    '"',
    ';',
    '\u{2018}',
    '\u{2019}',
    '\u{201A}',
    '\u{201C}',
    '\u{201D}',
    '\u{201E}',
    '\u{AB}',
    '\u{BB}',
    '\u{02C2}',
    '\u{02C3}',
    '\u{1433}',
    '\u{1438}',
    '\u{16B2}',
    '\u{2039}',
    '\u{203A}',
    '\u{226E}',
    '\u{226F}',
    '\u{227A}',
    '\u{227B}',
    '\u{2280}',
    '\u{2281}',
    '\u{22D6}',
    '\u{22D7}',
    '\u{2329}',
    '\u{232A}',
    '\u{276C}',
    '\u{276D}',
    '\u{276E}',
    '\u{276F}',
    '\u{2770}',
    '\u{2771}',
    '\u{27E8}',
    '\u{27E9}',
    '\u{2991}',
    '\u{2992}',
    '\u{2993}',
    '\u{2994}',
    '\u{29FC}',
    '\u{29FD}',
    '\u{2A79}',
    '\u{2A7A}',
    '\u{2AA6}',
    '\u{2AA7}',
    '\u{3008}',
    '\u{3009}',
    '\u{FE3F}',
    '\u{FE40}',
    '\u{FE64}',
    '\u{FE65}',
    '\u{FF1C}',
    '\u{FF1E}',
    '\u{16F3F}',
    '\u{1D236}',
    '\u{1D237}',
    '\u{226A}',
    '\u{22D8}',
    '\u{2AA1}',
    '\u{2AF7}',
    '\u{27EA}',
    '\u{FE3D}',
    '\u{226B}',
    '\u{22D9}',
    '\u{2AA2}',
    '\u{2AF8}',
    '\u{27EB}',
    '\u{FE3E}',
    '\u{300A}',
    '\u{300B}',
];

/// A key that names a secret (`O`).
const KEY: &str = r"(?:access[-_ ]?|refresh[-_ ]?|id[-_ ]?|client[-_ ]?|api[-_ ]?|x[-_]api[-_ ]?|session[-_ ]?|auth[-_ ]?)?(?:token|key|secret|password|authorization|credential)s?";
/// A character of a secret value.
const VALUE: &str = r"[A-Za-z0-9._~+/=%-]";
/// JavaScript's `\b` before a word character, or right after an escaped
/// letter or quote (`\n`, `\"`).
const START: &str = r#"(?:(?<![A-Za-z0-9_])|(?<=\\[A-Za-z"']))"#;
const QUOTE: &str = r#"(?:\\*["']|\\+)"#;
const SEPARATOR: &str = r"[:=\x{FF1A}\x{FF1D}]";

fn value() -> String {
    // `fe`: another `key=value` glued on; `ne`: the value, stopping there.
    let glued = format!(
        r"(?<![A-Za-z0-9_]){KEY}{QUOTE}?\s*{SEPARATOR}\s*{QUOTE}?\s*(?!=*{KEY}=*(?!{VALUE}))(?={VALUE}{{8}})[A-Za-z]*[0-9._~+/=%-]"
    );
    format!(r"(?:{VALUE}+?(?={glued})|{VALUE}{{8,}})")
}

/// `de`: `Bearer <secret>`, `Basic <secret>`.
static SCHEME: LazyLock<FancyRegex> = LazyLock::new(|| {
    let pattern = format!(
        r"(?i){START}(bearer|basic)(?![\s:=\x{{FF1A}}\x{{FF1D}}]+(?:{QUOTE}\s*)?{KEY}=*(?!{VALUE}))[\s:=\x{{FF1A}}\x{{FF1D}}]+(?:{QUOTE}\s*)?({})",
        value()
    );
    FancyRegex::new(&pattern).expect("valid pattern")
});

/// `he`: `token=<secret>`, `api_key: <secret>`, ….
static PAIR: LazyLock<FancyRegex> = LazyLock::new(|| {
    let pattern =
        format!(r"(?i){START}({KEY}){QUOTE}?\s*{SEPARATOR}\s*{QUOTE}?\s*(?!=*{KEY}=*(?!{VALUE}))({})", value());
    FancyRegex::new(&pattern).expect("valid pattern")
});

/// Claude Code's `Zl(text, limit)`.
pub fn quoted(text: &str, limit: usize) -> String {
    let normalized: String = text.nfkc().collect();
    let visible = INVISIBLE.replace_all(&cut_input(&normalized), " ").into_owned();
    let safe: String = visible.chars().map(|c| if UNSAFE.contains(&c) { ' ' } else { c }).collect();
    let collapsed = safe.split(' ').filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ");
    let scan = SCAN_FLOOR.max(limit + SCAN_SLACK);
    let long = utf16_len(&collapsed) > scan;
    let redacted = redact(&if long { take_utf16(&collapsed, scan) } else { collapsed });
    if long || utf16_len(&redacted) > limit { format!("{}\u{2026}", take_utf16(&redacted, limit)) } else { redacted }
}

/// `AJt`: secrets replaced by `<key> [redacted]`.
fn redact(text: &str) -> String {
    let once = replace(&SCHEME, text);
    replace(&PAIR, &once)
}

/// JavaScript's `replace` with `te`: a value with no digit or symbol stays.
fn replace(pattern: &FancyRegex, text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    let mut at = 0;
    while at <= text.len() {
        let Ok(Some(caps)) = pattern.captures_from_pos(text, at) else { break };
        let Some(whole) = caps.get(0) else { break };
        out.push_str(&text[last..whole.start()]);
        out.push_str(&replacement(&caps, text, whole.end()));
        last = whole.end();
        at = if whole.end() > whole.start() { whole.end() } else { whole.end() + 1 };
    }
    out.push_str(&text[last.min(text.len())..]);
    out
}

fn replacement(caps: &Captures<'_, str>, text: &str, end: usize) -> String {
    let whole = caps.get(0).map_or("", |m| m.as_str());
    let key = caps.get(1).map_or("", |m| m.as_str());
    let secret = caps.get(2).map_or("", |m| m.as_str());
    if !secret.chars().any(|c| matches!(c, '0'..='9' | '.' | '_' | '~' | '+' | '/' | '=' | '%' | '-')) {
        return whole.to_owned();
    }
    let word_follows = text[end..].chars().next().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
    if word_follows { format!("{key} [redacted] ") } else { format!("{key} [redacted]") }
}

/// The first 4096 UTF-16 units; half a surrogate pair left at the end
/// becomes a space, as `qVn` does.
fn cut_input(text: &str) -> String {
    let mut out = String::new();
    let mut units = 0;
    for c in text.chars() {
        let len = c.len_utf16();
        if units + len > INPUT_LIMIT {
            if units < INPUT_LIMIT {
                out.push(' ');
            }
            break;
        }
        units += len;
        out.push(c);
    }
    out
}

fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

/// `Q`: the first `n` UTF-16 units, without half a surrogate pair.
fn take_utf16(text: &str, n: usize) -> String {
    let mut out = String::new();
    let mut units = 0;
    for c in text.chars() {
        units += c.len_utf16();
        if units > n {
            break;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Inputs and what Claude Code's `Zl` (run in Node) makes of them.
    const NODE: &[(&str, &str)] = &[
        ("  Connection  \"refused\" <now>\n", "Connection refused now"),
        ("Executable not found in $PATH: npx", "Executable not found in $PATH: npx"),
        ("\u{FF21}\u{FF22} \u{FF1C}x\u{FF1E} \u{3008}y\u{3009}", "AB x y"),
        ("a\u{200B}b\u{A0}c\td\u{2028}e\u{2800}f", "a b c d e f"),
        ("HTTP 401: Bearer abc.def-123456 rejected", "HTTP 401: Bearer [redacted] rejected"),
        ("bad token=ghp_1234567890abcdef", "bad token [redacted]"),
        ("api_key: sk-ant-0123456789xyz end", "api_key [redacted] end"),
        ("Authorization=Basic dXNlcjpwYXNz1", "Authorization=Basic [redacted]"),
        ("token=abcdefghijkl", "token=abcdefghijkl"),
        ("token=password", "token=password"),
        ("secret=abc12345678xyz", "secret [redacted]"),
        ("x\\\"token\\\"=abcd1234efgh", "x\\ token [redacted]"),
        ("token=abc12345password=zz9999999xx", "token [redacted]"),
        ("\u{E9}token=abcd1234efgh", "\u{E9}token [redacted]"),
        ("Bearer token=abcdef123456", "Bearer [redacted]"),
        ("access-token: \"abc123456789\"x", "access-token [redacted] x"),
        (
            "AUTH_TOKEN=Ab12345678cd and refresh_token=zz/yy+xx==ww more",
            "AUTH_TOKEN [redacted] and refresh_token [redacted] more",
        ),
        ("SESSION key \u{FF1D} 12345678abcd", "SESSION key [redacted]"),
        ("credentials:abcd1234efgh,ok", "credentials [redacted],ok"),
        ("bearer\tXYZ12345678 tail", "bearer [redacted] tail"),
    ];

    #[test]
    fn matches_claude_code() {
        for (input, expected) in NODE {
            assert_eq!(quoted(input, QUOTE_LIMIT), *expected, "{input:?}");
        }
    }

    #[test]
    fn cuts_in_utf16_units_like_claude_code() {
        assert_eq!(quoted(&"x".repeat(250), QUOTE_LIMIT), format!("{}\u{2026}", "x".repeat(200)));
        let emoji = "\u{1F600}".repeat(150);
        assert_eq!(quoted(&emoji, QUOTE_LIMIT), format!("{}\u{2026}", "\u{1F600}".repeat(100)));
        // Past 2000 units only the start is searched for secrets.
        let long = format!("{} token=abc1234567890", "x".repeat(2100));
        assert_eq!(quoted(&long, QUOTE_LIMIT), format!("{}\u{2026}", "x".repeat(200)));
    }
}
