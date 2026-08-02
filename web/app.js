// ri web client — Agent Client Protocol v2 (only) over WebSocket.
//
// Talks the ACP v2 (unstable) surface at ws://<host>/acp:
//   initialize(protocolVersion:2) → session/new → session/prompt
// with streaming `session/update` notifications, `session/request_permission`
// requests, and `session/cancel` for interruption. Only v2 is spoken here —
// the web UI does not fall back to protocol v1.

"use strict";

const $ = (id) => document.getElementById(id);

let ws = null;
let rpcId = 0;
let sessionId = null;
let running = false;            // a prompt turn is streaming
let model = "…";
let reconnectTimer = null;

// Streams cache: per-turn messageId → current message element.
let turn = {
  assistant: null,      // last assistant bubble Node
  assistantId: null,
  thought: null,
  usageTotal: null,     // usage tokens of the in-flight turn
  promptId: null,       // JSON-RPC id of the in-flight prompt request
};

const log = $("log");
function appendQuery(klass, node) {
  const div = document.createElement("div");
  div.className = "msg " + klass;
  div.appendChild(node);
  log.appendChild(div);
  scrollBottom();
  return div;
}
function bubbleNode() {
  const b = document.createElement("div");
  b.className = "bubble";
  return b;
}
// Auto-stick to the bottom only while the user is already near the bottom;
// the moment they scroll up (to read a tool card / earlier message) we stop
// yanking them back down.
let stickToBottom = true;
log.addEventListener("scroll", () => {
  const atBottom = log.scrollHeight - log.scrollTop - log.clientHeight < 80;
  if (atBottom) stickToBottom = true;
  else if (log.scrollTop < log.scrollHeight - log.clientHeight - 160) stickToBottom = false;
}, { passive: true });

function scrollBottom() {
  if (stickToBottom) log.scrollTop = log.scrollHeight;
}
function forceScrollBottom() {
  stickToBottom = true;
  log.scrollTop = log.scrollHeight;
}
function setStatus(text, good) {
  const s = $("status");
  s.textContent = text;
  s.className = "pill " + (good ? "good" : "bad");
}

// ── JSON-RPC plumbing ────────────────────────────────────────────────────────
const pending = new Map(); // id → handler(result | error)
let nextId = () => ++rpcId;

function send(method, params, handler) {
  const id = nextId();
  if (handler) pending.set(id, handler);
  const msg = { jsonrpc: "2.0", id, method, params };
  ws.send(JSON.stringify(msg));
}

function sendNotify(method, params) {
  const msg = { jsonrpc: "2.0", method, params };
  ws.send(JSON.stringify(msg));
}

// ── connection lifecycle ─────────────────────────────────────────────────────
function wsUrl() {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  return `${proto}//${location.host}/acp`;
}

function connect() {
  setStatus("connecting…", false);
  try {
    ws = new WebSocket(wsUrl());
  } catch (e) {
    scheduleReconnect();
    return;
  }
  ws.onopen = () => {
    setStatus("connected", true);
    $("proto").textContent = "ACP v2";
    send("initialize",
      { protocolVersion: 2, info: { name: "ri-web", version: "1" }, capabilities: {} },
      (res, err) => {
        if (err) {
          setStatus("initialize failed: " + err.message, false);
          return;
        }
        model = (res.info && res.info.name) || "ri-agent";
        if (res.capabilities && res.capabilities.session) {
          setStatus("connected · server " + model, true);
        }
        $("model").textContent = model;
        newSession();
      });
  };
  ws.onmessage = (ev) => {
    let m;
    try {
      m = JSON.parse(ev.data);
      if (m.id !== undefined && m.id !== null && pending.has(m.id)) {
        const h = pending.get(m.id);
        pending.delete(m.id);
        h(m.result !== undefined ? { ok: true, result: m.result } : { ok: false, error: m.error || {} });
        return;
      }
      if (m.method === "session/update") { onSessionUpdate(m.params); return; }
      if (m.method === "session/request_permission") { onPermission(m.params, m.id); return; }
    } catch (e) {
      // Never let a UI bug swallow transport messages silently: surface it.
      const d = document.createElement("div");
      d.className = "error-line";
      d.textContent = "[ui] " + (e && e.message ? e.message : String(e));
      log.appendChild(d);
      scrollBottom();
    }
  };
  ws.onclose = () => {
    setStatus("disconnected", false);
    if (running) finishTurn();
    sessionId = null;
    $("session").textContent = "no session";
    scheduleReconnect();
  };
  ws.onerror = () => { try { ws.close(); } catch {} };
}

