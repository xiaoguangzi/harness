# TODO

What is known to be missing or wrong, kept until it is fixed. Each entry says how it is known:
**seen** (reproduced with the built binary), or **read** (found reading the code, not run).

## Plugins

Review of 2026-11-10. Tested against `claude-plugins-official` (317 plugins) and `openai-curated` (65): all 116
plugins with a local source load, 12 with `url` / `git-subdir` sources install, and a turn through a real gateway
ran a plugin's skills, commands, hooks, `bin/` and stdio MCP server. Fixed then: a skill's commands keep to the
`Bash` deny and ask rules, long MCP tool names stay apart, agents' `disallowedTools`, Codex's inline `hooks`, and
MCP servers with keys the harness does not know.

### MCP servers

- [ ] **OAuth for remote servers** — the largest gap: many plugins are little more than a connector. Of the 116
  local-source plugins, 29 servers cannot be used (11 declare `oauth` and are left out, 18 answer "Auth required");
  6 of the 12 installed from remote sources. Needs discovery, client registration, a loopback PKCE callback, token
  storage and refresh, and a command to sign in; Codex's `oauth`, `oauth_resource` and `scopes` keys name what to
  ask for. Check first what `rmcp` already does. (seen)
- [ ] A failed connection prints rmcp's `Debug` text (`Send message error Transport [rmcp::transport::worker::…`).
  Say what happened; "Auth required" should say the server needs a sign-in the harness cannot do. (seen)
- [ ] A `${VAR}` that is not set becomes an empty string without a word: the `github` plugin then sends
  `Authorization: Bearer ` and gets a 400. Name the variable and leave the server out. (seen)
- [ ] `type: sse` (and `ws`) fail when connecting rather than as a warning at load; `headersHelper` and `.mcpb` /
  `.dxt` bundles are not supported. (read)
- [ ] Codex's `enabled_tools`, `disabled_tools`, `tools.<tool>.approval_mode` and `default_tools_approval_mode`
  are dropped with a warning. `enabled_tools` / `disabled_tools` could filter the server's tools. (seen)
- [ ] Two tools that end up with one name replace each other silently (`ToolRegistry::register_arc` inserts):
  `plugin:a_b:c` and `plugin:a:b_c` both become `plugin_a_b_c`. Warn. (read)
- [ ] A permission rule or hook matcher written as `mcp__plugin_<plugin>_<server>__*` does not match a tool whose
  name was shortened to fit 64 characters. (read)
- [ ] The variables a plugin's server reads for its headers are not hidden from the `bash` tool:
  `Settings::credential_vars` only looks at the settings' own servers. (read)
- [ ] A plugin server's `cwd` may leave the plugin's directory (`../x`, an absolute path); other manifest paths go
  through `inside`. Low: the server's command is the plugin's to choose anyway. (read)

### Skills and commands

- [ ] Frontmatter that is not read: `allowed-tools` outside `context: fork` (24 uses in the official local
  plugins; it should pre-approve tools when the mode asks), `disallowed-tools`, `model` and `effort` outside a
  fork, `when_to_use` (Claude Code appends it to the description, so it changes when the model picks the skill),
  `paths`, `hooks`, `shell`. (read)
- [ ] `${CLAUDE_SESSION_ID}` is not replaced (seen); `${CLAUDE_EFFORT}` and `@file` references are not either
  (read).
- [ ] A skill's `` !`command` ``: the rules are not applied when the user types `/skill args` (on purpose, like
  `!command` in the REPL; reconsider for pasted prompts), a rule that asks does not run PermissionRequest hooks,
  and arguments are filled in unquoted. (read)
- [ ] Hooks see the harness's parameter names: a `Skill` call arrives as `{name, arguments}` where Claude Code
  sends `{skill, args}`, so a hook script written for it reads nothing. Check each tool with a Claude Code
  counterpart and map the names in `hooks::input_for_hook`. (seen for `Skill`)
- [ ] The frontmatter parser is hand-written: `|` blocks lose their line breaks, and a nested map's lines are
  appended to the parent key (the comment says they are ignored). Fine for the keys read today. (read)
- [ ] Skills below `skills/<a>/<b>/SKILL.md` are not found (29 in `openai-curated`). Claude Code and the Agent
  Plugins spec look one level deep too; find out what Codex does before changing it. (seen)
- [ ] Codex's `agents/openai.yaml` next to a skill is not read: `policy.allow_implicit_invocation: false` should
  mean what `disable-model-invocation` does. (read)
