// The page's logic, without a browser: run with `node --test` from the
// repository root, on the Node that GitHub's runners ship. No packages.
//
// What a browser alone can show — pixels, the policy enforced, fetch
// streaming, the narrow layout, perceived frame rate — is in the manual
// script in the README. What is here is everything the script decides, and,
// at the end, the whole page booted against a store recorded from the
// daemon, on a document and a window just wide enough for it.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const Brain = createRequire(import.meta.url)(join(here, "..", "brain.js"));

// ---- text -------------------------------------------------------------------

test("a stored script tag renders as text, never as markup", () => {
  const sneaky = '<script>alert(1)</script><img src=x onerror="alert(2)">';
  const out = Brain.esc(sneaky);
  assert.ok(!out.includes("<script"), out);
  assert.ok(!out.includes("<img"), out);
  assert.equal(out, "&lt;script&gt;alert(1)&lt;/script&gt;&lt;img src=x onerror=&quot;alert(2)&quot;&gt;");
  assert.equal(Brain.esc(null), "");
  assert.equal(Brain.esc("a & b's"), "a &amp; b&#39;s");
});

test("tokens are bytes over four rounded up, and the headline counts things", () => {
  assert.equal(Brain.tokens(1420), 355);
  assert.equal(Brain.tokens(1), 1);
  assert.equal(Brain.tokens(0), 0);
  assert.equal(Brain.headline({ twice: 0 }), "0 things your agents did not have to learn twice");
  assert.equal(Brain.headline({ twice: 1 }), "1 thing your agents did not have to learn twice");
  assert.equal(Brain.headline({ twice: 7 }), "7 things your agents did not have to learn twice");
  assert.equal(Brain.plural(1, "node"), "1 node");
  assert.equal(Brain.plural(2, "entry", "entries"), "2 entries");
});

// ---- the token ----------------------------------------------------------------

test("the token comes from the fragment and nowhere else", () => {
  const token = "ab".repeat(32);
  assert.equal(Brain.readToken(`#t=${token}`), token);
  assert.equal(Brain.readToken(`#x=1&t=${token}&y=2`), token);
  assert.equal(Brain.readToken(""), null);
  assert.equal(Brain.readToken("#t=short"), null);
  assert.equal(Brain.readToken("#t=ZZ" + "a".repeat(30)), null);
  assert.equal(Brain.readToken("?t=" + token), null, "a query string is not the fragment");
});

// ---- the session and the dead daemon ----------------------------------------

function fakeFetch(answers) {
  const calls = [];
  const fetch = async (path, init) => {
    calls.push({ path, auth: init && init.headers && init.headers.authorization });
    const answer = answers(path);
    if (answer instanceof Error) throw answer;
    return answer;
  };
  return { fetch, calls, TextDecoder };
}
const ok = (body) => ({ status: 200, ok: true, json: async () => body });
const refused = () => ({ status: 401, ok: false, json: async () => ({}) });

test("every request carries the token as a bearer header, never in the path", async () => {
  const io = fakeFetch(() => ok({ port: 1 }));
  const s = Brain.makeSession(io);
  s.token = "abc";
  await s.get("summary", { ns: "shop", branch: "main", at: null, q: "" });
  assert.equal(io.calls.length, 1);
  assert.equal(io.calls[0].path, "/brain/summary?ns=shop&branch=main");
  assert.equal(io.calls[0].auth, "Bearer abc");
  assert.ok(!io.calls[0].path.includes("abc"));
});

test("a refused token means a different daemon: stopped, and nothing more is asked", async () => {
  const io = fakeFetch(() => refused());
  const s = Brain.makeSession(io);
  s.token = "abc";
  const states = [];
  s.on((state) => states.push(state));
  await assert.rejects(() => s.get("summary"));
  assert.equal(s.state, "stopped");
  assert.deepEqual(states, ["stopped"]);
  await assert.rejects(() => s.get("graph"), /stopped/);
  assert.equal(io.calls.length, 1, "a stopped page asked again");
  s.set("connected");
  assert.equal(s.state, "stopped", "stopped is final");
});

