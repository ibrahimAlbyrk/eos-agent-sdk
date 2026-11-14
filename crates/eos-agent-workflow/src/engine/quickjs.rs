//! The QuickJS (QuickJS-NG, through rquickjs) engine: one runtime and one
//! context per realm, built with the language intrinsics only — no module
//! loader, no `std`/`os`, no clock (`Performance`), nothing that reaches the
//! file system, network, environment or process. The runtime exists only
//! while its realm does.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rquickjs::context::{EvalOptions, intrinsic};
use rquickjs::convert::Coerced;
use rquickjs::{Context, Ctx, Function, Object, Persistent, Runtime, Value};

use super::rewrite::{self, Columns, PARAMS};
use super::slash;
use super::{Fatal, Host, Realm, RealmSetup, ScriptEngine, SyntaxError};

const PRELUDE: &str = include_str!("prelude.js");
const FILENAME: &str = "workflow.js";
/// The hooks' own frames, kept apart from the script's.
const PRELUDE_FILENAME: &str = "workflow-hooks.js";

type Intrinsics = (
    intrinsic::Date,
    intrinsic::Eval,
    intrinsic::RegExpCompiler,
    intrinsic::RegExp,
    intrinsic::Json,
    intrinsic::Proxy,
    intrinsic::MapSet,
    intrinsic::TypedArrays,
    intrinsic::Promise,
    intrinsic::WeakRef,
);

/// The engine; stateless, every realm gets its own runtime.
#[derive(Debug, Default, Clone, Copy)]
pub struct QuickJs;

/// When the current synchronous stretch started (`None` while waiting), and
/// whether the interrupt handler cut one short.
#[derive(Debug)]
struct Slice {
    started: Cell<Option<Instant>>,
    limit: Duration,
    timed_out: Cell<bool>,
    aborted: Cell<bool>,
}

impl Slice {
    fn interrupt(&self) -> bool {
        if self.aborted.get() {
            return true;
        }
        let over = self.started.get().is_some_and(|at| at.elapsed() > self.limit);
        if over {
            self.timed_out.set(true);
        }
        over
    }
}

struct QuickJsRealm {
    // Dropped before the context and runtime it belongs to.
    entry: Option<Persistent<Object<'static>>>,
    host: Arc<dyn Host>,
    id: u32,
    /// The rewritten body until it starts (or why it does not parse).
    code: Option<Result<String, SyntaxError>>,
    columns: Arc<Columns>,
    slice: Rc<Slice>,
    ctx: Context,
    rt: Runtime,
}

impl ScriptEngine for QuickJs {
    /// oxc's parser first (its messages read like acorn's, which Claude Code
    /// shows), then QuickJS compiles what would run, which also catches the
    /// early errors oxc's parser leaves to later passes (redeclarations…).
    fn check_syntax(&self, body: &str) -> Result<(), SyntaxError> {
        let script = rewrite::rewrite(body)?;
        let engine = |e: rquickjs::Error| SyntaxError { message: e.to_string(), line: None, column: None };
        let rt = Runtime::new().map_err(engine)?;
        let ctx = Context::custom::<Intrinsics>(&rt).map_err(engine)?;
        // Compiled, never called: the body's statements do not run.
        ctx.with(|ctx| match ctx.eval_with_options::<Value, _>(source(&script.code), options(FILENAME)) {
            Ok(_) => Ok(()),
            Err(rquickjs::Error::Exception) => {
                let (_, message, stack) = error_parts(&ctx, ctx.catch());
                let (line, column) = stack.as_deref().map_or((None, None), location);
                let column = match (line, column) {
                    (Some(line), Some(column)) => Some(script.columns.column(line, column)),
                    _ => column,
                };
                // The wrapper adds one line ahead of the body.
                Err(SyntaxError { message, line: line.map(|l| l.saturating_sub(1).max(1)), column })
            }
            Err(e) => Err(engine(e)),
        })
    }

    /// Compiled (never run) with the language intrinsics only.
    fn ambiguous_slash(&self, body: &str) -> Option<String> {
        let own = slash::body_slashes(body)?;
        let rt = Runtime::new().ok()?;
        let ctx = Context::custom::<Intrinsics>(&rt).ok()?;
        let compiles = |code: String| {
            ctx.with(|ctx| {
                let ok = ctx.eval_with_options::<Value, _>(code, options(FILENAME)).is_ok();
                if !ok {
                    let _ = ctx.catch();
                }
                ok
            })
        };
        slash::refusal(body, &own, |t| compiles(slash::check_wrapper(t)), |t| compiles(source(t)))
    }

