# 共享 TOML 配置

CLI 与 TUI 共用 `tarz-core::config`。配置在进程启动时读取为不可变默认值，用于新会话；已保存会话重开时仍采用保存的 `SessionDefinition`。修改 TOML 不热改活动会话，也不重写已有记录。

配置文件选择顺序：

1. `--config FILE`。
2. 环境变量 `TARZ_CONFIG`。
3. Unix/macOS：绝对路径 `$XDG_CONFIG_HOME/tarz/config.toml`，未设置时为 `~/.config/tarz/config.toml`；Windows：`%APPDATA%/Tarz/config.toml`。

不会自动搜索当前项目的配置文件。默认路径不存在时使用空配置；显式路径、`TARZ_CONFIG` 指向的文件不存在或配置无效时拒绝启动。`--no-config` 忽略文件及环境选择，与 `--config` 互斥。配置加载/参数错误返回退出码 2，帮助和离线 demo 不读取配置。

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
# [profiles.work.extra_body.thinking]
# type = "adaptive"
# [profiles.work.render_options]
# max_tokens_field = "max_tokens"
```

`version` 必须为 1；`store`、`recovery_dir`、`default_profile` 和 `profiles` 可省略。每个模型预设必须包含 provider 与非空 model。provider 为 `openai-chat`、`openai-responses` 或 `anthropic`。其余字段省略时采用表中默认：期限 120 秒、保留工作 256 项、每次工作最多 10 次模型调用和 64 个工具声明；`api_key_env` 随协议默认为 OPENAI_API_KEY 或 ANTHROPIC_API_KEY。`max_tokens`（每次模型调用的输出 token 上限）和 `reasoning_effort` 没有默认值，省略即不发送；`anthropic` 协议要求 `max_tokens`，未配置时请求在发出前被拒绝。对应的命令行参数是 `--max-tokens` 与 `--reasoning-effort`。`reasoning_effort` 是一段文本，原样写入请求里该协议的强度字段，Tarz 不维护可用值列表：可用值因模型和服务而异（常见的有 `low`、`medium`、`high`，有的服务用 `off` 关闭推理），服务不接受时返回的错误会原样显示。`extra_body` 是一个表，原样并入请求体，用于只有某个协议或模型才有的设置，例如 `anthropic` 协议的 `thinking` 对象。与 Tarz 自己写入的键重名时，两边都是表则逐键合并，例如在 `openai-responses` 协议上写 `reasoning = { summary = "auto" }` 会与 `reasoning_effort` 写入的强度并存；其他情况下保留 Tarz 写入的值（如 `model`、消息、工具），表里的值被忽略且不报错。命令行参数是 `--extra-body`，值为一个 JSON 对象，给出时整体替换预设里的表。在 `anthropic` 协议上，只设置 `reasoning_effort` 不会自动附带 `thinking` 对象，需要时在 `extra_body` 里写明。`max_model_attempts` 是每轮模型调用在瞬时故障或限流时允许的总次数（含第一次），省略或为 1 时不重试，命令行参数是 `--max-model-attempts`；重试只发生在任何输出可见之前，等待时间不超过工作期限。已保存会话的记录里没有这几项时按未配置读取。

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

MCP 服务器通过与配置文件同目录的 `mcp.json`（或 `TARZ_MCP_CONFIG` 环境变量）定义，支持 `${VAR}` 展开与字面密钥检查；预设用 `mcp = [...]` 选择已定义的 id；CLI `run` 和 `session create` 的 `--mcp ID,ID` 覆盖预设，空串表示不启用，TUI 新建表单有对应字段。所选服务器的配置快照随新会话定义保存，重开使用快照；会话打开时按快照连接（`session create` 与 `session repair` 不启动服务器），MCP 调用目前只在 `full-access` 模式下执行。

## 工作区与只读工具

预设可增加 `workspace_root = "../project"` 和 `tools = ["list_directory", "read_file", "search_text"]`。默认 tools 为空；工具启用时必须有根目录。CLI `run`、`session create` 和 TUI 启动选项接受 `--workspace DIR --tools NAME,NAME`；显式 `--tools ''` 清空预设。TUI 新建表单可修改 Workspace、Tools。重开仍采用原定义，不接受工具覆盖。

相对工作区同样使用配置目录/CLI cwd 基准，但在首次解析时 canonicalize 为物理绝对目录并验证存在；保存后重开不再解析别名。未知/重复工具、缺根或超限拒绝。`[profiles.NAME.tool_limits]` 可以逐项降低 `max_concurrent_operations`、`call_timeout_secs`、`max_output_bytes`、`max_directory_entries`、`max_file_scan_bytes`、`max_search_files`、`max_search_depth`、`max_search_scan_bytes` 和 `max_search_matches`；未写字段采用默认，持久定义保存完整有效值。数值和访问/取消边界见[工作区工具](workspace-tools.md)。
