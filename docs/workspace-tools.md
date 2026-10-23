# 工作区只读工具

Tarz 可以为新会话显式启用 `list_directory`、`read_file` 和 `search_text`。CLI/TUI 共用 core 的配置、实际工具和资源监督；模型往返仍由 Causa runner 执行。默认工具清单为空，仅设置工作区不会启用工具。

```sh
tarz run --provider openai-chat --model "$MODEL_ID" \
  --workspace . --tools list_directory,read_file,search_text \
  --input '找出保存失败后的恢复路径，给出文件和行号'
tarz session create --store ./data --provider openai-chat --model "$MODEL_ID" \
  --workspace . --tools list_directory,read_file,search_text --json
```

同样的 `--workspace`、`--tools` 可作为 `tarz-tui` 新建默认值。TUI 表单提供 Workspace 和 Tools 字段；`--tools ''` 或清空表单 Tools 字段禁用预设工具。保存的会话固定工作区、选择及完整限额；重开采用原定义，不随配置或 cwd 改变。缺失或不再满足访问规则的根目录会使打开失败，不会退回当前目录，也不发模型请求。

## 文件访问

首次解析配置时把所选路径解析为物理绝对目录，TOML 相对路径以配置文件目录为基准，CLI 相对路径以解析时 cwd 为基准。打开实例从 `/` 逐段以目录句柄和 NOFOLLOW 打开保存路径，之后每段均相对已打开目录访问。Linux/macOS 提供该实现；本轮真实文件和终端证据来自 macOS，Linux 仍需独立平台验证。其他平台启用工具时明确拒绝，无工具会话仍可使用。

工具只接受非空相对路径，`.` 表示根目录；拒绝绝对路径、任何 `..`、反斜杠、NUL、超过 4096 bytes 的路径、符号链接、不同设备及特殊文件。文件以 NONBLOCK 打开后检查普通文件类型，避免被替换为 FIFO 后卡在打开阶段。目录列表可以呈现链接条目，但不跟随它。搜索跳过链接/特殊文件并报告不完整。

实例绑定根目录句柄，路径被替换不会重定向当前实例；已打开子目录被移动后仍代表原资源。此机制不隔离同设备 bind mount 或硬链接，也不是进程 OS 沙箱。结果是某次读取的观察，不是跨文件一致快照。文件可在后续调用前变化，Store 保存已提交材料，不备份工作区。

## 输入与结果

| 工具 | 输入 | 输出 |
| --- | --- | --- |
| `list_directory` | `path`，可选 `limit` | 单层 `entries`（名称/类型）、`truncated`、`limit_reason`；枚举顺序由文件系统决定 |
| `read_file` | `path`，可选一基 `start_line`、`max_lines`（默认 200，最多 2000） | 请求起始行、实际 `end_line`（无返回行时为 null）、文本、扫描字节、截断原因 |
| `search_text` | 目录 `path`、非空单行字面量 `query`（最多 1024 bytes，拒绝 CR/LF/NUL），可选 `max_matches` | 相对文件路径、一基行号、匹配附近片段、扫描文件/条目/字节数、跳过数量、最多 32 条问题及 `complete` |

参数拒绝未知字段、错误类型、重复/禁用工具及越限值。schema 供模型参考，实际调用重新校验。非法参数与禁止访问为 Rejected；合法路径的不存在、普通 I/O、NUL 二进制或非法 UTF-8 为 Failed。到达扫描/输出上限可以是 Succeeded，但结果明确标记截断或不完整；不能据此断言未扫描区域没有匹配。搜索只处理 UTF-8 文本，不运行 shell 或正则程序。

| 有效限制 | 默认及硬上限 |
| --- | --- |
| 每会话 / 全 host 实际文件操作 | 4 / 16 |
| 单调用期限（包含等待许可） | 10 秒，同时受工作剩余期限限制 |
| 完整序列化工具结果 | 64 KiB |
| 目录条目 / 搜索条目 | 200 / 4000 |
| 单文件扫描 / 搜索总扫描 | 2 MiB / 8 MiB |
| 搜索文件 / 目录深度 / 匹配 | 1000 / 32 / 200 |

预设可降低有效限制（结果最小 1024 bytes），工具参数只能进一步降低。跳过前文、长行和 JSON 转义都计入限制；扫描与输出在构造过程中限量，不先收集全部结果再裁剪。搜索采用深度遍历，目录句柄随深度有界。

Causa executor 以同一保存期限作为调用等待的超时兜底；期限和取消也在排队及文件操作检查点生效；丢弃工具 future 会请求对应操作停止。每个工作的 blocking 任务由 TaskTracker 监督，许可保留至真实闭包退出，工作终态和关闭都等待收尾。该机制不能强制抢占卡住的 OS I/O，不保证网络或异常文件系统上的有限时退出。

## 观察、保存与旧记录

TUI 区分 Waiting、Running、Returned（待材料提交）、Committed，以及工作终态和保存状态。观察丢失会提示，并用可靠终态摘要校正。每工作最多保留 128 个调用摘要，名称/参数摘要分别最多 128/256 bytes；取消后保留 Causa 实际返回的 UnknownOutcome；真实文件任务全部退出后，应用再区分已启动但无已接受结果与等待许可期间未执行。此投影不修改原批次或已提交结果，已提交的未知结果仍保留材料提交事实。摘要不消费原未提交 ToolBatch，不构成持久工作账本。`/tools` 或 F6 从实际材料缓存展开完整调用和结果；终端控制字符仅在展示时处理。

CLI 执行 stdout 仍只输出最终文本或既有 JSON 报告；工具材料可通过保存/导出查看。保存失败重试原材料，不重跑工具或模型。重开可显示原已提交调用/结果，不恢复或执行旧工作。取消可能只留下已提交声明，结果仍在原未提交批次中；保存不会补造配对结果。新输入遇到未配对材料会被原样返还。TUI F7 / `/repair` 可预览采纳原结果或移除未配对声明，CLI `session repair` 可显式移除；应用并保存后再发送新工作。UnknownOutcome 和原工作状态不变，完整条件见[中断后继续](session-continuation.md)。

记录格式为 `tarz-session-v2`，要求完整权限和限额字段。v1 明确拒绝，不自动补权限或改写旧文件。可离线提取经过完整 Context 校验的材料，再显式创建采用新定义的会话：

```sh
tarz session export-materials --record ./old-record.json --output ./materials-new.json
tarz session create --store ./data --provider openai-chat --model "$MODEL_ID" \
  --load ./materials-new.json --json
```

导出最多读取 64 MiB，目标必须不存在；不读取凭据或调用模型，不复用旧定义。新建时如需工具，仍须显式选择工具和工作区。MCP 工具通过 [mcp.json 配置](configuration.md)接入，命令执行和父子 Agent 不在当前范围；CLI 与 TUI 可以写入文件：`ask` 模式在终端/确认面板确认，非终端时拒绝；`accept-edits` 和 `full-access` 不询问；`tarz-tui` 已接通写入确认面板、`/mode` 命令和 `Shift-Tab` 快捷键。
