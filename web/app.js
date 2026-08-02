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
  thoughtId: null,
  tools: new Map(),     // toolCallId → elements {card, pre, dot}
  usageTotal: null,
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
function scrollBottom() {
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
    try { m = JSON.parse(ev.data); } catch { return; }
    if (m.id !== undefined && m.id !== null && pending.has(m.id)) {
      const h = pending.get(m.id);
      pending.delete(m.id);
      h(m.result !== undefined ? { ok: true, result: m.result } : { ok: false, error: m.error || {} });
      return;
    }
    if (m.method === "session/update") { onSessionUpdate(m.params); return; }
    if (m.method === "session/request_permission") { onPermission(m.params, m.id); return; }
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
  turn = { assistant: null, assistantId: null, thought: null, thoughtId: null,
           tools: new Map(), usageTotal: null, promptId: rpcId };
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

function streamText(u, isThought) {
  const text = textOf(u.content);
  if (isThought) {
    if (!turn.thoughtId || u.messageId !== turn.thoughtId) {
      const t = document.createElement("div");
      t.className = "thought";
      turn.thought = t;
      turn.thoughtId = u.messageId;
      log.appendChild(t);
    }
    turn.thought.textContent += text;
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
}

function streamTool(u) {
  const id = u.toolCallId;
  let t = turn.tools.get(id);
  if (!t) {
    const card = document.createElement("details");
    card.className = "tool";
    const sum = document.createElement("summary");
    const dot = document.createElement("span");
    dot.className = "dot";
    const name = document.createElement("span");
    name.className = "name";
    sum.appendChild(dot);
    sum.appendChild(name);
    const pre = document.createElement("pre");
    card.appendChild(sum);
    card.appendChild(pre);
    log.appendChild(card);
    t = { card, pre, dot, name, status: "" };
    turn.tools.set(id, t);
    t.card.open = true; // auto-open streaming tool cards
  }
  if (u.title) t.name.textContent = u.title;
  t.status = u.status || t.status;
  t.dot.className = "dot " + (t.status || "pending");
  if (u.content && u.content.length) {
    const text = u.content.map((c) => textOf(c)).join("");
    t.pre.textContent = text;
  }
  if (u.status === "completed" || u.status === "error") {
    if (t.pre.textContent.trim().length > 0) t.card.open = false;
  }
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
  // A cwd change applies to the NEXT session; close the current one so a
  // fresh session starts in the new workspace on the following prompt.
  if (sessionId) {
    send("session/close", { sessionId }, () => {});
    sessionId = null;
    $("session").textContent = "(workspace will change)";
    $("send").disabled = true;
  }
});

// Start.
connect();
