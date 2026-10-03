# 当前实现

2026-10-03：共享 SessionHost、固定会话定义、JSON Store、托管保存与 CLI 已接通。`tarz session create/run/list` 可保存并跨进程重开同一会话；`tarz run` 使用同一 host 的临时模式。TUI/GUI、流式展示、工具配置、父子 Agent、安装和发布仍在后续范围。

## 代码结构

| 路径 | 当前职责 |
| --- | --- |
| `crates/tarz-core/src/application/` | 活动目录、opening/关闭监督、实例视图、保存门禁与版本编辑 |
| `crates/tarz-core/src/definition/` | 可序列化的固定定义与纯校验 |
| `crates/tarz-core/src/assembly/` | 打开时解析凭据、装配 gateway/runner 与固定执行选项 |
| `crates/tarz-core/src/session/` | 单会话 owner/handle、原工作受理、监督、去重与结果 |
| `crates/tarz-core/src/store/` | 私有 JSON 后端、活动期文件锁、提交与确认 |
| `crates/tarz-core/src/material/` | 显式 Context 文件导入/导出、引用校验 |
| `crates/tarz-cli/src/cli/` | 命令参数、信号、展示、恢复文件与退出码 |
| `crates/tarz-cli/src/demo/` | 脚本模型和本地工具的底层 Session 演示 |
| `crates/tarz-core/tests/`、`crates/tarz-cli/tests/` | 核心契约消费者、真实 CLI 与本地 HTTP 测试 |

workspace 只有 `tarz-core` 和 `tarz-cli`，均为 `publish=false`，binary 仍名 `tarz`。CLI 单向依赖核心，核心没有入口/UI/信号依赖；内部 lib 不承诺第三方 SDK。Causa 精确消费 registry `=0.0.2`，不使用 path/patch 或源码复制。

完整行为见 [Session 运行机制](session-lifecycle.md)，使用方法见 [README](../README.md)，逐项验收与本机证据见 [专项提案](../.agents/pipelines/proposals/architecture/2026-10-03-session-structure-and-storage.md)和[实施记录](../.agents/reviews/2026-10-03-session-implementation/README.md)。

## 共享应用入口

`SessionHost` 独占活动目录与底层 Session owner。`PreparedCreate` 在异步前固定身份，`create/open` 登记后由独立任务监督；丢弃等待或 `ManagedSession` 视图不终止操作。`SessionTarget` 区分同一 ConversationId 的不同打开实例；旧 target 不会作用于重开的实例。

`ManagedSession` 提供 submit/observe/wait/cancel、状态、版本材料、原子 edit、wait_saved/retry_save 和 forget。视图不暴露底层 owner/handle。host 提供明确的 Save/DiscardLocal 关闭和原未提交 ToolBatch 的一次领取。调用方自行决定谁可使用 host；它不是用户身份认证系统。

固定定义包含 provider、model、endpoint、凭据环境变量引用、工作期限、保留容量和模型/工具次数限制。打开时校验、解析一次并固定资源，创建和重开不调用模型。重开保留定义与 BlockId，但工作表、请求键、WorkRef、终态和未提交批次不恢复，不自动重跑旧工作。当前实际产品工具目录为空。

低层 `session` 仍供核心与 fixture 消费者使用；产品 CLI 通过托管入口。其原去重、取消、typed 结果、批次和材料交还契约保留。

## 保存与关闭

执行终态和保存状态是独立事实。持久实例在 T1 受理后封锁新键和编辑，直到实际返回材料保存确认；同参保留键仍重放，异参仍 Conflict。Finished 可以早于 Saved。失败保留原结果与待提交快照，`retry_save` 确认/重试同一 commit，不调用模型。版本编辑先验证候选，拒绝时材料不变，成功后自动保存。

`close(Save)` 等待取消、材料交还和保存；保存失败保持 owner/锁，可重试。`DiscardLocal` 等待在途 worker/I/O 停止并确认原待提交操作后放弃本地内容，不撤销已落盘记录。CloseOutcome 返回已提交、未提交、未尝试或仍无法确认的事实，后者保留原错误。最后一个 host 被丢弃会请求受监督的 DiscardLocal 清理；正常等待收尾仍要求 Tokio runtime 存活，强退不保证未保存材料可恢复。

CLI 持久运行在成功、失败和受控中断后等待保存。保存失败按已选择的行为：导出实际 Context 到随机且不覆盖现有文件的 `tarz-recovery-<uuid>.json`，然后错误退出；导出失败独立报告。临时运行只在显式 `--save` 时导出。

## 后端与资源边界

`--store DIR` 是明确指定的本机目录，无隐式默认目录。记录位于 `DIR/sessions/<uuid>.json`，格式 `tarz-session-v1`。固定定义、Context、ConversationId、revision 和 last_commit_id 同次写入；初始 revision 为 1。凭据配置只保存环境变量名。用户材料本身按原样保存。

稳定的 `<uuid>.lock` 文件与可替换记录分离，OS 排他锁从读取前持有至本地 worker/I/O 停止。占用、不支持或 I/O 错误均拒绝，不降级为无锁。提交校验基线与完整内容，经同目录临时文件、内容刷新和原子替换；确认不明时保持原操作。list 忽略锁和临时文件，逐记录报告损坏与活动状态；列表不是全目录原子快照。

单份记录读取/序列化上限为 64 MiB；这是文件限制，不是内存预算。默认 host 最多 32 个 opening/活动/受阻/closing 槽、最多 4 个并发存储操作；每会话最多一个待确认提交。CLI 持久定义默认保留 256 项工作，临时定义为 1 项。容量用尽明确拒绝；无自动淘汰、卸载或工作历史归档。

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
./target/release/tarz demo
node scripts/verify-proposals.mjs --links
```

测试使用本地 HTTP、脚本模型/工具、真实文件、进程和确定性屏障，无线上模型凭据。Unix 测试发 SIGINT 与 kill；Windows 控制台信号和远端各平台尚无本轮本机证据。CI 配置包含 Linux/macOS/Windows，不代表已在每个平台执行。

开发流程在 [.agents/pipelines/WORKFLOW.md](../.agents/pipelines/WORKFLOW.md)；`.agents/`、`scripts/` 和 `AGENTS.local.md` 留在本机，不强行加入版本库或应用分发。
