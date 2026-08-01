# Provider Model Specification

## Purpose

This document defines ri-agent's backend model: what a *provider instance* is,
how it is configured and selected, and how the interactive UI exposes it.

ri-agent is Linux-only with a **single provider surface**: any OpenAI-compatible
endpoint (OpenAI API, DeepSeek, vLLM, Open WebUI, local inference servers, …).
There are no per-vendor adapters, no OAuth, and no built-in hosted singletons —
every usable provider is a user-configured instance.

## Domain model

### Provider instance

A **provider instance** is one configured, selectable backend entry. It is the
unit that appears in the main provider picker (`/provider`) and the unit that
drives a concrete LLM connection.

A provider instance contains:

| Field | Purpose |
|-------|---------|
| `id` | Stable, user-visible identifier (e.g. `gpu-box`) |
| `backend_preset` | Which preset the instance belongs to (`openai-compatible` or internal `test`) |
| `api_type` | Wire protocol: `openai-responses` (`/v1/responses`) or `openai-compatible` (`/v1/chat/completions`) |
| `base_url` | Full endpoint base URL (user-supplied; HTTPS by default) |
| `api_key` | API key / bearer token (optional for local servers) |
| `model` | Last-selected model name; falls back to the preset default `gpt-4o` |

### Presets

The static catalog (`BACKEND_PRESET_CATALOG`) defines two presets:

- **`openai-compatible`** — any OpenAI-compatible endpoint. User-supplied URL,
  optional API key, user may choose between the Responses and Completions
  protocols. This is the only preset users ever configure.
- **`test`** — internal UI-exercise provider. Never shown in the picker,
  never persisted to config, never listed by `--provider`.

## Configuration semantics

### Representation

Provider instances are stored as first-class entries in
`~/.config/ri/config.toml`:

```toml
provider = "work-webui"

[[providers]]
id = "work-webui"
backend_preset = "openai-compatible"
api_type = "openai-compatible"
base_url = "https://my-webui.example.com/api"
api_key = "token123"
model = "llama3.1"

[[providers]]
id = "gpu-box"
backend_preset = "openai-compatible"
api_type = "openai-compatible"
base_url = "http://gpu-box:11434"
```

There is no separate provider/service/API/preset taxonomy in config — the
instance record carries everything.

### Active provider

The active provider is selected by `config.provider` (the instance id).
Changing provider means selecting a different provider instance. The active
instance is resolved through `config.resolve_effective_providers()` /
`resolve_provider(id)` and, if it cannot be resolved, ri falls back to a
synthetic OpenAI-compatible default so the TUI always has something to start
with.

### Protocol choice

`api_type` selects between OpenAI's two wire protocols:

- `openai-responses` — OpenAI Responses API; supports `reasoning.effort`
  (thinking level).
- `openai-compatible` — Chat Completions protocol; works with any
  OpenAI-compatible server (including OpenAI, DeepSeek, vLLM, Open WebUI,
  Ollama's compat endpoint, …); also carries `reasoning.effort`.

The hidden `test` preset uses the internal `Test` transport and ignores
thinking settings.

## UI semantics

### Main provider picker

- `/provider` lists configured provider instances.
- `-P <name>` / `--provider <name>` selects one on the command line.
- `Ctrl+E` opens the picker in edit mode for the highlighted instance.
- A placeholder ("No providers configured") with a hint to add one is shown
  when no instances are configured.

### Adding / editing an instance

The add-provider flow (`/provider` → add) drives `ProviderManager` through
a short state machine (`provider_manager.rs`):

1. choose a preset (only `openai-compatible` is offered to users)
2. choose the protocol (`openai-responses` or `openai-compatible`)
3. enter the endpoint URL (normalized: scheme prepended, trailing `/` stripped)
4. enter the API key (can be left empty for local servers)
5. name the instance

The instance is saved back to `config.toml` on completion.

### `/v1` handling

OpenAI-compatible servers expose their API under a `/v1` path (rig appends
`/chat/completions` or `/responses`). To make endpoint entry forgiving,
`/v1` is handled automatically in both directions:

- **Normalization** — when the entered URL has no path of its own
  (e.g. `https://api.openai.com` or `http://localhost:11434`), `/v1` is
  appended, so `https://api.openai.com` → `https://api.openai.com/v1`.
  URLs that already carry a path (e.g. `https://host/api` for Open WebUI)
  are left untouched.
- **Runtime fallback** — if the primary endpoint returns 404 (Not Found)
  before any content streams, the transport retries once against the
  opposite variant (pathless ↔ `/v1`). Both the streaming `/chat/completions`
  `/responses` calls and the `/models` listing fall back this way, so a
  mis-styled base URL still works.

### Model picker

`/model` fetches the live model list from the provider's `/models` endpoint
and offers it as a picker; a typed model name also works directly.

## Internal nomenclature

- `BackendPreset` — `openai-compatible` | `test`.
- `ApiType` — `openai-responses` | `openai-compatible` | `test`.
- `ProviderInstance` — the configured entry (above).
- `ProviderManager` / `ProviderSetupStep` — interactive add/edit/remove
  state machine (`provider_manager.rs`).
- `build_provider_for_instance` — maps an instance to a concrete
  `Arc<dyn LlmProvider>` (`provider.rs`).
