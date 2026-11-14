//! `export const meta = {...}`: the pure literal a script must begin with,
//! read without running anything (Claude Code's literal reader, ported).

use serde_json::{Map, Number, Value};

/// Claude Code's cap on a script (`sO`), in UTF-16 units there; bytes here.
pub const MAX_SCRIPT: usize = 524_288;

const RESERVED: [&str; 3] = ["__proto__", "constructor", "prototype"];
const FIRST_STATEMENT: &str =
    "`export const meta = { name, description, phases }` must be the FIRST statement in the script";

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Phase {
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    pub name: String,
    pub description: String,
    pub title: Option<String>,
    pub when_to_use: Option<String>,
    pub phases: Option<Vec<Phase>>,
}

/// A script split into its meta and the body that runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub meta: Meta,
    pub body: String,
}

/// Claude Code's `FC`: the meta literal and the body after it, or the error
/// it reports (body syntax is checked separately, by the engine).
pub fn parse(script: &str) -> Result<Parsed, String> {
    if script.len() > MAX_SCRIPT {
        return Err(format!("Script exceeds {MAX_SCRIPT} bytes"));
    }
    let Some(open) = meta_start(script) else { return Err(FIRST_STATEMENT.to_owned()) };
    let reader = Reader { s: script.as_bytes(), text: script };
    let (value, end) = reader.object(open).map_err(|e| format!("meta must be a pure literal: {e}"))?;
    let Some(after) = reader.statement_end(end) else { return Err(FIRST_STATEMENT.to_owned()) };
    let meta = validate(&value)?;
    Ok(Parsed { meta, body: body_after(script, after) })
}

/// The body after the meta statement: leading `;` and blank lines dropped.
fn body_after(script: &str, at: usize) -> String {
    let rest = &script[at..];
    let trimmed = match rest.find('\n') {
        Some(nl) if rest[..nl].chars().all(|c| c == ';' || c.is_whitespace()) => &rest[nl + 1..],
        _ => rest,
    };
    trimmed.trim_start().to_owned()
}

/// Where `{` of `export const meta = {` is, after leading comments and space.
fn meta_start(script: &str) -> Option<usize> {
    let s = script.as_bytes();
    let mut i = skip_space(s, 0);
    for word in ["export", "const", "meta"] {
        if !script[i..].starts_with(word) || s.get(i + word.len()).is_some_and(|&c| is_ident(c)) {
            return None;
        }
        i += word.len();
        let next = skip_ws(s, i);
        if next == i && word != "meta" {
            return None;
        }
        i = next;
    }
    if s.get(i) != Some(&b'=') {
        return None;
    }
    i = skip_ws(s, i + 1);
    (s.get(i) == Some(&b'{')).then_some(i)
}

/// Whitespace only (Claude Code's `\s+` between the keywords).
fn skip_ws(s: &[u8], mut i: usize) -> usize {
    while let Some(&c) = s.get(i) {
        if c.is_ascii_whitespace() {
            i += 1;
        } else if c >= 0x80 {
            let ch = std::str::from_utf8(&s[i..]).ok().and_then(|t| t.chars().next());
            match ch {
                Some(ch) if ch.is_whitespace() => i += ch.len_utf8(),
                _ => break,
            }
        } else {
            break;
        }
    }
    i
}

/// Whitespace and comments.
fn skip_space(s: &[u8], mut i: usize) -> usize {
    loop {
        let j = skip_ws(s, i);
        if s[j..].starts_with(b"//") {
            i = s[j..].iter().position(|&c| c == b'\n' || c == b'\r').map_or(s.len(), |p| j + p + 1);
        } else if s[j..].starts_with(b"/*") {
            match s[j + 2..].windows(2).position(|w| w == b"*/") {
                Some(p) => i = j + 2 + p + 2,
                None => return j,
            }
        } else {
            return j;
        }
    }
}

fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c == b'\\' || c >= 0x80
}

struct Reader<'a> {
    s: &'a [u8],
    text: &'a str,
}

type Read<T> = Result<(T, usize), String>;

