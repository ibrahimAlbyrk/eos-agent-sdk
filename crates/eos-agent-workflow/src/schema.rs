//! `agent({schema})`: Claude Code's checks of an output schema before a
//! subagent starts — a malformed schema, and one no output can satisfy
//! (its lint `ve`, ported).

use serde_json::{Map, Value};

const MAX_DEPTH: usize = 32;
const MAX_NODES: usize = 100_000;
const TYPES: [&str; 7] = ["object", "array", "string", "integer", "number", "boolean", "null"];
const SHOWN: usize = 80;
const PATH_SHOWN: usize = 300;

/// Why no output can satisfy a schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub reason: &'static str,
    /// The whole output is impossible, or one property / item of it.
    pub whole: bool,
    pub message: String,
}

impl Finding {
    /// Claude Code's `yHt`.
    pub fn consequence(&self) -> &'static str {
        if self.reason == "root_not_object" {
            "the API only accepts an object-rooted tool input schema, so every request would be rejected"
        } else if self.whole {
            "no output can satisfy this schema, so StructuredOutput validation would fail on every attempt"
        } else {
            "one property or item sub-schema admits no value, so any output that fills it in fails validation"
        }
    }
}

/// A schema that is not one at all (Claude Code: Ajv's meta-schema check).
pub fn invalid(schema: &Value) -> Option<String> {
    let Some(obj) = schema.as_object() else { return Some("schema must be object or boolean".into()) };
    if obj.get("$async").is_some_and(|v| v != &Value::Bool(false)) {
        return Some("$async schemas are not supported".into());
    }
    if let Some(t) = obj.get("type") {
        let ok = match t {
            Value::String(s) => TYPES.contains(&s.as_str()),
            Value::Array(list) => list.iter().all(|v| v.as_str().is_some_and(|s| TYPES.contains(&s))),
            _ => false,
        };
        if !ok {
            return Some("data/type must be equal to one of the allowed values".into());
        }
    }
    if obj.get("required").is_some_and(|r| !r.as_array().is_some_and(|a| a.iter().all(Value::is_string))) {
        return Some("data/required must be array".into());
    }
    if obj.get("properties").is_some_and(|p| !p.is_object()) {
        return Some("data/properties must be object".into());
    }
    None
}

/// Claude Code's lint: `None` when some output can satisfy the schema.
pub fn lint(schema: &Value) -> Option<Finding> {
    let obj = schema.as_object()?;
    if obj.get("type").and_then(Value::as_str) != Some("object") {
        return Some(Finding {
            reason: "root_not_object",
            whole: true,
            message: "the root schema must declare type: 'object' (the API rejects any other root type for a tool \
                      input schema); wrap arrays or primitives in an object property"
                .into(),
        });
    }
    let mut budget = MAX_NODES;
    walk(schema, "", true, MAX_DEPTH, &mut budget)
}

fn walk(schema: &Value, path: &str, whole: bool, depth: usize, budget: &mut usize) -> Option<Finding> {
    if depth == 0 || *budget == 0 {
        return None;
    }
    *budget -= 1;
    let obj = schema.as_object()?;
    if obj.contains_key("$ref") {
        return None;
    }
    let types = types_of(obj);
    if let Some((reason, message)) = contradiction(obj, path, types.as_deref()) {
        return Some(Finding { reason, whole, message });
    }
    if admits(types.as_deref(), "object")
        && let Some(Value::Object(props)) = obj.get("properties")
    {
        let required: Vec<&str> = obj
            .get("required")
            .and_then(Value::as_array)
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let only_object = is_only(types.as_deref(), "object");
        for (key, sub) in props {
            if key == "__proto__" {
                continue;
            }
            let sub_path = format!("{path}/properties/{}", pointer(key));
            let whole = whole && only_object && required.contains(&key.as_str());
            if let Some(f) = walk(sub, &sub_path, whole, depth - 1, budget) {
                return Some(f);
            }
        }
    }
    if admits(types.as_deref(), "array")
        && let Some(items @ Value::Object(_)) = obj.get("items")
    {
        return walk(items, &format!("{path}/items"), false, depth - 1, budget);
    }
    None
}

