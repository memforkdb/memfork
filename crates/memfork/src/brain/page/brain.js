// The Brain's script. Plain JavaScript, no library, nothing loaded from
// anywhere but this daemon. Split in two: `Brain` holds everything that can
// run without a browser, which is how it is tested under Node; `boot` at the
// end wires it to the page.
//
// Memory contents are untrusted text. Every value that reaches the markup
// goes through `esc` or `textContent`; nothing stored is ever markup.
//
// Nothing here changes memory. Every request is a GET with the read token,
// and the daemon refuses that token on every route that writes.
"use strict";

const Brain = (() => {
  // ---- text ----------------------------------------------------------------
  const esc = (s) =>
    String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
  const tokens = (bytes) => Math.ceil((bytes || 0) / 4);
  const plural = (n, one, many) => `${n} ${n === 1 ? one : many || one + "s"}`;
  const short = (key, ns) => (ns && key.startsWith(ns + ":") ? key.slice(ns.length + 1) : key);
  const family = (key) => {
    const parts = String(key).split(":");
    return parts.length >= 3 ? parts[1] : "entry";
  };

  // ---- the token, from the fragment and nowhere else ------------------------
  // `#t=<read token>`. Kept in a closure and in the address bar so a reload
  // works; never written to a cookie, storage or a query string.
  function readToken(hash) {
    const m = /(?:^#|[#&])t=([0-9a-f]{16,128})(?:&|$)/.exec(hash || "");
    return m ? m[1] : null;
  }

  // ---- the session: connecting, connected, stopped --------------------------
  // The one signal that the daemon is gone is the event stream ending. One
  // probe on the same address tells a hiccup from a stop; a refusal means a
  // different daemon has the port and this page's token is dead too. After
  // that nothing is requested again: not this port, and never another one.
  function makeSession(io) {
    const s = {
      state: "connecting", // connecting | connected | stopped | no-token
      why: "",
      token: null,
      port: null,
      version: null,
      listeners: [],
      on(fn) { s.listeners.push(fn); },
      set(state, why) {
        if (s.state === "stopped") return;
        s.state = state;
        s.why = why || "";
        s.listeners.forEach((fn) => fn(state));
      },
      path(name, params) {
        const q = Object.entries(params || {})
          .filter(([, v]) => v !== undefined && v !== null && v !== "")
          .map(([k, v]) => `${encodeURIComponent(k)}=${encodeURIComponent(v)}`)
          .join("&");
        return `/brain/${name}${q ? "?" + q : ""}`;
      },
      async get(name, params) {
        if (s.state === "stopped") throw new Error("stopped");
        const r = await io.fetch(s.path(name, params), {
          headers: { authorization: `Bearer ${s.token}` },
          cache: "no-store",
        });
        if (r.status === 401) {
          s.set("stopped", "the token was refused: a different daemon answers here now");
          throw new Error("stopped");
        }
        const body = await r.json();
        if (!r.ok) throw new Error(body && body.error ? body.error : `HTTP ${r.status}`);
        return body;
      },
      // One probe, then a verdict. Never a retry loop.
      async probe() {
        try {
          const r = await io.fetch("/brain/summary", {
            headers: { authorization: `Bearer ${s.token}` },
            cache: "no-store",
          });
          if (r.status === 401) return s.set("stopped", "the token was refused");
          if (!r.ok) return s.set("stopped", `HTTP ${r.status}`);
          return true;
        } catch (e) {
          return s.set("stopped", "the daemon did not answer");
        }
      },
    };
    return s;
  }

  // Reads the newline-delimited event stream with fetch, so the token travels
  // as a header. Calls `onLine` per line and `onEnd` once, when the stream
  // ends for any reason.
  async function streamEvents(io, token, onLine, onEnd) {
    let r;
    try {
      r = await io.fetch("/events", { headers: { authorization: `Bearer ${token}` }, cache: "no-store" });
    } catch (e) {
      return onEnd("unreachable");
    }
    if (r.status === 401) return onEnd("refused");
    if (!r.ok || !r.body) return onEnd(`HTTP ${r.status}`);
    const reader = r.body.getReader();
    const decoder = new io.TextDecoder();
    let pending = "";
    try {
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        pending += decoder.decode(value, { stream: true });
        let at;
        while ((at = pending.indexOf("\n")) >= 0) {
          const line = pending.slice(0, at);
          pending = pending.slice(at + 1);
          if (!line.trim()) continue;
          let parsed = null;
          try { parsed = JSON.parse(line); } catch (e) { continue; }
          onLine(parsed);
        }
      }
    } catch (e) {
      // The connection broke: the same as ending.
    }
    onEnd("ended");
  }

  // ---- the graph -----------------------------------------------------------
  // Columns left to right, as fractions of the width, in the daemon's order.
  const COLX = [0.06, 0.22, 0.38, 0.54, 0.7, 0.86, 0.97];
  const COLUMN = { agent: 0, brief: 1, decision: 2, handoff: 2, lesson: 3, task: 4, fact: 5, entry: 5, note: 5, file: 6 };
  const COLOR = {
    decision: "#3ddc97", fact: "#9eb3c2", task: "#7fd1e8", handoff: "#3ddc97", lesson: "#f2b866",
    file: "#1c7293", agent: "#e6edf3", brief: "#b39dff", entry: "#9eb3c2", note: "#9eb3c2",
  };
  const AMBER = "#f2b866", MINT = "#3ddc97", BLUE = "#7fd1e8", VIOLET = "#b39dff", GREY = "#9eb3c2";
  // The same hash the daemon uses, so a node added from an event lands
  // where the next graph will put it.
  const FNV_OFFSET = 0xcbf29ce484222325n, FNV_PRIME = 0x100000001b3n, MASK64 = 0xffffffffffffffffn;
  function hashY(id) {
    let h = FNV_OFFSET;
    const bytes = unescape(encodeURIComponent(id));
    for (let i = 0; i < bytes.length; i++) {
      h ^= BigInt(bytes.charCodeAt(i));
      h = (h * FNV_PRIME) & MASK64;
    }
    return Number(h >> 48n);
  }

  function makeGraph() {
    return { nodes: [], byId: new Map(), edges: [], few: 12, height: 65535, seq: 0, at: null, counts: {}, loaded: false };
  }

  // Every node of a column, evenly spaced in id order, when the column holds
  // few enough: the daemon's rule, mirrored for nodes added between graphs.
  function spaceColumn(g, col) {
    const members = g.nodes.filter((n) => n.col === col).sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
    const n = members.length;
    members.forEach((node, rank) => {
      node.y = n <= g.few ? Math.floor(((2 * rank + 1) * g.height) / (2 * n)) : hashY(node.id);
    });
  }

  // Replace the graph with what the daemon sent. Nodes that were already
  // there keep their birth time, so nothing re-animates; new ones are born
  // now, unless this is the first graph, which simply appears.
  function loadGraph(g, json, now) {
    const first = !g.loaded;
    const old = g.byId;
    g.nodes = json.nodes.map((n) => {
      const was = old.get(n[0]);
      return {
        id: n[0], kind: n[1], label: n[2], col: n[3], y: n[4], seq: n[5], state: n[6], who: n[7],
        born: was ? was.born : first ? 0 : now,
        glow: was ? was.glow : 0,
        fresh: !was && !first,
      };
    });
    g.byId = new Map(g.nodes.map((n) => [n.id, n]));
    g.edges = json.edges.map((e) => ({ a: g.nodes[e[0]].id, b: g.nodes[e[1]].id, kind: e[2] }));
    g.few = json.few || 12;
    g.height = json.height || 65535;
    g.seq = json.seq;
    g.at = json.at == null ? null : json.at;
    g.counts = json.counts || {};
    g.loaded = true;
    return g.nodes.filter((n) => n.fresh);
  }

  // A node the page adds from an event, before the next graph confirms it.
  function addLocal(g, id, kind, label, who, seq, now) {
    if (g.byId.has(id)) return g.byId.get(id);
    const node = { id, kind, label, col: COLUMN[kind] ?? 5, y: 0, seq, state: null, who, born: now, glow: 0, fresh: true, local: true };
    g.nodes.push(node);
    g.byId.set(id, node);
    spaceColumn(g, node.col);
    return node;
  }

  function addEdge(g, a, b, kind) {
    if (g.edges.some((e) => e.a === a && e.b === b && e.kind === kind)) return;
    g.edges.push({ a, b, kind });
  }

  function removeNode(g, id) {
    if (!g.byId.has(id)) return;
    const node = g.byId.get(id);
    g.nodes = g.nodes.filter((n) => n.id !== id);
    g.byId.delete(id);
    g.edges = g.edges.filter((e) => e.a !== id && e.b !== id);
    spaceColumn(g, node.col);
  }

  // Which nodes a lens shows. `scrub` hides what was written after a point
  // in time; `narrow` is a small screen, where only the lens is drawn.
  function visible(g, node, lens, scrub) {
    if (scrub != null && node.seq > scrub) return false;
    if (lens === "all") return true;
    const k = node.kind;
    const linked = (kinds) => g.edges.some((e) => (e.a === node.id && kinds.includes(g.byId.get(e.b)?.kind)) || (e.b === node.id && kinds.includes(g.byId.get(e.a)?.kind)));
    if (lens === "handoff") return k === "handoff" || k === "agent" || k === "brief" || linked(["handoff"]);
    if (lens === "plan") return k === "task" || k === "agent";
    if (lens === "fact") return k === "fact" || k === "file";
    if (lens === "lesson") return k === "lesson" || k === "agent" || linked(["lesson"]);
    if (lens === "brief") return k === "brief" || k === "agent" || linked(["brief"]);
    if (lens === "race") return false;
    return true;
  }

  // ---- events, applied to the graph -----------------------------------------
  // Every piece of motion comes from here, and each is one real event. The
  // result lists what to animate; the caller draws it. Returns `null` for an
  // event that is not about the graph being shown.
  function applyEvent(g, ev, ctx, now) {
    const fx = { pulses: [], glows: [], refetch: false, panels: false };
    const agentId = ev.client ? `agent:${ev.client}` : null;
    const inScope = (!ev.namespace || ev.namespace === ctx.ns) && (!ev.branch || ev.branch === ctx.branch || ev.kind !== "operation");
    if (ev.kind === "hello") {
      for (const c of ev.clients || []) {
        const a = addLocal(g, `agent:${c.client}`, "agent", c.client, null, 0, now);
        a.state = "connected";
      }
      return fx;
    }
    if (ev.kind === "connected" && agentId) {
      const a = addLocal(g, agentId, "agent", ev.client, null, 0, now);
      a.state = "connected";
      fx.glows.push(agentId);
      fx.panels = true;
      return fx;
    }
    if (ev.kind === "disconnected" && agentId) {
      const a = g.byId.get(agentId);
      if (a) a.state = "away";
      fx.panels = true;
      return fx;
    }
    if (ev.kind !== "operation" || !inScope) return null;
    const op = ev.operation, key = ev.key;
    const ensure = (kind, label) => addLocal(g, key, kind, label || short(key, ctx.ns), ev.client, g.seq + 1, now);
    const pulse = (a, b, color, reverse) => { if (a && b) fx.pulses.push({ a, b, color, reverse: !!reverse }); };
    if (agentId && !g.byId.has(agentId) && ev.client !== "memfork-cli") addLocal(g, agentId, "agent", ev.client, null, 0, now);
    if (agentId && ev.client === "memfork-cli" && !g.byId.has(agentId)) addLocal(g, agentId, "agent", ev.client, null, 0, now);
    switch (op) {
      case "put": {
        if (!ev.ok || !key) return fx;
        const kind = family(key);
        const node = ensure(kind in COLUMN ? kind : "entry");
        if (agentId) addEdge(g, agentId, key, "wrote");
        pulse(agentId, key, COLOR[node.kind] || GREY);
        fx.glows.push(key);
        fx.refetch = fx.panels = true;
        return fx;
      }
      case "handoff": {
        if (!ev.ok || !key) return fx;
        ensure("handoff", `handoff ${key.split(":").pop().replace(/^0+/, "")}`);
        if (agentId) addEdge(g, agentId, key, "wrote");
        pulse(agentId, key, MINT);
        fx.glows.push(key);
        fx.refetch = fx.panels = true;
        return fx;
      }
      case "lesson": {
        if (!key) return fx;
        ensure("lesson");
        if (agentId) addEdge(g, agentId, key, "wrote");
        pulse(agentId, key, AMBER);
        fx.glows.push(key);
        fx.refetch = fx.panels = true;
        return fx;
      }
      case "add": case "plan": {
        if (key) ensure("task");
        fx.refetch = fx.panels = true;
        return fx;
      }
      case "claim": {
        if (!key) return fx;
        const task = ensure("task");
        if (ev.ok) {
          task.state = "claimed";
          if (agentId) addEdge(g, agentId, key, "holds");
          pulse(agentId, key, BLUE);
        } else {
          // Refused: the conflict avoided. The pulse comes back.
          pulse(agentId, key, AMBER, true);
        }
        fx.glows.push(key);
        fx.panels = true;
        return fx;
      }
      case "release": {
        if (!key) return fx;
        const task = ensure("task");
        task.state = "open";
        g.edges = g.edges.filter((e) => !(e.kind === "holds" && e.b === key));
        pulse(agentId, key, BLUE, true);
        fx.panels = true;
        return fx;
      }
      case "done": {
        if (!key) return fx;
        const task = ensure("task");
        if (ev.ok) {
          task.state = "done";
          g.edges = g.edges.filter((e) => !(e.kind === "holds" && e.b === key));
        } else {
          task.state = "open";
        }
        fx.glows.push(key);
        fx.refetch = fx.panels = true;
        return fx;
      }
      case "ready": {
        if (key) { ensure("task"); fx.glows.push(key); }
        fx.panels = true;
        return fx;
      }
      case "fact": {
        if (!key) return fx;
        const node = ensure("fact");
        if (ev.detail === "stale") {
          node.state = "stale";
          // Backwards from every source file to the fact, then on to what
          // cited it.
          for (const e of g.edges) {
            if (e.b === key && e.kind === "source of") pulse(e.a, key, AMBER);
          }
          for (const e of g.edges) {
            if (e.a === key && e.kind === "cited by") pulse(key, e.b, AMBER);
          }
          fx.glows.push(key);
        } else if (ev.detail === "fresh") {
          node.state = null;
        }
        fx.panels = true;
        return fx;
      }
      case "flag": case "maintain": {
        if (key && g.byId.has(key)) fx.glows.push(key);
        fx.panels = true;
        return fx;
      }
      case "resume": {
        // The briefing itself arrives with the next graph, where its edges
        // are known; the pulses are drawn then (see `announce`).
        fx.refetch = fx.panels = true;
        return fx;
      }
      case "delete": {
        if (ev.ok && key) removeNode(g, key);
        fx.refetch = fx.panels = true;
        return fx;
      }
      case "fork": case "merge": case "discard": case "checkout": {
        fx.refetch = fx.panels = true;
        return fx;
      }
      default:
        return fx;
    }
  }

  // Pulses for nodes that arrived with a graph rather than an event: a
  // briefing draws from what it gathered and flows to the agent it served.
  function announce(g, freshNodes) {
    const pulses = [];
    for (const n of freshNodes) {
      if (n.kind !== "brief") continue;
      for (const e of g.edges) {
        if (e.b === n.id && e.kind === "in") pulses.push({ a: e.a, b: n.id, color: COLOR[g.byId.get(e.a)?.kind] || GREY, reverse: false });
        if (e.a === n.id && e.kind === "served to") pulses.push({ a: n.id, b: e.b, color: VIOLET, reverse: false, delay: 900 });
      }
    }
    return pulses;
  }

  function headline(counts) {
    const n = (counts && counts.twice) || 0;
    return `${n} ${n === 1 ? "thing" : "things"} your agents did not have to learn twice`;
  }

  // The nearest visible node in a direction, for the keyboard.
  function nearest(nodes, from, dir, pos) {
    let best = null, bestScore = Infinity;
    for (const n of nodes) {
      if (n === from) continue;
      const p = pos(n), f = pos(from);
      const dx = p.x - f.x, dy = p.y - f.y;
      const along = dir === "right" ? dx : dir === "left" ? -dx : dir === "down" ? dy : -dy;
      const across = dir === "right" || dir === "left" ? Math.abs(dy) : Math.abs(dx);
      if (along <= 0.5) continue;
      const score = along + across * 2.5;
      if (score < bestScore) { bestScore = score; best = n; }
    }
    return best;
  }

  return {
    esc, tokens, plural, short, family, readToken, makeSession, streamEvents,
    COLX, COLOR, COLUMN, hashY, makeGraph, loadGraph, spaceColumn, addLocal, addEdge, removeNode,
    visible, applyEvent, announce, headline, nearest,
  };
})();

