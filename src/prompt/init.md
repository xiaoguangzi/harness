Analyze this codebase and create an `AGENTS.md` file in the working directory. Its contents are put into the system prompt of every future conversation with coding agents (harness, Codex, Claude Code and others) working in this repository.

What to add:

1. The commands used day to day: how to build, lint and run the tests, including how to run a single test.
2. The high-level architecture and code structure: the big picture that takes reading several files to understand, so a future agent can be productive quickly.

Notes:

- If there already is an `AGENTS.md`, improve it rather than starting over: keep what is still right, fix what is out of date, add what is missing.
- If the repository has notes written for other agents (`CLAUDE.md`, `.cursorrules`, `.cursor/rules/`, `.github/copilot-instructions.md`), carry their important parts into `AGENTS.md`, since that is the only file read here. Leave those files as they are.
- Use what matters from `README.md` too.
- Keep it short: it costs context in every conversation. Do not repeat yourself, and leave out generic advice ("write tests", "never commit secrets") and lists of every file or component that are easy to discover.
- Do not make up sections such as "Common tasks" or "Tips for development" unless the repository's own files say so.
- Begin the file with:

```
# AGENTS.md

This file provides guidance to coding agents when working with code in this repository.
```
