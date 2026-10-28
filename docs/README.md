# 当前实现

Tarz 的目的是验证并改进 Causa，应用是验证的载体，见 [README 的“目的”](../README.md#目的)。本页只记录当前实现。

2026-10-26：共享 SessionHost、固定会话定义、JSON Store、托管保存与 CLI 已接通。`tarz session create/run/list` 可保存并跨进程重开同一会话；`tarz run` 使用同一 host 的临时模式。CLI 与 TUI 原型共用 TOML 配置，`tarz-tui` 已接通会话、输入、临时流式正文预览、结果、取消及保存恢复；工作区工具（三个只读工具与 `write_file`/`edit_file`）与可靠状态已接通；写入前先读后改，`ask` 模式在终端/确认面板确认，非终端时拒绝；`accept-edits` 和 `full-access` 不询问；四种权限模式可在运行中切换并随记录保存，`tarz-tui` 已接通写入确认面板、`/mode` 命令和 `Shift-Tab` 模式切换。MCP 通过独立的 `mcp.json` 配置（支持 `${VAR}` 展开与字面密钥检查）、预设/CLI/TUI 选择、定义快照和内置 MCP 客户端的工具调用已接通，MCP 调用按 `mcp.json` 的 `approval` 设置和当前权限模式执行、确认（终端或 TUI 确认面板）或拒绝；GUI、父子 Agent、安装和发布也仍在后续范围。

2026-10-27：`tarz-core::sandbox` 已提供平台无关策略、后端能力报告、进程树与 macOS Seatbelt 后端；探针验证文件、网络、信号与凭据边界。该层已接入 `run_command`（macOS；权限类别、定义/配置和修复清单已接通，Linux/Windows 失败关闭），完整文档和真实使用仍待第 3 步。

## 代码结构

| 路径 | 当前职责 |
| --- | --- |
| `crates/tarz-core/src/application/` | 活动目录、opening/关闭监督、实例视图、保存门禁与版本编辑 |
| `crates/tarz-core/src/definition/` | 可序列化的固定定义与纯校验 |
| `crates/tarz-core/src/config/` | 共享 TOML 加载、路径、命名模型预设与新定义默认值 |
| `crates/tarz-core/src/workspace/`、`tools/` | 固定根目录句柄、工作区工具（三个只读工具与两个写入工具）、有界扫描与受监督文件任务 |
| `crates/tarz-core/src/sandbox/` | 平台无关沙箱策略、后端能力报告、进程树与 macOS Seatbelt 后端；由 `run_command` 使用 |
| `crates/tarz-core/src/mcp/` | 会话的 MCP 服务器：启动与关闭、子进程环境、工具过滤、结果上限与状态 |
| `crates/tarz-core/src/assembly/` | 打开时解析凭据、装配 gateway/runner 与固定执行选项 |
| `crates/tarz-core/src/session/` | 单会话 owner/handle、原工作受理、监督、去重、可靠结果与有界临时观察 |
| `crates/tarz-core/src/store/` | 私有 JSON 后端、活动期文件锁、提交与确认 |
| `crates/tarz-core/src/material/` | 显式 Context 文件导入/导出、引用校验 |
| `crates/tarz-cli/src/cli/` | 命令参数、信号、展示、恢复文件与退出码 |
| `crates/tarz-cli/src/demo/` | 脚本模型和本地工具的底层 Session 演示 |
| `crates/tarz-tui/src/` | 单列对话、命令/会话选择、输入适配、流式预览与实际材料呈现 |
| `crates/tarz-core/tests/`、`crates/tarz-cli/tests/` | 核心契约消费者、真实 CLI 与本地 HTTP 测试 |

workspace 包含 `tarz-core`、`tarz-cli` 和 `tarz-tui`，均为 `publish=false`，binary 为 `tarz` 和 `tarz-tui`。CLI/TUI 只向下依赖核心和 Causa（另直接使用 `BlockId` 等事实词汇），核心没有入口/UI/信号依赖；内部 lib 不承诺第三方 SDK。以公开 facade 消费 crates.io 上发布的 `causa 0.1`，具体版本由 Cargo.lock 固定，构建不需要同级 Causa checkout。没有源码 include 或复制。独立 CI/分发是后续交付步骤。共享配置及终端边界见 [配置](configuration.md)与 [TUI](tui.md)。

完整行为见 [Session 运行机制](session-lifecycle.md)，使用方法见 [README](../README.md)，逐项验收与本机证据见 [专项提案](../.agents/pipelines/archives/architecture/2026-10-03-session-structure-and-storage.md)和[实施记录](../.agents/reviews/2026-10-03-session-implementation/README.md)。

## 共享应用入口

`SessionHost` 独占活动目录与底层 Session owner。`PreparedCreate` 在异步前固定身份，`create/open` 登记后由独立任务监督；丢弃等待或 `ManagedSession` 视图不终止操作。`SessionTarget` 区分同一 ConversationId 的不同打开实例；旧 target 不会作用于重开的实例。

`ManagedSession` 提供 submit/observe/wait/cancel、subscribe_progress、状态、版本材料、原子 edit、wait_saved/retry_save 和 forget。视图不暴露底层 owner/handle。host 提供明确的 Save/DiscardLocal 关闭和原未提交 ToolBatch 的一次领取。调用方自行决定谁可使用 host；它不是用户身份认证系统。

中断后继续通过 `inspect_repair/plan_repair/apply_repair` 显式处理材料。正式内置工具装配可采纳受控中断的原结果，重开无原批次时可移除未配对声明；新输入先经过配对准入，保存确认后才开始新工作。CLI `session repair` 与 TUI F7 共用此行为，完整条件、版本与保留边界见[中断后继续](session-continuation.md)。

固定定义包含 provider、model、endpoint、凭据环境变量引用、工作期限、保留容量、模型/工具次数限制，以及工作区、工具选择、完整工具与写入限额、初始权限模式和 MCP 快照。打开时校验、解析一次并固定资源，创建和重开不调用模型。重开保留定义与 BlockId，但工作表、请求键、WorkRef、终态和未提交批次不恢复，不自动重跑旧工作。默认工具目录为空，可显式启用 list_directory/read_file/search_text/write_file/edit_file。边界及记录迁移见[工作区工具](workspace-tools.md)，本轮证据见[实施记录](../.agents/reviews/2026-10-04-workspace-tools/README.md)。

低层 `session` 仍供核心与 fixture 消费者使用；产品 CLI 通过托管入口。其原去重、取消、typed 结果、批次和材料交还契约保留。

## 流式观察

Session worker 使用 `TurnRunner::run_streaming`，原装配 observer 仍按同步借用契约执行。CLI 继续等待并呈现最终结果；TUI 在 submit 前订阅 `ManagedSession::subscribe_progress`。事件按 WorkRef 和 InvocationId 归属，携带 ModelStarted、正文 Text、ModelOutputReady 和有界工具状态；工具状态还以声明 BlockId 区分。当前不展示 reasoning 或未完成工具参数。

通道属于一个打开实例，容量 128 项，每项正文最多 16 KiB，UTF-8 边界截断带 `truncated` 标记；无订阅者不复制正文。慢消费者收到 Tokio broadcast 的 Lagged，迟到订阅不回放历史；订阅/丢弃观察器不会取消工作或改变最终结果。调用方保留的事件 Arc 不计入通道容量。可靠终态仍走 observe/wait，与可丢失的临时预览分开。

TUI 每 50 ms 最多处理 128 项，多个视图轮转；只保留当前工作、当前模型轮的最多 256 KiB 正文，落后/超限明确提示。结束、失败、取消和关闭清除预览，正式对话从实际返回的 Context 更新。ModelOutputReady 仅表示模型交付完整输出，不表示已提交材料或保存成功。预览不进入 Store、材料导出或恢复文件。

## 保存与关闭

执行终态和保存状态是独立事实。持久实例在 T1 受理后封锁新键和编辑，直到实际返回材料保存确认；同参保留键仍重放，异参仍 Conflict。Finished 可以早于 Saved。失败保留原结果与待提交快照，`retry_save` 确认/重试同一 commit，不调用模型。版本编辑先验证候选，拒绝时材料不变，成功后自动保存。

`close(Save)` 等待取消、材料交还和保存；保存失败保持 owner/锁，可重试。`DiscardLocal` 等待在途 worker/I/O 停止并确认原待提交操作后放弃本地内容，不撤销已落盘记录。CloseOutcome 返回已提交、未提交、未尝试或仍无法确认的事实，后者保留原错误。最后一个 host 被丢弃会请求受监督的 DiscardLocal 清理；正常等待收尾仍要求 Tokio runtime 存活，强退不保证未保存材料可恢复。

CLI 持久运行在成功、失败和受控中断后等待保存。保存等待期间第一次 Ctrl-C 在 stderr 提示，第二次停止等待并按保存失败路径导出实际 Context、以错误退出。保存失败按已选择的行为：导出实际 Context 到随机且不覆盖现有文件的 `tarz-recovery-<uuid>.json`，然后错误退出；导出失败独立报告。临时运行只在显式 `--save` 时导出。

## 后端与资源边界

本机存储目录由 `--store DIR` 或共享 TOML 的 `store` 明确指定，没有自动生成的存储默认路径。记录位于 `DIR/sessions/<uuid>.json`，格式 `tarz-session-v2`；`workspace_root`、`tools`、`tool_limits` 必须存在，写入限额、初始/当前权限模式和 MCP 快照可省略，省略时按默认写入限额、`ask` 和没有服务器读取。v1 不自动升级；`session export-materials` 可离线导出经校验的实际 Context，再显式采用新定义创建会话。固定定义、Context、ConversationId、revision 和 last_commit_id 同次写入；初始 revision 为 1。凭据配置只保存环境变量名。用户材料本身按原样保存。

稳定的 `<uuid>.lock` 文件与可替换记录分离，OS 排他锁从读取前持有至本地 worker/I/O 停止。占用、不支持或 I/O 错误均拒绝，不降级为无锁。提交校验基线与完整内容，经同目录临时文件、内容刷新和原子替换；确认不明时保持原操作。list 忽略锁和临时文件，逐记录报告损坏与活动状态；列表不是全目录原子快照。

单份记录读取/序列化上限为 64 MiB；这是文件限制，不是内存预算。恢复导出不截断材料，超过该上限时仍完整写出，但在结果中明确报告当前版本无法直接导入。默认 host 最多 32 个 opening/活动/受阻/closing 槽、最多 4 个并发存储操作；每会话最多一个待确认提交。CLI 持久定义默认保留 256 项工作，临时定义为 1 项。容量用尽明确拒绝；无自动淘汰、卸载或工作历史归档。

保留成本包括当前 Context、创建时的原始材料、请求原输入、Arc 终态/未领取批次、待保存与基线副本、序列化缓冲，以及调用方持有的副本。关闭后仍持有的视图/观察也可能保留底层工作表和材料；活动槽上限不限制这些外部持有对象。list 需要完整校验每份记录；大量记录的扫描和内存仍随规模增长。观测样本中 4.2 KiB/1.13 MiB/9.06 MiB 材料场景峰值 RSS 约为 6.9/24.9/158.7 MiB；8 个同时活动的中型会话为 93.8 MiB。这是本机单次实验，不能外推吞吐保证，也未采集可靠 CPU cache 指标。原始规模、延迟、复制/序列化和字节计数见实施记录。

默认拒绝外部媒体/artifact 引用。显式 material 扩展接口可交给调用方验证引用，但 Store 不自动复制资产。当前验证范围是本机进程中断后的完整旧/新记录；不承诺掉电后的目录项持久性、网络文件系统/同步盘跨主机执行或远程副作用 exactly-once。

## 本地检查

```sh
cargo fmt --all --check
cargo clippy --workspace --locked --all-targets -- -D warnings
cargo test --workspace --locked
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --locked --no-deps
cargo build -p tarz-core --locked
cargo build --workspace --locked --release
./target/release/tarz session --help
./target/release/tarz-tui --help
./target/release/tarz demo
node scripts/verify-proposals.mjs --links
cargo deny check   # 安全公告、许可证、来源；规则在 deny.toml
```

测试使用本地 HTTP、脚本模型/工具、真实文件、进程和确定性屏障，无线上模型凭据。Unix 测试发 SIGINT 与 kill；工具访问实现仅开放 Linux/macOS，本轮实测为 macOS。Windows 无工具功能与各平台远端 CI 尚需各自证据。CI 配置包含 Linux/macOS/Windows，不代表已在每个平台执行。

开发流程在 [.agents/pipelines/WORKFLOW.md](../.agents/pipelines/WORKFLOW.md)；`.agents/`、`scripts/` 和 `AGENTS.local.md` 留在本机，不强行加入版本库或应用分发。