- [ ] A plugin's skill, command or agent whose name is taken is dropped without a warning, and a plugin's skill
  is not checked against the user's of the same name. (read)

### Hooks

- [ ] 13 of Claude Code's 33 events run. Real plugins use `UserPromptExpansion`, `StopFailure` and
  `PostToolBatch`; Codex adds `Interrupt`, `PreModelRequest` and `PostModelRequest`. Left out with a warning. (seen)
- [ ] Types `http`, `mcp_tool` (Codex's bundled `browser` plugin) and `agent` are not run. (seen)
- [ ] `statusMessage`, `once`, `shell` and `rewakeSummary` are not read; `CLAUDE_ENV_FILE` is not provided. (read)
- [ ] On the first prompt UserPromptSubmit runs before SessionStart; Claude Code runs SessionStart first. (seen)
- [ ] Cursor's hook files (`hooks-cursor.json`: `version`, `sessionStart`, a flat `{command}`) are not understood,
  so a plugin with only a `.cursor-plugin` manifest loses its hooks. (read)

### Agents

- [ ] Frontmatter that is not read: `maxTurns` (3 uses), `skills`, `isolation`, `background`, `memory`,
  `initialPrompt`, `color`. (read)
- [ ] Tools the harness has no counterpart for are dropped from a plugin agent's list without a word
  (`WebFetch`, `WebSearch`, `TodoWrite`, `NotebookRead`, `Task*`): on purpose today, but an agent that lists only
  such tools ends up with none. (read)
- [ ] A `model` that is a Claude model id (`claude-opus-5-5`) or `fable` goes to the gateway as written, where
  only `<provider>/<model>` resolves; `sonnet`, `opus`, `haiku` and `inherit` already mean the default. (read)
- [ ] Codex plugins' `agents/*.md` have no frontmatter (8 in `openai-curated`: figma, zoom); each is a warning.
  Load them by file name, or skip them quietly. (seen)
- [ ] Agents in subfolders: Claude Code names them `<plugin>:<dir>:<name>`. Not checked. (read)

### Manifests and marketplaces

- [ ] `strict` is not Claude Code's: with a `plugin.json`, only the entry's `skills` are added. Claude Code also
  appends its `commands`, `agents` and `outputStyles`, replaces hooks per event, and refuses `strict: false` with
  components in both places. (read)
- [ ] Plugin sources not supported: `archive` and `command` (`xcode` in the official marketplace). Marketplace
  sources not supported: `url` (a `marketplace.json` by itself), `settings` (inline) and `npm`; `sparsePaths` fails
  the settings' strict parse, and `marketplace add https://…/marketplace.json` is taken for a git URL. (seen for
  `command`, read for the rest)
- [ ] Two plugins with one manifest `name` (`linear` and `github` are in both marketplaces) share a namespace:
  the second one's skills and agents are dropped and its MCP server replaces the first's, silently; two
  `--plugin-dir` plugins of one name share a data directory. Warn, or name by `<plugin>@<marketplace>`. (read)
- [ ] `userConfig`: `type` is not checked, `multiple`, `options`, `min` and `max` are not handled, and a number
  answered `nan` is stored as `null`. Unknown keys in a manifest are ignored silently, where Claude Code refuses
  them inside `userConfig` and `lspServers`. (read)
- [ ] A root `plugin.json` whose `$schema` is not the Agent Plugins one is skipped without a warning. (read)
- [ ] `--plugin-dir` with a folder of plugins or a `.zip`, `--plugin-url`, and a `plugin validate` command do
  not exist. (read)
- [ ] Not used, and reported as such: `dependencies` (not installed), output styles, monitors, channels, themes,
  workflows, a plugin's `settings.json`, Codex's `apps` (36 of 62 curated plugins; ChatGPT connectors, which the
  harness cannot offer). (seen)

### Fetching

- [ ] `git` and `npm` run without a timeout, and a missing checkout is fetched while starting
  (`block_in_place`): with no network, startup hangs. (read)
- [ ] No lock between processes: two fetching one version (the second `rename` fails), `prune` deleting a version
  another process fetched but has not marked yet, `marketplace add` again deleting versions in use, and
  `uninstall` / `marketplace remove` ignoring `.in-use`. (read)
- [ ] The settings are written in place, not through a temporary file; settings and secrets are two writes.
  A `secrets.json` that cannot be read counts as empty, silently, and a failing `stty` leaves a secret echoed. (read)
