//! Checks on a script's tokens before it runs: Claude Code's determinism
//! check for inline scripts (`Date.now`, `Math.random`, argless
//! `new Date()`), and the constructs it refuses at compile time. A small
//! tokenizer skips strings, template text, comments and regular
//! expressions, so none of those can trigger a false alarm.

use std::borrow::Cow;

/// Claude Code's refusal of an inline script that uses the clock or
/// randomness (validation error code 4).
pub const NONDETERMINISTIC: &str = "Workflow scripts must be deterministic: Date.now()/Math.random()/new Date() are \
unavailable (breaks resume). Stamp results after the workflow returns, or pass timestamps via args.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Ident,
    Punct,
    Number,
    /// A string, template or regular expression (contents ignored).
    Literal,
}

#[derive(Debug, Clone, Copy)]
struct Token<'a> {
    kind: Kind,
    text: &'a str,
}

/// Words after which `/` starts a regular expression.
const BEFORE_REGEX: [&str; 15] = [
    "return",
    "typeof",
    "instanceof",
    "in",
    "of",
    "new",
    "delete",
    "void",
    "throw",
    "case",
    "do",
    "else",
    "yield",
    "await",
    "extends",
];

fn tokens(src: &str) -> Vec<Token<'_>> {
    let b = src.as_bytes();
    let mut out: Vec<Token<'_>> = Vec::new();
    // Brace depth at which each open template substitution resumes its template.
    let mut templates: Vec<usize> = Vec::new();
    let mut depth = 0usize;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c >= 0x80 {
            let ch = src[i..].chars().next().unwrap_or(' ');
            if ch.is_whitespace() {
                i += ch.len_utf8();
                continue;
            }
            let start = i;
            i = ident_end(src, i);
            out.push(Token { kind: Kind::Ident, text: &src[start..i] });
            continue;
        }
        if b[i..].starts_with(b"//") {
            i = b[i..].iter().position(|&c| c == b'\n' || c == b'\r').map_or(b.len(), |p| i + p);
            continue;
        }
        if b[i..].starts_with(b"/*") {
            i = b[i + 2..].windows(2).position(|w| w == b"*/").map_or(b.len(), |p| i + 2 + p + 2);
            continue;
        }
        let start = i;
        match c {
            b'\'' | b'"' => {
                i = string_end(b, i);
                out.push(Token { kind: Kind::Literal, text: &src[start..i] });
            }
            b'`' => {
                let (end, opens) = template_end(b, i + 1);
                i = end;
                out.push(Token { kind: Kind::Literal, text: &src[start..i] });
                if opens {
                    templates.push(depth);
                    depth += 1;
                }
            }
            b'}' if templates.last() == Some(&(depth.saturating_sub(1))) && depth > 0 => {
                templates.pop();
                depth -= 1;
                let (end, opens) = template_end(b, i + 1);
                i = end;
                out.push(Token { kind: Kind::Literal, text: &src[start..i] });
                if opens {
                    templates.push(depth);
                    depth += 1;
                }
            }
            b'/' if regex_allowed(out.last()) => {
                i = regex_end(b, i);
                out.push(Token { kind: Kind::Literal, text: &src[start..i] });
            }
            b'0'..=b'9' => {
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'.') {
                    i += 1;
                }
                out.push(Token { kind: Kind::Number, text: &src[start..i] });
            }
            b'.' if b.get(i + 1).is_some_and(u8::is_ascii_digit) => {
                i += 1;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                out.push(Token { kind: Kind::Number, text: &src[start..i] });
            }
            c if c.is_ascii_alphabetic() || c == b'_' || c == b'$' || c == b'\\' => {
                i = ident_end(src, i);
                out.push(Token { kind: Kind::Ident, text: &src[start..i] });
            }
            _ => {
                let len = if b[i..].starts_with(b"?.") && !b.get(i + 2).is_some_and(u8::is_ascii_digit) {
                    2
                } else if b[i..].starts_with(b"...") {
                    3
                } else {
                    1
                };
                match c {
                    b'{' => depth += 1,
                    b'}' => depth = depth.saturating_sub(1),
                    _ => {}
                }
                i += len;
                out.push(Token { kind: Kind::Punct, text: &src[start..i] });
            }
        }
    }
    out
}

fn ident_end(src: &str, mut i: usize) -> usize {
    while let Some(ch) = src[i..].chars().next() {
        if ch.is_alphanumeric() || ch == '_' || ch == '$' || ch == '\\' || (!ch.is_ascii() && !ch.is_whitespace()) {
            i += ch.len_utf8();
        } else {
            break;
        }
    }
    i
}

fn regex_allowed(prev: Option<&Token<'_>>) -> bool {
    match prev {
        None => true,
        Some(t) => match t.kind {
            Kind::Number | Kind::Literal => false,
            Kind::Ident => BEFORE_REGEX.contains(&t.text),
            Kind::Punct => !matches!(t.text, ")" | "]"),
        },
    }
}

