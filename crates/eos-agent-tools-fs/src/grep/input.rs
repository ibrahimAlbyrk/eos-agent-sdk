//! Grep's input, with Claude Code's quiet fixes for common model mistakes
//! (`file_path` for `path`, `-l`/`-c` for `output_mode`, a stray `-r`).

use serde_json::{Map, Value};

use crate::common::{Fail, Res};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    Content,
    Files,
    Count,
}

#[derive(Debug)]
pub(super) struct Input {
    pub(super) pattern: String,
    pub(super) path: Option<String>,
    pub(super) glob: Option<String>,
    pub(super) mode: Mode,
    pub(super) before: Option<u64>,
    pub(super) after: Option<u64>,
    pub(super) context: Option<u64>,
    pub(super) line_numbers: bool,
    pub(super) ignore_case: bool,
    pub(super) only_matching: bool,
    pub(super) file_type: Option<String>,
    /// `None`: the default (250); `Some(0)`: unlimited.
    pub(super) head_limit: Option<u64>,
    pub(super) offset: u64,
    pub(super) multiline: bool,
    /// "Note: …" about fixed-up input, appended to the result.
    pub(super) note: Option<String>,
}

const KNOWN: &[&str] = &[
    "pattern",
    "path",
    "glob",
    "output_mode",
    "-B",
    "-A",
    "-C",
    "context",
    "-n",
    "-i",
    "-o",
    "type",
    "head_limit",
    "offset",
    "multiline",
];

fn invalid(msg: String) -> Fail {
    Fail(format!("InputValidationError: {msg}"))
}

/// `true`/`false` or their strings.
fn flag(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::String(s) if s == "true" => Some(true),
        Value::String(s) if s == "false" => Some(false),
        _ => None,
    }
}

fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok().filter(|f: &f64| f.is_finite()),
        _ => None,
    }
}

fn take_str(map: &mut Map<String, Value>, key: &str) -> Res<Option<String>> {
    match map.remove(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(invalid(format!("{key} must be a string"))),
    }
}

fn take_flag(map: &mut Map<String, Value>, key: &str) -> Res<Option<bool>> {
    map.remove(key).map(|v| flag(&v).ok_or_else(|| invalid(format!("{key} must be a boolean")))).transpose()
}

/// A count of lines or entries: whole and not negative.
fn take_count(map: &mut Map<String, Value>, key: &str, pass_zero: bool) -> Res<Option<u64>> {
    let Some(v) = map.remove(key) else { return Ok(None) };
    let n = number(&v).ok_or_else(|| invalid(format!("{key} must be a number")))?;
    if n < 0.0 || n.fract() != 0.0 {
        let extra = if pass_zero { " Pass 0 for unlimited." } else { "" };
        return Err(Fail(format!("{key} must be a whole number of 0 or more, got {v}.{extra}")));
    }
    Ok(Some(n as u64))
}

