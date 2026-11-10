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

- [ ] `cargo test` leaves git worktrees and `harness/<date>-…` branches registered in this repository (39 branches
  on 2026-11-10, most of their directories gone): the worktree tests run against the checkout itself. Run them in
  a temporary repository, or remove what they create. (seen)
- [ ] `cli::menu::tests::files_list_the_directory_then_search_the_project` expects a `target/` directory in the
  checkout, so it fails with `CARGO_TARGET_DIR` set elsewhere. (seen)

### Not tested yet

- [ ] A session with a third-party plugin's own hook scripts (`superpowers`' SessionStart,
  `crowdstrike-falcon-foundry`'s router): install one and start a conversation.
- [ ] Plugins in the web UI: the plugin list, and a permission question a skill's command raises (`WebAsker`).
- [ ] 12 of the 262 plugins with `url` / `git-subdir` sources were installed; npm sources only in the unit test.
- [ ] `/reload-plugins`, `plugin update`, `autoUpdate` and `plugin configure` with a real plugin.
