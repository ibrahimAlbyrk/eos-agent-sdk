//! Claude Code's `StructuredOutput` tool: a subagent asked for a structured
//! answer must call it once with an input that matches the caller's JSON
//! Schema; a valid call ends its turn. Validation reports what Ajv (Claude
//! Code's validator) reports, in its words.

use std::sync::Arc;

use eos_agent_core::{BoxFuture, Tool, ToolContext, ToolOutput, ToolSpec};
use serde_json::value::RawValue;
use serde_json::{Map, Value};

pub const STRUCTURED_OUTPUT: &str = "StructuredOutput";
const PROMPT: &str = "Use this tool to return your final response in the requested structured format. You MUST call \
this tool exactly once at the end of your response to provide the structured output.";
/// What a subagent is told when its turn ended without the tool.
pub const STRUCTURED_OUTPUT_NUDGE: &str =
    "[structured-output-enforce] You MUST call the StructuredOutput tool to complete this request. Call this tool now.";

/// `StructuredOutput` over one output schema.
pub struct StructuredOutput {
    spec: ToolSpec,
    schema: Arc<Value>,
}

impl std::fmt::Debug for StructuredOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StructuredOutput").finish_non_exhaustive()
    }
}

impl StructuredOutput {
    pub fn new(schema: Value) -> Self {
        let spec = ToolSpec::new(STRUCTURED_OUTPUT, PROMPT, schema.clone())
            .with_search_hint("return the final response as structured JSON");
        Self { spec, schema: Arc::new(schema) }
    }
}

impl Tool for StructuredOutput {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn is_concurrency_safe(&self, _input: &RawValue) -> bool {
        true
    }

    fn call<'a>(&'a self, input: &'a RawValue, _ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let value: Value = match serde_json::from_str(input.get()) {
                Ok(v) => v,
                Err(e) => return crate::invalid_input(&e),
            };
            let errors = validate(&self.schema, &value);
            if errors.is_empty() {
                ToolOutput::text("Structured output provided successfully").ending_turn()
            } else {
                crate::refused(format_args!("Output does not match required schema: {}", errors.join(", ")))
            }
        })
    }
}

/// Every way `value` fails `schema`, as `<path or root>: <Ajv message>`.
pub fn validate(schema: &Value, value: &Value) -> Vec<String> {
    let mut out = Vec::new();
    check(schema, value, "", &mut out);
    out
}

fn check(schema: &Value, value: &Value, path: &str, out: &mut Vec<String>) {
    let Some(s) = schema.as_object() else {
        if schema == &Value::Bool(false) {
            push(out, path, "boolean schema is false".into());
        }
        return;
    };
    if let Some(types) = types(s)
        && !types.iter().any(|t| is_type(t, value))
    {
        push(out, path, format!("must be {}", types.join(",")));
        return;
    }
    if let Some(values) = s.get("enum").and_then(Value::as_array)
        && !values.contains(value)
    {
        push(out, path, "must be equal to one of the allowed values".into());
    }
    if let Some(c) = s.get("const")
        && c != value
    {
        push(out, path, "must be equal to constant".into());
    }
    match value {
        Value::Object(obj) => object(s, obj, path, out),
        Value::Array(items) => array(s, items, path, out),
        Value::String(text) => string(s, text, path, out),
        Value::Number(n) => number(s, n.as_f64().unwrap_or(0.0), path, out),
        _ => {}
    }
    combinators(s, value, path, out);
}

fn types(s: &Map<String, Value>) -> Option<Vec<String>> {
    let mut out: Vec<String> = match s.get("type")? {
        Value::String(t) => vec![t.clone()],
        Value::Array(list) => list.iter().filter_map(|t| t.as_str().map(str::to_owned)).collect(),
        _ => return None,
    };
    if s.get("nullable") == Some(&Value::Bool(true)) && !out.iter().any(|t| t == "null") {
        out.push("null".into());
    }
    Some(out)
}

fn is_type(t: &str, v: &Value) -> bool {
    match t {
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "number" => v.is_number(),
        "integer" => v.as_f64().is_some_and(|f| f.fract() == 0.0),
        "boolean" => v.is_boolean(),
        "null" => v.is_null(),
        _ => true,
    }
}

fn object(s: &Map<String, Value>, obj: &Map<String, Value>, path: &str, out: &mut Vec<String>) {
    for name in s.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
        if !obj.contains_key(name) {
            push(out, path, format!("must have required property '{name}'"));
        }
    }
    let props = s.get("properties").and_then(Value::as_object);
    for (key, v) in obj {
        let child = format!("{path}/{}", pointer(key));
        match props.and_then(|p| p.get(key)) {
            Some(sub) => check(sub, v, &child, out),
            None => match s.get("additionalProperties") {
                Some(Value::Bool(false)) => {
                    let shown = if key.chars().count() > 80 {
                        format!("{}\u{2026}", key.chars().take(80).collect::<String>())
                    } else {
                        key.clone()
                    };
                    push(out, path, format!("must NOT have additional properties ('{shown}' is not allowed)"));
                }
                Some(sub @ Value::Object(_)) => check(sub, v, &child, out),
                _ => {}
            },
        }
    }
    let count = |k: &str| s.get(k).and_then(Value::as_u64);
    if let Some(min) = count("minProperties")
        && (obj.len() as u64) < min
    {
        push(out, path, format!("must NOT have fewer than {min} properties"));
    }
    if let Some(max) = count("maxProperties")
        && (obj.len() as u64) > max
    {
        push(out, path, format!("must NOT have more than {max} properties"));
    }
}