test("when the stream ends, one probe decides: alive means a hiccup, anything else means stopped", async () => {
  // The daemon is gone: the probe cannot connect.
  let io = fakeFetch(() => new Error("connection refused"));
  let s = Brain.makeSession(io);
  s.token = "abc";
  assert.equal(await s.probe(), undefined);
  assert.equal(s.state, "stopped");
  assert.equal(io.calls.length, 1, "more than one probe");

  // Another daemon has the port: it refuses the token.
  io = fakeFetch(() => refused());
  s = Brain.makeSession(io);
  s.token = "abc";
  await s.probe();
  assert.equal(s.state, "stopped");

  // The daemon is there: the stream just broke.
  io = fakeFetch(() => ok({ port: 1 }));
  s = Brain.makeSession(io);
  s.token = "abc";
  assert.equal(await s.probe(), true);
  assert.equal(s.state, "connecting");
  assert.equal(io.calls[0].path, "/brain/summary", "the probe went somewhere else");
});

test("the event stream is read line by line with the token as a header", async () => {
  const lines = ['{"schema":1,"kind":"hello","port":7}', '{"kind":"operation","operation":"put"}', "not json", ""];
  const chunks = [lines.slice(0, 1).join("\n") + "\n{\"kind\":\"oper", 'ation","operation":"put"}\nnot json\n\n'];
  const body = {
    getReader() {
      let i = 0;
      return { read: async () => (i < chunks.length ? { value: new TextEncoder().encode(chunks[i++]), done: false } : { done: true }) };
    },
  };
  const io = fakeFetch(() => ({ status: 200, ok: true, body }));
  const seen = [];
  let ended = null;
  await Brain.streamEvents(io, "abc", (line) => seen.push(line), (why) => { ended = why; });
  assert.equal(io.calls[0].path, "/events");
  assert.equal(io.calls[0].auth, "Bearer abc");
  assert.deepEqual(seen.map((l) => l.kind), ["hello", "operation"]);
  assert.equal(ended, "ended");

  let refusedWhy = null;
  await Brain.streamEvents(fakeFetch(() => refused()), "abc", () => {}, (why) => { refusedWhy = why; });
  assert.equal(refusedWhy, "refused");
});

// ---- the graph ---------------------------------------------------------------

const graphJson = () => ({
  seq: 3, at: null, few: 12, height: 65535,
  kinds: ["agent", "brief", "decision", "handoff", "lesson", "task", "fact", "entry", "file"],
  agents: ["Claude Code"],
  nodes: [
    ["agent:Claude Code", 0, "Claude Code", 0, 32767, 0, "connected", null],
    ["shop:decision:payments", 2, null, 2, 32767, 2, null, 0],
    ["shop:fact:auth", 6, "auth", 5, 32767, 1, null, 0],
    ["file:src/a.rs", 8, "src/a.rs", 6, 32767, 1, null, null],
  ],
  edges: [[3, 2, "source of"], [2, 1, "cited by"], [0, 2, "wrote"]],
  counts: {},
});

test("the hash matches the daemon's, so a node added from an event lands where the graph will put it", () => {
  // FNV-1a 64: the top sixteen bits of the hash of "a" and of "".
  assert.equal(Brain.hashY("a"), 0xaf63);
  assert.equal(Brain.hashY(""), 0xcbf2);
  assert.equal(Brain.hashY("shop:decision:payments"), Brain.hashY("shop:decision:payments"));
});

test("a graph loads with kinds and writers resolved and labels derived, and a reload keeps birth times", () => {
  const g = Brain.makeGraph();
  const fresh = Brain.loadGraph(g, graphJson(), 100);
  assert.equal(fresh.length, 0, "the first graph simply appears");
  const d = g.byId.get("shop:decision:payments");
  assert.equal(d.kind, "decision");
  assert.equal(d.label, "payments");
  assert.equal(d.who, "Claude Code");
  assert.equal(g.edges.length, 3);
  assert.equal(g.edges[0].a, "file:src/a.rs");
  d.born = 42;
  const again = graphJson();
  again.nodes.push(["shop:lesson:00000001", 4, "a lesson", 3, 1000, 3, null, 0]);
  const added = Brain.loadGraph(g, again, 500);
  assert.equal(g.byId.get("shop:decision:payments").born, 42, "a reload re-animated a node");
  assert.deepEqual(added.map((n) => n.id), ["shop:lesson:00000001"]);
  assert.equal(added[0].born, 500);
});

