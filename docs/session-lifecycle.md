# Session 的组装与运行机制

本文说明 2026-10-05 的当前实现。设计与验收集中在 [Session 结构与存储提案](../.agents/pipelines/archives/architecture/2026-10-03-session-structure-and-storage.md)，本机测试和资源原始数据见[实施记录](../.agents/reviews/2026-10-03-session-implementation/README.md)。

## 1. 创建和重开

```text
CLI 参数 → SessionDefinition + 初始 Context → PreparedCreate
  → SessionHost 登记 opening + 启动独立监督
  → 持久实例取得稳定文件锁 → 读取并校验最新完整记录
  → assembly 解析一次凭据并冻结 gateway / runner / 执行选项
  → 创建：确认初始 revision 1；重开：采用已有 revision
  → 发布绑定本次 SessionTarget 的 ManagedSession
```

[Host](../crates/tarz-core/src/application/host.rs)在持有目录锁时登记并启动监督任务，同 ID 并发调用连接同一实例。create 的 ConversationId 和初始 commit_id 由同步 PreparedCreate 固定；等待者离开不改变它们。读取/装配失败且任务停止后移除 opening；初始提交失败则保留可 lookup 的 InitializationBlocked 与原操作，不发布可提交视图。

[Definition](../crates/tarz-core/src/definition/types.rs)是可保存的数据；[assembly](../crates/tarz-core/src/assembly/provider.rs)才解析环境引用。定义在活动实例中固定，重开重新解析同一引用，连接不捕获某次 WorkRef。每工作仍获得对应身份、控制和固定策略的选项。构造和重开不发模型请求。

## 2. 受理与执行

[ManagedSession::submit](../crates/tarz-core/src/application/managed.rs)先借用底层唯一请求键表比较完整参数，再检查生命周期和保存门禁。保留键同参返回原回执，异参 Conflict；新键拒绝返还原请求。新工作受理与 host active 槽登记在同一应用门禁内，worker 即使先完成也不能让 T2 越过保存。

底层 [SessionHandle](../crates/tarz-core/src/session/handle.rs)分配 TurnId，原子追加输入，保存原请求副本，并移交 Context 给 worker。每实例最多一个活动工作，无后台提交队列。工作期限从受理起覆盖逐工作装配和执行；不覆盖 opening、stdin 和磁盘 I/O。

[Worker](../crates/tarz-core/src/session/worker.rs)检查控制，调用逐工作装配，再交给 Causa 公开 facade 的 `TurnRunner::run_streaming`。Causa 处理一次工作中的模型/工具循环，返回真实 Context、typed TurnResult 和未提交 ToolBatch；它不拥有 Tarz 目录、文件或工作历史。产品装配默认空工具目录，可按固定定义绑定五个内置工作区工具（三个只读和两个写入）。根资源在实例内共享，工具包装和 ToolReporter 按 WorkRef 新建。旧 Context 中的工具材料不会触发旧工具重跑。

## 3. Finished 与 Saved

底层 supervisor 在正常和异常路径均等待该工作的受监督文件闭包实际退出，冻结可靠工具状态，然后归还材料及批次并发布原 `Arc<WorkResult>`。工具 future 的丢弃会请求停止操作，不能代替真实任务回收。host 的[独立监督路径](../crates/tarz-core/src/application/supervisor.rs)等到真实材料可用，推进 MaterialVersion，并固定本次不可变保存操作。

```text
submit → Dirty → 原 WorkResult::Finished → 固定返回材料 → Saving
                                                       ├─ 确认 → Saved → 可受理 T2
                                                       └─ 错误 → SaveBlocked
                                                                    ↓ retry_save
                                                               原提交确认/重试
```

保存 I/O 在有并发上限的 blocking 任务执行，不占同步协调锁。等待者、取消、重放和观察仍响应。Saved 是持久事实；Finished 只描述执行。保存失败不会把 Completed 改成执行失败；重试不重跑模型。临时实例保持 NotTracked，仍等材料交还后才解除活动门禁。

`read_materials` 返回带版本的实际空闲材料副本。`edit` 以该 MaterialVersion 为基线验证并一次替换全部 blocks，拒绝旧版本、旧实例、Busy 或未处理保存；合法持久编辑固定新快照并保存，不接受可重入的任意编辑回调。

中断后材料处理复用同一编辑/保存门禁：`inspect_repair` 只读检查，`plan_repair` 构建经过协议校验的不可改写候选，`apply_repair` 核对材料版本、打开实例及原批次可用性再一次提交。take/forget 会使既有计划失效。新键配对准入失败返还原输入且不登记工作，保留键回放仍优先。CLI/TUI 操作与原结果保留范围见[中断后继续](session-continuation.md)。

## 4. JSON 提交与确认

