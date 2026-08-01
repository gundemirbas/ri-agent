# Architecture

> See [GLOSSARY.md](GLOSSARY.md) for definitions of *session*, *agent loop*,
> *turn*, *LLM invocation*, *tool call*, *steering message*, *compaction*,
> and *projection*.

## Purpose

`ri-agent` is a terminal AI agent harness. It provides a streaming TUI for
conversational interaction with an OpenAI-compatible LLM endpoint and runs the
full agentic loop: user message → model response → tool call → tool result →
model continues, until the model returns a final answer without tool calls.

The backend surface is deliberately single: one OpenAI-compatible provider
(`rig`-powered) plus a hidden `test` provider used to exercise the UI.

## Module Map

```
src/
  main.rs              — tokio entry point, CLI parsing, outer provider loop
  app.rs               — App state, event handling, submission, scroll
  app_interaction.rs   — App methods that manipulate UI state (selection,
                         completion, thinking, provider pickers)
  app_submission.rs    — submit pipeline (user text → session events → agent loop)
  app_agent_handlers.rs— handling of AgentEvent → session/live-turn updates
  app_event.rs         — AppEvent enum (agent events + UI events over mpsc)
  input.rs             — keyboard routing by input mode
  keybindings.rs       — keybinding catalog and matching
  ui.rs (+ ui/)        — all ratatui rendering, pre-wrapping, scroll logic
  theme.rs             — Theme struct + theme.toml loading
  markdown.rs          — markdown → ratatui Lines renderer
  mouse_select.rs      — click-drag text selection and copy in the log view
  commands/mod.rs      — slash-command registry (COMMANDS, SlashCommand, parse)
  completion.rs        — CompletionItem and completions_for (completion popup)
  completion_state.rs  — CompletionState sub-struct (popup + model-fetch state)
  selection_state.rs   — SelectionState/SelectionKind menu picker state
  ask_user_state.rs    — AskUserState/PendingAsk (agent ask-user bridge to TUI)
  agent_turn_state.rs  — AgentTurnState (streaming status/throbber fields)
  shell_state.rs       — shell-mode textarea state (bash only)
  step_back_state.rs   — step-back/step-forward navigation state
  log_view_state.rs    — LogCache/LogViewState (wrapped-line cache, scroll)
  agents.rs            — user-definable agent profiles (SYSTEM.md/AGENTS.md,
                         tool/skill filtering)
  skills.rs            — SKILL.md loading and /skill: expansion
  config.rs            — config.toml loading/saving (XDG + HOME fallback)
  provider.rs          — provider routing, thinking support
  provider_instance.rs — ProviderInstance type and preset metadata
  provider_manager.rs  — provider picker/setup/removal state machine
  provider_setup.rs    — UnavailableProvider sentinel + default resolution
  thinking.rs          — ThinkingLevel (off/minimal/low/…/xhigh)
  session.rs           — SessionStore (file-backed JSONL session storage)
  session_manager.rs   — SessionManager (App-owned session state bundle)
  event_log.rs         — append-only durable session event log (JSONL)
  session_event.rs     — durable committed conversation/domain event types
  session_state.rs     — committed session owner: EventLog + display/LLM models
  live_turn.rs         — transient in-flight assistant/tool state for one turn
  projection.rs        — pure/incremental projections from event history
  context_window.rs    — per-model context-window table and token budgets
  export.rs            — /export: self-contained HTML session export
  tool_presentation.rs — tool call/result rendering helpers for the TUI
  at_file.rs           — @file argument expansion
  clipboard.rs         — OSC 52 clipboard set
  atomic_file.rs       — atomic file writes
  dirs.rs              — shared ProjectDirs + --print-dirs
  debug_log.rs         — debug logging to ~/.cache/ri
  tracked.rs           — Tracked<T> (change-tracking wrapper)
  process.rs           — subprocess detach-from-tty plumbing
  print_mode.rs        — non-interactive -p/--print mode
  agent/
    mod.rs             — run_agent_loop: the multi-turn agentic loop
    types.rs           — Tool/ToolExecutor traits, AgentEvent, AgentLoopConfig
    system_prompt.rs   — build_system_prompt (dynamic, agent-aware)
    file_tracker.rs    — FileTracker: external-change detection + diff
    compaction.rs      — context compaction (summary generation, triggers)
    tools/
      mod.rs           — register_builtin_tools (built-ins + custom tools)
      bash.rs          — BashTool (💻 run shell command)
      read.rs          — ReadFileTool (👀 read file with offset/limit)
      write.rs         — WriteTool (✏️ write/overwrite file)
      edit.rs          — EditTool  (📝 replace exact text in file)
      find.rs          — FindTool  (🔍 search by name glob or content pattern)
      ask_user.rs      — AskUserTool (❓ interactive question to the user)
      read_skill.rs    — ReadSkillTool (load a SKILL.md body)
      exec.rs          — exec-path resolution helpers
      subprocess.rs    — SubprocessCommand (shared run/stream/cancel logic)
      truncate.rs      — output truncation (TruncationResult)
      utf8.rs          — UTF-8/binary handling for file tools
      terminal.rs      — apply_terminal_render (\r emulation for bash output)
      custom.rs        — CustomTool, load_custom_tools, custom_tool_dirs
  llm/
    mod.rs             — LlmProvider trait, Message/Role/LlmEvent/ToolDefinition
    error.rs           — ProviderError/ProviderErrorKind (typed failures)
    rig_provider.rs    — RigOpenAiProvider (OpenAI Responses + Completions via rig)
    test_provider.rs   — TestProvider (hidden UI/system-prompt exercise provider)
```