test("few nodes are spaced evenly in id order and many are placed by hash", () => {
  const g = Brain.makeGraph();
  Brain.loadGraph(g, { seq: 0, nodes: [], edges: [], few: 12, height: 65535 }, 0);
  for (let i = 0; i < 3; i++) Brain.addLocal(g, `shop:decision:d${i}`, "decision", `d${i}`, null, 1, 0);
  assert.deepEqual(g.nodes.map((n) => n.y), [10922, 32767, 54612], "the daemon's even spacing");
  for (let i = 3; i < 20; i++) Brain.addLocal(g, `shop:decision:d${i}`, "decision", `d${i}`, null, 1, 0);
  const before = g.nodes.map((n) => [n.id, n.y]);
  assert.equal(g.byId.get("shop:decision:d5").y, Brain.hashY("shop:decision:d5"));
  Brain.addLocal(g, "shop:decision:one-more", "decision", "one more", null, 1, 0);
  for (const [id, y] of before) assert.equal(g.byId.get(id).y, y, `adding a node moved ${id}`);
});

test("lenses and the timeline filter the same graph", () => {
  const g = Brain.makeGraph();
  Brain.loadGraph(g, graphJson(), 0);
  const ids = (lens, scrub) => g.nodes.filter((n) => Brain.visible(g, n, lens, scrub)).map((n) => n.id);
  assert.equal(ids("all", null).length, 4);
  assert.deepEqual(ids("fact", null), ["shop:fact:auth", "file:src/a.rs"]);
  assert.deepEqual(ids("plan", null), ["agent:Claude Code"]);
  assert.deepEqual(ids("all", 1), ["agent:Claude Code", "shop:fact:auth", "file:src/a.rs"], "the past hides what came later");
  assert.deepEqual(ids("race", null), []);
});

// ---- events ------------------------------------------------------------------

test("every pulse is one real event, and an event from another project is ignored", () => {
  const g = Brain.makeGraph();
  Brain.loadGraph(g, graphJson(), 0);
  const ctx = { ns: "shop", branch: "main" };
  let fx = Brain.applyEvent(g, { kind: "operation", client: "Codex CLI", namespace: "shop", branch: "main", operation: "put", key: "shop:lesson:00000001", ok: true }, ctx, 10);
  assert.ok(g.byId.has("shop:lesson:00000001"), "the written entry was not added");
  assert.ok(g.byId.has("agent:Codex CLI"), "the writer was not added");
  assert.deepEqual(fx.pulses.map((p) => [p.a, p.b]), [["agent:Codex CLI", "shop:lesson:00000001"]]);
  assert.ok(fx.refetch && fx.panels);

  fx = Brain.applyEvent(g, { kind: "operation", client: "Codex CLI", namespace: "shop", operation: "fact", key: "shop:fact:auth", detail: "stale", ok: true }, ctx, 20);
  assert.equal(g.byId.get("shop:fact:auth").state, "stale");
  assert.deepEqual(fx.pulses.map((p) => [p.a, p.b]), [["file:src/a.rs", "shop:fact:auth"], ["shop:fact:auth", "shop:decision:payments"]], "the amber pulse runs from the file to the fact and on to what cited it");

  fx = Brain.applyEvent(g, { kind: "operation", client: "Codex CLI", namespace: "shop", branch: "main", operation: "claim", key: "shop:task:t1", ok: true }, ctx, 30);
  assert.equal(g.byId.get("shop:task:t1").state, "claimed");
  assert.ok(g.edges.some((e) => e.kind === "holds" && e.a === "agent:Codex CLI" && e.b === "shop:task:t1"));
  fx = Brain.applyEvent(g, { kind: "operation", client: "Claude Code", namespace: "shop", branch: "main", operation: "claim", key: "shop:task:t1", ok: false, detail: "held by Codex CLI" }, ctx, 40);
  assert.equal(fx.pulses[0].reverse, true, "a refused claim pulses back");
  fx = Brain.applyEvent(g, { kind: "operation", client: "Codex CLI", namespace: "shop", branch: "main", operation: "release", key: "shop:task:t1", ok: true }, ctx, 50);
  assert.equal(g.byId.get("shop:task:t1").state, "open");
  assert.ok(!g.edges.some((e) => e.kind === "holds"));

  assert.equal(Brain.applyEvent(g, { kind: "operation", client: "x", namespace: "other", operation: "put", key: "other:decision:d", ok: true }, ctx, 60), null);
  assert.ok(!g.byId.has("other:decision:d"));

  fx = Brain.applyEvent(g, { kind: "disconnected", client: "Codex CLI", namespace: "shop", ok: true }, ctx, 70);
  assert.equal(g.byId.get("agent:Codex CLI").state, "away");
  fx = Brain.applyEvent(g, { kind: "connected", client: "Codex CLI", namespace: "shop", ok: true }, ctx, 80);
  assert.equal(g.byId.get("agent:Codex CLI").state, "connected");
  fx = Brain.applyEvent(g, { kind: "operation", client: "Codex CLI", namespace: "shop", branch: "main", operation: "delete", key: "shop:lesson:00000001", ok: true }, ctx, 90);
  assert.ok(!g.byId.has("shop:lesson:00000001"));
});

