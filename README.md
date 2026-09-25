# harness
[English](README.md) | [简体中文](README.zh-CN.md)

A minimal coding-agent harness in Rust. It talks to any mix of LLM gateways that speak the Anthropic Messages protocol or the OpenAI Responses protocol (raw HTTP with streaming).

## Requirements

- Rust 1.88+
- [ugrep](https://github.com/Genivia/ugrep) on `PATH` for the `grep` tool (`apt install ugrep` / `brew install ugrep`)

## Usage

```sh
export ANTHROPIC_API_KEY=sk-ant-...     # or OPENAI_API_KEY, or configure gateways (below)
cargo run --release                      # interactive REPL (/exit to quit)
cargo run --release -- "list the files"  # one-shot
cargo run --release -- --continue        # resume the latest conversation here (see below)
cargo run --release -- --web             # web UI at http://localhost:8787 (see below)
cargo run --release -- --sandbox         # run the model's commands in an OS sandbox (see below)
```

| Variable | Default |
|---|---|
| `HARNESS_MODEL` | `default_model` from `providers.json`, else `anthropic/claude-opus-5` |
| `HARNESS_SUBAGENT_MODEL` | `subagent_model` from `providers.json`, else the main model |
| `HARNESS_MAX_TOKENS` | `64000` (overridden per provider or model) |
| `HARNESS_EFFORT` | `effort` from `providers.json`, else `high` |
| `HARNESS_PROVIDERS` | read only this providers file |
| `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_BASE_URL` | implicit `anthropic` provider |
| `OPENAI_API_KEY`, `OPENAI_BASE_URL` | implicit `openai` provider (Responses API) |
| `HARNESS_SANDBOX` | `1` or `0`: turn the [sandbox](#sandbox) on or off (the `--sandbox` / `--no-sandbox` flags win) |
| `HARNESS_WEB_TOKEN` | web UI token (generated at startup if unset) |

## Web UI

`harness --web` (or `--web=ADDR`, default `127.0.0.1:8787`) starts a browser UI instead of the REPL:

- **任务 (Tasks)**: dispatch a task to the main agent or straight to a subagent type (`explore` or a custom agent), optionally on a specific model. Tasks run in the background, several at once; each is its own conversation that streams text, tool calls and results (subagent progress shows under its `agent` call). Continue a task with follow-up messages, switch its model, cancel it (running `bash` commands are killed) or delete it. `@path` mentions and `/<skill> args` work as in the REPL.
- **工具 (Tools)**: every registered tool (built-in, skill, MCP, `agent`) with its description and schema; fill in JSON input and call it directly.
- **模型配置 (Models)**: the active providers and models, and a form or raw-JSON editor for `.harness/providers.json` (or `$HARNESS_PROVIDERS`). A save is validated against the other config files and the environment first, then applied without a restart; idle tasks move to the new config.
- **Agents / Skills / MCP**: what was discovered and which MCP servers connected.

The server starts even without a usable provider, so the first model can be configured in the browser. The API can run commands, so it is guarded: every API request needs a token, even on a loopback address (other users of the machine can reach that too), and the startup line prints a URL that carries it (`http://HOST:PORT/#token=...`; the browser remembers it for later tabs); on a loopback address, requests must also use a loopback host name (no DNS rebinding); and state-changing requests need an `X-Harness` header (no cross-site requests). The providers file the UI saves is readable by its owner only.

| Endpoint | |
|---|---|
| `GET /api/info` | active providers, the editable config file, agents, skills, MCP status |
| `POST /api/config` `{text}` | validate, save and apply the providers file |
| `GET /api/tools`, `POST /api/tools/{name}` `{input}` | list / call tools |
| `GET/POST /api/tasks` `{prompt, agent?, model?, title?}` | list / create tasks |
| `POST /api/tasks/{id}/messages` `{prompt}`, `/model` `{model}`, `/cancel`; `DELETE /api/tasks/{id}` | drive a task |
| `GET /api/tasks/{id}/events` | server-sent events: the task's log replayed, then live (resumes via `Last-Event-ID`) |

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
| `effort` | top level or per model: `low`, `medium`, `high` (default), `xhigh` or `max`. Sent as Anthropic `output_config.effort` or OpenAI `reasoning.effort`; `extra_body` still overrides it, e.g. `{"output_config": null}` for a model that takes no effort |
| `extra_body` | merged into every request body, also per model; a `null` removes a default key, e.g. `{"thinking": null}` for a model without adaptive thinking, or `{"include": null}` for a gateway that rejects encrypted reasoning |
| `fallbacks`, `eager_input_streaming` | Anthropic-only API features; default on for `api.anthropic.com`, off for gateways |
| `prompt_cache` | Anthropic automatic prompt caching; default on |
| `models` | per-model overrides; listed models show in `/model` and Tab completion |

`${VAR}` / `${VAR:-default}` expand from the environment in `base_url`, `api_key` and `headers`. A spec without a known provider prefix is a model on the current provider, so gateway model names containing `/` (such as `openai/gpt-5`) also work.

Switch models mid-conversation with `/model <provider>/<model>`; `/model` alone lists providers and models. `/effort <level>` changes the effort for the rest of the session (kept across `/model`), and `/effort auto` goes back to the configured one. The history is kept in Anthropic form and converted for OpenAI (`function_call` / `function_call_output`, `input_image`, `input_file`); requests are stateless (`store: false`), with reasoning items replayed from their encrypted content. Model-bound blocks (Anthropic thinking, OpenAI reasoning) are only sent back to the model that produced them. Subagents can use a different gateway through `subagent_model` or an agent's `model`.

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

Mentions work mid-sentence without spaces (`看看@src/main.rs的代码`); trailing punctuation is ignored. Only existing paths are attached, and an `@` right after an e-mail-style character is left alone, so `me@example.com` stays text. In the REPL, typing `@` opens a file menu under the prompt: it lists the current directory, then searches the project's files (what `git ls-files` lists, or a walk outside git) as you type, best matches first (path prefix, file name, substring, then fuzzy). ↑/↓ select, Tab or Enter fills in the path, and a directory stays open for its entries. Git-ignored entries are hidden unless you type into that directory (`@target/`). Attached text files count as read for the `write` tool. The REPL also keeps line-editing history in `~/.harness/history`.

## REPL

The assistant's Markdown is rendered as it streams (a line at a time; tables are aligned once complete). Tool calls show as `● bash(cargo test)` with the first lines of their result, subagent calls are listed under their `agent` call, and a spinner shows what is running. Each turn ends with its time, output tokens and context size. When stdout is not a terminal (or `NO_COLOR` is set), text is printed as is, without styling or the summary; in one-shot mode errors go to stderr and the exit code is non-zero.

| Command / key | |
|---|---|
| `/help` | list commands |
| `/model [spec]` | show providers and models, or switch to `<provider>/<model>` |
| `/effort [level]` | show the reasoning effort, or set it: `low`, `medium`, `high`, `xhigh`, `max`, or `auto` for the configured one |
| `/clear` | start a new conversation |
| `/resume [id]` | pick up an earlier conversation from this directory (see [Resuming](#resuming-conversations)) |
| `/tools` | list the tools the model can call |
| `/skills` | list discovered skills |
| `/agents` | list subagent types, their permission and tools |
| `/mcp` | list MCP servers, their tools and log files |
| `/sandbox` | show what the sandbox allows (see [Sandbox](#sandbox)) |
| `/<skill> [args]` | run a skill directly (also works one-shot: `harness /greet Bob`) |
| `/exit`, `exit`, Ctrl-D | quit (`exit` or `quit` only when alone on the line) |
| Ctrl-C | interrupt the running turn: running commands are killed, and the model is told the request was interrupted. At the prompt, press it twice to quit |
| `\` at line end, Alt+Enter | continue the message on a new line |
| `/` | open the command menu under the prompt; it narrows as you type, ↑/↓ select, Tab fills in, Enter runs. After `/model ` it lists models |
| `@` | open the file menu (see [Attaching files](#attaching-files-with-)) |

## Resuming conversations

Every REPL and one-shot conversation is saved after each turn to `~/.harness/sessions/<id>.jsonl` (readable by its owner only): a header line with the id, working directory, first prompt, model and times, then the history, one message per line. `/clear` starts a new file; the old one stays.

| | |
|---|---|
| `harness -c` / `--continue` | resume the latest conversation from this directory |
| `harness -r` / `--resume` | list this directory's conversations and pick one |
| `harness -r <id>` / `--resume=<id>` | resume that one; a unique prefix of the id is enough |
| `/resume`, `/resume <id>` | the same from inside the REPL |

They combine with one-shot mode: `harness -c "now run the tests"` or `harness -r <id> "..."` (a one-shot `--resume` needs the id). A resumed conversation continues under its id, on the model it last used if that is still configured, and in the REPL its last turns are shown again. Reasoning blocks are kept, so they are sent back to the model that produced them. The `write` tool's read tracking is not saved: the model reads a file again before overwriting it. Web UI tasks are not saved.

## Sandbox

`harness --sandbox` (or `HARNESS_SANDBOX=1`, or `"enabled": true` in `~/.harness/sandbox.json`) runs the model's `bash` commands and `grep` searches in an OS sandbox: Seatbelt (`sandbox-exec`, built in) on macOS, [bubblewrap](https://github.com/containers/bubblewrap) on Linux (`apt install bubblewrap` / `dnf install bubblewrap`; it needs unprivileged user namespaces, which some containers lack). `--no-sandbox` or `HARNESS_SANDBOX=0` turns it off for one run. If the sandbox cannot start, the harness exits rather than run without it.

- **Reads**: everything except `deny_read`.
- **Writes**: only the working directory, the temp directories (`/tmp`, `/var/tmp`, `$TMPDIR`, and on macOS the per-user temp and cache directories) and `allow_write`. In the working directory, `.harness/`, `.claude/`, `.mcp.json` and, in a git repository, `.git/hooks/` and `.git/config` stay read-only, because they make code run outside the sandbox later (MCP servers, hooks, `core.fsmonitor`); `.git` itself cannot be renamed or removed.
- **Network**: loopback only (local servers and tests work; DNS and outbound connections fail), unless `"network": true`.
- Commands run in a new session without a terminal. On Linux they also get their own PID namespace, so background jobs end with the command; on macOS, setuid programs (`sudo`, `ps`) cannot run.

The `read` and `write` tools check the same rules. `/sandbox` shows them, the REPL banner says `sandboxed`, and the system prompt tells the model the limits, so it reports a blocked action instead of working around it. MCP servers and the harness itself are not sandboxed.

```json
{
  "enabled": true,
  "network": false,
  "allow_write": ["~/.cargo/registry", "~/.npm"],
  "deny_read": ["~/.ssh", "~/.aws", ".env"]
}
```

Paths take `~/` and `${VAR}`; relative ones are relative to the working directory. Where rules overlap the deeper path wins, so `"deny_read": ["~"]` hides the home directory while a working directory inside it stays usable. The config is only read from the home directory, so a repository cannot switch the sandbox off. When git's metadata is outside the working directory (a worktree, or a subdirectory of the repository), commits need its path in `allow_write`. On Linux, a protected path that does not exist yet is covered by an empty stand-in while commands run, removed once none is running.

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

- `src/main.rs` — startup: config, tools, skills, MCP servers, agents; then the CLI or web frontend
- `src/cli/` — terminal frontend: the REPL and one-shot mode (`mod.rs`), the spinner and turn display (`output.rs`), streaming Markdown rendering (`markdown.rs`), display-width helpers (`text.rs`), the menu under the prompt: commands, project file search (`menu.rs`), rustyline helper with menu keys, completion and multi-line input (`repl.rs`)
- `src/web/` — web frontend (`--web`): axum server and JSON/SSE API (`mod.rs`), background tasks and their event logs (`tasks.rs`), the single-page UI (`index.html`, embedded in the binary)
- `src/agent.rs` — agent loop: request → run tools (concurrently) → send results → repeat until `end_turn`
- `src/llm/` — LLM gateways
  - `config.rs` — `providers.json`, model spec resolution, per-provider/model settings
  - `anthropic.rs` — Messages API backend (SSE into content blocks, thinking, prompt caching, refusal fallbacks)
  - `openai.rs` — Responses API backend (history ⇄ input/output item conversion, encrypted reasoning)
  - `http.rs` — POST with retries, SSE reader
- `src/session.rs` — saved conversations: the session files, `--continue` / `--resume` flags
- `src/mentions.rs` — `@path` parsing and expansion into attachment blocks
- `src/sandbox.rs` — the OS sandbox: config, the Seatbelt profile and bwrap arguments, the path rules `read` and `write` check
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