function scheduleReconnect() {
  if (reconnectTimer) return;
  setStatus("reconnecting…", false);
  reconnectTimer = setTimeout(() => {
    reconnectTimer = null;
    connect();
  }, 2000);
}

function newSession() {
  const cwd = $("cwd").value.trim() || "/";
  send("session/new", { cwd }, (res, err) => {
    if (err) {
      setStatus("session/new failed: " + (err.message || err.data || "?"), false);
      return;
    }
    sessionId = res.result.sessionId;
    $("session").textContent = sessionId;
    setStatus("connected · " + (model || "ri-agent"), true);
    appendLine("conn", "session " + sessionId + " · workspace " + cwd);
    $("send").disabled = false;
  });
}

// ── prompt ───────────────────────────────────────────────────────────────────
function sendPrompt() {
  const input = $("input");
  const text = input.value.trim();
  if (!text || running || !ws || ws.readyState !== 1 || !sessionId) return;
  input.value = "";
  input.style.height = "auto";

  // echo the user message
  const ub = bubbleNode();
  ub.textContent = text;
  appendQuery("user", ub);

  forceScrollBottom();
  beginTurn();
  send("session/prompt", { sessionId, prompt: [{ type: "text", text }] }, (res, err) => {
    if (err) {
      const e = document.createElement("div");
      e.className = "error-line";
      e.textContent = "prompt error: " + (err.message || err.data || JSON.stringify(err));
      appendQuery("assistant", e);
      finishTurn();
      return;
    }
    finishTurn();
  });
}

function beginTurn() {
  running = true;
  turn = { assistant: null, assistantId: null, thought: null,
           tools: new Map(), usageTotal: null, promptId: rpcId };
  forceScrollBottom();
  $("send").disabled = true;
  $("stop").style.display = "";
}

function finishTurn() {
  running = false;
  if (turn.assistant && !turn.assistant.classList.contains("streaming")) {
    turn.assistant.classList.remove("streaming");
  }
  renderUsage();
  $("send").disabled = false;
  $("stop").style.display = "none";
  scrollBottom();
}

function stopTurn() {
  if (!running || !sessionId) return;
  sendNotify("session/cancel", { sessionId });
  // The server aborts the turn; its result/error closes it out.
}

// ── session/update streaming ─────────────────────────────────────────────────
function onSessionUpdate(params) {
  const u = params.update;
  switch (u.sessionUpdate) {
    case "agent_message_chunk":
      streamText(u, false);
      break;
    case "agent_thought_chunk":
      streamText(u, true);
      break;
    case "user_message_chunk":
      // Echo of a user prompt (usually the one we optimistically rendered).
      break;
    case "tool_call_update":
      streamTool(u);
      break;
    case "usage_update":
      turn.usageTotal = u;
      renderUsage();
      break;
  }
  scrollBottom();
}

function textOf(contentBlock) {
  // {type:"text", text:"..."} (also survives {content:{...}} wraps)
  if (!contentBlock) return "";
  if (typeof contentBlock === "string") return contentBlock;
  let c = contentBlock;
  while (c && c.content && typeof c.content === "object" && c.type === "content") {
    c = c.content;
  }
  if (c.type === "text") return c.text || "";
  if (c.type === "resource") {
    const r = c.resource || {};
    const t = r.contents || [];
    return t.map((x) => x.text || "").join("\n");
  }
  return "";
}

// A turn shares ONE messageId across all its agent text/thoughts (the server
// emits them under a single turn-scoped message), so bursts cannot be told
// apart by id. Heuristic: when thinking arrives and the current thought block
// is NOT the last element anymore (a tool card / text came in between), open a
// fresh thought block right below — otherwise append to the running one. This
// keeps a thinking burst after tool calls BELOW the tool cards.
function currentThought() {
  if (!turn.thought || log.lastElementChild !== turn.thought) {
    const t = document.createElement("div");
    t.className = "thought";
    turn.thought = t;
    log.appendChild(t);
  }
  return turn.thought;
}

function streamText(u, isThought) {
  const text = textOf(u.content);
  if (isThought) {
    currentThought().textContent += text;
    scrollBottom();
    return;
  }
  if (!turn.assistant || u.messageId !== turn.assistantId) {
    const b = bubbleNode();
    b.classList.add("streaming");
    turn.assistant = b;
    turn.assistantId = u.messageId;
    appendQuery("assistant", b);
  }
  turn.assistant.textContent += text;
  if (!running) turn.assistant.classList.remove("streaming");
  scrollBottom();
}

