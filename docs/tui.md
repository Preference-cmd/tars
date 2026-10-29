# TUI 原型

`tarz-tui` 是 Ratatui + Crossterm 的原生终端消费者，共享 `SessionHost` 与 [TOML 配置](configuration.md)。目前是开发原型：会话创建/打开、当前材料、临时流式正文、最终结果、多行输入、取消、保存及恢复操作已接通；已接通工作区工具（包括写入确认面板、`/mode` 命令和 `Shift-Tab` 模式切换）；MCP 通过 `mcp.json` 配置与表单选择、定义快照和运行期工具已接通，打开会话后状态行提示未启动的服务器和被跳过的工具，重开时若保存的 full-access 被降为 ask 也会提示，MCP 调用按权限模式支持执行、确认与拒绝；没有 GUI 或安装分发；命令经内置 `run_command` 执行，详见[工作区工具](workspace-tools.md)与[命令工具提案](../.agents/pipelines/proposals/feature/2026-10-27-sandboxed-shell-command.md)。构建与依赖基线见[当前实现](README.md)。

命令确认面板显示完整命令、工作目录、期限、网络策略和“工作区可写、`.git` 钩子与配置只读、凭据不可读”；沙箱外请求改为醒目的“不在沙箱内：可读写整机、可联网、可读凭据”并显示模型理由。`/status` 增加 `Commands: ...`，显示沙箱后端和各能力维度（不可用时显示原因）。新建表单在 Tools 包含 `run_command` 时显示只读的 `Command: network ... · timeout ...` 行；它不是输入字段，不能编辑。禁用命令工具后不再显示该行。

```sh
cargo build -p tarz-tui --locked
./target/debug/tarz-tui --config /path/to/config.toml
# 不使用全局配置，从新建表单填写模型：
./target/debug/tarz-tui --no-config --store ./tarz-data
```

stdin 和 stdout 都必须是交互终端。至少 30×12；低于该尺寸时整屏只显示缩放提示。有待确认请求时需要至少 30×19 才显示面板并接受允许/拒绝键；更小时同样只显示缩放提示，Esc 或 Ctrl-C 仍取消本轮。矮窗口表单一次显示当前字段，Tab 切换。新建会话需要 provider/model 和可解析的凭据环境变量，创建本身不调用模型；表单还包括 Workspace、Tools、初始权限模式和 MCP 服务器字段。F3 重开已存会话时采用原固定定义，不套用新的全局模型预设。

| 操作 | 按键 |
| --- | --- |
| 命令菜单 | 空输入时键入 `/`，或任意草稿下 F1；输入筛选，↑↓ 选择，Enter 执行，Esc 返回 |
| 新建/填写模型配置 | `/new` 或 F2；Tab/Shift-Tab 换字段，Ctrl-S 创建，Esc 返回 |
| 权限模式切换 | `Shift-Tab`（在主视图与确认面板内循环切换 ask / accept-edits / read-only）；或 `/mode` / `/mode <name>` |
| 写入确认 | 确认面板中 `Enter` / `y` 允许；`n` 拒绝并可输理由；`Esc` / `Ctrl-C` 取消本轮；`Shift-Tab` 切换模式；`↑` `↓` `PgUp` `PgDn` 滚动 diff（新请求从顶部开始，滚动不会越过内容末尾；待确认时窗口小于 30×19 只显示缩放提示，允许/拒绝键不生效；面板出现、队列换到下一条请求、切到有待确认请求的会话或 resize 重新显示后 0.5 秒内只响应 Esc/Ctrl-C 和滚动键，其他键被忽略且不进入草稿，提示变暗） |
| 会话选择 | `/sessions`，按模型名或 ID 筛选，Enter 切换并打开 |
| 帮助 / 完整状态与错误详情 | `/help` / `/status`；↑↓ 或 PageUp/PageDown 滚动 |
| 完整工具调用/结果 | `/tools` 或 F6；↑↓ / PageUp/PageDown 滚动 |
| 中断后处理材料 | `/repair` 或 F7；`a` 预览原结果、`r` 预览移除，Enter 应用并保存（新预览到达后 0.5 秒内忽略 Enter 及其他面板按键，Esc 与滚动仍生效，提示变暗） |
| 打开选中记录 | F3 |
| 切换会话 / 刷新记录列表 | F4 / F5 |
| 提交输入 | Enter 或 Ctrl-S |
| 换行 | Ctrl-J；也支持 Alt-Enter 和终端能报告的 Shift-Enter |
| 编辑 / 撤销、重做 | 方向键、Backspace、Delete / Ctrl-Z、Ctrl-Y |
| 滚动当前材料 | PageUp / PageDown |
| 取消当前工作 | Esc 或 Ctrl-C；菜单内 Esc 只返回 |
| 保存并关闭当前会话 / 保存并退出 | Ctrl-W / Ctrl-Q |
| 重试原保存 / 导出实际材料 | Ctrl-R / Ctrl-E |
| 放弃本地未确认内容并关闭 | Ctrl-D，再明确 y 确认；n 或 Esc 取消 |