test("a briefing that arrives with a graph draws from what it carried and flows to its agent", () => {
  const g = Brain.makeGraph();
  Brain.loadGraph(g, graphJson(), 0);
  const json = graphJson();
  json.nodes.push(["brief:000000000001", 1, "briefing · 640 B · to Claude Code", 1, 30000, 3, null, null]);
  json.edges.push([1, 4, "in"], [4, 0, "served to"]);
  const fresh = Brain.loadGraph(g, json, 100);
  const pulses = Brain.announce(g, fresh);
  assert.deepEqual(pulses.map((p) => [p.a, p.b]), [["shop:decision:payments", "brief:000000000001"], ["brief:000000000001", "agent:Claude Code"]]);
  assert.equal(pulses[1].delay, 900);
});

test("an event is applied well within one frame, even on a large graph", () => {
  const g = Brain.makeGraph();
  const json = graphJson();
  for (let i = 0; i < 100000; i++) json.nodes.push([`shop:decision:d${i}`, 2, null, 2, (i * 7919) % 65535, 1, null, 0]);
  Brain.loadGraph(g, json, 0);
  const ctx = { ns: "shop", branch: "main" };
  const events = 2000;
  const start = performance.now();
  for (let i = 0; i < events; i++) {
    Brain.applyEvent(g, { kind: "operation", client: "Codex CLI", namespace: "shop", branch: "main", operation: "put", key: `shop:decision:d${i}`, ok: true }, ctx, i);
  }
  const each = (performance.now() - start) / events;
  console.log(`brain page: one event applied in ${each.toFixed(3)} ms on ${g.nodes.length} nodes`);
  assert.ok(each < 4, `an event took ${each} ms on average; a frame is 16`);
});

test("the keyboard moves to the nearest node in a direction", () => {
  const nodes = [{ id: "a" }, { id: "b" }, { id: "c" }, { id: "d" }];
  const at = { a: [0, 0], b: [100, 0], c: [0, 100], d: [100, 100] };
  const pos = (n) => ({ x: at[n.id][0], y: at[n.id][1] });
  assert.equal(Brain.nearest(nodes, nodes[0], "right", pos).id, "b");
  assert.equal(Brain.nearest(nodes, nodes[0], "down", pos).id, "c");
  assert.equal(Brain.nearest(nodes, nodes[3], "left", pos).id, "c");
  assert.equal(Brain.nearest(nodes, nodes[3], "up", pos).id, "b");
  assert.equal(Brain.nearest(nodes, nodes[0], "left", pos), null);
});

