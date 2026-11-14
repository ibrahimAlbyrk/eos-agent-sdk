//! The script as QuickJS runs it: Claude Code's wrapper plus the bookkeeping
//! that stands in for `AsyncLocalStorage`.
//!
//! Claude Code chains resume keys along async branches ("lanes"): node's
//! `AsyncLocalStorage` hands every `await` continuation and `.then` callback
//! the context that was current when it was scheduled, and `parallel()` /
//! `pipeline()` give each branch its own. QuickJS-NG never reports when an
//! `await` resumes (its promise hooks skip those jobs), so the script is
//! rewritten instead, before it runs, with oxc's parser:
//!
//! - an async function's body starts with `const __wRg$k=__wRg$g();` — the
//!   lane it was called in, which (as with AsyncLocalStorage) is the lane of
//!   everything it does;
//! - `await E` becomes `__wRg$r(__wRg$k,await E)`: back in that lane when it
//!   resumes (`__wRg$k` is read before `E` runs, the lane is set after);
//! - `catch` and `finally` blocks start with `__wRg$s(__wRg$k);` (a rejected
//!   `await` resumes there), and `for await` restores it at the top of each
//!   iteration and after the loop;
//! - where there is no statement to put the capture in (an async arrow with
//!   an expression body) or the lane may change between resumptions (async
//!   generators, resumed by whoever calls `next()`), `await E` captures the
//!   current lane itself: `__wRg$r(__wRg$g(),await E)`.
//!
//! `.then` callbacks and timers are bound by the prelude. The names use
//! Claude Code's reserved `__wRg$` prefix, which scripts may not use, and are
//! parameters of the wrapper, not globals. Insertions never add a line
//! break, so line numbers stay the script's; [`Rewritten::map_stack`] moves
//! columns back.

use std::borrow::Cow;

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    ArrowFunctionBody, ArrowFunctionExpression, AwaitExpression, CatchClause, ForOfStatement, Function, FunctionBody,
    LabeledStatement, Statement, StaticBlock, TryStatement,
};
use oxc_ast_visit::{Visit, walk};
use oxc_span::{GetSpan, SourceType};
use oxc_syntax::scope::ScopeFlags;

use super::SyntaxError;

/// The wrapper's parameters: get the current lane, resume in one (returning
/// the awaited value), set one.
pub(crate) const PARAMS: &str = "__wRg$g,__wRg$r,__wRg$s";
/// Claude Code's wrapper; the body starts on line 2.
const HEAD: &str = "(async () => {'use strict';\n";
const TAIL: &str = "\n})()";

/// A rewritten body: `code` is the wrapped script (an expression that starts
/// the run when the wrapper's parameters are bound).
#[derive(Debug, Clone)]
pub(crate) struct Rewritten {
    pub(crate) code: String,
    pub(crate) columns: Columns,
}

/// Where the insertions went, to move positions in `code` back.
#[derive(Debug, Clone, Default)]
pub(crate) struct Columns {
    /// Every insertion in order: its line in `code` (1-based), the byte
    /// column (0-based) in `code` where it starts, and its length.
    inserted: Vec<(u32, u32, u32)>,
}

