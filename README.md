# harness
[English](README.md) | [简体中文](README.zh-CN.md)

A minimal coding-agent harness in Rust. It talks to any mix of LLM gateways that speak the Anthropic Messages protocol or the OpenAI Responses protocol (raw HTTP with streaming).

## Requirements

- Rust 1.88+
- [ugrep](https://github.com/Genivia/ugrep) on `PATH` for the `grep` and `glob` tools (`apt install ugrep` / `brew install ugrep`)
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
harness --plugin-dir ./my-plugin        # load a plugin from a directory for this run (see below)
harness provider add relay              # add an LLM gateway, asking for its URL, key and models (see below)
harness plugin install fmt@team         # install a plugin (see below; harness plugin help)
harness --check-plugins                 # report what each enabled plugin loads, and what not
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
| `hooks` | [hooks](#hooks) |
| `enabledPlugins`, `extraKnownMarketplaces`, `pluginConfigs` | [plugins](#plugins) |

Every key is optional, and unknown keys are an error. Nothing is read from the project directory (a project's `.mcp.json` included): a repository could otherwise send requests carrying your API keys to its own server, start MCP commands or hooks, or loosen the sandbox. The project's `.harness/agents`, `.harness/skills`, their `.claude/` counterparts and `.agents/skills` are still read, as they only add prompts (a skill's `` !`command` `` lines run like the model's own `bash` commands, in the sandbox when it is on). Plugins, whose hooks and MCP servers run commands outside it, are enabled only here. Older versions read `~/.harness/providers.json`, `mcp.json` and `sandbox.json`; the harness warns while they exist, and their contents go under the keys above (`default_model` is now `model`).

## Web UI

`harness --web` (or `--web=ADDR`, default `127.0.0.1:8787`) starts a browser UI instead of the REPL:

- **任务 (Tasks)**: dispatch a task to the main agent or straight to a subagent type (`explore`, `general-purpose` or a custom agent), optionally on a specific model. Tasks run in the background, several at once; each is its own conversation that streams text, tool calls and results. Each subagent gets a card with its tool calls, status, report and a Stop button, and a background subagent's report starts a turn by itself when the task is idle. Continue a task with follow-up messages, switch its model, cancel it (running `bash` commands and foreground subagents are stopped; background subagents carry on) or delete it (which stops its subagents too). `@path` mentions, `/<skill> args`, `/init` and `/compact` work as in the REPL.
- Assistant text is rendered as Markdown as it streams (code blocks, lists, tables, links); a tool call's summary line shows what it is about (the `bash` command, the file path, the `grep` pattern). Model switching and deletion sit under the `⋯` menu of the task header.
- **工具 (Tools)**: every registered tool (built-in, skill, MCP, `agent`) with its description and schema; fill in JSON input and call it directly.
- **模型配置 (Models)**: the active providers and models, and a form or raw-JSON editor for the settings file. A save is validated with the environment first; the model setup then applies without a restart, and idle tasks move to it (`mcpServers`, `sandbox` and `hooks` changes apply on the next start).
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

`harness provider` edits these settings from the command line (`/provider …` in the REPL does the same, and switches the conversation to the changed setup at once, on the same model if its provider is still there):

```sh
harness provider add relay-b       # asks for the protocol, base URL, API key and models; options skip the questions:
harness provider add relay-b --protocol openai-responses --base-url https://relay-b.example.com/v1 \
  --api-key '${RELAY_B_KEY}' --model gpt-5 --model gpt-5-mini
harness provider model add relay-b/gpt-5 --context-window 400000 --effort high --default   # also --max-tokens
harness provider default relay-a/claude-opus-5
harness provider list              # also: remove <name>, model remove <provider>/<model>
```

`add` on an existing provider changes only what its options give (with no options, it asks again, Enter keeping each value). The key is stored as typed, so `${VAR}` (in single quotes on the shell) keeps it out of the file. When there is no working default model, `add` makes its first model the default. Each change says when the setup cannot be used yet, for example without a default model.

Switch models mid-conversation with `/model <provider>/<model>`, or run `/model` without arguments to open the model settings panel. Tab switches between the main model and subagent default, ↑/↓ selects a model, typing filters (Enter takes the highlighted model; filtering alone changes nothing), and ←/→ adjusts the shared session effort (`auto`, `low`, `medium`, `high`, `xhigh`, `max`). The panel shows both model choices, protocol, configured context/output limits and effort. Enter applies all previewed changes to this session; Esc cancels them. Subagents can follow the main model or use a separate default; agent-specific models still take priority and existing children keep their original model. Redirected input still prints providers, models and effective settings. `/effort <level>` changes the effort for the rest of the session (kept across `/model`), and `/effort auto` goes back to the configured one. The history is kept in Anthropic form and converted for OpenAI (`function_call` / `function_call_output`, `input_image`, `input_file`); requests are stateless (`store: false`), with reasoning items replayed from their encrypted content. Model-bound blocks (Anthropic thinking, OpenAI reasoning) are only sent back to the model that produced them. Subagents can use a different gateway through `subagent_model` or an agent's `model`.

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

The assistant's Markdown is rendered as it streams (a line at a time; tables are aligned once complete). Tool calls show as `● bash(cargo test)` with the first lines of their result, a foreground subagent's calls are listed under its `agent` call, a background subagent gets a line when it ends, and a spinner shows what is running. Each turn ends with its time, output tokens and context size. When stdout is not a terminal, text is printed as is, without styling or the summary; in one-shot mode errors go to stderr and the exit code is non-zero.

On an interactive terminal, startup claims the current screen and places the conversation at the top, preserving the shell and conversation in native terminal scrollback. The input box stays between two rules at the bottom, while waiting and while a turn runs. Commands and files open a menu below it; the box moves up to make room. One renderer handles text wrapping, cursor placement, menus and resizing, including when `NO_COLOR` disables colors. After a terminal reset or clear, the next edit, resize or running status redraw restores the viewport. What you type while the model works goes into it: press Enter and the message is listed above the box until the model gets it with its next request (after the tool calls running then; if the model was writing its answer, it carries on instead of ending the turn), and then it shows in the conversation. `@path` mentions and Ctrl+V work there too; the menus and history only at the prompt. A `/command` sent while a turn runs waits for the turn to end. To change your mind, press ↑ on the box's first line (the empty box says so while messages are queued): the messages the model has not got yet come back into the box, ahead of what you typed, to edit and send again, as in Claude Code. If you interrupt the turn, messages the model has not got yet go back into the box, and anything left in the box is still there at the prompt after the turn.

Each completed tool call is printed together with its own result, including repeated calls to the same tool in a concurrent batch. The normal transcript shows a compact preview. Press Ctrl+O or run `/output` to browse the expanded conversation: prompts, reasoning, replies and all retained tool results appear together, with commands above their output. Press `i` or Tab to show/hide full JSON inputs and call IDs. Use ↑/↓ or `j`/`k` to scroll, PgUp/PgDn or Space/`b` to page, Home/End or `g`/`G` to jump to the beginning/end, `/` then Enter to search, and `n`/`N` for the next/previous match. Esc cancels an active search; otherwise Esc, `q` or Ctrl+O closes the viewer. It opens at the bottom and follows new output until you scroll up; `G` resumes following. The footer stays at the bottom. This also works while the model runs without submitting or discarding your input draft. `/resume` restores saved conversation content, and `/clear` clears the viewer. Full returned text means the text retained by the tool; a tool's own output limits still apply (for example, `bash` retains up to 30,000 bytes and reports truncation). Image/document attachments are labeled rather than decoded in this text viewer.

Interactive command panels use the available terminal width, with headings and spacing and no borders; each item's description sits beside its name when there is room, and the one selected is shown in full underneath when it doesn't fit. Bare selection commands open keyboard panels: `/effort`, `/resume`, `/rewind`, `/tasks`, `/stop`, `/provider` and `/plugins`. Use ↑/↓ to select, type to filter, Enter to continue and Esc to cancel or go back. `/help` and `/skills` can insert a command into the input box for you to edit before running it. `/tools`, `/agents`, `/mcp`, `/usage` and `/sandbox` open searchable read-only inspectors; Tab shows the selected item’s full details, ↑/↓ scroll, and Tab/Esc returns. `/stop` and `/tasks` just say so when there is nothing to show, and `/rewind` offers to restore the code only when it changed and can be restored. Commands with explicit arguments and redirected input keep their line-based behavior. `/clear`, `/compact`, `/init`, `/reload-plugins` and `/exit` remain direct actions.

| Command / key | |
|---|---|
| `/help` | browse commands; Enter inserts one into the prompt |
| `/model [spec]` | configure main/subagent models and effort, or switch the main model directly to `<provider>/<model>` |
| `/provider [command]` | open provider management;  `/provider add`, `remove`, `model add`, `model remove` and `default` work as `harness provider` does and apply at once (see [LLM gateways](#llm-gateways)) |
| `/effort [level]` | choose reasoning effort, or set it directly: `low`, `medium`, `high`, `xhigh`, `max`, or `auto` for the configured one |
| `/clear` | start a new conversation |
| `/compact [focus]` | summarize the conversation to free context (see [Compaction](#compaction)); `focus` says what the summary should keep |
| `/resume [id]` | pick up an earlier conversation from this directory (see [Resuming](#resuming-conversations)) |
| `/rewind` | go back to before an earlier prompt, with the code, the conversation or both (see [Rewinding](#rewinding)) |
| `/init [notes]` | have the model write the project's `AGENTS.md`, or improve it (see [Project notes](#project-notes)) |
| `/usage` | show the tokens used since the REPL started, per model (requests, input, cache reads and writes, output; subagents and compaction included, kept across `/clear` and `/resume`), and about how much of the context window the conversation takes |
| `/tools` | browse tool descriptions and input schemas |
| `/output` | browse the expanded conversation; also Ctrl+O while running |
| `/skills` | browse discovered skills; Enter inserts an invocation |
| `/agents` | browse subagent types, models, permissions and tools |
| `/tasks` | list the subagents this conversation started (id, status, model, tool calls, time, a running one's latest call) and its [background commands](#background-commands) |
| `/stop [id]` | choose one or all running tasks to stop; an id stops that subagent or background command (`shell_1`) directly (see [Subagents](#subagents)) |
| `/mcp` | list MCP servers, their tools and log files |
| `/plugins [command]` | open plugin management;  `/plugins update` fetches their latest versions, and `/plugins install`, `uninstall`, `enable`, `disable`, `configure`, `marketplace …` work as `harness plugin` does (see [Plugins](#plugins)) |
| `/reload-plugins` | load the plugins, skills, commands, agents and hooks again from the settings, for the next conversation: at once if this one is empty, else from `/clear` or `/resume` on |
| `/sandbox` | show what the sandbox allows (see [Sandbox](#sandbox)) |
| `/<skill> [args]` | run a skill directly (also works one-shot: `harness /greet Bob`) |
| `/exit`, `exit`, Ctrl-D | quit (`exit` or `quit` only when alone on the line) |
| Esc | interrupt the running turn, like Ctrl-C |
| Esc Esc | at an empty prompt: `/rewind` |
| ↑ while a turn runs | take the queued messages the model has not got yet back into the input box, to edit |
| Ctrl-C | interrupt the running turn: running commands are killed, and the model is told the request was interrupted. At the prompt, press it twice to quit |
| `\` at line end, Alt+Enter | continue the message on a new line (Shift+Enter also works when the terminal sends a distinct key sequence) |
| `/` | open the command menu under the prompt; it narrows as you type, ↑/↓ select, Tab fills in, Enter runs. Run `/model` to configure main/subagent models and effort; `/model ` also offers model completion |
| `@` | open the file menu (see [Attaching files](#attaching-files-with-)) |
| Ctrl+V | paste an image from the clipboard (see [Pasting images](#pasting-images)) |
| Ctrl+O | open/close the expanded transcript, including during a running turn |

## Background commands

`bash` with `run_in_background: true` starts a command that keeps running, such as a dev server, a watcher or a long build, and returns its id (`shell_1`) at once. `bash_output` returns what it printed since the last look (stdout and stderr together, the newest 30000 bytes of it; up to 1 MB is kept) and whether it still runs; `kill_shell` stops it and the processes it started. A command that exits by itself is reported to the model, which then gets a turn if it is idle, like a background subagent's report. Background commands run under the same sandbox and file-change rules as other `bash` commands, at most 8 at once per conversation. `/tasks` browses them, `/stop shell_1` stops one, and `/stop` lets you choose one or all of them (the model is told); `/clear`, `/resume`, deleting a web task or quitting kills them.

Finite background commands default to `background_kind: "task"`. One-shot mode waits for them and background subagents before exiting; interactive and web views distinguish waiting for results from completion. Use `background_kind: "service"` for a dev server or watcher, with an explicit `resource` such as `service:web`; services do not hold task completion and must be checked for readiness separately.

When the next step depends on a result, call `wait_tasks` with `task_ids` (shell IDs or subagent IDs), `mode: "any"` or `"all"` (default), and optional `timeout_ms` (default 60000, maximum 300000). It suspends without polling the model, returns statuses and completed results, and acknowledges pending duplicate completion notifications. A timeout, new user message, or interrupted wait leaves background tasks running; stop them explicitly when no longer needed. `bash_output` remains an immediate, incremental output check. Required work must be collected before reporting success.

Tool results appear as soon as they finish, paired by call ID, while the model receives one complete batch in call order. Independent reads overlap; unknown mutating tools and commands serialize on the workspace. A background command retains its resource guard until it exits or is stopped. Set `resource` only for work that can safely overlap the workspace, for example `host:aliyun`; commands using the same resource serialize. File changes retain the existing read-before-write and stale-file checks. These guards coordinate harness tools, not external programs or unnamed remote resources. No command-dependency inference or automatic worktree isolation is performed.

## Compaction

Before a request would leave too little of the model's `context_window` for a full response (`max_tokens`) plus a margin of 5% of the window, the model summarizes the conversation and the summary replaces it, as in Codex. With a 256k window and the default `max_tokens` of 64k that is at about 179k tokens; with a 1M window, at about 886k. The summary is asked for with an ordinary request (the same system prompt and tools, so the cached prefix still applies) that ends in a summarization prompt: progress and decisions, the user's constraints, files involved, errors and their fixes, what remains and the next step. The new history is one message: the user's own messages, word for word up to about 20k tokens counting from the newest, then the summary and the 5 files read most recently, as they are now (each cut to about 5k tokens, and all of them to 10% of the window). The assistant turns, tool results and attachments are gone, and only the re-attached files still count as read for `edit` and `write`.

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

## Rewinding

As in Claude Code, every prompt you send in the REPL is a checkpoint: before `edit` or `write` first changes a file after it, the file is kept as it was (or noted as not there yet). `/rewind`, or Esc twice at an empty prompt, lists the conversation's prompts, the most recent first, with how many files each changed. Pick one by its number, see which files going back to before it would change (`+3 -1`, `removed`, `recreated`), and choose what to restore:

| | |
|---|---|
| code and conversation | both of the below |
| conversation | that prompt and everything after it leave the conversation, and the prompt goes back into the input box to edit and send again; the files stay as they are |
| code | every file changed since is put back the way it was before that prompt, and files created since are removed; the conversation stays |

- Only changes made with `edit` and `write`, a subagent's too, are tracked. What `bash` commands, MCP tools or your editor change is not, and restoring the code overwrites what you changed since in the files it puts back: it does not replace version control.
- Messages sent from the input box while a turn runs belong to that turn's prompt; a turn for a background subagent's report makes no checkpoint.
- A compaction removes the earlier checkpoints with the conversation they belong to: only prompts after it can be rewound to.
- After rewinding the conversation, the model reads files again before changing them.
- Checkpoints are saved with the session, the kept versions in `~/.harness/file-history/<id>/` (readable by their owner only), so they still work after `--continue` or `/resume`. A conversation rewound to its first prompt is no longer saved. The web UI has no rewind.

## Sandbox

`harness --sandbox` (or `HARNESS_SANDBOX=1`, or `"enabled": true` in the settings' `sandbox` section) runs the model's `bash` commands and `grep` / `glob` searches in an OS sandbox: Seatbelt (`sandbox-exec`, built in) on macOS, [bubblewrap](https://github.com/containers/bubblewrap) on Linux (`apt install bubblewrap` / `dnf install bubblewrap`; it needs unprivileged user namespaces, which some containers lack). `--no-sandbox` or `HARNESS_SANDBOX=0` turns it off for one run. If the sandbox cannot start, the harness exits rather than run without it.

- **Reads**: everything except `deny_read`.
- **Writes**: only the working directory, the temp directories (`/tmp`, `/var/tmp`, `$TMPDIR`, and on macOS the per-user temp and cache directories) and `allow_write`. In the working directory, `.harness/`, `.claude/`, `.mcp.json` and, in a git repository, `.git/hooks/` and `.git/config` stay read-only, because they make code run outside the sandbox later (MCP servers, hooks, `core.fsmonitor`); `.git` itself cannot be renamed or removed.
- **Network**: loopback only (local servers and tests work; DNS and outbound connections fail), unless `"network": true`.
- Commands run in a new session without a terminal. On Linux they also get their own PID namespace, so background jobs end with the command; on macOS, setuid programs (`sudo`, `ps`) cannot run.

The `read`, `edit`, `write` and `lsp` tools check the same rules. `/sandbox` shows them, the REPL banner says `sandboxed`, and the system prompt tells the model the limits, so it reports a blocked action instead of working around it. MCP and language servers, [hooks](#hooks) and the harness itself are not sandboxed.

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

- Transports: `stdio` (default when `command` is set) and streamable `http` (default when `url` is set), via the official Rust SDK [`rmcp`](https://github.com/modelcontextprotocol/rust-sdk). A stdio server may set `cwd`, the directory it runs in. Servers that sign in with OAuth are not supported.
- `${VAR}` / `${VAR:-default}` are expanded from the environment in every string.
- Servers connect concurrently at startup; a server that fails is reported and skipped.
- Tools are exposed as `mcp__<server>__<tool>`. Text, embedded text resources and PNG/JPEG/GIF/WebP images are passed back to the model.
- A stdio server's stderr goes to `$TMPDIR/harness-mcp-<server>.log`.

## Hooks

Hooks run your own commands at points of the agent loop: to block dangerous commands, format files after an edit, add context to a prompt, or keep the model working until the tests pass. They follow [Claude Code's hook format](https://code.claude.com/docs/en/hooks), which Codex shares, so scripts written for those work here. Declare them under `hooks` in the [settings](#settings):

```json
{
  "hooks": {
    "PreToolUse": [
      { "matcher": "Bash", "hooks": [{ "type": "command", "command": "~/.harness/hooks/guard.sh" }] }
    ],
    "PostToolUse": [
      { "matcher": "Edit|Write", "hooks": [{ "type": "command", "command": "~/.harness/hooks/format.sh", "timeout": 30 }] }
    ],
    "Stop": [
      { "hooks": [{ "type": "prompt", "prompt": "Did the agent run the tests after its last change? $ARGUMENTS" }] }
    ],
    "Notification": [
      { "hooks": [{ "type": "command", "command": "terminal-notifier", "args": ["-message", "harness is waiting"] }] }
    ]
  }
}
```

| Event | Runs | Matcher | What it can do |
|---|---|---|---|
| `PreToolUse` | before a tool call | tool name | block the call (exit 2, or `permissionDecision: "deny"`), change its input (`updatedInput`), add context |
| `PostToolUse` | after a call that succeeded | tool name | give the model feedback (exit 2, or `decision: "block"` with a `reason`), add context, replace the result (`updatedToolOutput`) |
| `PostToolUseFailure` | after a call that failed | tool name | give the model feedback, add context |
| `UserPromptSubmit` | before your prompt is sent | — | refuse it (exit 2 or `decision: "block"`; the reason is shown to you), add context |
| `Stop` | when the model ends its turn | — | send it back to work (exit 2 or `decision: "block"`; the reason is its instruction) |
| `SubagentStart` | before a subagent's first request | agent type | add context for the subagent |
| `SubagentStop` | when a subagent ends its run | agent type | the same as `Stop`, for the subagent |
| `PreCompact` | before the conversation is [compacted](#compaction) | `manual` (`/compact`), `auto` | keep it from being compacted (exit 2 or `decision: "block"`); an automatic compaction is then skipped and the request goes as it is, which may overflow the context |
| `PostCompact` | after it was compacted | `manual`, `auto` | nothing; it gets the summary |
| `SessionStart` | before the first request of a conversation, and after `/clear`, `/resume` or a compaction | `startup`, `clear`, `resume`, `compact` | add context |
| `SessionEnd` | when a conversation that sent a request ends: before `/clear` or `/resume` replaces it, when you quit the REPL, at the end of a one-shot run, when its web task is deleted | `clear`, `resume`, `prompt_input_exit`, `other` | nothing; it gets 1.5 s unless it sets `timeout` (at most 60 s) |
| `Notification` | when the REPL's prompt, or a web task, has waited 60 s for you after a turn | `idle_prompt` | nothing; it gets `notification_type` and `message`, e.g. to show a desktop notification |

- A hook runs as `sh -c <command>` in the working directory, outside the sandbox, with `$CLAUDE_PROJECT_DIR` and `$HARNESS_PROJECT_DIR` set to it. With `args` (a list of strings), `command` is started directly with them instead, without a shell, so nothing in them is expanded or split (a plugin's `${CLAUDE_PLUGIN_ROOT}` and the like are still replaced). It gets the event as JSON on stdin: `session_id`, `transcript_path` (the saved session file), `cwd`, `hook_event_name`, `model`, `permission_mode` (always `bypassPermissions`: the harness never asks before running a tool), `agent_id` and `agent_type` in a subagent, and the event's own fields (`tool_name`, `tool_use_id`, `tool_input`, `tool_response` or `error`; `prompt`; `stop_hook_active`, `last_assistant_message`; `trigger`, `custom_instructions`, `compact_summary`; `source`, `reason`).
- Exit code 0: stdout is read as JSON if it starts with `{` (`continue` / `stopReason`, `systemMessage`, `decision` / `reason`, `hookSpecificOutput`), otherwise it is context for the model for `SessionStart`, `SubagentStart` and `UserPromptSubmit`, and ignored for the rest. Exit code 2 blocks, with stderr as the reason. Another exit code, a timeout (`timeout`, in seconds, default 600) or bad JSON is shown to you and changes nothing. `continue: false` ends the turn.
- Tools also match, and appear to hooks, under their Claude Code names: `bash` is `Bash`, `read` / `edit` / `write` / `grep` / `glob` / `lsp` are `Read` / `Edit` / `Write` / `Grep` / `Glob` / `LSP`, `agent` is `Agent` (or `Task`); MCP tools keep `mcp__<server>__<tool>`. A matcher made of names (`Edit|Write`) matches them exactly, anything else is a regular expression (`mcp__fs__.*`), and an empty one or `*` matches everything. The file tools' `path` also comes as `file_path`, and an `updatedInput` may use either.
- `permissionDecision: "ask"` refuses the call, as there is no one to ask.
- A `prompt` hook (`{"type": "prompt", "prompt": "…", "model": "…", "timeout": 30}`) asks a model instead of running a command: the conversation's, or `model` when it names one the providers know (Claude Code's `haiku` and the like do not). The event's input JSON goes in place of `$ARGUMENTS` in the prompt (after it, without one), and the model answers `{"ok": true}` or `{"ok": false, "reason": "…"}`; `ok: false` acts like exit code 2 with the reason, so a Stop prompt hook sends the model back to work. It gets 30 s unless it sets `timeout`; its requests are not counted in `/usage`.
- All the hooks matching an event run at once; a command listed twice runs once. Hooks run in subagents too (`SubagentStop` instead of `Stop`) and in web tasks.
- What hooks tell the model arrives in `<hook-feedback>` blocks; what they have for you (a `systemMessage`, a refusal, a failure) shows as a `⚑` line.
- `if` narrows a hook of a tool event to some calls, in Claude Code's permission syntax: `Bash(git commit:*)` or `Bash(git push *)` (the command, or a part of a compound one, starts so; `*` matches anything), `Edit(src/**/*.ts)` (the file path), or a tool's name alone. A hook with `async: true` runs in the background without holding up the call; what it has for the model goes with the next request. With `asyncRewake: true` it does too, and if it exits with code 2 the model is woken with its `rewakeMessage` and the hook's reason, in a turn of its own if it is idle.
- Only `command` and `prompt` hooks and the events above are supported; other hook types (`http`, `agent`, `mcp_tool`) and other events (`PermissionRequest`, …) are an error in the settings (a [plugin](#plugins)'s are left out with a warning). Hooks come only from the settings file and the plugins it enables, so a repository cannot add any; changes apply on the next start, or to the next conversation after `/reload-plugins`.

## Project notes

If the working directory has an `AGENTS.md`, its contents go into the system prompt, for the main agent and for subagents (except the built-in `explore`, which only searches). Past 40,000 bytes it is cut at a line, and the model is told to read the rest. No other file (a `CLAUDE.md`, one in a parent directory or in `~`) is looked for.

The file is read when a conversation starts: the REPL's first one, after `/clear` or `/resume`, and each new web task. `/init` has the model write it, or improve the one there is, from the code, the `README.md` and notes written for other agents (`CLAUDE.md`, `.cursorrules`, `.github/copilot-instructions.md`), which it leaves in place; `/init <notes>` says what else to cover. It also works one-shot (`harness /init`) and in the web UI.

## Skills

A skill is a directory with a `SKILL.md` (the [Agent Skills](https://agentskills.io) format):

```markdown
---
name: release-notes
description: Draft release notes from the git log. Use when asked for a changelog.
---
1. Run `git log --oneline <last-tag>..HEAD` ...
```

Skills are discovered in `.harness/skills/`, `.claude/skills/`, `.agents/skills/` (the location Codex and other agents share), `~/.harness/skills/`, `~/.claude/skills/` and `~/.agents/skills/` (earlier wins on name clashes), and come with [plugins](#plugins). Only each skill's name and description (cut to 300 characters) go to the model up front, listed in the `skill` tool; the body is loaded when the model calls that tool or when you type `/<name>`. Bundled scripts and reference files are used through the normal tools, relative to the skill's base directory, which the loaded skill states.

As in Claude Code:

- `$ARGUMENTS` in the body is replaced by the arguments, `$ARGUMENTS[N]` or `$N` by the Nth (from 0, split like a shell would), and `$name` by the one at the position of `name` in the frontmatter's `arguments` list; `\$` keeps a `$`. Arguments no placeholder took are appended as `ARGUMENTS: …`. `${CLAUDE_SKILL_DIR}` and `${CLAUDE_PROJECT_DIR}` are replaced by the skill's and the working directory.
- `` !`command` `` (at the start of a line or after a space) and ```` ```! ```` blocks are run when the skill is loaded, and replaced by what they print. They run like `bash` tool commands, in the sandbox when it is on; one that fails stops the skill from loading. A read-only subagent gets the `skill` tool only while no skill the model can load has such commands.
- `disable-model-invocation: true` keeps the model from loading a skill (only `/<name>` runs it); `user-invocable: false` keeps it out of the `/` commands.
- `context: fork` runs the skill in a [subagent](#subagents) of its own: loading it starts one with the skill's text as its prompt, and only its report comes back (for `/<name>`, the report goes to the model, which takes it from there). `agent` picks the subagent type (default `general-purpose`), `model` its model (`<provider>/<model>`; `sonnet`, `opus`, `haiku` and `inherit` mean the default), and `allowed-tools` (`Read, Grep, Bash(git status:*)`; what is in parentheses is ignored) leaves it only those of its agent's tools. A subagent, which starts no subagents, loads such a skill in place instead.

Claude Code's commands, the older form of a skill, work too: each Markdown file in `.harness/commands/`, `.claude/commands/` or the same under `~` (earlier wins) runs as `/<file name>`, and a subdirectory adds a part (`frontend/component.md` is `/frontend:component`). They take the same frontmatter (`description`, else their first line describes them) and placeholders, and the model can load them through the `skill` tool unless `disable-model-invocation` is set. A skill wins over a command of the same name.

## Subagents

The main agent can delegate a self-contained task with the `agent` tool. A subagent runs in its own fresh conversation with its own system prompt and tool set, and only its final report comes back, which keeps long searches out of the main context. Several subagents started in one response run in parallel.

- **`explore`** (built-in): a codebase search agent whose only tools are `grep` and `glob`. Use it for "where is X / which files use Y" questions.
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

- **Custom agents default to read-only**: a read-only agent only gets tools marked read-only (`read`, `grep`, `glob`, `lsp`, `skill`, and MCP tools whose server sets `readOnlyHint`), even if its `tools` list names others. `tools` narrows further. An agent without `bash` does not get `bash_output` or `kill_shell` either, having no background commands. The `agent` tool lists each type as read-only or able to modify files by the tools it actually gets, with its description cut to 400 characters.
- Subagents cannot start subagents. Their file reads use their own tracker, so they never count as reads for the main agent's `edit` and `write`.
- Model: the agent's `model`, else `subagent_model` / `HARNESS_SUBAGENT_MODEL`, else the model the conversation is on now (after any `/model` or `/effort`); any `<provider>/<model>`. Claude Code's `sonnet`, `opus` and `haiku` aliases count as unset, like `inherit`.
- Tools may be named as in Claude Code (`Read`, `Bash`, `KillShell`); they map to the harness's own (`LS` to `glob`).
- `/agents` lists the available types.

### Background subagents and follow-ups

- **Foreground** (the default): the `agent` call waits for the report. Interrupting the turn stops the subagent.
- **Background** (`run_in_background: true`): the call returns at once with the subagent's `agent_id`, and the main agent carries on. When the subagent is done, its report goes to the main agent with its next request: during a turn, after the tool calls running then (or, if the model was writing its answer, it carries on instead of ending the turn); after the turn, in a turn of its own. The REPL starts that turn while the prompt is empty. Anything you have typed is kept, and the report goes along with your message instead. One-shot mode waits for background subagents before it exits, showing what each did last, and a web task starts the turn when it is idle. Background subagents keep running when you interrupt the turn.
- **Follow-ups**: every subagent keeps its conversation. `message_agent` runs a finished or stopped one again on a new message, with its context intact (waiting for its report, or in the background). A running one gets the message at its next step, and the main agent is told how far it has got (tool calls, time, latest call). `stop_agent` stops one; changes it made remain.
- `/tasks` lists this conversation's subagents; `/stop <id>` stops one and `/stop` every running one. The main agent hears about subagents you stopped with your next message. In the web UI each subagent's card has a Stop button.
- Agents running at once (the main agent, its subagents, web tasks) change files one at a time: `edit` and `write` refuse a file another changed since this one read it, rather than write over that change.
- At most 8 subagents of a conversation run at once, and the 32 most recent are kept for follow-ups. They live in memory only: `/clear`, `/resume`, deleting a web task or quitting stops and forgets them.

## Plugins

Plugins are in [Claude Code's format](https://code.claude.com/docs/en/plugins-reference): a directory bundling skills, commands, subagents, hooks, MCP and language servers, usually installed from a marketplace (a repository listing plugins in `.claude-plugin/marketplace.json`). [Codex's plugins](https://developers.openai.com/codex/plugins/build) load as well (see below). `harness plugin` installs and manages them (`/plugins …` in the REPL does the same):

```sh
harness plugin marketplace add your-org/plugins   # owner/repo, a git URL (#ref for a branch or tag), a directory or a marketplace.json
harness plugin install fmt@team                   # fetch it, enable it, and ask for the settings it needs
harness plugin list --available                   # the installed ones, and what the known marketplaces offer
harness plugin disable fmt@team                   # also: enable, uninstall [--keep-data], configure, update
harness plugin marketplace remove team            # also: marketplace list; add --auto-update
harness --check-plugins                           # load them all, say what loaded and what not, and exit
```

They edit the [settings](#settings), keeping the rest of the file as it is; plugins are enabled there under Claude Code's keys, by `<plugin>@<marketplace>`, which you may also write yourself:

```json
{
  "enabledPlugins": {
    "commit-commands@claude-plugins-official": true,
    "fmt@team": true
  },
  "extraKnownMarketplaces": {
    "team": { "source": { "source": "github", "repo": "your-org/plugins" }, "autoUpdate": true }
  },
  "pluginConfigs": {
    "fmt@team": { "options": { "line_width": 100 } }
  }
}
```

- `claude-plugins-official` ([anthropics/claude-plugins-official](https://github.com/anthropics/claude-plugins-official)) and Codex's `openai-curated` ([openai/plugins](https://github.com/openai/plugins)) are known without declaring them, and so is Codex's personal marketplace, `~/.agents/plugins/marketplace.json`, by the name it gives itself. Other marketplaces come from `github` (`repo`, `ref`, `path`), `git` (`url`, `ref`, `path`), `directory` (`path`, read in place, so edits apply at the next start) or `file` (`path` to a `marketplace.json`).
- A marketplace's plugins may be in it (a relative path, or a bare name under `metadata.pluginRoot`), in repositories of their own (`github`, `url` and `git-subdir` sources, with `ref` / `sha`), or npm packages (`npm` sources: `package`, `version`, `registry`; fetched with `npm pack --ignore-scripts`, so `npm` must be installed, and the package's scripts never run). `archive` and `command` sources are not supported.
- What is missing is fetched at startup (`git clone --depth 1`, `npm pack`) into `~/.harness/plugins`, each kept by version: `marketplaces/<name>/<version>/` and `cache/<marketplace>/<plugin>/<version>/`, where the version is the commit or the package's version and `current` names the one in use. `/plugins update` (or `harness plugin update`) fetches the latest version of every marketplace and plugin (one pinned to a `sha` stays) beside the old one; it applies at the next start, or after `/reload-plugins`. Hooks and MCP servers already running keep reading the old version, which is deleted once no running harness process uses it.
- A marketplace with `"autoUpdate": true` (`marketplace add --auto-update`) is fetched again in the background at every start, with its plugins; the new versions apply from the next start, and what happened goes to `~/.harness/plugins/auto-update.log`. Without it, versions move only when you update.
- A plugin's data directory (`${CLAUDE_PLUGIN_DATA}`) is `~/.harness/plugins/data/<id>`; `uninstall` deletes it unless given `--keep-data`.
- `--plugin-dir DIR` loads the plugin in `DIR` for one run, as `<name>@inline`, as Claude Code's flag does.

Each part of a plugin is named after it, as in Claude Code:

| Part | Where | As |
|---|---|---|
| Skills | `skills/<name>/SKILL.md`, plus the manifest's `skills` directories | `/<plugin>:<name>` and the `skill` tool |
| Commands | `commands/**/*.md`, or what the manifest's `commands` names (files, directories, or a map to a `source` file or `content`) | `/<plugin>:<name>` (a subdirectory adds a part: `/<plugin>:db:migrate`); the model can load them as skills unless `disable-model-invocation` |
| Agents | `agents/**/*.md`, or the manifest's `agents` | `<plugin>:<name>` [subagents](#subagents) |
| Hooks | `hooks/hooks.json` plus the manifest's `hooks` | run with yours (see [Hooks](#hooks)) |
| MCP servers | `.mcp.json` plus the manifest's `mcpServers` | `plugin:<plugin>:<server>`, tools `mcp__plugin_<plugin>_<server>__<tool>` |
| Language servers | `.lsp.json` plus the manifest's `lspServers` | the `lsp` tool (see [Language servers](#language-servers)) |
| Executables | `bin/` | on the `PATH` of the model's commands, after yours; the system prompt names them |

- `.claude-plugin/plugin.json` is optional; without it a marketplace entry stands in for it, and the plugin's name is the directory's. Manifest paths must stay inside the plugin.
- Codex's and Cursor's layouts are found too: catalogs in `.agents/plugins/marketplace.json` or `.cursor-plugin/marketplace.json` (their paths start from the directory holding `.agents/`), `local` sources and `url` sources with a `path`, manifests in `.codex-plugin/plugin.json` or `.cursor-plugin/plugin.json`, and an [Agent Plugins](https://agent-plugins.org) `plugin.json` at the root (skills in `skills/`, MCP servers in `mcp.json`, hooks under `extensions."com.openai"`). Hooks and MCP servers also get Codex's `PLUGIN_ROOT` / `PLUGIN_DATA`. Codex's apps (ChatGPT connectors) are left out, and of its MCP settings `cwd` is kept (inside the plugin) while comments and timeouts are dropped.
- `${CLAUDE_PLUGIN_ROOT}` and `${CLAUDE_PLUGIN_DATA}` (or `${PLUGIN_ROOT}` and `${PLUGIN_DATA}`) are replaced in hook commands, MCP and language server configs, and skill, command and agent text; hooks and stdio MCP and language servers also get them as environment variables.
- A plugin's `userConfig` declares settings it needs (`type`: `string`, `number`, `boolean`, `directory` or `file`; `title`, `description`, `default`, `sensitive`). `install` and `configure` ask for them; the answers go under `pluginConfigs` in the settings, except `sensitive` ones, which go to `~/.harness/plugins/secrets.json` (readable only by you). They replace `${user_config.<key>}` in hook commands and MCP and language server configs, and, unless sensitive, in skill, command and agent text, which the model reads; hooks and MCP and language servers get them as `CLAUDE_PLUGIN_OPTION_<KEY>` too. A plugin missing an answer that has no default loads with a warning naming `harness plugin configure`.
- Plugin agents get their `tools` as in Claude Code, where that list is all an agent gets: they may write (`bash`, `edit`, `write`) unless they say `permission: read-only`, and without `tools` they get every tool but the subagent ones. Unlike your [custom agents](#subagents), which stay read-only by default: you enabled the plugin, and its agents were written for these rules. Tools the harness lacks (`WebFetch`, `WebSearch`, `TodoWrite`) are skipped. As plugins' skills, commands and agents name Claude Code's tools (`Task`, `Bash`, `Read`, …), the system prompt (a plugin agent's too) says which tool each name is here and which have no counterpart.
- What the harness cannot use is left out with a warning at startup, and the rest of the plugin loads: hook events or types it does not run, MCP bundles (`.mcpb`), MCP servers that sign in with OAuth or whose config it does not accept, language servers whose command is not installed. Output styles, monitors, channels, a default `settings.json` and `dependencies` (which are not installed with it) are not used; `/plugins` and `--check-plugins` list them.
- `/plugins` (and the web UI's environment page) lists each plugin with what it adds, what was left out, or why it was not loaded. `harness --check-plugins` (with `--plugin-dir` too, to try a plugin you are writing) loads every enabled plugin without starting a session, starts their MCP servers, checks their language servers' commands, says what loaded, what was left out and why, and exits non-zero if a plugin or a server failed.
- `/reload-plugins` reads the settings again and loads the plugins, skills, commands, agents and hooks anew, with the plugins' MCP servers. A conversation keeps its tools and system prompt (the prompt cache's prefix), so they apply to the next one: at once if the conversation is still empty, else from `/clear` or `/resume` on. Providers, the sandbox and your own MCP servers need a restart, as does the web UI.
- Plugins run commands (hooks, MCP and language servers, `!` lines in their commands), so only your settings and the command line enable them, never the project.

### Installing a plugin from scratch

A marketplace is a directory (or repository) listing plugins. Make one with a plugin that has a command, a hook and a setting:

```sh
mkdir -p ~/my-plugins/.claude-plugin ~/my-plugins/hello/.claude-plugin ~/my-plugins/hello/commands ~/my-plugins/hello/hooks
cat > ~/my-plugins/.claude-plugin/marketplace.json <<'EOF'
{ "name": "mine", "owner": { "name": "me" }, "plugins": [{ "name": "hello", "source": "./hello", "description": "Says hello" }] }
EOF
cat > ~/my-plugins/hello/.claude-plugin/plugin.json <<'EOF'
{ "name": "hello", "version": "1.0.0",
  "userConfig": { "name": { "type": "string", "title": "Your name", "default": "friend" } } }
EOF
cat > ~/my-plugins/hello/commands/greet.md <<'EOF'
---
description: Greet the user
---
Greet ${user_config.name} in one sentence, then list the files in this directory.
EOF
cat > ~/my-plugins/hello/hooks/hooks.json <<'EOF'
{ "hooks": { "SessionStart": [{ "hooks": [{ "type": "command", "command": "echo 'Today is '$(date +%A)" }] }] } }
EOF
```

Then add the marketplace, install the plugin (it asks for `name`), check it, and use it:

```sh
harness plugin marketplace add ~/my-plugins   # added marketplace mine with 1 plugins
harness plugin install hello@mine             # Your name (string) [friend; Enter keeps it]: Ada
harness --check-plugins                       # ✓ hello@mine v1.0.0  1 command, hooks (SessionStart)
harness /hello:greet                          # or /hello:greet in the REPL (after /reload-plugins if it was running)
```

The settings now enable `hello@mine` and hold `"pluginConfigs": {"hello@mine": {"options": {"name": "Ada"}}}`. A marketplace in a git repository is added the same way (`harness plugin marketplace add your-org/plugins`), and is cloned rather than read in place.

### Language servers

A plugin's language servers (Claude Code's `.lsp.json` format) give the model an `lsp` tool:

```json
{ "go": { "command": "gopls", "args": ["serve"], "extensionToLanguage": { ".go": "go" } } }
```

- Each takes `command`, `args`, `env`, `extensionToLanguage`, `initializationOptions` and `settings` (what it gets for `workspace/configuration`); only the `stdio` transport is supported, and Claude Code's timeouts and restart settings are ignored. The command must be installed: `--check-plugins` says if it is not.
- A server starts the first time the tool asks about a file with one of its extensions, in the working directory, and stays up until the harness exits (or `/reload-plugins` replaces it). Its stderr goes to `$TMPDIR/harness-lsp-<server>.log`.
- The tool's operations are `definition`, `implementation`, `references`, `hover`, `symbols` (the file's outline), `workspace_symbols` (by `query`) and `diagnostics` (the errors and warnings in the file as it is on disk; the server is sent the file's new text first). Lines and columns count from 1, as `read` shows them.
- Like MCP servers, language servers run outside the [sandbox](#sandbox); the tool checks the files it names against the sandbox's read rules. It counts as read-only, so read-only subagents get it.

## Layout

- `src/main.rs` — startup: settings, `harness plugin`, `harness provider` and `--check-plugins`, the sandbox, then what `setup.rs` builds; then the CLI or web frontend
- `src/setup.rs` — what a conversation is built from: the plugins with their skills, agents, hooks, MCP and language servers (rebuilt by `/reload-plugins`), the user's MCP servers, the tool set and system prompt; `--check-plugins`; background auto-updates
- `src/settings.rs` — the settings file: its location, `--settings`, parsing into each section, editing it in place
- `src/cli/` — terminal frontend: the REPL and one-shot mode (`mod.rs`), `/rewind` (`rewind.rs`), the keyboard panels slash commands open (`dialog.rs`) and what each bare command shows in one (`commands.rs`), the `/model` panel for models and effort (`model.rs`), the spinner, input box and turn display (`output.rs`), retained tool results (`tool_output.rs`), the searchable expanded conversation (`transcript.rs`), the input box while a turn runs: its text, rendering and queued messages (`input.rs`), the keys read during a turn: Esc and editing the input box (`keys.rs`), streaming Markdown rendering (`markdown.rs`), display-width helpers (`text.rs`), the menu under the prompt: commands, project file search (`menu.rs`), shared viewport and native scrollback (`screen.rs`), prompt input, menu keys, completion, history, multi-line input, Ctrl+V and Esc Esc (`repl.rs`), reading an image or copied files from the clipboard (`clipboard.rs`)
- `src/web/` — web frontend (`--web`): axum server and JSON/SSE API (`mod.rs`), background tasks and their event logs (`tasks.rs`), the single-page UI (`index.html`, embedded in the binary)
- `src/agent.rs` — agent loop: request → run tools (concurrently) → send results → repeat until `end_turn`; the inbox that hands the model what arrives meanwhile, such as background subagent reports
- `src/agent/usage.rs` — the per-model token counts `/usage` shows, shared with the subagents
- `src/agent/compact.rs` — compaction: when to compact, the summarization request, the history that replaces the conversation
- `src/llm/` — LLM gateways
  - `config.rs` — providers from the settings and environment, model spec resolution, per-provider/model settings
  - `manage.rs` — `harness provider` and `/provider …`: adding and removing providers and listed models, the default model, by editing the settings
  - `anthropic.rs` — Messages API backend (SSE into content blocks, thinking, prompt caching, refusal fallbacks)
  - `openai.rs` — Responses API backend (history ⇄ input/output item conversion, encrypted reasoning)
  - `http.rs` — POST with retries, SSE reader; marks the stream failures (a dropped connection, an overload) after which `llm::Client` requests the response again
- `src/prompt/` — system prompts: the main agent's template (`system.md`) with its tools, environment and `AGENTS.md`, and subagents' (`mod.rs`); what `/init` asks for (`init.md`)
- `src/session.rs` — saved conversations: the session files, `--continue` / `--resume` flags
- `src/checkpoint.rs` — checkpoints for `/rewind`: the files `edit` and `write` change, kept as they were before each prompt, saved with the session; restoring them
- `src/mentions.rs` — `@path` parsing and expansion into attachment blocks; pasted images' blocks
- `src/sandbox.rs` — the OS sandbox: config, the Seatbelt profile and bwrap arguments, the path rules `read`, `edit` and `write` check
- `src/subagent.rs` — subagent definitions (`explore`, `general-purpose`, custom files), permissions, the `agent`, `message_agent` and `stop_agent` tools
- `src/subagent/runtime.rs` — a conversation's subagents: foreground and background runs, follow-ups, stopping, reports to the parent's inbox
- `src/mcp.rs` — MCP client: server configs, connections, `mcp__*` tool adapters
- `src/lsp.rs` — LSP client for plugins' language servers: configs, starting them on demand, JSON-RPC over stdio, diagnostics; the `lsp` tool
- `src/hooks.rs` — hooks: the settings' `hooks` section, matchers, Claude Code tool names, running the commands (through `sh` or directly) and prompt hooks, and reading their answers
- `src/skills.rs` — skill discovery, `SKILL.md` frontmatter parsing (`context: fork` included), commands, rendering a skill (`$ARGUMENTS`, `` !`command` ``), the `skill` tool
- `src/plugins.rs` — plugins: the settings' marketplaces, `marketplace.json` and `plugin.json`, each plugin's skills, commands, agents, hooks, MCP and language servers, its `userConfig` answers, and what of it is not used
  - `fetch.rs` — marketplaces and plugins fetched from git or npm under `~/.harness/plugins`, kept by version; updating them, and deleting versions no running process uses
  - `manage.rs` — `harness plugin` and `/plugins …`: installing, enabling, configuring and removing plugins and marketplaces by editing the settings (and the secrets file)
- `src/tools/` — `Tool` trait, registry, and `FileTracker` (shared read/edit/write state)
  - `bash.rs` — run a command via `bash -c` (timeout, output truncation); refuses commands that change files themselves (inline Python/Node/Perl scripts that write files, `sed -i`, heredocs or `echo` into a file), which go through `edit` / `write`
  - `read.rs` — read a text file with line numbers (`offset` / `limit` paging), or an image or PDF for the model to view
  - `edit.rs` — replace an exact, unique string in a file (or every occurrence with `replace_all`), so a change costs as much as the change; the same read-first rule as `write`, and edits to one file from one response apply in order
  - `write.rs` — create or overwrite a file; an existing file must have been read first and be unchanged since
  - `scheduling.rs` — shared workspace, file and named-resource execution guards
  - `progress.rs` — tool waiting events for CLI and Web observers
  - `wait.rs` — wait for one or several background commands or subagents
  - `shells.rs` — background commands (`bash` `run_in_background`): each conversation's own set, the `bash_output` and `kill_shell` tools, exit reports to the inbox
  - `todo.rs` — the `todo_write` tool: the model's checklist, sent whole each time (not registered: kept for re-enabling)
  - `grep.rs` — regex search via ugrep (PCRE, honours `.gitignore`, `files_with_matches` / `content` / `count` modes)
  - `glob.rs` — find files by glob pattern via ugrep (same `.gitignore` and sandbox rules as `grep`; `{a,b}` alternatives; newest first, at most `head_limit`, default 100)

On the official Anthropic API, tools are declared with `eager_input_streaming`, so large inputs (such as file contents for `write`) stream as they are generated. The API then no longer validates tool input, so each built-in tool parses its input into a strict struct (unknown fields rejected), and a call whose streamed JSON does not parse is not run: the model gets an `INVALID_JSON` error result instead. MCP tools keep input buffering on, so the API validates their input against the server-provided schema.

## Adding a tool

Implement `tools::Tool` (name, description, JSON schema, async `run`) and register it in `ToolRegistry::with_defaults`.