界面采用单列对话布局：顶部是 Tarz、模型和会话信息，正文以 `❯ You` 和 `●` 区分输入与其他材料，输入区只有上下分隔线，随内容增高至最多 6 行，并受终端高度限制。背景和主文字沿用终端配色，暖橙色突出选择，灰色用于辅助信息，黄色用于错误和确认。没有常驻侧栏，宽窄终端采用相同结构；退出/放弃使用独立确认视图。

命令菜单包含 `/new`、`/sessions`、`/mode`、`/help`、`/status`、`/tools`、`/repair`、`/open`、`/cancel`、`/retry`、`/export`、`/close`、`/discard` 和 `/quit`。菜单出现期间输入只用于筛选；需要在正文输入以 `/` 开头的文本时可以粘贴。`/status` 显示打开时的详情快照，再次打开可刷新。F2–F7 和保存恢复快捷键在主视图可用；菜单内按键用于菜单操作，Ctrl-Q 仍可发起退出。

会话各自保留未提交草稿。切换视图和重绘不取消工作、不重复提交；粘贴只插入文本，控制字符会被过滤，粘贴 `/quit` 等字符串也不会执行命令。Enter 在本轮改为发送；换行请使用 Ctrl-J。Ctrl-Q/关闭遇到草稿先确认；未提交草稿不属于自动保存的 Context。执行和保存状态分别显示，Completed 不等于 Saved。历史材料按内容显示，不恢复旧工作表或执行旧工具。

保存失败时保留 owner 和写锁，并暂停退出；可以修复目录后 Ctrl-R，或 Ctrl-E 导出至配置的恢复目录（默认当前目录），再选择下一步。导出生成 `tarz-recovery-<uuid>.json`，不覆盖已有文件；导出失败独立报告。超过 64 MiB 的恢复文件仍完整写出，但会明确提示当前版本无法直接导入。Ctrl-D 不撤销已落盘记录，关闭报告保留原提交是否已应用/未应用/未尝试/仍无法确认的事实。

中断后有未配对声明时，发送会保留草稿并提示 F7。材料面板先等原工作收尾，显示有界诊断与原批次摘要；`a` 或 `r` 只生成预览，Enter 才应用并等待保存，Esc 返回草稿；新预览到达后 0.5 秒内忽略 Enter 及其他面板按键（Esc 与滚动仍生效），提示变暗。预览中 F4 切换会话会释放计划；迟到结果不能覆盖新面板。原工作仍保持 Cancelled/TimedOut，另行发送草稿才创建新工作。重开不能找回原批次，但可使用移除模式；详见[中断后继续](session-continuation.md)。

后台适配任务 panic 只标记对应会话，界面和其他会话继续运行。终端读取/绘制错误或可捕获 panic 会恢复终端模式，然后尝试保存；保存失败时尝试导出实际材料，最后等待资源释放并以 1 退出，失败细节写 stderr。这是失去交互面的异常清理路径。不可捕获强退不保证清理和未保存内容可恢复。

渲染只使用缓存的可见行。材料版本变化时读取一次完整 Context，在后台生成文本和换行索引；读取失败后至少间隔 1 秒再重试，不逐帧重试；终端变宽/变窄时重新计算索引，输入事件仍可处理。任务最多 64 个，核心另有活动实例和存储并发限制；这些计数不构成总内存预算。当前列表保留本次进程已见到的记录，F5 补入新记录，打开时重新验证实际记录。