/// Rewrites `body`, or the parser's error (line and column in the body).
pub(crate) fn rewrite(body: &str) -> Result<Rewritten, SyntaxError> {
    let source = format!("{HEAD}{body}{TAIL}");
    let allocator = Allocator::default();
    let parsed = oxc_parser::Parser::new(&allocator, &source, SourceType::script()).parse();
    if let Some(error) = parsed.diagnostics.first() {
        let offset = error.labels.first().map_or(source.len(), |l| l.offset() as usize).min(source.len());
        return Err(syntax_error(&source, offset, error.message.to_string()));
    }
    let mut visitor = Rewriter { insertions: Vec::new(), frames: Vec::new(), labeled: Vec::new() };
    visitor.visit_program(&parsed.program);
    let mut insertions = visitor.insertions;
    insertions.retain(|(_, text)| !text.is_empty());
    // Stable: equal offsets keep the order they were visited in (outer
    // openings first, inner closings first).
    insertions.sort_by_key(|(at, _)| *at);
    let mut code = String::with_capacity(source.len() + insertions.iter().map(|(_, t)| t.len()).sum::<usize>());
    let mut inserted = Vec::with_capacity(insertions.len());
    let (mut from, mut line) = (0, 1u32);
    let narrow = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
    for (at, text) in &insertions {
        let at = *at as usize;
        let between = &source[from..at];
        line += narrow(between.matches('\n').count());
        code.push_str(between);
        let column = code.len() - code.rfind('\n').map_or(0, |n| n + 1);
        inserted.push((line, narrow(column), narrow(text.len())));
        code.push_str(text);
        from = at;
    }
    code.push_str(&source[from..]);
    Ok(Rewritten { code, columns: Columns { inserted } })
}

/// A parse error at byte `offset` of the wrapped source, placed in the body.
fn syntax_error(source: &str, offset: usize, message: String) -> SyntaxError {
    let mut offset = offset;
    while !source.is_char_boundary(offset) {
        offset -= 1;
    }
    let before = &source[..offset];
    let line = before.matches('\n').count() + 1;
    let start = before.rfind('\n').map_or(0, |n| n + 1);
    let column = source[start..offset].chars().count() + 1;
    // Line 1 is the wrapper's; past the body is its closing line.
    let body_lines = source.matches('\n').count() - 1;
    let line = line.saturating_sub(1).clamp(1, body_lines.max(1));
    SyntaxError { message, line: Some(line), column: Some(column) }
}

impl Columns {
    /// The column (1-based, bytes) in the original body of a column on line
    /// `line` (1-based) of `code`.
    pub(crate) fn column(&self, line: usize, column: usize) -> usize {
        let line = u32::try_from(line).unwrap_or(u32::MAX);
        let first = self.inserted.partition_point(|(l, _, _)| *l < line);
        let at = column.saturating_sub(1);
        let mut shift = 0;
        for &(_, start, len) in self.inserted[first..].iter().take_while(|(l, _, _)| *l == line) {
            let (start, len) = (start as usize, len as usize);
            if start >= at {
                break;
            }
            // Inside an insertion: where it was inserted.
            shift += len.min(at - start);
        }
        at - shift + 1
    }

