// The workflow realm's globals, over one host function. Engine-independent:
// any JavaScript engine that can call a host function with two strings and
// call back into the object this returns can run a workflow.
//
// host(op, payloadJson) -> '{"v": value}' or '{"e": message, "n": name?, "t": "TypeError"?}'
(function (host, cfgText) {
  'use strict';
  const G = globalThis;
  const { stringify, parse } = JSON;
  const isArray = Array.isArray;
  const P = Promise;
  const allSettled = P.allSettled.bind(P);
  const freeze = Object.freeze;
  const define = Object.defineProperty;
  const getProto = Object.getPrototypeOf;
  const S = String;
  const Err = Error;
  const TypeErr = TypeError;
  const EvalErr = EvalError;
  const MapC = Map;
  const cfg = parse(cfgText);
  const MAX = 4096;
  const apply = Reflect.apply;

  // Lanes: Claude Code keys each agent() call by its async branch, which it
  // follows with AsyncLocalStorage. Here the current lane is `lane` (an id
  // the run keeps the hashes of); the rewritten script restores it after
  // every await (see engine/rewrite.rs), callbacks below restore theirs.
  let lane = cfg.lane ?? 0;
  const inLane = (id, fn) => {
    const prev = lane;
    lane = id;
    try { return fn(); } finally { lane = prev; }
  };
  const bound = (fn) => {
    if (typeof fn !== 'function') return fn;
    const mine = lane;
    return function (v) { lane = mine; return fn(v); };
  };
  const nativeThen = P.prototype.then;
  define(P.prototype, 'then', {
    value: function then(onFulfilled, onRejected) {
      return apply(nativeThen, this, [bound(onFulfilled), bound(onRejected)]);
    },
    writable: true, enumerable: false, configurable: true,
  });
  if (typeof G.queueMicrotask === 'function') {
    const queue = G.queueMicrotask;
    define(G, 'queueMicrotask', {
      value: function queueMicrotask(fn) { return apply(queue, G, [bound(fn)]); },
      writable: true, enumerable: false, configurable: true,
    });
  }
  const laneOps = [() => lane, (id, v) => { lane = id; return v; }, (id) => { lane = id; }];

  // Claude Code's determinism shims, verbatim.
  (() => {
    'use strict';
    const NOW_ERR = "Date.now() / new Date() are unavailable in workflow scripts (breaks resume). Stamp results after the workflow returns, or pass timestamps via args.";
    const RANDOM_ERR = "Math.random() is unavailable in workflow scripts (breaks resume). For N independent samples, include the index in the agent label or prompt.";
    Math.random = function random() { throw new Error(RANDOM_ERR) };
    const RealDate = Date;
    RealDate.now = function now() { throw new Error(NOW_ERR) };
    function ShimDate(...a) {
      if (!new.target) throw new Error(NOW_ERR); // bare Date() -> now-string
      if (a.length === 0) throw new Error(NOW_ERR);
      return Reflect.construct(RealDate, a, new.target);
    }
    ShimDate.now = RealDate.now;
    ShimDate.parse = RealDate.parse;
    ShimDate.UTC = RealDate.UTC;
    ShimDate.prototype = RealDate.prototype;
    RealDate.prototype.constructor = ShimDate;
    Object.freeze(RealDate);
    globalThis.Date = ShimDate;
  })();

  // No code from strings (node:vm's codeGeneration: {strings: false}).
  const noCodegen = function Function() {
    throw new EvalErr('Code generation from strings disallowed for this context');
  };
  noCodegen.prototype = Function.prototype;
  delete G.eval;
  for (const proto of [
    Function.prototype,
    getProto(async function () {}),
    getProto(function* () {}),
    getProto(async function* () {}),
  ]) {
    define(proto, 'constructor', { value: noCodegen, writable: false, enumerable: false, configurable: false });
  }
  define(G, 'Function', { value: noCodegen, writable: true, enumerable: false, configurable: true });

  const capError = (n) =>
    new Err(`array length ${n} exceeds the maximum of ${MAX} supported across the workflow VM boundary`);
  const toJson = (v) =>
    stringify(v, function (_k, x) {
      if (isArray(x) && x.length > MAX) throw capError(x.length);
      return x;
    });
  // An array-like's items, at most MAX of them (Claude Code refuses more).
  const snapshot = (v) => {
    if (v === null || typeof v !== 'object') return [];
    const n = v.length;
    if (typeof n !== 'number' || !(n >= 0) || n !== Math.floor(n) || n > Number.MAX_SAFE_INTEGER)
      throw new Err('array length is not a safe integer across the workflow VM boundary');
    if (n > MAX) throw capError(n);
    const out = [];
    for (let i = 0; i < n; i++) {
      try { out[i] = v[i]; } catch { out[i] = undefined; }
    }
    return out;
  };
  const named = (e, name) => {
    if (name) define(e, 'name', { value: name, writable: true, enumerable: false, configurable: true });
    return e;
  };
  const call = (op, payload) => {
    const r = parse(host(op, payload === undefined ? '' : toJson(payload)));
    if (r.e !== undefined) throw named(r.t === 'TypeError' ? new TypeErr(r.e) : new Err(r.e), r.n);
    return r.v;
  };
  const messageOf = (e) => {
    try {
      if (e !== null && typeof e === 'object' && typeof e.message === 'string') return e.message;
      return S(e);
    } catch {
      return '<unprintable thrown value>';
    }
  };
  const nameOf = (e) => {
    try {
      return e !== null && typeof e === 'object' && typeof e.name === 'string' ? e.name : 'Error';
    } catch {
      return 'Error';
    }
  };
  const errorInfo = (e) => {
    let stack;
    try { stack = e !== null && typeof e === 'object' && typeof e.stack === 'string' ? e.stack : undefined; } catch {}
    return { name: nameOf(e), message: messageOf(e), stack };
  };

  let aborted = false;
  const never = () => new P(() => {});
  const pending = new MapC();
  const track = (id) => new P((resolve, reject) => { pending.set(id, [resolve, reject]); });

  const agent = async function agent(prompt, opts) {
    if (aborted) return never();
    const o = opts !== null && typeof opts === 'object' ? opts : undefined;
    const id = call('agent', { prompt: S(prompt), opts: o, lane });
    return track(id);
  };

  const settled = async (results, kind) => {
    let dropped = 0;
    const out = results.map((r, i) => {
      if (r.status === 'fulfilled') return r.value.v;
      if (nameOf(r.reason) === 'WorkflowBudgetExceededError') {
        dropped++;
        return null;
      }
      call('failure', { message: `${kind}[${i}] failed: ${messageOf(r.reason)}`, log: true });
      return null;
    });
    if (dropped > 0)
      call('failure', { message: `${kind}: ${dropped} ${dropped === 1 ? 'slot' : 'slots'} dropped — token budget exceeded`, log: false });
    return out;
  };

  const parallel = async function parallel(thunks) {
    const mine = lane;
    if (aborted) return never();
    const fork = call('fork', { lane: mine, kind: 'parallel' });
    await undefined;
    lane = mine;
    if (!isArray(thunks)) throw new TypeErr('parallel() expects an array of functions');
    const fns = snapshot(thunks);
    if (fns.length === 0) return [];
    call('caps');
    for (const f of fns)
      if (typeof f !== 'function')
        throw new TypeErr('parallel() expects an array of functions, not promises. Wrap each call: () => agent(...)');
    const slots = call('slots', { fork, n: fns.length });
    const results = await allSettled(fns.map((f, i) => {
      try {
        return inLane(slots[i], () => P.resolve(f()).then((v) => ({ v })));
      } catch (e) {
        return P.reject(e);
      }
    }));
    lane = mine;
    call('join', { fork });
    return settled(results, 'parallel');
  };

  const pipeline = async function pipeline(items, ...stages) {
    const mine = lane;
    if (aborted) return never();
    const fork = call('fork', { lane: mine, kind: 'pipeline' });
    await undefined;
    lane = mine;
    if (!isArray(items)) throw new TypeErr('pipeline() expects an array as the first argument');
    const list = snapshot(items);
    const fns = snapshot(stages);
    if (list.length === 0) return [];
    call('caps');
    for (const f of fns)
      if (typeof f !== 'function')
        throw new TypeErr('pipeline() stages must be functions: pipeline(items, item => ..., result => ...)');
    const slots = call('slots', { fork, n: list.length });
    const results = await allSettled(list.map((item, i) => inLane(slots[i], async () => {
      const slot = lane;
      let v = await item;
      lane = slot;
      for (const stage of fns) {
        if (v === null) break;
        v = await stage(v, item, i);
        lane = slot;
      }
      return { v };
    })));
    lane = mine;
    call('join', { fork });
    return settled(results, 'pipeline');
  };

  const log = function log(message) { call('log', { message: S(message) }); };
  const phase = cfg.child
    ? function phase(_title) {}
    : function phase(title) { call('phase', { title: S(title) }); };

  const formatArgs = (args) =>
    args.map((w) => {
      if (typeof w === 'string') return w;
      if (w === null || (typeof w !== 'object' && typeof w !== 'function')) {
        try { return stringify(w); } catch { return `[${typeof w}]`; }
      }
      let fallback;
      try {
        const r = toJson(w);
        if (r !== undefined && r !== '{}') return r;
        fallback = r ?? `[${typeof w}]`;
      } catch (e) {
        return messageOf(e).includes('exceeds the maximum')
          ? `[${typeof w}: array exceeds the ${MAX}-element logging cap]`
          : `[${typeof w}]`;
      }
      let t;
      try { t = S(w); } catch { t = '<unprintable>'; }
      return t === '[object Object]' || t === '<unprintable>' ? fallback : t;
    }).join(' ');
  const prefix = cfg.child ? `[${cfg.child.name}] ` : '';
  const say = (head) => function (...args) { call('log', { message: prefix + head + formatArgs(args) }); };
  const console = freeze({ __proto__: null, log: say(''), info: say(''), debug: say(''), error: say('[error] '), warn: say('[warn] ') });

  const total = cfg.total ?? null;
  const budget = freeze({
    __proto__: null,
    total,
    spent: function spent() { return call('spent'); },
    remaining: function remaining() { return total == null ? Infinity : Math.max(0, total - call('spent')); },
  });

  const timers = new MapC();
  const setTimeout = function setTimeout(fn, ms) {
    if (aborted) return 0;
    const delay = typeof ms === 'number' ? ms : typeof ms === 'string' ? +ms || 0 : 0;
    const id = call('timer', { ms: delay });
    timers.set(id, [fn, lane]);
    return id;
  };
  const clearTimeout = function clearTimeout(id) {
    if (typeof id === 'number' || typeof id === 'string') {
      const n = typeof id === 'number' ? id : +id;
      if (timers.has(n)) {
        timers.delete(n);
        call('clear', { id: n });
      }
    }
  };

  const workflow = cfg.child
    ? function workflow() {
        return P.reject(new Err('workflow() cannot be called from within a child workflow — nesting is limited to one level. Inline the inner script or call its agents directly.'));
      }
    : async function workflow(nameOrRef, args) {
        if (aborted) return never();
        let ref;
        if (typeof nameOrRef === 'string') ref = nameOrRef;
        else if (nameOrRef !== null && typeof nameOrRef === 'object' && typeof nameOrRef.scriptPath === 'string')
          ref = { scriptPath: nameOrRef.scriptPath };
        else throw new TypeErr('workflow() expects a workflow name (string) or {scriptPath: string}');
        const id = call('workflow', args === undefined ? { ref, lane } : { ref, args, lane });
        return track(id);
      };

  const globals = { agent, parallel, pipeline, workflow, log, phase, console, budget, setTimeout, clearTimeout };
  for (const name of Object.keys(globals))
    define(G, name, { value: globals[name], writable: true, enumerable: true, configurable: true });
  define(G, 'args', {
    value: cfg.args === undefined ? undefined : parse(cfg.args),
    writable: true,
    enumerable: true,
    configurable: true,
  });

  return freeze({
    settle(id, ok, text) {
      const p = pending.get(id);
      if (p === undefined) return;
      pending.delete(id);
      if (ok) {
        p[0](parse(text));
      } else {
        const r = parse(text);
        p[1](named(new Err(r.message), r.name));
      }
    },
    fire(id) {
      const timer = timers.get(id);
      if (timer === undefined) return;
      timers.delete(id);
      lane = timer[1];
      try { timer[0](); } catch {}
    },
    // `script` is the rewritten body as a function of the lane operations.
    run(script) {
      lane = cfg.lane ?? 0;
      let promise;
      try {
        promise = apply(script, undefined, laneOps);
      } catch (e) {
        promise = P.reject(e);
      }
      P.resolve(promise).then(
        (v) => {
          if (typeof v === 'function') {
            host('fail', stringify({ name: 'Error', message: 'workflow result cannot be a function' }));
            return;
          }
          let text;
          try {
            text = v === undefined ? '' : toJson(v);
          } catch (e) {
            host('fail', stringify(errorInfo(e)));
            return;
          }
          host('done', text === undefined ? '' : text);
        },
        (e) => { host('fail', stringify(errorInfo(e))); },
      );
    },
    abort() {
      aborted = true;
      timers.clear();
    },
  });
})