impl Reader<'_> {
    fn sp(&self, i: usize) -> usize {
        skip_space(self.s, i)
    }

    fn at(&self, i: usize) -> Option<u8> {
        self.s.get(i).copied()
    }

    /// Claude Code's `I`: the meta statement ends at `;`, the end of the
    /// script, or a line break not followed by `in`/`instanceof`.
    fn statement_end(&self, end: usize) -> Option<usize> {
        let t = self.sp(end);
        if self.at(t) == Some(b';') {
            return Some(t + 1);
        }
        if t >= self.s.len() {
            return Some(end);
        }
        let between = &self.text[end..t];
        if !between.contains(['\n', '\r', '\u{2028}', '\u{2029}']) {
            return None;
        }
        match self.ident(t) {
            None => None,
            Some(word) if word == "in" || word == "instanceof" => None,
            Some(_) => Some(end),
        }
    }

    fn ident(&self, i: usize) -> Option<&str> {
        let first = self.at(i)?;
        if !(first.is_ascii_alphabetic() || first == b'_' || first == b'$') {
            return None;
        }
        let len = self.s[i..].iter().take_while(|&&c| c.is_ascii_alphanumeric() || c == b'_' || c == b'$').count();
        if self.at(i + len).is_some_and(is_ident) {
            return None;
        }
        Some(&self.text[i..i + len])
    }

    fn value(&self, i: usize) -> Read<Value> {
        match self.at(i) {
            Some(b'{') => return self.object(i).map(|(v, e)| (Value::Object(v), e)),
            Some(b'[') => return self.array(i),
            Some(b'\'' | b'"' | b'`') => return self.string(i).map(|(s, e)| (Value::String(s), e)),
            Some(b'-') => {
                let (n, e) = self.number(self.sp(i + 1)).ok_or("only negative-number unary allowed in meta")?;
                return Ok((Value::Number(negate(n)), e));
            }
            _ => {}
        }
        if let Some((n, e)) = self.number(i) {
            return Ok((Value::Number(n), e));
        }
        match self.ident(i) {
            Some("true") => Ok((Value::Bool(true), i + 4)),
            Some("false") => Ok((Value::Bool(false), i + 5)),
            Some("null") => Ok((Value::Null, i + 4)),
            Some(_) => Err("non-literal node type in meta: Identifier".into()),
            None if self.at(i).is_none() => Err("Unexpected end of input".into()),
            None => Err(format!("non-literal value in meta at offset {i}")),
        }
    }

    fn object(&self, i: usize) -> Read<Map<String, Value>> {
        let mut out = Map::new();
        let end = self.list(i, b'}', |i| {
            let (key, e) = match self.at(i) {
                Some(b'\'' | b'"') => self.string(i)?,
                Some(b'[') => return Err("computed keys not allowed in meta".into()),
                Some(b'.') if self.s[i..].starts_with(b"...") => return Err("spread not allowed in meta".into()),
                _ => match self.ident(i) {
                    Some(k) => (k.to_owned(), i + k.len()),
                    None => return Err("unsupported key type in meta".into()),
                },
            };
            if RESERVED.contains(&key.as_str()) {
                return Err(format!("reserved key name not allowed in meta: {key}"));
            }
            let colon = self.sp(e);
            match self.at(colon) {
                Some(b':') => {}
                Some(b'(') => return Err("methods/accessors not allowed in meta".into()),
                _ => return Err("only plain properties allowed in meta".into()),
            }
            let (value, e) = self.value(self.sp(colon + 1))?;
            out.insert(key, value);
            Ok(e)
        })?;
        Ok((out, end))
    }

    fn array(&self, i: usize) -> Read<Value> {
        let mut out = Vec::new();
        let end = self.list(i, b']', |i| {
            if self.at(i) == Some(b',') {
                return Err("sparse arrays not allowed".into());
            }
            if self.s[i..].starts_with(b"...") {
                return Err("spread not allowed in meta".into());
            }
            let (v, e) = self.value(i)?;
            out.push(v);
            Ok(e)
        })?;
        Ok((Value::Array(out), end))
    }

    /// `open item (, item)* ,? close`.
    fn list(
        &self,
        open: usize,
        close: u8,
        mut item: impl FnMut(usize) -> Result<usize, String>,
    ) -> Result<usize, String> {
        let mut i = open + 1;
        let mut first = true;
        loop {
            i = self.sp(i);
            if self.at(i) == Some(close) {
                return Ok(i + 1);
            }
            if !first {
                if self.at(i) != Some(b',') {
                    return Err(match self.at(i) {
                        None => "Unexpected end of input".into(),
                        Some(_) => format!("unexpected token at offset {i}"),
                    });
                }
                i = self.sp(i + 1);
                if self.at(i) == Some(close) {
                    return Ok(i + 1);
                }
            }
            first = false;
            i = item(i)?;
        }
    }

    fn number(&self, i: usize) -> Option<(Number, usize)> {
        let s = self.s;
        let mut j = i;
        let digits = |j: &mut usize| {
            let start = *j;
            while s.get(*j).is_some_and(u8::is_ascii_digit) {
                *j += 1;
            }
            *j > start
        };
        match s.get(j) {
            Some(b'0') => j += 1,
            Some(b'1'..=b'9') => {
                digits(&mut j);
            }
            Some(b'.') => {}
            _ => return None,
        }
        if s.get(j) == Some(&b'.') {
            j += 1;
            let had = digits(&mut j);
            if !had && j == i + 1 {
                return None;
            }
        }
        if matches!(s.get(j), Some(b'e' | b'E')) {
            let mut k = j + 1;
            if matches!(s.get(k), Some(b'+' | b'-')) {
                k += 1;
            }
            if digits(&mut k) {
                j = k;
            }
        }
        if s.get(j).is_some_and(|&c| is_ident(c) || c == b'.') {
            return None;
        }
        let text = &self.text[i..j];
        let n: f64 = text.parse().ok()?;
        let number = if n.fract() == 0.0 && n.abs() < 9.0e15 {
            #[expect(clippy::cast_possible_truncation, reason = "integral and in range")]
            Number::from(n as i64)
        } else {
            Number::from_f64(n)?
        };
        Some((number, j))
    }

    fn string(&self, i: usize) -> Read<String> {
        let quote = self.text[i..].chars().next().ok_or("Unexpected end of input")?;
        let template = quote == '`';
        let mut out = String::new();
        let mut chars = self.text[i + 1..].char_indices().peekable();
        while let Some((off, c)) = chars.next() {
            let at = i + 1 + off;
            if c == quote {
                return Ok((out, at + 1));
            }
            match c {
                '\\' => {
                    let (value, consumed) = escape(&self.text[at..]).ok_or("Bad escape sequence in meta")?;
                    out.push_str(&value);
                    // Skip the escape's remaining characters.
                    let end = at + consumed;
                    while chars.peek().is_some_and(|&(o, _)| i + 1 + o < end) {
                        chars.next();
                    }
                }
                '$' if template && chars.peek().is_some_and(|&(_, n)| n == '{') => {
                    return Err("template interpolation not allowed in meta".into());
                }
                '\n' | '\r' if !template => return Err("Unterminated string constant".into()),
                '\r' if template => {
                    out.push('\n');
                    if chars.peek().is_some_and(|&(_, n)| n == '\n') {
                        chars.next();
                    }
                }
                other => out.push(other),
            }
        }
        Err("Unterminated string constant".into())
    }
}

