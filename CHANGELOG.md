# Changelog

## Unreleased

### Added

- **ACP headless server**: `ri --serve` exposes the agent loop over the
  vendor-neutral Agent Client Protocol (JSON-RPC 2.0 over stdio): `initialize`
  (protocol v1, image prompts), `session/new`, `session/prompt` (streams
  `agent_message_chunk` / `agent_thought_chunk` / `tool_call` (±update) /
  `usage_update`), and `session/cancel`. The existing agent loop is reused
  unchanged; the ACP connection runs on a dedicated thread. Now requires
  Rust 1.88 (ACP dependency baseline).
- **ACP event-loop refactor**: `session/prompt` handlers now return immediately
  and run the whole turn (event forwarding, the `session/request_permission`
  ask bridge, the final response) as a concurrent task with the `Responder`
  moved in — this keeps the SDK event loop free during a run. As a result the
  full `ask_user` → `request_permission` round-trip works (verified e2e),
  `_ri/get_state` reports **live** mid-turn state, and `session/cancel` is
  dispatched while a turn is streaming (honoured at the agent loop's next
  checkpoint). Previously the serial handler blocked the loop, so a client's
  permission reply could never be read back (hang) and `_ri/get_state` only
  reflected state between turns.
- **Per-session tool cwd + auto-compaction**: every `session/prompt` roots its
  tools at the session `cwd` — `bash`/`exec` run with that working directory
  and `read`/`write`/`edit`/`find` resolve relative paths against it — and ACP
  sessions run with auto-compaction enabled (matching the TUI).
- **ACP extensions**: `ask_user` now maps to `session/request_permission` so
  headless clients can approve/deny tool operations (freeform-only asks get a
  single "Continue" option). `--serve-ws <ADDR>` serves the same surface over
  HTTP + WebSocket at `/acp` (axum). Ri-specific `_ri/get_state`,
  `_ri/set_model`, `_ri/set_thinking` custom methods can introspect and swap
  the active provider/thinking level at runtime. `session/load` replays a
  known in-memory session's history. Live tool output now streams over the
  wire: each `ToolOutputChunk` is forwarded as an in-progress
  `tool_call_update` so bash/exec output appears as it runs (`TestProvider`
  gains a `tool <name> <args-json>` command to exercise this offline). Tools
  are registered per prompt so `ask_user` routes through the same channel the
  loop reads; the sessions data dir and cache dir are excluded from the
  headless file tracker.
- **ACP session persistence**: each completed prompt is written to
  `~/.local/share/ri/sessions/acp/<id>.json` (atomically via temp+rename), so
  `session/load` can resume a session from a previous run: `_ri/list_sessions`
  lists persisted sessions (newest first), `session/load` replays the restored
  history and registers it in memory, and the next `session/prompt` continues
  the multi-turn conversation from that history across process restarts.
- **ACP ops**: `_ri/logs` surfaces a bounded recent-activity buffer (session
  lifecycle, prompt start/end, asks, provider swaps, deletes). Mutating
  `_ri/*` methods (`set_model`, `set_thinking`, `delete_session`,
  `prune_sessions`) require an admin `token` when `--serve-ws-token` is set.
  `--serve-ws` can run TLS via `--serve-ws-cert`/`--serve-ws-key` (`wss://`),
  and the WS server multiplexes many client connections while stdio serves
  one.
- **Decoupled TUI (`--tui-acp`)**: the ratatui UI can now be driven by a
  detached `ri --serve` child over ACP instead of the in-process loop. A new
  ACP client bridge (`src/acp_tui.rs`) translates `session/update`
  notifications back into the TUI's `AgentEvent` vocabulary, routes
  `session/request_permission` through the TUI ask dialog, and maps Ctrl-C to
  `session/cancel`. Verified over a real pseudo-terminal: text streams,
  cancel, multi-turn, and a live detached child process. The in-process TUI
  remains the default.
