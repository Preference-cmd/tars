# 共享 TOML 配置

CLI 与 TUI 共用 `tarz-core::config`。配置在进程启动时读取为不可变默认值，用于新会话；已保存会话重开时仍采用保存的 `SessionDefinition`。修改 TOML 不热改活动会话，也不重写已有记录。

配置文件选择顺序：

1. `--config FILE`。
2. 环境变量 `TARZ_CONFIG`。
3. Unix/macOS：绝对路径 `$XDG_CONFIG_HOME/tarz/config.toml`，未设置时为 `~/.config/tarz/config.toml`；Windows：`%APPDATA%/Tarz/config.toml`。

不会自动搜索当前项目的配置文件。默认路径不存在时使用空配置；显式路径、`TARZ_CONFIG` 指向的文件不存在或配置无效时拒绝启动。`--no-config` 忽略文件及环境选择，与 `--config` 互斥。配置文件的读取、解析、校验和预设解析错误返回退出码 1；未知参数、缺少参数值等参数错误返回 2。帮助和离线 demo 不读取配置。

参照根目录的 [config.example.toml](../config.example.toml)：

```toml
version = 1
store = "data"
recovery_dir = "."
default_profile = "work"

[profiles.work]
provider = "openai-chat"
model = "your-model-id"
api_key_env = "OPENAI_API_KEY"
# endpoint = "http://127.0.0.1:8080/v1/chat/completions"
timeout_secs = 120
capacity = 256
max_model_rounds = 10
max_tool_calls = 64
# max_tokens = 8192
# reasoning_effort = "medium"
# max_model_attempts = 3
# permission_mode = "ask"      # read-only | ask | accept-edits | full-access
# mcp = ["git"]                # ids defined in mcp.json; omitted or empty selects none
# [profiles.work.extra_body.thinking]
# type = "adaptive"
# [profiles.work.render_options]
# max_tokens_field = "max_tokens"
# [profiles.work.write_limits]
# max_edit_file_bytes = 1048576
# max_write_bytes = 524288
# max_edit_segments = 16
```

`version` 必须为 1；`store`、`recovery_dir`、`default_profile` 和 `profiles` 可省略。每个模型预设必须包含 provider 与非空 model。provider 为 `openai-chat`、`openai-responses` 或 `anthropic`。其余字段省略时采用表中默认：期限 120 秒、保留工作 256 项、每次工作最多 10 次模型调用和 64 个工具声明；`api_key_env` 随协议默认为 OPENAI_API_KEY 或 ANTHROPIC_API_KEY。`max_tokens`（每次模型调用的输出 token 上限）和 `reasoning_effort` 没有默认值，省略即不发送；`anthropic` 协议要求 `max_tokens`，未配置时请求在发出前被拒绝。对应的命令行参数是 `--max-tokens` 与 `--reasoning-effort`。`reasoning_effort` 是一段文本，原样写入请求里该协议的强度字段，Tarz 不维护可用值列表：可用值因模型和服务而异（常见的有 `low`、`medium`、`high`，有的服务用 `off` 关闭推理），服务不接受时返回的错误会原样显示。`extra_body` 是一个表，原样并入请求体，用于只有某个协议或模型才有的设置，例如 `anthropic` 协议的 `thinking` 对象。与 Tarz 自己写入的键重名时，两边都是表则逐键合并，例如在 `openai-responses` 协议上写 `reasoning = { summary = "auto" }` 会与 `reasoning_effort` 写入的强度并存；其他情况下保留 Tarz 写入的值（如 `model`、消息、工具），表里的值被忽略且不报错。命令行参数是 `--extra-body`，值为一个 JSON 对象，给出时整体替换预设里的表。在 `anthropic` 协议上，只设置 `reasoning_effort` 不会自动附带 `thinking` 对象，需要时在 `extra_body` 里写明。`max_model_attempts` 是每轮模型调用在瞬时故障或限流时允许的总次数（含第一次），省略或为 1 时不重试，命令行参数是 `--max-model-attempts`；重试只发生在任何输出可见之前，等待时间不超过工作期限。已保存会话的记录里没有这几项时按未配置读取。

