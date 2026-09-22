// The page's logic, without a browser: run with `node --test` from the
// repository root, on the Node that GitHub's runners ship. No packages.
//
// What a browser alone can show — pixels, the policy enforced, fetch
// streaming, the narrow layout, perceived frame rate — is in the manual
// script in the README. What is here is everything the script decides.
import { test } from "node:test";
import assert from "node:assert/strict";
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