const TOOL_STATUS = {
  pending: "hazırlanıyor…", in_progress: "çalışıyor…", completed: "tamam",
  error: "hata",
};
function streamTool(u) {
  const id = u.toolCallId;
  let t = turn.tools.get(id);
  if (!t) {
    const card = document.createElement("details");
    card.className = "tool";
    const sum = document.createElement("summary");
    const dot = document.createElement("span");
    dot.className = "dot";
    const tag = document.createElement("span");
    tag.className = "tag";
    tag.textContent = "tool";
    const name = document.createElement("span");
    name.className = "name";
    const chip = document.createElement("span");
    chip.className = "chip";
    const pre = document.createElement("pre");
    sum.appendChild(dot);
    sum.appendChild(tag);
    sum.appendChild(name);
    sum.appendChild(chip);
    card.appendChild(sum);
    card.appendChild(pre);
    log.appendChild(card);
    t = { card, pre, dot, chip, name, status: "" };
    turn.tools.set(id, t);
    // Keep tool cards OPEN so tool work is always visible; the user may
    // collapse them manually.
    t.card.open = true;
  }
  if (u.title) t.name.textContent = u.title;
  t.status = u.status || t.status;
  t.dot.className = "dot " + (t.status || "pending");
  t.chip.textContent = TOOL_STATUS[t.status] || t.status;
  t.chip.className = "chip " + (t.status || "pending");
  if (u.content && u.content.length) {
    const text = u.content.map((c) => textOf(c)).join("");
    t.pre.textContent = text;
  }
  scrollBottom();
}

function renderUsage() {
  const u = turn.usageTotal;
  if (!u) return;
  const part = u.partial !== undefined ? u.partial : u.totalTokens;
  const total = u.totalTokens !== undefined ? u.totalTokens : u.total;
  $("usage").textContent = (part !== undefined && total !== undefined)
    ? `${part} / ${total} tokens` : "";
}

// ── session/request_permission ───────────────────────────────────────────────
// The response must ECHO the request's JSON-RPC id (the agent is waiting on a
// request it sent us), so we bypass the auto-increment `send` helper here.
function answerPermission(reqId, outcome) {
  ws.send(JSON.stringify({ jsonrpc: "2.0", id: reqId,
                           result: { outcome } }));
}

function onPermission(params, reqId) {
  const opts = params.options || [];

  if ($("autoAllow").checked && opts.length > 0) {
    // Auto-approve the first option ("continue" for ordinary tool calls).
    const first = opts.find((o) => o.optionId === "continue") || opts[0];
    $("perm").style.display = "none";
    answerPermission(reqId, { selected: { optionId: first.optionId } });
    return;
  }

  $("permTitle").textContent = params.title || "Permission requested";
  const wrap = $("permOpts");
  wrap.textContent = "";
  for (const o of opts || []) {
    const b = document.createElement("button");
    b.className = o.optionId === "continue" ? "primary" : "";
    const il8n = { allow: "Allow", allow_once: "Allow once",
                   "allow-for-session": "Allow this session",
                   "allow-for-project": "Allow this project",
                   continue: "Continue", deny: "Deny", "ask-what-you-mean": "Ask" };
    b.textContent = il8n[o.name] || o.name || o.optionId;
    b.onclick = () => {
      $("perm").style.display = "none";
      answerPermission(reqId, { selected: { optionId: o.optionId } });
    };
    wrap.appendChild(b);
  }
  const cancelBtn = document.createElement("button");
  cancelBtn.textContent = "Cancel";
  cancelBtn.style.borderColor = "var(--error)";
  cancelBtn.onclick = () => {
    $("perm").style.display = "none";
    answerPermission(reqId, "cancelled");
  };
  wrap.appendChild(cancelBtn);
  $("perm").style.display = "";
}

function appendLine(klass, text) {
  const d = document.createElement("div");
  d.className = "conn-line " + klass;
  d.textContent = text;
  log.appendChild(d);
  scrollBottom();
}

// ── ui wiring ────────────────────────────────────────────────────────────────
const input = $("input");
input.addEventListener("keydown", (e) => {
  if (e.key === "Enter" && !e.shiftKey) {
    e.preventDefault();
    sendPrompt();
  }
});
input.addEventListener("input", () => {
  input.style.height = "auto";
  input.style.height = Math.min(input.scrollHeight, 180) + "px";
});
$("send").addEventListener("click", sendPrompt);
$("stop").addEventListener("click", stopTurn);
$("cwd").addEventListener("change", () => {
  // The workspace applies to the (next) session: close the current one and
  // immediately open a fresh session in the new workspace so the input never
  // gets stuck in a session-less state.
  if (sessionId) {
    send("session/close", { sessionId }, () => {});
    sessionId = null;
    $("session").textContent = "(workspace değişiyor)";
    $("send").disabled = true;
    setTimeout(newSession, 150);
  } else {
    newSession();
  }
});

// Start.
connect();