`permission_mode` 是新会话的初始权限模式，省略为 `ask`；可选 `read-only`、`ask`、`accept-edits`、`full-access`。命令行对应 `--permission-mode`，TUI 新建表单也有该字段。运行中切换、四种模式的含义和保存规则见[工作区工具](workspace-tools.md#确认与权限模式)。

`write_limits` 只允许在支持的上限内调低（默认见[工作区工具](workspace-tools.md#限额)），字段为 `max_edit_file_bytes`、`max_write_bytes` 和 `max_edit_segments`；命令行没有对应参数，未写字段采用默认。`tool_limits` 和 `write_limits` 都不能提高硬上限。

`render_options` 是一个表，设置所选协议在 Causa 里的渲染选项。例如 `openai-chat` 默认把输出上限写成 `max_completion_tokens`；服务只认 `max_tokens` 时写 `max_tokens_field = "max_tokens"`，请求里就只有 `max_tokens`。可用的键和取值以所用 Causa 版本中该协议的渲染选项类型为准（`ChatRenderOptions`、`ResponsesRenderOptions`、`AnthropicRenderOptions`），Tarz 不另列清单，也不按服务自动选择；没写的键用 Causa 的默认值。未知的键、无效的取值和不属于所选协议的键在加载配置或建会话时报错，不发请求，错误信息不显示表的内容。所以用 `--provider` 换协议时，预设里的表也必须适用于新协议。命令行参数是 `--render-options`，值为一个 JSON 对象，给出时整体替换预设里的表。会话记录保存写下的键值，没有这一项时按空表读取。

临时 `tarz run` 只保留其单次工作，容量固定为 1。

所有预设在加载时校验，未知字段、重复键、无效类型、无效预设、空路径和不支持的版本都报错。文件最多 1 MiB。解析诊断只显示行列，不回显 TOML 值；`api_key` 明文字段不被接受。真正凭据在打开会话时从指定环境变量读取。

路径不展开 `~` 或 `$VAR`。相对 `store` 和 `recovery_dir` 以 TOML 所在目录为基准；命令行相对路径以当前工作目录为基准。store 会按现有 Store 规则创建，recovery_dir 必须已存在。没有配置 store 时，持久 CLI 与 TUI 都需要显式 `--store`。

```sh
tarz run --config ./config.toml --profile work --input '分析当前问题'
tarz session create --config ./config.toml --model another-model --json
tarz session list --config ./config.toml --json
tarz-tui --config ./config.toml --profile work
```

选择 `--profile NAME` 优先于 default_profile。参数按字段覆盖所选预设；没有覆盖的 endpoint、凭据引用、期限和限制仍沿用预设。CLI 模型参数用于 `run` 与 `session create`；`session run` 不接受 `--profile` 或模型覆盖。`--store`、`--recovery-dir` 覆盖配置中的相应路径。

TUI 新建表单从所选预设填充，修改只影响本次新会话，预设中未显示的容量/调用限制、生成设置和 `render_options` 原样保留。首版配置由用户编辑文件，TUI 不自动保存或覆盖全局配置。后续 GUI 应复用相同加载与解析模块。

## MCP 服务器

MCP 服务器定义在独立的 `mcp.json`，与 `config.toml` 分开：常见的 `mcpServers` JSON 片段可以直接粘贴，`config.toml` 只保存预设选择。完整示例见 [mcp.example.json](../mcp.example.json)。

文件位置：

- 默认是 `config.toml` 同目录下的 `mcp.json`；`TARZ_CONFIG` 指向其他文件时，取那个文件的同目录。
- `TARZ_MCP_CONFIG` 可以单独指定；设置后该文件必须存在，相对路径以当前目录为基准。
- `--no-config` 隔离调用不读 `mcp.json`，也不读 `TARZ_MCP_CONFIG`。
- 默认文件不存在等于没有服务器；其他读取、解析或校验错误使启动失败。

```json
{
  "mcpServers": {
    "git": {
      "command": "uvx",
      "args": ["mcp-server-git", "--repository", "."],
      "env": { "LOG_LEVEL": "warn", "GIT_AUTHOR_NAME": "${GIT_AUTHOR_NAME}" },
      "startupTimeoutSec": 10,
      "toolTimeoutSec": 60,
      "enabledTools": ["git_status", "git_diff"],
      "approval": "prompt",
      "required": false,
      "tools": { "git_status": { "approval": "approve" } }
    },
    "docs": {
      "type": "http",
      "url": "https://example.invalid/mcp",
      "headers": { "Authorization": "Bearer ${DOCS_MCP_TOKEN}" },
      "approval": "writes"
    }
  }
}
```

顶层只允许 `mcpServers`，其值是以服务器 id 为键的对象。id 只允许小写字母、数字和 `-`，最长 16 个字符；每个会话最多选择 8 个服务器。每个条目必须有 `command` 或 `url` 之一：

- `command`、`args`、`env` 用于 stdio 服务器；`headers` 不适用于 stdio。
- `url`、`headers` 用于 HTTP 服务器；`args`、`env` 不适用于 HTTP。
- `type` 可省略，按 `command`/`url` 推断；显式写时只接受 `stdio`、`http`、`streamable-http`。`sse` 不支持。

| 字段 | 默认 | 说明 |
| --- | --- | --- |
| `startupTimeoutSec` | 10 | 启动期限，1–60 秒 |
| `toolTimeoutSec` | 60 | 单次调用期限，1–3600 秒 |
| `enabledTools` / `disabledTools` | 空 | 只允许或排除列出的工具；两者不能同时出现，名字必须非空且唯一 |
| `approval` | `prompt` | `prompt` 每次都问；`writes` 把服务器声明只读且未声明破坏性的工具视为只读，其余询问；`approve` 不问 |
| `tools.<name>.approval` | 继承服务器 | 按工具覆盖 `approval`，目前只能写这一个键 |
| `required` | `false` | 为 `true` 时启动失败使会话打开失败，不发模型请求 |

模型看到的工具名是 `mcp_{id}_{tool}`。超过 64 bytes、含不合法字符或与内置工具重名的工具会被跳过并在界面报告，不让整个服务器失败。服务器发出 `tools/list_changed` 后新增的工具仍受同样过滤和确认设置约束。

### 变量与密钥

`args` 的每个元素、`env` 的值、`url` 和 `headers` 的值可以写 `${VAR}` 或 `${VAR:-默认值}`，在打开会话时从 Tarz 进程环境展开。`command` 不支持变量。`${VAR}` 未定义且没有默认值时，该服务器启动失败，原因包含变量名而不包含值；`required = true` 时整个打开失败。会话快照保存未展开的写法，展开后的值不进入记录、列表或导出。

为减少“照文档粘贴就把 token 写进记录”的风险：

- `Authorization` 头的值必须是 `<方案> ${VAR}`（如 `Bearer ${DOCS_MCP_TOKEN}`），变量不能写默认值。名字按 `-` 分段后含 `authorization` 的头同样处理。
- 名字按 `-` 分段后含 `TOKEN`、`KEY`、`APIKEY`、`SECRET`、`PASSWORD`、`PASSWD`、`CREDENTIAL` 或 `CREDENTIALS` 的请求头，值必须是单个 `${VAR}` 且不带默认值。
- 名字按 `_` 分段后含上述词的 `env` 变量，规则相同。
- 这是按名字的启发式检查：`args` 里的字面密钥查不到，仍应使用 `${VAR}`。

HTTP 请求头不能覆盖传输层自己写的 `Accept`、`Content-Type`、`Last-Event-Id` 和 `Mcp-*`，也不能重复；非法名字或值在加载时报错。URL 里的变量展开后必须是 URL 安全字符，Tarz 不做百分号编码；需要编码的内容放进请求头。

别家配置里常见的 `disabled`、`autoApprove`、`alwaysAllow`、`trust`、`description` 会被忽略，并在加载时提示一次；它们不会放宽 Tarz 的 `approval` 或服务器选择。`cwd`、`timeout` 这类会改变行为的未知键报错（`timeout` 提示改用两个超时字段），其他未知键也报错。`config.toml` 里出现 `[mcp_servers.*]` 时加载失败，提示移到 `mcp.json`。错误信息不回显 `env`、请求头或 URL 的值。

### 选择、生命周期与边界

预设用 `mcp = ["git"]` 选择已定义的 id；CLI `run` 和 `session create` 用 `--mcp git,docs` 覆盖预设，空串表示不启用；TUI 新建表单有 MCP 字段。默认一个都不启用。所选服务器的配置快照随新会话定义保存，之后改 `mcp.json` 不影响已有会话；重开时按快照重新连接，因此轮换后的变量值会生效。

打开会话时启动所选服务器，关闭时关闭连接并等待 stdio 子进程结束；`session create` 和 `session repair` 只保存定义或材料，不启动服务器。启动失败或启动后列出工具失败、超时时，该服务器的工具缺席，界面显示原因，其他工具照常可用。stdio 子进程只拿到 `PATH`、`HOME`、`LANG` 和 `env` 列出的变量，不继承 Tarz 的全部环境，避免模型凭据泄给第三方服务器。工作目录默认是会话工作区根目录；没有配置工作区时继承 Tarz 进程的当前目录。

**MCP 服务器以用户权限运行，Tarz 的工作区边界不约束它。** 服务器能访问的用户文件、网络和进程，它都可能访问；`read-only` 模式只能阻止调用，不能把它关进沙箱。`writes` 档采信服务器的只读声明；服务器把会写入的工具声明为只读时 Tarz 无法识别，这是选择该档的代价。

单次结果序列化后最多 64 KiB；超出时把内容转成文本，保留首尾、截掉中间并标明省略字节数。工具调用超时按 Causa 的 `UnknownOutcome` 处理，可能已经在服务器侧产生效果。`approval` 与权限模式的组合见[工作区工具](workspace-tools.md#确认与权限模式)。

## 工作区与内置工具

预设可增加 `workspace_root = "../project"` 和 `tools`。内置工具为 `list_directory`、`read_file`、`search_text`、`write_file`、`edit_file`、`run_command`；默认 tools 为空，工具启用时必须有根目录。`run_command` 还需可选的 `[profiles.NAME.command]`：`network`（`none`/`open`，默认 `none`）、`default_timeout_secs`（默认 120）和 `max_timeout_secs`（默认 600，硬上限 600）；命令行对应 `--command-network`，其他两项来自预设。CLI `run`、`session create` 接受 `--workspace DIR --tools NAME,NAME`；显式 `--tools ''` 清空预设。TUI 新建表单从预设填入 Workspace、Tools、初始权限模式和 MCP 选择，修改只影响本次新会话。重开仍采用原定义，不接受工具覆盖。

相对工作区同样使用配置目录/CLI cwd 基准，但在首次解析时 canonicalize 为物理绝对目录并验证存在；保存后重开不再解析别名。未知/重复工具、缺根或超限拒绝。`[profiles.NAME.tool_limits]` 可以逐项降低 `max_concurrent_operations`、`call_timeout_secs`、`max_output_bytes`、`max_directory_entries`、`max_file_scan_bytes`、`max_search_files`、`max_search_depth`、`max_search_scan_bytes` 和 `max_search_matches`；`[profiles.NAME.write_limits]` 控制 `max_edit_file_bytes`、`max_write_bytes` 和 `max_edit_segments`。未写字段采用默认，持久定义保存完整有效值。数值、路径和确认边界见[工作区工具](workspace-tools.md)。
