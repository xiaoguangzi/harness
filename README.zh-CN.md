# harness

[English](README.md) | 简体中文

一个用 Rust 编写的极简编码代理（coding-agent）框架。它通过原生 HTTP（带流式传输）与任意组合的 LLM 网关通信，支持 Anthropic Messages 协议或 OpenAI Responses 协议。

## 环境要求

- Rust 1.88+
- `PATH` 上需要有 [ugrep](https://github.com/Genivia/ugrep)，供 `grep` 和 `glob` 工具使用（`apt install ugrep` / `brew install ugrep`）
- Linux 上[粘贴图片](#粘贴图片)需要 `wl-paste`（[wl-clipboard](https://github.com/bugaevc/wl-clipboard)，Wayland）或 `xclip`（X11）；macOS 无需额外安装

## 用法

安装一次，之后即可在任意项目目录运行 `harness`：

```sh
cargo install --path .                  # 装到 ~/.cargo/bin；或加 --root ~/.local 装到 ~/.local/bin
```

```sh
export ANTHROPIC_API_KEY=sk-ant-...     # 或 OPENAI_API_KEY，或按下文配置网关
harness                                 # 交互式 REPL（/exit 退出）
harness "list the files"                # 一次性模式
harness --continue                      # 继续当前目录最近的会话（见下文）
harness --web                           # Web UI，地址 http://localhost:8787（见下文）
harness --sandbox                       # 在操作系统沙箱中运行模型的命令（见下文）
harness --permission-mode default       # 运行会改动东西的工具调用前先询问（见下文）
harness --plugin-dir ./my-plugin        # 本次运行额外加载某个目录中的插件（见下文）
harness provider add relay              # 添加 LLM 网关，会询问 URL、密钥和模型（见下文）
harness plugin install fmt@team         # 安装插件（见下文；harness plugin help）
harness --check-plugins                 # 报告每个已启用插件加载了什么、没加载什么
harness --output-format json "..."      # 一次性模式，输出供脚本使用的 JSON 结果（见下文）
harness --bare "..."                    # 不加载插件、hooks、MCP 服务器、技能和 AGENTS.md 启动
harness --help                          # 查看全部选项
```

拉取更新后重新执行 `cargo install --path .`。开发时用 `cargo run --release -- <参数>` 效果相同。

| 环境变量 | 默认值 |
|---|---|
| `HARNESS_SETTINGS` | 读取这个[配置](#配置)文件，代替 `~/.harness/settings.json`（`--settings FILE` 优先） |
| `HARNESS_MODEL` | 配置中的 `model`，否则 `anthropic/claude-opus-5` |
| `HARNESS_SUBAGENT_MODEL` | 配置中的 `subagent_model`，否则与主模型相同 |
| `HARNESS_MAX_TOKENS` | 配置中的 `max_tokens`，否则 `64000`（可被 provider 或单个模型覆盖） |
| `HARNESS_EFFORT` | 配置中的 `effort`，否则 `high` |
| `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN`、`ANTHROPIC_BASE_URL` | 隐式的 `anthropic` provider |
| `OPENAI_API_KEY`、`OPENAI_BASE_URL` | 隐式的 `openai` provider（Responses API） |
| `HARNESS_SANDBOX` | `1` 或 `0`：开启或关闭[沙箱](#沙箱)（`--sandbox` / `--no-sandbox` 参数优先） |
| `HARNESS_WEB_TOKEN` | Web UI token（未设置则启动时自动生成） |

## 脚本调用

`--output-format json` 让一次性运行在结束时向 stdout 输出一个 JSON 对象，此外什么都不输出：`result`（模型最后的回答）、`is_error` 和 `error`、`session_id`（可用于 `--resume`）、`model`、`requests`、`duration_ms` 和 `usage`（输入、缓存读写、输出 token）。`--output-format stream-json` 在运行过程中每行输出一个 JSON 对象：`init`（会话、模型、工作目录、工具名）、`text`（回答中每个完成的文本块）、`tool_call` 和 `tool_result`（按 `id` 配对）、每次请求的 `usage`、`retry`、`compact`、`hook`，最后是同样的 `result`。退出码为 0，出错为 1，被中断为 130。和文本模式一样，运行会等它启动的后台工作完成。

`--bare` 以精简、可预期的方式启动：不加载配置里的插件、hooks 和 MCP 服务器，不从目录读取技能、命令和自定义代理，也不读 `AGENTS.md`；provider、沙箱、内置工具和内置代理照常可用，`--plugin-dir` 仍会加载它指定的插件。

```sh
harness --bare --output-format json "列出 src 里的 TODO 注释" | jq -r .result
```

## 配置

所有配置都在一个文件里：`~/.harness/settings.json`（或 `--settings FILE` / `$HARNESS_SETTINGS`）：

```json
{
  "model": "relay-a/claude-opus-5",
  "effort": "high",
  "providers": { "relay-a": { "protocol": "anthropic", "base_url": "https://relay-a.example.com", "api_key": "${RELAY_A_KEY}" } },
  "mcpServers": { "exa": { "url": "https://mcp.exa.ai/mcp" } },
  "sandbox": { "enabled": true }
}
```

| 键 | 说明 |
|---|---|
| `model`、`subagent_model`、`effort`、`max_tokens`、`context_window`、`providers` | 模型和 [LLM 网关](#llm-网关) |
| `idle_compaction`、`idle_compaction_min_tokens` | [空闲压缩](#上下文压缩) |
| `mcpServers` | [MCP 服务器](#mcp-服务器) |
| `sandbox` | [沙箱](#沙箱) |
| `permissions` | 哪些工具调用先询问或被拒绝：[权限](#权限) |
| `shell` | [模型所执行命令的环境](#命令的环境) |
| `hooks` | [Hooks](#hooks) |
| `statusLine` | REPL 的[状态栏](#状态栏) |
| `enabledPlugins`、`extraKnownMarketplaces`、`pluginConfigs` | [插件](#插件plugins) |

所有键都可省略，未知的键会报错。项目目录中的配置一概不读（包括项目的 `.mcp.json`）：否则仓库可以把带着你 API key 的请求发到它自己的服务器、启动 MCP 命令或 hook，或放宽沙箱。项目中的 `.harness/agents`、`.harness/skills`、`.claude/` 下的同名目录及 `.agents/skills` 仍会读取，因为它们只增加提示词（技能中的 `` !`command` `` 行与模型自己的 `bash` 命令一样运行，开启沙箱时在沙箱内）。插件的 hook 和 MCP 服务器会在沙箱外运行命令，因此只能在这里启用。旧版本读取 `~/.harness/providers.json`、`mcp.json` 和 `sandbox.json`；这些文件存在时 harness 会给出警告，其内容应移到上面对应的键下（`default_model` 改名为 `model`）。

## Web UI

`harness --web`（或 `--web=ADDR`，默认 `127.0.0.1:8787`）启动浏览器 UI 而非 REPL：

- **任务 (Tasks)**：把任务派发给主代理，或直接派发给某个子代理类型（`explore`、`general-purpose` 或自定义代理），也可指定模型。任务在后台运行，可以多个并发；每个任务都是独立的会话，流式展示文本、工具调用和结果。每个子代理有一张卡片，显示它的工具调用、状态、报告和停止按钮；任务空闲时，后台子代理的报告会自动开始新的一轮。可以用后续消息继续任务、切换模型、取消任务（运行中的 `bash` 命令和前台子代理会停止，后台子代理继续运行）或删除任务（其子代理也会停止）。`@路径` 提及、`/<skill> args`、`/init` 和 `/compact` 与 REPL 中一致。
- 助手文本按 Markdown 流式渲染（代码块、列表、表格、链接）；工具调用的摘要行直接显示它的对象（`bash` 命令、文件路径、`grep` 模式）。切换模型和删除任务放在任务头部的 `⋯` 菜单里。
- **工具 (Tools)**：列出所有已注册的工具（内置、技能、MCP、`agent`）及其描述和 schema；填入 JSON 输入即可直接调用。
- **模型配置 (Models)**：当前生效的 provider 和模型，以及配置文件的表单或原始 JSON 编辑器。保存时会先结合环境变量校验，模型配置随即无需重启生效，空闲任务会切换过去（`mcpServers`、`sandbox` 和 `hooks` 的修改在下次启动时生效）。
- **Agents / Skills / MCP**：发现了哪些内容，以及哪些 MCP 服务器已连接。

即使没有可用的 provider，服务器也会启动，因此可以在浏览器里配置第一个模型。该 API 能执行命令，所以做了防护：每个 API 请求都需要 token，绑定回环地址时也不例外（本机其他用户同样能访问回环端口），启动时会打印带有 token 的 URL（`http://HOST:PORT/#token=...`，浏览器会记住它，之后新开的标签页也能用）；绑定回环地址时，请求还必须使用回环主机名（防 DNS rebinding）；改变状态的请求需要 `X-Harness` 请求头（防跨站请求）。UI 保存的配置文件只有其所有者可读。

| 端点 | 说明 |
|---|---|
| `GET /api/info` | 生效的 provider、配置文件、agents、skills、MCP 状态 |
| `POST /api/config` `{text}` | 校验、保存并应用配置文件 |
| `GET /api/tools`、`POST /api/tools/{name}` `{input}` | 列出 / 调用工具 |
| `GET/POST /api/tasks` `{prompt, agent?, model?, title?}` | 列出 / 创建任务 |
| `POST /api/tasks/{id}/messages` `{prompt}`、`/model` `{model}`、`/cancel`；`DELETE /api/tasks/{id}` | 驱动任务 |
| `GET /api/tasks/{id}/subagents`、`POST /api/tasks/{id}/subagents/{agent_id}/stop` | 列出 / 停止该任务的子代理 |
| `POST /api/tasks/{id}/permissions/{question}` `{answer, message?}`、`/mode` `{mode}` | 回答 `permission` 事件（`allow`、`always`、`deny`），或设置该任务的[权限模式](#权限) |
| `GET /api/tasks/{id}/events` | server-sent events：先回放该任务的日志，再实时推送（可通过 `Last-Event-ID` 恢复）；`reset` 事件表示之前的内容不再显示（回退之后） |
| `GET /api/tasks/{id}/checkpoints`、`POST /api/tasks/{id}/rewind` `{message, code, conversation}` | 任务可以回到的提示，以及执行回退 |
| `GET /api/sessions`、`POST /api/sessions/{id}/resume` | 这个目录下保存的会话，以及在任务中继续其中一个 |

## LLM 网关

在[配置](#配置)的 `providers` 下声明网关。每个网关只说一种协议；模型以 `<provider>/<model>` 寻址。

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

| 字段 | 说明 |
|---|---|
| `protocol` | `anthropic`（`POST {base_url}/v1/messages`）或 `openai-responses`（`POST {base_url}/v1/responses`）；`base_url` 中已有的 `/v1` 不会重复拼接 |
| `api_key` | 以 `x-api-key`（Anthropic）或 `Authorization: Bearer`（OpenAI）发送；可用 `"auth": "bearer"` / `"x-api-key"` 覆盖 |
| `headers` | 额外的请求头 |
| `max_tokens` | 输出上限：顶层、按 provider 或按模型设置 |
| `context_window` | 模型的上下文窗口（token 数）：顶层、按 provider 或按模型设置；会话在窗口占满前会被[压缩](#上下文压缩)。不设置时，常见模型用它们自己的窗口（Claude Opus 4.6 及以后、Sonnet 4.6 及以后、Haiku 5.5 和 Fable 为 1M；更早的 Claude 模型为 200k；GPT-5 为 400k；网关加了前缀如 `anthropic/claude-…` 也能识别），其他模型为 `200000`；网关允许的更小时请手动设置 |
| `effort` | 顶层或按模型设置：`low`、`medium`、`high`（默认）、`xhigh` 或 `max`。以 Anthropic 的 `output_config.effort` 或 OpenAI 的 `reasoning.effort` 发送；`extra_body` 仍可覆盖它，例如对不支持 effort 的模型用 `{"output_config": null}` |
| `extra_body` | 合并进每个请求体，也可按模型设置；`null` 会移除默认键，例如对不支持自适应思考的模型用 `{"thinking": null}`，或对拒绝加密推理的网关用 `{"include": null}` |
| `fallbacks`、`eager_input_streaming` | 仅 Anthropic 的 API 特性；`api.anthropic.com` 默认开启，网关默认关闭 |
| `tool_result_media` | 仅 Anthropic：工具返回的图片和 PDF（`read`、MCP 工具）放在 `tool_result` 里。`api.anthropic.com` 默认开启；网关默认关闭，此时它们放在同一条消息里、紧跟工具结果之后，因为很多网关会丢掉工具结果里的图片 |
| `prompt_cache` | Anthropic 自动提示词缓存；默认开启 |
| `prompt_cache_ttl` | 仅 Anthropic：缓存在最后一次使用后保留多久，`5m`（默认）或 `1h`。写入 1 小时缓存的价格是输入价的 2 倍而不是 1.25 倍，常在 5 分钟到 1 小时之间停顿时更划算 |
| `gzip` | 用 gzip 压缩 1 KiB 及以上的请求体；`api.anthropic.com` 默认开启，网关默认关闭，因为很多网关不接受 |
| `tool_search` | 仅 Anthropic：MCP 工具定义会占上下文窗口十分之一以上时，把它们[延迟加载](#延迟加载的工具)到 `tool_search` 工具之后。`api.anthropic.com` 默认开启，网关默认关闭；provider 对这样的请求返回 HTTP 400 或 422 时，会把所有工具完整发送再试一次，成功的话本次运行剩下的时间里都这样发送 |
| `models` | 按模型的覆盖项；列出的模型会显示在 `/model` 和 Tab 补全中 |
| `price` | 按模型设置：每百万 token 的美元价格，`{"input": 1.25, "output": 10, "cache_read": 0.125}`（也可设 `cache_write`；默认缓存读取为输入价的十分之一、写入为 1.25 倍，1 小时缓存为 2 倍），用于 `/usage` 显示的费用估算。Claude 模型内置了官方价格 |

临时性失败的请求（连接失败或断开、HTTP 408、409 或 5xx、回复中途断开）最多重发 3 次，分别等 1、2、4 秒；API 过载或限流（HTTP 429、529，或流中出现这类错误）最多重发 10 次，等待时间翻倍直到 1 分钟，总共约 5 分钟。响应带 `retry-after`（或 `retry-after-ms`）头时按它等待；超过 10 分钟的不等。每次重试都会显示一行 `↻` 和原因。

`base_url`、`api_key` 和 `headers` 中的 `${VAR}` / `${VAR:-default}` 会从环境变量展开。没有已知 provider 前缀的 spec 视为当前 provider 上的一个模型，因此包含 `/` 的网关模型名（如 `openai/gpt-5`）同样可用。

`harness provider` 在命令行中修改这些配置（REPL 中的 `/provider …` 作用相同，并让当前会话立即改用新配置；模型所在的 provider 还在时保持原模型）：

```sh
harness provider add relay-b       # 询问协议、base URL、API key 和模型；给出选项则跳过提问：
harness provider add relay-b --protocol openai-responses --base-url https://relay-b.example.com/v1 \
  --api-key '${RELAY_B_KEY}' --model gpt-5 --model gpt-5-mini
harness provider model add relay-b/gpt-5 --context-window 400000 --effort high --default   # 另有 --max-tokens
harness provider default relay-a/claude-opus-5
harness provider list              # 另有：remove <name>、model remove <provider>/<model>
```

对已有的 provider 执行 `add` 只修改选项给出的部分（不带选项时重新逐项询问，直接回车保留原值）。密钥按输入原样保存，所以写 `${VAR}`（在 shell 中用单引号）可以不把它写进文件。没有可用的默认模型时，`add` 把它的第一个模型设为默认模型。每次修改后，如果配置还不能使用（例如没有默认模型），会说明原因。

会话中途可用 `/model <provider>/<model>` 直接切换模型，或输入无参数的 `/model` 并回车打开模型设置面板：Tab 切换主模型和子代理默认模型，↑/↓ 选择模型，输入文字筛选（Enter 采用高亮的模型，仅筛选不会改变设置），←/→ 调整共同的会话 effort（`auto`、`low`、`medium`、`high`、`xhigh`、`max`）。面板显示两个模型选择、协议、配置的上下文/输出上限和 effort；Enter 一次应用所有预览修改，Esc 放弃修改，仅影响本次会话。子代理默认模型可设置为跟随主模型，也可独立选择；代理自身指定的模型仍优先，已有子代理保留原模型。重定向输入时仍打印 provider、模型列表和实际生效的设置。`/effort <level>` 修改本次会话剩余部分的推理强度（`/model` 切换后保留），`/effort auto` 恢复为配置值。历史始终以 Anthropic 形式保存，请求 OpenAI 时再转换（`function_call` / `function_call_output`、`input_image`、`input_file`）；请求是无状态的（`store: false`），推理项会以其加密内容重放。绑定模型的块（Anthropic thinking、OpenAI reasoning）只会发回产生它们的那个模型。子代理可以通过 `subagent_model` 或代理自身的 `model` 使用不同的网关。

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

提及可以出现在句中且无需空格（`看看@src/main.rs的代码`）；尾部标点会被忽略。只有真实存在的路径才会被附加，紧跟在电子邮件风格字符之后的 `@` 会被原样保留，因此 `me@example.com` 仍是普通文本。在 REPL 中，输入 `@` 会在提示符下方打开文件菜单：先列出当前目录，然后随输入搜索项目文件（git 仓库中是 `git ls-files` 的输出，仓库外则遍历目录），最佳匹配优先（路径前缀、文件名、子串，然后模糊匹配）。↑/↓ 选择，Tab 或 Enter 填入路径，目录会保持打开以列出其中条目。被 git 忽略的条目默认隐藏，除非你输入到该目录里（`@target/`）。附加的文本文件会计入已读，满足 `edit` 和 `write` 工具的要求。REPL 还会在 `~/.harness/history` 中保存行编辑历史。

### 粘贴图片

在 REPL 中，Ctrl+V 粘贴剪贴板里的图片，例如截图。macOS 上同样是 Ctrl+V，因为 Cmd+V 归终端所有，而终端只会粘贴文本。图片在提示符中显示为 `[Image #1]`，只要这个占位符还在消息里，图片就会随消息发送，删掉占位符即不发送该图片。编号在整个会话中延续，因此之后的消息可以回头引用 `[Image #1]`。在 Finder 或文件管理器中复制的文件会粘贴为指向其路径的 `@` 提及；剪贴板中没有图片时，提示符下方会给出说明。

macOS 通过 `osascript` 读取剪贴板（应用放入的任意图片格式均可）。长边超过 2576 像素的图片会缩小到该尺寸，反正 API 也会缩小；缩小后仍超过 3.75 MB（base64 后 5 MB，即 API 的上限）的图片以 JPEG 发送。Linux 上由 `wl-paste` 或 `xclip` 原样提供 PNG、JPEG、GIF 或 WebP 图片，上限相同。

## REPL

助手的 Markdown 在流式输出时即时渲染（逐行；表格在完整后对齐）。工具调用显示为 `● bash(cargo test)` 并带有结果的前几行，前台子代理的调用列在其 `agent` 调用之下，后台子代理结束时显示一行，并有旋转指示器显示正在运行的内容。每轮结束时显示耗时、输出 token 数和上下文大小。当 stdout 不是终端时，文本按原样输出，没有样式和摘要；一次性模式下错误输出到 stderr，退出码非零。

交互式终端启动后会占满当前屏幕，对话从顶部开始，shell 和对话输出保留在终端原生滚动历史中。输入框夹在两条横线之间，在等待输入和模型运行期间都固定在底部；命令和文件菜单在输入框下方展开，输入框会向上让出空间。文本换行、光标、菜单和窗口缩放由同一个渲染器处理，设置 `NO_COLOR` 也只关闭颜色，仍保留布局。终端 Reset 或 Clear Buffer 后，下一次编辑、缩放或运行状态刷新会恢复布局。模型工作时输入的内容会进入这个输入框：按 Enter 后，这条消息列在输入框上方，直到模型在下一次请求中拿到它（跟在当时正在运行的工具调用结果之后；如果模型正在写回答，它会继续而不是结束本轮），之后它显示在会话中。`@路径` 提及、Ctrl+V 以及命令和文件菜单在这里同样可用；历史只在提示符处可用。不改动对话的 `/命令` 会立即在本轮旁边执行，与 Claude Code 一样：`/model`、`/effort`、`/permissions`、`/help`、`/skills`、`/tools`、`/mcp`、`/agents`、`/tasks`、`/stop` 和 `/sandbox`（未知命令也会立即提示）。它们的面板显示在输入框的位置，本轮期间结束时面板仍保持打开；此时选定的模型或 effort 从下一条消息开始使用，一轮内的请求始终使用同一个模型。其他命令（`/clear`、`/compact`、`/rewind`、`/usage`、`!命令`、技能等）会等本轮结束再执行。想改主意时，在输入框第一行按 ↑（有排队消息时空输入框会给出提示）：模型尚未拿到的消息会回到输入框中、排在你已输入的内容之前，可以修改后重新发送，与 Claude Code 一样。如果中断本轮，模型尚未拿到的消息会回到输入框中；本轮结束后，输入框里剩下的内容仍会保留在提示符中。

每个已完成的工具调用会与自己的结果相邻显示，同一批并发调用多次使用同名工具也能明确对应。正常对话默认只显示简短预览。按 Ctrl+O 或执行 `/output`，可连续浏览展开的整段会话：用户问题、思考、回复和所有保留的工具结果放在一起，命令下方直接显示输出。按 `i` 或 Tab 显示/隐藏完整 JSON 输入与 Call ID；↑/↓ 或 `j`/`k` 逐行滚动，PgUp/PgDn 或空格/`b` 翻页，Home/End 或 `g`/`G` 跳到开头/末尾，按 `/` 输入关键词并回车搜索，`n`/`N` 查找下一处/上一处。搜索输入中 Esc 取消搜索；其他时候 Esc、`q` 或 Ctrl+O 关闭视图。打开时定位到底部并跟随新输出，向上滚动后暂停跟随，按 `G` 恢复；操作提示固定在屏幕底部。模型运行期间同样可用，不会提交或丢弃输入草稿。`/resume` 恢复已保存的会话内容，`/clear` 清空视图。完整文本仍受工具自身的输出上限约束，例如 `bash` 最多返回 30,000 字节（见[大输出](#大输出)）；图片和文档附件在此文本视图中只显示说明。

交互面板按终端可用宽度展开，使用标题和留白，不绘制边框或分隔线；宽度足够时每项的说明显示在名称旁边，放不下时在列表下方完整显示选中项的说明。无参数的选择类命令会打开键盘交互面板：`/effort`、`/resume`、`/rewind`、`/tasks`、`/stop`、`/provider` 和 `/plugins`。↑/↓ 选择，输入文字筛选，Enter 继续，Esc 取消或返回。`/help` 和 `/skills` 可将选中的命令填入输入框，便于补充参数后执行。`/tools`、`/agents`、`/mcp`、`/usage` 和 `/sandbox` 提供可搜索的只读详情面板；Tab 展开完整详情，↑/↓ 滚动，Tab/Esc 返回。没有可显示的任务时 `/stop` 和 `/tasks` 只给出提示；`/rewind` 仅在代码有改动且可以恢复时提供恢复代码的选项。带参数的命令和重定向输入保留原来的行为；`/clear`、`/compact`、`/init`、`/reload-plugins` 和 `/exit` 继续直接执行。

| 命令 / 按键 | 说明 |
|---|---|
| `/help` | 浏览命令，Enter 将选项填入输入框 |
| `/model [spec]` | 设置主模型、子代理默认模型及 effort，或直接切换主模型到 `<provider>/<model>` |
| `/provider [子命令]` | 打开 provider 管理；`/provider add`、`remove`、`model add`、`model remove`、`default` 与 `harness provider` 相同，并立即生效（见 [LLM 网关](#llm-网关)） |
| `/effort [level]` | 交互选择推理强度，或直接设置为 `low`、`medium`、`high`、`xhigh`、`max`；`auto` 恢复为配置值 |
| `/clear` | 开始新会话 |
| `/compact [重点]` | 总结会话以腾出上下文（见[上下文压缩](#上下文压缩)）；`重点` 指定摘要要特别保留的内容 |
| `/resume [id]` | 继续当前目录下之前的会话（见[恢复会话](#恢复会话)） |
| `/rewind` | 回到之前某条提示之前，恢复代码、对话或两者（见[回退](#回退)） |
| `/init [说明]` | 让模型编写项目的 `AGENTS.md`，已有时改进它（见[项目说明](#项目说明)） |
| `/usage` | 按模型显示 REPL 启动以来用掉的 token（请求数、输入、缓存读取与写入、输出；包括子代理和上下文压缩，`/clear` 和 `/resume` 后不清零）、已知[价格](#llm-网关)的模型大约花了多少钱，以及当前会话大约占用了多少上下文窗口 |
| `/tools` | 浏览工具说明和输入参数 schema |
| `/output` | 浏览展开的整段会话；运行中也可按 Ctrl+O |
| `/skills` | 浏览技能，Enter 将调用命令填入输入框 |
| `/agents` | 浏览子代理类型、模型、权限和工具 |
| `/tasks` | 列出本会话启动的子代理（ID、状态、模型、工具调用次数、耗时、运行中子代理最近的一次调用）和[后台命令](#后台命令) |
| `/stop [id]` | 选择一个或全部运行中的任务停止；指定 ID 时直接停止对应子代理或后台命令（`shell_1`）（见[子代理](#子代理subagents)） |
| `/mcp` | 列出 MCP 服务器、其工具和日志文件 |
| `/plugins [子命令]` | 打开插件管理；`/plugins update` 拉取它们的最新版本，`/plugins install`、`uninstall`、`enable`、`disable`、`configure`、`marketplace …` 与 `harness plugin` 相同（见[插件](#插件plugins)） |
| `/reload-plugins` | 从配置重新加载插件、技能、命令、代理和 hook，供下一个会话使用：当前会话还是空的则立即生效，否则从 `/clear` 或 `/resume` 起生效 |
| `/sandbox` | 显示沙箱允许的操作（见[沙箱](#沙箱)） |
| `/permissions [模式\|rules]` | 选择[权限模式](#权限)、直接设置（`/permissions plan`），或列出规则 |
| `!命令` | 自己运行 `命令`，与模型的 `bash` 相同（开启沙箱时在沙箱中，但不受“禁止自行修改文件”的规则限制）；输出会显示出来，并随你的下一条消息交给模型 |
| `/btw <问题>` | 就当前会话问一个旁支问题：模型根据已知信息回答，不调用工具，问题和回答都不会留在会话中。只发一次前缀相同的请求，所以会命中提示词缓存 |
| `/copy [n]` | 把模型最后一条（或倒数第 `n` 条）回复复制到剪贴板：用 `pbcopy`、`wl-copy`、`xclip` 或 `xsel`，都没有时通过终端（OSC 52，终端允许时通过 SSH 也能用） |
| `/export [文件]` | 把会话写成 Markdown 文件（默认 `harness-<会话>.md`）：提示、回复，以及每个工具调用和其结果的开头 |
| `/<skill> [args]` | 直接运行一个技能（一次性模式同样可用：`harness /greet Bob`） |
| `/exit`、`exit`、Ctrl-D | 退出（`exit` 或 `quit` 仅在单独占一行时生效） |
| Esc | 中断运行中的轮次，同 Ctrl-C |
| Esc Esc | 在空的提示符下：`/rewind` |
| Shift+Tab | 切换到下一个[权限模式](#权限)：default、accept edits、plan、bypass permissions |
| 一轮运行期间按 ↑ | 把模型尚未拿到的排队消息取回输入框，以便修改 |
| Ctrl-C | 中断运行中的轮次：运行中的命令被终止，并告知模型请求已被中断。在提示符处连按两次退出 |
| 行尾 `\`、Alt+Enter | 在新行继续这条消息；终端能发送独立键序列时也支持 Shift+Enter |
| `/` | 在提示符下方打开命令菜单；随输入收窄，↑/↓ 选择，Tab 填入，Enter 运行。执行 `/model` 设置主模型、子代理模型及 effort；`/model ` 也可补全模型 |
| `@` | 打开文件菜单（见[用 `@` 附加文件](#用--附加文件)） |
| Ctrl+V | 粘贴剪贴板中的图片（见[粘贴图片](#粘贴图片)） |
| Ctrl+O | 打开/关闭展开的会话视图，模型运行期间同样可用 |

### 状态栏

[配置](#配置)中的 `statusLine`（[Claude Code 的格式](https://code.claude.com/docs/en/statusline)）会在输入框下方显示一行你自己定义的内容：

```json
{ "statusLine": { "type": "command", "command": "~/.harness/statusline.sh" } }
```

每次提示符打开时运行这个命令：通过 `sh -c` 在工作目录中运行，与 hook 一样不在沙箱中，stdin 收到 JSON：`session_id`、`transcript_path`、`cwd`、`model`（`id`、`display_name`）、`workspace`（`current_dir`、`project_dir`）、`version`、`effort`、`context_window`（`context_window_size`、`used_tokens`、`used_percentage`）和 `cost`（`total_input_tokens`、`total_output_tokens`）。显示它 5 秒内输出的第一行，保留颜色；命令失败时不显示。例如 `jq -r '"\(.model.display_name) · \(.context_window.used_percentage)% context"'`。

## 命令的环境

每条 `bash` 命令都在工作目录中新起一个 `bash -c` 进程运行，所以 `cd`、`export`、`source` 只对这一条命令有效。启动一条命令只需几毫秒（`/sandbox` 会显示沙箱额外增加的开销），与一次模型请求相比微不足道，因此不使用常驻 Shell：命令可以并发执行，超时只会杀掉这条命令及其启动的进程，一条命令的改动也不会影响后续命令，或其他子代理、Web 任务的命令。

`bash -c` 不读取任何启动文件，所以 harness 启动时会在后台运行一次你的登录 Shell（`$SHELL -l -i -c 'env -0'`，在 home 目录中运行，不连接终端），并把它最终得到的环境变量交给每条命令：例如 nvm、pyenv、conda 或 Homebrew 在 `~/.profile`、`~/.bashrc` 或 `~/.zshrc` 中设置的 `PATH`，以及 `JAVA_HOME` 之类的变量；即使 harness 不是从这样的 Shell 启动的也一样。第一条命令会等待它完成，最多 5 秒；如果失败或超时，命令就使用 harness 自身的环境，并在第一条命令的结果中说明原因。设置 `"shell": {"snapshot": false}` 可以关闭这个功能。MCP 服务器、语言服务器和 hooks 仍使用 harness 自身的环境。

命令继承 harness 的环境变量，但 harness 自己发送的凭据除外：`ANTHROPIC_API_KEY`、`ANTHROPIC_AUTH_TOKEN`、`OPENAI_API_KEY`、`HARNESS_WEB_TOKEN`，以及 providers 的 `api_key`、`headers` 和 MCP 服务器的 `headers` 中以 `${VAR}` 引用的变量。这样仓库里的脚本就读不到它们（在开放网络时也就无法把它们发出去）。MCP 服务器 `env` 中引用的变量会保留，因为 `gh` 这类工具常常共用它们。如果确实要让命令拿到其中某个变量：

```json
{ "shell": { "keep_env": ["OPENAI_API_KEY"], "snapshot": true } }
```

### SSH

命令里的每次 `ssh` 都会建立一条新连接：密钥交换加认证，即使连本机也要 0.2 秒，连远端机器要几秒。OpenSSH 可以共用一条连接（之后每条命令约 10 毫秒），在 `~/.ssh/config` 中加入：

```
Host *
  ControlMaster auto
  ControlPath ~/.ssh/cm-%C
  ControlPersist 10m
```

这样第一次 `ssh` 某台主机后，会留下一条主连接运行 10 分钟，之后的 `ssh` 都通过它的 socket 走。在 Linux 上开启[沙箱](#沙箱)时，命令启动的主连接会随命令一起结束（命令所在的 PID 命名空间被回收），所以要在 harness 之外自己启动它：在终端里执行 `ssh -fN myhost`，之后的命令就会复用它。socket 所在目录不能在 `deny_read` 中（如果隐藏了 `~/.ssh`，就把 `ControlPath` 设到别处）。注意，这时即使 `"network": false`，命令也能通过这个 socket 访问该主机，因为这个选项拦的是网络连接，不拦 Unix socket。

命令超时时，harness 杀掉的是本地的 `ssh`，而不是它在远端启动的进程；远端的长任务要在远端用 `timeout` 包一层，或者作为后台命令运行。

## 后台命令

`bash` 加上 `run_in_background: true` 会启动一个持续运行的命令（如开发服务器、watcher 或耗时的构建），并立即返回它的 ID（`shell_1`）。`bash_output` 返回它自上次查看以来的输出（stdout 和 stderr 合在一起，最多返回最新的 30000 字节；最多保留 1 MB）以及它是否仍在运行；`kill_shell` 停止它及其启动的进程。自行退出的命令会通知模型，模型空闲时会因此开始新的一轮，与后台子代理的报告相同。后台命令与其他 `bash` 命令一样受沙箱和文件修改规则约束，每个会话最多同时运行 8 个。`/tasks` 浏览任务，`/stop shell_1` 停止一个，`/stop` 可选择停止一个或全部任务（会告知模型）；`/clear`、`/resume`、删除 Web 任务或退出程序都会杀掉它们。

有限的后台命令默认使用 `background_kind: "task"`。单次 CLI 模式会等待它们和后台子代理收尾后才退出；交互 CLI 和 Web 界面会区分“等待后台任务”和“完成”。开发服务器、watcher 使用 `background_kind: "service"`，并指定 `resource`（如 `service:web`）；常驻服务不阻止任务进入空闲，但需要单独检查是否就绪。

下一步依赖结果时，调用 `wait_tasks`：`task_ids` 可以混合 shell ID 和子代理 ID；`mode` 为 `any`（任意一个完成）或 `all`（默认，全部完成）；`timeout_ms` 默认 60000，最大 300000。等待会挂起工具，不通过反复请求模型轮询；返回状态和已完成结果，并撤销尚未交给模型的同一条完成通知。等待超时、收到用户新消息或等待被打断，都不会杀掉后台任务；不再需要时明确停止。`bash_output` 仍用于即时读取增量输出。汇报成功前必须收集并检查必要的结果。

工具结果完成后会及时显示，并按调用 ID 配对；交给模型时仍是一批完整、按调用顺序排列的结果。独立读取可以并发，未知写入工具和默认命令在工作区上串行。后台命令会持有资源锁直到退出或被停止。只有能够与工作区安全并行的任务才应声明其他 `resource`，如 `host:aliyun`；相同资源的命令会串行。文件修改保留原有的先读后写与过期读取检查。这些锁协调 harness 内的工具，不控制外部程序或未声明的远程资源；不自动推断命令依赖；同时修改文件的子代理可以用 [worktree 隔离](#子代理subagents)分开。

## 大输出

太大而无法整个返回的输出（`bash`、`grep`、`glob` 或 `lsp` 超过 30,000 字节，MCP 工具超过 100,000 字节，子代理报告超过上限）不会被直接截断：模型拿到它的前三分之二和后三分之一，所以构建或测试输出的结尾仍然在，中间的说明给出保存全部内容的文件（`$TMPDIR/harness-<uid>/tool-output/`，只有你能读，每个最多 64 MB）。模型可以再用 `read` 或 `grep` 读取需要的部分。`bash` 在命令运行时就写这个文件，内存里只留开头和结尾；stdout 和 stderr 各有各的文件。

## 上下文压缩

当一次请求之后，模型 `context_window` 剩下的空间不够放一次完整回复（`max_tokens`）再加窗口 5% 的余量时，模型先总结会话，再用摘要替换历史，做法与 Codex 相同。窗口 256k、`max_tokens` 为默认的 64k 时，阈值约为 179k token；窗口 1M 时约为 886k。摘要通过一次普通请求获得（system 提示词和工具不变，缓存前缀仍然命中），请求末尾是一段总结提示：进展和决定、用户的约束、涉及的文件、遇到的错误及修复、剩余工作和下一步。新的历史只有一条消息：用户自己发过的消息（从最新往前，原文保留约 2 万 token），然后是摘要，以及最近读过的 5 个文件的当前内容（每个约 5k token 以内，总共不超过窗口的 10%）。assistant 回复、工具结果和附件都会丢弃，只有重新附上的文件仍算作 `edit` 和 `write` 已读过。

压缩发生在一轮开始前（新消息原样保留在摘要之后），也发生在同一轮的工具调用之间，所以长任务会自己继续下去；REPL 会显示 `✻ Compacted the conversation: context 172.4k → about 9.8k`。`/compact [重点]` 可以手动压缩。如果总结请求本身也超长，会去掉最早的几轮对话再试；如果压缩后上下文仍然太满，或同一轮里第三次又被填满，这一轮会报错停止，而不是无限循环。压缩可以像普通一轮一样打断，打断后历史保持原样。保存的会话存的是压缩后的历史，恢复时会在压缩处显示 `✻ Conversation compacted`。新的历史还会写明摘要取代了每个工具各多少次调用（`bash ×12, read ×30`），展开的对话记录（Ctrl+O）也会在每次压缩处标出这些数字。

**空闲压缩。** 上下文至少 20 万 token 的会话停在 REPL 提示符、或 web 任务空闲时，会在提示词缓存过期前 45 秒压缩（4¼ 分钟后；`"prompt_cache_ttl": "1h"` 时为 59¼ 分钟后），这样总结请求仍然命中缓存，你的下一条消息也不会把整段长历史以未缓存的价格重新发送。REPL 会在输入框上方显示 `✻ Compacted the idle conversation: …`，输入框保持打开；在压缩完成前发送消息会取消它，会话保持原样（web UI 中则是消息等压缩完成再发送）。PreCompact 和 PostCompact hook 看到的触发方式是 `auto`。在配置中设置 `"idle_compaction": false` 可以关闭，`"idle_compaction_min_tokens"` 设置阈值。

## 恢复会话

REPL 和一次性模式的每个会话都会在每轮结束后保存到 `~/.harness/sessions/<id>.jsonl`（仅所有者可读）：第一行是头部（id、工作目录、第一条提示、模型和时间），之后每行一条消息。`/clear` 会开始一个新文件，旧文件保留。

| | |
|---|---|
| `harness -c` / `--continue` | 继续当前目录最近的会话 |
| `harness -r` / `--resume` | 列出当前目录的会话并选择一个 |
| `harness -r <id>` / `--resume=<id>` | 继续指定会话；id 的唯一前缀即可 |
| `/resume`、`/resume <id>` | 在 REPL 内做同样的事 |

它们可以与一次性模式组合：`harness -c "now run the tests"` 或 `harness -r <id> "..."`（一次性模式下 `--resume` 需要给出 id）。恢复的会话沿用原来的 id；若上次使用的模型仍在配置中，则切换回该模型；在 REPL 中会重新显示最后几轮。推理块会被保留，因此仍会发回给产生它的模型。`edit` 和 `write` 的已读记录不会保存：模型在修改文件前需要重新读取。Web UI 的任务也以同样方式在每轮之后保存：新建任务时，web UI 会列出这个目录下保存的会话，可以在新任务中（主代理上）继续其中一个，并显示它之前的内容；已经在某个任务中打开的会话会直接打开那个任务。

## 回退

与 Claude Code 一样，你在 REPL 中发送的每条提示都是一个检查点：在它之后 `edit` 或 `write` 第一次修改某个文件之前，该文件原来的内容会被保留（若文件尚不存在，则记下这一点）。`/rewind`，或在空的提示符下按两次 Esc，会列出会话中的提示（最近的在前），以及每条提示改动了几个文件。按编号选择一条，查看回到它之前会改动哪些文件（`+3 -1`、`removed`、`recreated`），然后选择恢复什么：

| | |
|---|---|
| 代码和对话 | 以下两项都做 |
| 对话 | 这条提示及其之后的内容从对话中移除，提示放回输入框，可以修改后重新发送；文件保持现状 |
| 代码 | 之后改动过的每个文件都恢复到这条提示之前的样子，之后新建的文件被删除；对话保持不变 |

- 只跟踪 `edit` 和 `write`（包括子代理的）做的修改。`bash` 命令、MCP 工具或你的编辑器所做的修改不会被跟踪，恢复代码时会覆盖你之后对这些文件做的修改：它不能替代版本控制。
- 一轮运行期间从输入框发送的消息属于这一轮的提示；为后台子代理的报告运行的一轮不产生检查点。
- 上下文压缩会连同之前的对话一起移除更早的检查点：只能回退到压缩之后的提示。
- 回退对话之后，模型在修改文件前需要重新读取。
- 检查点随会话一起保存，保留的文件版本在 `~/.harness/file-history/<id>/`（仅所有者可读），因此 `--continue` 或 `/resume` 之后仍然可用。回退到第一条提示的会话不再保存。在 web UI 中，任务 `⋯` 菜单里的 `回退…` 会列出它的提示，可以回到其中一个之前，恢复代码、会话或两者；该提示会回到消息输入框中。

## 权限

默认情况下 harness 和以前一样，运行每个工具调用都不询问；约束模型命令的是操作系统[沙箱](#沙箱)。[配置](#配置)中的 `permissions` 部分（Claude Code 的格式）可以让它先询问，或者直接拒绝：

```json
{
  "permissions": {
    "defaultMode": "default",
    "allow": ["Bash(cargo test:*)", "Bash(git status)", "mcp__exa"],
    "ask": ["Bash(git push:*)"],
    "deny": ["Read(.env)", "Bash(rm -rf:*)", "mcp__gh__delete*"]
  }
}
```

- 规则使用 Claude Code 的权限语法，与 hook 的 `if` 相同：用工具名或 Claude Code 的叫法（`Bash`、`Edit`、`read`），括号里写命令的开头（`git commit:*`；复合命令的每一部分都算）或路径要匹配的模式（`src/**`）。`mcp__<服务器>` 表示该服务器的所有工具，末尾的 `*` 表示名字以此开头的工具。
- deny 规则在任何模式下都拒绝调用，即使 PreToolUse hook 给了 `allow`。其次，ask 规则（或 hook 的 `ask`）在任何模式下都会询问。再次，allow 规则或你在本会话中允许过的规则会直接放行。都不匹配时由模式决定：

| 模式 | |
|---|---|
| `bypassPermissions`（默认） | 全部运行 |
| `default` | 只读的调用直接运行（`read`、`grep`、`glob`、`lsp`、标记为只读的 MCP 工具），开启沙箱时 `bash` 也直接运行（在 `sandbox` 部分设置 `"auto_allow_bash": false` 则它也要询问）；其余都要询问 |
| `acceptEdits` | 同 `default`，另外工作目录中文件的 `edit` 和 `write` 直接运行 |
| `plan` | 只运行只读的调用，其余都拒绝，并告诉模型先调研、再给出计划；离开 plan 模式时会告诉它可以动手了 |

- 启动或等待其他工作的工具（`agent`、`message_agent`、`stop_agent`、`wait_tasks`、`bash_output`、`kill_shell`）从不询问；它们启动的子代理的调用会询问，因为子代理共享所在会话的权限和模式。后台子代理的询问和主代理的一样显示。
- 在 REPL 中，询问会占据输入框的位置：显示 `Allow bash(make)?` 和需要询问的原因（编辑还会显示改动的行），按 1 允许，按 2 允许并在本会话中放行同类调用（如 `Bash(make:*)`、`Edit`），按 3 或 Esc 拒绝并让模型换个做法。在 web UI 中，询问显示在任务里，带有按钮。会先运行 `PermissionRequest` [hook](#hooks)，它可以代你决定。无法询问时（一次性模式、重定向的输入、fork 运行的技能），调用会被拒绝，并告诉模型怎样才能被允许。
- 用 Shift+Tab、`/permissions` 或 `--permission-mode 模式` 切换模式；当前模式不是启动时的模式时，会显示在输入框下方。web UI 在 `⋯` 菜单中按任务设置。

## 沙箱

`harness --sandbox`（或 `HARNESS_SANDBOX=1`，或在配置的 `sandbox` 部分写 `"enabled": true`）让模型的 `bash` 命令和 `grep` / `glob` 搜索在操作系统沙箱里运行：macOS 用系统自带的 Seatbelt（`sandbox-exec`），Linux 用 [bubblewrap](https://github.com/containers/bubblewrap)（`apt install bubblewrap` / `dnf install bubblewrap`；它需要非特权用户命名空间，有些容器里没有）。`--no-sandbox` 或 `HARNESS_SANDBOX=0` 可以单次关闭。沙箱无法启动时，harness 直接退出，不会在没有沙箱的情况下运行。

- **读**：除 `deny_read` 以外都可读。
- **写**：只能写工作目录、临时目录（`/tmp`、`/var/tmp`、`$TMPDIR`，macOS 上还有每用户的临时和缓存目录）以及 `allow_write`。工作目录中的 `.harness/`、`.claude/`、`.mcp.json`，以及 git 仓库中的 `.git/hooks/` 和 `.git/config` 保持只读，因为它们会让代码之后在沙箱外运行（MCP 服务器、hooks、`core.fsmonitor`）；`.git` 本身不能被重命名或删除。
- **网络**：只允许回环地址（本地服务和测试可用，DNS 和外连会失败），除非设置 `"network": true`。
- 命令在没有终端的新会话中运行。Linux 上还有独立的 PID 命名空间，后台任务随命令一起结束；macOS 上无法运行 setuid 程序（`sudo`、`ps`）。

`read`、`edit`、`write` 和 `lsp` 工具按同样的规则检查。`/sandbox` 显示这些规则，REPL 横幅会显示 `sandboxed`，系统提示词也会告诉模型这些限制，让它报告被拦截的操作，而不是设法绕过。MCP 与语言服务器、[hooks](#hooks) 和 harness 本身不在沙箱中。

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

路径支持 `~/` 和 `${VAR}`，相对路径相对于工作目录。规则重叠时更深的路径优先，所以 `"deny_read": ["~"]` 会隐藏主目录，但其中的工作目录仍可正常使用。配置只来自你自己的配置文件，仓库无法关闭沙箱。git 元数据不在工作目录内时（worktree，或工作目录是仓库的子目录），需要把它的路径加入 `allow_write` 才能提交。Linux 上，尚不存在的受保护路径在命令运行期间由一个空的占位替身覆盖，没有命令运行后即删除。

## MCP 服务器

在[配置](#配置)的 `mcpServers` 下声明服务器，结构与 Claude Code 的 `.mcp.json` 相同。项目的 `.mcp.json` 不会被读取；需要的服务器请复制到你的配置里。

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

- 传输方式：`stdio`（设置了 `command` 时的默认值）和可流式的 `http`（设置了 `url` 时的默认值），通过官方 Rust SDK [`rmcp`](https://github.com/modelcontextprotocol/rust-sdk) 实现。stdio 服务器可以用 `cwd` 指定运行目录。不支持需要 OAuth 登录的服务器。
- 每个字符串中的 `${VAR}` / `${VAR:-default}` 都会从环境变量展开。
- 服务器在启动时于后台并发连接：REPL 立即可用（启动信息会显示还有几个服务器在连接），第一次请求前才等待它们连完，这样一个对话从一开始就有相同的工具，prompt cache 的前缀也保持不变。one-shot 模式和 Web UI 会先等连接完成再开始。连接失败的服务器会被报告并跳过；`/mcp` 会显示每个服务器连接用了多久。
- 连接断开时（服务器退出，或 HTTP 会话过期），下一次调用会重新建立连接，工具列表沿用最初获取的那份。被断线打断的调用只有在工具标记为只读（`readOnlyHint`）时才会自动重试；否则会告诉模型这次调用可能已经生效。重连失败后的 10 秒内，调用会直接失败，之后才会再次尝试重连。
- `/reload-plugins` 会保留配置没有变化的插件 MCP 服务器继续运行，不会重新启动它们。
- 工具以 `mcp__<server>__<tool>` 形式暴露。文本、内嵌文本资源和 PNG/JPEG/GIF/WebP 图片会传回给模型。
- stdio 服务器的 stderr 写入 `$TMPDIR/harness-mcp-<server>.log`。
- `"alwaysLoad": true` 让一个服务器的工具始终完整加载，即使其他工具被[延迟加载](#延迟加载的工具)。

### 延迟加载的工具

MCP 工具一多，还没开始干活就会占掉大量上下文。当它们的定义会占上下文窗口十分之一以上、且 provider 开启了 `tool_search` 时，对话会像 Claude Code 一样延迟加载它们：这些工具带 `defer_loading` 发送，API 不把它们的描述和 schema 放进上下文，模型只在一条 `<deferred-tools>` 消息里看到它们的名字。需要哪个，模型就用 `tool_search` 工具（hook 里叫 `ToolSearch`）加载：按名字（`select:mcp__gh__create_issue,mcp__gh__list_issues`）、按前缀（`mcp__gh`）或按关键词（`github issue`；`+slack send` 要求名字里有 `slack`）。搜索在本地进行，结果是 `tool_reference` 块，API 会把它们替换成完整定义；工具列表始终不变，prompt cache 的前缀也就不变。

- 对话在第一次请求前决定一次，直到 `/clear` 都保持不变；恢复或 fork 出来的对话，如果原来延迟了工具，会继续这样做。子代理按自己的工具单独决定。
- 延迟的工具有变化时、以及 compact 之后，会再告诉模型一次，并列出它之前加载过、需要重新加载的工具。
- 不使用 `defer_loading` 时（OpenAI provider，或 provider 拒绝了它之后），所有工具都完整发送，`tool_search` 加载了什么以文本告诉模型，所以 `/model` 切换后对话照常继续。

## Hooks

Hooks 在 agent 循环的特定时机运行你自己的命令：拦截危险命令、编辑后格式化文件、给 prompt 补充上下文，或者在测试通过前让模型继续干活。格式沿用 [Claude Code 的 hooks](https://code.claude.com/docs/en/hooks)（Codex 也是同一套），为它们写的脚本在这里也能用。在[配置](#配置)的 `hooks` 下声明：

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

| 事件 | 运行时机 | matcher 匹配 | 能做什么 |
|---|---|---|---|
| `PreToolUse` | 工具调用之前 | 工具名 | 拦截调用（退出码 2，或 `permissionDecision: "deny"`）、修改输入（`updatedInput`）、补充上下文 |
| `PostToolUse` | 调用成功之后 | 工具名 | 给模型反馈（退出码 2，或 `decision: "block"` 加 `reason`）、补充上下文、替换结果（`updatedToolOutput`） |
| `PostToolUseFailure` | 调用失败之后 | 工具名 | 给模型反馈、补充上下文 |
| `UserPromptSubmit` | 你的 prompt 发出之前；`/命令` 按你输入的原样给出，尚未展开 | — | 拒绝它（退出码 2 或 `decision: "block"`；原因显示给你）、补充上下文 |
| `Stop` | 模型结束回合时 | — | 让它继续干活（退出码 2 或 `decision: "block"`；原因就是给它的指示） |
| `SubagentStart` | 子代理的第一个请求之前 | agent 类型 | 给子代理补充上下文 |
| `SubagentStop` | 子代理结束一次运行时 | agent 类型 | 与 `Stop` 相同，作用于子代理 |
| `PreCompact` | 会话[压缩](#上下文压缩)之前 | `manual`（`/compact`）、`auto` | 阻止压缩（退出码 2 或 `decision: "block"`）；自动压缩被阻止时跳过压缩、请求照常发出，可能超出上下文窗口 |
| `PostCompact` | 压缩之后 | `manual`、`auto` | 不影响执行；会收到摘要 |
| `SessionStart` | 会话的第一个请求之前，以及 `/clear`、`/resume` 或压缩之后 | `startup`、`clear`、`resume`、`compact` | 补充上下文 |
| `SessionEnd` | 发过请求的会话结束时：`/clear` 或 `/resume` 替换它之前、退出 REPL 时、one-shot 运行结束时、删除其 web 任务时 | `clear`、`resume`、`prompt_input_exit`、`other` | 不影响执行；默认只有 1.5 秒，可用 `timeout` 设置（最多 60 秒） |
| `Notification` | 一个回合结束后，REPL 提示符或 web 任务等你输入已满 60 秒时 | `idle_prompt` | 不影响执行；会收到 `notification_type` 和 `message`，可用来弹出桌面通知 |
| `PermissionRequest` | 就某个工具调用询问你之前（见[权限](#权限)） | 工具名 | 代你决定：`hookSpecificOutput.decision.behavior` 为 `"allow"` 或 `"deny"`（可带给模型的 `message`）；它还会收到 `permission_suggestions` |

- hook 以 `sh -c <command>` 在工作目录中运行，不在沙箱内，`$CLAUDE_PROJECT_DIR` 和 `$HARNESS_PROJECT_DIR` 都设为工作目录。给出 `args`（字符串列表）时，`command` 带着这些参数直接启动、不经过 shell，参数不会被展开或切分（插件 hook 中的 `${CLAUDE_PLUGIN_ROOT}` 等仍会替换）。事件以 JSON 从 stdin 传入：`session_id`、`transcript_path`（保存的会话文件）、`cwd`、`hook_event_name`、`model`、`permission_mode`（会话的[权限模式](#权限)）、子代理中的 `agent_id` 和 `agent_type`，以及事件自己的字段（`tool_name`、`tool_use_id`、`tool_input`、`tool_response` 或 `error`；`prompt`；`stop_hook_active`、`last_assistant_message`；`trigger`、`custom_instructions`、`compact_summary`；`source`、`reason`）。
- 退出码 0：stdout 以 `{` 开头时按 JSON 读取（`continue` / `stopReason`、`systemMessage`、`decision` / `reason`、`hookSpecificOutput`），否则对 `SessionStart`、`SubagentStart` 和 `UserPromptSubmit` 作为给模型的上下文，对其他事件忽略。退出码 2 表示拦截，stderr 是原因。其他退出码、超时（`timeout`，单位秒，默认 600）、命令无法启动或无效 JSON 会显示给你，不影响执行；除非 command hook 设置了 `"onFailure": "block"`：这时失败与退出码 2 一样会拦截，适合出错时也不能被绕过的守卫脚本。`continue: false` 结束当前回合。
- 工具也可以用 Claude Code 的名字匹配，hook 看到的也是这些名字：`bash` 是 `Bash`，`read` / `edit` / `write` / `grep` / `glob` / `lsp` 是 `Read` / `Edit` / `Write` / `Grep` / `Glob` / `LSP`，`agent` 是 `Agent`（或 `Task`）；MCP 工具仍是 `mcp__<server>__<tool>`。只由名字组成的 matcher（`Edit|Write`）做精确匹配，其他的按正则表达式匹配（`mcp__fs__.*`），空串或 `*` 匹配全部。文件工具的 `path` 也会以 `file_path` 给出，`updatedInput` 用哪个都可以。
- `permissionDecision: "ask"` 会让这次调用先询问用户（见[权限](#权限)），`"allow"` 则不询问直接运行，除非被 deny 规则拒绝。
- `prompt` 类型的 hook（`{"type": "prompt", "prompt": "…", "model": "…", "timeout": 30}`）不运行命令，而是询问模型：当前会话的模型，或者 `model` 指定的、provider 认识的模型（Claude Code 的 `haiku` 之类别名不算）。事件的输入 JSON 替换 prompt 中的 `$ARGUMENTS`（没有占位符时附在末尾），模型回答 `{"ok": true}` 或 `{"ok": false, "reason": "…"}`；`ok: false` 等同于退出码 2 加上该原因，所以 Stop 上的 prompt hook 能让模型继续干活。默认 30 秒，可用 `timeout` 设置；它的请求不计入 `/usage`。
- 匹配同一事件的所有 hook 同时运行，最多同时 12 个；同一条命令列出两次只运行一次。hooks 在子代理（用 `SubagentStop` 代替 `Stop`）和 web 任务中同样生效。
- hook 给模型的内容放在 `<hook-feedback>` 块中；给你的内容（`systemMessage`、拒绝原因、运行失败）显示为 `⚑` 开头的一行。
- `if` 用 Claude Code 的权限规则语法把工具事件的 hook 限定到部分调用：`Bash(git commit:*)` 或 `Bash(git push *)`（命令，或复合命令中的某一段，以此开头；`*` 匹配任意内容）、`Edit(src/**/*.ts)`（文件路径），或只写工具名。`async: true` 的 hook 在后台运行，不阻塞调用；它给模型的内容随下一个请求发出。`asyncRewake: true` 同样在后台运行，若以退出码 2 结束，则用 `rewakeMessage` 加上 hook 给出的原因唤醒模型，模型空闲时会单独开一个回合。
- 只支持 `command` 和 `prompt` 类型的 hook 以及上面这些事件；其他 hook 类型（`http`、`agent`、`mcp_tool`）以及其他事件（`Elicitation` 等）在配置中会报错（[插件](#插件plugins)中的则给出警告并略过）。hooks 只来自配置文件及其启用的插件，仓库无法自行添加；修改在下次启动时生效，或在 `/reload-plugins` 后对下一个会话生效。

## 项目说明

如果工作目录下有 `AGENTS.md`，它的内容会放进系统提示词，主代理和子代理都有（内置的 `explore` 只做搜索，不带）。超过 40,000 字节时在行尾截断，并告诉模型其余部分自己读取。另外还会读取两种：

- `~/.harness/AGENTS.md`：你自己给所有项目的说明，也放进系统提示词，在项目的说明之前（20,000 字节处截断）。
- 工作目录的子目录里的 `AGENTS.md`（如 `crates/core/AGENTS.md`）：模型第一次用 `read` 读取它下面的文件时，附在文件内容之后，由外到内，每个只附一次（20,000 字节处截断）。[压缩](#上下文压缩)之后会再附一次，因为模型已经看不到它们了。

不会查找其他文件（`CLAUDE.md`、上级目录中的文件）。

文件在会话开始时读取：REPL 的第一个会话、`/clear` 或 `/resume` 之后，以及每个新的 web 任务。`/init` 让模型根据代码、`README.md` 和写给其他代理的说明（`CLAUDE.md`、`.cursorrules`、`.github/copilot-instructions.md`，这些文件保持不动）编写它，已有时则改进它；`/init <说明>` 指定还要涵盖的内容。一次性模式（`harness /init`）和 web UI 中同样可用。

## 技能（Skills）

技能是一个包含 `SKILL.md` 的目录（[Agent Skills](https://agentskills.io) 格式）：

```markdown
---
name: release-notes
description: Draft release notes from the git log. Use when asked for a changelog.
---
1. Run `git log --oneline <last-tag>..HEAD` ...
```

技能在 `.harness/skills/`、`.claude/skills/`、`.agents/skills/`（Codex 等 agent 共用的位置）、`~/.harness/skills/`、`~/.claude/skills/` 和 `~/.agents/skills/` 中发现（名称冲突时靠前的优先），[插件](#插件plugins)也可以带来技能。预先提供给模型的只有每个技能的名称和描述（截断到 300 个字符），列在 `skill` 工具里；正文在模型调用该工具或你输入 `/<name>` 时才加载。技能自带的脚本和参考文件通过普通工具使用，路径相对于技能的基准目录（加载的技能会说明该目录）。

与 Claude Code 相同：

- 正文中的 `$ARGUMENTS` 替换为全部参数，`$ARGUMENTS[N]` 或 `$N` 替换为第 N 个参数（从 0 开始，按 shell 的方式切分），`$name` 替换为 frontmatter 的 `arguments` 列表中 `name` 所在位置的参数；`\$` 保留 `$` 本身。没有被占位符用掉的参数以 `ARGUMENTS: …` 附在末尾。`${CLAUDE_SKILL_DIR}` 和 `${CLAUDE_PROJECT_DIR}` 替换为技能目录和工作目录。
- `` !`command` ``（位于行首或空格之后）和 ```` ```! ```` 代码块在加载技能时运行，并替换为它们的输出。它们与 `bash` 工具的命令一样运行，开启沙箱时在沙箱内；任一命令失败，技能就不会加载。只有在模型可加载的技能都不含这类命令时，只读子代理才会获得 `skill` 工具。
- `disable-model-invocation: true` 让模型不能加载该技能（只能用 `/<name>` 运行）；`user-invocable: false` 让它不出现在 `/` 命令中。
- `context: fork` 让技能在单独的[子代理](#子代理subagents)中运行：加载它会以技能文本为 prompt 启动一个子代理，只有它的报告会返回（用 `/<name>` 运行时，报告交给模型，由模型接着处理）。`agent` 指定子代理类型（默认 `general-purpose`），`model` 指定其模型（`<provider>/<model>`；`sonnet`、`opus`、`haiku` 和 `inherit` 表示默认模型），`allowed-tools`（`Read, Grep, Bash(git status:*)`；括号内的部分忽略）只保留其代理工具中列出的那些。子代理不能再启动子代理，在子代理中这类技能会就地加载。

Claude Code 的命令（技能的旧形式）同样可用：`.harness/commands/`、`.claude/commands/` 或 `~` 下同名目录（靠前的优先）中的每个 Markdown 文件都以 `/<文件名>` 运行，子目录会多一段（`frontend/component.md` 是 `/frontend:component`）。它们支持相同的 frontmatter（没有 `description` 时用第一行作为说明）和占位符；除非设置 `disable-model-invocation`，模型也能通过 `skill` 工具加载它们。技能与命令重名时技能优先。

## 子代理（Subagents）

主代理可以用 `agent` 工具委派一个自包含的任务。子代理在自己的全新会话中运行，有自己的系统提示和工具集，只有最终报告会返回，从而把漫长的搜索挡在主上下文之外。同一次响应中启动的多个子代理并行运行。

- **`explore`**（内置）：只有 `grep` 和 `glob` 工具的代码库搜索代理。用于“X 在哪里 / 哪些文件用了 Y”这类问题。
- **`general-purpose`**（内置）：拥有主代理全部工具（`bash`、`edit`、`write`、MCP 工具等，受同一沙箱约束）的编码代理，用于多步调研或范围明确的修改。子代理共享工作目录，因此主代理会被要求给每个修改文件的子代理分配各自的文件。
- **`fork`**（内置）：主代理的一个副本，带着整段会话、相同的 system 提示词、工具和模型启动，用于需要会话中已有信息的支线任务。它的请求与主代理以相同的前缀开头，会从提示词缓存读取，所以即使上下文很长，启动 fork 也很便宜。它的第一条消息先回应启动它的那次回复中的工具调用（启动它的那个调用会说明它是 fork），再给出任务。为了保持前缀相同，它的工具列表里也有子代理工具，但在 fork 里调用会被拒绝。
- **自定义代理**：`.harness/agents/` 或 `.claude/agents/`（项目或 `~`）中的 Markdown 文件（先找到的优先，与内置代理同名的文件会覆盖内置项）：

  ```markdown
  ---
  name: reviewer
  description: Reviews the current diff for bugs. Use after making changes.
  tools: read, grep          # 可选；`mcp__server__*` 匹配前缀
  permission: read-only      # 默认；`write` 允许 bash/edit/write 和非只读的 MCP 工具
  model: claude-sonnet-5     # 可选
  effort: low                # 可选；思考力度（low … max）
  autoCompactWindow: 100000  # 可选；按这么大的窗口压缩它的会话
  ---
  You are a code reviewer...
  ```

- **自定义代理默认只读**：只读代理只会获得标记为只读的工具（`read`、`grep`、`glob`、`lsp`、`skill`，以及服务器设置了 `readOnlyHint` 的 MCP 工具），即使它的 `tools` 列表写了其他工具也不行。`tools` 用于进一步收窄。没有 `bash` 的代理也不会获得 `bash_output` 和 `kill_shell`，因为它没有后台命令。`agent` 工具按每种代理实际拿到的工具标注它是只读还是能修改文件，描述截断到 400 个字符。
- 子代理不能再启动子代理。它们的文件读取使用自己的追踪器，因此不会计入主代理 `edit` 和 `write` 所需的已读记录。
- 模型：代理的 `model`，否则 `subagent_model` / `HARNESS_SUBAGENT_MODEL`，否则会话当前使用的模型（包括 `/model`、`/effort` 切换之后）；任意 `<provider>/<model>`。Claude Code 的 `sonnet`、`opus`、`haiku` 别名视同未设置，与 `inherit` 相同。思考力度：`agent` 调用的 `effort`，否则代理自己的，否则会话的。
- 工具可以用 Claude Code 的名称（`Read`、`Bash`、`KillShell`），会映射为 harness 自己的工具（`LS` 映射为 `glob`）。
- `/agents` 列出可用类型。

### 后台子代理与后续追问

- **前台**（默认）：`agent` 调用会等待报告。中断本轮会停止该子代理。
- **后台**（`run_in_background: true`）：调用立即返回子代理的 `agent_id`，主代理继续工作。子代理完成后，报告随主代理的下一次请求送达：在一轮之中，就跟在当时正在运行的工具调用结果之后（如果模型正在写回答，它会继续而不是结束本轮）；本轮结束之后，则单独开始一轮。REPL 在提示符为空时开始这一轮；已经输入的内容不会丢失，报告改为随你的这条消息一起送达。一次性模式会等后台子代理结束再退出（期间显示每个子代理最近的一次调用），Web 任务在空闲时开始这一轮。中断本轮时后台子代理继续运行。
- **后续追问**：每个子代理都保留自己的会话。`message_agent` 让已完成或已停止的子代理带着原有上下文处理一条新消息（等待其报告，或在后台运行）；仍在运行的子代理会在下一步收到这条消息，主代理同时得知它的进度（工具调用次数、耗时、最近的一次调用）。`stop_agent` 停止子代理，它已做的修改会保留。
- `/tasks` 列出本会话的子代理；`/stop <id>` 停止一个，`/stop` 停止所有运行中的。你停止的子代理会随你的下一条消息告知主代理。Web UI 中每张子代理卡片都有停止按钮。
- 同时运行的代理（主代理、它的子代理、Web 任务）逐个修改文件：如果某个文件在本代理读过之后被另一个代理改过，`edit` 和 `write` 会拒绝修改，而不是覆盖对方的改动。
- **worktree 隔离**：`agent` 调用带上 `isolation: "worktree"` 时，子代理在自己的 git worktree 中工作：从当前检出的提交新建分支 `harness/<id>`，放在 `$TMPDIR/harness-<uid>/worktrees/`（沙箱允许命令写入这里）；它的相对路径和命令都从那里开始，并且位于仓库中相同的子目录。它只能看到已提交的文件，看不到未提交的改动。结束时，如果它在 worktree 中什么都没改，worktree 会被删除；否则它的报告会说明改动在哪里（文件、提交、分支）以及如何审阅、合并和删除，这些由主代理决定。用 `message_agent` 追问时会继续在同一个 worktree 中工作。需要一个至少有一次提交的 git 仓库；`lsp` 仍然查看主检出目录。
- 每个会话最多同时运行 8 个子代理，保留最近的 32 个用于后续追问。它们只存在于内存中：`/clear`、`/resume`、删除 Web 任务或退出程序都会停止并遗忘它们。

## 插件（Plugins）

插件采用 [Claude Code 的格式](https://code.claude.com/docs/en/plugins-reference)：一个目录，打包了技能、命令、子代理、hook、MCP 服务器和语言服务器，通常从插件市场（在 `.claude-plugin/marketplace.json` 中列出插件的仓库）安装。[Codex 的插件](https://developers.openai.com/codex/plugins/build)也能加载（见下文）。`harness plugin` 负责安装和管理（REPL 中的 `/plugins …` 作用相同）：

```sh
harness plugin marketplace add your-org/plugins   # owner/repo、git URL（#ref 指定分支或 tag）、目录或 marketplace.json
harness plugin install fmt@team                   # 拉取、启用，并询问它需要的设置
harness plugin list --available                   # 已安装的插件，以及已知市场提供的插件
harness plugin disable fmt@team                   # 另有：enable、uninstall [--keep-data]、configure、update
harness plugin marketplace remove team            # 另有：marketplace list；add --auto-update
harness --check-plugins                           # 全部加载一遍，说明哪些加载了、哪些没有，然后退出
```

这些命令修改[配置](#配置)，文件的其余部分保持原样；插件在配置中用 Claude Code 的键、以 `<plugin>@<marketplace>` 启用，也可以手工编写：

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

- `claude-plugins-official`（[anthropics/claude-plugins-official](https://github.com/anthropics/claude-plugins-official)）和 Codex 的 `openai-curated`（[openai/plugins](https://github.com/openai/plugins)）无需声明即可使用；Codex 的个人市场 `~/.agents/plugins/marketplace.json` 也一样，按它自己声明的名字使用。其他市场可以来自 `github`（`repo`、`ref`、`path`）、`git`（`url`、`ref`、`path`）、`directory`（`path`，原地读取，修改在下次启动时生效）或 `file`（指向 `marketplace.json` 的 `path`）。
- 市场中的插件可以就在市场里（相对路径，或 `metadata.pluginRoot` 下的裸名称），可以在各自的仓库中（`github`、`url` 和 `git-subdir` 来源，可带 `ref` / `sha`），也可以是 npm 包（`npm` 来源：`package`、`version`、`registry`；用 `npm pack --ignore-scripts` 拉取，所以需要安装 `npm`，包里的脚本不会运行）。不支持 `archive` 和 `command` 来源。
- 缺少的内容在启动时拉取（`git clone --depth 1`、`npm pack`）到 `~/.harness/plugins`，按版本存放：`marketplaces/<name>/<version>/` 和 `cache/<marketplace>/<plugin>/<version>/`，版本是提交或包的版本号，`current` 文件记录正在使用的版本。`/plugins update`（或 `harness plugin update`）把每个市场和插件的最新版本拉取到旧版本旁边（固定到某个 `sha` 的保持不变），在下次启动或 `/reload-plugins` 后生效。已在运行的 hook 和 MCP 服务器继续读旧版本，旧版本在没有运行中的 harness 进程使用后删除。
- 设置了 `"autoUpdate": true`（`marketplace add --auto-update`）的市场每次启动时在后台连同其插件重新拉取，新版本从下次启动起生效，经过记录在 `~/.harness/plugins/auto-update.log`。不设置时，版本只在你更新时变动。
- 插件的数据目录（`${CLAUDE_PLUGIN_DATA}`）是 `~/.harness/plugins/data/<id>`；`uninstall` 会删除它，除非加上 `--keep-data`。
- `--plugin-dir DIR` 在本次运行中以 `<name>@inline` 加载 `DIR` 中的插件，与 Claude Code 的同名参数一致。

插件的各个部分都以插件名命名，与 Claude Code 相同：

| 部分 | 位置 | 名称 |
|---|---|---|
| 技能 | `skills/<name>/SKILL.md`，加上清单 `skills` 中的目录 | `/<plugin>:<name>` 及 `skill` 工具 |
| 命令 | `commands/**/*.md`，或清单 `commands` 指定的内容（文件、目录，或名称到 `source` 文件或 `content` 的映射） | `/<plugin>:<name>`（子目录会多一段：`/<plugin>:db:migrate`）；除非设置 `disable-model-invocation`，模型也可以把它们当作技能加载 |
| 代理 | `agents/**/*.md`，或清单的 `agents` | `<plugin>:<name>` [子代理](#子代理subagents) |
| Hooks | `hooks/hooks.json` 加上清单的 `hooks` | 与你的 hook 一起运行（见 [Hooks](#hooks)） |
| MCP 服务器 | `.mcp.json` 加上清单的 `mcpServers` | `plugin:<plugin>:<server>`，工具名 `mcp__plugin_<plugin>_<server>__<tool>` |
| 语言服务器 | `.lsp.json` 加上清单的 `lspServers` | `lsp` 工具（见[语言服务器](#语言服务器)） |
| 可执行文件 | `bin/` | 加到模型所运行命令的 `PATH` 中，排在你自己的目录之后；系统提示词会列出它们 |

- `.claude-plugin/plugin.json` 可以省略；没有它时由市场条目代替，插件名取目录名。清单中的路径必须位于插件目录之内。
- Codex 和 Cursor 的目录结构同样识别：`.agents/plugins/marketplace.json` 或 `.cursor-plugin/marketplace.json` 中的市场目录（其中的路径从包含 `.agents/` 的目录算起）、`local` 来源和带 `path` 的 `url` 来源、`.codex-plugin/plugin.json` 或 `.cursor-plugin/plugin.json` 清单，以及根目录下的 [Agent Plugins](https://agent-plugins.org) `plugin.json`（技能在 `skills/`，MCP 服务器在 `mcp.json`，hook 在 `extensions."com.openai"` 下）。hook 和 MCP 服务器也会拿到 Codex 的 `PLUGIN_ROOT` / `PLUGIN_DATA`。Codex 的 apps（ChatGPT 连接器）会被略过；它的 MCP 设置中保留 `cwd`（在插件目录内解析），注释和超时设置则丢弃。
- `${CLAUDE_PLUGIN_ROOT}` 和 `${CLAUDE_PLUGIN_DATA}`（或 `${PLUGIN_ROOT}` 和 `${PLUGIN_DATA}`）会在 hook 命令、MCP 与语言服务器配置以及技能、命令和代理的文本中被替换；hook 和 stdio MCP 服务器、语言服务器还会以环境变量的形式拿到它们。
- 插件的 `userConfig` 声明它需要的设置（`type`：`string`、`number`、`boolean`、`directory` 或 `file`；`title`、`description`、`default`、`sensitive`）。`install` 和 `configure` 会逐项询问；答案存入配置的 `pluginConfigs`，但 `sensitive` 的存到 `~/.harness/plugins/secrets.json`（仅你可读）。它们替换 hook 命令、MCP 与语言服务器配置中的 `${user_config.<key>}`，非敏感的也替换技能、命令和代理文本（模型会读到）中的；hook、MCP 与语言服务器还会以 `CLAUDE_PLUGIN_OPTION_<KEY>` 环境变量拿到它们。缺少答案且没有默认值的插件照常加载，但会给出警告，提示运行 `harness plugin configure`。
- 插件代理按 Claude Code 的方式获得 `tools`，即这份列表就是代理能用的全部工具：除非声明 `permission: read-only`，否则可以写（`bash`、`edit`、`write`），不写 `tools` 则获得除子代理工具外的全部工具。这和你的[自定义代理](#子代理subagents)默认只读不同：插件是你启用的，它的代理也是按这套规则写的。harness 没有的工具（`WebFetch`、`WebSearch`、`TodoWrite`）会被跳过。由于插件的技能、命令和代理用的是 Claude Code 的工具名（`Task`、`Bash`、`Read` 等），系统提示词（插件代理的也一样）会说明每个名字在这里对应哪个工具，以及哪些没有对应工具。
- harness 用不了的部分会在启动时给出警告并略过，插件的其余部分照常加载：它不运行的 hook 事件或类型、MCP 包（`.mcpb`）、需要 OAuth 登录或配置不被接受的 MCP 服务器、命令没有安装的语言服务器。输出风格、monitors、channels、默认的 `settings.json` 以及 `dependencies`（不会随之安装）不会被使用；`/plugins` 和 `--check-plugins` 会列出它们。
- `/plugins`（以及 Web UI 的环境页）列出每个插件提供的内容、被略过的部分，或它未能加载的原因。`harness --check-plugins`（也可加 `--plugin-dir`，用来试正在编写的插件）不启动会话，加载所有已启用插件、启动它们的 MCP 服务器、检查语言服务器的命令是否存在，说明哪些加载了、哪些被略过及原因；有插件或服务器失败时以非零退出码退出。
- `/reload-plugins` 重新读取配置，重新加载插件、技能、命令、代理和 hook，以及插件的 MCP 服务器。一个会话的工具和系统提示词保持不变（这是提示词缓存的前缀），所以新内容对下一个会话生效：当前会话还是空的则立即生效，否则从 `/clear` 或 `/resume` 起生效。provider、沙箱和你自己的 MCP 服务器需要重启才生效，Web UI 也需要重启。
- 插件会运行命令（hook、MCP 与语言服务器、命令中的 `!` 行），因此只有你的配置和命令行可以启用插件，项目不行。

### 从零安装一个插件

插件市场就是一个列出插件的目录（或仓库）。先建一个市场，里面放一个带命令、hook 和一项设置的插件：

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

然后添加市场、安装插件（会询问 `name`）、检查并使用：

```sh
harness plugin marketplace add ~/my-plugins   # added marketplace mine with 1 plugins
harness plugin install hello@mine             # Your name (string) [friend; Enter keeps it]: Ada
harness --check-plugins                       # ✓ hello@mine v1.0.0  1 command, hooks (SessionStart)
harness /hello:greet                          # 或在 REPL 中输入 /hello:greet（REPL 已在运行时先 /reload-plugins）
```

此时配置中启用了 `hello@mine`，并保存了 `"pluginConfigs": {"hello@mine": {"options": {"name": "Ada"}}}`。git 仓库中的市场用同样的方式添加（`harness plugin marketplace add your-org/plugins`），只是会被克隆而不是原地读取。

### 语言服务器

插件的语言服务器（Claude Code 的 `.lsp.json` 格式）为模型提供 `lsp` 工具：

```json
{ "go": { "command": "gopls", "args": ["serve"], "extensionToLanguage": { ".go": "go" } } }
```

- 每个服务器支持 `command`、`args`、`env`、`extensionToLanguage`、`initializationOptions` 和 `settings`（作为 `workspace/configuration` 的回答）；只支持 `stdio` 传输，Claude Code 的超时和重启设置会被忽略。命令需要已经安装：`--check-plugins` 会指出没装的。
- 工具第一次询问某个扩展名的文件时，对应的服务器在工作目录中启动，之后一直运行到 harness 退出（或被 `/reload-plugins` 替换）。它的 stderr 写到 `$TMPDIR/harness-lsp-<server>.log`。
- 工具的操作有 `definition`、`implementation`、`references`、`hover`、`symbols`（文件大纲）、`workspace_symbols`（按 `query`）和 `diagnostics`（文件在磁盘上当前内容的错误和警告；会先把文件的新内容发给服务器）。行号和列号从 1 开始，与 `read` 显示的一致。
- 和 MCP 服务器一样，语言服务器在[沙箱](#沙箱)之外运行；工具会按沙箱的读取规则检查它涉及的文件。它算作只读工具，所以只读子代理也能用。

## 目录结构

- `src/main.rs` — 启动：配置、`harness plugin`、`harness provider` 和 `--check-plugins`、沙箱，然后是 `setup.rs` 构建的内容；最后进入 CLI 或 web 前端
- `src/setup.rs` — 会话由什么构成：插件及其技能、代理、hook、MCP 与语言服务器（由 `/reload-plugins` 重建）、用户的 MCP 服务器、工具集和系统提示词；`--check-plugins`；后台自动更新
- `src/settings.rs` — 配置文件：位置、`--settings`、解析为各个部分、原地修改
- `src/cli/` — 终端前端：启动与 REPL 主循环（`mod.rs`）、一次性模式（`one_shot.rs`）、内置命令及输入行的解析（`parse.rs`）、运行一轮并在其下保留输入框（`turn.rs`）、选择、恢复（并重新显示其结尾）和保存会话（`sessions.rs`）、命令逐行打印的内容（`print.rs`）、`--output-format json` / `stream-json`（`json.rs`）、`!命令`、`/btw`、`/copy` 和 `/export`（`actions.rs`）、询问工具调用（`approve.rs`）、状态栏（`status_line.rs`）、`/rewind`（`rewind.rs`）、斜杠命令打开的键盘面板（`dialog.rs`）、各个无参数命令在面板中显示的内容及在一轮运行期间执行命令（`commands.rs`）、设置模型和 effort 的 `/model` 面板（`model.rs`）、共享的标准输出句柄，让旋转指示器和输入框始终画在输出下方（`output.rs`）、实时显示一轮的进度（`terminal.rs`）、工具调用及其结果的显示方式（`calls.rs`）、保留工具返回结果（`tool_output.rs`）、可搜索的展开会话视图（`transcript.rs`）、一轮运行期间的输入框：其文本、渲染和排队的消息（`input.rs`）、一轮运行期间读取的按键：Esc 和编辑输入框（`keys.rs`）、流式 Markdown 渲染（`markdown.rs`）、显示宽度辅助（`text.rs`）、提示符下方的菜单：命令、项目文件搜索（`menu.rs`）、共享屏幕布局与原生滚动历史（`screen.rs`）、提示符输入、菜单按键、补全、历史、多行输入、Ctrl+V 粘贴和 Esc Esc（`repl.rs`）、从剪贴板读取图片或复制的文件（`clipboard.rs`）
- `src/web/` — web 前端（`--web`）：axum 服务器与 JSON/SSE API（`mod.rs`）、后台任务及其事件日志（`tasks.rs`）、单页 UI（`index.html`，内嵌于二进制）
- `src/agent.rs` — 代理循环：请求 → 并发运行工具 → 发送结果 → 循环直到 `end_turn`；收件箱，把期间到达的内容（如后台子代理的报告）交给模型
- `src/agent/usage.rs` — `/usage` 显示的按模型统计的 token 数，与子代理共享
- `src/agent/compact.rs` — 上下文压缩：何时压缩、总结请求、替换会话的新历史
- `src/llm/` — LLM 网关
  - `config.rs` — 由配置和环境变量得到 provider、模型 spec 解析、按 provider/模型的设置
  - `manage.rs` — `harness provider` 与 `/provider …`：通过修改配置添加和移除 provider 与列出的模型、设置默认模型
  - `anthropic.rs` — Messages API 后端（SSE 转内容块、thinking、提示词缓存、拒答回退）
  - `openai.rs` — Responses API 后端（历史 ⇄ 输入/输出项转换、加密推理）
  - `http.rs` — POST（开启时用 gzip 压缩）、SSE 读取器；标记哪些失败（连接断开、过载、可重试的状态码）之后 `llm::Client` 会重发请求，以及要等多久
- `src/prompt/` — 系统提示词：主代理的模板（`system.md`）及其工具、环境和 `AGENTS.md`，子代理的提示词（`mod.rs`）；`/init` 的要求（`init.md`）
- `src/session.rs` — 会话保存：会话文件、`--continue` / `--resume` 参数
- `src/checkpoint.rs` — `/rewind` 的检查点：`edit` 和 `write` 修改的文件在每条提示之前的版本，随会话保存；恢复它们
- `src/mentions.rs` — `@路径` 的解析与展开为附件块；粘贴图片的附件块
- `src/permissions.rs` — 权限：配置中的规则、各个模式、哪些调用直接运行、询问或拒绝，以及通过前端询问
- `src/sandbox.rs` — 操作系统沙箱：配置、Seatbelt 配置文件与 bwrap 参数、`read`、`edit` 和 `write` 检查的路径规则
- `src/subagent.rs` — 子代理定义（`explore`、`general-purpose`、自定义文件）、权限、`agent`、`message_agent` 和 `stop_agent` 工具
- `src/subagent/worktree.rs` — `isolation: "worktree"` 子代理的 git worktree：创建、结算与删除
- `src/subagent/runtime.rs` — 一个会话的子代理：前台与后台运行、后续追问、停止、向主代理收件箱报告
- `src/mcp.rs` — MCP 客户端：服务器配置、连接、`mcp__*` 工具适配器
- `src/lsp.rs` — 插件语言服务器的 LSP 客户端：配置、按需启动、基于 stdio 的 JSON-RPC、诊断；`lsp` 工具
- `src/hooks.rs` — hooks：配置中的 `hooks` 部分、matcher、Claude Code 工具名、运行命令（经过 `sh` 或直接启动）和 prompt hook，并解析其输出
- `src/skills.rs` — 技能发现、`SKILL.md` frontmatter 解析（含 `context: fork`）、命令、技能的渲染（`$ARGUMENTS`、`` !`command` ``）、`skill` 工具
- `src/plugins.rs` — 插件：配置中的插件市场、`marketplace.json` 与 `plugin.json`、每个插件的技能、命令、代理、hook、MCP 与语言服务器、`userConfig` 的答案，以及未被使用的部分
  - `fetch.rs` — 从 git 或 npm 拉取到 `~/.harness/plugins` 的市场和插件，按版本存放；更新它们，并删除没有运行中进程使用的旧版本
  - `manage.rs` — `harness plugin` 与 `/plugins …`：通过修改配置（及敏感设置文件）安装、启用、配置和移除插件与市场
- `src/tools/` — `Tool` trait、注册表和 `FileTracker`（共享的读/改/写状态）
  - `bash.rs` — 通过 `bash -c` 运行命令（超时、大输出经 `spill.rs` 保存）；拒绝自行修改文件的命令（写文件的内联 Python/Node/Perl 脚本、`sed -i`、写入文件的 heredoc 或 `echo`），这些修改应走 `edit` / `write`
  - `read.rs` — 读取文本文件并带行号（`offset` / `limit` 分页），或读取图片、PDF 供模型查看
  - `edit.rs` — 把文件中一段精确且唯一的文本替换掉（`replace_all` 则替换全部出现），修改的开销只取决于改动本身；与 `write` 相同的先读规则，同一次回复中对同一文件的多个修改按顺序生效
  - `write.rs` — 创建或覆盖文件；已存在的文件必须先被读取且此后未被修改
  - `scheduling.rs` — 共享工作区、文件及显式资源的执行锁
  - `progress.rs` — 交给 CLI 和 Web 的工具等待事件
  - `wait.rs` — 等待一个或多个后台命令、子代理
  - `snapshot.rs` — 登录 Shell 的环境，启动时为模型执行的命令采集一次
  - `shells.rs` — 后台命令（`bash` 的 `run_in_background`）：每个会话各自的一组，`bash_output` 和 `kill_shell` 工具，退出时通知收件箱
  - `todo.rs` — `todo_write` 工具：模型的待办清单，每次整体发送（未注册，保留以便重新启用）
  - `workdir.rs` — 子代理自己的工作目录（它的 worktree），工具据此解析相对路径
  - `spill.rs` — 太大无法返回的输出：给模型开头和结尾，全部内容写入文件
  - `grep.rs` — 通过 ugrep 的正则搜索（PCRE，遵循 `.gitignore`，`files_with_matches` / `content` / `count` 模式）
  - `glob.rs` — 通过 ugrep 按 glob 模式查找文件（`.gitignore` 与沙箱规则同 `grep`；支持 `{a,b}`；最近修改的在前，最多 `head_limit` 个，默认 100）

在官方 Anthropic API 上，工具以 `eager_input_streaming` 声明，因此较大的输入（例如 `write` 的文件内容）会在生成时流式传输。此后 API 不再校验工具输入，所以每个内置工具都把输入解析成严格的结构体（拒绝未知字段），流式 JSON 解析失败的调用不会执行：模型会得到 `INVALID_JSON` 错误结果。MCP 工具保持输入缓冲开启，因此 API 会按服务器提供的 schema 校验其输入。

## 添加工具

实现 `tools::Tool`（name、description、JSON schema、异步 `run`），并在 `ToolRegistry::with_defaults` 中注册。
