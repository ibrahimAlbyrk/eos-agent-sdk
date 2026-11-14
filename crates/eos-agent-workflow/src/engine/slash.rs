//! Claude Code's regex-or-division check (`jr` in its workflow compiler;
//! behind a remote flag, `tengu_steady_metcalfe`, that is on unless the
//! server turns it off). The script is rewritten from what the parser read,
//! so the engine must read every `/` the same way: each one is made
//! unambiguous — a regular expression becomes `/a/.source`, a division `%`
//! — and the engine must still parse the result. The first `/` whose
//! rewrite breaks the parse (found by bisection) is refused, before and
//! again after the resume rewrite.

use oxc_allocator::Allocator;
use oxc_parser::config::TokensParserConfig;
use oxc_parser::{Kind, Parser};
use oxc_span::SourceType;

use super::rewrite::rewrite;

/// The wrapper Claude Code parses the body in for this check (its `Gs`).
const HEAD: &str = "(async () => {'use strict';\n";
const TAIL: &str = "\n})()";

/// A `/` as the parser read it: a regular expression, or a division (`/`,
/// `/=`). Byte offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Slash {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) regex: bool,
}

/// Every `/` of `source` (a whole script), or `None` when it does not parse.
pub(crate) fn slashes(source: &str) -> Option<Vec<Slash>> {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, SourceType::script()).with_config(TokensParserConfig).parse();
    if !parsed.diagnostics.is_empty() {
        return None;
    }
    let slash = |t: &oxc_parser::Token, regex| Slash { start: t.start() as usize, end: t.end() as usize, regex };
    Some(
        parsed
            .tokens
            .iter()
            .filter_map(|t| match t.kind() {
                Kind::RegExp => Some(slash(t, true)),
                Kind::Slash | Kind::SlashEq => Some(slash(t, false)),
                _ => None,
            })
            .collect(),
    )
}

/// The body's own slashes (none from the wrapper it runs in), or `None`
/// when it has none.
pub(crate) fn body_slashes(body: &str) -> Option<Vec<Slash>> {
    let head = HEAD.len();
    let own: Vec<Slash> = slashes(&format!("{HEAD}{body}{TAIL}"))?
        .into_iter()
        .filter(|s| s.start >= head && s.end <= head + body.len())
        .map(|s| Slash { start: s.start - head, end: s.end - head, ..s })
        .collect();
    (!own.is_empty()).then_some(own)
}

/// Claude Code's `Vr`: the body as an async function's strict body, in an
/// expression that compiles it without calling it.
pub(crate) fn check_wrapper(body: &str) -> String {
    format!("(function () {{ async function _check() {{'use strict';\n{body}\n}} }})")
}

/// The refusal for `body`, whose slashes are `own`: checked as written
/// (`as_written` compiles a [`check_wrapper`]) and as rewritten for resume
/// (`as_run` compiles the rewritten script), Claude Code's two passes.
pub(crate) fn refusal(
    body: &str,
    own: &[Slash],
    as_written: impl Fn(&str) -> bool,
    as_run: impl Fn(&str) -> bool,
) -> Option<String> {
    let found = first_ambiguous(body, own, as_written).or_else(|| {
        let code = rewrite(body).ok()?.code;
        // The rewrite adds no slash: the same ones, in the same order.
        let theirs = slashes(&code).filter(|t| t.len() == own.len())?;
        first_ambiguous(&code, &theirs, as_run)
    })?;
    Some(message(body, own[found].start))
}

/// `text` with its first `count` slashes made unambiguous.
fn disambiguated(text: &str, slashes: &[Slash], count: usize) -> String {
    let mut out = String::with_capacity(text.len() + count * 9);
    let mut from = 0;
    for s in &slashes[..count] {
        out.push_str(&text[from..s.start]);
        if s.regex {
            out.push_str("/a/.source");
        } else {
            out.push('%');
            out.push_str(&text[s.start + 1..s.end]);
        }
        from = s.end;
    }
    out.push_str(&text[from..]);
    out
}

