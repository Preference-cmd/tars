# 工作区工具

Tarz 可以为新会话显式启用六个内置工具：只读的 `list_directory`、`read_file`、`search_text`，写入的 `write_file`、`edit_file`，以及在 macOS Seatbelt 沙箱内执行命令的 `run_command`。CLI/TUI 共用 core 的配置、实际工具和资源监督；模型往返仍由 Causa runner 执行。默认工具清单为空，仅设置工作区不会启用工具。

```sh
tarz run --provider openai-chat --model "$MODEL_ID" \
  --workspace . --tools list_directory,read_file,search_text \
  --input '找出保存失败后的恢复路径，给出文件和行号'
tarz session create --store ./data --provider openai-chat --model "$MODEL_ID" \
  --workspace . --tools list_directory,read_file,search_text,write_file,edit_file,run_command \
  --command-network none --permission-mode ask --json
```

共享 TOML 预设中的 `workspace_root`、`tools`、`permission_mode` 和 `mcp` 会填入 `tarz-tui` 新建表单；TUI 不提供这些启动参数。表单提供 Workspace、Tools、权限模式和 MCP 字段；`--tools ''` 或清空表单 Tools 字段禁用预设工具。保存的会话固定工作区、选择及完整限额；重开采用原定义，不随配置或 cwd 改变。缺失或不再满足访问规则的根目录会使打开失败，不会退回当前目录，也不发模型请求。

`run_command` 每次在沙箱内启动 `/bin/bash -c`，默认无网络、工作区可写、`.git` 只读、用户目录大多不可读；确认面板显示完整命令、工作目录、期限和网络策略。权限类别与四种模式的关系与写入工具相同但始终要求确认（`full-access` 除外）；Linux/Windows 暂不支持该工具。完整命令契约、输出上限和残余见[命令工具提案](../.agents/pipelines/proposals/feature/2026-10-27-sandboxed-shell-command.md)。

## 文件访问

首次解析配置时把所选路径解析为物理绝对目录，TOML 相对路径以配置文件目录为基准，CLI 相对路径以解析时 cwd 为基准。打开实例从 `/` 逐段以目录句柄和 NOFOLLOW 打开保存路径，之后每段均相对已打开目录访问。Linux/macOS 提供该实现；本轮真实文件和终端证据来自 macOS，Linux 仍需独立平台验证。其他平台启用工具时明确拒绝，无工具会话仍可使用。

工具只接受非空相对路径，`.` 表示根目录；拒绝绝对路径、任何 `..`、反斜杠、NUL、超过 4096 bytes 的路径、符号链接、不同设备及特殊文件。文件以 NONBLOCK 打开后检查普通文件类型，避免被替换为 FIFO 后卡在打开阶段。目录列表可以呈现链接条目，但不跟随它。搜索跳过链接/特殊文件并报告不完整。

实例绑定根目录句柄，路径被替换不会重定向当前实例；已打开子目录被移动后仍代表原资源。此机制不隔离同设备 bind mount 或硬链接，也不是进程 OS 沙箱。结果是某次读取的观察，不是跨文件一致快照。文件可在后续调用前变化，Store 保存已提交材料，不备份工作区。

写入与读取使用同一路径规则。`write_file` 可以在已有目录内新建文件，缺失的中间目录逐段以目录句柄创建；目标已存在时按“先读后改”检查。写入先在同一目录创建临时文件、同步，再用 `renameat` 覆盖；新建目标使用不覆盖语义，防止并发出现的文件被静默替换。已有文件的权限位尽量保留，读者只会看到旧内容或新内容。已有文件为 CRLF 时保持其换行风格。

## 输入与结果

| 工具 | 输入 | 输出 |
| --- | --- | --- |
| `list_directory` | `path`，可选 `limit` | 单层 `entries`（名称/类型）、`truncated`、`limit_reason`；枚举顺序由文件系统决定 |
| `read_file` | `path`，可选一基 `start_line`、`max_lines`（默认 200，最多 2000） | 请求起始行、实际 `end_line`（无返回行时为 null）、文本、扫描字节、截断原因 |
| `search_text` | 目录 `path`、非空单行字面量 `query`（最多 1024 bytes，拒绝 CR/LF/NUL），可选 `max_matches` | 相对文件路径、一基行号、匹配附近片段、扫描文件/条目/字节数、跳过数量、最多 32 条问题及 `complete` |
| `write_file` | `path`、`content` | `path`、`action`（`created` / `updated`）、`bytes_written`、`lines_added`、`lines_removed` |
| `edit_file` | `path`、`edits`（每段 `old_text`、`new_text`） | 同上 |

参数拒绝未知字段、错误类型、重复/禁用工具及越限值。schema 供模型参考，实际调用重新校验。非法参数与禁止访问为 Rejected；合法路径的不存在、普通 I/O、NUL 二进制或非法 UTF-8 为 Failed。到达扫描/输出上限可以是 Succeeded，但结果明确标记截断或不完整；不能据此断言未扫描区域没有匹配。搜索只处理 UTF-8 文本，不运行 shell 或正则程序。写入工具给模型的结果只有摘要，不回传 diff 或全文。

