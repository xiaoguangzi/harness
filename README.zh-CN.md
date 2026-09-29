# harness

[English](README.md) | 简体中文

一个用 Rust 编写的极简编码代理（coding-agent）框架。它通过原生 HTTP（带流式传输）与任意组合的 LLM 网关通信，支持 Anthropic Messages 协议或 OpenAI Responses 协议。

## 环境要求

- Rust 1.88+
- `PATH` 上需要有 [ugrep](https://github.com/Genivia/ugrep)，供 `grep` 工具使用（`apt install ugrep` / `brew install ugrep`）
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
| `mcpServers` | [MCP 服务器](#mcp-服务器) |
| `sandbox` | [沙箱](#沙箱) |

所有键都可省略，未知的键会报错。项目目录中的配置一概不读（包括项目的 `.mcp.json`）：否则仓库可以把带着你 API key 的请求发到它自己的服务器、启动 MCP 命令或放宽沙箱。项目中的 `.harness/agents`、`.harness/skills` 及 `.claude/` 下的同名目录仍会读取，因为它们只增加提示词。旧版本读取 `~/.harness/providers.json`、`mcp.json` 和 `sandbox.json`；这些文件存在时 harness 会给出警告，其内容应移到上面对应的键下（`default_model` 改名为 `model`）。

## Web UI

`harness --web`（或 `--web=ADDR`，默认 `127.0.0.1:8787`）启动浏览器 UI 而非 REPL：

- **任务 (Tasks)**：把任务派发给主代理，或直接派发给某个子代理类型（`explore`、`general-purpose` 或自定义代理），也可指定模型。任务在后台运行，可以多个并发；每个任务都是独立的会话，流式展示文本、工具调用和结果。每个子代理有一张卡片，显示它的工具调用、状态、报告和停止按钮；任务空闲时，后台子代理的报告会自动开始新的一轮。可以用后续消息继续任务、切换模型、取消任务（运行中的 `bash` 命令和前台子代理会停止，后台子代理继续运行）或删除任务（其子代理也会停止）。`@路径` 提及、`/<skill> args` 和 `/compact` 与 REPL 中一致。
- 助手文本按 Markdown 流式渲染（代码块、列表、表格、链接）；工具调用的摘要行直接显示它的对象（`bash` 命令、文件路径、`grep` 模式）。切换模型和删除任务放在任务头部的 `⋯` 菜单里。
- **工具 (Tools)**：列出所有已注册的工具（内置、技能、MCP、`agent`）及其描述和 schema；填入 JSON 输入即可直接调用。
- **模型配置 (Models)**：当前生效的 provider 和模型，以及配置文件的表单或原始 JSON 编辑器。保存时会先结合环境变量校验，模型配置随即无需重启生效，空闲任务会切换过去（`mcpServers` 和 `sandbox` 的修改在下次启动时生效）。
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
| `GET /api/tasks/{id}/events` | server-sent events：先回放该任务的日志，再实时推送（可通过 `Last-Event-ID` 恢复） |

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
| `context_window` | 模型的上下文窗口（token 数，默认 `200000`）：顶层、按 provider 或按模型设置；会话在窗口占满前会被[压缩](#上下文压缩) |
| `effort` | 顶层或按模型设置：`low`、`medium`、`high`（默认）、`xhigh` 或 `max`。以 Anthropic 的 `output_config.effort` 或 OpenAI 的 `reasoning.effort` 发送；`extra_body` 仍可覆盖它，例如对不支持 effort 的模型用 `{"output_config": null}` |
| `extra_body` | 合并进每个请求体，也可按模型设置；`null` 会移除默认键，例如对不支持自适应思考的模型用 `{"thinking": null}`，或对拒绝加密推理的网关用 `{"include": null}` |
| `fallbacks`、`eager_input_streaming` | 仅 Anthropic 的 API 特性；`api.anthropic.com` 默认开启，网关默认关闭 |
| `tool_result_media` | 仅 Anthropic：工具返回的图片和 PDF（`read`、MCP 工具）放在 `tool_result` 里。`api.anthropic.com` 默认开启；网关默认关闭，此时它们放在同一条消息里、紧跟工具结果之后，因为很多网关会丢掉工具结果里的图片 |
| `prompt_cache` | Anthropic 自动提示词缓存；默认开启 |
| `models` | 按模型的覆盖项；列出的模型会显示在 `/model` 和 Tab 补全中 |

`base_url`、`api_key` 和 `headers` 中的 `${VAR}` / `${VAR:-default}` 会从环境变量展开。没有已知 provider 前缀的 spec 视为当前 provider 上的一个模型，因此包含 `/` 的网关模型名（如 `openai/gpt-5`）同样可用。

会话中途可用 `/model <provider>/<model>` 切换模型；单独输入 `/model` 会列出 provider 和模型。`/effort <level>` 修改本次会话剩余部分的推理强度（`/model` 切换后保留），`/effort auto` 恢复为配置值。历史始终以 Anthropic 形式保存，请求 OpenAI 时再转换（`function_call` / `function_call_output`、`input_image`、`input_file`）；请求是无状态的（`store: false`），推理项会以其加密内容重放。绑定模型的块（Anthropic thinking、OpenAI reasoning）只会发回产生它们的那个模型。子代理可以通过 `subagent_model` 或代理自身的 `model` 使用不同的网关。

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

助手的 Markdown 在流式输出时即时渲染（逐行；表格在完整后对齐）。工具调用显示为 `● bash(cargo test)` 并带有结果的前几行，模型的待办清单（`todo_write`）显示为勾选列表（`✔` 已完成、`◼` 进行中、`☐` 待办），前台子代理的调用列在其 `agent` 调用之下，后台子代理结束时显示一行，并有旋转指示器显示正在运行的内容。每轮结束时显示耗时、输出 token 数和上下文大小。当 stdout 不是终端（或设置了 `NO_COLOR`）时，文本按原样输出，没有样式和摘要；一次性模式下错误输出到 stderr，退出码非零。

提示符是一个夹在两条横线之间的输入框，始终停在终端最底部的几行，位于输出和旋转指示器之下，在一轮运行期间也是如此；输出不满一屏时，输出从上往下排，和输入框之间留空（与 Claude Code 一样），菜单也在这片空白里展开。模型工作时输入的内容会进入这个输入框：按 Enter 后，这条消息列在输入框上方，直到模型在下一次请求中拿到它（跟在当时正在运行的工具调用结果之后；如果模型正在写回答，它会继续而不是结束本轮），之后它显示在会话中。`@路径` 提及和 Ctrl+V 在这里同样可用；菜单和历史只在提示符处可用。一轮运行期间发送的 `/命令` 会等本轮结束再执行。想改主意时，在输入框第一行按 ↑（有排队消息时空输入框会给出提示）：模型尚未拿到的消息会回到输入框中、排在你已输入的内容之前，可以修改后重新发送，与 Claude Code 一样。如果中断本轮，模型尚未拿到的消息会回到输入框中；本轮结束后，输入框里剩下的内容仍会保留在提示符中。

| 命令 / 按键 | 说明 |
|---|---|
| `/help` | 列出命令 |
| `/model [spec]` | 显示 provider 和模型，或切换到 `<provider>/<model>` |
| `/effort [level]` | 显示推理强度，或设置为 `low`、`medium`、`high`、`xhigh`、`max`；`auto` 恢复为配置值 |
| `/clear` | 开始新会话 |
| `/compact [重点]` | 总结会话以腾出上下文（见[上下文压缩](#上下文压缩)）；`重点` 指定摘要要特别保留的内容 |
| `/resume [id]` | 继续当前目录下之前的会话（见[恢复会话](#恢复会话)） |
| `/usage` | 按模型显示 REPL 启动以来用掉的 token（请求数、输入、缓存读取与写入、输出；包括子代理和上下文压缩，`/clear` 和 `/resume` 后不清零），以及当前会话大约占用了多少上下文窗口 |
| `/tools` | 列出模型可调用的工具 |
| `/skills` | 列出发现的技能 |
| `/agents` | 列出子代理类型及其权限和工具 |
| `/tasks` | 列出本会话启动的子代理（ID、状态、模型、工具调用次数、耗时）和[后台命令](#后台命令) |
| `/stop [id]` | 停止一个运行中的子代理或后台命令（`shell_1`），或全部停止（见[子代理](#子代理subagents)） |
| `/mcp` | 列出 MCP 服务器、其工具和日志文件 |
| `/sandbox` | 显示沙箱允许的操作（见[沙箱](#沙箱)） |
| `/<skill> [args]` | 直接运行一个技能（一次性模式同样可用：`harness /greet Bob`） |
| `/exit`、`exit`、Ctrl-D | 退出（`exit` 或 `quit` 仅在单独占一行时生效） |
| Esc | 中断运行中的轮次，同 Ctrl-C |
| 一轮运行期间按 ↑ | 把模型尚未拿到的排队消息取回输入框，以便修改 |
| Ctrl-C | 中断运行中的轮次：运行中的命令被终止，并告知模型请求已被中断。在提示符处连按两次退出 |
| 行尾 `\`、Alt+Enter | 在新行继续这条消息 |
| `/` | 在提示符下方打开命令菜单；随输入收窄，↑/↓ 选择，Tab 填入，Enter 运行。`/model ` 之后会列出模型 |
| `@` | 打开文件菜单（见[用 `@` 附加文件](#用--附加文件)） |
| Ctrl+V | 粘贴剪贴板中的图片（见[粘贴图片](#粘贴图片)） |

## 后台命令

`bash` 加上 `run_in_background: true` 会启动一个持续运行的命令（如开发服务器、watcher 或耗时的构建），并立即返回它的 ID（`shell_1`）。`bash_output` 返回它自上次查看以来的输出（stdout 和 stderr 合在一起，最多返回最新的 30000 字节；最多保留 1 MB）以及它是否仍在运行；`kill_shell` 停止它及其启动的进程。自行退出的命令会通知模型，模型空闲时会因此开始新的一轮，与后台子代理的报告相同。后台命令与其他 `bash` 命令一样受沙箱和文件修改规则约束，每个会话最多同时运行 8 个。`/tasks` 列出它们，`/stop shell_1` 停止一个，`/stop` 全部停止（会告知模型）；`/clear`、`/resume`、删除 Web 任务或退出程序都会杀掉它们。

## 上下文压缩

当一次请求之后，模型 `context_window` 剩下的空间不够放一次完整回复（`max_tokens`）再加窗口 5% 的余量时，模型先总结会话，再用摘要替换历史，做法与 Codex 相同。窗口 256k、`max_tokens` 为默认的 64k 时，阈值约为 179k token；窗口 1M 时约为 886k。摘要通过一次普通请求获得（system 提示词和工具不变，缓存前缀仍然命中），请求末尾是一段总结提示：进展和决定、用户的约束、涉及的文件、遇到的错误及修复、剩余工作和下一步。新的历史只有一条消息：用户自己发过的消息（从最新往前，原文保留约 2 万 token），然后是摘要、最新的待办清单，以及最近读过的 5 个文件的当前内容（每个约 5k token 以内，总共不超过窗口的 10%）。assistant 回复、工具结果和附件都会丢弃，只有重新附上的文件仍算作 `edit` 和 `write` 已读过。

压缩发生在一轮开始前（新消息原样保留在摘要之后），也发生在同一轮的工具调用之间，所以长任务会自己继续下去；REPL 会显示 `✻ Compacted the conversation: context 172.4k → about 9.8k`。`/compact [重点]` 可以手动压缩。如果总结请求本身也超长，会去掉最早的几轮对话再试；如果压缩后上下文仍然太满，或同一轮里第三次又被填满，这一轮会报错停止，而不是无限循环。压缩可以像普通一轮一样打断，打断后历史保持原样。保存的会话存的是压缩后的历史，恢复时会在压缩处显示 `✻ Conversation compacted`。

## 恢复会话

REPL 和一次性模式的每个会话都会在每轮结束后保存到 `~/.harness/sessions/<id>.jsonl`（仅所有者可读）：第一行是头部（id、工作目录、第一条提示、模型和时间），之后每行一条消息。`/clear` 会开始一个新文件，旧文件保留。

| | |
|---|---|
| `harness -c` / `--continue` | 继续当前目录最近的会话 |
| `harness -r` / `--resume` | 列出当前目录的会话并选择一个 |
| `harness -r <id>` / `--resume=<id>` | 继续指定会话；id 的唯一前缀即可 |
| `/resume`、`/resume <id>` | 在 REPL 内做同样的事 |

它们可以与一次性模式组合：`harness -c "now run the tests"` 或 `harness -r <id> "..."`（一次性模式下 `--resume` 需要给出 id）。恢复的会话沿用原来的 id；若上次使用的模型仍在配置中，则切换回该模型；在 REPL 中会重新显示最后几轮。推理块会被保留，因此仍会发回给产生它的模型。`edit` 和 `write` 的已读记录不会保存：模型在修改文件前需要重新读取。Web UI 的任务不会保存。

## 沙箱

`harness --sandbox`（或 `HARNESS_SANDBOX=1`，或在配置的 `sandbox` 部分写 `"enabled": true`）让模型的 `bash` 命令和 `grep` 搜索在操作系统沙箱里运行：macOS 用系统自带的 Seatbelt（`sandbox-exec`），Linux 用 [bubblewrap](https://github.com/containers/bubblewrap)（`apt install bubblewrap` / `dnf install bubblewrap`；它需要非特权用户命名空间，有些容器里没有）。`--no-sandbox` 或 `HARNESS_SANDBOX=0` 可以单次关闭。沙箱无法启动时，harness 直接退出，不会在没有沙箱的情况下运行。

- **读**：除 `deny_read` 以外都可读。
- **写**：只能写工作目录、临时目录（`/tmp`、`/var/tmp`、`$TMPDIR`，macOS 上还有每用户的临时和缓存目录）以及 `allow_write`。工作目录中的 `.harness/`、`.claude/`、`.mcp.json`，以及 git 仓库中的 `.git/hooks/` 和 `.git/config` 保持只读，因为它们会让代码之后在沙箱外运行（MCP 服务器、hooks、`core.fsmonitor`）；`.git` 本身不能被重命名或删除。
- **网络**：只允许回环地址（本地服务和测试可用，DNS 和外连会失败），除非设置 `"network": true`。
- 命令在没有终端的新会话中运行。Linux 上还有独立的 PID 命名空间，后台任务随命令一起结束；macOS 上无法运行 setuid 程序（`sudo`、`ps`）。

`read`、`edit` 和 `write` 工具按同样的规则检查。`/sandbox` 显示这些规则，REPL 横幅会显示 `sandboxed`，系统提示词也会告诉模型这些限制，让它报告被拦截的操作，而不是设法绕过。MCP 服务器和 harness 本身不在沙箱中。

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

- 传输方式：`stdio`（设置了 `command` 时的默认值）和可流式的 `http`（设置了 `url` 时的默认值），通过官方 Rust SDK [`rmcp`](https://github.com/modelcontextprotocol/rust-sdk) 实现。
- 每个字符串中的 `${VAR}` / `${VAR:-default}` 都会从环境变量展开。
- 服务器在启动时并发连接；连接失败的服务器会被报告并跳过。
- 工具以 `mcp__<server>__<tool>` 形式暴露。文本、内嵌文本资源和 PNG/JPEG/GIF/WebP 图片会传回给模型。
- stdio 服务器的 stderr 写入 `$TMPDIR/harness-mcp-<server>.log`。

## 项目说明

如果工作目录下有 `AGENTS.md`，系统提示词会告诉模型它的存在，模型需要了解项目约定时自己用 `read` 读取。文件内容不会放进提示词，也不会查找其他文件（`CLAUDE.md`、上级目录或 `~` 下的文件）。

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

主代理可以用 `agent` 工具委派一个自包含的任务。子代理在自己的全新会话中运行，有自己的系统提示和工具集，只有最终报告会返回，从而把漫长的搜索挡在主上下文之外。同一次响应中启动的多个子代理并行运行。

- **`explore`**（内置）：只有 `grep` 工具的代码库搜索代理。用于“X 在哪里 / 哪些文件用了 Y”这类问题。
- **`general-purpose`**（内置）：拥有主代理全部工具（`bash`、`edit`、`write`、MCP 工具等，受同一沙箱约束）的编码代理，用于多步调研或范围明确的修改。子代理共享工作目录，因此主代理会被要求给每个修改文件的子代理分配各自的文件。
- **自定义代理**：`.harness/agents/` 或 `.claude/agents/`（项目或 `~`）中的 Markdown 文件（先找到的优先，与内置代理同名的文件会覆盖内置项）：

  ```markdown
  ---
  name: reviewer
  description: Reviews the current diff for bugs. Use after making changes.
  tools: read, grep          # 可选；`mcp__server__*` 匹配前缀
  permission: read-only      # 默认；`write` 允许 bash/edit/write 和非只读的 MCP 工具
  model: claude-sonnet-5     # 可选
  ---
  You are a code reviewer...
  ```

- **自定义代理默认只读**：只读代理只会获得标记为只读的工具（`read`、`grep`、`todo_write`、`skill`，以及服务器设置了 `readOnlyHint` 的 MCP 工具），即使它的 `tools` 列表写了其他工具也不行。`tools` 用于进一步收窄。
- 子代理不能再启动子代理。它们的文件读取使用自己的追踪器，因此不会计入主代理 `edit` 和 `write` 所需的已读记录。
- 模型：代理的 `model`，否则 `subagent_model` / `HARNESS_SUBAGENT_MODEL`，否则会话当前使用的模型（包括 `/model`、`/effort` 切换之后）；任意 `<provider>/<model>`。
- `/agents` 列出可用类型。

### 后台子代理与后续追问

- **前台**（默认）：`agent` 调用会等待报告。中断本轮会停止该子代理。
- **后台**（`run_in_background: true`）：调用立即返回子代理的 `agent_id`，主代理继续工作。子代理完成后，报告随主代理的下一次请求送达：在一轮之中，就跟在当时正在运行的工具调用结果之后（如果模型正在写回答，它会继续而不是结束本轮）；本轮结束之后，则单独开始一轮。REPL 在提示符为空时开始这一轮；已经输入的内容不会丢失，报告改为随你的这条消息一起送达。一次性模式会等后台子代理结束再退出，Web 任务在空闲时开始这一轮。中断本轮时后台子代理继续运行。
- **后续追问**：每个子代理都保留自己的会话。`message_agent` 让已完成或已停止的子代理带着原有上下文处理一条新消息（等待其报告，或在后台运行）；仍在运行的子代理会在下一步收到这条消息。`stop_agent` 停止子代理，它已做的修改会保留。
- `/tasks` 列出本会话的子代理；`/stop <id>` 停止一个，`/stop` 停止所有运行中的。你停止的子代理会随你的下一条消息告知主代理。Web UI 中每张子代理卡片都有停止按钮。
- 每个会话最多同时运行 8 个子代理，保留最近的 32 个用于后续追问。它们只存在于内存中：`/clear`、`/resume`、删除 Web 任务或退出程序都会停止并遗忘它们。

## 目录结构

- `src/main.rs` — 启动：配置、工具、技能、MCP 服务器、代理；然后进入 CLI 或 web 前端
- `src/settings.rs` — 配置文件：位置、`--settings`、解析为各个部分
- `src/cli/` — 终端前端：REPL 和一次性模式（`mod.rs`）、旋转指示器、输入框与轮次显示（`output.rs`）、一轮运行期间的输入框：其文本、渲染和排队的消息（`input.rs`）、一轮运行期间读取的按键：Esc 和编辑输入框（`keys.rs`）、流式 Markdown 渲染（`markdown.rs`）、显示宽度辅助（`text.rs`）、提示符下方的菜单：命令、项目文件搜索（`menu.rs`）、带输入框横线、菜单按键、补全、多行输入、Ctrl+V 粘贴以及为子代理报告唤醒提示符的 rustyline 辅助（`repl.rs`）、从剪贴板读取图片或复制的文件（`clipboard.rs`）
- `src/web/` — web 前端（`--web`）：axum 服务器与 JSON/SSE API（`mod.rs`）、后台任务及其事件日志（`tasks.rs`）、单页 UI（`index.html`，内嵌于二进制）
- `src/agent.rs` — 代理循环：请求 → 并发运行工具 → 发送结果 → 循环直到 `end_turn`；收件箱，把期间到达的内容（如后台子代理的报告）交给模型
- `src/agent/usage.rs` — `/usage` 显示的按模型统计的 token 数，与子代理共享
- `src/agent/compact.rs` — 上下文压缩：何时压缩、总结请求、替换会话的新历史
- `src/llm/` — LLM 网关
  - `config.rs` — 由配置和环境变量得到 provider、模型 spec 解析、按 provider/模型的设置
  - `anthropic.rs` — Messages API 后端（SSE 转内容块、thinking、提示词缓存、拒答回退）
  - `openai.rs` — Responses API 后端（历史 ⇄ 输入/输出项转换、加密推理）
  - `http.rs` — 带重试的 POST、SSE 读取器；标记哪些流式失败（连接断开、过载）之后 `llm::Client` 会重新请求回复
- `src/session.rs` — 会话保存：会话文件、`--continue` / `--resume` 参数
- `src/mentions.rs` — `@路径` 的解析与展开为附件块；粘贴图片的附件块
- `src/sandbox.rs` — 操作系统沙箱：配置、Seatbelt 配置文件与 bwrap 参数、`read`、`edit` 和 `write` 检查的路径规则
- `src/subagent.rs` — 子代理定义（`explore`、`general-purpose`、自定义文件）、权限、`agent`、`message_agent` 和 `stop_agent` 工具
- `src/subagent/runtime.rs` — 一个会话的子代理：前台与后台运行、后续追问、停止、向主代理收件箱报告
- `src/mcp.rs` — MCP 客户端：服务器配置、连接、`mcp__*` 工具适配器
- `src/skills.rs` — 技能发现、`SKILL.md` frontmatter 解析、`skill` 工具
- `src/tools/` — `Tool` trait、注册表和 `FileTracker`（共享的读/改/写状态）
  - `bash.rs` — 通过 `bash -c` 运行命令（超时、输出截断）；拒绝自行修改文件的命令（写文件的内联 Python/Node/Perl 脚本、`sed -i`、写入文件的 heredoc 或 `echo`），这些修改应走 `edit` / `write`
  - `read.rs` — 读取文本文件并带行号（`offset` / `limit` 分页），或读取图片、PDF 供模型查看
  - `edit.rs` — 把文件中一段精确且唯一的文本替换掉（`replace_all` 则替换全部出现），修改的开销只取决于改动本身；与 `write` 相同的先读规则，同一次回复中对同一文件的多个修改按顺序生效
  - `write.rs` — 创建或覆盖文件；已存在的文件必须先被读取且此后未被修改
  - `shells.rs` — 后台命令（`bash` 的 `run_in_background`）：每个会话各自的一组，`bash_output` 和 `kill_shell` 工具，退出时通知收件箱
  - `todo.rs` — `todo_write` 工具：模型的待办清单，每次整体发送
  - `grep.rs` — 通过 ugrep 的正则搜索（PCRE，遵循 `.gitignore`，`files_with_matches` / `content` / `count` 模式）

在官方 Anthropic API 上，工具以 `eager_input_streaming` 声明，因此较大的输入（例如 `write` 的文件内容）会在生成时流式传输。此后 API 不再校验工具输入，所以每个内置工具都把输入解析成严格的结构体（拒绝未知字段），流式 JSON 解析失败的调用不会执行：模型会得到 `INVALID_JSON` 错误结果。MCP 工具保持输入缓冲开启，因此 API 会按服务器提供的 schema 校验其输入。

## 添加工具

实现 `tools::Tool`（name、description、JSON schema、异步 `run`），并在 `ToolRegistry::with_defaults` 中注册。
