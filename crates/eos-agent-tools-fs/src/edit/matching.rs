//! Finding `old_string` the way Claude Code does — exactly, then with curly
//! quotes straightened, then with `\uXXXX` escapes swapped for their
//! characters (or back) — and shaping `new_string` to match what was found.

const LEFT_SINGLE: char = '\u{2018}';
const RIGHT_SINGLE: char = '\u{2019}';
const LEFT_DOUBLE: char = '\u{201c}';
const RIGHT_DOUBLE: char = '\u{201d}';

/// The text in `file` that `needle` stands for, if any.
pub(super) fn find_actual(file: &str, needle: &str) -> Option<String> {
    if file.contains(needle) {
        return Some(needle.to_owned());
    }
    if let Some(found) = find_quote_normalized(file, needle) {
        return Some(found);
    }
    if has_escape(needle) {
        let decoded = decode_escapes(needle);
        if decoded != needle && file.contains(&decoded) {
            return Some(decoded);
        }
    }
    if !needle.is_ascii() {
        if escaped_len(needle) > file.len() || !file.contains("\\u") {
            return None;
        }
        return find_escaped(needle, file);
    }
    None
}

/// `new` rewritten the way `old` had to be rewritten to become `actual`.
pub(super) fn adapt_new(old: &str, actual: &str, new: &str) -> String {
    if old == actual {
        return new.to_owned();
    }
    let new = restore_quotes(actual, new);
    if !old.is_ascii() && escaped_len(old) == actual.len() && find_escaped(old, actual).is_some() {
        return escape_like(old, actual, &new);
    }
    if has_escape(old) && decode_escapes(old) == actual {
        return decode_escapes(&new);
    }
    new
}

/// One (or every) occurrence replaced. Deleting text that ends a line also
/// deletes that line break.
pub(super) fn apply(file: &str, old: &str, new: &str, replace_all: bool) -> String {
    let with_newline;
    let target = if new.is_empty() && !old.ends_with('\n') && file.contains(&format!("{old}\n")) {
        with_newline = format!("{old}\n");
        with_newline.as_str()
    } else {
        old
    };
    if replace_all { file.replace(target, new) } else { file.replacen(target, new, 1) }
}

fn straighten(c: char) -> char {
    match c {
        LEFT_SINGLE | RIGHT_SINGLE => '\'',
        LEFT_DOUBLE | RIGHT_DOUBLE => '"',
        c => c,
    }
}

fn find_quote_normalized(file: &str, needle: &str) -> Option<String> {
    let norm_file: String = file.chars().map(straighten).collect();
    let norm_needle: String = needle.chars().map(straighten).collect();
    let at = norm_file.find(&norm_needle)?;
    let skip = norm_file[..at].chars().count();
    let take = needle.chars().count();
    let mut chars = file.char_indices().skip(skip);
    let start = chars.next().map(|(i, _)| i)?;
    let end = file[start..].char_indices().nth(take).map_or(file.len(), |(i, _)| start + i);
    Some(file[start..end].to_owned())
}

/// Curly quotes put back into `new` when the file used them.
fn restore_quotes(actual: &str, new: &str) -> String {
    let doubles = actual.contains(LEFT_DOUBLE) || actual.contains(RIGHT_DOUBLE);
    let singles = actual.contains(LEFT_SINGLE) || actual.contains(RIGHT_SINGLE);
    if !doubles && !singles {
        return new.to_owned();
    }
    let chars: Vec<char> = new.chars().collect();
    let opens = |i: usize| {
        i == 0 || matches!(chars[i - 1], ' ' | '\t' | '\n' | '\r' | '(' | '[' | '{' | '\u{2014}' | '\u{2013}')
    };
    chars
        .iter()
        .enumerate()
        .map(|(i, &c)| match c {
            '"' if doubles => {
                if opens(i) {
                    LEFT_DOUBLE
                } else {
                    RIGHT_DOUBLE
                }
            }
            '\'' if singles => {
                let letter = |j: Option<&char>| j.is_some_and(|c| c.is_alphabetic());
                if letter(i.checked_sub(1).and_then(|j| chars.get(j))) && letter(chars.get(i + 1)) {
                    RIGHT_SINGLE
                } else if opens(i) {
                    LEFT_SINGLE
                } else {
                    RIGHT_SINGLE
                }
            }
            c => c,
        })
        .collect()
}

fn is_hex(u: u16) -> bool {
    u8::try_from(u).is_ok_and(|b| b.is_ascii_hexdigit())
}

/// Contains a `\uXXXX` escape.
fn has_escape(s: &str) -> bool {
    s.as_bytes().windows(6).any(|w| w[0] == b'\\' && w[1] == b'u' && w[2..].iter().all(u8::is_ascii_hexdigit))
}

/// `\uXXXX` escapes turned into characters; an escaped backslash (`\\`) is
/// left alone, so `\\u0041` stays as written.
fn decode_escapes(s: &str) -> String {
    let units: Vec<u16> = s.encode_utf16().collect();
    let mut out = Vec::with_capacity(units.len());
    let mut i = 0;
    while i < units.len() {
        let bs = u16::from(b'\\');
        if units[i] == bs && units.get(i + 1) == Some(&bs) {
            out.extend_from_slice(&units[i..i + 2]);
            i += 2;
        } else if units[i] == bs
            && units.get(i + 1) == Some(&u16::from(b'u'))
            && units.len() >= i + 6
            && units[i + 2..i + 6].iter().all(|&u| is_hex(u))
        {
            let hex = String::from_utf16_lossy(&units[i + 2..i + 6]);
            out.push(u16::from_str_radix(&hex, 16).unwrap_or(0));
            i += 6;
        } else {
            out.push(units[i]);
            i += 1;
        }
    }
    String::from_utf16_lossy(&out)
}