## 写入与先读后改

`write_file` 接受 `path` 和 `content`，目标不存在时新建，已存在时整体覆盖。新建不需要先读；执行时目标已经出现但没读过，按“已存在但未读”拒绝。覆盖已有文件必须先 `read_file`，且当前版本与读取时一致。`content` 最多 `max_write_bytes`（默认 1 MiB），已有文件还要不超过 `max_edit_file_bytes`（默认 2 MiB）。已有文件为 CRLF 时，`content` 中单独的 LF 会写成 CRLF。

`edit_file` 接受 `path` 和 `edits: [{old_text, new_text}]`。每个 `old_text` 必须在原文件中恰好出现一次，各段不重叠；所有替换对照原文定位后一次生成新内容。只做精确匹配，不做模糊匹配，也不提供 `replace_all`。`old_text` 为空、与 `new_text` 相同、匹配不到、匹配多处或段落重叠都是 Rejected，不弹确认，文件不动；错误信息指出是 `edits[i]` 及原因，方便模型自己修正。单次最多 `max_edit_segments` 段（默认 32）。

打开中的实例维护一张观察表，记录 `read_file` 读过的文件及其版本（设备号、inode、大小、mtime、内容哈希）；部分读取也算读过。`edit_file` 以及覆盖已有文件的 `write_file`，目标必须在表里且当前版本与记录一致。没读过时返回 Rejected，提示先用 `read_file`；读后被外部改过时返回 Rejected，提示重新读取。本工具写入成功后用新版本更新观察表，所以连续编辑同一文件不需要重读。观察表不保存，重开会话后需要重新读取。

确认面板显示完整 diff（3 行上下文）；`/tools` 面板展开的是工具参数和结果摘要，不含 diff。diff 只在打开中的实例里保留，不进入 Context，也不保存；重开后只能看到摘要。

## 确认与权限模式

