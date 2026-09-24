# harness

A minimal coding-agent harness in Rust, built on the Anthropic Messages API (raw HTTP, since there is no official Rust SDK).

## Usage

```sh
export ANTHROPIC_API_KEY=sk-ant-...     # or ANTHROPIC_AUTH_TOKEN
cargo run --release                      # interactive REPL (/exit to quit)
cargo run --release -- "list the files"  # one-shot
```

| Variable | Default |
|---|---|
| `HARNESS_MODEL` | `claude-opus-5` |
| `HARNESS_MAX_TOKENS` | `16000` |
| `ANTHROPIC_BASE_URL` | `https://api.anthropic.com` |

## Layout

- `src/main.rs` — CLI frontend (REPL / one-shot) and terminal output
- `src/agent.rs` — agent loop: request → run tools (concurrently) → send results → repeat until `end_turn`
- `src/api.rs` — Messages API client (retries, adaptive thinking, prompt caching, refusal fallbacks)
- `src/tools/` — `Tool` trait + registry
  - `bash.rs` — run a command via `bash -c` (timeout, output truncation)
  - `read.rs` — read a text file with line numbers (`offset` / `limit` paging)

## Adding a tool

Implement `tools::Tool` (name, description, JSON schema, async `run`) and register it in `ToolRegistry::with_defaults`.
