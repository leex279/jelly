// Injected into every jelly view as `window.jelly`.
//
//   jelly.state                 current state (read only; change it with verbs)
//   jelly.onState(fn)           fn(state, event) now and after every change, from anyone
//   jelly.call(verb, args)      run a verb; resolves to the event, rejects with the reason
//   jelly.read(name)            another jelly's state ("plan" means plan in this project)
//   jelly.asset(file)           URL of a file added with `jelly asset` (or just use "assets/<file>")
//   jelly.watch(name, fn)       fn(state, event) for another jelly's changes (a feed)
//   jelly.onAttend(fn)          fn(item) when the dock opens a question; returns unsubscribe
//   jelly.itemsAt("#thumbs")    questions asked about that element; .answer.option is the pick
//
// Comments anywhere: hold Option (Alt) and click any element, or press the
// Comment button, to pin a comment to it. The agent gets a `comment` event that
// names the element. Give elements an `id` so comments survive a reshape.
// Agents answer on the pin (`jelly reply`); the thread shows when you click it.
//
// A reshape or an undo that changes the view reloads the page. State lives in
// the daemon, so nothing is lost.
//
// An older version (/j/<name>/v/<n>/) boots read only: it never reloads, posts
// no snapshot, and every write rejects. Restore it to change it.
(() => {
  const boot = window.__JELLY__;
  const readOnly = boot.readOnly || null;
  const readOnlyError = () => new Error(`This is version ${readOnly.version} of ${boot.name}, read only. Restore it to change it.`);
  // Errors the page hits, so `jelly see` can tell the agent its page is broken.
  const errors = [];
  let snapT = null;
  const agentMode = new URLSearchParams(location.search).has("agent");
  const noteError = (msg) => { if (errors.length < 10 && msg && !errors.includes(msg)) { errors.push(String(msg).slice(0, 300)); postSnapshot(); } };
  addEventListener("error", (e) => noteError(e.error?.stack?.split("\n").slice(0, 2).join(" ") || e.message || (e.target?.src || e.target?.href ? `could not load ${e.target.src || e.target.href}` : "")), true);
  addEventListener("unhandledrejection", (e) => noteError(`unhandled rejection: ${e.reason?.message || e.reason}`));
  const consoleError = console.error.bind(console);
  console.error = (...a) => { noteError(a.map((x) => x?.stack || x?.message || String(x)).join(" ")); consoleError(...a); };
  let state = boot.state;
  let version = boot.version;
  let comments = boot.comments || boot.feedback || [];
  let attention = boot.attention || [];
  const attentionSubs = [];
  const attendSubs = new Set();
  let working = boot.working || null;
  const subs = [];
  const enc = encodeURIComponent;
  const api = `/api/jellies/${enc(boot.name)}`;
  // Names are unique per project: a short name means a jelly in this one.
  const project = boot.name.includes(":") ? boot.name.split(":")[0] : "";
  const full = (name) => (name.includes(":") || name === "surface" || !project ? name : `${project}:${name}`);

  function apply(ev) {
    if (ev.version <= version || ev.state === undefined) return;
    version = ev.version;
    state = ev.state;
    for (const fn of subs) {
      try { fn(state, ev); } catch (e) { console.error(e); }
    }
  }

  // The element behind a person's verb call, so the agent hears "clicked Pick B".
  let lastUsed = null;
  for (const type of ["pointerdown", "keydown"]) {
    addEventListener(type, (e) => {
      const el = e.target?.closest?.("button, a, input, select, textarea, [data-verb], [role=button], label") || e.target;
      if (el?.nodeType === 1 && !el.closest(".jf-pop, .jf-marker, .jf-ask")) lastUsed = { el, at: Date.now() };
    }, true);
  }
  function source() {
    if (!lastUsed || Date.now() - lastUsed.at > 2000) return undefined;
    const el = lastUsed.el;
    // For a field, name the field, not what was typed in it (that is in the args).
    const field = el.matches("input, textarea, select");
    const text = field ? el.getAttribute("aria-label") || el.placeholder || el.name || "a field" : el.innerText || el.getAttribute("aria-label") || "";
    return { selector: selectorFor(el), text: text.trim().replace(/\s+/g, " ").slice(0, 80) };
  }

  async function post(path, body) {
    if (readOnly) throw readOnlyError();
    const r = await fetch(path, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ ...body, by: "person" }),
    });
    const j = await r.json();
    if (!r.ok) throw new Error(j.error || r.statusText);
    return j;
  }

  // One WebSocket per page carries every event; handlers filter by jelly.
  const listeners = new Set();
  let dropped = false;
  (function connect() {
    const ws = new WebSocket(`${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/api/ws`);
    ws.onopen = () => { if (dropped) location.reload(); };
    ws.onmessage = (m) => {
      const ev = JSON.parse(m.data);
      if (ev.kind === "resync") return location.reload();
      for (const fn of listeners) {
        try { fn(ev); } catch (e) { console.error(e); }
      }
    };
    ws.onclose = () => { dropped = true; setTimeout(connect, 1000); };
  })();

  listeners.add((ev) => {
    if (ev.jelly !== boot.name) return;
    if (ev.kind === "remove") {
      document.body.innerHTML = '<p style="font:14px system-ui;padding:16px;opacity:.7">This jelly was removed.</p>';
      return;
    }
    if (readOnly) return;
    if (ev.kind === "new" || ev.view_rev !== boot.viewRev) { location.reload(); return; }
    if (ev.kind === "comment" || ev.kind === "resolve" || ev.kind === "reply") refreshThreads().catch(console.error);
    if (ev.kind === "attention" || ev.kind === "answer" || ev.kind === "withdraw") refreshThreads().catch(console.error);

    if (ev.kind === "working") { working = ev.args ? { what: ev.args, by: ev.by } : null; drawWorking(); }
    apply(ev);
  });

  window.jelly = {
    name: boot.name,
    about: boot.about,
    get state() { return state; },
    /** Every event from every jelly. Returns a function that stops listening. */
    events(fn) { listeners.add(fn); return () => listeners.delete(fn); },
    /** Comment threads on this jelly. */
    get comments() { return comments; },
    get feedback() { return comments; }, // old name
    onState(fn) { subs.push(fn); fn(state, null); },
    async call(verb, args = {}) {
      const ev = await post(`${api}/call`, { verb, args, source: source() });
      apply(ev);
      return ev;
    },
    async read(name) {
      const r = await fetch(`/api/jellies/${enc(full(name))}/state`);
      if (!r.ok) throw new Error((await r.json()).error);
      return r.json();
    },
    watch(name, fn) {
      name = full(name);
      let seen = 0;
      this.read(name).then((s) => fn(s, null), (e) => console.warn(e.message));
      return this.events((ev) => {
        if (ev.jelly === name && ev.state !== undefined && ev.version > seen) { seen = ev.version; fn(ev.state, ev); }
      });
    },
    comment: (anchor, text) => post(`${api}/comment`, { anchor, text }),
    /** Put another jelly on screen: on the jelly page it opens full size, alone it navigates. */
    focus(name) {
      name = full(name);
      if (embedded) parent.postMessage({ type: "jelly:show", name }, location.origin);
      else location.href = `/j/${enc(name)}/`;
    },
    show(name) { return this.focus(name); }, // old name
    resolve: (id) => post(`${api}/resolve`, { id }),
    /** Open asks and looks the agent put on this jelly. */
    get attention() { return attention.filter((a) => !a.answer && !a.withdrawn); },
    /** Every attention item, answered and withdrawn ones too. */
    get items() { return attention; },
    /** The items asked about one element (`--at "#thumbs"`), newest first: item.answer.option is the pick. */
    itemsAt: (selector) => attention.filter((a) => a.anchor?.selector === selector && !a.withdrawn).reverse(),
    /** fn() now and whenever attention items or comments change. */
    onAttention(fn) { attentionSubs.push(fn); fn(); },
    onAttend(fn) { attendSubs.add(fn); return () => attendSubs.delete(fn); },
    /** Answer one: an option id, text, or both. A look takes nothing ("seen"). */
    answer: (id, { option, text } = {}) => post(`${api}/attention/${enc(id)}/answer`, { option, text: text || "" }),
    asset: (file) => `/j/${enc(boot.name)}/assets/${enc(file)}`,
    reply: (id, text) => post(`${api}/reply`, { id, text }),
  };

  // ---- comments: markers on the elements they are about ------------------------

  // Views place Comment via --jelly-comment-inset inherited from root or body.
  const css = `
    .jf-btn { position: fixed; inset: var(--jelly-comment-inset, auto 10px 10px auto); z-index: 2147483000; font: 12px -apple-system, sans-serif;
      background: #1a2021; color: #d6dedc; border: 1px solid #2f383a; border-radius: 999px; padding: 5px 11px; cursor: pointer; opacity: .85; }
    .jf-btn.on { background: #e0a24f; color: #1b2221; border-color: #e0a24f; opacity: 1; }
    .jf-hover { outline: 2px dashed #e0a24f !important; outline-offset: 2px; cursor: crosshair !important; }
    .jf-marker { position: absolute; z-index: 2147482999; width: 20px; height: 20px; border-radius: 50% 50% 0 50%; background: #e0a24f; color: #1b2221;
      font: 600 11px -apple-system, sans-serif; display: grid; place-items: center; cursor: pointer; box-shadow: 0 2px 6px rgba(0,0,0,.3); }
    .jf-marker.agent { background: #46c4b8; }
    .jf-marker.done { opacity: .55; }
    .jf-pop { color-scheme: light; position: absolute; z-index: 2147483001; width: 260px; background: #fff; color: #1b2221; border: 1px solid #e2e7e5; border-radius: 10px;
      box-shadow: 0 14px 34px -10px rgba(0,0,0,.45); padding: 8px; font: 12px -apple-system, sans-serif; }
    .jf-pop .jf-on { font: 10px ui-monospace, Menlo, monospace; color: #66726f; margin-bottom: 5px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
    .jf-pop textarea { width: 100%; box-sizing: border-box; min-height: 54px; font: 12px -apple-system, sans-serif; border: 1px solid #e2e7e5; border-radius: 6px; padding: 5px; resize: vertical; }
    .jf-pop .jf-row { display: flex; gap: 6px; justify-content: flex-end; margin-top: 6px; align-items: center; }
    .jf-pop button { font: 12px -apple-system, sans-serif; color: #1b2221; border: 1px solid #e2e7e5; background: #fff; border-radius: 6px; padding: 3px 9px; cursor: pointer; }
    .jf-pop button.jf-send { background: #0f7b74; color: #fff; border-color: #0f7b74; }
    .jf-pop .jf-by { font: 10px ui-monospace, monospace; color: #b26a12; }
    .jf-pop .jf-thread { display: grid; gap: 6px; margin-bottom: 6px; max-height: 260px; overflow-y: auto; }
    .jf-pop .jf-msg { border-left: 2px solid #e0a24f; padding: 1px 0 1px 7px; }
    .jf-pop .jf-msg.agent { border-color: #46c4b8; }
    .jf-pop .jf-msg.agent .jf-by { color: #0f7b74; }
    .jf-pop .jf-text { white-space: pre-wrap; margin-top: 1px; }
    .jf-pop .jf-done { font-size: 11px; color: #66726f; }
    .jf-work { position: fixed; left: 50%; top: 10px; transform: translateX(-50%); z-index: 2147483000; background: #141a19; color: #e8eeec; border-radius: 999px; padding: 6px 14px 6px 12px; font: 12.5px -apple-system, sans-serif; display: flex; gap: 8px; align-items: center; box-shadow: 0 8px 24px -8px rgba(0,0,0,.5); }
    .jf-work i { width: 8px; height: 8px; border-radius: 50%; background: #46c4b8; animation: jf-blink 1s infinite; } @keyframes jf-blink { 50% { opacity: .3; } }
    .jf-version { position: fixed; left: 50%; top: 10px; transform: translateX(-50%); z-index: 2147483000; background: #141a19; color: #e8eeec; border-radius: 999px; padding: 6px 8px 6px 14px; font: 12.5px -apple-system, sans-serif; display: flex; gap: 10px; align-items: center; box-shadow: 0 8px 24px -8px rgba(0,0,0,.5); }
    .jf-version button { font: 12px -apple-system, sans-serif; color: #e8eeec; border: 1px solid #3a4446; background: transparent; border-radius: 999px; padding: 3px 10px; cursor: pointer; }
    .jf-version button.jf-restore { background: #e0a24f; color: #1b2221; border-color: #e0a24f; }
    `;
  let commenting = false, hovered = null, pop = null;
  const btn = document.createElement("button");
  btn.className = "jf-btn";
  btn.textContent = "Comment";
  btn.title = "Pin a comment to any part of this jelly (or hold Option and click)";

  // Everything the runtime adds to a view. Selectors name view elements only.
  const OURS = ".jf-btn, .jf-pop, .jf-marker, .jf-ask, .jf-work, .jf-version";
  const isOurs = (el) => el.closest(OURS);
  function selectorFor(el) {
    const parts = [];
    for (let n = el; n && n.nodeType === 1 && n !== document.body; n = n.parentElement) {
      if (n.id) { parts.unshift("#" + CSS.escape(n.id)); break; }
      let i = 1;
      for (let s = n.previousElementSibling; s; s = s.previousElementSibling) if (s.tagName === n.tagName && !isOurs(s)) i++;
      parts.unshift(`${n.tagName.toLowerCase()}:nth-of-type(${i})`);
    }
    return parts.join(" > ") || "body";
  }

  // On the jelly page, the page's header owns the Comment button.
  const embedded = window.parent !== window;
  function setMode(on) {
    commenting = on;
    btn.classList.toggle("on", on);
    btn.textContent = on ? "Click a part to comment · Esc" : "Comment";
    if (!on && hovered) { hovered.classList.remove("jf-hover"); hovered = null; }
    if (embedded) parent.postMessage({ type: "jelly:commenting", name: boot.name, on }, location.origin);
  }
  addEventListener("message", (e) => {
    if (e.origin === location.origin && e.data?.type === "jelly:comment") setMode(!!e.data.on);
  });

  function openPop(x, y, html) {
    pop?.remove();
    pop = document.createElement("div");
    pop.className = "jf-pop";
    pop.innerHTML = html;
    document.body.appendChild(pop);
    const w = document.documentElement.clientWidth;
    pop.style.left = Math.max(8, Math.min(x, w - 276)) + scrollX + "px";
    pop.style.top = y + scrollY + 10 + "px";
    return pop;
  }

  function startComment(el, x, y) {
    const anchor = { selector: selectorFor(el), quote: (el.innerText || el.textContent || "").trim().replace(/\s+/g, " ").slice(0, 80) };
    const p = openPop(x, y, `<div class="jf-on"></div><textarea placeholder="What should change here?"></textarea>
      <div class="jf-row"><button class="jf-cancel">Cancel</button><button class="jf-send">Send</button></div>`);
    p.querySelector(".jf-on").textContent = anchor.quote ? `on "${anchor.quote}"` : `on ${anchor.selector}`;
    const ta = p.querySelector("textarea");
    ta.focus();
    const send = () => jelly.comment(anchor, ta.value).then(() => { p.remove(); pop = null; }, (e) => alert(e.message));
    p.querySelector(".jf-send").onclick = send;
    p.querySelector(".jf-cancel").onclick = () => { p.remove(); pop = null; };
    ta.onkeydown = (e) => { if (e.key === "Enter" && !e.shiftKey && !e.isComposing) { e.preventDefault(); send(); } if (e.key === "Escape") { p.remove(); pop = null; } };
  }

  let markerEls = [];
  const DAY = 86400000;
  const lastAt = (f) => Math.max(f.at_ms, ...(f.replies || []).map((r) => r.at_ms));
  // Open threads, plus resolved ones from the last day so the person sees the answer.
  const visible = () => comments.filter((f) => !f.resolved || Date.now() - lastAt(f) < DAY);
  const who = (by) => (by === "person" ? "you" : by);

  function thread(f, x, y) {
    const msgs = [{ by: f.by, text: f.text, at_ms: f.at_ms }, ...(f.replies || [])];
    const p = openPop(x, y, `<div class="jf-on"></div><div class="jf-thread"></div>
      <textarea placeholder="${f.resolved ? "Not done? Say what is still off" : "Reply"}"></textarea>
      <div class="jf-row"><button class="jf-close">Close</button>${f.resolved ? "" : '<button class="jf-resolve">Resolve</button>'}<button class="jf-send">Reply</button></div>`);
    p.querySelector(".jf-on").textContent = f.anchor?.quote ? `on "${f.anchor.quote}"` : `on ${f.anchor?.selector || "the jelly"}`;
    const t = p.querySelector(".jf-thread");
    for (const m of msgs) {
      const d = document.createElement("div");
      d.className = "jf-msg" + (m.by === "person" ? "" : " agent");
      d.innerHTML = '<div class="jf-by"></div><div class="jf-text"></div>';
      d.querySelector(".jf-by").textContent = who(m.by);
      d.querySelector(".jf-text").textContent = m.text;
      t.appendChild(d);
    }
    if (f.resolved) t.insertAdjacentHTML("beforeend", '<div class="jf-done">Resolved</div>');
    const ta = p.querySelector("textarea");
    const close = () => { p.remove(); pop = null; };
    const send = () => ta.value.trim() && jelly.reply(f.id, ta.value).then(close, (e) => alert(e.message));
    p.querySelector(".jf-close").onclick = close;
    p.querySelector(".jf-send").onclick = send;
    const r = p.querySelector(".jf-resolve");
    if (r) r.onclick = () => jelly.resolve(f.id).then(close);
    ta.onkeydown = (e) => { if (e.key === "Enter" && !e.shiftKey && !e.isComposing) { e.preventDefault(); send(); } if (e.key === "Escape") close(); };
  }

  function drawMarkers() {
    drawAttention();
    markerEls.forEach((p) => p.remove());
    markerEls = [];
    const shown = visible();
    const anchors = attentionAnchors(shown.map((f) => ({ anchor: { selector: f.anchor?.selector || "body" } })), keepFocus());
    shown.forEach((f, i) => {
      const el = anchors[i].element;
      if (!el) return;
      const r = el.getBoundingClientRect();
      const last = f.replies?.at(-1);
      const pin = document.createElement("div");
      // Amber: waiting on the agent. Teal: the agent answered. Faded: resolved.
      pin.className = "jf-marker" + (last && last.by !== "person" ? " agent" : "") + (f.resolved ? " done" : "");
      pin.textContent = f.resolved ? "✓" : i + 1;
      pin.title = last ? `${who(last.by)}: ${last.text}` : f.text;
      // In the margin left of the element, pointing at it.
      pin.style.left = Math.max(2, r.left - 24) + scrollX + "px";
      pin.style.top = r.top + Math.min(r.height, 40) / 2 - 10 + scrollY + "px";
      pin.onclick = (e) => { e.stopPropagation(); thread(f, r.left, r.bottom); };
      document.body.appendChild(pin);
      markerEls.push(pin);
    });
  }
  // ---- attention: questions sit on the page, right below what they are about ----
  // Each question is a card in the page flow, after its anchor row (or at the
  // top when it has none). It lives in a shadow root so the page's CSS and ours
  // never touch. Answered questions shrink to one line for a day, with Change.
  const DAY_MS = 86400000;
  const cards = new Map(); // item id -> host element, reused so drafts survive redraws
  const isOpen = (a) => !a.answer && !a.withdrawn;
  const shownItems = () => attention.filter((a) => !a.withdrawn && (!a.answer || Date.now() - a.answer.at_ms < DAY_MS));
  const editing = new Set(); // answered items the person reopened with Change
  const CARD_CSS = `
    :host { all: initial; display: block; margin: 14px 0; --bg:#fff; --ink:#2a1620; --soft:#6b5260; --line:#f0dfe5; --tint:#fdf0f4; --accent:#e8336f; --accent-ink:#a3134c; }
    :host([dark]) { --bg:#25162c; --ink:#f6e8f2; --soft:#c3a9bd; --line:#43294d; --tint:#33203b; --accent:#ff5c95; --accent-ink:#ff8fb6; }
    * { box-sizing: border-box; font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; letter-spacing: normal; text-transform: none; }
    .c { background: var(--bg); color: var(--ink); border: 1.5px solid var(--line); border-left: 4px solid var(--accent); border-radius: 14px; padding: 16px 18px; font-size: 14px; line-height: 1.5; text-align: left; }
    .c.flash { animation: flash 1.2s ease-out; } @keyframes flash { 0% { box-shadow: 0 0 0 0 var(--accent); } 40% { box-shadow: 0 0 0 6px color-mix(in srgb, var(--accent) 25%, transparent); } }
    .who { font: 600 12px ui-monospace, Menlo, monospace; color: var(--accent-ink); }
    h3 { margin: 4px 0 0; font-size: 18px; line-height: 1.3; font-weight: 700; }
    .ctx { margin: 6px 0 0; color: var(--soft); white-space: pre-wrap; } .ctx:empty { display: none; }
    .rec { margin-top: 10px; background: var(--tint); border-radius: 10px; padding: 8px 12px; font-size: 13.5px; }
    .rec b { color: var(--accent-ink); }
    .opts { display: grid; grid-template-columns: repeat(auto-fit, minmax(200px, 1fr)); gap: 8px; margin-top: 12px; }
    .o { all: unset; box-sizing: border-box; cursor: pointer; padding: 11px 13px; border: 1.5px solid var(--line); border-radius: 11px; background: var(--bg); color: var(--ink); }
    button:focus-visible, textarea:focus-visible, .c:focus-visible { outline: 3px solid var(--accent); outline-offset: 3px; }
    .exposure { color: var(--soft); font-size: 12px; margin-bottom: 8px; }
    .exposure:empty { display: none; }
    .o:hover { border-color: var(--accent); }
    .o b { display: block; font-size: 15px; font-weight: 650; }
    .o span { display: block; font-size: 13px; color: var(--soft); margin-top: 2px; } .o span:empty { display: none; }
    .o.rec-o b::after { content: "recommended"; margin-left: 8px; font-size: 11px; font-weight: 600; color: #fff; background: var(--accent); border-radius: 99px; padding: 1px 7px; vertical-align: 2px; }
    .o.on { border-color: var(--accent); background: var(--tint); }
    .row { display: flex; gap: 8px; margin-top: 10px; align-items: flex-end; }
    textarea { flex: 1; min-width: 0; min-height: 40px; font-size: 14px; color: var(--ink); background: var(--bg); border: 1.5px solid var(--line); border-radius: 10px; padding: 8px 10px; resize: vertical; }
    textarea:focus { outline: none; border-color: var(--accent); }
    .go { all: unset; cursor: pointer; font-size: 14px; font-weight: 600; padding: 9px 16px; border-radius: 10px; background: var(--accent); color: #fff; white-space: nowrap; }
    .go[disabled] { opacity: .4; cursor: default; }
    .hint { font-size: 12px; color: var(--soft); margin-top: 6px; }
    .done { display: flex; gap: 10px; align-items: baseline; flex-wrap: wrap; }
    .done .q { color: var(--soft); } .done .a { font-weight: 650; } .done .n { color: var(--soft); white-space: pre-wrap; }
    .chg { all: unset; cursor: pointer; color: var(--accent-ink); font-size: 13px; font-weight: 600; margin-left: auto; }
    .c.answered { border-left-color: #3aa57a; padding: 10px 14px; }`;

  // Light or dark from the page's own background, so the card belongs to the page.
  function pageIsDark() {
    for (const el of [document.body, document.documentElement]) {
      const m = getComputedStyle(el).backgroundColor.match(/[\d.]+/g);
      if (m && (m.length < 4 || +m[3] > 0.5)) return (0.299 * m[0] + 0.587 * m[1] + 0.114 * m[2]) < 128;
    }
    // No background anywhere: the browser paints white unless the page opts into dark.
    const scheme = getComputedStyle(document.documentElement).colorScheme + " " + (document.querySelector('meta[name="color-scheme"]')?.content || "");
    return /dark/.test(scheme) && matchMedia("(prefers-color-scheme: dark)").matches;
  }
  function normalBoundary(el) {
    while (el?.parentElement && el.parentElement !== document.body) {

      const p = el.parentElement, d = getComputedStyle(p).display;
      if (/grid|flex|table|inline/.test(d) || ["UL", "OL", "TR", "TBODY", "THEAD", "TFOOT", "TABLE", "P", "BUTTON", "A", "LABEL"].includes(p.tagName)) el = p;
      else break;
    }
    return el === document.body || el === document.documentElement ? null : el;
  }
  function fallback(group) {
    return { kind: "block", after: normalBoundary(group), observe: group, status: "fallback", reason: "this layout cannot take a full-width question row" };
  }
  function rowPlacement(group, item) {
    const cs = getComputedStyle(group), grid = cs.display.includes("grid");
    if (cs.transform !== "none" || [...group.childNodes].some((n) => n.nodeType === 3 && n.textContent.trim()) ||
        cs.writingMode !== "horizontal-tb" || cs.direction !== "ltr" ||
        (grid ? cs.gridAutoFlow !== "row" : cs.flexDirection !== "row" || cs.flexWrap !== "wrap")) return fallback(group);
    const children = [...group.children].filter((el) => !isOurs(el) && el.getClientRects().length && !["absolute", "fixed"].includes(getComputedStyle(el).position));
    const rows = [];
    for (const el of children) {
      const style = getComputedStyle(el), r = el.getBoundingClientRect();
      if (style.order !== "0" || style.transform !== "none" || style.position === "relative" && [style.top, style.right, style.bottom, style.left].some((v) => v !== "auto" && parseFloat(v) !== 0) ||
          (grid && [style.gridRowStart, style.gridRowEnd, style.gridColumnStart, style.gridColumnEnd].some((v) => v !== "auto"))) return fallback(group);
      if (!r.width || !r.height) continue;
      let row = rows.at(-1);
      if (!row || r.top >= row.bottom - 1) { row = { top: r.top, bottom: r.bottom, members: [] }; rows.push(row); }
      else if (r.bottom <= row.top + 1 || row.members.some((peer) => {
        const pr = peer.getBoundingClientRect();
        return r.left < pr.right - 1 && r.right > pr.left + 1;
      })) return fallback(group);
      row.top = Math.min(row.top, r.top); row.bottom = Math.max(row.bottom, r.bottom); row.members.push(el);
    }
    const i = rows.findIndex((row) => row.members.includes(item));
    if (i < 0) return fallback(group);
    return { kind: grid ? "grid" : "flex", group, after: rows[i].members.at(-1), peers: rows[i].members, later: rows.slice(i + 1).flatMap((row) => row.members) };
  }
  function tablePlacement(row) {
    const table = row.closest("table"), rows = [...table.rows].filter((r) => !isOurs(r));
    let occupied = [], rowGroup = null;
    let columns = 0, crossing = false;
    for (const r of rows) {
      if (r.parentElement !== rowGroup) { occupied = []; rowGroup = r.parentElement; }
      let col = 0;
      for (const cell of r.cells) {
        while (occupied[col] > 0) col++;
        const span = cell.rowSpan || Infinity;
        for (let i = 0; i < cell.colSpan; i++) occupied[col++] = span;
      }
      columns = Math.max(columns, occupied.length);
      if (r === row) crossing = occupied.some((span) => span > 1);
      for (let i = 0; i < occupied.length; i++) occupied[i] = Math.max(0, occupied[i] - 1);
    }
    return crossing ? fallback(table) : { kind: "table", group: table, after: row, columns, peers: [row], later: rows.slice(rows.indexOf(row) + 1) };
  }
  function listCanInsert(group) {
    // Without an explicit start, a reversed list counts every li, including slots.
    if (group.reversed && !group.hasAttribute("start")) return false;
    return [...group.children].filter((el) => !isOurs(el)).every((el) => {
      const increment = getComputedStyle(el).counterIncrement;
      return ["none", "list-item 1", "list-item -1"].includes(increment) &&
        ["::before", "::after"].every((pseudo) => ["none", "normal"].includes(getComputedStyle(el, pseudo).content));
    });
  }
  function resolvePlacement(a, { element: el, invalid }) {
    if (invalid) return { kind: "block", after: null, status: "invalid" };
    if (!el) return { kind: "block", after: null, status: a.anchor?.selector ? "missing" : "direct" };
    if (el === document.body || el === document.documentElement || isOurs(el)) return { kind: "block", after: null, status: "floating" };
    const hidden = !available(el);
    const svg = !!el.ownerSVGElement;
    while (el.ownerSVGElement) el = el.ownerSVGElement;
    for (let n = el; n && n !== document.body; n = n.parentElement) if (["fixed", "absolute", "sticky"].includes(getComputedStyle(n).position)) return { kind: "block", after: null, status: "floating" };
    if (hidden) return { kind: "block", after: null, status: "hidden" };
    let result;
    for (let item = el; item && item !== document.body; item = item.parentElement) {
      const group = item.parentElement;
      if (!group) break;
      const display = getComputedStyle(group).display;
      if (["TD", "TH"].includes(item.tagName)) { result = tablePlacement(item.parentElement); break; }
      if (item.tagName === "TR") { result = tablePlacement(item); break; }
      if (["UL", "OL"].includes(group.tagName) && !listCanInsert(group)) { result = fallback(group); break; }
      if (display.includes("grid") || display.includes("flex")) {
        const restricted = group.closest("button, a, label, p");
        if (restricted === group) continue;
        result = restricted ? fallback(restricted) : rowPlacement(group, item);
        break;
      }
      if (item.tagName === "LI" && ["UL", "OL"].includes(group.tagName)) {
        const siblings = [...group.children].filter((el) => !isOurs(el));
        result = { kind: "list", after: item, group, peers: [item], later: siblings.slice(siblings.indexOf(item) + 1) };
        break;
      }
    }
    result ||= { kind: "block", after: normalBoundary(el) };
    if (!result.status && result.kind === "block" && result.after !== el) { result.status = "fallback"; result.reason = "the anchor cannot contain a question card"; }
    if (svg && !result.status) { result.status = "fallback"; result.reason = "the anchor is inside a drawing"; }
    return result;
  }
  function renderCard(host, a) {
    const root = host.shadowRoot;
    const isAsk = a.kind === "ask";
    const opts = a.options || [];
    const answered = a.answer && !editing.has(a.id);
    const draft = root.querySelector("textarea")?.value || "";
    root.innerHTML = `<style>${CARD_CSS}</style><div class="c${answered ? " answered" : ""}"></div>`;
    const c = root.querySelector(".c");
    c.tabIndex = -1;
    c.setAttribute("aria-label", a.title);
    if (answered) {
      const o = opts.find((x) => x.id === a.answer.option);
      c.innerHTML = `<div class="done"><span class="q"></span><span class="a"></span><span class="n"></span><button class="chg">Change</button></div>`;
      c.querySelector(".q").textContent = a.title;
      c.querySelector(".a").textContent = "✓ " + (o ? o.label || o.id : a.kind === "look" ? "Seen" : "Answered");
      c.querySelector(".n").textContent = a.answer.text || "";
      c.querySelector(".chg").onclick = () => { editing.add(a.id); drawAttention(); };
      return;
    }
    const rec = opts.find((o) => o.id === a.recommend);
    c.innerHTML = `<div class="who"></div><h3></h3><p class="ctx"></p>
      ${rec ? '<div class="rec"><b></b> <span></span></div>' : ""}
      ${opts.length ? '<div class="opts"></div>' : ""}
      <div class="row"><textarea rows="1"></textarea><button class="go"></button></div>
      <div class="hint">${opts.length ? "Click an option to answer. Add a note first if you like." : "Enter to send · Shift+Enter for a new line"}</div>`;
    c.querySelector(".who").textContent = `${a.by} ${isAsk ? "asks" : "asks you to look"}`;
    c.querySelector("h3").textContent = a.title;
    c.querySelector(".ctx").textContent = a.context || "";
    if (rec) { c.querySelector(".rec b").textContent = `Recommends ${rec.label || rec.id}.`; c.querySelector(".rec span").textContent = a.why || ""; }
    const ta = c.querySelector("textarea"), go = c.querySelector(".go");
    ta.value = draft;
    ta.placeholder = !isAsk ? "Anything to say? (optional)" : opts.length ? "A note, or your own answer if none fit" : "Your answer";
    go.textContent = !isAsk ? "Seen" : opts.length ? "Answer in words" : "Answer";
    const sync = () => (go.disabled = isAsk && !ta.value.trim());
    ta.oninput = sync; sync();
    const send = (option) => {
      if (isAsk && !option && !ta.value.trim()) return;
      go.disabled = true;
      jelly.answer(a.id, { option, text: ta.value }).then(() => editing.delete(a.id), (e) => { sync(); alert(e.message); });
    };
    for (const o of opts) {
      const b = document.createElement("button");
      b.className = "o" + (o.id === a.recommend ? " rec-o" : "") + (a.answer?.option === o.id ? " on" : "");
      b.innerHTML = "<b></b><span></span>";
      b.querySelector("b").textContent = o.label || o.id;
      b.querySelector("span").textContent = o.does || "";
      b.onclick = () => send(o.id);
      c.querySelector(".opts").appendChild(b);
    }
    go.onclick = () => send(undefined);
    ta.onkeydown = (e) => { if (e.key === "Enter" && !e.shiftKey && !e.isComposing) { e.preventDefault(); send(undefined); } };
  }
  function anchorElement(a) {
    const selector = a.anchor?.selector;
    const question = inline() && selector?.startsWith("#q-") ? document.getElementById(selector.slice(1)) : null;
    if (question) return question;
    try { return selector ? document.querySelector(selector) : null; } catch { return null; }
  }
  function available(el, whole = false) {
    if (!el?.isConnected || el.closest("[inert]")) return false;
    if (getComputedStyle(el).display === "contents") return [...el.children].some((c) => available(c, whole));
    if (!el.checkVisibility({ checkOpacity: true, checkVisibilityCSS: true })) return false;
    const r = el.getBoundingClientRect();
    if (!r.width || !r.height) return false;
    for (let p = el.parentElement; p; p = p.parentElement) {
      const cs = getComputedStyle(p), b = p.getBoundingClientRect();
      const left = b.left + p.clientLeft, top = b.top + p.clientTop;
      const right = left + p.clientWidth, bottom = top + p.clientHeight;
      if (["hidden", "clip"].includes(cs.overflowX) && (whole ? r.left < left - 1 || r.right > right + 1 : r.right <= left || r.left >= right)) return false;
      if (["hidden", "clip"].includes(cs.overflowY) && (whole ? r.top < top - 1 || r.bottom > bottom + 1 : r.bottom <= top || r.top >= bottom)) return false;
    }
    return true;
  }
  // Hiding a subtree can blur its control before the placement observer runs.
  let blurredCardControl = null;
  addEventListener("focusin", () => { blurredCardControl = null; });
  addEventListener("focusout", (e) => { blurredCardControl = e.target.matches?.(".jf-ask") && !available(e.target) ? e.composedPath()[0] : null; });
  let topStack = null;
  function exposureLabel(host, a, rescued) {
    let label = host.shadowRoot.querySelector(".exposure");
    if (!label) {
      label = document.createElement("div");
      label.className = "exposure";
      host.shadowRoot.querySelector(".c").prepend(label);
    }
    const text = rescued ? `About ${a.anchor?.quote || a.anchor?.selector} · The question is shown here so you can answer it.` : "";
    if (label.textContent !== text) label.textContent = text;
    host.toggleAttribute("data-exposed", rescued);
  }
  let outcomes = new Map();
  const slots = new Map();
  const groupObserver = new ResizeObserver(() => queueDraw());
  let observedGroups = new Set();
  function slotFor(id, host, place) {
    // The form's intrinsic width must not widen an auto-layout table.
    host.style.contain = place.kind === "table" ? "inline-size" : "";
    const tag = place.kind === "table" ? "tr" : place.kind === "list" || ["UL", "OL"].includes(place.group?.tagName) ? "li" : "div";
    let slot = slots.get(id);
    if (!slot || slot.localName !== tag) {
      slot?.remove();
      slot = document.createElement(tag);
      slot.className = "jf-ask jf-slot";
      slots.set(id, slot);
      if (tag === "tr") slot.appendChild(document.createElement("td"));
    }
    const container = tag === "tr" ? slot.firstElementChild : slot;
    if (host.parentElement !== container) container.appendChild(host);
    let style = "display:block;min-width:0;box-sizing:border-box;";
    if (tag === "li") style += "list-style:none;counter-increment:list-item 0;padding:0;";
    if (place.kind === "grid") style += "grid-column:1 / -1;grid-row:auto;align-self:stretch;justify-self:stretch;";
    if (place.kind === "flex") style += "flex:0 0 100%;";
    if (tag === "tr") {
      style = "";
      const cell = slot.firstElementChild;
      cell.colSpan = place.columns;
      cell.style.cssText = "padding:0;min-width:0;";
    }
    if (slot.style.cssText !== style) slot.style.cssText = style;
    return slot;
  }
  function insertSlot(slot, place, tails) {
    if (place.after) {
      const tail = tails.get(place.after) || place.after;
      if (tail.nextElementSibling !== slot) tail.after(slot);
      tails.set(place.after, slot);
    } else {
      if (!topStack?.isConnected) { topStack = document.createElement("div"); topStack.className = "jf-ask jf-ask-top"; }
      if (document.body.firstElementChild !== topStack) document.body.prepend(topStack);
      const style = "display:block;grid-column:1/-1;flex-basis:100%;box-sizing:border-box;width:100%;min-width:0;max-width:calc(100vw - 32px);margin:14px 0";
      if (topStack.style.cssText !== style) topStack.style.cssText = style;
      const tail = tails.get(topStack);
      if (tail) { if (tail.nextElementSibling !== slot) tail.after(slot); }
      else if (topStack.firstElementChild !== slot) topStack.prepend(slot);
      tails.set(topStack, slot);
    }
  }
  function rowFits(place, slot) {
    const r = slot.getBoundingClientRect(), g = place.group.getBoundingClientRect(), cs = getComputedStyle(place.group);
    if (place.kind === "table") {
      const peer = place.after.getBoundingClientRect();
      return Math.abs(r.left - peer.left) <= 1 && Math.abs(r.right - peer.right) <= 1 &&
        peer.bottom <= r.top + 1 && place.later.every((el) => el.getBoundingClientRect().top >= r.bottom - 1);
    }
    const left = g.left + parseFloat(cs.borderLeftWidth) + parseFloat(cs.paddingLeft);
    const right = g.right - parseFloat(cs.borderRightWidth) - parseFloat(cs.paddingRight);
    return Math.abs(r.left - left) <= 1 && Math.abs(r.right - right) <= 1 &&
      place.peers.every((el) => el.getBoundingClientRect().bottom <= r.top + 1) &&
      place.later.every((el) => el.getBoundingClientRect().top >= r.bottom - 1);
  }
  function attentionAnchors(items, restoreFocus) {
    const focusStates = ["focus", "focus-visible", "focus-within"].map((pseudo) => ({
      pseudo, elements: [...document.querySelectorAll(`:${pseudo}`)].filter((el) => !isOurs(el)),
    }));
    // Structural selectors need owned siblings absent, but detachment clears
    // focus pseudos. Match saved elements by their paths in the original tree.
    // A style rule's selectorText is the browser's own serialization: escapes
    // resolved, names lowercased, invalid forgiving arguments dropped.
    const sheet = new CSSStyleSheet();
    sheet.replaceSync("*{}");
    const rule = sheet.cssRules[0];
    const elementPath = (el) => {
      const path = [];
      for (let node = el; node.parentElement; node = node.parentElement) {
        let index = 1;
        for (let sibling = node.previousElementSibling; sibling; sibling = sibling.previousElementSibling) index++;
        path.unshift(`:nth-child(${index})`);
      }
      return [":root", ...path].join(" > ");
    };
    const owned = [...document.querySelectorAll(OURS)]
      .filter((el) => !el.parentElement || !isOurs(el.parentElement));
    const positions = owned.map((el) => [el, el.parentNode, el.nextSibling]);
    try {
      for (const el of owned) el.remove();
      const focusSelectors = focusStates.map(({ pseudo, elements }) => ({
        pseudo, selector: elements.map(elementPath).join(",") || ":not(*)",
      }));
      const selectorWithFocus = (selector) => {
        // An invalid selector leaves selectorText as it was, so reject it first.
        document.createDocumentFragment().querySelector(selector);
        rule.selectorText = selector;
        return rule.selectorText.replace(/"(?:\\[\s\S]|[^"\\])*"|\\[\s\S]|:[\w-]+/g, (token) => {
          const state = focusSelectors.find((state) => `:${state.pseudo}` === token);
          return state ? `:is(${state.selector})` : token;
        });
      };
      return items.map((a) => {
        try {
          const selector = a.anchor?.selector;
          if (inline() && selector?.startsWith("#q-")) {
            const element = document.getElementById(selector.slice(1));
            if (!element) document.createDocumentFragment().querySelector(selector);
            return { element };
          }
          return { element: typeof selector === "string" ? document.querySelector(selectorWithFocus(selector)) : null };
        } catch { return { element: null, invalid: true }; }
      });
    } finally {
      for (const [el, parent, next] of positions.reverse()) parent.insertBefore(el, next);
      restoreFocus();
    }
  }
  // Returns a function that puts focus and selection back where they are now.
  function keepFocus() {
    const focusedHost = document.hasFocus() ? [...cards.values()].find((h) => h.shadowRoot.activeElement || (h.shadowRoot.contains(blurredCardControl) && !available(h))) : null;
    const focused = focusedHost?.shadowRoot.activeElement || (focusedHost ? blurredCardControl : document.activeElement);
    const oldSlotParent = focusedHost?.parentElement?.parentElement;
    const selection = focused && "selectionStart" in focused ? [focused.selectionStart, focused.selectionEnd, focused.selectionDirection] : null;
    return () => {
      if (focused?.isConnected && focused.getRootNode().activeElement !== focused) {
        focused.focus({ preventScroll: true });
        if (selection) focused.setSelectionRange(...selection);
      }
      if (focusedHost?.isConnected && oldSlotParent !== focusedHost.parentElement?.parentElement) focused.scrollIntoView({ block: "nearest" });
    };
  }
  function drawAttention() {
    const restoreFocus = keepFocus();
    const allItems = shownItems();
    const selfRendered = inline();
    const items = selfRendered ? [] : allItems;
    const keep = new Set(items.map((a) => a.id));
    for (const [id, host] of cards) if (!keep.has(id)) { slots.get(id)?.remove(); slots.delete(id); host.remove(); cards.delete(id); editing.delete(id); }
    if (!document.body) return;
    const anchors = attentionAnchors(allItems, restoreFocus);
    const positions = [...slots.values()].filter((slot) => slot.isConnected).map((slot) => [slot, slot.style.cssText]);
    let placements;
    try {
      // Keep focus and :focus-within live while taking cards out of row flow.
      for (const [slot] of positions) slot.style.setProperty("position", "absolute", "important");
      placements = allItems.map((a, i) => resolvePlacement(a, anchors[i]));
    } finally {
      for (const [slot, style] of positions) slot.style.cssText = style;
    }
    const groups = new Set(placements.map((p) => p.group || p.observe).filter(Boolean));
    for (const g of observedGroups) if (!groups.has(g)) groupObserver.unobserve(g);
    for (const g of groups) if (!observedGroups.has(g)) groupObserver.observe(g);
    observedGroups = groups;
    const dark = pageIsDark(), tails = new Map();
    items.forEach((a, i) => {
      let host = cards.get(a.id);
      if (!host) {
        host = document.createElement("div"); host.className = "jf-ask"; host.id = `jf-q-${a.id}`;
        host.attachShadow({ mode: "open" }); cards.set(a.id, host);
      }
      host.toggleAttribute("dark", dark);
      const sig = JSON.stringify([a.title, a.context, a.options, a.answer, editing.has(a.id)]);
      if (host.dataset.sig !== sig) { host.dataset.sig = sig; renderCard(host, a); }
      exposureLabel(host, a, false);
      insertSlot(slotFor(a.id, host, placements[i]), placements[i], tails);
    });
    restoreFocus();
    const failed = items.map((a, i) => placements[i].peers && !rowFits(placements[i], slots.get(a.id)));
    if (failed.some(Boolean)) {
      failed.forEach((fail, i) => { if (fail) placements[i] = fallback(placements[i].group); });
      const settledTails = new Map();
      items.forEach((a, i) => insertSlot(slotFor(a.id, cards.get(a.id), placements[i]), placements[i], settledTails));
    }
    restoreFocus();
    const rescued = items.map((a, i) => placements[i].status === "hidden" || !available(cards.get(a.id), true));
    if (rescued.some(Boolean)) {
      const visibleTails = new Map();
      items.forEach((a, i) => {
        if (rescued[i]) placements[i] = { kind: "block", after: null, status: placements[i].status, reason: placements[i].reason };
        insertSlot(slotFor(a.id, cards.get(a.id), placements[i]), placements[i], visibleTails);
      });
    }
    items.forEach((a, i) => exposureLabel(cards.get(a.id), a, rescued[i]));
    restoreFocus();
    outcomes = new Map((refreshingAttention ? [] : allItems).map((a, i) => {
      const p = placements[i];
      const status = selfRendered && !["invalid", "missing", "floating", "hidden"].includes(p.status) ? "unchecked" : p.status || "direct";
      return [a.id, { status, destination: p.after ? { kind: "after", selector: selectorFor(p.after) } : { kind: "top" }, reason: p.reason }];
    }));
    if (topStack && !topStack.children.length) topStack.remove();
    postSnapshot();
  }
  let drawQueued = false;
  const queueDraw = () => { if (!drawQueued) { drawQueued = true; requestAnimationFrame(() => { drawQueued = false; drawMarkers(); }); } };
  new MutationObserver((ms) => {
    if (ms.some((m) => !isOurs(m.target.nodeType === 1 ? m.target : m.target.parentElement) && (m.type !== "childList" || ![...m.addedNodes, ...m.removedNodes].every((n) => n.nodeType === 1 && isOurs(n))))) queueDraw();
  }).observe(document.documentElement, { childList: true, subtree: true, attributes: true, characterData: true });
  for (const type of ["resize", "transitionend", "animationend"]) addEventListener(type, queueDraw);

  let opening = 0, openingMessage = null;
  function showOpeningMessage(text) {
    openingMessage?.remove();
    openingMessage = document.createElement("div");
    openingMessage.className = "jf-ask";
    openingMessage.setAttribute("role", "alert");
    openingMessage.tabIndex = -1;
    openingMessage.textContent = text;
    openingMessage.style.cssText = "display:block;padding:16px;background:#fff;color:#1b2221;border:2px solid #b26a12;font:16px system-ui";
    document.body.prepend(openingMessage);
    openingMessage.scrollIntoView({ block: "center" });
    openingMessage.focus();
  }
  async function openAttention(id) {
    const request = ++opening;
    let item = attention.find((a) => a.id === id);
    if (!item) {
      try {
        await refreshThreads();
      } catch (e) {
        if (request === opening) showOpeningMessage("Could not load this question. Open it from the dock to try again.");
        console.error(e);
        return;
      }
      if (request !== opening) return;
      item = attention.find((a) => a.id === id);
    }
    openingMessage?.remove();
    if (!item || item.withdrawn) {
      showOpeningMessage("This question is no longer waiting for an answer.");
      return;
    }
    for (const fn of attendSubs) { try { fn(item); } catch (e) { console.error(e); } }
    drawAttention();
    const host = inline() ? anchorElement(item) : cards.get(id);
    if (!host) return;
    const root = host.shadowRoot || host;
    let control = root.querySelector("button.o:not(:disabled), [data-pick]") || root.querySelector("textarea:not(:disabled)") || root.querySelector("button:not(:disabled)") || root.querySelector(".c") || host;
    host.scrollIntoView({ block: inline() ? "start" : "center" });
    if (inline()) {
      const r = control.getBoundingClientRect();
      if (r.top < 0 || r.bottom > innerHeight || r.left < 0 || r.right > innerWidth) {
        control = root.querySelector("h1, h2, h3") || host;
        control.tabIndex = -1;
      }
    }
    if (!control.hasAttribute("tabindex") && control === host) control.tabIndex = -1;
    control.focus({ preventScroll: true });
    if (!inline()) control.scrollIntoView({ block: "nearest" });
    const c = root.querySelector(".c");
    if (c) { c.classList.remove("flash"); void c.offsetWidth; c.classList.add("flash"); }
  }
  addEventListener("message", (e) => {
    if (e.origin === location.origin && e.data?.type === "jelly:attend") openAttention(e.data.id).catch(console.error);
  });

  // A page that shows its own questions (the ask page) opts out:
  // <meta name="jelly-attention" content="inline">.
  const inline = () => document.querySelector('meta[name="jelly-attention"]')?.content === "inline";
  // Alone in a tab, a version view says what it is and offers the way back; on
  // the jelly page the page's own bar does.
  function drawVersionBanner() {
    const ago = (ms) => { const s = Math.max(0, (Date.now() - ms) / 1000); return s < 60 ? "just now" : s < 3600 ? `${Math.floor(s / 60)}m ago` : s < 86400 ? `${Math.floor(s / 3600)}h ago` : `${Math.floor(s / 86400)}d ago`; };
    const bar = document.createElement("div");
    bar.className = "jf-version";
    bar.innerHTML = '<span></span><button class="jf-live">Back to live</button><button class="jf-restore"></button>';
    const who = readOnly.by === "person" ? "you" : readOnly.by;
    bar.querySelector("span").textContent = `Version ${readOnly.version} of ${boot.name} · read only · ${who} · ${ago(readOnly.at_ms)}${readOnly.kept ? ` · "${readOnly.kept}"` : ""}`;
    bar.querySelector(".jf-restore").textContent = `Restore v${readOnly.version}`;
    const live = `/j/${enc(boot.name)}/`;
    bar.querySelector(".jf-live").onclick = () => { location.href = live; };
    bar.querySelector(".jf-restore").onclick = async () => {
      const r = await fetch(`${api}/history/${readOnly.version}/restore`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ by: "person" }) });
      if (!r.ok) return alert((await r.json()).error || r.statusText);
      location.href = live;
    };
    document.body.appendChild(bar);
  }

  let workEl = null;
  function drawWorking() {
    workEl?.remove();
    workEl = null;
    if (!working || !document.body) return;
    workEl = document.createElement("div");
    workEl.className = "jf-work";
    workEl.innerHTML = "<i></i><span></span>";
    workEl.querySelector("span").textContent = `${working.by} is working on this${working.what ? ": " + working.what : ""}`;
    document.body.appendChild(workEl);
  }

  // ---- agent view: what is on screen, as text, for `jelly see` ----------------
  const LEAF = new Set(["button", "a", "input", "textarea", "select", "img", "canvas", "svg", "iframe", "video", "p", "li", "pre", "h1", "h2", "h3", "h4", "h5", "h6", "label", "td", "th", "figcaption", "blockquote", "summary"]);
  const GROUP = new Set(["section", "article", "nav", "header", "footer", "main", "aside", "form", "table", "ul", "ol", "dialog", "details", "figure"]);
  function agentSnapshot() {
    const lines = [];
    const label = (el) => (el.getAttribute("aria-label") || el.innerText || el.value || el.alt || el.title || "").trim().replace(/\s+/g, " ").slice(0, 120);
    const shown = (el) => { const r = el.getBoundingClientRect(); const cs = getComputedStyle(el); return r.width > 0 && r.height > 0 && cs.visibility !== "hidden" && cs.display !== "none" && cs.opacity !== "0"; };
    const add = (depth, text, el) => lines.push(`${"  ".repeat(depth)}- ${text}${el.id ? ` #${el.id}` : ""}`);
    (function walk(el, depth) {
      for (const c of el.children) {
        if (lines.length > 400 || c.matches(OURS) || ["SCRIPT", "STYLE", "NOSCRIPT", "TEMPLATE", "LINK", "META"].includes(c.tagName) || !shown(c)) continue;
        const tag = c.tagName.toLowerCase(), role = c.getAttribute("role");
        if (/^h[1-6]$/.test(tag)) add(depth, `heading "${label(c)}" [${tag}]`, c);
        else if (tag === "button" || role === "button") add(depth, `button "${label(c)}"${c.disabled ? " (disabled)" : ""}`, c);
        else if (tag === "a") add(depth, `link "${label(c)}"`, c);
        else if (tag === "input" && (c.type === "checkbox" || c.type === "radio")) add(depth, `${c.type} "${label(c.closest("label") || c)}" ${c.checked ? "checked" : "unchecked"}`, c);
        else if (tag === "input" || tag === "textarea") add(depth, `${tag === "textarea" || !c.type || c.type === "text" ? "textbox" : c.type}${c.placeholder ? ` "${c.placeholder}"` : ""} = "${String(c.value).slice(0, 80)}"`, c);
        else if (tag === "select") add(depth, `select = "${c.options[c.selectedIndex]?.text || ""}"`, c);
        else if (tag === "img") add(depth, `img "${c.alt || ""}"`, c);
        else if (["canvas", "svg", "video", "iframe"].includes(tag)) { const r = c.getBoundingClientRect(); add(depth, `${tag} ${Math.round(r.width)}×${Math.round(r.height)}${c.title ? ` "${c.title}"` : ""}${tag === "svg" && label(c) ? ` "${label(c).slice(0, 60)}"` : ""}`, c); }
        else if (LEAF.has(tag)) { const t = label(c); if (t) add(depth, `text "${t}"`, c); }
        else if (c.id && c.children.length === 0) { const t = label(c); if (t) add(depth, `text "${t}"`, c); }
        else if ((GROUP.has(tag) || role || c.id) && c.innerText?.trim()) { add(depth, `${role || tag}${c.getAttribute("aria-label") ? ` "${c.getAttribute("aria-label")}"` : ""}:`, c); walk(c, depth + 1); continue; }
        else if (c.children.length === 0) { const t = label(c); if (t) add(depth, `text "${t}"`, c); }
        else walk(c, depth);
      }
    })(document.body, 0);
    return lines.join("\n");
  }
  // At most one post per interval, so a page that changes all the time still reports.
  // A version view has no owner to post with; it reports nothing.
  function postSnapshot() {
    if (snapT || readOnly) return;
    snapT = setTimeout(() => {
      snapT = null;
      fetch(`${api}/screen`, { method: "POST", headers: { "content-type": "application/json" },
        body: JSON.stringify({ owner: boot.snapshotOwner, text: agentSnapshot(), width: innerWidth, height: innerHeight, version, errors,
          inline: inline(), page_height: document.documentElement.scrollHeight, below: anchorsThat("direct"), invalid: anchorsThat("invalid"), missing: anchorsThat("missing"), floating: anchorsThat("floating"), hidden: anchorsThat("hidden"),
          exposed: [...cards].filter(([, h]) => h.hasAttribute("data-exposed") && available(h, true)).map(([id]) => id),
          fallback: attention.filter(isOpen).filter((a) => outcomes.get(a.id)?.status === "fallback").map((a) => ({ id: a.id, destination: outcomes.get(a.id).destination, reason: outcomes.get(a.id).reason })) }) }).then((r) => { if (r.status === 409) location.reload(); }).catch(() => {});
    }, agentMode ? 400 : 1200);
  }
  function anchorsThat(kind) {
    return attention.filter((a) => isOpen(a) && typeof a.anchor?.selector === "string" && outcomes.get(a.id)?.status === kind).map((a) => `${a.id} ${a.anchor.selector}`);
  }
  addEventListener("load", postSnapshot);
  subs.push(() => postSnapshot());
  // Text a page fills in later (fetches, timers) counts too.
  new MutationObserver(() => postSnapshot()).observe(document.documentElement, { childList: true, subtree: true, characterData: true });

  let refreshingAttention = false;
  let latestRefresh;
  function refreshThreads() {
    refreshingAttention = true;
    outcomes.clear();
    const refresh = (async () => {
      let j;
      try {
        const r = await fetch(api);
        if (!r.ok) throw new Error(`Could not refresh questions (${r.status})`);
        j = await r.json();
      } catch (e) {
        if (refresh !== latestRefresh) return latestRefresh;
        throw e;
      }
      if (refresh !== latestRefresh) return latestRefresh;
      refreshingAttention = false;
      comments = j.comments || []; attention = j.attention || []; working = j.working || null;
      drawMarkers(); drawWorking();
      for (const fn of attentionSubs) { try { fn(); } catch (e) { console.error(e); } }
    })();
    latestRefresh = refresh;
    return refresh;
  }

  addEventListener("DOMContentLoaded", () => {
    const style = document.createElement("style");
    style.textContent = css;
    document.head.appendChild(style);
    // A view that draws its own comment control opts out with <meta name="jelly-comment" content="off">.
    const optOut = document.querySelector('meta[name="jelly-comment"]')?.content === "off";
    if (!embedded && !optOut && !readOnly) document.body.appendChild(btn);
    if (readOnly && !embedded) drawVersionBanner();
    drawMarkers();
    drawWorking();
    new ResizeObserver(queueDraw).observe(document.body);
    addEventListener("resize", queueDraw);
  });
  subs.push(() => setTimeout(drawMarkers, 0));
  btn.onclick = (e) => { e.stopPropagation(); setMode(!commenting); };
  addEventListener("keydown", (e) => {
    if (e.key !== "Escape") return;
    // Esc inside a jelly closes the comment mode first, then reaches the jelly page.
    const inPop = e.target.closest?.(".jf-pop");
    if (embedded && !commenting && !inPop && !pop?.isConnected) parent.postMessage({ type: "jelly:escape", name: boot.name }, location.origin);
    setMode(false);
  });
  addEventListener("mouseover", (e) => {
    if (!(commenting || e.altKey) || isOurs(e.target)) return;
    hovered?.classList.remove("jf-hover");
    hovered = e.target;
    hovered.classList.add("jf-hover");
  });
  addEventListener("click", (e) => {
    if (!(commenting || e.altKey) || isOurs(e.target)) return;
    e.preventDefault();
    e.stopPropagation();
    e.target.classList.remove("jf-hover");
    setMode(false);
    startComment(e.target, e.clientX, e.clientY);
  }, true);
})();
