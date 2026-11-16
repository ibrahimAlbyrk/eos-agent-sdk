//! The bundled tool schemas go to every provider as written. DeepSeek refuses a
//! whole request when one `pattern` writes a character as `\0`, so a NUL is
//! written `\x00`, which every provider accepts with the same meaning.

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::Value;

    fn patterns<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
        match value {
            Value::Object(map) => {
                if let Some(Value::String(p)) = map.get("pattern") {
                    out.push(p);
                }
                map.values().for_each(|v| patterns(v, out));
            }
            Value::Array(items) => items.iter().for_each(|v| patterns(v, out)),
            _ => {}
        }
    }

    #[test]
    fn no_tool_schema_pattern_writes_nul_as_backslash_zero() {
        let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("../crates");
        let mut checked = 0;
        for krate in std::fs::read_dir(&crates).expect("crates dir").flatten() {
            let Ok(prompts) = std::fs::read_dir(krate.path().join("prompts")) else { continue };
            for file in prompts.flatten().map(|f| f.path()) {
                if !file.to_string_lossy().ends_with(".schema.json") {
                    continue;
                }
                let text = std::fs::read_to_string(&file).expect("schema file");
                let schema: Value = serde_json::from_str(&text).expect("schema json");
                let mut found = Vec::new();
                patterns(&schema, &mut found);
                for p in found {
                    assert!(!p.contains("\\0"), "{}: pattern {p:?} writes NUL as \\0; use \\x00", file.display());
                }
                checked += 1;
            }
        }
        assert!(checked > 0, "no tool schemas found under {}", crates.display());
    }
}