- **ACP v2 (fork, end_turn usage) + TUI bridge hardening**: `session/fork`
  (clones a live/persisted session into a new id for branching conversations),
  `end_turn_token_usage` (the `session/prompt` response folds the turn's
  tokens alongside the streamed `usage_update`), and `_ri/steering` (queues
  steering applied at the next prompt turn boundary — the SDK serves requests
  serially, so mid-turn injection is impossible). The `--tui-acp` client now
  FIFO-queues rapid submissions instead of dropping them, and pushes
  model/thinking changes to the child via `_ri/set_model`/`_ri/set_thinking`;
  `TestProvider` gains a `usage` command to exercise the usage paths offline.

### Fixed

- **Subagents (completed)**: `mode: subagent` agent profiles can now be invoked
  by the orchestrator through the new `invoke_subagent` tool. Each subagent runs
  with its own system prompt, tool/skill filters (with `invoke_subagent` stripped
  to prevent recursion), and a bounded 20-step loop; live output streams under
  the outer tool call and only the final answer is persisted. Wired in both the
  TUI and `--print` mode.
- **CI release artifact**: pushes to `main`/`master` now build and upload a static
  `x86_64-unknown-linux-musl` release binary.

### Fixed

- **CI branch mismatch**: the workflow now runs on `main` *and* `master`, so
  pushes to the active default branch (`master`) trigger checks.
- **Session ID collisions**: session/ephemeral IDs now use 8 random bytes plus a
  unique fallback (nanos+pid) if entropy is unavailable; migrated `getrandom` to
  0.3 (`fill`).
- **Panic on missing session state**: `start_agent_task` no longer
  `.expect()`s — it returns `false` and surfaces a notice instead.
- **Redundant assignment** in `/model` handler removed.
- **Dead code removed**: identity-function `normalize_cwd_for_match` and the
  stale `#[allow(dead_code)]` guards on agent `path`/`base_dir`/`resolve_agent`
  (now consumed by the subagent runner).

### Internal

- Rebranded `XiConfig` → `RiConfig` and remaining "Xi" references in code/docs to
  "ri".
- Added `rust-version = "1.85"` to `Cargo.toml`.
- Removed duplicate doc comments; documented `/agent`, `/retry`, `@file`
  attachments, mouse selection, and subagents in the README.

## v0.5.0 — 2026-07-20

### Added

