use std::sync::{Mutex, PoisonError};

use eos_agent_core::{BoxFuture, Tool, ToolContext, ToolOutput, ToolSpec};
use serde_json::Value;
use serde_json::value::RawValue;

const DESCRIPTION: &str = include_str!("../prompts/StructuredOutput.md");

/// The value an agent returned through StructuredOutput, in its extensions.
#[derive(Debug, Default)]
pub struct StructuredResult(Mutex<Option<Value>>);

impl StructuredResult {
    pub fn get(&self) -> Option<Value> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    pub fn take(&self) -> Option<Value> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).take()
    }

    fn set(&self, value: Value) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(value);
    }
}

/// Claude Code's `StructuredOutput`: the tool's input schema is the shape the
/// caller wants; a call that matches it is kept as the agent's result
/// ([`StructuredResult`]). Claude Code also ends the turn after a successful
/// call; here the host decides when to stop.
#[derive(Debug)]
pub struct StructuredOutput {
    spec: ToolSpec,
    schema: Value,
}

impl StructuredOutput {
    pub fn new(schema: Value) -> Self {
        let spec = ToolSpec::new("StructuredOutput", DESCRIPTION, schema.clone())
            .with_search_hint("return the final response as structured JSON");
        Self { spec, schema }
    }
}

impl Tool for StructuredOutput {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn is_concurrency_safe(&self, _: &RawValue) -> bool {
        true
    }

    fn call<'a>(&'a self, input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        let out = match serde_json::from_str::<Value>(input.get()) {
            Err(e) => ToolOutput::error(format!("Output does not match required schema: {e}")),
            Ok(value) => match crate::schema::validate(&self.schema, &value) {
                Err(problems) => ToolOutput::error(format!("Output does not match required schema: {problems}")),
                Ok(()) => {
                    ctx.extensions.get_or_default::<StructuredResult>().set(value);
                    ToolOutput::text("Structured output provided successfully")
                }
            },
        };
        Box::pin(std::future::ready(out))
    }
}