- [ ] npm: `registry` is not checked, a package may be a path, URL or git address (Claude Code refuses those),
  and the tarball's members are not checked before `tar` unpacks them. git: the user's config applies, `file://`
  and local paths are taken as a plugin's `url`, submodules are not fetched. (read)

### Tests

- [ ] `cli::menu::tests::files_list_the_directory_then_search_the_project` expects a `target/` directory in the
  checkout, so it fails with `CARGO_TARGET_DIR` set elsewhere. (seen)

### Not tested yet

- [ ] A session with a third-party plugin's own hook scripts (`superpowers`' SessionStart,
  `crowdstrike-falcon-foundry`'s router): install one and start a conversation.
- [ ] Plugins in the web UI: the plugin list, and a permission question a skill's command raises (`WebAsker`).
- [ ] 12 of the 262 plugins with `url` / `git-subdir` sources were installed; npm sources only in the unit test.
- [ ] `/reload-plugins`, `plugin update`, `autoUpdate` and `plugin configure` with a real plugin.

## Design review of 2026-11-16

The whole design compared with mainstream harnesses (Claude Code, Codex CLI, Gemini CLI, OpenCode, Aider, Cline,
Cursor). Four read-only passes over the code (loop and LLM layer; tools, sandbox, permissions; extensions and
subagents; frontends), then a comparison drawn from what those harnesses are remembered to do, not checked against
their docs: each entry gives that as **mainstream: sure / likely / unsure**. Check the mainstream claim against the
other harness's docs or source before acting on an entry. All findings here are (read) unless marked.

### Where the harness differs from what mainstream harnesses agree on

- [ ] Nothing contains the model by default: the mode is `bypassPermissions` and the sandbox is off. Claude Code
  asks for edits and bash unless `--dangerously-skip-permissions`; Codex defaults to a workspace-write sandbox
  with no network; Gemini CLI and Cline ask per call. (mainstream: likely)
- [ ] No trust step for a project: the settings refuse project config, yet the project's skills, agents and
  `AGENTS.md` load without a question and take precedence over `~` and the built-ins (a project `explore.md`
  replaces the built-in; a project skill's `` !`cmd` `` runs, unsandboxed, when the model loads it; a project
  agent can declare `permission: write`). Claude Code, Codex, Gemini CLI and Cursor read project config behind a
  per-directory trust prompt. (mainstream: likely)
- [ ] Bash allow rules are still narrower than Claude Code's (checked against its permissions docs): wrappers
  (`timeout`, `nice`, `nohup`, …) are not stripped, so `Bash(npm test:*)` does not allow `timeout 30 npm test`;
  there is no built-in set of read-only commands (`ls`, `cat`, read-only `git`) that run without asking; heredocs
  and comments are never allowed by a pattern; and redirection targets are not checked, so `Bash(cargo test:*)` allows
  `cargo test > any/file`. Deny prefixes being easy to get around (`$(…)`, `bash -c`, `/bin/rm`) is a limit
  Claude Code documents too. (read)
- [ ] Sessions are rewritten whole (after each round of tool calls and each turn), compaction overwrites the
  history before it, and subagent transcripts are not kept (`agent_transcript_path` is null). Claude Code and
  Codex append per message; Claude Code keeps the pre-compaction history behind a boundary record and saves
  subagent transcripts. (mainstream: sure)
- [ ] (Put off for now.) `AGENTS.md` is read from the working directory only; `CLAUDE.md` never. Claude Code walks up from the cwd;
  Codex concatenates from the git root down to the cwd; Gemini CLI and OpenCode walk up too, OpenCode falling back
  to `CLAUDE.md`. The lazy subdirectory notes already match. (mainstream: sure)
- [ ] No two-way programmatic interface: headless is one prompt in, events out (`--output-format stream-json`),
  any call that would ask is refused, no subagent progress; no library target. Claude Code has stream-json input
  and output with a control protocol for approvals and an SDK; Codex `exec --json`, app-server and an SDK;
  OpenCode a client/server split; ACP for editors. They put the core behind one event protocol, where the harness
  has four channels (`Observer`, `Asker`, `Reporter`, `Inbox`). (mainstream: likely)
- [ ] Missing tools: web fetch and web search, which nearly every mainstream harness has. (mainstream: sure)
- [ ] One level of context management (a full summary). Claude Code (microcompact), OpenCode (prune) and the
  Anthropic API (context editing) clear old tool results before summarizing. (mainstream: likely)