    fn realm(&self, setup: RealmSetup, host: Arc<dyn Host>) -> Result<Box<dyn Realm>, Fatal> {
        let fatal = |e: rquickjs::Error| Fatal::Engine(e.to_string());
        let rt = Runtime::new().map_err(fatal)?;
        let slice = Rc::new(Slice {
            started: Cell::new(None),
            limit: setup.slice_limit,
            timed_out: Cell::new(false),
            aborted: Cell::new(false),
        });
        let watch = Rc::clone(&slice);
        rt.set_interrupt_handler(Some(Box::new(move || watch.interrupt())));
        let ctx = Context::custom::<Intrinsics>(&rt).map_err(fatal)?;
        let (code, columns) = match rewrite::rewrite(&setup.body) {
            Ok(script) => (Ok(script.code), Arc::new(script.columns)),
            Err(e) => (Err(e), Arc::default()),
        };
        let mut realm = QuickJsRealm { entry: None, host, id: setup.id, code: Some(code), columns, slice, ctx, rt };
        let host = Arc::clone(&realm.host);
        let id = realm.id;
        let mapped = Arc::clone(&realm.columns);
        let entry = realm.stretch(|ctx| {
            let call = Function::new(ctx.clone(), move |op: String, payload: String| {
                // A failed run's stack, in the script's own columns.
                let payload = if op == "fail" { mapped.map_stack(&payload, FILENAME) } else { payload };
                envelope(&*host, id, &op, &payload)
            })?;
            let prelude: Function = ctx.eval_with_options(PRELUDE, options(PRELUDE_FILENAME))?;
            let entry: Object = prelude.call((call, setup.config))?;
            Ok(Persistent::save(ctx, entry))
        })?;
        realm.entry = Some(entry);
        Ok(Box::new(realm))
    }
}

fn options(filename: &str) -> EvalOptions {
    let mut options = EvalOptions::default();
    options.filename = Some(filename.to_owned());
    options
}

/// The rewritten script as a function of the lane operations; the body
/// stays on the lines after the first.
fn source(code: &str) -> String {
    format!("(function ({PARAMS}) {{ return {code}; }})")
}

/// A host answer in the prelude's envelope.
fn envelope(host: &dyn Host, id: u32, op: &str, payload: &str) -> String {
    match host.call(id, op, payload) {
        Ok(value) => format!("{{\"v\":{}}}", if value.is_empty() { "null" } else { &value }),
        Err(e) => {
            let mut out = serde_json::Map::new();
            out.insert("e".into(), e.message.into());
            if let Some(name) = e.name {
                out.insert("n".into(), name.into());
            }
            if e.type_error {
                out.insert("t".into(), "TypeError".into());
            }
            serde_json::Value::Object(out).to_string()
        }
    }
}

impl QuickJsRealm {
    /// One synchronous stretch: `f`, then every job it queued, under the
    /// slice limit.
    fn stretch<R>(&mut self, f: impl FnOnce(&Ctx<'_>) -> rquickjs::Result<R>) -> Result<R, Fatal> {
        self.slice.started.set(Some(Instant::now()));
        let out = self.ctx.with(|ctx| match f(&ctx) {
            Ok(v) => Ok(Some(v)),
            Err(rquickjs::Error::Exception) => {
                let caught = ctx.catch();
                if self.slice.timed_out.get() || self.slice.aborted.get() {
                    return Ok(None);
                }
                let (name, message, stack) = error_parts(&ctx, caught);
                let stack = stack.map(|stack| self.columns.map_stack(&stack, FILENAME));
                Err(Fatal::Engine(match stack {
                    Some(stack) => format!("{name}: {message}\n{stack}"),
                    None => format!("{name}: {message}"),
                }))
            }
            Err(e) => Err(Fatal::Engine(e.to_string())),
        });
        let drained = self.drain();
        self.slice.started.set(None);
        if self.slice.timed_out.get() {
            return Err(Fatal::TimedOut(self.slice.limit));
        }
        drained?;
        match out? {
            Some(v) => Ok(v),
            None => Err(Fatal::Engine("the script was stopped".into())),
        }
    }

    fn drain(&self) -> Result<(), Fatal> {
        loop {
            match self.rt.execute_pending_job() {
                Ok(true) => {}
                Ok(false) => return Ok(()),
                // Jobs only throw what nothing can catch: the interrupt.
                Err(_) if self.slice.timed_out.get() || self.slice.aborted.get() => return Ok(()),
                Err(e) => {
                    let message = e.0.with(|ctx| {
                        let (name, message, _) = error_parts(&ctx, ctx.catch());
                        format!("{name}: {message}")
                    });
                    return Err(Fatal::Engine(message));
                }
            }
        }
    }

    fn entry_call<A>(&mut self, method: &str, args: A) -> Result<(), Fatal>
    where
        A: for<'js> rquickjs::function::IntoArgs<'js>,
    {
        let Some(entry) = self.entry.clone() else { return Ok(()) };
        self.stretch(|ctx| {
            let entry = entry.restore(ctx)?;
            let f: Function = entry.get(method)?;
            f.call::<_, ()>(args)
        })
    }
}

impl Realm for QuickJsRealm {
    fn start(&mut self) -> Result<(), Fatal> {
        let Some(entry) = self.entry.clone() else { return Ok(()) };
        let host = Arc::clone(&self.host);
        let id = self.id;
        let columns = Arc::clone(&self.columns);
        let code = match self.code.take() {
            Some(Ok(code)) => code,
            None => return Ok(()),
            Some(Err(e)) => {
                // A body that does not parse (checked before launch, so rare).
                let info = serde_json::json!({ "name": "SyntaxError", "message": e.message });
                let _ = host.call(id, "fail", &info.to_string());
                return Ok(());
            }
        };
        self.stretch(|ctx| {
            let entry = entry.restore(ctx)?;
            let run: Function = entry.get("run")?;
            match ctx.eval_with_options::<Value, _>(source(&code), options(FILENAME)) {
                Ok(compiled) => run.call::<_, ()>((compiled,)),
                Err(rquickjs::Error::Exception) => {
                    // A body that does not compile (checked before launch,
                    // so rare): the run fails with the engine's error.
                    let (name, message, stack) = error_parts(ctx, ctx.catch());
                    let stack = stack.map(|s| columns.map_stack(&s, FILENAME));
                    let info = serde_json::json!({ "name": name, "message": message, "stack": stack });
                    let _ = host.call(id, "fail", &info.to_string());
                    Ok(())
                }
                Err(e) => Err(e),
            }
        })
    }

