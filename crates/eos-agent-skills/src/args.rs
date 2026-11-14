//! Filling a skill body with its arguments: `$ARGUMENTS`, `$ARGUMENTS[n]`,
//! `$n`, named `$arg`, the `${CLAUDE_…}` variables, `\$` escapes, and the
//! `ARGUMENTS: …` line when the body has no placeholder.

use crate::expand::ExpandError;

/// Claude Code's cap on a body with its arguments filled in.
const MAX_EXPANDED: usize = 4_000_000;
const MAX_NAMES: usize = 64;
const MAX_NAME_LEN: usize = 64;

/// Values for `${CLAUDE_SKILL_DIR}` and friends; `None` leaves the text as is.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Vars<'a> {
    pub skill_dir: Option<&'a str>,
    pub project_dir: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub plugin_root: Option<&'a str>,
}

impl Vars<'_> {
    fn get(&self, name: &str) -> Option<&str> {
        match name {
            "CLAUDE_SKILL_DIR" => self.skill_dir,
            "CLAUDE_PROJECT_DIR" => self.project_dir,
            "CLAUDE_SESSION_ID" => self.session_id,
            "CLAUDE_EFFORT" => self.effort,
            "CLAUDE_PLUGIN_ROOT" => self.plugin_root,
            _ => None,
        }
    }
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Shell-style words (quotes group, backslash escapes); plain whitespace
/// splitting when the quotes do not balance.
pub(crate) fn split_args(args: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = args.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some('\''), '\'') | (Some('"'), '"') => quote = None,
            (Some('"'), '\\') if chars.peek().is_some_and(|n| matches!(n, '"' | '\\' | '$' | '`')) => {
                word.extend(chars.next());
            }
            (Some(_), c) => word.push(c),
            (None, c) if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            (None, '\'' | '"') => {
                quote = Some(c);
                in_word = true;
            }
            (None, '\\') => {
                word.extend(chars.next());
                in_word = true;
            }
            (None, c) => {
                word.push(c);
                in_word = true;
            }
        }
    }
    if quote.is_some() {
        return args.split_whitespace().map(str::to_owned).collect();
    }
    if in_word {
        words.push(word);
    }
    words
}

/// Defuses `` !`cmd` `` in substituted text, so arguments cannot smuggle
/// in an inline shell command.
pub(crate) fn defuse(text: &str) -> String {
    let text = text.replace("`!", "` !").replace("!`", "! `");
    let mut out = String::with_capacity(text.len() + 4);
    let mut prev_space = true;
    for c in text.chars() {
        if c == '!' && prev_space {
            out.push('\\');
        }
        out.push(c);
        prev_space = c.is_whitespace();
    }
    out
}