[Store](../crates/tarz-core/src/store/json.rs)是 core 内部具体后端。记录格式 `tarz-session-v2` 保存 ConversationId、revision、last_commit_id、definition、context；只接受规范 UUID 文件名，不把任意输入拼为路径。定义中的 workspace_root、tools、tool_limits 必须完整存在；写入限额、初始/当前权限模式和 MCP 快照可省略，省略时按默认值读取。旧 v1 明确拒绝，离线材料导出见[工作区工具](workspace-tools.md)。工作表、请求键、执行结果、未提交批次、任务与连接不序列化。Causa 把工具结果状态和 artifact 类别的序列化形式改为 snake_case 之后，读取已存 Context 的三条路径（会话记录、材料文件、`export-materials`）都经过 [`material::vocabulary`](../crates/tarz-core/src/material/vocabulary.rs)：它在解码前把这两个字段的旧写法改成现行写法，对现行数据不做改动，所以不升记录格式；下一次写入即为现行写法。Causa 0.0.5 把消息角色改为 block 自身的事实后，同一处还把旧的 `{"shape":"parts","value":[..]}` 改写为 `{"shape":"message","value":{"role":..,"parts":[..]}}`。角色沿用旧渲染器从 `meta.source` 推断的结果：缺失或 `"assistant"` 为 `assistant`，`"system"` 为 `system`，其余为 `user`；唯一例外是 Tarz 的材料修复说明（来源 `application:tarz:material-repair`）读为 `system`，与现在写入时一致。

writer 持有稳定锁文件；每个在途 blocking 操作也持有它。提交先读最新版本：同 commit_id 必须完整内容相同；否则要求基线一致。写临时文件、同步后替换最终 JSON。回应丢失后读取确认原 commit；完整旧基线和持续同一写访问才可证明尚未提交。记录已推进或不可读时保留冲突/无法确认，不生成新身份或强行覆盖。

list 校验最终记录，独立报告坏 JSON、格式、身份、重复 BlockId、引用和锁错误，不回退默认定义或空会话。临时文件不出现在列表，清理要求对应写访问。锁文件不在关闭时删除；外部删除/替换锁文件不属于受支持的协调方式。

## 5. 关闭、故障和批次

host 的 close 检查完整 SessionTarget，标记 Closing，关闭新受理并请求取消，等待真实 worker 和 I/O。Save 确认最新材料后释放；保存失败保留 owner/写访问，重试可继续关闭。DiscardLocal 也等在途写入停止，只确认原 pending commit 而不再写入。返回的 CloseOutcome 区分 Applied、NotApplied、NotSubmitted 和 Unresolved；无法确认会保留原结构化错误，不能把关闭当成记录不存在。它不回滚已提交记录。opening 也可用 lookup 的 target 关闭，原等待者不会获得可提交句柄；进行中不同策略请求拒绝，受阻且无在途操作时可明确改选 DiscardLocal。

旧 ManagedSession 只能观察其保留事实，不会跟随 ConversationId 重定向。视图 Drop 不关闭 owner；最后一个外部 host Drop 请求监督清理，不能代替明确 await。运行时或进程退出不保证正常收尾。

owner/worker 异常会隔离新工作，等待可确认的任务停止，保留故障和锁直到明确处理。ContextUnavailable 如实报告，不把旧磁盘版本伪装成本次返回材料。操作系统句柄存活期间的本机文件锁没有租约到期；不支持外部撤销锁或跨主机共享执行。

`SessionHost::take_uncommitted_tool_batch(target, work)` 在同一协调边界从原 owner 一次取走原 U，无 await、复制或工具执行。观察结果的 Arc 不变。关联保存未处理时 forget 拒绝；forget 后释放请求键、终态和未领取批次，已有 Arc 仍有效。入口若需要 U 必须在最终关闭前领取；Store 不补存 U。

## 6. CLI 消费者

`session create` 只创建并确认初始记录。`session run ID` 重开保存定义，提交新工作，等待独立保存结果后关闭；不能临时覆盖 provider/model。`session list` 无需解析凭据。它们均要求显式 `--store`。

CLI 在保存失败时保留原执行 status/text，另输出 save_error；按用户选择导出随机不覆盖的恢复 Context，再以 1 退出。recovery_error、close_error 和 close_outcome 单列；恢复文件包含实际返回材料，不包含原工作表或未提交内容。Ctrl-C 取消工作后仍等待交还和保存；保存等待期间第二次 Ctrl-C 会停止等待、导出实际 Context 并以错误退出。

`tarz run` 创建 transient host，仍支持原 `--load/--save`：导入获得新 ConversationId，只有显式 `--save` 导出 Context，报告与退出码保持。低层 offline demo 是独立 fixture，用来观察单会话契约。


## 临时流式观察

`SessionHandle::subscribe_progress` 与 `ManagedSession::subscribe_progress` 返回本实例的 broadcast receiver。需要观察新工作的调用方先订阅再 submit；每项 WorkProgress 携带 WorkRef、InvocationId，以及 ModelStarted、正文 Text 或 ModelOutputReady。当前限额、Lagged 和截断语义见[当前实现](README.md#流式观察)。原 `TurnRunOptions::observer` 被组合调用，不被静默替换。

这些事件是可丢失的临时观察，无工作历史或重开恢复含义，也不是材料编辑指令。可靠 WorkResult 仍由原工作表保留；同步 observer 不等待 UI 消费。仅 Causa 返回的 Context 进入原保存流程，取消/EOF/error 前的文字碎片不被补成完整模型输出。模型输出已齐、工作已终结、材料已保存继续是不同事实。
