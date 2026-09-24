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

## Layout

- `src/main.rs` — CLI frontend (REPL / one-shot) and terminal output
- `src/agent.rs` — agent loop: request → run tools (concurrently) → send results → repeat until `end_turn`
- `src/api.rs` — streaming Messages API client: SSE parsing into content blocks, retries, adaptive thinking, prompt caching, refusal fallbacks
- `src/tools/` — `Tool` trait, registry, and `FileTracker` (shared read/write state)
  - `bash.rs` — run a command via `bash -c` (timeout, output truncation)
  - `read.rs` — read a text file with line numbers (`offset` / `limit` paging)
  - `write.rs` — create or overwrite a file; an existing file must have been read first and be unchanged since
  - `grep.rs` — regex search via ugrep (PCRE, honours `.gitignore`, `files_with_matches` / `content` / `count` modes)

Tools are declared with `eager_input_streaming`, so large inputs (such as file contents for `write`) stream as they are generated. The API then no longer validates tool input, so each tool parses its input into a strict struct (unknown fields rejected), and a call whose streamed JSON does not parse is not run: the model gets an `INVALID_JSON` error result instead.

## Adding a tool

Implement `tools::Tool` (name, description, JSON schema, async `run`) and register it in `ToolRegistry::with_defaults`.
