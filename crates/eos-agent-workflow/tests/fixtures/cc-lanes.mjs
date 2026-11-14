// Claude Code 2.1.319's resume keys, transcribed from its bundle (`zr`, `ro`,
// `Ho`, `ai`, the lane fork `Ve` and the key/replay part of `agent()`), run
// with the real AsyncLocalStorage. For each scenario: a fresh run of the
// first script, then each later script resumed from the journal so far; for
// every agent() call, its prompt, its key and whether it replayed.
// `tests/lanes.rs` runs the same scenarios through our engine.
//
//   node tests/fixtures/cc-lanes.mjs > tests/fixtures/cc-lanes.json   (bun prints the same)
import { AsyncLocalStorage } from 'node:async_hooks';
import { createHash } from 'node:crypto';

const ai = (e) => {
  if (!e) return '{}';
  const n = {};
  for (const h of ['schema', 'model', 'effort', 'isolation', 'agentType', 'disallowedTools', 'bashCommandClamp']) {
    const w = e[h];
    if (w === undefined || typeof w === 'function') continue;
    n[h] = w;
  }
  const s = (h) => {
    if (typeof h === 'function') return;
    if (Array.isArray(h)) return h.map(s);
    if (h && typeof h === 'object') {
      const w = {};
      for (const b of Object.keys(h).sort()) if (b !== '__proto__') w[b] = s(h[b]);
      return w;
    }
    return h;
  };
  return JSON.stringify(s(n));
};
const zr = (prompt, opts, prev) =>
  'v2:' + createHash('sha256').update(prev).update('\0').update(prompt).update('\0').update(ai(opts)).digest('hex');
const ro = (prev, kind, r) => {
  const s = createHash('sha256').update(prev).update('\0').update(kind).update('\0');
  if (typeof r === 'number') s.update(`slot:${r}`);
  else if (r === 'fork') s.update('fork');
  else {
    s.update('join');
    for (const h of r) s.update('\0').update(h);
  }
  return s.digest('hex');
};
const Ho = (key, agentId) => createHash('sha256').update(key).update('\0replaced\0').update(agentId).digest('hex');

// Claude Code's `oo`.
function replayOf(journal) {
  const results = new Map(), started = new Map(), failed = new Set(), demoted = new Set(), replaced = new Set();
  for (const t of journal) {
    if (t.type === 'result') {
      if (results.has(t.key) || demoted.has(t.key) || failed.has(t.key)) replaced.add(t.key);
      results.set(t.key, t);
      demoted.delete(t.key);
    } else if (t.type === 'started') {
      if (results.delete(t.key)) demoted.add(t.key);
      (started.get(t.key) ?? started.set(t.key, []).get(t.key)).push(t);
    } else if (t.type === 'failed') {
      results.delete(t.key);
      failed.add(t.key);
    }
  }
  return { results, started, failed, demoted, replaced };
}

const tick = (ms) => new Promise((r) => setTimeout(r, ms));
// `wait:<ms>:` in a prompt delays its agent.
const delay = (p) => Number(/^wait:(\d+):/.exec(p)?.[1] ?? 1);

// Agent ids as tests/run.rs's fake hands them out: a count, in hex.
let n = 0;
const agentId = () => `a${(++n).toString(16).padStart(16, '0')}`;

