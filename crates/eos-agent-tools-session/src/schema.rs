//! A light JSON Schema check for StructuredOutput, with Ajv's wording (what
//! Claude Code reports). It covers `type`, `enum`, `const`, `properties`,
//! `required`, `additionalProperties`, `items`, `anyOf`, the length and item
//! counts and `minimum`/`maximum`. It skips `$ref`, `pattern`, `format`,
//! `allOf`/`oneOf`/`not`, `multipleOf`, `uniqueItems` and the conditional
//! keywords: values those would reject pass.

use serde_json::Value;

/// All problems found, Ajv style (`/path: message`), joined by ", ".
pub(crate) fn validate(schema: &Value, value: &Value) -> Result<(), String> {
    let mut errors = Vec::new();
    check(schema, value, "", &mut errors);
    if errors.is_empty() { Ok(()) } else { Err(errors.join(", ")) }
}

fn check(schema: &Value, value: &Value, path: &str, errors: &mut Vec<String>) {
    let Some(schema) = schema.as_object() else { return };
    let mut fail = |message: String| errors.push(format!("{}: {message}", if path.is_empty() { "root" } else { path }));

    if let Some(types) = schema.get("type") {
        let names: Vec<&str> = match types {
            Value::String(t) => vec![t.as_str()],
            Value::Array(ts) => ts.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        if !names.is_empty() && !names.iter().any(|t| has_type(value, t)) {
            fail(format!("must be {}", names.join(",")));
            return;
        }
    }
    if let Some(Value::Array(allowed)) = schema.get("enum")
        && !allowed.contains(value)
    {
        fail(format!("must be equal to one of the allowed values: {}", Value::Array(allowed.clone())));
    }
    if let Some(expected) = schema.get("const")
        && expected != value
    {
        fail(format!("must be equal to constant: {expected}"));
    }
    if let Some(Value::Array(options)) = schema.get("anyOf") {
        let matches = options.iter().any(|o| {
            let mut inner = Vec::new();
            check(o, value, path, &mut inner);
            inner.is_empty()
        });
        if !matches {
            fail("must match a schema in anyOf".into());
        }
    }
    match value {
        Value::String(s) => {
            let len = s.chars().count() as u64;
            if let Some(min) = bound(schema, "minLength").filter(|m| len < *m) {
                fail(format!("must NOT have fewer than {min} characters (got {len})"));
            }
            if let Some(max) = bound(schema, "maxLength").filter(|m| len > *m) {
                fail(format!("must NOT have more than {max} characters (got {len})"));
            }
        }
        Value::Number(n) => {
            let n = n.as_f64().unwrap_or(f64::NAN);
            if let Some(min) = schema.get("minimum").and_then(Value::as_f64).filter(|m| n < *m) {
                fail(format!("must be >= {min}"));
            }
            if let Some(max) = schema.get("maximum").and_then(Value::as_f64).filter(|m| n > *m) {
                fail(format!("must be <= {max}"));
            }
        }
        Value::Array(items) => {
            let len = items.len() as u64;
            if let Some(min) = bound(schema, "minItems").filter(|m| len < *m) {
                fail(format!("must NOT have fewer than {min} items (got {len})"));
            }
            if let Some(max) = bound(schema, "maxItems").filter(|m| len > *m) {
                fail(format!("must NOT have more than {max} items (got {len})"));
            }
            if let Some(item_schema) = schema.get("items").filter(|s| s.is_object()) {
                for (i, item) in items.iter().enumerate() {
                    check(item_schema, item, &format!("{path}/{i}"), errors);
                }
            }
        }
        Value::Object(fields) => {
            if let Some(Value::Array(required)) = schema.get("required") {
                for name in required.iter().filter_map(Value::as_str) {
                    if !fields.contains_key(name) {
                        fail(format!("must have required property '{name}'"));
                    }
                }
            }
            let properties = schema.get("properties").and_then(Value::as_object);
            if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
                for name in fields.keys().filter(|k| !properties.is_some_and(|p| p.contains_key(*k))) {
                    fail(format!("must NOT have additional properties ('{name}' is not allowed)"));
                }
            }
            for (name, field) in fields {
                let path = format!("{path}/{}", name.replace('~', "~0").replace('/', "~1"));
                if let Some(sub) = properties.and_then(|p| p.get(name)) {
                    check(sub, field, &path, errors);
                } else if let Some(extra) = schema.get("additionalProperties").filter(|s| s.is_object()) {
                    check(extra, field, &path, errors);
                }
            }
        }
        _ => {}
    }
}

fn bound(schema: &serde_json::Map<String, Value>, key: &str) -> Option<u64> {
    schema.get(key).and_then(Value::as_u64)
}

fn has_type(value: &Value, name: &str) -> bool {
    match name {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "number" => value.is_number(),
        "integer" => value.as_f64().is_some_and(|n| n.fract() == 0.0),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn reports_like_ajv() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "minLength": 2},
                "tags": {"type": "array", "items": {"enum": ["a", "b"]}, "maxItems": 2},
                "n": {"type": "integer", "minimum": 0}
            },
            "required": ["name", "n"],
            "additionalProperties": false
        });
        assert!(validate(&schema, &json!({"name": "ok", "n": 3, "tags": ["a"]})).is_ok());
        let err = validate(&schema, &json!({"name": "x", "tags": ["a", "c"], "extra": 1})).expect_err("invalid");
        assert_eq!(
            err,
            "root: must have required property 'n', root: must NOT have additional properties ('extra' is not allowed), \
/name: must NOT have fewer than 2 characters (got 1), \
/tags/1: must be equal to one of the allowed values: [\"a\",\"b\"]"
        );
        assert_eq!(validate(&schema, &json!([])).expect_err("type"), "root: must be object");
        assert_eq!(validate(&json!({"type": "integer"}), &json!(1.5)).expect_err("int"), "root: must be integer");
    }
}
