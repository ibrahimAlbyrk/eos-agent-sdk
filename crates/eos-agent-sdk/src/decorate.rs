//! Tool decorators: one that sends a tool with `defer_loading`, one that
//! moves an oversized text result to a file and hands back a preview. The
//! per-model wordings are in [`crate::wording`].

use std::path::PathBuf;
use std::sync::Arc;

use eos_agent_core::{
    BoxFuture, Extensions, StoredMessage, Tool, ToolContext, ToolOutput, ToolReview, ToolSpec, Workspace,
    persisted_output,
};
use eos_agent_types::ToolResultContent;
use serde_json::value::RawValue;

pub(crate) use crate::wording::{classic, full_tools};

/// Claude Code's `maxResultSizeChars` per tool; `None` = never spilled
/// (Read bounds itself; MCP tools apply their own token cap).
pub fn result_limit(tool: &str) -> Option<usize> {
    match tool {
        "Read" => None,
        _ if tool.starts_with("mcp__") => None,
        "Grep" => Some(20_000),
        _ => Some(100_000),
    }
}

/// Sends `inner` with `defer_loading`, so the model loads it through ToolSearch.
pub struct Deferred {
    inner: Arc<dyn Tool>,
    spec: ToolSpec,
}

impl Deferred {
    pub fn wrap(inner: Arc<dyn Tool>, search_hint: &str) -> Arc<dyn Tool> {
        if inner.spec().is_deferred() {
            return inner;
        }
        let mut spec = inner.spec().clone().deferred();
        if spec.search_hint().is_none() {
            spec = spec.with_search_hint(search_hint);
        }
        Arc::new(Self { inner, spec })
    }
}

impl std::fmt::Debug for Deferred {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Deferred").field(&self.spec.name()).finish()
    }
}

impl Tool for Deferred {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn is_concurrency_safe(&self, input: &RawValue) -> bool {
        self.inner.is_concurrency_safe(input)
    }

    fn runs_sandboxed(&self, input: &RawValue, extensions: &Extensions) -> bool {
        self.inner.runs_sandboxed(input, extensions)
    }

    fn observable_input(&self, input: &RawValue, extensions: &Extensions) -> Option<Arc<RawValue>> {
        self.inner.observable_input(input, extensions)
    }

    fn written_paths(&self, input: &RawValue, extensions: &Extensions) -> Vec<std::path::PathBuf> {
        self.inner.written_paths(input, extensions)
    }

    fn review<'a>(&'a self, input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, Option<ToolReview>> {
        self.inner.review(input, ctx)
    }

    fn call<'a>(&'a self, input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        self.inner.call(input, ctx)
    }

    fn restore<'a>(&'a self, history: &'a [StoredMessage], extensions: &'a Extensions) -> BoxFuture<'a, ()> {
        self.inner.restore(history, extensions)
    }
}

/// Sends a deferred tool in full, for models without tool search.
pub(crate) struct Undeferred {
    inner: Arc<dyn Tool>,
    spec: ToolSpec,
}

impl Undeferred {
    pub(crate) fn wrap(inner: Arc<dyn Tool>) -> Arc<dyn Tool> {
        if !inner.spec().is_deferred() {
            return inner;
        }
        let spec = inner.spec().clone().undeferred();
        Arc::new(Self { inner, spec })
    }
}

impl std::fmt::Debug for Undeferred {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Undeferred").field(&self.spec.name()).finish()
    }
}

impl Tool for Undeferred {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn is_concurrency_safe(&self, input: &RawValue) -> bool {
        self.inner.is_concurrency_safe(input)
    }

    fn runs_sandboxed(&self, input: &RawValue, extensions: &Extensions) -> bool {
        self.inner.runs_sandboxed(input, extensions)
    }

    fn observable_input(&self, input: &RawValue, extensions: &Extensions) -> Option<Arc<RawValue>> {
        self.inner.observable_input(input, extensions)
    }

    fn written_paths(&self, input: &RawValue, extensions: &Extensions) -> Vec<std::path::PathBuf> {
        self.inner.written_paths(input, extensions)
    }

    fn review<'a>(&'a self, input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, Option<ToolReview>> {
        self.inner.review(input, ctx)
    }

    fn call<'a>(&'a self, input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        self.inner.call(input, ctx)
    }

    fn restore<'a>(&'a self, history: &'a [StoredMessage], extensions: &'a Extensions) -> BoxFuture<'a, ()> {
        self.inner.restore(history, extensions)
    }
}

/// Caps a tool's text result at `limit` chars: a longer result is saved to
/// `<scratch>/tool-results/<tool_use_id>.txt` and the model gets Claude
/// Code's `<persisted-output>` preview instead.
pub struct SpillOver {
    inner: Arc<dyn Tool>,
    limit: usize,
}

impl SpillOver {
    /// Wraps `inner` with its [`result_limit`], if it has one.
    pub fn wrap(inner: Arc<dyn Tool>) -> Arc<dyn Tool> {
        match result_limit(inner.spec().name()) {
            Some(limit) => Arc::new(Self { inner, limit }),
            None => inner,
        }
    }

    pub fn with_limit(inner: Arc<dyn Tool>, limit: usize) -> Arc<dyn Tool> {
        Arc::new(Self { inner, limit })
    }
}

impl std::fmt::Debug for SpillOver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpillOver").field("tool", &self.inner.spec().name()).field("limit", &self.limit).finish()
    }
}

impl Tool for SpillOver {
    fn spec(&self) -> &ToolSpec {
        self.inner.spec()
    }

    fn is_concurrency_safe(&self, input: &RawValue) -> bool {
        self.inner.is_concurrency_safe(input)
    }

    fn runs_sandboxed(&self, input: &RawValue, extensions: &Extensions) -> bool {
        self.inner.runs_sandboxed(input, extensions)
    }

    fn observable_input(&self, input: &RawValue, extensions: &Extensions) -> Option<Arc<RawValue>> {
        self.inner.observable_input(input, extensions)
    }

    fn written_paths(&self, input: &RawValue, extensions: &Extensions) -> Vec<std::path::PathBuf> {
        self.inner.written_paths(input, extensions)
    }

    fn review<'a>(&'a self, input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, Option<ToolReview>> {
        self.inner.review(input, ctx)
    }

    fn call<'a>(&'a self, input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let out = self.inner.call(input, ctx).await;
            let ToolResultContent::Text(text) = &out.content else { return out };
            // Bytes bound chars from above: most results skip the count.
            if text.len() <= self.limit || text.chars().count() <= self.limit {
                return out;
            }
            let path = spill_path(&ctx);
            match write(&path, text).await {
                Ok(()) => ToolOutput {
                    content: ToolResultContent::Text(persisted_output(&path, text.len() as u64, text)),
                    is_error: out.is_error,
                    ends_turn: out.ends_turn,
                    beside: out.beside.clone(),
                    details: out.details.clone(),
                },
                // Unsaved, the full result is still better than none.
                Err(_) => out,
            }
        })
    }

    fn restore<'a>(&'a self, history: &'a [StoredMessage], extensions: &'a Extensions) -> BoxFuture<'a, ()> {
        self.inner.restore(history, extensions)
    }
}

fn spill_path(ctx: &ToolContext<'_>) -> PathBuf {
    let scratch = ctx
        .extensions
        .get::<Workspace>()
        .map_or_else(|| std::env::temp_dir().join("eos-agent").join(ctx.agent_id), |w| w.scratch().to_path_buf());
    scratch.join("tool-results").join(format!("{}.txt", ctx.tool_use_id))
}

async fn write(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    tokio::fs::write(path, text).await
}