async function run(body, journal) {
  const R = journal ? replayOf(journal) : undefined;
  const out = [], log = journal ? [] : [{ type: 'launched' }];
  const Rt = new AsyncLocalStorage(), je = { prev: '' };
  let Tt = false;
  function Ve(kind) {
    const W = Rt.getStore() ?? je, Ae = W.prev;
    W.prev = ro(Ae, kind, 'fork');
    const Le = [];
    return {
      inSlot: (i, fn) => { const Qe = { prev: ro(Ae, kind, i) }; Le[i] = Qe; return Rt.run(Qe, fn); },
      join: () => { W.prev = ro(W.prev, kind, Array.from(Le, (g) => g?.prev ?? '')); },
    };
  }
  const agent = async (prompt, opts) => {
    const be = Rt.getStore() ?? je;
    prompt = String(prompt);
    const me = zr(prompt, opts, be.prev);
    be.prev = me;
    const F = Tt ? undefined : R?.results.get(me);
    if (F !== undefined) {
      if (R.replaced.has(me)) be.prev = Ho(me, F.agentId);
      out.push({ prompt, key: me, cached: true });
      return F.result;
    }
    const Te = R?.started.get(me);
    if (!(!Tt && Te !== undefined && Te.length > 0 && !R.failed.has(me) && !R.demoted.has(me))) Tt = true;
    const gn = R !== undefined && (R.results.has(me) || R.demoted.has(me) || R.failed.has(me));
    out.push({ prompt, key: me, cached: false });
    const id = agentId();
    log.push({ type: 'started', key: me, agentId: id });
    await tick(delay(prompt));
    const result = `echo:${prompt}`;
    log.push({ type: 'result', key: me, agentId: id, result });
    if (gn && be.prev === me) be.prev = Ho(me, id);
    return result;
  };
  const ue = async (v) => ({ v: await v });
  const parallel = async (thunks) => {
    const M = Ve('parallel');
    await tick(0);
    if (thunks.length === 0) return [];
    const r = await Promise.allSettled(thunks.map((f, i) => { try { return M.inSlot(i, () => ue(f())); } catch (e) { return Promise.reject(e); } }));
    M.join();
    return r.map((x) => (x.status === 'fulfilled' ? x.value.v : null));
  };
  const pipeline = async (items, ...stages) => {
    const W = Ve('pipeline');
    await tick(0);
    if (items.length === 0) return [];
    const r = await Promise.allSettled(items.map((item, i) => W.inSlot(i, async () => {
      let be = await ue(item);
      for (const s of stages) { if (be.v === null) break; be = await ue(s(be.v, item, i)); }
      return be;
    })));
    W.join();
    return r.map((x) => (x.status === 'fulfilled' ? x.value.v : null));
  };
  const AsyncFunction = (async () => {}).constructor;
  const result = await new AsyncFunction('agent', 'parallel', 'pipeline', `'use strict';\n${body}`)(agent, parallel, pipeline);
  return { out, log, result };
}

