# harness
[English](README.md) | [简体中文](README.zh-CN.md)

A minimal coding-agent harness in Rust. It talks to any mix of LLM gateways that speak the Anthropic Messages protocol or the OpenAI Responses protocol (raw HTTP with streaming).

## Requirements

- Rust 1.88+
- [ugrep](https://github.com/Genivia/ugrep) on `PATH` for the `grep` tool (`apt install ugrep` / `brew install ugrep`)
- On Linux, `wl-paste` ([wl-clipboard](https://github.com/bugaevc/wl-clipboard), Wayland) or `xclip` (X11) for [pasting images](#pasting-images); macOS needs nothing extra

## Usage

Install once, then run `harness` from any project directory:

```sh
cargo install --path .                  # into ~/.cargo/bin; or --root ~/.local for ~/.local/bin
```

```sh
export ANTHROPIC_API_KEY=sk-ant-...     # or OPENAI_API_KEY, or configure gateways (below)
harness                                 # interactive REPL (/exit to quit)
harness "list the files"                # one-shot
harness --continue                      # resume the latest conversation here (see below)
harness --web                           # web UI at http://localhost:8787 (see below)
harness --sandbox                       # run the model's commands in an OS sandbox (see below)
harness --help                          # all options
```

Rerun `cargo install --path .` after pulling changes. While developing, `cargo run --release -- <args>` works the same.

| Variable | Default |
|---|---|
| `HARNESS_SETTINGS` | read this [settings](#settings) file instead of `~/.harness/settings.json` (`--settings FILE` wins) |
| `HARNESS_MODEL` | `model` from the settings, else `anthropic/claude-opus-5` |
| `HARNESS_SUBAGENT_MODEL` | `subagent_model` from the settings, else the main model |
| `HARNESS_MAX_TOKENS` | `max_tokens` from the settings, else `64000` (overridden per provider or model) |
| `HARNESS_EFFORT` | `effort` from the settings, else `high` |
| `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_BASE_URL` | implicit `anthropic` provider |
| `OPENAI_API_KEY`, `OPENAI_BASE_URL` | implicit `openai` provider (Responses API) |
| `HARNESS_SANDBOX` | `1` or `0`: turn the [sandbox](#sandbox) on or off (the `--sandbox` / `--no-sandbox` flags win) |
| `HARNESS_WEB_TOKEN` | web UI token (generated at startup if unset) |

## Settings

All configuration lives in one file, `~/.harness/settings.json` (or `--settings FILE` / `$HARNESS_SETTINGS`):

```json
{
  "model": "relay-a/claude-opus-5",
  "effort": "high",
  "providers": { "relay-a": { "protocol": "anthropic", "base_url": "https://relay-a.example.com", "api_key": "${RELAY_A_KEY}" } },
  "mcpServers": { "exa": { "url": "https://mcp.exa.ai/mcp" } },
  "sandbox": { "enabled": true }
}
```

| Key | |
|---|---|
| `model`, `subagent_model`, `effort`, `max_tokens`, `context_window`, `providers` | models and [LLM gateways](#llm-gateways) |
| `mcpServers` | [MCP servers](#mcp-servers) |
| `sandbox` | the [sandbox](#sandbox) |

Every key is optional, and unknown keys are an error. Nothing is read from the project directory (a project's `.mcp.json` included): a repository could otherwise send requests carrying your API keys to its own server, start MCP commands or loosen the sandbox. The project's `.harness/agents`, `.harness/skills` and their `.claude/` counterparts are still read, as they only add prompts. Older versions read `~/.harness/providers.json`, `mcp.json` and `sandbox.json`; the harness warns while they exist, and their contents go under the keys above (`default_model` is now `model`).

## Web UI

`harness --web` (or `--web=ADDR`, default `127.0.0.1:8787`) starts a browser UI instead of the REPL:

- **任务 (Tasks)**: dispatch a task to the main agent or straight to a subagent type (`explore`, `general-purpose` or a custom agent), optionally on a specific model. Tasks run in the background, several at once; each is its own conversation that streams text, tool calls and results. Each subagent gets a card with its tool calls, status, report and a Stop button, and a background subagent's report starts a turn by itself when the task is idle. Continue a task with follow-up messages, switch its model, cancel it (running `bash` commands and foreground subagents are stopped; background subagents carry on) or delete it (which stops its subagents too). `@path` mentions, `/<skill> args` and `/compact` work as in the REPL.
- **工具 (Tools)**: every registered tool (built-in, skill, MCP, `agent`) with its description and schema; fill in JSON input and call it directly.
- **模型配置 (Models)**: the active providers and models, and a form or raw-JSON editor for the settings file. A save is validated with the environment first; the model setup then applies without a restart, and idle tasks move to it (`mcpServers` and `sandbox` changes apply on the next start).
- **Agents / Skills / MCP**: what was discovered and which MCP servers connected.

The server starts even without a usable provider, so the first model can be configured in the browser. The API can run commands, so it is guarded: every API request needs a token, even on a loopback address (other users of the machine can reach that too), and the startup line prints a URL that carries it (`http://HOST:PORT/#token=...`; the browser remembers it for later tabs); on a loopback address, requests must also use a loopback host name (no DNS rebinding); and state-changing requests need an `X-Harness` header (no cross-site requests). The settings file the UI saves is readable by its owner only.

| Endpoint | |
|---|---|
| `GET /api/info` | active providers, the settings file, agents, skills, MCP status |
| `POST /api/config` `{text}` | validate, save and apply the settings file |
| `GET /api/tools`, `POST /api/tools/{name}` `{input}` | list / call tools |
| `GET/POST /api/tasks` `{prompt, agent?, model?, title?}` | list / create tasks |
| `POST /api/tasks/{id}/messages` `{prompt}`, `/model` `{model}`, `/cancel`; `DELETE /api/tasks/{id}` | drive a task |
| `GET /api/tasks/{id}/subagents`, `POST /api/tasks/{id}/subagents/{agent_id}/stop` | list / stop the task's subagents |
| `GET /api/tasks/{id}/events` | server-sent events: the task's log replayed, then live (resumes via `Last-Event-ID`) |

## LLM gateways

Declare gateways under `providers` in the [settings](#settings). Each one speaks one protocol; models are addressed as `<provider>/<model>`.

```json
{
  "model": "relay-a/claude-opus-5",
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
| `max_tokens` | output cap: top level, per provider or per model |
| `context_window` | the model's context window in tokens (default `200000`): top level, per provider or per model; the conversation is [compacted](#compaction) before it fills up |
| `effort` | top level or per model: `low`, `medium`, `high` (default), `xhigh` or `max`. Sent as Anthropic `output_config.effort` or OpenAI `reasoning.effort`; `extra_body` still overrides it, e.g. `{"output_config": null}` for a model that takes no effort |
| `extra_body` | merged into every request body, also per model; a `null` removes a default key, e.g. `{"thinking": null}` for a model without adaptive thinking, or `{"include": null}` for a gateway that rejects encrypted reasoning |
| `fallbacks`, `eager_input_streaming` | Anthropic-only API features; default on for `api.anthropic.com`, off for gateways |
| `tool_result_media` | Anthropic only: images and PDFs a tool returns (`read`, MCP tools) stay inside its `tool_result`. Default on for `api.anthropic.com`; off for gateways, where they follow the tool results in the same message instead, because many gateways drop them from tool results |
| `prompt_cache` | Anthropic automatic prompt caching; default on |
| `models` | per-model overrides; listed models show in `/model` and Tab completion |

`${VAR}` / `${VAR:-default}` expand from the environment in `base_url`, `api_key` and `headers`. A spec without a known provider prefix is a model on the current provider, so gateway model names containing `/` (such as `openai/gpt-5`) also work.

Switch models mid-conversation with `/model <provider>/<model>`; `/model` alone lists providers and models. `/effort <level>` changes the effort for the rest of the session (kept across `/model`), and `/effort auto` goes back to the configured one. The history is kept in Anthropic form and converted for OpenAI (`function_call` / `function_call_output`, `input_image`, `input_file`); requests are stateless (`store: false`), with reasoning items replayed from their encrypted content. Model-bound blocks (Anthropic thinking, OpenAI reasoning) are only sent back to the model that produced them. Subagents can use a different gateway through `subagent_model` or an agent's `model`.

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

Mentions work mid-sentence without spaces (`看看@src/main.rs的代码`); trailing punctuation is ignored. Only existing paths are attached, and an `@` right after an e-mail-style character is left alone, so `me@example.com` stays text. In the REPL, typing `@` opens a file menu under the prompt: it lists the current directory, then searches the project's files (what `git ls-files` lists, or a walk outside git) as you type, best matches first (path prefix, file name, substring, then fuzzy). ↑/↓ select, Tab or Enter fills in the path, and a directory stays open for its entries. Git-ignored entries are hidden unless you type into that directory (`@target/`). Attached text files count as read for the `edit` and `write` tools. The REPL also keeps line-editing history in `~/.harness/history`.

### Pasting images

In the REPL, Ctrl+V pastes the image in the clipboard, such as a screenshot. It is Ctrl+V on macOS too, because Cmd+V belongs to the terminal, which only pastes text. The image shows in the prompt as `[Image #1]` and goes with the message as long as that placeholder is still in it, so deleting the placeholder drops the image. Numbers carry on through the conversation, so a later message can refer back to `[Image #1]`. Files copied in Finder or a file manager are pasted as `@` mentions of their paths instead, and when the clipboard has no image, a note under the prompt says so.

On macOS the clipboard is read through `osascript` (any image type an app puts there). An image whose long edge is over 2576 pixels is scaled down to that, since the API would scale it down anyway, and one still over 3.75 MB (5 MB in base64, the API's limit) is sent as a JPEG. On Linux, `wl-paste` or `xclip` supply PNG, JPEG, GIF or WebP images as they are, up to the same limit.

## REPL

The assistant's Markdown is rendered as it streams (a line at a time; tables are aligned once complete). Tool calls show as `● bash(cargo test)` with the first lines of their result, the model's todo list (`todo_write`) as a checklist with `✔` done, `◼` in progress and `☐` pending items, a foreground subagent's calls are listed under its `agent` call, a background subagent gets a line when it ends, and a spinner shows what is running. Each turn ends with its time, output tokens and context size. When stdout is not a terminal (or `NO_COLOR` is set), text is printed as is, without styling or the summary; in one-shot mode errors go to stderr and the exit code is non-zero.

The prompt is an input box between two rules that stays at the bottom, under the output and the spinner, while a turn runs too. What you type while the model works goes into it: press Enter and the message is listed above the box until the model gets it with its next request (after the tool calls running then; if the model was writing its answer, it carries on instead of ending the turn), and then it shows in the conversation. `@path` mentions and Ctrl+V work there too; the menus and history only at the prompt. A `/command` sent while a turn runs waits for the turn to end. If you interrupt the turn, messages the model has not got yet go back into the box, and anything left in the box is still there at the prompt after the turn.

| Command / key | |
|---|---|
| `/help` | list commands |
| `/model [spec]` | show providers and models, or switch to `<provider>/<model>` |
| `/effort [level]` | show the reasoning effort, or set it: `low`, `medium`, `high`, `xhigh`, `max`, or `auto` for the configured one |
| `/clear` | start a new conversation |
| `/compact [focus]` | summarize the conversation to free context (see [Compaction](#compaction)); `focus` says what the summary should keep |
| `/resume [id]` | pick up an earlier conversation from this directory (see [Resuming](#resuming-conversations)) |
| `/tools` | list the tools the model can call |
| `/skills` | list discovered skills |
| `/agents` | list subagent types, their permission and tools |
| `/tasks` | list the subagents this conversation started (id, status, model, tool calls, time) and its [background commands](#background-commands) |
| `/stop [id]` | stop a running subagent or background command (`shell_1`), or all of them (see [Subagents](#subagents)) |
| `/mcp` | list MCP servers, their tools and log files |
| `/sandbox` | show what the sandbox allows (see [Sandbox](#sandbox)) |
| `/<skill> [args]` | run a skill directly (also works one-shot: `harness /greet Bob`) |
| `/exit`, `exit`, Ctrl-D | quit (`exit` or `quit` only when alone on the line) |
| Esc | interrupt the running turn, like Ctrl-C |
| Ctrl-C | interrupt the running turn: running commands are killed, and the model is told the request was interrupted. At the prompt, press it twice to quit |
| `\` at line end, Alt+Enter | continue the message on a new line |
| `/` | open the command menu under the prompt; it narrows as you type, ↑/↓ select, Tab fills in, Enter runs. After `/model ` it lists models |
| `@` | open the file menu (see [Attaching files](#attaching-files-with-)) |
| Ctrl+V | paste an image from the clipboard (see [Pasting images](#pasting-images)) |

## Background commands

`bash` with `run_in_background: true` starts a command that keeps running, such as a dev server, a watcher or a long build, and returns its id (`shell_1`) at once. `bash_output` returns what it printed since the last look (stdout and stderr together, the newest 30000 bytes of it; up to 1 MB is kept) and whether it still runs; `kill_shell` stops it and the processes it started. A command that exits by itself is reported to the model, which then gets a turn if it is idle, like a background subagent's report. Background commands run under the same sandbox and file-change rules as other `bash` commands, at most 8 at once per conversation. `/tasks` lists them, `/stop shell_1` stops one and `/stop` all of them (the model is told); `/clear`, `/resume`, deleting a web task or quitting kills them.

## Compaction

Before a request would leave too little of the model's `context_window` for a full response (`max_tokens`) plus a margin of 5% of the window, the model summarizes the conversation and the summary replaces it, as in Codex. With a 256k window and the default `max_tokens` of 64k that is at about 179k tokens; with a 1M window, at about 886k. The summary is asked for with an ordinary request (the same system prompt and tools, so the cached prefix still applies) that ends in a summarization prompt: progress and decisions, the user's constraints, files involved, errors and their fixes, what remains and the next step. The new history is one message: the user's own messages, word for word up to about 20k tokens counting from the newest, then the summary, the latest todo list, and the 5 files read most recently, as they are now (each cut to about 5k tokens, and all of them to 10% of the window). The assistant turns, tool results and attachments are gone, and only the re-attached files still count as read for `edit` and `write`.

This happens before a turn starts (the new message stays as it is, after the summary) and between tool rounds within a turn, so a long task carries on by itself; the REPL shows `✻ Compacted the conversation: context 172.4k → about 9.8k`. `/compact [focus]` does it on demand. If the summarization request itself is too long, its oldest exchanges are left out; if the context is still too full afterwards, or fills up again three times in one turn, the turn stops with an error instead of looping. A compaction can be interrupted like a turn, which leaves the history as it was. Saved sessions store the compacted history, and a resumed one shows `✻ Conversation compacted` where it happened.

## Resuming conversations

Every REPL and one-shot conversation is saved after each turn to `~/.harness/sessions/<id>.jsonl` (readable by its owner only): a header line with the id, working directory, first prompt, model and times, then the history, one message per line. `/clear` starts a new file; the old one stays.

| | |
|---|---|
| `harness -c` / `--continue` | resume the latest conversation from this directory |
| `harness -r` / `--resume` | list this directory's conversations and pick one |
| `harness -r <id>` / `--resume=<id>` | resume that one; a unique prefix of the id is enough |
| `/resume`, `/resume <id>` | the same from inside the REPL |

They combine with one-shot mode: `harness -c "now run the tests"` or `harness -r <id> "..."` (a one-shot `--resume` needs the id). A resumed conversation continues under its id, on the model it last used if that is still configured, and in the REPL its last turns are shown again. Reasoning blocks are kept, so they are sent back to the model that produced them. The read tracking of `edit` and `write` is not saved: the model reads a file again before changing it. Web UI tasks are not saved.

## Sandbox

`harness --sandbox` (or `HARNESS_SANDBOX=1`, or `"enabled": true` in the settings' `sandbox` section) runs the model's `bash` commands and `grep` searches in an OS sandbox: Seatbelt (`sandbox-exec`, built in) on macOS, [bubblewrap](https://github.com/containers/bubblewrap) on Linux (`apt install bubblewrap` / `dnf install bubblewrap`; it needs unprivileged user namespaces, which some containers lack). `--no-sandbox` or `HARNESS_SANDBOX=0` turns it off for one run. If the sandbox cannot start, the harness exits rather than run without it.

- **Reads**: everything except `deny_read`.
- **Writes**: only the working directory, the temp directories (`/tmp`, `/var/tmp`, `$TMPDIR`, and on macOS the per-user temp and cache directories) and `allow_write`. In the working directory, `.harness/`, `.claude/`, `.mcp.json` and, in a git repository, `.git/hooks/` and `.git/config` stay read-only, because they make code run outside the sandbox later (MCP servers, hooks, `core.fsmonitor`); `.git` itself cannot be renamed or removed.
- **Network**: loopback only (local servers and tests work; DNS and outbound connections fail), unless `"network": true`.
- Commands run in a new session without a terminal. On Linux they also get their own PID namespace, so background jobs end with the command; on macOS, setuid programs (`sudo`, `ps`) cannot run.

The `read`, `edit` and `write` tools check the same rules. `/sandbox` shows them, the REPL banner says `sandboxed`, and the system prompt tells the model the limits, so it reports a blocked action instead of working around it. MCP servers and the harness itself are not sandboxed.

```json
{
  "sandbox": {
    "enabled": true,
    "network": false,
    "allow_write": ["~/.cargo/registry", "~/.npm"],
    "deny_read": ["~/.ssh", "~/.aws", ".env"]
  }
}
```

Paths take `~/` and `${VAR}`; relative ones are relative to the working directory. Where rules overlap the deeper path wins, so `"deny_read": ["~"]` hides the home directory while a working directory inside it stays usable. The config comes only from your settings file, so a repository cannot switch the sandbox off. When git's metadata is outside the working directory (a worktree, or a subdirectory of the repository), commits need its path in `allow_write`. On Linux, a protected path that does not exist yet is covered by an empty stand-in while commands run, removed once none is running.

## MCP servers

Declare servers under `mcpServers` in the [settings](#settings), in the same shape as Claude Code's `.mcp.json`. A project's `.mcp.json` is not read; copy the servers you want into your settings.

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

## Project notes

If the working directory has an `AGENTS.md`, the system prompt tells the model it is there, and the model reads it with `read` when it needs the project's conventions. Its contents are not put into the prompt, and no other file (a `CLAUDE.md`, one in a parent directory or in `~`) is looked for.

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

The main agent can delegate a self-contained task with the `agent` tool. A subagent runs in its own fresh conversation with its own system prompt and tool set, and only its final report comes back, which keeps long searches out of the main context. Several subagents started in one response run in parallel.

- **`explore`** (built-in): a codebase search agent whose only tool is `grep`. Use it for "where is X / which files use Y" questions.
- **`general-purpose`** (built-in): a coding agent with the main agent's tools (`bash`, `edit`, `write`, MCP tools and the rest, under the same sandbox) for multi-step research or a well-scoped change. Subagents share the working directory, so the main agent is told to give each one that changes files its own files.
- **Custom agents**: Markdown files in `.harness/agents/` or `.claude/agents/` (project or `~`; the first found wins, and a file named like a built-in overrides it):

  ```markdown
  ---
  name: reviewer
  description: Reviews the current diff for bugs. Use after making changes.
  tools: read, grep          # optional; `mcp__server__*` matches a prefix
  permission: read-only      # default; `write` allows bash/edit/write and non-read-only MCP tools
  model: claude-sonnet-5     # optional
  ---
  You are a code reviewer...
  ```

- **Custom agents default to read-only**: a read-only agent only gets tools marked read-only (`read`, `grep`, `todo_write`, `skill`, and MCP tools whose server sets `readOnlyHint`), even if its `tools` list names others. `tools` narrows further.
- Subagents cannot start subagents. Their file reads use their own tracker, so they never count as reads for the main agent's `edit` and `write`.
- Model: the agent's `model`, else `subagent_model` / `HARNESS_SUBAGENT_MODEL`, else the model the conversation is on now (after any `/model` or `/effort`); any `<provider>/<model>`.
- `/agents` lists the available types.

### Background subagents and follow-ups

- **Foreground** (the default): the `agent` call waits for the report. Interrupting the turn stops the subagent.
- **Background** (`run_in_background: true`): the call returns at once with the subagent's `agent_id`, and the main agent carries on. When the subagent is done, its report goes to the main agent with its next request: during a turn, after the tool calls running then (or, if the model was writing its answer, it carries on instead of ending the turn); after the turn, in a turn of its own. The REPL starts that turn while the prompt is empty. Anything you have typed is kept, and the report goes along with your message instead. One-shot mode waits for background subagents before it exits, and a web task starts the turn when it is idle. Background subagents keep running when you interrupt the turn.
- **Follow-ups**: every subagent keeps its conversation. `message_agent` runs a finished or stopped one again on a new message, with its context intact (waiting for its report, or in the background). A running one gets the message at its next step. `stop_agent` stops one; changes it made remain.
- `/tasks` lists this conversation's subagents; `/stop <id>` stops one and `/stop` every running one. The main agent hears about subagents you stopped with your next message. In the web UI each subagent's card has a Stop button.
- At most 8 subagents of a conversation run at once, and the 32 most recent are kept for follow-ups. They live in memory only: `/clear`, `/resume`, deleting a web task or quitting stops and forgets them.

## Layout

- `src/main.rs` — startup: settings, tools, skills, MCP servers, agents; then the CLI or web frontend
- `src/settings.rs` — the settings file: its location, `--settings`, parsing into each section
- `src/cli/` — terminal frontend: the REPL and one-shot mode (`mod.rs`), the spinner, input box and turn display (`output.rs`), the input box while a turn runs: its text, rendering and queued messages (`input.rs`), the keys read during a turn: Esc and editing the input box (`keys.rs`), streaming Markdown rendering (`markdown.rs`), display-width helpers (`text.rs`), the menu under the prompt: commands, project file search (`menu.rs`), rustyline helper with the input box's rules, menu keys, completion, multi-line input, pasting with Ctrl+V and waking the prompt for a subagent's report (`repl.rs`), reading an image or copied files from the clipboard (`clipboard.rs`)
- `src/web/` — web frontend (`--web`): axum server and JSON/SSE API (`mod.rs`), background tasks and their event logs (`tasks.rs`), the single-page UI (`index.html`, embedded in the binary)
- `src/agent.rs` — agent loop: request → run tools (concurrently) → send results → repeat until `end_turn`; the inbox that hands the model what arrives meanwhile, such as background subagent reports
- `src/agent/compact.rs` — compaction: when to compact, the summarization request, the history that replaces the conversation
- `src/llm/` — LLM gateways
  - `config.rs` — providers from the settings and environment, model spec resolution, per-provider/model settings
  - `anthropic.rs` — Messages API backend (SSE into content blocks, thinking, prompt caching, refusal fallbacks)
  - `openai.rs` — Responses API backend (history ⇄ input/output item conversion, encrypted reasoning)
  - `http.rs` — POST with retries, SSE reader; marks the stream failures (a dropped connection, an overload) after which `llm::Client` requests the response again
- `src/session.rs` — saved conversations: the session files, `--continue` / `--resume` flags
- `src/mentions.rs` — `@path` parsing and expansion into attachment blocks; pasted images' blocks
- `src/sandbox.rs` — the OS sandbox: config, the Seatbelt profile and bwrap arguments, the path rules `read`, `edit` and `write` check
- `src/subagent.rs` — subagent definitions (`explore`, `general-purpose`, custom files), permissions, the `agent`, `message_agent` and `stop_agent` tools
- `src/subagent/runtime.rs` — a conversation's subagents: foreground and background runs, follow-ups, stopping, reports to the parent's inbox
- `src/mcp.rs` — MCP client: server configs, connections, `mcp__*` tool adapters
- `src/skills.rs` — skill discovery, `SKILL.md` frontmatter parsing, the `skill` tool
- `src/tools/` — `Tool` trait, registry, and `FileTracker` (shared read/edit/write state)
  - `bash.rs` — run a command via `bash -c` (timeout, output truncation); refuses commands that change files themselves (inline Python/Node/Perl scripts that write files, `sed -i`, heredocs or `echo` into a file), which go through `edit` / `write`
  - `read.rs` — read a text file with line numbers (`offset` / `limit` paging), or an image or PDF for the model to view
  - `edit.rs` — replace an exact, unique string in a file (or every occurrence with `replace_all`), so a change costs as much as the change; the same read-first rule as `write`, and edits to one file from one response apply in order
  - `write.rs` — create or overwrite a file; an existing file must have been read first and be unchanged since
  - `shells.rs` — background commands (`bash` `run_in_background`): each conversation's own set, the `bash_output` and `kill_shell` tools, exit reports to the inbox
  - `todo.rs` — the `todo_write` tool: the model's checklist, sent whole each time
  - `grep.rs` — regex search via ugrep (PCRE, honours `.gitignore`, `files_with_matches` / `content` / `count` modes)

On the official Anthropic API, tools are declared with `eager_input_streaming`, so large inputs (such as file contents for `write`) stream as they are generated. The API then no longer validates tool input, so each built-in tool parses its input into a strict struct (unknown fields rejected), and a call whose streamed JSON does not parse is not run: the model gets an `INVALID_JSON` error result instead. MCP tools keep input buffering on, so the API validates their input against the server-provided schema.

## Adding a tool

Implement `tools::Tool` (name, description, JSON schema, async `run`) and register it in `ToolRegistry::with_defaults`.
