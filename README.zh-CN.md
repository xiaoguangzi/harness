# harness

[English](README.md) | 简体中文

一个用 Rust 编写的极简编码代理（coding-agent）框架。它通过原生 HTTP（带流式传输）与任意组合的 LLM 网关通信，支持 Anthropic Messages 协议或 OpenAI Responses 协议。

## 环境要求

- Rust 1.88+
- `PATH` 上需要有 [ugrep](https://github.com/Genivia/ugrep)，供 `grep` 工具使用（`apt install ugrep` / `brew install ugrep`）

## 用法

```sh
export ANTHROPIC_API_KEY=sk-ant-...     # 或 OPENAI_API_KEY，或按下文配置网关
cargo run --release                      # 交互式 REPL（/exit 退出）
cargo run --release -- "list the files"  # 一次性模式
cargo run --release -- --web             # Web UI，地址 http://localhost:8787（见下文）
```

| 环境变量 | 默认值 |
|---|---|
| `HARNESS_MODEL` | `providers.json` 中的 `default_model`，否则 `anthropic/claude-opus-5` |
| `HARNESS_SUBAGENT_MODEL` | `providers.json` 中的 `subagent_model`，否则与主模型相同 |
| `HARNESS_MAX_TOKENS` | `64000`（可被 provider 或单个模型覆盖） |
| `HARNESS_PROVIDERS` | 只读取这个 providers 文件 |
| `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN`、`ANTHROPIC_BASE_URL` | 隐式的 `anthropic` provider |
| `OPENAI_API_KEY`、`OPENAI_BASE_URL` | 隐式的 `openai` provider（Responses API） |

| `HARNESS_WEB_TOKEN` | Web UI token（`--web` 绑定非回环地址时必填，未设置则自动生成） |

## Web UI

`harness --web`（或 `--web=ADDR`，默认 `127.0.0.1:8787`）启动浏览器 UI 而非 REPL：

- **任务 (Tasks)**：把任务派发给主代理，或直接派发给某个子代理类型（`explore` 或自定义代理），也可指定模型。任务在后台运行，可以多个并发；每个任务都是独立的会话，流式展示文本、工具调用和结果（子代理进度显示在其 `agent` 调用之下）。可以用后续消息继续任务、切换模型、取消任务（运行中的 `bash` 命令会被终止）或删除任务。`@路径` 提及和 `/<skill> args` 与 REPL 中一致。
- **工具 (Tools)**：列出所有已注册的工具（内置、技能、MCP、`agent`）及其描述和 schema；填入 JSON 输入即可直接调用。
- **模型配置 (Models)**：当前生效的 provider 和模型，以及 `.harness/providers.json`（或 `$HARNESS_PROVIDERS`）的表单或原始 JSON 编辑器。保存时会先对照其他配置文件和环境进行校验，然后无需重启即生效；空闲任务会切换到新配置。
- **Agents / Skills / MCP**：发现了哪些内容，以及哪些 MCP 服务器已连接。

即使没有可用的 provider，服务器也会启动，因此可以在浏览器里配置第一个模型。该 API 能执行命令，所以做了防护：绑定回环地址时，请求必须使用回环主机名（防 DNS rebinding），且改变状态的请求需要 `X-Harness` 请求头（防跨站请求）；绑定其他任何地址时还需要 token，启动时会打印带有 token 的 URL（`http://HOST:PORT/#token=...`）。

| 端点 | 说明 |
|---|---|
| `GET /api/info` | 生效的 provider、可编辑的配置文件、agents、skills、MCP 状态 |
| `POST /api/config` `{text}` | 校验、保存并应用 providers 文件 |
| `GET /api/tools`、`POST /api/tools/{name}` `{input}` | 列出 / 调用工具 |
| `GET/POST /api/tasks` `{prompt, agent?, model?, title?}` | 列出 / 创建任务 |
| `POST /api/tasks/{id}/messages` `{prompt}`、`/model` `{model}`、`/cancel`；`DELETE /api/tasks/{id}` | 驱动任务 |
| `GET /api/tasks/{id}/events` | server-sent events：先回放该任务的日志，再实时推送（可通过 `Last-Event-ID` 恢复） |

## LLM 网关

在 `~/.harness/providers.json` 和/或 `.harness/providers.json` 中声明网关（按 provider 名，项目文件优先）。每个网关只说一种协议；模型以 `<provider>/<model>` 寻址。

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

| 字段 | 说明 |
|---|---|
| `protocol` | `anthropic`（`POST {base_url}/v1/messages`）或 `openai-responses`（`POST {base_url}/v1/responses`）；`base_url` 中已有的 `/v1` 不会重复拼接 |
| `api_key` | 以 `x-api-key`（Anthropic）或 `Authorization: Bearer`（OpenAI）发送；可用 `"auth": "bearer"` / `"x-api-key"` 覆盖 |
| `headers` | 额外的请求头 |
| `max_tokens` | 输出上限（也可按模型设置） |
| `extra_body` | 合并进每个请求体，也可按模型设置；`null` 会移除默认键，例如对不支持自适应思考的模型用 `{"thinking": null}`，或对拒绝加密推理的网关用 `{"include": null}` |
| `fallbacks`、`eager_input_streaming` | 仅 Anthropic 的 API 特性；`api.anthropic.com` 默认开启，网关默认关闭 |
| `prompt_cache` | Anthropic 自动提示词缓存；默认开启 |
| `models` | 按模型的覆盖项；列出的模型会显示在 `/model` 和 Tab 补全中 |

`base_url`、`api_key` 和 `headers` 中的 `${VAR}` / `${VAR:-default}` 会从环境变量展开。没有已知 provider 前缀的 spec 视为当前 provider 上的一个模型，因此包含 `/` 的网关模型名（如 `openai/gpt-5`）同样可用。

会话中途可用 `/model <provider>/<model>` 切换模型；单独输入 `/model` 会列出 provider 和模型。历史始终以 Anthropic 形式保存，请求 OpenAI 时再转换（`function_call` / `function_call_output`、`input_image`、`input_file`）；请求是无状态的（`store: false`），推理项会以其加密内容重放。绑定模型的块（Anthropic thinking、OpenAI reasoning）只会发回产生它们的那个模型。子代理可以通过 `subagent_model` 或代理自身的 `model` 使用不同的网关。

注意：项目级 `.harness/providers.json`（同 `.mcp.json`）会被原样信任，所以请只在可信的仓库中运行 harness。

## 用 `@` 附加文件

用 `@` 提及一个路径，其内容会附加到你的消息中，模型无需工具调用即可看到：

| 提及方式 | 附加内容 |
|---|---|
| `@src/main.rs` | 该文件，带行号（前 2000 行） |
| `@src/main.rs#L10-20` / `#L10` | 仅那些行 |
| `@src/` | 目录列表 |
| `@screenshot.png`（png/jpg/gif/webp） | 图片 |
| `@spec.pdf` | PDF 作为文档 |
| `@"name with spaces.txt"` | 带引号的路径 |

提及可以出现在句中且无需空格（`看看@src/main.rs的代码`）；尾部标点会被忽略。只有真实存在的路径才会被附加，紧跟在电子邮件风格字符之后的 `@` 会被原样保留，因此 `me@example.com` 仍是普通文本。在 REPL 中，输入 `@` 会在提示符下方打开文件菜单：先列出当前目录，然后随输入搜索项目文件（git 仓库中是 `git ls-files` 的输出，仓库外则遍历目录），最佳匹配优先（路径前缀、文件名、子串，然后模糊匹配）。↑/↓ 选择，Tab 或 Enter 填入路径，目录会保持打开以列出其中条目。被 git 忽略的条目默认隐藏，除非你输入到该目录里（`@target/`）。附加的文本文件会计入已读，满足 `write` 工具的要求。REPL 还会在 `~/.harness/history` 中保存行编辑历史。

## REPL

助手的 Markdown 在流式输出时即时渲染（逐行；表格在完整后对齐）。工具调用显示为 `● bash(cargo test)` 并带有结果的前几行，子代理调用列在其 `agent` 调用之下，并有旋转指示器显示正在运行的内容。每轮结束时显示耗时、输出 token 数和上下文大小。当 stdout 不是终端（或设置了 `NO_COLOR`）时，文本按原样输出，没有样式和摘要；一次性模式下错误输出到 stderr，退出码非零。

| 命令 / 按键 | 说明 |
|---|---|
| `/help` | 列出命令 |
| `/model [spec]` | 显示 provider 和模型，或切换到 `<provider>/<model>` |
| `/clear` | 开始新会话 |
| `/tools` | 列出模型可调用的工具 |
| `/skills` | 列出发现的技能 |
| `/agents` | 列出子代理类型及其权限和工具 |
| `/mcp` | 列出 MCP 服务器、其工具和日志文件 |
| `/<skill> [args]` | 直接运行一个技能（一次性模式同样可用：`harness /greet Bob`） |
| `/exit`、`exit`、Ctrl-D | 退出（`exit` 或 `quit` 仅在单独占一行时生效） |
| Ctrl-C | 中断运行中的轮次：运行中的命令被终止，并告知模型请求已被中断。在提示符处连按两次退出 |
| 行尾 `\`、Alt+Enter | 在新行继续这条消息 |
| `/` | 在提示符下方打开命令菜单；随输入收窄，↑/↓ 选择，Tab 填入，Enter 运行。`/model ` 之后会列出模型 |
| `@` | 打开文件菜单（见[用 `@` 附加文件](#用--附加文件)） |

## MCP 服务器

在 `.mcp.json`（项目）和/或 `~/.harness/mcp.json`（用户）中声明服务器，结构与 Claude Code 相同；名称冲突时项目文件优先。`HARNESS_MCP_CONFIG=path` 则只使用这一个文件。

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

- 传输方式：`stdio`（设置了 `command` 时的默认值）和可流式的 `http`（设置了 `url` 时的默认值），通过官方 Rust SDK [`rmcp`](https://github.com/modelcontextprotocol/rust-sdk) 实现。
- 每个字符串中的 `${VAR}` / `${VAR:-default}` 都会从环境变量展开。
- 服务器在启动时并发连接；连接失败的服务器会被报告并跳过。
- 工具以 `mcp__<server>__<tool>` 形式暴露。文本、内嵌文本资源和 PNG/JPEG/GIF/WebP 图片会传回给模型。
- stdio 服务器的 stderr 写入 `$TMPDIR/harness-mcp-<server>.log`。

## 技能（Skills）

技能是一个包含 `SKILL.md` 的目录（[Agent Skills](https://agentskills.io) 格式）：

```markdown
---
name: release-notes
description: Draft release notes from the git log. Use when asked for a changelog.
---
1. Run `git log --oneline <last-tag>..HEAD` ...
```

技能在 `.harness/skills/`、`.claude/skills/`、`~/.harness/skills/` 和 `~/.claude/skills/` 中发现（名称冲突时靠前的优先）。预先提供给模型的只有每个技能的名称和描述，列在 `skill` 工具里；正文在模型调用该工具或你输入 `/<name>` 时才加载。技能自带的脚本和参考文件通过普通工具使用，路径相对于技能的基准目录（加载的技能会说明该目录）。

## 子代理（Subagents）

主代理可以用 `agent` 工具委派一个自包含的任务。子代理在自己的全新会话中运行，有自己的系统提示和工具集，只有最终报告会返回，从而把漫长的搜索挡在主上下文之外。同一次响应中启动的多个子代理并行运行。它们的工具调用在终端中缩进显示。

- **`explore`**（内置）：一个只以 `grep` 为工具的代码库搜索代理。用于"X 在哪里 / 哪些文件用了 Y"这类问题。
- **自定义代理**：`.harness/agents/` 或 `.claude/agents/`（项目或 `~`）中的 Markdown 文件（先找到的优先，名为 `explore` 的文件会覆盖内置项）：

  ```markdown
  ---
  name: reviewer
  description: Reviews the current diff for bugs. Use after making changes.
  tools: read, grep          # 可选；`mcp__server__*` 按前缀匹配
  permission: read-only      # 默认值；`write` 允许 bash/write 和非只读的 MCP 工具
  model: claude-sonnet-5     # 可选
  ---
  You are a code reviewer...
  ```

- **权限默认只读**：只读代理只会得到标记为只读的工具（`read`、`grep`、`skill`，以及服务器设置了 `readOnlyHint` 的 MCP 工具），即使其 `tools` 列表列出了其他工具。`tools` 会进一步收窄。
- 子代理不能再启动子代理。它们的文件读取使用自己的追踪器，因此不会计入主代理 `write` 所需的已读记录。
- 模型：代理的 `model`，否则 `subagent_model` / `HARNESS_SUBAGENT_MODEL`，否则主模型；任意 `<provider>/<model>`。
- `/agents` 列出可用类型。

## 目录结构

- `src/main.rs` — 启动：配置、工具、技能、MCP 服务器、代理；然后进入 CLI 或 web 前端
- `src/cli/` — 终端前端：REPL 和一次性模式（`mod.rs`）、旋转指示器与轮次显示（`output.rs`）、流式 Markdown 渲染（`markdown.rs`）、显示宽度辅助（`text.rs`）、提示符下方的菜单：命令、项目文件搜索（`menu.rs`）、带菜单按键、补全与多行输入的 rustyline 辅助（`repl.rs`）
- `src/web/` — web 前端（`--web`）：axum 服务器与 JSON/SSE API（`mod.rs`）、后台任务及其事件日志（`tasks.rs`）、单页 UI（`index.html`，内嵌于二进制）
- `src/agent.rs` — 代理循环：请求 → 并发运行工具 → 发送结果 → 循环直到 `end_turn`
- `src/llm/` — LLM 网关
  - `config.rs` — `providers.json`、模型 spec 解析、按 provider/模型的设置
  - `anthropic.rs` — Messages API 后端（SSE 转内容块、thinking、提示词缓存、拒答回退）
  - `openai.rs` — Responses API 后端（历史 ⇄ 输入/输出项转换、加密推理）
  - `http.rs` — 带重试的 POST、SSE 读取器
- `src/mentions.rs` — `@路径` 的解析与展开为附件块
- `src/subagent.rs` — 子代理定义（内置 `explore`、自定义文件）、权限、`agent` 工具
- `src/mcp.rs` — MCP 客户端：配置加载、服务器连接、`mcp__*` 工具适配器
- `src/skills.rs` — 技能发现、`SKILL.md` frontmatter 解析、`skill` 工具
- `src/tools/` — `Tool` trait、注册表和 `FileTracker`（共享的读/写状态）
  - `bash.rs` — 通过 `bash -c` 运行命令（超时、输出截断）
  - `read.rs` — 读取文本文件并带行号（`offset` / `limit` 分页）
  - `write.rs` — 创建或覆盖文件；已存在的文件必须先被读取且此后未被修改
  - `grep.rs` — 通过 ugrep 的正则搜索（PCRE，遵循 `.gitignore`，`files_with_matches` / `content` / `count` 模式）

在官方 Anthropic API 上，工具以 `eager_input_streaming` 声明，因此较大的输入（例如 `write` 的文件内容）会在生成时流式传输。此后 API 不再校验工具输入，所以每个内置工具都把输入解析成严格的结构体（拒绝未知字段），流式 JSON 解析失败的调用不会执行：模型会得到 `INVALID_JSON` 错误结果。MCP 工具保持输入缓冲开启，因此 API 会按服务器提供的 schema 校验其输入。

## 添加工具

实现 `tools::Tool`（name、description、JSON schema、异步 `run`），并在 `ToolRegistry::with_defaults` 中注册。