fn string_end(b: &[u8], start: usize) -> usize {
    let quote = b[start];
    let mut i = start + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            c if c == quote => return i + 1,
            b'\n' => return i,
            _ => i += 1,
        }
    }
    b.len()
}

/// From just inside a template part to its end: the index after the closing
/// backtick or `${`, and whether it stopped at `${`.
fn template_end(b: &[u8], mut i: usize) -> (usize, bool) {
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'`' => return (i + 1, false),
            b'$' if b.get(i + 1) == Some(&b'{') => return (i + 2, true),
            _ => i += 1,
        }
    }
    (b.len(), false)
}

fn regex_end(b: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    let mut class = false;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'[' => {
                class = true;
                i += 1;
            }
            b']' => {
                class = false;
                i += 1;
            }
            b'/' if !class => {
                i += 1;
                while i < b.len() && b[i].is_ascii_alphabetic() {
                    i += 1;
                }
                return i;
            }
            b'\n' => return i,
            _ => i += 1,
        }
    }
    b.len()
}

/// Claude Code's `EQn`: does the body read the clock or randomness?
/// (`Date.now`, `Math.random` as plain member reads, `new Date` with no
/// arguments.)
pub fn nondeterministic(body: &str) -> bool {
    let t = tokens(body);
    let at = |i: usize| t.get(i).map(|t| t.text);
    let member_object = |i: usize| i == 0 || !matches!(at(i - 1), Some("." | "?."));
    (0..t.len()).any(|i| {
        let plain = t[i].kind == Kind::Ident && member_object(i);
        let member = |object: &str, property: &str| {
            plain && t[i].text == object && matches!(at(i + 1), Some("." | "?.")) && at(i + 2) == Some(property)
        };
        let argless_date = t[i].text == "new"
            && t[i].kind == Kind::Ident
            && at(i + 1) == Some("Date")
            && match at(i + 2) {
                Some("(") => at(i + 3) == Some(")"),
                Some(".") | Some("?.") | Some("[") => false,
                _ => true,
            };
        member("Date", "now") || member("Math", "random") || argless_date
    })
}

/// Claude Code's prefix for the names it adds to a script (ours are the
/// lane operations, see `engine/rewrite.rs`).
pub const RESERVED_PREFIX: &str = "__wRg$";

/// What Claude Code refuses to compile, with its words: `with`, `import()`,
/// `await using` and names with its reserved prefix.
pub fn forbidden(body: &str) -> Option<Cow<'static, str>> {
    let t = tokens(body);
    let at = |i: usize| t.get(i).map(|t| t.text);
    for i in 0..t.len() {
        if t[i].kind != Kind::Ident {
            continue;
        }
        if t[i].text.starts_with(RESERVED_PREFIX) {
            return Some(format!("Identifier '{}' is reserved.", t[i].text).into());
        }
        if i > 0 && matches!(at(i - 1), Some("." | "?.")) {
            continue;
        }
        match t[i].text {
            "with" if at(i + 1) == Some("(") => {
                return Some("'with' statements are not supported in workflow scripts.".into());
            }
            "import" if at(i + 1) == Some("(") => return Some("import() is not available in workflow scripts.".into()),
            "await" if at(i + 1) == Some("using") && t.get(i + 2).is_some_and(|n| n.kind == Kind::Ident) => {
                return Some("'await using' declarations are not supported in workflow scripts.".into());
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_the_clock_and_randomness() {
        for body in [
            "const t = Date.now()",
            "x = Math.random() * 2",
            "const d = new Date()",
            "const d = new Date",
            "Date?.now()",
            "if (a) { return new Date() }",
        ] {
            assert!(nondeterministic(body), "{body}");
        }
    }

    #[test]
    fn no_false_alarms() {
        for body in [
            "log('Date.now() is banned')",
            "// Math.random()\nconst x = 1",
            "/* new Date() */ x()",
            "const s = `use ${a} then Date.now() in text`",
            "const r = /Date.now()/.test(s)",
            "const d = new Date(0)",
            "const d = new Date(args.ts)",
            "x.Date.now()",
            "obj.Math.random()",
            "const a = b / Date / c",
            "new Date.UTC(1)",
        ] {
            assert!(!nondeterministic(body), "{body}");
        }
        // A template substitution is code again.
        assert!(nondeterministic("const s = `at ${Date.now()}`"));
    }

    #[test]
    fn refused_constructs() {
        assert_eq!(
            forbidden("with (obj) { x }").as_deref(),
            Some("'with' statements are not supported in workflow scripts.")
        );
        assert_eq!(
            forbidden("const m = await import('fs')").as_deref(),
            Some("import() is not available in workflow scripts.")
        );
        assert_eq!(
            forbidden("await using res = open()").as_deref(),
            Some("'await using' declarations are not supported in workflow scripts.")
        );
        assert_eq!(forbidden("log('with (x)'); a.import(1); const using = 1"), None);
        assert_eq!(forbidden("const o = {}; o.__wRg$k = 1").as_deref(), Some("Identifier '__wRg$k' is reserved."));
        assert_eq!(forbidden("log('__wRg$k')"), None);
    }
}