test("the autopilot panel shows forks, orphans with the way out, and the journal as text", () => {
  assert.equal(Brain.autopilotRows(null), "");
  assert.equal(Brain.autopilotRows({ open: [], kept_forks: [], orphans: [], journal: [] }), "");
  const html = Brain.autopilotRows({
    open: [{ fork: "autopilot/main/1", parent: "main", action: "npx prisma migrate dev", rule: "migration", client: "Claude Code" }],
    kept_forks: ["autopilot/main/2"],
    orphans: [{ branch: "feature/<x>", why: "git branch deleted, or squash-merged: memory was not merged", merge_then_discard: ["memfork merge feature/<x> --branch main", "memfork discard feature/<x> --lesson \"<what it taught>\""], discard: "memfork discard feature/<x> --lesson \"<what it taught>\"" }],
    journal: [{ kind: "follow", branch: "feature/x", detail: "memory forked `feature/x` from `main` with git" }, { kind: "fork", branch: "autopilot/main/1", detail: "<b>bold</b>" }],
  });
  assert.ok(html.includes("fork open: autopilot/main/1"));
  assert.ok(html.includes("rule: migration"));
  assert.ok(html.includes("fork kept: autopilot/main/2"));
  assert.ok(html.includes("squash-merged"));
  assert.ok(html.includes("memfork merge feature/&lt;x&gt; --branch main, then memfork discard"));
  assert.ok(!html.includes("<b>bold</b>"), "a journal detail rendered as markup");
  assert.ok(html.includes("&lt;b&gt;bold&lt;/b&gt;"));
  // Newest journal entry first.
  assert.ok(html.indexOf("fork · autopilot/main/1") < html.indexOf("follow · feature/x"));
});


// ---- the whole page, booted against a recorded store --------------------------
// `boot` is the half of the script that wires `Brain` to the page. Here it
// runs against a summary and a graph recorded from a real store
// (`fixtures/`, kept honest by the Rust test that recorded them), on a
// document and a window just wide enough for it: elements by id, a canvas
// that records what is drawn on it, a fetch that answers from the files.
// What the Brain showed a person as "0 nodes" and "f is not defined" was a
// throw in this path, which the tests above never took.

function fakeCanvasContext() {
  const calls = [];
  const gradient = { addColorStop() {} };
  const target = { calls };
  return new Proxy(target, {
    get(t, prop) {
      if (prop in t) return t[prop];
      return (...args) => {
        calls.push({ method: String(prop), args });
        return prop === "createRadialGradient" ? gradient : undefined;
      };
    },
    set(t, prop, value) { t[prop] = value; return true; },
  });
}

function fakeElement(id, tag) {
  const attrs = new Map();
  const classes = new Set();
  const el = {
    id, tagName: (tag || "div").toUpperCase(),
    textContent: "", innerHTML: "", hidden: false, disabled: false, value: "", selected: false,
    min: "0", max: "0", width: 0, height: 0, dataset: {}, children: [], listeners: {},
    style: { setProperty() {}, cursor: "" },
    classList: {
      add: (c) => classes.add(c), remove: (c) => classes.delete(c),
      contains: (c) => classes.has(c),
      toggle: (c, on) => { if (on === undefined ? !classes.has(c) : on) classes.add(c); else classes.delete(c); },
    },
    get className() { return [...classes].join(" "); },
    set className(v) { classes.clear(); String(v).split(/\s+/).filter(Boolean).forEach((c) => classes.add(c)); },
    get options() { return el.children; },
    addEventListener(type, fn) { (el.listeners[type] ||= []).push(fn); },
    setAttribute(k, v) { attrs.set(k, String(v)); },
    getAttribute(k) { return attrs.has(k) ? attrs.get(k) : null; },
    replaceChildren(...nodes) {
      el.children = nodes;
      const chosen = nodes.find((n) => n.selected) || nodes[0];
      el.value = chosen ? chosen.value : "";
    },
    appendChild(n) { el.children.push(n); return n; },
    remove() {}, click() {}, focus() {},
    getBoundingClientRect() { return { width: 1200, height: 600, left: 0, top: 0 }; },
    getContext() { return (el.context ||= fakeCanvasContext()); },
    querySelector() { return null; },
    querySelectorAll() { return []; },
    closest() { return null; },
  };
  return el;
}