fn array(s: &Map<String, Value>, items: &[Value], path: &str, out: &mut Vec<String>) {
    if let Some(sub @ Value::Object(_)) = s.get("items") {
        for (i, item) in items.iter().enumerate() {
            check(sub, item, &format!("{path}/{i}"), out);
        }
    }
    let count = |k: &str| s.get(k).and_then(Value::as_u64);
    if let Some(min) = count("minItems")
        && (items.len() as u64) < min
    {
        push(out, path, format!("must NOT have fewer than {min} items"));
    }
    if let Some(max) = count("maxItems")
        && (items.len() as u64) > max
    {
        push(out, path, format!("must NOT have more than {max} items"));
    }
    if s.get("uniqueItems") == Some(&Value::Bool(true)) {
        for i in (0..items.len()).rev() {
            if let Some(j) = items[..i].iter().position(|x| x == &items[i]) {
                push(out, path, format!("must NOT have duplicate items (items ## {j} and {i} are identical)"));
                break;
            }
        }
    }
}

fn string(s: &Map<String, Value>, text: &str, path: &str, out: &mut Vec<String>) {
    let len = text.chars().count() as u64;
    if let Some(min) = s.get("minLength").and_then(Value::as_u64)
        && len < min
    {
        push(out, path, format!("must NOT have fewer than {min} characters (got {len})"));
    }
    if let Some(max) = s.get("maxLength").and_then(Value::as_u64)
        && len > max
    {
        push(out, path, format!("must NOT have more than {max} characters (got {len})"));
    }
    if let Some(pattern) = s.get("pattern").and_then(Value::as_str)
        && let Ok(re) = regex::Regex::new(pattern)
        && !re.is_match(text)
    {
        push(out, path, format!("must match pattern \"{pattern}\""));
    }
}

fn number(s: &Map<String, Value>, n: f64, path: &str, out: &mut Vec<String>) {
    let bound = |k: &str| s.get(k).and_then(Value::as_f64);
    let show = |f: f64| if f.fract() == 0.0 && f.abs() < 1e15 { format!("{f:.0}") } else { f.to_string() };
    if let Some(min) = bound("minimum")
        && n < min
    {
        push(out, path, format!("must be >= {}", show(min)));
    }
    if let Some(max) = bound("maximum")
        && n > max
    {
        push(out, path, format!("must be <= {}", show(max)));
    }
    if let Some(min) = bound("exclusiveMinimum")
        && n <= min
    {
        push(out, path, format!("must be > {}", show(min)));
    }
    if let Some(max) = bound("exclusiveMaximum")
        && n >= max
    {
        push(out, path, format!("must be < {}", show(max)));
    }
    if let Some(m) = bound("multipleOf").filter(|m| *m > 0.0)
        && ((n / m) - (n / m).round()).abs() > 1e-6
    {
        push(out, path, format!("must be multiple of {}", show(m)));
    }
}

fn combinators(s: &Map<String, Value>, value: &Value, path: &str, out: &mut Vec<String>) {
    let passes = |sub: &Value| validate_at(sub, value, path).is_empty();
    if let Some(all) = s.get("allOf").and_then(Value::as_array) {
        for sub in all {
            check(sub, value, path, out);
        }
    }
    if let Some(any) = s.get("anyOf").and_then(Value::as_array)
        && !any.iter().any(passes)
    {
        push(out, path, "must match a schema in anyOf".into());
    }
    if let Some(one) = s.get("oneOf").and_then(Value::as_array)
        && one.iter().filter(|sub| passes(sub)).count() != 1
    {
        push(out, path, "must match exactly one schema in oneOf".into());
    }
    if let Some(not) = s.get("not")
        && passes(not)
    {
        push(out, path, "must NOT be valid".into());
    }
}

fn validate_at(schema: &Value, value: &Value, path: &str) -> Vec<String> {
    let mut out = Vec::new();
    check(schema, value, path, &mut out);
    out
}

fn push(out: &mut Vec<String>, path: &str, message: String) {
    out.push(format!("{}: {message}", if path.is_empty() { "root" } else { path }));
}

fn pointer(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn ajv_messages() {
        let schema = json!({"type": "object", "properties": {
            "findings": {"type": "array", "items": {"type": "object", "properties": {"line": {"type": "integer"}},
                "required": ["line"]}},
            "verdict": {"enum": ["real", "fake"]}
        }, "required": ["findings"], "additionalProperties": false});
        assert!(validate(&schema, &json!({"findings": [{"line": 3}], "verdict": "real"})).is_empty());
        assert_eq!(
            validate(&schema, &json!({"findings": [{"line": 1.5}, {}], "verdict": "x", "extra": 1})),
            vec![
                "root: must NOT have additional properties ('extra' is not allowed)",
                "/findings/0/line: must be integer",
                "/findings/1: must have required property 'line'",
                "/verdict: must be equal to one of the allowed values",
            ]
        );
        assert_eq!(validate(&schema, &json!([])), vec!["root: must be object"]);
        let s = json!({"type": "string", "minLength": 3, "pattern": "^a"});
        assert_eq!(
            validate(&s, &json!("b")),
            vec!["root: must NOT have fewer than 3 characters (got 1)", "root: must match pattern \"^a\""]
        );
    }
}