/// Claude Code's `Ur`: the index of the first slash the engine reads
/// another way, or `None` when it reads them all as the parser did (or
/// cannot parse `text` at all, which other checks report).
pub(crate) fn first_ambiguous(text: &str, slashes: &[Slash], parses: impl Fn(&str) -> bool) -> Option<usize> {
    if slashes.is_empty() || parses(&disambiguated(text, slashes, slashes.len())) || !parses(text) {
        return None;
    }
    let (mut low, mut high) = (1, slashes.len());
    while low < high {
        let mid = (low + high) / 2;
        if parses(&disambiguated(text, slashes, mid)) {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    Some(low - 1)
}

/// Claude Code's refusal for the slash at byte `at` of the body; the column
/// counts UTF-16 units, as JavaScript does.
pub(crate) fn message(body: &str, at: usize) -> String {
    let before = &body[..at];
    let (mut line, mut last) = (1, 0);
    let mut chars = before.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let end = match c {
            '\r' if chars.peek().is_some_and(|(_, n)| *n == '\n') => {
                chars.next();
                i + 2
            }
            '\r' | '\n' | '\u{2028}' | '\u{2029}' => i + c.len_utf8(),
            _ => continue,
        };
        line += 1;
        last = end;
    }
    let column = before[last..].encode_utf16().count() + 1;
    format!(
        "Line {line}, column {column} of the script body (counted after the meta statement, if any): this '/' could be \
         read either as the start of a regular expression or as a division, so the script was not run. End the \
         statement before it with ';' (or parenthesize the expression) so it reads one way."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_regexes_and_divisions() {
        let found = slashes("let a = 4 / 2; a /= 2; const r = /x\\/y/g.test('s')").expect("parses");
        assert_eq!(found.iter().map(|s| s.regex).collect::<Vec<_>>(), [false, false, true]);
        assert_eq!(found[2].end - found[2].start, "/x\\/y/g".len());
        let text = "a / b; /c/i";
        let s = slashes(text).expect("parses");
        assert_eq!(disambiguated(text, &s, 2), "a % b; /a/.source");
    }

    #[test]
    fn bisects_to_the_first_slash_read_otherwise() {
        let text = "a / b; c / d; /e/; f / g";
        let s = slashes(text).expect("parses");
        // An engine that rejects a rewritten third slash (the regex).
        let parses = |t: &str| !t.contains("/a/.source");
        assert_eq!(first_ambiguous(text, &s, parses), Some(2));
        assert_eq!(first_ambiguous(text, &s, |_| true), None);
    }

    #[test]
    fn the_check_wrapper_only_compiles() {
        let rt = rquickjs::Runtime::new().expect("runtime");
        let ctx = rquickjs::Context::full(&rt).expect("context");
        ctx.with(|ctx| {
            let compiles = |body: &str| {
                let ok = ctx.eval::<rquickjs::Value, _>(check_wrapper(body)).is_ok();
                let _ = ctx.catch();
                ok
            };
            assert!(compiles("globalThis.ran = true; return await 1 / 2"));
            assert!(ctx.eval::<Option<bool>, _>("globalThis.ran").expect("ran").is_none());
            assert!(!compiles("return 1 %/ 2"));
            assert!(compiles("return /a/.source % 2"));
        });
        let body = "const r = /x/.test('x') / 2";
        let own = body_slashes(body).expect("slashes");
        assert_eq!(own.len(), 2);
        assert_eq!(refusal(body, &own, |_| true, |_| true), None);
        let refused = refusal(body, &own, |t| !t.contains('%'), |_| true).expect("refused");
        assert!(refused.starts_with("Line 1, column 25 of the script body"), "{refused}");
    }

    #[test]
    fn places_the_slash_like_claude_code() {
        let body = "const x = 1\r\nconst \u{e9}\u{1f600} = 2 / 3";
        let at = body.find(" / ").expect("slash") + 1;
        assert!(message(body, at).starts_with("Line 2, column 15 of the script body"));
        assert!(message("a\u{2028}b\r\rc", 8).starts_with("Line 4, column 2 "));
    }
}
