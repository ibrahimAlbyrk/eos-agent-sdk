//! The Glob tool: `rg --files --glob <pattern> --sort=modified --no-ignore
//! --hidden`, in process. The pattern follows rg's `--glob` (gitignore)
//! rules: without a `/` it matches a name at any depth.

use std::collections::BinaryHeap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use eos_agent_core::{BoxFuture, Extensions, Tool, ToolContext, ToolOutput, ToolSpec, Workspace};
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use serde::Deserialize;
use serde_json::value::RawValue;

use crate::common::{
    Fail, Res, blocking, cwd_note, display_path, finish, mtime_ms, parse_input, resolve, suggestion, with_absolute,
    workspace,
};
use crate::spec::tool_spec;
use crate::walk::{self, Merge};

const MAX_RESULTS: usize = 100;

/// Claude Code's `Glob`.
#[derive(Debug)]
pub struct GlobTool {
    spec: ToolSpec,
}

impl GlobTool {
    pub fn new() -> Self {
        Self { spec: tool_spec!("Glob") }
    }
}

impl Default for GlobTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for GlobTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn is_concurrency_safe(&self, _input: &RawValue) -> bool {
        true
    }

    /// The gate sees an absolute `path` (Claude Code passes it as written).
    fn observable_input(&self, input: &RawValue, extensions: &Extensions) -> Option<Arc<RawValue>> {
        with_absolute(input, extensions, "path")
    }

    fn call<'a>(&'a self, input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move { finish(run(input, &ctx).await) })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    pattern: String,
    path: Option<String>,
}

/// The oldest files seen (max-heap, so the newest is dropped first) and how
/// many matched in all.
#[derive(Debug, Default)]
struct Oldest {
    heap: BinaryHeap<(u64, PathBuf)>,
    total: usize,
}

impl Oldest {
    fn push(&mut self, item: (u64, PathBuf)) {
        self.total += 1;
        if self.heap.len() < MAX_RESULTS {
            self.heap.push(item);
        } else if self.heap.peek().is_some_and(|top| item < *top) {
            self.heap.pop();
            self.heap.push(item);
        }
    }
}

impl Merge for Oldest {
    fn merge(&mut self, other: Self) {
        let total = self.total + other.total;
        for item in other.heap {
            self.push(item);
        }
        self.total = total;
    }
}

pub(crate) fn check_dir(ws: &Workspace, raw: &str, dir: &Path) -> Res<()> {
    match std::fs::metadata(dir) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(Fail(format!("Path is not a directory: {raw}"))),
        Err(_) => {
            let mut msg = format!("Directory does not exist: {raw}. {}", cwd_note(ws));
            if let Some(s) = suggestion(&ws.cwd(), dir) {
                msg.push_str(&format!(" Did you mean {s}?"));
            }
            Err(Fail(msg))
        }
    }
}

/// An absolute pattern becomes a search root plus a relative pattern.
fn split_absolute(pattern: &str) -> Option<(PathBuf, String)> {
    if !pattern.starts_with('/') {
        return None;
    }
    let literal = &pattern[..pattern.find(['*', '?', '[', '{']).unwrap_or(pattern.len())];
    let cut = literal.rfind('/')?;
    let base = if cut == 0 { "/" } else { &pattern[..cut] };
    Some((PathBuf::from(base), pattern[cut + 1..].to_owned()))
}

/// `src/**/*.ts` searched from inside `…/src` means `/**/*.ts`, unless the
/// search dir really has a `src` below it.
fn strip_repeated_prefix(pattern: &str, dir: &Path) -> String {
    let parts: Vec<&str> = pattern.split('/').collect();
    let literal = parts[..parts.len() - 1]
        .iter()
        .take_while(|p| !p.is_empty() && !p.contains(['*', '?', '[', ']', '{', '}', '!']))
        .count();
    let dir_parts: Vec<String> = dir.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    for n in (1..=literal).rev() {
        if n <= dir_parts.len() && parts[..n].iter().zip(&dir_parts[dir_parts.len() - n..]).all(|(a, b)| a == b) {
            let exists = parts[..n].iter().fold(dir.to_path_buf(), |p, s| p.join(s)).is_dir();
            if !exists {
                return format!("/{}", parts[n..].join("/"));
            }
            break;
        }
    }
    pattern.to_owned()
}

async fn run(raw: &RawValue, ctx: &ToolContext<'_>) -> Res<ToolOutput> {
    let input: Input = parse_input(raw)?;
    let ws = workspace(ctx)?;
    let cwd = ws.cwd();
    let mut dir = match &input.path {
        Some(p) => {
            let dir = resolve(&ws, p);
            check_dir(&ws, p, &dir)?;
            dir
        }
        None => cwd.clone(),
    };
    let pattern = match split_absolute(&input.pattern) {
        Some((base, rel)) => {
            dir = resolve(&ws, &base.to_string_lossy());
            rel
        }
        None => strip_repeated_prefix(&input.pattern, &dir),
    };
    let mut overrides = OverrideBuilder::new(&dir);
    overrides.add(&pattern).map_err(|e| Fail(format!("Invalid glob pattern: {e}")))?;
    let overrides = overrides.build().map_err(|e| Fail(format!("Invalid glob pattern: {e}")))?;
    let mut builder = WalkBuilder::new(&dir);
    builder.standard_filters(false).overrides(overrides).threads(walk::threads());
    let cancel = ctx.cancel.clone();
    let (found, cancelled) = blocking(move || {
        Ok(walk::walk(&builder, &cancel, |acc: &mut Oldest, entry| {
            if entry.file_type().is_some_and(|t| t.is_file()) {
                let mtime = entry.metadata().map_or(0, |m| mtime_ms(&m));
                acc.push((mtime, entry.into_path()));
            }
        }))
    })
    .await?;
    if cancelled {
        return Err(Fail("The search was cancelled.".into()));
    }
    let total = found.total;
    let files = found.heap.into_sorted_vec();
    if files.is_empty() {
        return Ok(ToolOutput::text("No files found"));
    }
    let mut out: Vec<String> = files.iter().map(|(_, p)| display_path(&cwd, p)).collect();
    if total > files.len() {
        out.push(format!(
            "(Showing {} of {total} matching files; {} more are not listed. Narrow the pattern or path to see the rest.)",
            files.len(),
            total - files.len()
        ));
    }
    Ok(ToolOutput::text(out.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_patterns_split_at_the_first_wildcard() {
        assert_eq!(split_absolute("/a/b/**/*.rs"), Some((PathBuf::from("/a/b"), "**/*.rs".into())));
        assert_eq!(split_absolute("/a/b/c.rs"), Some((PathBuf::from("/a/b"), "c.rs".into())));
        assert_eq!(split_absolute("src/*.rs"), None);
    }

    #[test]
    fn repeated_prefix_is_dropped_only_when_missing() {
        let dir = std::env::temp_dir().join("eos-glob-prefix-test").join("src");
        assert_eq!(strip_repeated_prefix("src/**/*.ts", &dir), "/**/*.ts");
        assert_eq!(strip_repeated_prefix("lib/*.ts", &dir), "lib/*.ts");
    }
}