function fakePage(fixtures, options) {
  const ids = ["stopped", "exported", "notoken", "q", "ns", "branch", "dot", "state", "port", "head", "kpi", "lens", "perf",
    "stage", "under", "over", "tip", "narrow", "scrub", "past", "hand", "coord", "facts", "less", "briefs", "auto", "attn",
    "compare", "export", "theme", "foot", "foot-store", "foot-policy", "foot-build", "sheet", "sx", "sc"];
  const byId = new Map(ids.map((id) => [id, fakeElement(id, id === "ns" || id === "branch" ? "select" : id === "under" || id === "over" ? "canvas" : "div")]));
  const panels = fakeElement("panels");
  const documentElement = fakeElement("html");
  const document = {
    title: "", documentElement, body: fakeElement("body"), activeElement: null,
    getElementById: (id) => byId.get(id) || null,
    createElement: (tag) => fakeElement("", tag),
    querySelector: (sel) => (sel === ".panels" ? panels : null),
    querySelectorAll: () => [],
    addEventListener() {},
  };
  const token = "ab".repeat(32);
  const calls = [];
  let streaming;
  const streamOpened = new Promise((resolve) => { streaming = resolve; });
  const ok = (body) => ({ status: 200, ok: true, json: async () => body });
  const fetch = async (path, init) => {
    calls.push({ path, auth: init && init.headers && init.headers.authorization });
    const name = String(path).split("?")[0];
    if (name === "/brain/summary") return ok(fixtures.summary);
    if (name === "/brain/graph") return ok(fixtures.graph);
    if (name === "/brain/attention") return ok({ attention: [] });
    if (name === "/events") {
      streaming();
      // A stream that stays open: the page is connected for the whole test.
      return { status: 200, ok: true, body: { getReader: () => ({ read: () => new Promise(() => {}) }) } };
    }
    return { status: 404, ok: false, json: async () => ({ error: `no route ${path}` }) };
  };
  let clock = 1000;
  const frames = [];
  const errors = [];
  const window = {
    fetch, TextDecoder,
    performance: { now: () => clock },
    matchMedia: () => ({ matches: false, addEventListener() {} }),
    requestAnimationFrame: (fn) => { frames.push(fn); return frames.length; },
    addEventListener() {},
    setTimeout: (fn, ms) => setTimeout(fn, ms), clearTimeout: (t) => clearTimeout(t),
    getComputedStyle: () => ({ getPropertyValue: () => "" }),
    devicePixelRatio: 1,
    location: { hash: `#t=${token}`, port: String(fixtures.summary.port), search: "", href: `http://127.0.0.1:${fixtures.summary.port}/brain/#t=${token}` },
    ...(options || {}),
  };
  // One animation frame: every callback queued so far, errors kept.
  const frame = (advance) => {
    clock += advance || 16;
    for (const fn of frames.splice(0)) {
      try { fn(); } catch (e) { errors.push(e); }
    }
  };
  return { document, window, byId, calls, token, streamOpened, frame, errors };
}

const fixtures = () => {
  const dir = join(here, "fixtures");
  return {
    summary: JSON.parse(readFileSync(join(dir, "summary.json"), "utf8")),
    graph: JSON.parse(readFileSync(join(dir, "graph.json"), "utf8")),
  };
};

