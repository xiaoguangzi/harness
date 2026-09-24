# harness

A minimal coding-agent harness in Rust. It talks to any mix of LLM gateways that speak the Anthropic Messages protocol or the OpenAI Responses protocol (raw HTTP with streaming).

## Requirements

- Rust 1.88+
- [ugrep](https://github.com/Genivia/ugrep) on `PATH` for the `grep` tool (`apt install ugrep` / `brew install ugrep`)

## Usage

```sh
export ANTHROPIC_API_KEY=sk-ant-...     # or OPENAI_API_KEY, or configure gateways (below)
cargo run --release                      # interactive REPL (/exit to quit)
cargo run --release -- "list the files"  # one-shot
```

| Variable | Default |
|---|---|
| `HARNESS_MODEL` | `default_model` from `providers.json`, else `anthropic/claude-opus-5` |
| `HARNESS_SUBAGENT_MODEL` | `subagent_model` from `providers.json`, else the main model |
| `HARNESS_MAX_TOKENS` | `64000` (overridden per provider or model) |
| `HARNESS_PROVIDERS` | read only this providers file |
| `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_BASE_URL` | implicit `anthropic` provider |
| `OPENAI_API_KEY`, `OPENAI_BASE_URL` | implicit `openai` provider (Responses API) |

## LLM gateways

Declare gateways in `~/.harness/providers.json` and/or `.harness/providers.json` (the project file wins per provider name). Each one speaks one protocol; models are addressed as `<provider>/<model>`.

```json
{
  "default_model": "relay-a/claude-opus-5",
  "subagent_model": "relay-b/gpt-5-mini",
  "providers": {
    "relay-a": {
      "protocol": "anthropic",
      "base_url": "https://relay-a.example.com",
      "api_key": "${RELAY_A_KEY}"
    },
    "relay-b": {
      "protocol": "openai-responses",
      "base_url": "https://relay-b.example.com/v1",
      "api_key": "${RELAY_B_KEY}",
      "headers": { "X-Team": "harness" },
      "models": {
        "gpt-5": { "extra_body": { "reasoning": { "effort": "high" } } },
        "gpt-5-mini": {}
      }
    }
  }
}
```

| Field | |
|---|---|
| `protocol` | `anthropic` (`POST {base_url}/v1/messages`) or `openai-responses` (`POST {base_url}/v1/responses`); a `/v1` already in `base_url` is not doubled |
| `api_key` | sent as `x-api-key` (Anthropic) or `Authorization: Bearer` (OpenAI); override with `"auth": "bearer"` / `"x-api-key"` |
| `headers` | extra request headers |
| `max_tokens` | output cap (also per model) |
| `extra_body` | merged into every request body, also per model; a `null` removes a default key, e.g. `{"thinking": null}` for a model without adaptive thinking, or `{"include": null}` for a gateway that rejects encrypted reasoning |
| `fallbacks`, `eager_input_streaming` | Anthropic-only API features; default on for `api.anthropic.com`, off for gateways |
| `prompt_cache` | Anthropic automatic prompt caching; default on |
| `models` | per-model overrides; listed models show in `/model` and Tab completion |

`${VAR}` / `${VAR:-default}` expand from the environment in `base_url`, `api_key` and `headers`. A spec without a known provider prefix is a model on the current provider, so gateway model names containing `/` (such as `openai/gpt-5`) also work.

Switch models mid-conversation with `/model <provider>/<model>`; `/model` alone lists providers and models. The history is kept in Anthropic form and converted for OpenAI (`function_call` / `function_call_output`, `input_image`, `input_file`); requests are stateless (`store: false`), with reasoning items replayed from their encrypted content. Model-bound blocks (Anthropic thinking, OpenAI reasoning) are only sent back to the model that produced them. Subagents can use a different gateway through `subagent_model` or an agent's `model`.

Note: a project-level `.harness/providers.json` (like `.mcp.json`) is trusted as-is, so only run the harness in repositories you trust.

## Attaching files with `@`

Mention a path with `@` and its contents are attached to your message, so the model sees them without a tool call:

| Mention | Attaches |
|---|---|
| `@src/main.rs` | the file, line-numbered (first 2000 lines) |
| `@src/main.rs#L10-20` / `#L10` | just those lines |
| `@src/` | a directory listing |
| `@screenshot.png` (png/jpg/gif/webp) | the image |
| `@spec.pdf` | the PDF as a document |
| `@"name with spaces.txt"` | quoted paths |

Mentions work mid-sentence without spaces (`看看@src/main.rs的代码`); trailing punctuation is ignored. Only existing paths are attached, and an `@` right after an e-mail-style character is left alone, so `me@example.com` stays text. In the REPL, Tab completes paths after `@` and commands after `/`. Attached text files count as read for the `write` tool. The REPL also keeps line-editing history in `~/.harness/history`.

## REPL commands

| Command | |
|---|---|
| `/help` | list commands |
| `/skills` | list discovered skills |
| `/model [spec]` | show providers and models, or switch to `<provider>/<model>` |
| `/agents` | list subagent types, their permission and tools |
| `/mcp` | list MCP servers, their tools and log files |
| `/<skill> [args]` | run a skill directly (also works one-shot: `harness /greet Bob`) |
| `/exit` | quit |

## MCP servers

Declare servers in `.mcp.json` (project) and/or `~/.harness/mcp.json` (user), using the same shape as Claude Code; the project file wins on name clashes. `HARNESS_MCP_CONFIG=path` uses that single file instead.

```json
{
  "mcpServers": {
    "fs": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "."] },
    "remote": {
      "type": "http",
      "url": "https://example.com/mcp",
      "headers": { "Authorization": "Bearer ${API_TOKEN}" }
    }
  }
}
```

- Transports: `stdio` (default when `command` is set) and streamable `http` (default when `url` is set), via the official Rust SDK [`rmcp`](https://github.com/modelcontextprotocol/rust-sdk).
- `${VAR}` / `${VAR:-default}` are expanded from the environment in every string.
- Servers connect concurrently at startup; a server that fails is reported and skipped.
- Tools are exposed as `mcp__<server>__<tool>`. Text, embedded text resources and PNG/JPEG/GIF/WebP images are passed back to the model.
- A stdio server's stderr goes to `$TMPDIR/harness-mcp-<server>.log`.

## Skills

A skill is a directory with a `SKILL.md` (the [Agent Skills](https://agentskills.io) format):

```markdown
---
name: release-notes
description: Draft release notes from the git log. Use when asked for a changelog.
---
1. Run `git log --oneline <last-tag>..HEAD` ...
```

Skills are discovered in `.harness/skills/`, `.claude/skills/`, `~/.harness/skills/` and `~/.claude/skills/` (earlier wins on name clashes). Only each skill's name and description go to the model up front, listed in the `skill` tool; the body is loaded when the model calls that tool or when you type `/<name>`. Bundled scripts and reference files are used through the normal tools, relative to the skill's base directory, which the loaded skill states.

## Subagents

The main agent can delegate a self-contained task with the `agent` tool. A subagent runs in its own fresh conversation with its own system prompt and tool set, and only its final report comes back, which keeps long searches out of the main context. Several subagents started in one response run in parallel. Their tool calls are shown indented in the terminal.

- **`explore`** (built-in): a codebase search agent whose only tool is `grep`. Use it for "where is X / which files use Y" questions.
- **Custom agents**: Markdown files in `.harness/agents/` or `.claude/agents/` (project or `~`; the first found wins, and a file named `explore` overrides the built-in):

  ```markdown
  ---
  name: reviewer
  description: Reviews the current diff for bugs. Use after making changes.
  tools: read, grep          # optional; `mcp__server__*` matches a prefix
  permission: read-only      # default; `write` allows bash/write and non-read-only MCP tools
  model: claude-sonnet-5     # optional
  ---
  You are a code reviewer...
  ```

- **Permissions default to read-only**: a read-only agent only gets tools marked read-only (`read`, `grep`, `skill`, and MCP tools whose server sets `readOnlyHint`), even if its `tools` list names others. `tools` narrows further.
- Subagents cannot start subagents. Their file reads use their own tracker, so they never count as reads for the main agent's `write`.
- Model: the agent's `model`, else `subagent_model` / `HARNESS_SUBAGENT_MODEL`, else the main model; any `<provider>/<model>`.
- `/agents` lists the available types.

## Layout

- `src/main.rs` — CLI frontend (REPL / one-shot) and terminal output
- `src/agent.rs` — agent loop: request → run tools (concurrently) → send results → repeat until `end_turn`
- `src/llm/` — LLM gateways
  - `config.rs` — `providers.json`, model spec resolution, per-provider/model settings
  - `anthropic.rs` — Messages API backend (SSE into content blocks, thinking, prompt caching, refusal fallbacks)
  - `openai.rs` — Responses API backend (history ⇄ input/output item conversion, encrypted reasoning)
  - `http.rs` — POST with retries, SSE reader
- `src/mentions.rs` — `@path` parsing and expansion into attachment blocks
- `src/repl.rs` — rustyline helper: Tab completion of `@paths` and `/commands`
- `src/subagent.rs` — subagent definitions (built-in `explore`, custom files), permissions, the `agent` tool
- `src/mcp.rs` — MCP client: config loading, server connections, `mcp__*` tool adapters
- `src/skills.rs` — skill discovery, `SKILL.md` frontmatter parsing, the `skill` tool
- `src/tools/` — `Tool` trait, registry, and `FileTracker` (shared read/write state)
  - `bash.rs` — run a command via `bash -c` (timeout, output truncation)
  - `read.rs` — read a text file with line numbers (`offset` / `limit` paging)
  - `write.rs` — create or overwrite a file; an existing file must have been read first and be unchanged since
  - `grep.rs` — regex search via ugrep (PCRE, honours `.gitignore`, `files_with_matches` / `content` / `count` modes)

On the official Anthropic API, tools are declared with `eager_input_streaming`, so large inputs (such as file contents for `write`) stream as they are generated. The API then no longer validates tool input, so each built-in tool parses its input into a strict struct (unknown fields rejected), and a call whose streamed JSON does not parse is not run: the model gets an `INVALID_JSON` error result instead. MCP tools keep input buffering on, so the API validates their input against the server-provided schema.

## Adding a tool

Implement `tools::Tool` (name, description, JSON schema, async `run`) and register it in `ToolRegistry::with_defaults`.