输入组件当前使用 ratatui-textarea 0.9.2，普通左右移动、Backspace/Delete 适配了组合字符/emoji 字素边界；这不是完整 IME 编辑器保证。PTY 已覆盖 UTF-8、多行粘贴、resize、取消/关闭/保存故障和终端恢复，实际输入法候选窗、不同终端的字宽及 Linux/Windows 终端仍需人工/平台验证。远端 CI 未在本轮执行，不能把本机构建当作跨平台支持证据。

此前双栏原型的本机 debug 测量中 100 KiB/1 MiB/8 MiB 和 8×1 MiB 会话的缓存绘制 p95 约 1–3 ms，30 次绘制没有额外读取完整 Context。8 MiB 重新换行约 1.2 秒，属于后续优化点；数据只代表该次本机实验，详见[原型验证记录](../.agents/reviews/2026-10-04-tui-prototype/README.md)。

本轮对话布局的终端捕获、宽窄窗口与菜单行为证据见[界面改造记录](../.agents/reviews/2026-10-04-conversation-ui/README.md)。界面参考 Claude Code 的对话/输入/命令交互，仍使用 Ratatui 全屏备用屏幕；尚无原生终端 scrollback、Markdown 渲染或 Claude Code 的其他命令能力。该记录为流式接线之前的界面证据。


## 流式正文

OpenAI Chat Completions、OpenAI Responses 和 Anthropic Messages 的正文增量显示在对话尾部，标记 `Generating · temporary`。完整模型输出到达后可显示 `Temporary output · awaiting result`；这个状态不表示保存成功。当前只预览正文，不展示 reasoning、工具参数草稿或增量用量。

预览独立于正式材料缓存，每个会话只保留当前工作的当前模型轮。可以继续编辑下一条草稿、滚动、resize 或切换会话；取消仍作用于明确的 WorkRef。切换不会重新提交；已关闭或重开实例的迟到事件被忽略。已发送输入仍以实际材料刷新为准，预览不会伪造 Context。

UI 最多每 50 ms 更新一次，所有实例合计每 tick 最多取 128 条观察，超出时轮转到下一个实例。core 通道容量 128，每条正文最多 16 KiB；TUI 正文预览最多 256 KiB。拥塞或截断会显示 `Preview incomplete`，最终结果仍从可靠工作终态和实际 Context 获取。消费者落后不能阻塞模型/取消，也不会丢失原来的结果或保存错误。

增量只重新排布末尾两行；对异常长组合字符序列，最多回看 4 KiB UTF-8 边界，因而这种病理性 cluster 的临时换行可能分行。变宽/变窄时重新排布这份有界预览；正式历史保持原缓存，不按每个增量复制或重排。

工作完成、失败、取消或关闭会移除临时预览，正式正文从最终返回材料更新，可能与预览不同。保存受阻时仍显示真实返回材料并沿用重试/导出/放弃流程。恢复文件、导出及 Store 均不包含预览；未发送草稿也不自动保存。

本轮真实分段 HTTP、TestBackend、PTY 和资源证据见[流式接线记录](../.agents/reviews/2026-10-04-tui-streaming/README.md)。本地路径及本机验证不代表 registry 已发布或各平台 CI 已通过。

## 工具执行

新建表单的 Workspace 与 Tools 使用 core 的统一解析。正文展示有界参数摘要及 Waiting / Running / Returned / Committed；Returned 仍待材料提交，Committed 不等于工作完成或 Saved。调用以当前实例、WorkRef、InvocationId 和声明 BlockId 归属，相同参数仍是两次独立调用。

工具事件走同一有界观察通道，落后会提示 `Tool observations incomplete`，结束时用可靠摘要校正。取消后显示实际 UnknownOutcome 或未执行事实，不能把整个批次都标为已运行。完整结果从已有实际材料缓存展开：F6 或 `/tools`；这不会领取原未提交批次、读取文件或重跑工具。摘要与完整结果中的控制字符、双向文本控制符和行/段分隔符仅在展示层转义，普通中文、emoji 和换行保持原样；Store 中材料保持原样。

工具事件更新小状态集合，不逐事件复制或重排整个 Context。材料版本变化才刷新缓存；展开、滚动、resize 和切换不增加模型/工具调用。详细限额和平台边界见[工作区工具](workspace-tools.md)。