fn types_of(obj: &Map<String, Value>) -> Option<Vec<String>> {
    let mut out: Vec<String> = match obj.get("type")? {
        Value::String(s) if TYPES.contains(&s.as_str()) => vec![s.clone()],
        Value::Array(list)
            if !list.is_empty() && list.iter().all(|v| v.as_str().is_some_and(|s| TYPES.contains(&s))) =>
        {
            list.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()
        }
        _ => return None,
    };
    if obj.get("nullable") == Some(&Value::Bool(true)) && !out.iter().any(|t| t == "null") {
        out.push("null".into());
    }
    Some(out)
}

fn admits(types: Option<&[String]>, t: &str) -> bool {
    types.is_none_or(|ts| ts.iter().any(|x| x == t))
}

fn is_only(types: Option<&[String]>, t: &str) -> bool {
    types.is_some_and(|ts| ts.len() == 1 && ts[0] == t)
}

fn contradiction(obj: &Map<String, Value>, path: &str, types: Option<&[String]>) -> Option<(&'static str, String)> {
    let at =
        if path.is_empty() { "the root object".to_owned() } else { format!("the sub-schema at {}", short_path(path)) };
    if is_only(types, "object")
        && let Some(Value::Array(required)) = obj.get("required")
        && obj.get("additionalProperties") == Some(&Value::Bool(false))
        && !obj.contains_key("patternProperties")
    {
        let props = obj.get("properties").and_then(Value::as_object);
        for name in required.iter().filter_map(Value::as_str) {
            if !props.is_some_and(|p| p.contains_key(name)) {
                return Some((
                    "required_property_forbidden",
                    format!(
                        "{at} lists \"{}\" in required but does not declare it in properties, and additionalProperties \
                         is false, so no object can satisfy it \u{2014} declare the property or drop it from required",
                        shown(name)
                    ),
                ));
            }
        }
    }
    let enumeration = obj.get("enum").and_then(Value::as_array).filter(|e| !e.is_empty());
    if let (Some(types), Some(values)) = (types, enumeration)
        && !values.iter().any(|v| fits(types, v))
    {
        return Some((
            "enum_type_mismatch",
            format!(
                "{at} declares type {} but none of its enum values has that type, so no value can satisfy it \u{2014} \
                 change type to match the enum values or vice versa",
                type_list(types)
            ),
        ));
    }
    if let Some(constant) = obj.get("const") {
        if let Some(types) = types
            && !fits(types, constant)
        {
            return Some((
                "const_mismatch",
                format!(
                    "{at} declares type {} but its const value does not have that type, so no value can satisfy it",
                    type_list(types)
                ),
            ));
        }
        if let Some(values) = enumeration
            && !constant.is_object()
            && !constant.is_array()
            && !values.contains(constant)
        {
            return Some((
                "const_mismatch",
                format!("{at} has a const value that is not one of its enum values, so no value can satisfy it"),
            ));
        }
    }
    bounds(obj, types).map(|b| ("crossed_bounds", format!("{at} has {b}, so no value can satisfy it")))
}