## Data Flow

```
User keystroke → input.rs → App::submit
  └─ ensure SessionState exists (create/resume persisted session)
     └─ append UserMessage event via SessionState ingestion
        └─ spawns tokio task: run_agent_loop(config, provider, tx, steering_rx, cancel_rx)
             instrumented by AppEvent::Agent events
             for each turn:
               check FileTracker for externally modified files
                 └─ if any: inject ⚠️ user message with unified diff
               provider.stream(messages, tool_defs)
                 └─ yields LlmEvent::{ThinkingToken, Token{phase}, Usage,
                                      ToolCall{args}, Done, Error}
               if ToolCall → executor.execute_tool → ToolResult (+ live chunks)
               loop until no tool calls
               sends AgentEvent::{TextToken, ThinkingToken, Usage, …,
                                  ToolCallStart, ToolCallEnd, TurnEnd,
                                  Done, Error} on tx

User keystroke (while streaming) → App::enqueue_steering_from_input
  └─ pushes text onto queued_steering + sends on steering_tx
     (consumed at the next turn boundary)

App::apply_event drains tx on each draw tick
  ├─ committed events → SessionState ingestion (EventLog + projections)
  └─ transient streaming/tool/notices → LiveTurnState
      ui::draw renders committed SessionState display + LiveTurnState overlay

LLM input construction
  └─ system prompt (agent-aware) + SessionState committed LLM projection only
```

## Key Types

### `llm/mod.rs`

```rust
pub enum AssistantPhase { Unknown, Provisional, Final }
pub struct UsageStats { input/output/total/cached tokens, used_tokens() }

pub struct Message {
    pub role: Role,                       // System|User|Assistant|ToolCall|ToolResult
    pub content: String,
    pub thinking: Option<String>,         // chain-of-thought block
    pub assistant_phase: Option<AssistantPhase>,
    pub hidden: bool,                     // persisted + sent but not rendered
    pub include_in_llm: bool,
    pub tool_call_id / tool_name / tool_args,
    pub is_error: bool,
    pub display_range: Option<DisplayRange>,   // partial read_file windows
    pub image_data: Option<ImageData>,         // binary image tool results
}

pub enum LlmEvent {
    ThinkingToken(String),
    Token { text: String, phase: AssistantPhase },
    Usage(UsageStats),
    ToolCallStart { id, name },
    ToolCallArgsDelta { id, partial_json },
    ToolCall { id, name, args },
    Done,
    Error(ProviderError),
}

pub trait LlmProvider: Send + Sync {
    fn stream(&self, messages: Vec<Message>, tools: Vec<ToolDefinition>) -> LlmStream;
    fn list_models(&self) -> ModelListFuture;   // default: []
}
```

### `agent/types.rs`

```rust
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> serde_json::Value;
    fn streaming_field(&self) -> Option<&'static str> { None }
    fn run(&self, args, ctx: ToolCallContext) -> Future<Output = ToolResult>;
}

pub trait ToolExecutor: Send + Sync {
    fn execute_tool(&self, id, name, args, tools, log, tx) -> Future<Output = ToolResult>;
}

pub struct AgentLoopConfig {
    pub tools: ToolRegistry,
    pub file_tracker: Arc<Mutex<FileTracker>>,
    pub tool_output_log: Arc<Mutex<ToolOutputLog>>,
    pub executor: Arc<dyn ToolExecutor>,
    pub session_events: Vec<SessionEvent>,         // compaction decisions
    pub current_model: String,
    pub auto_compaction_enabled: bool,
    pub manual_compaction_instructions: Option<String>,
    pub system_prompt: Option<String>,
}
```

## Key Design Decisions

**Pre-wrapping instead of ratatui Wrap** — ratatui's `Wrap` widget wraps at
render time and cannot easily pad individual lines to full width. By
pre-wrapping in `build_log_lines` we know the exact row count before
rendering, which makes scroll arithmetic exact and lets us apply per-row
background styles (e.g. the grey user-message highlight).

**Channel-based LLM events** — `tokio::mpsc` decouples the async HTTP
streaming task from the synchronous draw loop. The draw loop never awaits;
it drains the channel non-blockingly on each tick, keeping the TUI
responsive during long model responses.

**Steering queue during streaming** — while a loop is active, Enter enqueues
user steering text into a dedicated channel. The UI renders queued entries at
the bottom with `🕹️` until the loop consumes them at the next turn boundary.
On consumption, a `SteeringConsumed` event removes the pinned row and inserts
the message into normal transcript order. Already-emitted tool calls in the
current turn are allowed to finish; steering does not cancel them.

