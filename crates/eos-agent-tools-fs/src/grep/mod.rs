//! The Grep tool: ripgrep's search, run in process. Hidden files are
//! searched, VCS directories skipped, ignore files respected.

mod input;
mod output;
mod search;

use std::path::PathBuf;
use std::sync::Arc;

use eos_agent_core::{BoxFuture, Extensions, Tool, ToolContext, ToolOutput, ToolSpec};
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::Searcher;
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use ignore::types::TypesBuilder;
use serde_json::value::RawValue;

use self::input::{Input, Mode, split_globs};
use self::output::{DEFAULT_HEAD_LIMIT, Found};
use crate::common::{Fail, Res, blocking, cwd_note, finish, mtime_ms, resolve, suggestion, with_absolute, workspace};
use crate::spec::tool_spec;
use crate::walk;

/// One walk thread's hits and its reusable searcher.
#[derive(Default)]
struct Worker {
    found: Found,
    searcher: Option<Searcher>,
}

impl walk::Merge for Worker {
    fn merge(&mut self, other: Self) {
        self.found.merge(other.found);
    }
}

const VCS_DIRS: [&str; 6] = [".git", ".svn", ".hg", ".bzr", ".jj", ".sl"];

/// Claude Code's `Grep`.
#[derive(Debug)]
pub struct GrepTool {
    spec: ToolSpec,
}

impl GrepTool {
    pub fn new() -> Self {
        Self { spec: tool_spec!("Grep") }
    }
}

impl Default for GrepTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for GrepTool {
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

fn matcher(input: &Input) -> Res<RegexMatcher> {
    let mut b = RegexMatcherBuilder::new();
    b.case_insensitive(input.ignore_case).multi_line(true);
    if input.multiline {
        b.dot_matches_new_line(true);
    } else {
        b.line_terminator(Some(b'\n'));
    }
    b.build(&input.pattern).map_err(|e| Fail(e.to_string()))
}

fn walker(input: &Input, dir: &std::path::Path) -> Res<WalkBuilder> {
    let mut overrides = OverrideBuilder::new(dir);
    for vcs in VCS_DIRS {
        overrides.add(&format!("!{vcs}")).map_err(|e| Fail(e.to_string()))?;
    }
    for glob in input.glob.as_deref().map(split_globs).unwrap_or_default() {
        overrides.add(&glob).map_err(|e| Fail(format!("Invalid glob {glob}: {e}")))?;
    }
    let mut builder = WalkBuilder::new(dir);
    builder
        .hidden(false)
        .add_custom_ignore_filename(".rgignore")
        .overrides(overrides.build().map_err(|e| Fail(e.to_string()))?)
        .threads(walk::threads());
    if let Some(name) = &input.file_type {
        let mut types = TypesBuilder::new();
        types.add_defaults().select(name);
        builder.types(types.build().map_err(|e| Fail(e.to_string()))?);
    }
    Ok(builder)
}

async fn run(raw: &RawValue, ctx: &ToolContext<'_>) -> Res<ToolOutput> {
    let input = input::parse(raw.get())?;
    let ws = workspace(ctx)?;
    let cwd = ws.cwd();
    let target = match &input.path {
        Some(p) => {
            let path = resolve(&ws, p);
            if !path.exists() {
                let mut msg = format!("Path does not exist: {p}. {}", cwd_note(&ws));
                if let Some(s) = suggestion(&cwd, &path) {
                    msg.push_str(&format!(" Did you mean {s}?"));
                }
                return Err(Fail(msg));
            }
            path
        }
        None => cwd.clone(),
    };
    let searched_dir = target.is_dir();
    let matcher = matcher(&input)?;
    let walk = if searched_dir { Some(walker(&input, &target)?) } else { None };
    let breaks = search::context_breaks(&input);
    let need = match input.head_limit {
        Some(0) => None,
        limit => Some((input.offset + limit.unwrap_or(DEFAULT_HEAD_LIMIT) + 1) as usize),
    };
    let cancel = ctx.cancel.clone();
    let (input, found) = blocking(move || {
        let visit = |worker: &mut Worker, path: PathBuf, mtime: Option<u64>| {
            let searcher = worker.searcher.get_or_insert_with(|| search::searcher(&input));
            let max = need.unwrap_or(usize::MAX);
            let Ok(hits) = search::search_file(searcher, &matcher, &path, input.mode, input.only_matching, breaks, max)
            else {
                return;
            };
            match input.mode {
                Mode::Content => worker.found.add_lines(path, hits.lines, need, breaks),
                Mode::Count if hits.count > 0 => worker.found.add_count(path, hits.count),
                Mode::Files if hits.count > 0 => worker.found.add_file(path, mtime.unwrap_or(0)),
                _ => {}
            }
        };
        let worker = match &walk {
            None => {
                let mut worker = Worker { found: Found::new(need, breaks), searcher: None };
                let mtime = std::fs::metadata(&target).map_or(0, |m| mtime_ms(&m));
                visit(&mut worker, target.clone(), Some(mtime));
                worker
            }
            Some(walk) => {
                let (worker, cancelled) = walk::walk(walk, &cancel, |worker: &mut Worker, entry| {
                    if entry.file_type().is_some_and(|t| t.is_file()) {
                        let mtime = (input.mode == Mode::Files).then(|| entry.metadata().map_or(0, |m| mtime_ms(&m)));
                        visit(worker, entry.into_path(), mtime);
                    }
                });
                if cancelled {
                    return Err(Fail("The search was cancelled.".into()));
                }
                worker
            }
        };
        Ok((input, worker.found))
    })
    .await?;
    let mut text = output::render(found, &input, &cwd, searched_dir);
    if let Some(note) = &input.note {
        text.push_str("\n\n");
        text.push_str(note);
    }
    Ok(ToolOutput::text(text))
}