/// Length of `s` with every non-ASCII UTF-16 unit written as `\uXXXX`.
fn escaped_len(s: &str) -> usize {
    s.encode_utf16().map(|u| if u < 0x80 { 1 } else { 6 }).sum()
}

fn backslashes_before(s: &[u8], at: usize) -> usize {
    s[..at].iter().rev().take_while(|&&b| b == b'\\').count()
}

/// `needle` with its non-ASCII characters written as `\uXXXX` (any hex
/// case), found unescaped in `file` at its first occurrence.
fn find_escaped(needle: &str, file: &str) -> Option<String> {
    if backslashes_before(needle.as_bytes(), needle.len()) % 2 == 1 {
        return None;
    }
    let units: Vec<u16> = needle.encode_utf16().collect();
    let mut escape_at: Vec<usize> = if needle.starts_with('\\') { vec![0] } else { Vec::new() };
    let mut offset = 0;
    for &u in &units {
        if u >= 0x80 {
            escape_at.push(offset);
            offset += 6;
        } else {
            offset += 1;
        }
    }
    let hay = file.as_bytes();
    let matches_at = |start: usize| -> bool {
        let mut p = start;
        for &u in &units {
            if u < 0x80 {
                if hay.get(p) != Some(&(u as u8)) {
                    return false;
                }
                p += 1;
            } else {
                let Some(esc) = hay.get(p..p + 6) else { return false };
                if &esc[..2] != b"\\u" || !esc[2..].eq_ignore_ascii_case(format!("{u:04x}").as_bytes()) {
                    return false;
                }
                p += 6;
            }
        }
        true
    };
    (0..hay.len().saturating_sub(offset).saturating_add(1))
        .filter(|&s| hay[s].is_ascii() && matches_at(s))
        .find(|&s| escape_at.iter().all(|&a| backslashes_before(hay, s + a).is_multiple_of(2)))
        .and_then(|s| {
            let found = &file[s..s + offset];
            (file.find(found) == Some(s)).then(|| found.to_owned())
        })
}

/// `new`'s non-ASCII characters escaped with the hex the file used for the
/// same characters, else in the case the file mostly used.
fn escape_like(old: &str, actual: &str, new: &str) -> String {
    let mut seen = std::collections::HashMap::new();
    let (mut upper, mut lower) = (0, 0);
    let mut p = 0;
    for u in old.encode_utf16() {
        if u >= 0x80 {
            let hex = &actual[p + 2..p + 6];
            upper += hex.bytes().filter(u8::is_ascii_uppercase).count();
            lower += hex.bytes().filter(u8::is_ascii_lowercase).count();
            seen.insert(u, hex.to_owned());
            p += 6;
        } else {
            p += 1;
        }
    }
    let mut out = String::with_capacity(new.len() * 2);
    for u in new.encode_utf16() {
        if u < 0x80 {
            out.push(char::from(u as u8));
        } else {
            out.push_str("\\u");
            match seen.get(&u) {
                Some(hex) => out.push_str(hex),
                None if upper > lower => out.push_str(&format!("{u:04X}")),
                None => out.push_str(&format!("{u:04x}")),
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn straight_quotes_find_curly_text_and_keep_its_style() {
        let file = "say \u{201c}hi\u{201d} and don\u{2019}t\n";
        let actual = find_actual(file, "say \"hi\" and don't").expect("found");
        assert_eq!(actual, "say \u{201c}hi\u{201d} and don\u{2019}t");
        let new = adapt_new("say \"hi\" and don't", &actual, "say \"bye\" and won't");
        assert_eq!(new, "say \u{201c}bye\u{201d} and won\u{2019}t");
    }

    #[test]
    fn escapes_swap_both_ways() {
        let file = r#"{"name": "café"}"#;
        let actual = find_actual(file, "caf\u{e9}").expect("found");
        assert_eq!(actual, r"café");
        assert_eq!(adapt_new("caf\u{e9}", &actual, "th\u{e9}\u{e9}"), r"théé");

        let file = "caf\u{e9} au lait";
        let actual = find_actual(file, r"café").expect("found");
        assert_eq!(actual, "caf\u{e9}");
        assert_eq!(adapt_new(r"café", &actual, r"crème"), "cr\u{e8}me");
    }

    #[test]
    fn escaped_backslash_is_not_an_escape() {
        assert_eq!(decode_escapes(r"a\\u0041"), r"a\\u0041");
        assert_eq!(find_escaped("\u{e9}", r"\\u00e9"), None);
    }

    #[test]
    fn deleting_a_line_takes_its_newline() {
        assert_eq!(apply("a\nb\nc\n", "b", "", false), "a\nc\n");
        assert_eq!(apply("x x x", "x", "$&y", true), "$&y $&y $&y");
        assert_eq!(apply("x x", "x", "y", false), "y x");
    }
}
