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
| | `--print-dirs` | Print the file-system paths ri uses and exit |
| `-h` | `--help` | Print help |
| `-V` | `--version` | Print version |

## Keybindings

| Key             | Action                          |
|-----------------|-------------------------------|
| `F1`            | Show keyboard shortcuts         |
| `Enter`         | Submit message (or queue steering message if agent loop is running) |
| `Shift+Enter`   | Insert newline in input         |
| `Page Up`       | Scroll chat up one page         |
| `Page Down`     | Scroll chat to bottom           |
| `Scroll wheel`  | Scroll chat (3 lines/tick)      |
| `Ctrl+I`        | Toggle provider/model info bar  |
| `Ctrl+F`        | Toggle full tool output         |
| `Ctrl+R`        | Resume latest session for current folder |
| `Ctrl+Z`        | Suspend ri only when the UI is idle and no agent/shell subprocess is running |
| `Ctrl+D`        | Quit when input is empty (or leave shell mode if shell input is empty) |
| `Ctrl+E`        | Edit the selected custom provider (provider picker) |
| `Ctrl+S`        | Cycle between available shells (shell mode) |
| `!`             | Enter shell mode when input is empty |
| `Alt+C`         | Copy the last assistant response |
| `Alt+Up` / `Alt+Down` | Step backward / forward through session history |
| `Ctrl+C`        | Quit (or leave shell mode)      |
| `Esc`           | Abort current agent loop; also cancel slash/selection contexts |

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
| `/reload`            | Reload AGENTS.md context and available skills    |
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
