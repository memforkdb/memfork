// The Brain's script. Plain JavaScript, no library, nothing loaded from
// anywhere but this daemon. Split in two: `Brain` holds everything that can
// run without a browser, which is how it is tested; `boot` at the end wires
// it to the page.
//
// Memory contents are untrusted text. Every value that reaches the markup
// goes through `esc` or `textContent`; nothing stored is ever markup.
"use strict";

const Brain = (() => {
  // ---- text ----------------------------------------------------------------
  const esc = (s) =>
    String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
  const tokens = (bytes) => Math.ceil((bytes || 0) / 4);
  const plural = (n, one, many) => `${n} ${n === 1 ? one : many || one + "s"}`;

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
      async get(name, params) {
        if (s.state === "stopped") throw new Error("stopped");
        const q = Object.entries(params || {})
          .filter(([, v]) => v !== undefined && v !== null && v !== "")
          .map(([k, v]) => `${encodeURIComponent(k)}=${encodeURIComponent(v)}`)
          .join("&");
        const r = await io.fetch(`/brain/${name}${q ? "?" + q : ""}`, {
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

  return { esc, tokens, plural, readToken, makeSession, streamEvents };
})();

if (typeof module !== "undefined" && module.exports) module.exports = Brain;

// ---- the page ---------------------------------------------------------------
function boot(document, window) {
  const $ = (id) => document.getElementById(id);
  const io = { fetch: window.fetch.bind(window), TextDecoder: window.TextDecoder };
  const session = Brain.makeSession(io);
  const t0 = window.performance.now();

  function showState(state) {
    const dot = $("dot");
    const word = $("state");
    dot.className = "dot" + (state === "connected" ? "" : state === "stopped" || state === "no-token" ? " off" : " wait");
    word.textContent = state === "no-token" ? "no token" : state;
    $("stopped").hidden = state !== "stopped";
    $("notoken").hidden = state !== "no-token";
    if (state === "stopped") {
      $("port").textContent = session.port ? String(session.port) : "";
      document.title = "MemFork · The Brain · stopped";
    }
  }
  session.on(showState);

  function applySummary(sum) {
    session.version = sum.version;
    session.port = sum.port;
    $("port").textContent = String(sum.port);
    $("foot-store").textContent = `${Brain.plural(sum.entries, "entry", "entries")} on ${sum.branch} · seq ${sum.seq}`;
    $("foot-policy").textContent = `policy: ${sum.policy === "none" ? "none in force" : sum.policy}`;
    const ns = $("ns");
    ns.replaceChildren(...(sum.namespaces.length ? sum.namespaces : [sum.namespace]).map((n) => {
      const o = document.createElement("option");
      o.value = n; o.textContent = n; o.selected = n === sum.namespace; return o;
    }));
    const br = $("branch");
    br.replaceChildren(...sum.branches.map((b) => {
      const o = document.createElement("option");
      o.value = b; o.textContent = b; o.selected = b === sum.branch; return o;
    }));
    $("perf").textContent = `first paint ${Math.round(window.performance.now() - t0)} ms · 0 nodes`;
  }

  // The theme lives in the address, never in storage.
  const light = new URLSearchParams(window.location.search).has("light");
  if (light) document.documentElement.setAttribute("data-theme", "light");
  $("theme").setAttribute("aria-pressed", String(light));
  $("theme").addEventListener("click", () => {
    const url = new URL(window.location.href);
    if (light) url.searchParams.delete("light"); else url.searchParams.set("light", "");
    window.location.href = url.toString();
  });

  const token = Brain.readToken(window.location.hash);
  if (!token) {
    session.set("no-token");
    return;
  }
  session.token = token;

  session.get("summary").then((sum) => {
    if (String(sum.port) !== window.location.port) {
      session.set("stopped", "this page belongs to another daemon");
      return;
    }
    applySummary(sum);
    session.set("connected");
    Brain.streamEvents(io, token, (line) => {
      if (line.kind === "hello" && String(line.port) !== window.location.port) {
        session.set("stopped", "another daemon answers here");
      }
    }, () => {
      session.probe().then((alive) => {
        if (alive === true) session.set("stopped", "the event stream ended");
      });
    });
  }).catch(() => {
    if (session.state !== "stopped") session.set("stopped", "the daemon did not answer");
  });
}

if (typeof document !== "undefined" && document.getElementById("stage")) boot(document, window);