fn negate(n: Number) -> Number {
    if let Some(i) = n.as_i64() {
        return Number::from(-i);
    }
    Number::from_f64(-n.as_f64().unwrap_or(0.0)).unwrap_or(n)
}

/// One escape at the start of `s` (`\…`): its value and its length in bytes.
fn escape(s: &str) -> Option<(String, usize)> {
    let mut it = s.chars();
    it.next()?;
    let c = it.next()?;
    let simple = match c {
        'n' => Some('\n'),
        'r' => Some('\r'),
        't' => Some('\t'),
        'b' => Some('\u{8}'),
        'f' => Some('\u{c}'),
        'v' => Some('\u{b}'),
        _ => None,
    };
    if let Some(v) = simple {
        return Some((v.to_string(), 2));
    }
    match c {
        '0' => {
            if s[2..].starts_with(|d: char| d.is_ascii_digit()) {
                return None;
            }
            Some(("\0".into(), 2))
        }
        '1'..='9' => None,
        'x' => {
            let hex = s.get(2..4)?;
            let v = u32::from_str_radix(hex, 16).ok()?;
            Some((char::from_u32(v)?.to_string(), 4))
        }
        'u' if s[2..].starts_with('{') => {
            let close = s[3..].find('}')?;
            let v = u32::from_str_radix(&s[3..3 + close], 16).ok()?;
            Some((char::from_u32(v)?.to_string(), 3 + close + 1))
        }
        'u' => {
            let v = u32::from_str_radix(s.get(2..6)?, 16).ok()?;
            Some((char::from_u32(v).unwrap_or('\u{fffd}').to_string(), 6))
        }
        '\r' => Some((String::new(), if s[2..].starts_with('\n') { 3 } else { 2 })),
        '\n' | '\u{2028}' | '\u{2029}' => Some((String::new(), 1 + c.len_utf8())),
        other => Some((other.to_string(), 1 + other.len_utf8())),
    }
}