fn bounds(obj: &Map<String, Value>, types: Option<&[String]>) -> Option<String> {
    let num = |k: &str| obj.get(k).and_then(Value::as_f64).filter(|f| f.is_finite());
    let numeric = is_only(types, "number")
        || is_only(types, "integer")
        || types.is_some_and(|t| t.len() == 2 && t.iter().all(|x| x == "number" || x == "integer"));
    if numeric {
        let low = match (num("exclusiveMinimum"), num("minimum")) {
            (Some(x), m) if m.is_none_or(|m| x >= m) => Some((x, true, "exclusiveMinimum")),
            (_, Some(m)) => Some((m, false, "minimum")),
            _ => None,
        };
        let high = match (num("exclusiveMaximum"), num("maximum")) {
            (Some(x), m) if m.is_none_or(|m| x <= m) => Some((x, true, "exclusiveMaximum")),
            (_, Some(m)) => Some((m, false, "maximum")),
            _ => None,
        };
        if let (Some((lv, le, lk)), Some((hv, he, hk))) = (low, high)
            && (lv > hv || (lv == hv && (le || he)))
        {
            return Some(format!("{lk} {} and {hk} {}, which admit no number", js_num(lv), js_num(hv)));
        }
    }
    let pair = |min: &str, max: &str| match (num(min), num(max)) {
        (Some(a), Some(b)) if a > b => Some(format!("{min} {} greater than {max} {}", js_num(a), js_num(b))),
        _ => None,
    };
    if is_only(types, "string") {
        return pair("minLength", "maxLength");
    }
    if is_only(types, "array") {
        return pair("minItems", "maxItems");
    }
    if is_only(types, "object") {
        if let Some(b) = pair("minProperties", "maxProperties") {
            return Some(b);
        }
        if let (Some(max), Some(Value::Array(req))) = (num("maxProperties"), obj.get("required")) {
            let n = req.iter().filter(|v| v.is_string()).count();
            if n as f64 > max {
                return Some(format!("{n} required properties but maxProperties {}", js_num(max)));
            }
        }
    }
    None
}

fn fits(types: &[String], v: &Value) -> bool {
    types.iter().any(|t| match t.as_str() {
        "null" => v.is_null(),
        "boolean" => v.is_boolean(),
        "string" => v.is_string(),
        "number" => v.is_number(),
        "integer" => v.as_f64().is_some_and(|f| f.fract() == 0.0),
        "array" => v.is_array(),
        "object" => v.is_object(),
        _ => true,
    })
}

fn type_list(types: &[String]) -> String {
    if types.len() == 1 {
        format!("'{}'", types[0])
    } else {
        format!("[{}]", types.iter().map(|t| format!("'{t}'")).collect::<Vec<_>>().join(", "))
    }
}

fn js_num(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 1e15 { format!("{f:.0}") } else { f.to_string() }
}

fn shown(s: &str) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > SHOWN {
        format!("{}\u{2026}", flat.chars().take(SHOWN).collect::<String>())
    } else {
        flat
    }
}

fn pointer(key: &str) -> String {
    shown(key).replace('~', "~0").replace('/', "~1")
}

fn short_path(path: &str) -> String {
    let n = path.chars().count();
    if n > PATH_SHOWN {
        format!("\u{2026}{}", path.chars().skip(n - PATH_SHOWN).collect::<String>())
    } else {
        path.into()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn satisfiable_schemas_pass() {
        let s = json!({"type": "object", "properties": {"findings": {"type": "array", "items": {"type": "object",
            "properties": {"title": {"type": "string"}}, "required": ["title"]}}}, "required": ["findings"]});
        assert_eq!(lint(&s), None);
        assert_eq!(invalid(&s), None);
    }

    #[test]
    fn claude_codes_findings() {
        let root = lint(&json!({"type": "array", "items": {}})).expect("finding");
        assert_eq!(root.reason, "root_not_object");
        assert!(root.consequence().starts_with("the API only accepts"));
        let req = lint(&json!({"type": "object", "properties": {}, "required": ["x"], "additionalProperties": false}))
            .expect("finding");
        assert_eq!(
            req.message,
            "the root object lists \"x\" in required but does not declare it in properties, and additionalProperties \
             is false, so no object can satisfy it \u{2014} declare the property or drop it from required"
        );
        assert!(req.whole);
        let sub =
            lint(&json!({"type": "object", "properties": {"n": {"type": "integer", "minimum": 5, "maximum": 1}}}))
                .expect("finding");
        assert_eq!(
            sub.message,
            "the sub-schema at /properties/n has minimum 5 and maximum 1, which admit no number, so no value can satisfy it"
        );
        assert!(!sub.whole);
        let e = lint(
            &json!({"type": "object", "properties": {"k": {"type": "string", "enum": [1, 2]}}, "required": ["k"]}),
        )
        .expect("finding");
        assert_eq!(e.reason, "enum_type_mismatch");
        assert!(e.whole);
    }
}
