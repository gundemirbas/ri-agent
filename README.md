# ri-agent

**ri** is a fast, transparent terminal coding agent — a trimmed, Linux-only
fork of [xi-agent](https://github.com/larsch/xi-agent) with a single provider
surface (any OpenAI-compatible endpoint). Every tool call streams live as it
happens — you watch the agent read, edit, run commands, and reason in real
time. No hidden steps, no pre-canned workflows.

Inspired by [pi](https://pi.dev/) but more streamlined: colours, emoji, and
block characters give structure without noise. The UX stays compact even during
busy tool loops.

* Raw agent workflow — see everything, no black boxes
* Compact, styled output — clear block delineation with colour and emoji
* **You** define the instructions and skills that fit your workflows; ri provides a smooth, streamlined harness for the model to interact with your environment
* Linux-only, single provider surface — no OAuth, no per-vendor adapters
* LLM transport via `rig` with live streaming of text, reasoning, and tool-call argument deltas
* Lightweight Rust binary — low memory, fast startup, single executable
* Standard `AGENTS.md` and `SKILL.md` support
* Custom tools and skills — extend without bloat
* Caveats: **No safety guards** on tool calls; you are in control

## Providers

| Provider | Type | Auth |
|---|---|---|
| **OpenAI-compatible endpoint** | Any OpenAI-compatible API (OpenAI, DeepSeek, vLLM, local inference servers, …) | API key in config |

Configure named provider instances in `~/.config/ri/config.toml` and select them with `-P <name>` or `/provider <name>`.

Each provider is a `[[providers]]` array entry (not a `[providers.<id>]` table) with an `id` field:

```toml
[[providers]]
id = "my-endpoint"
service_type = "openai-compatible"
api_type = "openai-compatible"
base_url = "https://api.openai.com/v1"
api_key = "sk-..."
```

### Per-provider request options

All three are optional; omit them to defer to the endpoint's defaults.

| Config key | Description |
|---|---|
| `temperature` | Sampling temperature (0.0–2.0). Higher is more creative. |
| `max_tokens` | Maximum output tokens the model may emit per turn. |
| `output_schema` | JSON Schema constraining the model's final answer (structured output). |

```toml
[[providers]]
id = "my-endpoint"
service_type = "openai-compatible"
api_type = "openai-compatible"
base_url = "https://api.openai.com/v1"
api_key = "sk-..."
temperature = 0.7
max_tokens = 4096
output_schema = { type = "object", title = "Answer", additionalProperties = false, properties = { answer = { type = "string" } }, required = ["answer"] }
```

`output_schema` drives rig's structured output (`response_format` / `text.format`).
It asks the model for JSON-only answers, so only set it when the whole turn should be
machine-readable; while tools are active (and before any tool result is in history)
some endpoints suppress tool calls in favour of the schema — **not** recommended for
general coding work.

## License

AGPL-3.0-only. See [LICENSE](LICENSE).

## Installation

Install from source:

```sh
cargo install --path .
```

## Command line options

| Short | Long | Description |
|-------|------|-------------|
| `-P` | `--provider <PROVIDER>` | Configured provider instance id to use |
| `-m` | `--model <MODEL>` | Model name to use (e.g. gpt-4o) |
| `-p` | `--print <PROMPT>...` | Run in non-interactive mode: send PROMPT, stream the response to stdout, and exit. Accepts multiple words without shell quoting |
| | `--serve` | Run as a headless ACP (Agent Client Protocol) server on stdio instead of the TUI |
| | `--serve-ws <ADDR>` | Run as a headless ACP server over HTTP + WebSocket (`ws://ADDR/acp`) instead of the TUI |
| | `--serve-ws-token <TOKEN>` | Require this admin token on mutating `_ri/*` methods |
| | `--serve-ws-cert <CERT>` / `--serve-ws-key <KEY>` | Serve `--serve-ws` over TLS (`wss://`) |
| | `--tui-acp` | Keep the interactive TUI but run the agent as a detached child `ri --serve` over ACP (decoupled UI ↔ agent) |
| | `--sandbox` | Route agent tool subprocesses through the rootless container sandbox (`ri-sandbox`, user namespace + chroot). Linux-only; also settable via `sandbox = true` in config.toml |
| | `--print-dirs` | Print the file-system paths ri uses and exit |
| `-h` | `--help` | Print help |
| `-V` | `--version` | Print version |

## Rootless tool sandbox (`--sandbox`)

Agent tool subprocesses (`bash`, `exec`, custom tools) normally run directly on
the host. With `--sandbox` (or `sandbox = true` in config.toml) each one runs
inside a **rootless container** built from raw Linux syscalls — no OCI, no
podman/docker, no root:

- `ri-sandbox` (a second bin target, spawned per tool call) creates a
  **user namespace** (`unshare(CLONE_NEWUSER|CLONE_NEWNS)`), maps your uid to
  0, binds a scrubbed view into a flat rootfs directory and `chroot`s.
- Isolation: the host's `$HOME`, `/root`, `/etc` secrets and other projects are
  invisible. Only the explicit binds are visible/writable: `/work` (the tool
  cwd), `/tools` (custom tools), `/tmp` (scratch), non-secret `/etc/*` for
  DNS/users, and the dynamic loader dirs for the shell.
- Network stays host-shared (internet works: crates.io, APIs, curl).
- File tools come from a **static musl uutils coreutils** when provisioned
  (`just sandbox-provision` / `scripts/fetch-uutils-coreutils.sh`); otherwise
  the host's `/bin`, `/usr` are bind-mounted read-only as a fallback, so the
  sandbox works everywhere. Custom tools are static-musl binaries and run
  inside with zero dependencies.
- The shell is itself a **static Rust shell**: `ri-sh` (a ~70-line `sh`
  CLI built on the embeddable [`epsh`](https://crates.io/crates/epsh) POSIX
  shell library) is installed as `/bin/sh`. In the default **strict** image
  (ri-sh + static coreutils) every binary is static, so there is no host
  `/bin/sh` copy and no `/lib`/loader binds at all; a **compatible** fallback
  keeps the host shell when ri-sh is absent.
- Resource limits are applied by default
  (`cpu=30s, nproc=64, nofile=2048, as=512MiB, fsize=1GiB, core=0`) and can be
  tuned via `$RI_SANDBOX_RLIMITS` (`none` disables).
- Linux-only; user namespaces must be enabled in the kernel.

See `docs/CONTAINER-RUNTIME-SPEC.md` for the full design spec and its
implementation-status appendix. Tests: `tests/container_sandbox.rs` (uid
mapping, host isolation, `/work` persistence, static coreutils) plus an
in-process `bash`-tool round-trip; they skip gracefully when user namespaces
are unavailable.

```sh
scripts/fetch-uutils-coreutils.sh    # optional: static file tools
cargo build --bins                   # produces `ri` + `ri-sandbox` + `ri-sh`
ri --sandbox                         # TUI with sandboxed tools
ri --serve --sandbox                 # headless ACP agent, same sandbox
```

## Keybindings

| Key             | Action                          |
|-----------------|-------------------------------|
| `F1`            | Show keyboard shortcuts         |
| `Enter`         | Submit message (or queue steering message if agent loop is running) |
| `Shift+Enter`   | Insert newline in input         |
| `Tab` / `Up` / `Down` | Apply / navigate completion suggestions in the input line |
| `Page Up`       | Scroll chat up one page         |
| `Page Down`     | Scroll chat to bottom           |
| `Scroll wheel`  | Scroll chat (3 lines/tick)      |
| `Ctrl+I` / `Alt+S` | Toggle provider/model info bar  |
| `Ctrl+F`        | Toggle full tool output         |
| `Ctrl+R`        | Resume latest session for current folder |
| `Ctrl+Z`        | Suspend ri only when the UI is idle and no agent/shell subprocess is running |
| `Ctrl+D`        | Quit when input is empty (or leave shell mode if shell input is empty) |
| `Ctrl+E`        | Edit the selected custom provider (provider picker) |
| `!`             | Enter shell mode when input is empty |
| `Alt+C`         | Copy the last assistant response |
| `Alt+Up` / `Alt+Down` | Step backward / forward through session history |
| `Ctrl+C`        | Abort agent loop (1: stop after turn, 2: abort, 3: force-kill); quit in shell mode |
| `Esc`           | Abort current agent loop; also cancel slash/selection contexts |
| Mouse drag      | Drag to select text in the log; release to copy to the clipboard |

## Slash commands

| Command              | Description                                      |
|----------------------|--------------------------------------------------|
| `/new`               | Start a new conversation                         |
| `/model`             | Open interactive model picker                    |
| `/model <name>`      | Switch to a named model                          |
| `/provider`          | Open interactive provider picker                 |
| `/provider <name>`   | Switch to a configured provider instance         |
| `/thinking <level>`  | Set reasoning level (off / minimal / low / medium / high / xhigh) |
| `/resume`            | Open session picker (local + foreign sessions)   |
| `/compact [instructions]` | Compact session context now, optionally with summary instructions |
| `/export [path]`     | Export this session to a self-contained HTML file |
| `/agent [name]`      | Switch to a named agent profile, or show the agent picker |
| `/retry`             | Retry the last turn after an error               |
| `/reload`            | Reload AGENTS.md context, available skills, and agents |
| `/skill:<name>`      | Invoke a skill by name (e.g. `/skill:plan`)      |
| `/quit`              | Quit                                             |

## Skills

Add custom agent capabilities and expertise by placing [SKILL.md](https://agentskills.io/) files in these directories; reference them with `/skill:<name>`:

- `~/.ri/skills`
- `~/.agents/skills`
- `./.agents/skills`
- `./.ri/skills`

## Custom tools

Add custom tools by placing executable files in these directories (in this order):

- `~/.ri/tools`
- `~/.agents/tools`
- `./.agents/tools`
- `./.ri/tools`

Tools must respond to a `--describe` option and output a JSON description of the
tool's interface, including its name, description, and expected input
parameters. This allows the agent to understand how to use the tool effectively.
For example:

```json
{
  "name": "my_tool",
  "description": "A tool that does something useful",
  "parameters_schema": {
    "type": "object",
    "properties": {
      "path": { "type": "string", "description": "Path to operate on" }
    },
    "required": ["path"]
  }
}
```

## Agents and subagents

Agent profiles live in `~/.ri/agents/<name>/` (global) and `.ri/agents/<name>/`
(project-local, shadowing global). Each agent uses a `SYSTEM.md` (with YAML
frontmatter) as its system prompt and an optional `AGENTS.md` that replaces the
global instructions. Tool/skill availability can be restricted per agent with
`include_tools` / `exclude_tools` / `include_skills` / `exclude_skills` globs.
Switch agents with `/agent [name]` or the `/agent` picker.

Agents with `mode: subagent` in the frontmatter are *subagents*: they never
appear in the picker but can be delegated to by the orchestrator via the
`invoke_subagent` tool. A subagent runs with its own system prompt and tool
universe (filters applied, `invoke_subagent` removed to prevent recursion) and
finishes within a bounded number of steps.

## File attachments (`@file`)

Reference a file directly in your prompt with `@path` (tab-completable). ri
reads the file and attaches its contents (or the image itself, for image
files) to the message before the agent runs — useful for giving the model
context without a separate `read_file` turn.

## Session persistence and resuming

Every conversation is stored as an append-only event log under the XDG data
directory (grouped by working folder). Press `Ctrl+R` to resume the latest
session for the current folder, `/resume` for the full picker, and `/export`
to write a self-contained HTML transcript. Long sessions are automatically
compacted against the active model's context window; `/compact` forces it
manually, optionally with custom summary instructions.

## Headless mode (Agent Client Protocol)

`ri --serve` runs the same agent loop without the TUI, speaking the
vendor-neutral [Agent Client Protocol](https://agentclientprotocol.com/) —
JSON-RPC 2.0 over stdio, one message per newline — so any ACP-capable client
(editors, desktop/web UIs, `acpx`, …) can drive ri as a subprocess. A WebSocket
variant is available via `ri --serve-ws 127.0.0.1:8080` (route `/acp`, with
streamable HTTP on the same server for non-WebSocket clients):

```sh
ri --serve --provider test         # stdio
ri --serve-ws 127.0.0.1:8080 --provider test   # WebSocket + HTTP
```

Implemented surface:

- `initialize` — protocol negotiation, image prompts, `session/load`,
  `session/fork` (ACP v2 session capability)
- `session/new`, `session/prompt` (streams `agent_message_chunk`,
  `agent_thought_chunk`, `tool_call`/`tool_call_update` with live tool output
  forwarded as in-progress updates, `usage_update`; the `end_turn` response
  folds the turn's token usage)
- `session/fork` — clones a session (live or persisted) into a new id so
  clients can branch conversations
- `session/load` — replays a session's history as updates; sessions are
  persisted to disk after each prompt (`~/.local/share/ri/sessions/acp/`), so
  a later process can resume them (`_ri/list_sessions` → `session/load` →
  `session/prompt`); prompt tools run anchored at the session `cwd`
- `session/cancel` — maps to ri's hard abort
- `ask_user` — forwarded as `session/request_permission` (multiple-choice
  mapping with descriptions folded into labels; freeform asks get a trailing
  "Continue" escape), so headless clients can approve/deny tool operations
- ri-specific `_ri/*` methods: `_ri/get_state` (model, thinking level,
  sessions), `_ri/set_model`, `_ri/set_thinking`, `_ri/set_provider`
  (re-resolve a provider preset by id and hot-swap the active provider for
  subsequent turns), `_ri/list_sessions` / `_ri/delete_session` /
  `_ri/prune_sessions`
  (persisted-session management), `_ri/logs` (recent activity),
  `_ri/steering` (queues steering for the next prompt turn); call
  unit-request methods with `"params": null`; mutating methods accept a
  `token` field enforced by `--serve-ws-token`
- Transport: stdio (`--serve`) or HTTP+WebSocket at `/acp` (`--serve-ws`,
  optionally TLS via `--serve-ws-cert`/`--serve-ws-key`); the WS server
  multiplexes many clients, stdio serves one. `initialize` is negotiated
  per-connection: protocol v1 serves the full surface, and protocol v2
  (unstable) is served over the shared per-turn core with the standard v2
  session surface (`session/new`/`resume`/`list`/`close`/`fork`/`delete`/
  `prompt`/`cancel` + `ask_user` → `session/request_permission`, plus the
  ri-specific `_ri/*` methods via shared version-neutral implementations)

Current limitations: one prompt at a time per session; `_ri/steering` applies
at the next turn boundary (mid-turn injection is not implemented).
`session/cancel` maps to ri's `HardAbort`, which the agent loop honours
mid-stream: the current model stream is chopped on the next token (verified by
an in-process e2e). Requires Rust 1.88 (ACP dependency baseline).

## Decoupled TUI (`--tui-acp`)

`ri --tui-acp` keeps the familiar ratatui interface but stops owning the
agent loop: it spawns a detached child `ri --serve` (same provider/model) and
speaks ACP over stdio, so the UI and the agent are two separate processes
joined only by the protocol. Submitted messages become `session/prompt` calls,
`session/update` notifications are translated back into the TUI's event
vocabulary, tool-permission asks surface through the TUI ask dialog
(`session/request_permission`), and `Ctrl-C` maps to `session/cancel`.

```sh
ri --tui-acp --provider test   # interactive TUI driving a detached --serve
```

Limitations: steering applies at the next turn boundary (mid-turn injection is
not implemented) and provider *instance* (preset/API-key) changes inside the
TUI are not hot-swapped in the child — model/thinking changes are pushed
automatically via `_ri/set_model`/`_ri/set_thinking`, and `_ri/get_state` now
reflects live mid-turn state too. The plain in-process TUI remains the default.