if (typeof module !== "undefined" && module.exports) module.exports = Brain;

// ---- the page ---------------------------------------------------------------
function boot(document, window) {
  const $ = (id) => document.getElementById(id);
  const io = { fetch: window.fetch.bind(window), TextDecoder: window.TextDecoder };
  const session = Brain.makeSession(io);
  const t0 = window.performance.now();
  const now = () => window.performance.now();
  const reduced = window.matchMedia && window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  const narrowQuery = window.matchMedia ? window.matchMedia("(max-width: 720px)") : { matches: false, addEventListener() {} };

  const g = Brain.makeGraph();
  const view = { ns: null, branch: null, lens: "all", scrub: null, hover: null, focus: null, hits: new Set(), summary: null, firstPaint: 0 };
  const motion = { pulses: [] };
  let dirty = true;
  let stopped = false;

  // ---- status -------------------------------------------------------------
  function showState(state) {
    const dot = $("dot");
    dot.className = "dot" + (state === "connected" ? "" : state === "stopped" || state === "no-token" ? " off" : " wait");
    $("state").textContent = state === "no-token" ? "no token" : state;
    $("stopped").hidden = state !== "stopped";
    $("notoken").hidden = state !== "no-token";
    if (state === "stopped") {
      stopped = true;
      document.title = "MemFork · The Brain · stopped";
      for (const id of ["q", "scrub", "ns", "branch", "compare", "export"]) $(id).disabled = true;
      dirty = true;
    }
  }
  session.on(showState);

  // ---- canvas -------------------------------------------------------------
  const under = $("under"), over = $("over");
  const cu = under.getContext("2d"), co = over.getContext("2d");
  let W = 0, H = 0, grid = new Map(), CELL = 24;
  function fit() {
    const r = $("stage").getBoundingClientRect();
    W = r.width; H = r.height;
    const dpr = window.devicePixelRatio || 1;
    for (const [c, cx] of [[under, cu], [over, co]]) {
      c.width = Math.max(1, Math.round(W * dpr)); c.height = Math.max(1, Math.round(H * dpr));
      cx.setTransform(dpr, 0, 0, dpr, 0, 0);
    }
    dirty = true;
  }
  window.addEventListener("resize", fit);
  fit();
  const top = () => H * 0.11, span = () => H * 0.78;
  const pos = (n) => ({ x: W * Brain.COLX[n.col], y: top() + (span() * n.y) / g.height });
  const isVisible = (n) => Brain.visible(g, n, narrowQuery.matches && view.lens === "all" ? "all" : view.lens, view.scrub);
  const radius = (n) => (n.kind === "agent" ? 7 : n.kind === "file" ? 2.5 : 4.5);
  const colorOf = (n) => (n.state === "stale" ? "#f2b866" : n.state === "done" && n.kind === "task" ? "#3ddc97" : Brain.COLOR[n.kind] || "#9eb3c2");
  const LABELS_MAX = 40;

  function curve(cx, a, b, t) {
    const mx = (a.x + b.x) / 2;
    cx.moveTo(a.x, a.y);
    if (t == null) cx.bezierCurveTo(mx, a.y, mx, b.y, b.x, b.y);
    else {
      const u = 1 - t;
      const x = u * u * u * a.x + 3 * u * u * t * mx + 3 * u * t * t * mx + t * t * t * b.x;
      const y = u * u * u * a.y + 3 * u * u * t * a.y + 3 * u * t * t * b.y + t * t * t * b.y;
      cx.lineTo(x, y);
    }
  }
  function along(a, b, t) {
    const mx = (a.x + b.x) / 2, u = 1 - t;
    return {
      x: u * u * u * a.x + 3 * u * u * t * mx + 3 * u * t * t * mx + t * t * t * b.x,
      y: u * u * u * a.y + 3 * u * u * t * a.y + 3 * u * t * t * b.y + t * t * t * b.y,
    };
  }

  // The static layer: everything that stays put. Redrawn only when the graph,
  // the lens, the time or the size changes.
  function drawStatic() {
    cu.clearRect(0, 0, W, H);
    const shown = g.nodes.filter(isVisible);
    const shownIds = new Set(shown.map((n) => n.id));
    const perColumn = new Map();
    for (const n of shown) perColumn.set(n.col, (perColumn.get(n.col) || 0) + 1);
    const dimColor = getComputedStyle(document.documentElement).getPropertyValue("--dim").trim() || "#1c7293";
    const subColor = getComputedStyle(document.documentElement).getPropertyValue("--sub").trim() || "#9eb3c2";
    const txtColor = getComputedStyle(document.documentElement).getPropertyValue("--txt").trim() || "#e6edf3";
    cu.font = "11px system-ui, sans-serif";
    cu.fillStyle = dimColor;
    cu.textAlign = "center";
    const columns = ["agents", "briefings", "decisions · handoffs", "lessons", "plan", "facts", "files"];
    columns.forEach((l, i) => cu.fillText(l, W * Brain.COLX[i], H * 0.06));

    // Edges of visible nodes.
    cu.lineWidth = 1.1;
    for (const e of g.edges) {
      if (!shownIds.has(e.a) || !shownIds.has(e.b)) continue;
      const a = g.byId.get(e.a), b = g.byId.get(e.b);
      const stale = e.kind === "source of" && b.state === "stale";
      cu.strokeStyle = stale ? "#f2b866" : dimColor;
      cu.globalAlpha = stale ? 0.85 : e.kind === "wrote" ? 0.25 : 0.55;
      cu.beginPath();
      curve(cu, pos(a), pos(b), null);
      cu.stroke();
    }
    cu.globalAlpha = 1;

    // Nodes, and their labels where there is room.
    grid = new Map();
    const many = shown.length > 3000;
    for (const n of shown) {
      const p = pos(n), r = radius(n);
      const key = `${Math.floor(p.x / CELL)},${Math.floor(p.y / CELL)}`;
      (grid.get(key) || grid.set(key, []).get(key)).push(n);
      cu.fillStyle = colorOf(n);
      cu.globalAlpha = n.kind === "file" ? 0.6 : n.kind === "agent" && n.state === "away" ? 0.5 : 1;
      if (many) cu.fillRect(p.x - r / 2, p.y - r / 2, r, r);
      else { cu.beginPath(); cu.arc(p.x, p.y, r, 0, 7); cu.fill(); }
      cu.globalAlpha = 1;
      const labelled = (perColumn.get(n.col) || 0) <= LABELS_MAX || view.hits.has(n.id) || view.focus === n || view.hover === n;
      if (!labelled) continue;
      const big = n.kind === "agent";
      cu.fillStyle = big ? txtColor : n.state === "stale" ? "#f2b866" : subColor;
      cu.globalAlpha = big ? 1 : 0.85;
      cu.font = (big ? "600 13px" : "11.5px") + " system-ui, sans-serif";
      cu.textAlign = n.kind === "file" ? "right" : "center";
      const dense = (perColumn.get(n.col) || 0) > 6;
      const text = n.label.length > 34 && dense ? n.label.slice(0, 33) + "…" : n.label;
      const extra = n.kind === "agent" && n.state === "away" ? " (away)" : "";
      cu.fillText(text + extra, n.kind === "file" ? p.x - 8 : p.x, n.kind === "file" ? p.y + 4 : p.y - 11);
      cu.globalAlpha = 1;
    }
    dirty = false;
  }

  // The live layer: what just happened. Pulses, glows, the hovered and the
  // focused node, search hits, and nodes fading in.
  let frames = 0, fpsAt = now(), fps = 60;
  function drawLive() {
    const t = now();
    frames++;
    if (t - fpsAt > 1000) {
      fps = frames; frames = 0; fpsAt = t;
      $("perf").textContent = `first paint ${Math.round(view.firstPaint)} ms · ${Brain.plural(g.nodes.length, "node")} · ${fps} fps`;
    }
    if (dirty) drawStatic();
    co.clearRect(0, 0, W, H);
    if (!reduced && !stopped) {
      motion.pulses = motion.pulses.filter((p) => p.t < 1);
      for (const p of motion.pulses) {
        if (p.delay > 0) { p.delay -= 16; continue; }
        const a = g.byId.get(p.reverse ? p.b : p.a), b = g.byId.get(p.reverse ? p.a : p.b);
        if (!a || !b || !isVisible(a) || !isVisible(b)) { p.t = 1; continue; }
        p.t += 0.022;
        const k = p.t * p.t * (3 - 2 * p.t);
        const q = along(pos(a), pos(b), k);
        const grad = co.createRadialGradient(q.x, q.y, 0, q.x, q.y, 14);
        grad.addColorStop(0, p.color); grad.addColorStop(1, "rgba(0,0,0,0)");
        co.fillStyle = grad; co.beginPath(); co.arc(q.x, q.y, 14, 0, 7); co.fill();
        co.fillStyle = p.color; co.beginPath(); co.arc(q.x, q.y, 2.5, 0, 7); co.fill();
      }
    }
    for (const n of g.nodes) {
      if (!isVisible(n)) continue;
      const p = pos(n);
      if (!reduced && n.glow > t) {
        const grad = co.createRadialGradient(p.x, p.y, 0, p.x, p.y, 26);
        grad.addColorStop(0, colorOf(n) + "99"); grad.addColorStop(1, "rgba(0,0,0,0)");
        co.fillStyle = grad; co.beginPath(); co.arc(p.x, p.y, 26, 0, 7); co.fill();
      }
      if (!reduced && n.born && t - n.born < 500) {
        const age = (t - n.born) / 500;
        co.strokeStyle = colorOf(n); co.globalAlpha = 1 - age; co.lineWidth = 1.5;
        co.beginPath(); co.arc(p.x, p.y, radius(n) + 14 * age, 0, 7); co.stroke(); co.globalAlpha = 1;
      }
      if (view.hits.has(n.id)) {
        co.strokeStyle = "#3ddc97"; co.lineWidth = 1.5;
        co.beginPath(); co.arc(p.x, p.y, radius(n) + 5, 0, 7); co.stroke();
      }
      if (view.focus === n) {
        co.strokeStyle = "#e6edf3"; co.lineWidth = 2;
        co.beginPath(); co.arc(p.x, p.y, radius(n) + 7, 0, 7); co.stroke();
      }
    }
    if (view.hover && isVisible(view.hover)) {
      const p = pos(view.hover);
      co.strokeStyle = "#e6edf3"; co.lineWidth = 1;
      co.beginPath(); co.arc(p.x, p.y, radius(view.hover) + 4, 0, 7); co.stroke();
    }
    window.requestAnimationFrame(drawLive);
  }

  function glow(id, ms) {
    const n = g.byId.get(id);
    if (n) n.glow = now() + (ms || 1500);
  }
  function pulse(p) {
    if (reduced) return;
    motion.pulses.push({ t: 0, delay: p.delay || 0, ...p });
  }

  // ---- hit testing, hover, click, keyboard ----------------------------------
  function nodeAt(x, y) {
    const cx = Math.floor(x / CELL), cy = Math.floor(y / CELL);
    let best = null, bestD = 10;
    for (let i = cx - 1; i <= cx + 1; i++) for (let j = cy - 1; j <= cy + 1; j++) {
      for (const n of grid.get(`${i},${j}`) || []) {
        const p = pos(n), d = Math.hypot(p.x - x, p.y - y);
        if (d < bestD) { bestD = d; best = n; }
      }
    }
    return best;
  }
  const tip = $("tip");
  function showTip(n, x, y) {
    if (!n) { tip.classList.remove("on"); return; }
    const state = n.state && n.kind !== "agent" ? ` · ${n.state}` : n.kind === "agent" ? ` · ${n.state || "away"}` : "";
    tip.innerHTML = `<span class="k">${Brain.esc(n.kind)}${n.who ? " · by " + Brain.esc(n.who) : ""}${Brain.esc(state)}</span><b>${Brain.esc(n.label)}</b>${n.kind !== "agent" && n.kind !== "file" ? `<span class="k">${Brain.esc(n.id)}</span>` : ""}`;
    tip.style.left = Math.min(x + 14, W - 330) + "px";
    tip.style.top = y + 14 + "px";
    tip.classList.add("on");
  }
  over.addEventListener("mousemove", (e) => {
    const r = over.getBoundingClientRect();
    const n = nodeAt(e.clientX - r.left, e.clientY - r.top);
    if (n !== view.hover) { view.hover = n; dirty = true; }
    showTip(n, e.clientX - r.left, e.clientY - r.top);
    over.style.cursor = n ? "pointer" : "default";
  });
  over.addEventListener("mouseleave", () => { view.hover = null; showTip(null); dirty = true; });
  over.addEventListener("click", () => { if (view.hover) { view.focus = view.hover; openNode(view.hover); } });
  $("stage").addEventListener("keydown", (e) => {
    const shown = g.nodes.filter(isVisible);
    if (!shown.length) return;
    const dirs = { ArrowLeft: "left", ArrowRight: "right", ArrowUp: "up", ArrowDown: "down" };
    if (dirs[e.key]) {
      e.preventDefault();
      view.focus = view.focus && isVisible(view.focus) ? Brain.nearest(shown, view.focus, dirs[e.key], pos) || view.focus : shown[0];
      const p = pos(view.focus);
      showTip(view.focus, p.x, p.y);
      dirty = true;
    } else if (e.key === "Enter" && view.focus) {
      e.preventDefault();
      openNode(view.focus);
    } else if (e.key === "Escape") {
      closeSheet();
      view.focus = null; showTip(null); dirty = true;
    }
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "/" && !["INPUT", "SELECT", "TEXTAREA"].includes(document.activeElement?.tagName)) {
      e.preventDefault(); $("q").focus();
    } else if (e.key === "Escape") closeSheet();
  });

  // ---- the side sheet -------------------------------------------------------
  const sheet = $("sheet");
  function openSheet(html) {
    $("sc").innerHTML = html;
    sheet.hidden = false;
    window.requestAnimationFrame(() => sheet.classList.add("on"));
    const first = sheet.querySelector("h3, button.link, select");
    if (first) first.focus();
  }
  function closeSheet() { sheet.classList.remove("on"); window.setTimeout(() => { if (!sheet.classList.contains("on")) sheet.hidden = true; }, 280); }
  $("sx").addEventListener("click", closeSheet);
  const connections = (n) => g.edges
    .filter((e) => e.a === n.id || e.b === n.id)
    .map((e) => {
      const other = g.byId.get(e.a === n.id ? e.b : e.a);
      if (!other) return "";
      return `<div class="h"><span class="k">${Brain.esc(other.kind)}</span><button class="link" data-node="${Brain.esc(other.id)}">${Brain.esc(other.label)}</button><span>${Brain.esc(e.kind)}</span></div>`;
    })
    .join("") || '<div class="h">nothing yet</div>';
  async function openNode(n) {
    const head = (extra) => `<div class="k">${Brain.esc(n.kind)}${n.who ? " · written by " + Brain.esc(n.who) : ""}${extra || ""}</div><h3 id="sheet-title" tabindex="-1">${Brain.esc(n.label)}</h3>`;
    if (n.kind === "agent" || n.kind === "file") {
      openSheet(`${head(n.kind === "agent" ? ` · ${Brain.esc(n.state || "away")}` : "")}<h4>Connected to</h4>${connections(n)}`);
      return;
    }
    if (n.kind === "brief") {
      const b = (view.summary?.briefings || []).find((x) => `brief:${String(x.order).padStart(12, "0")}` === n.id);
      const carried = b ? Object.entries(b.carried || {}).map(([k, v]) => `${v} ${k}${v === 1 ? "" : "s"}`).join(", ") : "";
      const omitted = b && b.omitted && Object.keys(b.omitted).length ? `<h4>Cut to fit</h4><div class="h">${Brain.esc(Object.entries(b.omitted).map(([k, v]) => `${v} ${k}`).join(", "))}</div>` : "";
      openSheet(`${head()}<div class="val">${b ? `${b.bytes} B, about ${Brain.tokens(b.bytes)} tokens, to ${Brain.esc(b.to)}${b.since_commits != null ? ` · since last look: ${b.since_commits} commits` : ""}${b.task ? ` · ranked for “${Brain.esc(b.task)}”` : ""}` : ""}</div>${carried ? `<h4>Carried</h4><div class="h">${Brain.esc(carried)}</div>` : ""}${omitted}<h4>Connected to</h4>${connections(n)}`);
      return;
    }
    openSheet(`${head()}<div class="val">loading…</div>`);
    try {
      const e = await session.get("entry", { branch: view.branch, key: n.id, at: view.scrub });
      const sources = e.sources ? `<h4>Sources</h4>${e.sources.map((s) => `<div class="h"><b class="mono">${Brain.esc(s.path)}</b><span class="${s.state === "stale" ? "stale" : s.state === "fresh" ? "fresh" : "dim"}">${Brain.esc(s.state)}</span></div>`).join("")}` : "";
      const history = e.history.length ? e.history.map((h) => `<div class="h"><span class="mono">seq ${h.seq}</span><b>${Brain.esc(h.what)}</b>${h.by ? Brain.esc(h.by) : ""}${h.message ? `<span class="dim">${Brain.esc(h.message)}</span>` : ""}</div>`).join("") : '<div class="h">no history in the retained commits</div>';
      const meta = Object.entries(e.meta || {}).filter(([k]) => k !== "written_by").map(([k, v]) => `<div class="h"><span class="mono">${Brain.esc(k)}</span><b>${Brain.esc(v)}</b></div>`).join("");
      openSheet(`${head(e.by && !n.who ? " · written by " + Brain.esc(e.by) : "")}<div class="k mono">${Brain.esc(e.key)} · ${e.bytes} B · since seq ${e.created_seq}</div><div class="val">${Brain.esc(e.value)}</div>${sources}${meta ? `<h4>Metadata</h4>${meta}` : ""}<h4>History</h4>${history}${e.history_truncated ? '<div class="h dim">older history not shown</div>' : ""}<h4>Connected to</h4>${connections(n)}`);
    } catch (err) {
      openSheet(`${head()}<div class="val">${Brain.esc(String(err.message || err))}</div><h4>Connected to</h4>${connections(n)}`);
    }
  }
  sheet.addEventListener("click", (e) => {
    const b = e.target.closest("button[data-node]");
    if (b) { const n = g.byId.get(b.dataset.node); if (n) { view.focus = n; openNode(n); } }
    const k = e.target.closest("[data-key]");
    if (k) { const n = g.byId.get(k.dataset.key); if (n) { view.focus = n; openNode(n); } }
  });

  // ---- panels -------------------------------------------------------------
  const row = (icon, cls, body, status) => `<div class="row${cls ? " " + cls : ""}"><svg class="icon" aria-hidden="true"><use href="#${icon}"/></svg><div class="body">${body}</div>${status || ""}</div>`;
  const open = (key) => key ? ` open" data-key="${Brain.esc(key)}` : "";
  function renderPanels(s) {
    view.summary = s;
    $("head").textContent = Brain.headline(s.headline);
    const c = s.headline.counts;
    $("kpi").innerHTML = `<span><b>${c.briefings}</b> briefings served</span><span><b>${c.handoffs_picked_up}</b> handoffs picked up</span><span><b>${c.dead_ends_not_repeated}</b> dead ends not repeated</span><span><b>${c.stale_facts_caught}</b> stale facts caught</span><span><b>${c.claim_conflicts_avoided}</b> claim conflicts avoided</span>`;

    $("hand").innerHTML = s.handoffs.length ? s.handoffs.map((h) => {
      const p = h.picked_up;
      const to = p ? Brain.esc(p.to) : '<span class="wait">waiting for pickup</span>';
      const next = p && p.next && p.next.length ? ` · then took “${Brain.esc(p.next[0])}”` : "";
      return `<div class="row enter${open(h.key)}"><svg class="icon" aria-hidden="true"><use href="#i-handoff"/></svg><div class="body"><div class="v">${Brain.esc(h.by || "someone")} <span class="arrow">→</span> ${to}</div><div class="s">${Brain.esc(h.summary)}</div>${p ? `<div class="s">picked up as a ${p.bytes} B briefing (about ${Brain.tokens(p.bytes)} tokens)${next}</div>` : ""}${h.blockers && h.blockers.length ? `<div class="s am">blocked: ${Brain.esc(h.blockers.join("; "))}</div>` : ""}</div></div>`;
    }).join("") : '<span class="empty">No handoff yet.</span>';

    $("coord").innerHTML = s.tasks.length ? s.tasks.map((t) => {
      const st = t.status === "done" ? "done" : t.status === "claimed" ? `claimed by ${Brain.esc(t.held_by)} · lease ${Math.floor((t.seconds_left || 0) / 60)}:${String((t.seconds_left || 0) % 60).padStart(2, "0")}` : t.ready === false && t.blocked_by ? `blocked by ${Brain.esc(t.blocked_by.join(", "))}` : "ready";
      const cls = t.status === "done" ? "fresh" : t.status === "claimed" ? "cl" : t.ready === false && t.blocked_by ? "dim" : "";
      return `<div class="row${open(t.key)}"><svg class="icon" aria-hidden="true"><use href="#${t.status === "done" ? "i-task-done" : "i-task"}"/></svg><div class="body"><div class="v">${Brain.esc(t.title || t.id)}</div>${t.accept ? `<div class="s mono">${Brain.esc(t.accept)}</div>` : ""}</div><span class="st ${cls}"><span class="d"></span>${st}</span></div>`;
    }).join("") : '<span class="empty">No tasks yet.</span>';

    $("facts").innerHTML = s.facts.length ? s.facts.map((f) => `<div class="row${open(f.key)}"><svg class="icon" aria-hidden="true"><use href="#i-fact"/></svg><div class="body"><div class="k mono">${Brain.esc(f.key)}</div><div class="v">${Brain.esc(f.value)}</div><div class="s">${Brain.esc((f.sources || []).join(", "))}${f.state === "stale" ? " changed" : ""}</div></div><span class="st ${f.state === "stale" ? "stale" : f.state === "fresh" ? "fresh" : "dim"}"><span class="d"></span>${Brain.esc(f.state)}</span></div>`).join("") : '<span class="empty">No facts yet.</span>';

    $("less").innerHTML = s.lessons.length ? s.lessons.map((l) => `<div class="row am${open(l.key)}"><svg class="icon" aria-hidden="true"><use href="#i-lesson"/></svg><div class="body"><div class="v">${Brain.esc(l.lesson)}</div><div class="s">${l.branch ? `from discarded ${Brain.esc(l.branch)}` : l.task ? `from task ${Brain.esc(l.task)}` : ""} · served in ${Brain.plural(l.served, "briefing")} since</div></div></div>`).join("") : '<span class="empty">No lessons yet. A discarded fork can leave one.</span>';

    $("briefs").innerHTML = s.briefings.length ? s.briefings.map((b) => {
      const carried = Object.entries(b.carried || {}).map(([k, v]) => `${v} ${k}${v === 1 ? "" : "s"}`).join(", ");
      const cut = b.omitted && Object.keys(b.omitted).length ? ` · cut to fit: ${Object.entries(b.omitted).map(([k, v]) => `${v} ${k}`).join(", ")}` : "";
      const since = b.since_commits != null ? ` · since last look: ${b.since_commits} commits` : "";
      return `<div class="row vi"><svg class="icon" aria-hidden="true"><use href="#i-brief"/></svg><div class="body"><div class="v">to ${Brain.esc(b.to)} · ${b.bytes} B · about ${Brain.tokens(b.bytes)} tokens</div><div class="s">${Brain.esc(carried || "empty project: a hint to start")}${Brain.esc(cut)}${Brain.esc(since)}</div></div></div>`;
    }).join("") : '<span class="empty">None yet.</span>';

    $("attn").innerHTML = s.attention.length ? s.attention.map((a) => `<div class="row am${open(a.keys && a.keys[0])}"><svg class="icon" aria-hidden="true"><use href="#i-attention"/></svg><div class="body"><div class="v">${Brain.esc(a.title)}</div><div class="s">${Brain.esc(a.detail)}</div></div></div>`).join("") : '<span class="empty">Nothing needs attention.</span>';

    const f = s.footer;
    $("foot-store").textContent = `store ${f.store_bytes >= 1e6 ? (f.store_bytes / 1e6).toFixed(1) + " MB" : f.store_bytes >= 1e3 ? Math.round(f.store_bytes / 1e3) + " kB" : f.store_bytes + " B"} · ${f.commits_retained.toLocaleString()} commits retained · history from seq ${f.history_from_seq}`;
    $("foot-policy").textContent = `policy: ${s.policy === "none" ? "none in force" : s.policy}`;
    $("port").textContent = String(s.port);

    const pick = (id, list, chosen) => {
      const sel = $(id);
      if (sel.dataset.list === list.join("\n") && sel.value === chosen) return;
      sel.replaceChildren(...list.map((v) => { const o = document.createElement("option"); o.value = v; o.textContent = v; o.selected = v === chosen; return o; }));
      sel.dataset.list = list.join("\n");
    };
    pick("ns", s.namespaces.length ? s.namespaces : [s.namespace], s.namespace);
    pick("branch", s.branches, s.branch);
  }
  document.querySelector(".panels").addEventListener("click", (e) => {
    const r = e.target.closest(".row.open");
    if (!r) return;
    const n = g.byId.get(r.dataset.key);
    if (n) { view.focus = n; openNode(n); dirty = true; }
  });

  // ---- loading and reconciling ---------------------------------------------
  let loading = null, wantedAgain = false, lastLoad = 0, timer = null;
  async function load(reason) {
    if (stopped) return;
    if (loading) { wantedAgain = true; return; }
    loading = (async () => {
      try {
        const [s, gj] = await Promise.all([
          session.get("summary", { ns: view.ns, branch: view.branch }),
          session.get("graph", { ns: view.ns, branch: view.branch, at: view.scrub }),
        ]);
        if (String(s.port) !== window.location.port) { session.set("stopped", "this page belongs to another daemon"); return; }
        view.ns = s.namespace; view.branch = s.branch;
        renderPanels(s);
        const fresh = Brain.loadGraph(g, gj, now());
        for (const n of fresh) glow(n.id, 1200);
        for (const p of Brain.announce(g, fresh)) pulse(p);
        const scrub = $("scrub");
        if (view.scrub == null) { scrub.max = g.seq; scrub.value = g.seq; scrub.style.setProperty("--p", "100%"); }
        else scrub.max = Math.max(Number(scrub.max), s.seq);
        dirty = true;
        lastLoad = now();
        if (!view.firstPaint) window.requestAnimationFrame(() => { view.firstPaint = now() - t0; });
      } catch (err) {
        if (session.state !== "stopped") $("foot-store").textContent = String(err.message || err);
      }
    })();
    await loading;
    loading = null;
    if (wantedAgain) { wantedAgain = false; load("again"); }
  }
  // A store with a hundred thousand nodes is not refetched for every write;
  // the wait grows with the graph, and the event itself was already drawn.
  function reconcile() {
    const wait = Math.max(400, Math.min(5000, g.nodes.length / 20));
    const due = Math.max(0, lastLoad + wait - now());
    window.clearTimeout(timer);
    timer = window.setTimeout(() => load("event"), due);
  }
  $("ns").addEventListener("change", () => { view.ns = $("ns").value; view.focus = null; closeSheet(); load("ns"); });
  $("branch").addEventListener("change", () => { view.branch = $("branch").value; view.focus = null; closeSheet(); load("branch"); });

  // ---- lenses, time travel, search, theme -----------------------------------
  $("lens").addEventListener("click", (e) => {
    const b = e.target.closest("button");
    if (!b) return;
    view.lens = b.dataset.l;
    document.querySelectorAll("#lens button").forEach((x) => { x.classList.toggle("on", x === b); x.setAttribute("aria-selected", String(x === b)); });
    dirty = true;
  });
  narrowQuery.addEventListener("change", () => { $("narrow").hidden = !(narrowQuery.matches && view.lens !== "all"); dirty = true; });
  let scrubTimer = null;
  $("scrub").addEventListener("input", (e) => {
    const v = Number(e.target.value), max = Number(e.target.max);
    view.scrub = v < max ? v : null;
    e.target.style.setProperty("--p", (max ? (v / max) * 100 : 100) + "%");
    $("past").textContent = view.scrub == null ? "now · drag to see the past, read only" : `viewing seq ${v} of ${max} · the past, read only`;
    dirty = true;
    window.clearTimeout(scrubTimer);
    scrubTimer = window.setTimeout(() => load("scrub"), 200);
  });
  let searchTimer = null;
  $("q").addEventListener("input", (e) => {
    const q = e.target.value.trim();
    window.clearTimeout(searchTimer);
    if (!q) { view.hits = new Set(); dirty = true; return; }
    searchTimer = window.setTimeout(async () => {
      try {
        const r = await session.get("search", { ns: view.ns, branch: view.branch, q });
        view.hits = new Set(r.hits.map((h) => h.key));
        dirty = true;
      } catch (err) { /* stopped, or nothing */ }
    }, 120);
  });
  const light = new URLSearchParams(window.location.search).has("light");
  if (light) document.documentElement.setAttribute("data-theme", "light");
  $("theme").setAttribute("aria-pressed", String(light));
  $("theme").addEventListener("click", () => {
    const url = new URL(window.location.href);
    if (light) url.searchParams.delete("light"); else url.searchParams.set("light", "");
    window.location.href = url.toString();
  });

  // ---- compare branches -----------------------------------------------------
  $("compare").addEventListener("click", () => {
    const opts = (chosen) => (view.summary?.branches || []).map((b) => `<option value="${Brain.esc(b)}"${b === chosen ? " selected" : ""}>${Brain.esc(b)}</option>`).join("");
    const other = (view.summary?.branches || []).find((b) => b !== view.branch) || view.branch;
    openSheet(`<div class="k">compare</div><h3 id="sheet-title" tabindex="-1">Two branches, side by side</h3><div class="val">The keys that differ: what a merge would face. Nothing here merges anything.</div><div class="h"><label>a <select id="cmp-a">${opts(view.branch)}</select></label><label>b <select id="cmp-b">${opts(other)}</select></label><button class="primary" id="cmp-go">Compare</button></div><div id="cmp-out"></div>`);
    $("cmp-go").addEventListener("click", async () => {
      const a = $("cmp-a").value, b = $("cmp-b").value;
      $("cmp-out").innerHTML = '<div class="h">comparing…</div>';
      try {
        const d = await session.get("diff", { a, b, ns: view.ns });
        $("cmp-out").innerHTML = `<h4>${d.count === 0 ? "No key differs" : Brain.plural(d.count, "key differs", "keys differ")}${d.omitted ? ` · ${d.omitted} not shown` : ""}</h4>` + d.changes.map((c) => `<div class="h"><span class="mono ${c.kind === "added" ? "fresh" : c.kind === "removed" ? "stale" : "cl"}">${c.kind}</span><b class="mono">${Brain.esc(c.key)}</b>${c.a != null ? `<span><span class="dim">${Brain.esc(a)}:</span> ${Brain.esc(c.a)}</span>` : ""}${c.b != null ? `<span><span class="dim">${Brain.esc(b)}:</span> ${Brain.esc(c.b)}</span>` : ""}</div>`).join("");
      } catch (err) { $("cmp-out").innerHTML = `<div class="h">${Brain.esc(String(err.message || err))}</div>`; }
    });
  });

  // ---- start ----------------------------------------------------------------
  const token = Brain.readToken(window.location.hash);
  if (!token) { session.set("no-token"); return; }
  session.token = token;
  window.requestAnimationFrame(drawLive);

  session.get("summary").then((s) => {
    if (String(s.port) !== window.location.port) { session.set("stopped", "this page belongs to another daemon"); return; }
    view.ns = s.namespace; view.branch = s.branch;
    session.version = s.version; session.port = s.port;
    session.set("connected");
    load("first").then(() => {
      Brain.streamEvents(io, token, (line) => {
        if (line.kind === "hello" && String(line.port) !== window.location.port) { session.set("stopped", "another daemon answers here"); return; }
        const fx = Brain.applyEvent(g, line, { ns: view.ns, branch: view.branch }, now());
        if (!fx) return;
        for (const id of fx.glows) glow(id);
        for (const p of fx.pulses) pulse(p);
        dirty = true;
        if (fx.refetch || fx.panels) reconcile();
      }, () => {
        session.probe().then((alive) => { if (alive === true) session.set("stopped", "the event stream ended"); });
      });
    });
  }).catch(() => {
    if (session.state !== "stopped") session.set("stopped", "the daemon did not answer");
  });
}

if (typeof document !== "undefined" && document.getElementById("stage")) boot(document, window);