pub(crate) fn substitute(body: &str, args: &str, names: &[String], vars: &Vars<'_>) -> Result<String, ExpandError> {
    let mut named: Vec<(&str, usize)> = names
        .iter()
        .enumerate()
        .filter(|(_, n)| !n.is_empty() && n.len() <= MAX_NAME_LEN)
        .take(MAX_NAMES)
        .map(|(i, n)| (n.as_str(), i))
        .collect();
    named.sort_by_key(|(n, _)| std::cmp::Reverse(n.len()));
    let mut words: Option<Vec<String>> = None;
    let mut word = |i: usize| words.get_or_insert_with(|| split_args(args)).get(i).cloned();

    let bytes = body.as_bytes();
    let mut out = String::with_capacity(body.len());
    let mut used = false;
    let (mut i, mut last) = (0, 0);
    // A name placeholder at `at` (just after the `$`), with its length.
    let name_at = |at: usize| {
        named.iter().find(|(n, _)| {
            body[at..].starts_with(n) && bytes.get(at + n.len()).is_none_or(|&b| b != b'[' && !is_word(b))
        })
    };
    let digits = |at: usize| bytes[at.min(bytes.len())..].iter().take_while(|b| b.is_ascii_digit()).count();

    while i < bytes.len() {
        let rest = &body[i..];
        if bytes[i] == b'\\' && rest[1..].starts_with('$') && (i == 0 || bytes[i - 1] != b'\\') {
            let at = i + 2;
            if digits(at) > 0 || body[at..].starts_with("ARGUMENTS") || name_at(at).is_some() {
                out.push_str(&body[last..i]);
                out.push('$');
                i = at;
                last = i;
                continue;
            }
        }
        if bytes[i] != b'$' {
            i += 1;
            continue;
        }
        let at = i + 1;
        let mut value: Option<String> = None;
        let mut end = at;
        if let Some(var) = rest.strip_prefix("${").and_then(|r| r.split_once('}')).map(|(v, _)| v)
            && let Some(v) = vars.get(var)
        {
            value = Some(v.to_owned());
            end = at + var.len() + 2;
        }
        if value.is_none()
            && let Some(&(name, index)) = name_at(at)
        {
            value = Some(defuse(&word(index).unwrap_or_default()));
            end = at + name.len();
            used = true;
        }
        if value.is_none() && body[at..].starts_with("ARGUMENTS[") {
            let n = digits(at + 10);
            if n > 0 && bytes.get(at + 10 + n) == Some(&b']') {
                end = at + 11 + n;
                match body[at + 10..at + 10 + n].parse().ok().and_then(&mut word) {
                    Some(w) => {
                        value = Some(defuse(&w));
                        used = true;
                    }
                    None => value = Some(body[i..end].to_owned()),
                }
            }
        }
        if value.is_none() {
            let n = digits(at);
            if n > 0 && bytes.get(at + n).is_none_or(|&b| !is_word(b)) {
                end = at + n;
                if let Some(w) = body[at..end].parse().ok().and_then(&mut word) {
                    value = Some(defuse(&w));
                    used = true;
                }
            }
        }
        if value.is_none() && body[at..].starts_with("ARGUMENTS") {
            value = Some(defuse(args));
            end = at + 9;
            used = true;
        }
        match value {
            Some(v) => {
                out.push_str(&body[last..i]);
                out.push_str(&v);
                i = end;
                last = i;
                if out.len() > MAX_EXPANDED {
                    return Err(ExpandError::TooLong);
                }
            }
            None => i += 1,
        }
    }
    out.push_str(&body[last..]);
    if !used && !args.is_empty() {
        out.push_str("\n\nARGUMENTS: ");
        out.push_str(&defuse(args));
    }
    if out.len() > MAX_EXPANDED {
        return Err(ExpandError::TooLong);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub(body: &str, args: &str, names: &[&str]) -> String {
        let names: Vec<String> = names.iter().map(|s| (*s).to_owned()).collect();
        substitute(body, args, &names, &Vars::default()).expect("expands")
    }

    #[test]
    fn all_placeholder_forms() {
        assert_eq!(sub("Fix $ARGUMENTS now", "issue 12", &[]), "Fix issue 12 now");
        assert_eq!(sub("a=$0 b=$1 c=$2", "x \"y z\"", &[]), "a=x b=y z c=$2");
        assert_eq!(sub("$ARGUMENTS[1] / $ARGUMENTS[5]", "p q", &[]), "q / $ARGUMENTS[5]");
        assert_eq!(sub("env=$env region=$region $envx", "prod eu", &["env", "region"]), "env=prod region=eu $envx");
        assert_eq!(sub("missing=$region.", "prod", &["env", "region"]), "missing=.");
        // Escaped placeholders stay literal and do not count as used.
        assert_eq!(sub(r"cost \$1 and \$ARGUMENTS, \$HOME", "", &[]), r"cost $1 and $ARGUMENTS, \$HOME");
        assert_eq!(sub(r"\$0 $0", "a", &[]), "$0 a");
    }

    #[test]
    fn appends_unused_arguments() {
        assert_eq!(sub("No placeholders.", "do it", &[]), "No placeholders.\n\nARGUMENTS: do it");
        assert_eq!(sub("No placeholders.", "", &[]), "No placeholders.");
        assert_eq!(sub("Only $5 here", "one", &[]), "Only $5 here\n\nARGUMENTS: one");
    }

    #[test]
    fn variables_and_defusing() {
        let vars = Vars { skill_dir: Some("/s"), ..Vars::default() };
        let out = substitute("${CLAUDE_SKILL_DIR}/run ${CLAUDE_PROJECT_DIR} $ARGUMENTS", "!`rm -rf /`", &[], &vars)
            .expect("expands");
        assert_eq!(out, "/s/run ${CLAUDE_PROJECT_DIR} \\! `rm -rf /`");
        assert_eq!(split_args("a 'b c' \"d\\\"e\" f\\ g"), ["a", "b c", "d\"e", "f g"]);
        assert_eq!(split_args("it's broken"), ["it's", "broken"]);
    }
}