- [ ] Worktrees are removed when let go of only if unchanged; one a stopped subagent changed is kept without the
  parent being told where. A process killed outright leaves its worktrees, and nothing sweeps them later.
  `settle` runs git while holding the child's std mutex (runtime.rs). (read)
- [ ] Engineering: Unix only (no `cfg(windows)`), no logging/tracing, no CI, no integration tests (web: 6 unit
  tests, no router/auth test). Codex and Claude Code run on Windows and log or export OTel. (mainstream: likely)
- [ ] Smaller: no OpenAI `prompt_cache_key` (Codex sends one); retry backoff without jitter. (mainstream: likely)

### To check: what mainstream harnesses do here is not known

- [ ] An interrupted response's text, streamed in part, is not kept in the history (the assistant message is
  pushed only once the response is whole).
- [ ] Allow rules, session rules and a hook's `allow` are checked before the mode, so they override plan mode.
- [ ] A PreToolUse hook's `allow` skips the ask rules (Claude Code is thought to still apply them).
- [ ] Manual and idle compaction remove every `/rewind` point (`history.rebase(None, 1)`, compact.rs:231).
- [ ] Calls inside a `code` script never enter history or reach the Observer (no diff shown). Keeping them out
  of context is the point of programmatic tool calling; whether others show them to the user is not known.
- [ ] Secrets in the user's shell rc files reach `bash` (only the harness's own credentials are hidden).

### Found on the way (not a comparison)

- [ ] Repairs after an error or an interrupt look at `tool_use` only, not `server_tool_use`.
- [ ] `lsp` starts its servers at, and shows paths relative to, the directory the harness started in, so a worktree
  subagent's `lsp` looks at the main checkout (lsp.rs:212, 513; the README says so). (read)
- [ ] `FileTracker` keys are not normalized (`..`, symlinks), staleness is mtime only, and a partial read counts
  as a full one for `write`. `edit`/`write` are not atomic (no temp file and rename).
- [ ] Nothing in the system prompt says when to consult the advisor; `tool_search` gets no guidance line.
- [ ] The date in the system prompt goes stale in long-lived web tasks; no git branch or status in it.
- [ ] Approvals of MCP and other tools show only `call_label`'s one line of the input (the bash command and a
  `code` script are shown whole). (read)
- [ ] Not verified: whether the Seatbelt profile (`allow default`) lets commands reach launchd services
  (`open`, `osascript`).

### Decided against

- No cap on the number of a turn's rounds, unlike Claude Code's `--max-turns` or OpenCode's `steps`: on purpose,
  a turn runs until the model stops.
- No cap on a Stop hook that keeps sending the model back: on purpose too (`stop_hook_active` tells the hook).
- No `todo_write` for the model, unlike Claude Code's TodoWrite or Codex's `update_plan`: current models plan well
  enough without it (adb4273); the tool, its rendering and its compaction handling stay, one line from being on.
- The built-in `explore` has `grep` and `glob` only, no `read` (Claude Code's Explore has Read and read-only
  Bash): its prompt is built around reading code through `grep`'s `content` mode with context lines.
- The web UI's tool page (`POST /api/tools/{name}`) runs a tool on input the user wrote, without hooks,
  permissions or a checkpoint, as `!command` does in the REPL: the user's own call, behind the token.

### Already in line with mainstream harnesses (for reference)

The system prompt fixed per conversation; compaction keeping the user's words, a summary and recent files
(Codex, Claude Code); OpenAI `store: false` with encrypted reasoning (Codex); deferring MCP tools past 10% of the
window (Claude Code); subagents that cannot spawn subagents and return only their final text; input typed during
a turn delivered at the next tool boundary; read-before-write by mtime; a fresh process per command with a shell
environment snapshot; an inline viewport rather than the alternate screen.

Where mainstream harnesses disagree, the harness sides with one of them: exact-string `edit` (Claude Code; Codex
uses patches, Aider/Cline search/replace, OpenCode fuzzy matching); `/rewind` tracking only `edit`/`write`
(Claude Code; Cline, Gemini CLI and OpenCode snapshot with a shadow git); MCP tools only, no resources, prompts,
`list_changed` or elicitation (like Codex; Claude Code has them — OAuth is under Plugins → MCP servers); Seatbelt
and bwrap with file rules like Claude Code's and an on/off network like Codex's, without Claude Code's seccomp on
Linux; hardcoded model tables (Codex; OpenCode and Aider use external metadata).