/// Claude Code's `M`: the fields it requires and reads.
fn validate(meta: &Map<String, Value>) -> Result<Meta, String> {
    let text = |key: &str| meta.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);
    let name = text("name").ok_or("meta.name must be a non-empty string")?;
    let description = text("description").ok_or("meta.description must be a non-empty string")?;
    let phases = meta.get("phases").and_then(Value::as_array).map(|list| {
        list.iter()
            .filter_map(|p| {
                let p = p.as_object()?;
                let title = p.get("title")?.as_str()?.to_owned();
                let field = |k: &str| p.get(k).and_then(Value::as_str).map(str::to_owned);
                Some(Phase { title, detail: field("detail"), model: field("model") })
            })
            .collect::<Vec<_>>()
    });
    Ok(Meta {
        name,
        description,
        title: text("title"),
        when_to_use: meta.get("whenToUse").and_then(Value::as_str).map(str::to_owned),
        phases: phases.filter(|p| !p.is_empty()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_canonical_example() {
        let script = "export const meta = {\n  name: 'review-changes',\n  description: 'Review changed files',\n  \
                      phases: [{ title: 'Review' }, { title: 'Verify', detail: \"check\" }],\n}\n\
                      const DIMENSIONS = []\nreturn 1\n";
        let parsed = parse(script).expect("valid");
        assert_eq!(parsed.meta.name, "review-changes");
        assert_eq!(parsed.meta.description, "Review changed files");
        let phases = parsed.meta.phases.expect("phases");
        assert_eq!(phases[1], Phase { title: "Verify".into(), detail: Some("check".into()), model: None });
        assert_eq!(parsed.body, "const DIMENSIONS = []\nreturn 1\n");
    }

    #[test]
    fn comments_semicolons_and_escapes() {
        let script = "// header\n/* block */ export  const meta={name:\"a\\u0041\",description:`multi\nline`,n:-1.5,\
                      t:true,z:null,};\n\n  agent('x')";
        let parsed = parse(script).expect("valid");
        assert_eq!(parsed.meta.name, "aA");
        assert_eq!(parsed.meta.description, "multi\nline");
        assert_eq!(parsed.body, "agent('x')");
    }

    #[test]
    fn claude_codes_errors() {
        let first = Err(FIRST_STATEMENT.to_owned());
        assert_eq!(parse("const x = 1\nexport const meta = {name: 'a', description: 'b'}"), first);
        assert_eq!(parse("export let meta = {name: 'a', description: 'b'}"), first);
        assert_eq!(parse("export const meta = {name: 'a', description: 'b'} + 1"), first);
        let pure = |s: &str| parse(s).expect_err("refused");
        assert_eq!(
            pure("export const meta = {name: NAME, description: 'b'}"),
            "meta must be a pure literal: non-literal node type in meta: Identifier"
        );
        assert_eq!(
            pure("export const meta = {name: `a${b}`, description: 'b'}"),
            "meta must be a pure literal: template interpolation not allowed in meta"
        );
        assert_eq!(
            pure("export const meta = {...x, description: 'b'}"),
            "meta must be a pure literal: spread not allowed in meta"
        );
        assert_eq!(
            pure("export const meta = {[k]: 1}"),
            "meta must be a pure literal: computed keys not allowed in meta"
        );
        assert_eq!(
            pure("export const meta = {__proto__: 1}"),
            "meta must be a pure literal: reserved key name not allowed in meta: __proto__"
        );
        assert_eq!(pure("export const meta = {description: 'b'}"), "meta.name must be a non-empty string");
        assert_eq!(
            pure("export const meta = {name: 'a', description: ''}"),
            "meta.description must be a non-empty string"
        );
        assert_eq!(pure(&"x".repeat(MAX_SCRIPT + 1)), format!("Script exceeds {MAX_SCRIPT} bytes"));
    }

    #[test]
    fn a_line_break_ends_the_statement_unless_in_follows() {
        assert!(parse("export const meta = {name: 'a', description: 'b'}\nin_progress()").is_ok());
        assert!(parse("export const meta = {name: 'a', description: 'b'}\nin (x)").is_err());
    }
}
