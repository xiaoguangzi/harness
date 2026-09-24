# harness

A minimal coding-agent harness in Rust, built on the Anthropic Messages API (raw HTTP, since there is no official Rust SDK).

## Requirements

- Rust 1.88+
- [ugrep](https://github.com/Genivia/ugrep) on `PATH` for the `grep` tool (`apt install ugrep` / `brew install ugrep`)

## Usage

```sh
export ANTHROPIC_API_KEY=sk-ant-...     # or ANTHROPIC_AUTH_TOKEN
cargo run --release                      # interactive REPL (/exit to quit)
cargo run --release -- "list the files"  # one-shot
```

| Variable | Default |
|---|---|
| `HARNESS_MODEL` | `claude-opus-5` |
| `HARNESS_MAX_TOKENS` | `64000` |
| `ANTHROPIC_BASE_URL` | `https://api.anthropic.com` |

## REPL commands

| Command | |
|---|---|
| `/help` | list commands |
| `/skills` | list discovered skills |
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

## Layout

- `src/main.rs` — CLI frontend (REPL / one-shot) and terminal output
- `src/agent.rs` — agent loop: request → run tools (concurrently) → send results → repeat until `end_turn`
- `src/api.rs` — streaming Messages API client: SSE parsing into content blocks, retries, adaptive thinking, prompt caching, refusal fallbacks
- `src/mcp.rs` — MCP client: config loading, server connections, `mcp__*` tool adapters
- `src/skills.rs` — skill discovery, `SKILL.md` frontmatter parsing, the `skill` tool
- `src/tools/` — `Tool` trait, registry, and `FileTracker` (shared read/write state)
  - `bash.rs` — run a command via `bash -c` (timeout, output truncation)
  - `read.rs` — read a text file with line numbers (`offset` / `limit` paging)
  - `write.rs` — create or overwrite a file; an existing file must have been read first and be unchanged since
  - `grep.rs` — regex search via ugrep (PCRE, honours `.gitignore`, `files_with_matches` / `content` / `count` modes)

Tools are declared with `eager_input_streaming`, so large inputs (such as file contents for `write`) stream as they are generated. The API then no longer validates tool input, so each built-in tool parses its input into a strict struct (unknown fields rejected), and a call whose streamed JSON does not parse is not run: the model gets an `INVALID_JSON` error result instead. MCP tools keep input buffering on, so the API validates their input against the server-provided schema.

## Adding a tool

Implement `tools::Tool` (name, description, JSON schema, async `run`) and register it in `ToolRegistry::with_defaults`.