    /// `text` with every `<file>:<line>:<column>` moved back to the body's
    /// columns.
    pub(crate) fn map_stack(&self, text: &str, file: &str) -> String {
        let needle = format!("{file}:");
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(at) = rest.find(&needle) {
            let after = &rest[at + needle.len()..];
            out.push_str(&rest[..at + needle.len()]);
            let line_len = after.bytes().take_while(u8::is_ascii_digit).count();
            let col_part = after[line_len..].strip_prefix(':');
            let col_len = col_part.map_or(0, |c| c.bytes().take_while(u8::is_ascii_digit).count());
            match (after[..line_len].parse::<usize>(), col_part.and_then(|c| c[..col_len].parse::<usize>().ok())) {
                (Ok(line), Some(column)) if col_len > 0 => {
                    out.push_str(&format!("{line}:{}", self.column(line, column)));
                    rest = &after[line_len + 1 + col_len..];
                }
                _ => rest = after,
            }
        }
        out.push_str(rest);
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lane {
    /// An async function with a block body: its lane is `__wRg$k`.
    Captured,
    /// An async arrow with an expression body, or an async generator: each
    /// `await` captures the current lane.
    PerAwait,
    /// A synchronous function or class static block: never resumes.
    Sync,
}

struct Frame {
    lane: Lane,
    /// Where the `__wRg$k` declaration goes, filled in when it is used.
    declare: Option<usize>,
    used: bool,
}

struct Rewriter {
    insertions: Vec<(u32, Cow<'static, str>)>,
    frames: Vec<Frame>,
    /// Starts of labels in front of `for await` statements, by the loop's
    /// start: the block around the loop must hold its labels too.
    labeled: Vec<(u32, u32)>,
}

const CAPTURE: &str = ";const __wRg$k=__wRg$g();";
const RESTORE: &str = "__wRg$s(__wRg$k);";

impl Rewriter {
    fn insert(&mut self, at: u32, text: impl Into<Cow<'static, str>>) -> usize {
        self.insertions.push((at, text.into()));
        self.insertions.len() - 1
    }

    /// The innermost function's lane, marking its capture as used.
    fn lane(&mut self) -> Lane {
        match self.frames.last_mut() {
            Some(frame) => {
                if frame.lane == Lane::Captured {
                    frame.used = true;
                }
                frame.lane
            }
            None => Lane::Sync,
        }
    }

    fn enter(&mut self, lane: Lane, body: Option<&FunctionBody<'_>>) {
        let declare = match (lane, body) {
            (Lane::Captured, Some(body)) => {
                // After the directive prologue, so `'use strict'` stays one.
                let at = body.directives.last().map_or(body.span.start + 1, |d| d.span.end);
                Some(self.insert(at, ""))
            }
            _ => None,
        };
        self.frames.push(Frame { lane, declare, used: false });
    }

    fn leave(&mut self) {
        if let Some(frame) = self.frames.pop()
            && frame.used
            && let Some(slot) = frame.declare
        {
            self.insertions[slot].1 = Cow::Borrowed(CAPTURE);
        }
    }
}

impl<'a> Visit<'a> for Rewriter {
    fn visit_function(&mut self, it: &Function<'a>, flags: ScopeFlags) {
        let lane = match (it.r#async, it.generator) {
            (true, false) => Lane::Captured,
            (true, true) => Lane::PerAwait,
            (false, _) => Lane::Sync,
        };
        self.enter(lane, it.body.as_deref());
        walk::walk_function(self, it, flags);
        self.leave();
    }

    fn visit_arrow_function_expression(&mut self, it: &ArrowFunctionExpression<'a>) {
        match &it.body {
            ArrowFunctionBody::FunctionBody(body) if it.r#async => self.enter(Lane::Captured, Some(body)),
            _ if it.r#async => self.enter(Lane::PerAwait, None),
            _ => self.enter(Lane::Sync, None),
        }
        walk::walk_arrow_function_expression(self, it);
        self.leave();
    }

    fn visit_static_block(&mut self, it: &StaticBlock<'a>) {
        self.enter(Lane::Sync, None);
        walk::walk_static_block(self, it);
        self.leave();
    }

    fn visit_await_expression(&mut self, it: &AwaitExpression<'a>) {
        match self.lane() {
            Lane::Captured => {
                self.insert(it.span.start, "__wRg$r(__wRg$k,");
            }
            Lane::PerAwait => {
                self.insert(it.span.start, "__wRg$r(__wRg$g(),");
            }
            // `await` outside an async function does not parse.
            Lane::Sync => return walk::walk_await_expression(self, it),
        }
        walk::walk_await_expression(self, it);
        self.insert(it.span.end, ")");
    }

    fn visit_catch_clause(&mut self, it: &CatchClause<'a>) {
        if self.lane() == Lane::Captured {
            self.insert(it.body.span.start + 1, RESTORE);
        }
        walk::walk_catch_clause(self, it);
    }

    fn visit_try_statement(&mut self, it: &TryStatement<'a>) {
        self.visit_block_statement(&it.block);
        if let Some(handler) = &it.handler {
            self.visit_catch_clause(handler);
        }
        if let Some(finalizer) = &it.finalizer {
            if self.lane() == Lane::Captured {
                self.insert(finalizer.span.start + 1, RESTORE);
            }
            self.visit_block_statement(finalizer);
        }
    }

    fn visit_labeled_statement(&mut self, it: &LabeledStatement<'a>) {
        let mut inner = &it.body;
        while let Statement::LabeledStatement(label) = inner {
            inner = &label.body;
        }
        if let Statement::ForOfStatement(for_of) = inner
            && for_of.r#await
            && !self.labeled.iter().any(|(start, _)| *start == for_of.span.start)
        {
            self.labeled.push((for_of.span.start, it.span.start));
        }
        walk::walk_labeled_statement(self, it);
    }

    fn visit_for_of_statement(&mut self, it: &ForOfStatement<'a>) {
        if !it.r#await || self.lane() != Lane::Captured {
            return walk::walk_for_of_statement(self, it);
        }
        // { [labels:] for await (…) {restore; body} restore }
        let start = self.labeled.iter().find(|(s, _)| *s == it.span.start).map_or(it.span.start, |(_, l)| *l);
        self.insert(start, "{");
        self.visit_for_statement_left(&it.left);
        self.visit_expression(&it.right);
        self.insert(it.body.span().start, "{__wRg$s(__wRg$k);");
        self.visit_statement(&it.body);
        self.insert(it.body.span().end, "}");
        self.insert(it.span.end, ";__wRg$s(__wRg$k)}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[global_allocator]
    static ALLOC: eos_agent_testkit::CountingAlloc = eos_agent_testkit::CountingAlloc;

    fn body(code: &str) -> String {
        let out = rewrite(code).expect("parses").code;
        let inner = out.strip_prefix(HEAD.trim_end()).and_then(|o| o.strip_suffix(TAIL)).expect("wrapped");
        inner.strip_prefix(CAPTURE).unwrap_or(inner).strip_prefix('\n').expect("line").to_owned()
    }

    #[test]
    fn awaits_resume_in_their_lane() {
        assert_eq!(body("const a = await agent('x')"), "const a = __wRg$r(__wRg$k,await agent('x'))");
        assert_eq!(body("x = await await y + 1"), "x = __wRg$r(__wRg$k,await __wRg$r(__wRg$k,await y)) + 1");
        assert_eq!(
            body("const f = async (d) => { 'use strict'; return await g(d) }"),
            "const f = async (d) => { 'use strict';;const __wRg$k=__wRg$g(); return __wRg$r(__wRg$k,await g(d)) }"
        );
        assert_eq!(body("f(async d => await g(d), 2)"), "f(async d => __wRg$r(__wRg$g(),await g(d)), 2)");
        assert_eq!(
            body("class A { async m() {await 1} static { try {} catch {} } }\nfunction s() { try {} finally {} }"),
            "class A { async m() {;const __wRg$k=__wRg$g();__wRg$r(__wRg$k,await 1)} static { try {} catch {} } }\nfunction s() { try {} finally {} }"
        );
        assert_eq!(
            body("async function* g() { yield await h() }"),
            "async function* g() { yield __wRg$r(__wRg$g(),await h()) }"
        );
    }

    #[test]
    fn rejected_awaits_and_loops_restore_the_lane() {
        assert_eq!(
            body("try { await a() } catch (e) { b() } finally { c() }"),
            "try { __wRg$r(__wRg$k,await a()) } catch (e) {__wRg$s(__wRg$k); b() } finally {__wRg$s(__wRg$k); c() }"
        );
        assert_eq!(
            body("L: M: for await (const x of xs) if (x) break L\nnext()"),
            "{L: M: for await (const x of xs) {__wRg$s(__wRg$k);if (x) break L};__wRg$s(__wRg$k)}\nnext()"
        );
        // Top level: the wrapper's own capture.
        let out = rewrite("await x").expect("parses").code;
        assert!(out.starts_with("(async () => {'use strict';;const __wRg$k=__wRg$g();\n__wRg$r("), "{out}");
        // Nothing to capture: no declaration.
        assert_eq!(rewrite("return 1").expect("parses").code, format!("{HEAD}return 1{TAIL}"));
    }

    #[test]
    fn positions_stay_the_scripts() {
        let code = "const a = 1\nconst b = await f(await g())\n  throw new Error('x')";
        let out = rewrite(code).expect("parses");
        assert_eq!(out.code.lines().count(), code.lines().count() + 2);
        // Line 3 of the code is the body's line 2: `await f(` was at column 11.
        let line = out.code.lines().nth(2).expect("line");
        let at = line.find("f(").expect("call") + 1;
        assert_eq!(out.columns.column(3, at), "const b = await f(".find("f(").expect("call") + 1);
        // Inside an insertion: where it was inserted.
        assert_eq!(out.columns.column(3, 12), 11);
        assert_eq!(out.columns.column(4, 3), 3);
        assert_eq!(
            out.columns.map_stack(
                &format!("    at <anonymous> (workflow.js:3:{at})\n    at x (other.js:3:{at})"),
                "workflow.js"
            ),
            format!("    at <anonymous> (workflow.js:3:17)\n    at x (other.js:3:{at})")
        );
    }

    /// `cargo test --release -p eos-agent-workflow --lib rewrite_cost -- --ignored --nocapture`
    #[test]
    #[ignore = "measurement"]
    fn rewrite_cost() {
        let typical = "const DIMENSIONS = [{key: 'bugs', prompt: '...'}, {key: 'perf', prompt: '...'}]
const results = await pipeline(
  DIMENSIONS,
  d => agent(d.prompt, {label: `review:${d.key}`, phase: 'Review', schema: FINDINGS_SCHEMA}),
  review => parallel(review.findings.map(f => () =>
    agent(`Adversarially verify: ${f.title}`, {label: `verify:${f.file}`, phase: 'Verify', schema: VERDICT_SCHEMA})
      .then(v => ({...f, verdict: v}))
  ))
)
const confirmed = results.flat().filter(Boolean).filter(f => f.verdict?.isReal)
return { confirmed }
";
        let block = "async function step(i) {\n  try {\n    const r = await agent(`step ${i}`, {label: `s${i}`});\n    for await (const x of [r]) log(x)\n    return await parallel([1, 2].map(n => () => agent(`${i}:${n}`)))\n  } catch (e) { log(e.message) } finally { log('done') }\n}\n";
        let mut large = String::new();
        while large.len() + block.len() < 512 * 1024 {
            large.push_str(block);
        }
        for (name, body) in [("typical", typical), ("large", large.as_str())] {
            let n = if body.len() > 100_000 { 20 } else { 2000 };
            let before = eos_agent_testkit::AllocStats::now();
            let first = rewrite(body).expect("parses");
            let peak = eos_agent_testkit::AllocStats::now().peak_bytes.saturating_sub(before.live_bytes);
            println!("{name}: peak {:.1} KB while rewriting, {:.1} KB kept (code + columns)", peak as f64 / 1024.0, {
                let kept: usize = first.code.capacity() + first.columns.inserted.capacity() * 12;
                kept as f64 / 1024.0
            });
            drop(first);
            let started = std::time::Instant::now();
            let mut out = None;
            for _ in 0..n {
                out = Some(rewrite(body).expect("parses"));
            }
            let each = started.elapsed() / n;
            let out = out.expect("rewritten");
            let inserted = out.columns.inserted.len();
            println!("{name}: {} bytes -> {} bytes ({inserted} insertions) in {each:?}", body.len(), out.code.len());
        }
    }

    #[test]
    fn syntax_errors_point_into_the_body() {
        let e = rewrite("const a = 1\nconst x: string = 1").expect_err("error");
        assert_eq!((e.line, e.column), (Some(2), Some(7)));
        let e = rewrite("foo((").expect_err("error");
        assert_eq!(e.line, Some(1));
    }
}