export const SCENARIOS = {
  // Claude Code re-runs everything from the first call whose branch changed.
  removed: [
    "const a = await agent('A')\nconst b = await agent('B')\nreturn [a, b, await agent('C')]",
    "const b = await agent('B')\nreturn [b, await agent('C')]",
  ],
  moved: [
    "const a = await agent('A')\nconst b = await agent('B')\nreturn [a, b, await agent('C')]",
    "const b = await agent('B')\nconst a = await agent('A')\nreturn [a, b, await agent('C')]",
  ],
  tail_edit: [
    "const a = await agent('A')\nconst b = await agent('B')\nreturn [a, b, await agent('C')]",
    "const a = await agent('A')\nconst b = await agent('B')\nreturn [a, b, await agent('C, edited')]",
  ],
  parallel_reordered: [
    "const r = await parallel([() => agent('X'), () => agent('Y')])\nreturn [r, await agent('Z')]",
    "const r = await parallel([() => agent('Y'), () => agent('X')])\nreturn [r, await agent('Z')]",
  ],
  pipeline_nested: [
    "const out = await pipeline(['p', 'q'], async d => { const a = await agent('review ' + d); return a }, r => parallel([0, 1].map(i => () => agent(`verify ${r} ${i}`))))\nreturn [out, await agent('summary')]",
    "const out = await pipeline(['p', 'q'], async d => { const a = await agent('review ' + d); return a }, r => parallel([0, 1].map(i => () => agent(`verify ${r} ${i}`))))\nreturn [out, await agent('summary, edited')]",
  ],
  then_chains: [
    "const r = await parallel(['m', 'n'].map(x => () => agent('first ' + x).then(a => agent('second ' + a)).then(b => agent('third ' + b))))\nreturn r",
    "const r = await parallel(['m', 'n'].map(x => () => agent('first ' + x).then(a => agent('second ' + a)).then(b => agent('third ' + b))))\nreturn r",
  ],
  catch_and_timer: [
    "let x\ntry { await Promise.reject(new Error('no')) } catch (e) { x = await agent('caught ' + e.message) }\nawait new Promise(r => setTimeout(r, 5))\nreturn [x, await agent('later')]",
    "let x\ntry { await Promise.reject(new Error('no')) } catch (e) { x = await agent('caught ' + e.message) }\nawait new Promise(r => setTimeout(r, 5))\nreturn [x, await agent('later')]",
  ],
  // The root resumes after a promise a branch resolved: still the root's lane.
  gate: [
    "let release\nconst gate = new Promise(r => { release = r })\nconst branch = parallel([async () => { const a = await agent('wait:5:inner'); release(); return await agent('inner 2 ' + a) }, async () => { await agent('wait:20:other'); return await agent('other 2') }])\nawait gate\nconst z = await agent('root')\nreturn [await branch, z]",
    "let release\nconst gate = new Promise(r => { release = r })\nconst branch = parallel([async () => { const a = await agent('wait:5:inner'); release(); return await agent('inner 2 ' + a) }, async () => { await agent('wait:20:other'); return await agent('other 2') }])\nawait gate\nconst z = await agent('root')\nreturn [await branch, z]",
  ],
  // A rejected await resumes in a catch or finally block, in its own lane
  // (here another branch rejected it).
  catch_in_branch: [
    "let fail\nconst shared = new Promise((_, rej) => { fail = rej })\nreturn await parallel([async () => { try { await shared } catch (e) { return await agent('in catch ' + e.message) } }, async () => { try { await shared } finally { await agent('in finally') } }, async () => { await agent('wait:5:b'); fail(new Error('x')); return await agent('b2') }])",
    "let fail\nconst shared = new Promise((_, rej) => { fail = rej })\nreturn await parallel([async () => { try { await shared } catch (e) { return await agent('in catch ' + e.message) } }, async () => { try { await shared } finally { await agent('in finally') } }, async () => { await agent('wait:5:b'); fail(new Error('x')); return await agent('b2') }])",
  ],
  // for await over an async generator, in two branches.
  for_await: [
    "async function* gen(xs) { for (const x of xs) yield await agent('wait:' + (x.length * 3) + ':gen ' + x) }\nreturn await parallel(['p', 'qq'].map(k => async () => { const out = []; for await (const v of gen([k + '1', k + '2'])) out.push(await agent('use ' + v)); return out.concat(await agent('done ' + k)) }))",
    "async function* gen(xs) { for (const x of xs) yield await agent('wait:' + (x.length * 3) + ':gen ' + x) }\nreturn await parallel(['p', 'qq'].map(k => async () => { const out = []; for await (const v of gen([k + '1', k + '2'])) out.push(await agent('use ' + v)); return out.concat(await agent('done ' + k)) }))",
  ],
  // for await whose steps another branch settles.
  for_await_shared: [
    "let o1, o2\nconst g1 = new Promise(r => { o1 = r }), g2 = new Promise(r => { o2 = r })\nlet i = 0\nconst it = { [Symbol.asyncIterator]() { return { next: () => (i++ === 0 ? g1 : g2) } } }\nreturn await parallel([async () => { const out = []; for await (const v of it) out.push(await agent('use ' + v)); return out.concat(await agent('after loop')) }, async () => { await agent('wait:5:b'); o1({ value: 'v', done: false }); await agent('wait:20:b2'); o2({ value: undefined, done: true }); return await agent('b3') }])",
    "let o1, o2\nconst g1 = new Promise(r => { o1 = r }), g2 = new Promise(r => { o2 = r })\nlet i = 0\nconst it = { [Symbol.asyncIterator]() { return { next: () => (i++ === 0 ? g1 : g2) } } }\nreturn await parallel([async () => { const out = []; for await (const v of it) out.push(await agent('use ' + v)); return out.concat(await agent('after loop')) }, async () => { await agent('wait:5:b'); o1({ value: 'v', done: false }); await agent('wait:20:b2'); o2({ value: undefined, done: true }); return await agent('b3') }])",
  ],
  // Y's key is unchanged but it runs again (an earlier call missed), so its
  // lane goes on from a new hash; resumed once more, that hash replays.
  rerun_replaced: [
    "const r = await parallel([() => agent('wait:5:X'), () => agent('wait:30:Y').then(y => agent('after ' + y))])\nreturn [r, await agent('Z')]",
    "const r = await parallel([() => agent('wait:5:X, edited'), () => agent('wait:30:Y').then(y => agent('after ' + y))])\nreturn [r, await agent('Z')]",
    "const r = await parallel([() => agent('wait:5:X, edited'), () => agent('wait:30:Y').then(y => agent('after ' + y))])\nreturn [r, await agent('Z')]",
  ],
};

const scenarios = [];
for (const [name, scripts] of Object.entries(SCENARIOS)) {
  n = 0;
  const journal = [], runs = [];
  for (const script of scripts) {
    const { out, log } = await run(script, runs.length ? journal : undefined);
    journal.push(...log);
    runs.push(out);
  }
  scenarios.push({ name, scripts, runs });
}
console.log(JSON.stringify(scenarios, null, 1));
