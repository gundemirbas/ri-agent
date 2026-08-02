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
| | `--print-dirs` | Print the file-system paths ri uses and exit |
| `-h` | `--help` | Print help |
| `-V` | `--version` | Print version |

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
(editors, desktop/web UIs, `acpx`, …) can drive ri as a subprocess:

```sh
ri --serve --provider test   # or any configured provider
```

Implemented surface: `initialize` (protocol v1, image prompts accepted),
`session/new`, `session/prompt` (streams `agent_message_chunk`,
`agent_thought_chunk`, `tool_call`/`tool_call_update`, `usage_update`), and
`session/cancel` (maps to ri's hard abort).

Current limitations: one prompt at a time per session; auto-compaction is
disabled; `ask_user`/`request_permission` and session load/resume are not yet
wired. Requires Rust 1.88 (ATC dependency baseline).