pub(super) fn parse(raw: &str) -> Res<Input> {
    let mut map: Map<String, Value> = serde_json::from_str(raw).map_err(|e| invalid(e.to_string()))?;
    let mut notes: Vec<String> = Vec::new();

    if let Some(Value::String(fp)) = map.get("file_path").cloned().filter(|v| v.as_str().is_some_and(|s| !s.is_empty()))
    {
        let fresh = !map.contains_key("path");
        if fresh || map.get("path").and_then(Value::as_str) == Some(fp.as_str()) {
            map.remove("file_path");
            map.insert("path".into(), Value::String(fp));
            notes.push(if fresh {
                "Grep's parameter for where to search is named `path`. `file_path` was read as `path`.".into()
            } else {
                "Grep's parameter for where to search is named `path`. `file_path` repeated `path` and was ignored."
                    .into()
            });
        }
    }
    for (key, mode) in [("-l", "files_with_matches"), ("-c", "count")] {
        if map.get(key).and_then(flag) != Some(true) {
            continue;
        }
        let fresh = !map.contains_key("output_mode");
        if fresh || map.get("output_mode").and_then(Value::as_str) == Some(mode) {
            map.remove(key);
            map.insert("output_mode".into(), Value::String(mode.into()));
            notes.push(if fresh {
                format!("`{key}` is not a Grep parameter and was read as `output_mode: \"{mode}\"`.")
            } else {
                format!("`{key}` repeated `output_mode` and was ignored.")
            });
        }
    }
    if map.get("-r").and_then(flag) == Some(true) {
        map.remove("-r");
        notes.push("`-r` was ignored: Grep always searches a directory recursively.".into());
    }
    if let Some(unknown) = map.keys().find(|k| !KNOWN.contains(&k.as_str())) {
        return Err(invalid(format!("unknown field `{unknown}`")));
    }

    let pattern = take_str(&mut map, "pattern")?.ok_or_else(|| invalid("missing field `pattern`".into()))?;
    let mode = match take_str(&mut map, "output_mode")?.as_deref() {
        None | Some("files_with_matches") => Mode::Files,
        Some("content") => Mode::Content,
        Some("count") => Mode::Count,
        Some(other) => return Err(invalid(format!("unknown output_mode `{other}`"))),
    };
    let context = match take_count(&mut map, "context", false)? {
        Some(c) => {
            map.remove("-C");
            Some(c)
        }
        None => take_count(&mut map, "-C", false)?,
    };
    Ok(Input {
        pattern,
        path: take_str(&mut map, "path")?,
        glob: take_str(&mut map, "glob")?,
        mode,
        before: take_count(&mut map, "-B", false)?,
        after: take_count(&mut map, "-A", false)?,
        context,
        line_numbers: take_flag(&mut map, "-n")?.unwrap_or(true),
        ignore_case: take_flag(&mut map, "-i")?.unwrap_or(false),
        only_matching: take_flag(&mut map, "-o")?.unwrap_or(false),
        file_type: take_str(&mut map, "type")?.filter(|t| !t.is_empty()),
        head_limit: take_count(&mut map, "head_limit", true)?,
        offset: take_count(&mut map, "offset", false)?.unwrap_or(0),
        multiline: take_flag(&mut map, "multiline")?.unwrap_or(false),
        note: (!notes.is_empty()).then(|| format!("Note: {}", notes.join(" "))),
    })
}

/// `glob` split the way Claude Code passes it to rg: on whitespace, then on
/// commas, except inside a `{a,b}` group.
pub(super) fn split_globs(glob: &str) -> Vec<String> {
    let mut out = Vec::new();
    for word in glob.split_whitespace() {
        if word.contains('{') && word.contains('}') {
            out.push(word.to_owned());
        } else {
            out.extend(word.split(',').filter(|g| !g.is_empty()).map(str::to_owned));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixes_common_mistakes_with_a_note() {
        let input = parse(r#"{"pattern":"x","file_path":"src","-l":true,"-r":true}"#).expect("parse");
        assert_eq!(input.path.as_deref(), Some("src"));
        assert_eq!(input.mode, Mode::Files);
        let note = input.note.expect("note");
        assert!(note.starts_with("Note: Grep's parameter for where to search is named `path`."), "{note}");
        assert!(note.contains("`-l` is not a Grep parameter"));
        assert!(note.contains("`-r` was ignored"));
    }

    #[test]
    fn numbers_and_flags_may_be_strings() {
        let input = parse(r#"{"pattern":"x","-C":"2","-i":"true","head_limit":"0"}"#).expect("parse");
        assert_eq!((input.context, input.ignore_case, input.head_limit), (Some(2), true, Some(0)));
        assert!(parse(r#"{"pattern":"x","offset":-1}"#).is_err());
        assert!(parse(r#"{"pattern":"x","bogus":1}"#).is_err());
    }

    #[test]
    fn globs_split_outside_braces() {
        assert_eq!(split_globs("*.ts,*.tsx *.{js,jsx}"), vec!["*.ts", "*.tsx", "*.{js,jsx}"]);
    }
}
