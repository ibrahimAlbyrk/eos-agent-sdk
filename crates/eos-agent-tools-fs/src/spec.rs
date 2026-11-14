//! Tool definitions, kept as data files under `prompts/` so the wording can
//! change without touching code.

use eos_agent_core::ToolSpec;

macro_rules! tool_spec {
    ($name:literal) => {
        $crate::spec::build(
            $name,
            include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/prompts/", $name, ".md")),
            include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/prompts/", $name, ".schema.json")),
        )
    };
}
pub(crate) use tool_spec;

pub(crate) fn build(name: &str, description: &str, schema: &str) -> ToolSpec {
    let schema = serde_json::from_str(schema).expect("bundled tool schemas are valid JSON");
    ToolSpec::new(name, description, schema)
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_spec_builds_and_names_itself() {
        for (name, spec) in [
            ("Read", tool_spec!("Read")),
            ("Write", tool_spec!("Write")),
            ("Edit", tool_spec!("Edit")),
            ("Glob", tool_spec!("Glob")),
            ("Grep", tool_spec!("Grep")),
            ("NotebookEdit", tool_spec!("NotebookEdit")),
        ] {
            let v: serde_json::Value = serde_json::from_slice(spec.json()).expect("json");
            assert_eq!(v["name"], name);
            assert_eq!(v["input_schema"]["type"], "object");
            assert!(!v["description"].as_str().unwrap_or("").is_empty());
        }
    }
}