    fn settle(&mut self, call: u64, ok: bool, json: &str) -> Result<(), Fatal> {
        self.entry_call("settle", (call as f64, ok, json.to_owned()))
    }

    fn fire_timer(&mut self, id: u64) -> Result<(), Fatal> {
        self.entry_call("fire", (id as f64,))
    }

    fn abort(&mut self) {
        let _ = self.entry_call("abort", ());
        self.slice.aborted.set(true);
    }

    fn memory_bytes(&self) -> usize {
        usize::try_from(self.rt.memory_usage().memory_used_size).unwrap_or(0)
    }
}

/// `name`, `message` and `stack` of a thrown value.
fn error_parts<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> (String, String, Option<String>) {
    let _ = ctx;
    if let Some(obj) = value.as_object() {
        let text = |key: &str| obj.get::<_, Option<Coerced<String>>>(key).ok().flatten().map(|c| c.0);
        let name = text("name").unwrap_or_else(|| "Error".into());
        let message = text("message").unwrap_or_default();
        return (name, message, text("stack").filter(|s| !s.is_empty()));
    }
    let message = value.get::<Coerced<String>>().map(|c| c.0).unwrap_or_else(|_| "<unprintable thrown value>".into());
    ("Error".into(), message, None)
}

/// The first `file:line:column` of a stack.
fn location(stack: &str) -> (Option<usize>, Option<usize>) {
    let Some(at) = stack.find(&format!("{FILENAME}:")) else { return (None, None) };
    let mut parts = stack[at + FILENAME.len() + 1..].split(|c: char| !c.is_ascii_digit());
    let line = parts.next().and_then(|n| n.parse().ok());
    let column = parts.next().and_then(|n| n.parse().ok());
    (line, column)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use rquickjs::promise::PromiseHookType;

    use super::*;

    /// Resume keys chain along async lanes in Claude Code (AsyncLocalStorage).
    /// QuickJS-NG's promise hooks cannot carry such a context: `before` and
    /// `after` fire only around thenable jobs, never around the reaction job
    /// that resumes an `await`, so a continuation cannot be told which lane
    /// it belongs to. This pins why scripts are rewritten instead (see
    /// `rewrite.rs`).
    #[test]
    fn promise_hooks_do_not_bracket_await_continuations() {
        let rt = Runtime::new().expect("runtime");
        let seen: Rc<RefCell<Vec<&'static str>>> = Rc::default();
        let log = Rc::clone(&seen);
        rt.set_promise_hook(Some(Box::new(move |_ctx, kind, _promise, _parent| {
            log.borrow_mut().push(match kind {
                PromiseHookType::Init => "init",
                PromiseHookType::Before => "before",
                PromiseHookType::After => "after",
                PromiseHookType::Resolve => "resolve",
            });
        })));
        let ctx = Context::custom::<Intrinsics>(&rt).expect("context");
        ctx.with(|ctx| {
            let src = "globalThis.out = []; let r; const p = new Promise(x => r = x); \
                       (async () => { await p; out.push('resumed') })(); r(1);";
            ctx.eval::<(), _>(src).expect("eval");
        });
        seen.borrow_mut().clear();
        while rt.execute_pending_job().expect("job") {}
        let resumed = ctx.with(|ctx| ctx.eval::<String, _>("out.join()").expect("out"));
        assert_eq!(resumed, "resumed");
        assert!(!seen.borrow().contains(&"before"), "{:?}", seen.borrow());
    }
}