**Progressive cancellation** — `CancelLevel` (`None < SoftStop < HardAbort <
ForceKill`) is shared via a `tokio::sync::watch` channel. The agent loop checks
it at turn boundaries; subprocess tools poll it mid-execution. `SoftStop`
finishes the current turn (model response + tool batch) then exits; `HardAbort`
aborts the model request and SIGTERMs the subprocess; `ForceKill` SIGKILLs it.

**`LlmProvider` trait** — all provider wire formats are contained in
`llm/rig_provider.rs`. The agent loop, `app.rs`, and `ui` are
provider-agnostic. `provider.rs` maps a `ProviderInstance` (id, base_url,
api_key, model, api_type) to a `RigOpenAiProvider` over either the OpenAI
Responses protocol or the Chat-Completions protocol.

**Typed provider errors** — `LlmEvent::Error`, `AgentEvent::Error`, and
`ModelListFuture` carry `ProviderError` (with `ProviderErrorKind`). HTTP status
is mapped in `llm/error.rs`: 401→`Unauthorized`, 403→`Forbidden`, 429→
`RateLimited`, 5xx→`ServerError`, network failures→`Network`. User-facing
wording is composed later in `app.rs` using the active provider label, so
OpenAI-compatible transports do not surface as `OpenAI` for backends such as
Open WebUI.

**Display-only sanitization** — message content is stored and sent to the
LLM verbatim. Trailing whitespace per line, leading/trailing newlines, and
excess blank-line collapsing are applied only at render time inside
`ui::sanitize_for_display`. This avoids any mutation of LLM context.

**Thinking settings** — `ThinkingLevel` (Off/Minimal/Low/Medium/High/XHigh) is
resolved at request time from `config.thinking_by_model` (per-model override)
then `config.thinking` (global default). Both OpenAI wire protocols carry it as
`reasoning.effort`; the test provider ignores it.

**Custom user tools** — at startup (and on `/reload`), `load_custom_tools`
scans three directories in order: `~/.ri/tools/`, `./.ri/tools/` (project-
local), and the XDG config `tools/`. Each executable that responds to
`--describe` with a valid JSON descriptor is registered as a `CustomTool`.
Built-in tool names take precedence — a colliding custom tool is silently
dropped (logged at debug). All tool directories are shown by `--print-dirs`.

**Bash tool terminal rendering** — `apply_terminal_render()`
(`agent/tools/terminal.rs`) emulates terminal cursor behavior for carriage
returns (`\r`): characters overwrite from the cursor position (reset to 0 on
`\r`), and only the final rendered state is passed to the model. This keeps
progress bars/spinners out of the LLM's context while preserving multi-line
output unchanged.

**External file change detection** — `FileTracker` (`agent/file_tracker.rs`)
records a snapshot (mtime + SHA-256 + content) for every file touched by
`read_file`, `write_file`, or `edit_file`. At the start of each LLM turn,
`check_modified()` stats every tracked path; on mtime change the file is
re-read and rehashed, content-identical saves are suppressed, and truly changed
files produce a ⚠️ injected user message with a unified diff (or a warn-only
note for large diffs). Binary files are skipped.

**Durable session event log** — every conversation is an append-only JSONL
`session_event.rs` event stream (`Sessions` under the XDG data dir, grouped by
cwd). `SessionState` ingests events and maintains two projections:
`DisplayProjection` (what the chat log shows) and `LlmProjection` (what gets
sent to the model). `live_turn.rs` overlays the transient in-flight turn on top
of the committed display. This is what powers `/resume` (Ctrl-R), `/export`,
and durable context compaction.

**Context compaction** — when a completed turn crosses the active model's
context threshold, or a provider returns a context-overflow error, the agent
generates a structured summary of older history and appends a
`SessionEvent::CompactionSummary` boundary. The LLM projection injects the most
recent summary as a synthetic user message and excludes older events, while the
display projection shows a visible `[compacted: Xk → Yk tokens]` marker.
Compaction can also be triggered manually with `/compact [instructions]`.

**User-definable agents** — agent profiles live in `~/.ri/agents/<name>/`
(global) and `.ri/agents/<name>/` (project-local, shadowing global). Each agent
has a `SYSTEM.md` (YAML frontmatter + system-prompt body; `AGENT.md` fallback)
and an optional `AGENTS.md` that replaces the global instructions. Tool/skill
filtering is applied at prompt-build time via `agents::filter_tools` /
`filter_skills`. `ask_user` and `read_skill` are always present.
`/agent <name>` switches agents; `/agent` shows a picker.

**Outer provider loop in `main.rs`** — `run()` returns a `RunResult` enum
(`Quit | ChangeModel | ChangeProvider`) rather than mutating global state. The
outer loop rebuilds the active provider instance's transport and re-enters
`run()` on every model/provider switch, so `App` and `ui` never depend directly
on backend transport details.