这里的“权限模式”指 Tarz 的 `permission_mode`，决定哪些调用需要确认。MCP 条目里的 `approval` 是另一个设置，只决定该服务器或工具默认是否询问；两者组合后才是最终行为，见[配置](configuration.md#mcp-服务器)。

| 模式 | 确知只读 | 内置写入 | MCP 需询问 | MCP `approve` |
| --- | --- | --- | --- | --- |
| `read-only` | 执行 | 拒绝 | 拒绝 | 拒绝 |
| `ask`（默认） | 执行 | 询问 | 询问 | 执行 |
| `accept-edits` | 执行 | 执行 | 询问 | 执行 |
| `full-access` | 执行 | 执行 | 执行 | 执行 |

“确知只读”指三个内置只读工具，以及 `approval = "writes"` 下声明只读且没有声明破坏性的 MCP 工具。“MCP 需询问”指 `prompt` 档工具，以及 `writes` 档下没有声明只读或声明了破坏性的工具。`approve` 只回答“要不要问”，不说明工具没有副作用，所以 `read-only` 仍拒绝它。`writes` 采信服务器的只读声明；服务器把会写入的工具声明为只读时 Tarz 无法识别，这是选择该档的代价。

确认前的预检完成参数、路径、先读后改、edit 匹配和同批同路径去重，并生成统一 diff。预检失败的调用直接得到 Rejected，不打扰用户。同一批次里对同一路径有多个写入调用时，只处理第一个，其余 Rejected（“同一文件在一批里只能改一次”），也不询问；不同文件的写入照常并行。

- **允许本次**：执行时重新核对文件版本，与预检时一致才写入。不一致、目标已被删除或新建目标已被占用时结果为 Failed，文件不动。
- **拒绝**：可以附一句理由。模型收到 Rejected（`user declined this write`）和理由；这是普通工具结果，本轮继续，文件不动。
- 不提供“本会话内一直允许”。需要免确认时切换权限模式；弹窗本身只回答这一次。

等待确认不计入单次工具的执行期限（默认 10 秒），但仍受工作剩余期限约束；期限到会取消本轮，调用记为未执行，文件不动。TUI `Esc` 或 CLI `Ctrl-C` 也会结束等待。

- **CLI**：stdin 和 stderr 都是终端、且输入不是来自 `--stdin` 时，在 stderr 打印 diff 并读一行 `y`/`n`（也接受 `yes`/`no`）；拒绝时可再输入一句理由。stdout 的报告格式不变。不是终端时，`ask` 不阻塞，直接返回 Rejected（没有可用的确认途径），进程照常结束，文件不动。
- **TUI**：确认面板显示工具名、路径和可滚动 diff；`Enter` / `y` 允许，`n` 拒绝并可输理由，`Esc` / `Ctrl-C` 取消本轮。防误触窗口和其他面板细节见 [TUI](tui.md)。

建会话时用预设的 `permission_mode`、CLI `--permission-mode` 或 TUI 新建表单选择初始模式。运行中：

- TUI 用 `/mode <name>` 切换；`Shift-Tab` 在 `read-only`、`ask`、`accept-edits` 之间循环。`full-access` 不在循环里，只能用 `/mode full-access` 进入。
- CLI 的 `session run --permission-mode <name>` 改变并保存当前模式。
- 切换只影响之后才做决定的调用，包括当前批次里还没轮到询问的调用；正在显示的确认仍要用户回答。
- 当前模式随记录保存，重开时恢复；当前是 `full-access` 而初始模式不是时，重开得到 `ask`。
- 模式切换不写入 Context，模型只从 Rejected 结果的正文得知当前模式不允许。

## 限额

| 有效限制 | 默认及硬上限 |
| --- | --- |
| 每会话 / 全 host 实际文件操作 | 4 / 16 |
| 单次工具执行期限（不含等待确认） | 10 秒，同时受工作剩余期限限制 |
| 完整序列化工具结果 | 64 KiB |
| 目录条目 / 搜索条目 | 200 / 4000 |
| 单文件扫描 / 搜索总扫描 | 2 MiB / 8 MiB |
| 搜索文件 / 目录深度 / 匹配 | 1000 / 32 / 200 |

| 写入限制 | 默认及硬上限 |
| --- | --- |
| 可编辑文件大小 / 写入内容大小 | 2 MiB / 1 MiB |
| 单次 `edit_file` 的段数 | 32 |

预设可降低有效限制（结果最小 1024 bytes），工具参数只能进一步降低；预设和参数都不能提高硬上限。`[profiles.NAME.tool_limits]` 控制上面的通用限额，`[profiles.NAME.write_limits]` 控制三个写入限额，字段见[配置](configuration.md)。跳过前文、长行和 JSON 转义都计入限制；扫描与输出在构造过程中限量，不先收集全部结果再裁剪。搜索采用深度遍历，目录句柄随深度有界。

Causa executor 以同一保存期限作为调用等待的超时兜底；期限和取消也在排队及文件操作检查点生效；丢弃工具 future 会请求对应操作停止。每个工作的 blocking 任务由 TaskTracker 监督，写操作保留至真实闭包退出，工作终态和关闭都等待收尾。该机制不能强制抢占卡住的 OS I/O，不保证网络或异常文件系统上的有限时退出。

## 观察、保存与旧记录

TUI 区分 Waiting、Running、Returned（待材料提交）、Committed，以及工作终态和保存状态。观察丢失会提示，并用可靠终态摘要校正。每工作最多保留 128 个调用摘要，名称/参数摘要分别最多 128/256 bytes；取消后保留 Causa 实际返回的 UnknownOutcome；真实文件任务全部退出后，应用再区分已启动但无已接受结果与等待确认期间未执行。此投影不修改原批次或已提交结果，已提交的未知结果仍保留材料提交事实。摘要不消费原未提交 ToolBatch，不构成持久工作账本。`/tools` 或 F6 从实际材料缓存展开完整调用和结果；终端控制字符仅在展示时处理。

CLI 执行 stdout 仍只输出最终文本或既有 JSON 报告；工具材料可通过保存/导出查看。保存失败重试原材料，不重跑工具或模型。重开可显示原已提交调用/结果，不恢复或执行旧工作。取消可能只留下已提交声明，结果仍在原未提交批次中；保存不会补造配对结果。新输入遇到未配对材料会被原样返还。TUI F7 / `/repair` 可预览采纳原结果或移除未配对声明，CLI `session repair` 可显式移除；应用并保存后再发送新工作。UnknownOutcome 和原工作状态不变，完整条件见[中断后继续](session-continuation.md)。

写入执行到一半被取消时，Causa 给出 UnknownOutcome；原子写入保证文件只会是旧内容或新内容。修复预览对这类调用标出“文件可能已被写入，继续前请重新读取”。采纳原结果只支持正式装配的内置工具（三个只读加两个写入）；含 MCP 或其他非内置调用的批次只能移除，并提示外部副作用可能已经发生。

记录格式为 `tarz-session-v2`，`workspace_root`、`tools`、`tool_limits` 必须存在；写入限额、初始/当前权限模式和 MCP 快照可省略，省略时分别读作默认写入限额、`ask` 和没有服务器。v1 明确拒绝，不自动补权限或改写旧文件。可离线提取经过完整 Context 校验的材料，再显式创建采用新定义的会话：

```sh
tarz session export-materials --record ./old-record.json --output ./materials-new.json
tarz session create --store ./data --provider openai-chat --model "$MODEL_ID" \
  --load ./materials-new.json --json
```

导出最多读取 64 MiB，目标必须不存在；不读取凭据或调用模型，不复用旧定义。新建时如需工具，仍须显式选择工具和工作区。

## MCP 工具

MCP 工具不经过工作区目录句柄，因而不受上面的路径边界约束。配置、选择、确认档位、环境隔离和生命周期见[配置](configuration.md#mcp-服务器)。服务器以用户权限运行；需要限制它时只能靠权限模式和 `approval` 拒绝调用，不能依靠工作区边界或 OS 沙箱。