- **Ctrl-Z suspend/resume**: idle-only Ctrl‑Z suspend and foreground resume
  support. The TUI is recreated on resume to avoid cursor query races (#4, #5, #6).
- **Tab in ask_user**: pressing Tab in the ask_user prompt copies the
  highlighted option into the input field for editing before submitting.

### Fixed

- **Streaming edit diff stabilisation**: prefix-matching and symmetric
  common-lines computation produce fewer flickering diffs during streaming
  `edit_file` results.
- **Throbber gaps**: the bottom gap now shrinks instead of shifting content
  when the throbber appears, and the throbber is hidden only when tool output
  visually grows.
- **Streaming tool call labels**: streaming tool call labels and results use
  `┆` (not `╰`) for a cleaner flowing look.
- **Newlines in user messages**: newlines are now correctly preserved when
  rendering user messages in the log.
- **Provider menu login**: the login option in `/provider` menu no longer
  does nothing.
- **Provider setup prompt**: removed misleading "leave empty to keep
  current" hint from the API key prompt.
- **Ollama context window**: context window is now discovered via
  `/api/ps` instead of GGUF metadata; model tags are normalised in the
  cache.
- **`--model` CLI override**: the `--model` flag is now correctly applied
  to the provider instance on startup.
- **Step-back ask_user**: the ask_user question text is preserved when
  stepping back and re-answering.
- **Windows build**: `fmt`, `clippy`, and `check` now pass with zero
  warnings on Windows; cross-compile build scripts fixed.

### Internal

- `main.rs` decomposed into modules and handler functions.
- Terminal lifecycle extracted into its own module with a `recreate_terminal`
  helper.
- Dimmed hint text removed from provider setup input.
- Python program test command added.

## v0.4.0 — 2026-07-05

### Added

- **Prompt cache support**: OpenAI and Anthropic providers now report
  cached-token counts in the info bar. Anthropic sends automatic ephemeral
  `cache_control` annotations on every request (matching API spec), OpenAI
  sends a stable session ID to improve cache routing. Unexpected cache
  misses (zero cached tokens despite a recent cache-populating turn)
  surface a ⚠️ suffix in the info bar context display.
- **Session resume context usage**: the info bar now shows the last known
  token utilisation immediately when resuming a session, instead of showing
  only the context window size until the next turn completes.
- **DeepSeek V4 context window**: hard-coded 1M token fallback entries for
  `deepseek-v4-flash` and `deepseek-v4-pro` (kludge until upstream metadata
  is available).
- **Alt+S shortcut**: toggle the info bar on and off without `/info`.
- **Theme configuration system**: themeable UI with CSS-style color specs
  and terminal color support via a TOML configuration file.
- **Agent-level hooks system**: pluggable hook points for tool lifecycle
  (`OnToolIntent`), streaming milestones (`OnFirstThinkingToken`,
  `OnFirstTextToken`), and session events (`OnIdle`, `OnCompacting`,
  `OnExternalChange`, `OnStatusUpdate`), with IPC event streaming for
  external tooling.
- **Alt+C shortcut**: copy the last assistant response to the system
  clipboard.
- **Step-back navigation**: step back through ask_user answers with full
  prompt UI restoration, enabling re-answering or branching from any
  decision point.
- **Mouse text selection**: click-drag to select and copy text from the
  log view.
- **Similar session scope**: a "similar" scope between `local` and
  `foreign` in the session resume picker for faster filtering.
- **`/new` resets FileTracker and reloads skills**, ensuring a clean
  environment for each fresh session.
- **`@file` backtick notation**: `@filename` mentions are rewritten to
  backtick-quoted paths for LLM consumption, reducing confusion.
- **`read_skill` and `edit_skill` tools**: embedded tools for listing,
  loading, and editing skills, with filesystem paths and scope
  indicators.
- **User message markdown rendering**: user prompts are now rendered with
  markdown formatting in the log view, matching assistant output.
- **Keyboard shortcuts help**: accessible via `?` key.
- **Block content alignment**: all block content consistently aligned to
  column 3 with margin markers.
- **Edit diff fillers**: adjacent "total lines" and "common lines" fillers
  in `edit_file` diffs are collapsed to reduce noise.
- **Provider synthesis**: built-in provider instances synthesised
  automatically; explicit provider selection required for ambiguous
  configurations.
- **OSC 52 clipboard**: clipboard integration via OSC 52 escape sequences,
  replacing the `arboard` crate for broader terminal compatibility.
- **Python 🐍 emoji**: the built-in Python tool now shows a Python emoji
  icon.

### Fixed

- **Steering race condition**: pressing Enter during streaming now defers
  the user message until the current assistant turn and all its tool calls
  are committed, preventing transcript corruption and tool-call skipping.
  Explicit cancellation still interrupts immediately.
- **Attachment ordering**: synthesized `read_file` events from `@filename`
  attachments are now placed after the submitted user prompt in the event
  stream, matching provider expectations.
- **`--print` model override**: the `--model` flag now correctly overrides
  the configured model in non-interactive mode.
- **`@file` missing-file handling**: references to nonexistent files are
  now silently ignored (no synthetic tool call, no error notice, no
  provider error). The `@file` text remains in the prompt unchanged.
- **Anthropic `cache_control` placement**: removed the invalid top-level
  `cache_control` field and moved it to individual content blocks, fixing
  400 errors from the Copilot Anthropic proxy and restoring prompt caching
  on both direct and proxied routes.
- **Info bar token persistence**: previous-turn token usage (input size,
  cached size) remains visible when starting a new prompt, instead of
  clearing at turn launch. Still resets correctly on `/new`.
- **Codex prompt cache hits**: parse `input_tokens_details.cached_tokens`
  from the OpenAI Responses API in the Codex provider, so cache-hit
  indicators appear for Copilot GPT-5.x models.
- **Copilot auth**: full token (with metadata) is now stripped correctly
  before use as Bearer auth, fixing authentication failures.
- **ask_user rendering**: questions now stream from partial data during
  tool call rendering and appear in the log; rendering layout improved.
- **Provider switching**: changing providers no longer uses a stale model
  list for fetching; model list auto-fetches on startup even when a model
  is already configured.
- **Config load failures** are now fatal, preventing silent overwrite of
  user configuration with defaults.
- **Anthropic null tool_args**: providers now guard against null tool
  arguments in Anthropic wire format.
- **Auth token expiry**: standardized across backends with a new
  `OAuthBackend` trait and test infrastructure.
- **Serde error messages**: Rust struct names in serde errors are now
  translated to model-friendly JSON concepts.
- **Tool descriptions**: reworded to prevent redundant `2>&1` and
  absolute-path annotations in shell commands.
- **System prompt** clarifies that file paths are relative to the working
  directory.
- **Input panel**: scroll-to-cursor behavior added when text exceeds the
  viewport.
- **Tool invocation labels**: leading and trailing empty lines trimmed;
  placeholder labels shown consistently during streaming; tool icons
  render normally even when labels are italic.
- **Output trimming**: leading and trailing empty wrapped lines removed
  from output blocks; body line limit enforced on wrapped visual lines,
  not logical lines.
- **Thinking display**: final streaming line uses ┆ instead of ╰; blank
  separator line removed between thinking and response; display stabilized
  by truncating wrapped lines.
- **Throbber**: no longer sticks or blocks Escape during token refresh;
  remains visible during retry; refreshes correctly on tool intent, args
  delta, and output chunks.
- **Streaming blocks**: padded during shrink to prevent layout jitter;
  partial-JSON headline blink eliminated.
- **Markdown**: extra blank line after pre-formatted text removed;
  HTML/XML tags now rendered verbatim instead of silently dropped.
- **OpenAI reasoning content**: always included in assistant wire format;
  OpenAI-specific parameters guarded by backend type.
- **OpenWebUI context window**: auto-discovered for OpenAI-compatible
  backends.
- **PowerShell** now runs noninteractively.
- **Windows build** fixed and tests made cross-platform.
- **Log redraw** now happens before disk I/O on submit for lower perceived
  latency.

## v0.3.0 — 2026-05-31

### Added

- **`@filename` attachments**: type `@` in the chat input to get interactive
  file completion and attach file contents directly to your message.
- **Live subprocess output**: bash/exec tool output now streams into the UI in
  real time instead of appearing only after the command finishes.
- **Auto model picker**: when a provider has no model configured xi automatically
  opens the model picker so you can choose one without extra navigation.
- **Action verb placeholders**: while a tool call is still streaming in, the UI
  shows a meaningful verb (e.g. "Reading…", "Editing…") instead of a blank line.
- **Unified truncation indicators**: long tool outputs use consistent dimmed
  italic placeholder markers to signal hidden content.

### Fixed

- `edit_file` now returns an error if `old_text` and `new_text` are identical,
  preventing silent no-ops.
- Tool call pending labels appear immediately before any argument JSON arrives.
- `ask_user` question blocks render on the default background, consistent with
  agent response blocks; answers appear as normal user message blocks.
- `edit_file` diff truncation markers are now coloured by side (add/remove).
- Common-line diff placeholders are omitted for pure-addition or pure-removal
  hunks, reducing noise.

## v0.2.0 — 2026-05-24

First public release. xi is a focused AI agent for the terminal.

- **Multiple LLM providers**: OpenAI, Anthropic, Google Gemini, GitHub Copilot,
  Ollama, OpenRouter, Codex
- **Built-in tools**: read_file (with image support), write_file, edit_file,
  find_files, ask_user, bash, exec, python, custom user-defined tools, cmd
  (Windows), powershell (Windows)
- **Interactive TUI** with streaming responses, thinking tokens, tool call
  previews, session persistence, file change detection
- **Skills system**: pluggable AGENTS.md / SKILL.md expertise from home and
  project directories
- **Session management**: resume past sessions, session branching,
  compaction for long conversations
- **Non-interactive mode**: `xi --print "..."` for pipe-friendly inference
- **Custom tools**: executable protocol with `--describe` JSON interface

## v0.1.0 — Unreleased

Initial development. Internal use only prior to the v0.2.0 public release.