test("the page boots against a store recorded from the daemon: the graph is drawn, the panels fill, nothing throws", async () => {
  const fx = fixtures();
  const page = fakePage(fx);
  const $ = (id) => page.byId.get(id);
  Brain.boot(page.document, page.window);
  await page.streamOpened;

  // The session connected, on this daemon, with the token as a header.
  assert.equal($("state").textContent, "connected");
  assert.ok(page.calls.every((c) => c.auth === `Bearer ${page.token}`), JSON.stringify(page.calls));
  assert.ok(page.calls.some((c) => c.path.startsWith("/brain/summary")));
  assert.ok(page.calls.some((c) => c.path.startsWith("/brain/graph")));

  // The footer is the store's numbers, not an error message.
  const foot = $("foot-store").textContent;
  assert.match(foot, /^store \d+ B · \d+ commits retained · history from seq \d+$/, foot);
  assert.ok(!/not defined|undefined|error/i.test(foot), foot);
  assert.equal($("foot-policy").textContent, `policy: ${fx.summary.policy === "none" ? "none in force" : fx.summary.policy}`);
  // The page names its build, twelve hex digits from the daemon, so a stale
  // binary or a daemon started before a rebuild can be told from the footer.
  assert.match(fx.summary.page_build, /^[0-9a-f]{12}$/, fx.summary.page_build);
  assert.equal($("foot-build").textContent, `page build ${fx.summary.page_build}`);
  assert.equal($("port").textContent, String(fx.summary.port));

  // The header's selectors name the project and the branch, and list the others.
  for (const [id, list, chosen] of [["ns", fx.summary.namespaces, fx.summary.namespace], ["branch", fx.summary.branches, fx.summary.branch]]) {
    const sel = $(id);
    assert.deepEqual(sel.options.map((o) => o.textContent), list, `${id} options`);
    assert.deepEqual(sel.options.map((o) => o.value), list, `${id} values`);
    assert.equal(sel.value, chosen, `${id} chosen`);
    assert.equal(sel.options.filter((o) => o.selected).length, 1, `${id} selected`);
    assert.equal(sel.options.find((o) => o.selected).textContent, chosen);
    assert.equal(sel.disabled, false);
  }

  // The panels hold what the store holds.
  const rows = (id) => ($(id).innerHTML.match(/class="row/g) || []).length;
  assert.equal(rows("hand"), fx.summary.handoffs.length);
  assert.equal(rows("coord"), fx.summary.tasks.length);
  assert.equal(rows("facts"), fx.summary.facts.length);
  assert.equal(rows("less"), fx.summary.lessons.length);
  assert.equal(rows("briefs"), fx.summary.briefings.length);
  const c = fx.summary.headline.counts;
  assert.ok($("kpi").innerHTML.includes(`<b>${c.handoffs_picked_up}</b> handoffs picked up`), $("kpi").innerHTML);
  assert.ok($("kpi").innerHTML.includes(`<b>${c.briefings}</b> briefings served`));
  assert.equal($("head").textContent, Brain.headline(fx.summary.headline));

  // The graph is drawn: one disc per node on the static layer, labelled.
  page.frame();
  const under = $("under").getContext("2d").calls;
  const discs = under.filter((c) => c.method === "arc").length;
  assert.equal(discs, fx.graph.nodes.length, `discs drawn: ${discs}`);
  const labels = under.filter((c) => c.method === "fillText").map((c) => c.args[0]);
  assert.ok(labels.includes("payments"), labels.join(" | "));
  assert.ok(labels.some((l) => l.startsWith("handoff ")), labels.join(" | "));
  // The counter beside the lenses, after the second it is updated on.
  page.frame(1001);
  assert.match($("perf").textContent, new RegExp(`· ${fx.graph.nodes.length} nodes ·`), $("perf").textContent);
  assert.equal($("scrub").max, fx.graph.seq);
  assert.deepEqual(page.errors, [], "a frame threw");
});

test("a page whose panels cannot render still says what went wrong, and stays connected", async () => {
  // A summary missing a whole panel: the failure is written where the
  // footer goes, in words, rather than left as an empty graph.
  const fx = fixtures();
  delete fx.summary.handoffs;
  const page = fakePage(fx);
  Brain.boot(page.document, page.window);
  await page.streamOpened;
  const foot = page.byId.get("foot-store").textContent;
  assert.ok(foot.length > 0 && !foot.startsWith("store "), foot);
  assert.equal(page.byId.get("state").textContent, "connected");
});
