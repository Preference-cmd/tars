# Session 的组装与运行机制

2026-10-03，依据 `9ab10e6` 的实际实现核对。本文说明现状；可能的 Store 和后续结构建议放在 [设计附件](../.agents/pipelines/assets/tarz-agent/session-assembly-and-store.md)，不视为当前保证。后续收敛方案见 [托管 Session 设计](../.agents/pipelines/assets/tarz-agent/managed-session-design.md)，其中的 owner 管理和自动保存均为候选，尚未实现。

首个物理后端的建议与验证范围见 [JSON Store 方案](../.agents/pipelines/assets/tarz-agent/first-session-store.md)。该方案区分活动期独占、保存提交和崩溃保证；当前 CLI 仍只有下文所述的显式材料导入/导出。

## 1. 当前调用路径

```text
CLI 参数 / 环境变量 / 输入 / 可选材料文件
  → ModelConfig::session(initial_context, session_config)
  → Session::new(conversation_id, context, assemble, config)
  → run_once → handle.submit(request_key, blocks)
  → worker → assemble(work_ref, run_control)
  → TurnRunner::run(turn_id, context, options, control)
  → supervisor 归还材料、未提交批次并发布终态
  → run_once shutdown → RunReport
  → CLI 可选保存 Context，输出文本 / JSON
```

这条路径有会话级资源组装和逐工作装配两个时点。当前没有统一 Session 目录、持久化 SessionStore 或可跨进程恢复的工作记录。

## 2. 创建 Session：资源与活动容器

[CLI prepare](../crates/tarz/src/cli/execute.rs)读取参数指定的凭据环境变量、可选材料文件及文本输入，构造 ModelConfig。材料导入校验 BlockId 唯一性和外部引用；它不启动执行。

[ModelConfig::session](../crates/tarz/src/application/model.rs)现在同时承担：

1. 根据 provider 创建实际 HTTP gateway，可覆盖完整 endpoint。
2. 创建空工具目录的 ToolExecutor，并与 gateway 组装可共享的 Arc<TurnRunner>。
3. 新分配 ConversationId，把初始 Context、工作配置及装配闭包交给 Session::new。

当前模型和 provider 配置被闭包固定；正式 `run` 没有工具选择、preparer 或 observer 配置。装配闭包每次返回同一个 runner 的 Arc 和新构造的 TurnRunOptions。由于没有捕获工作身份的工具包装，这种复用适用于现有消费者。

[Session::new](../crates/tarz/src/session/owner.rs)只创建活动容器：Context、空工作表、空请求键索引、活动槽、关闭/故障标志、装配回调和配置。它不调用 assemble、不创建 worker、不发送网络请求，不根据材料猜测工作历史。

Session owner 不可 Clone；handle 可以 Clone。这个约束只在该实例内成立，不会阻止另一进程或另一个 Session::new 使用相同 ConversationId。

## 3. 受理：先去重，再新建工作

[SessionHandle::submit](../crates/tarz/src/session/handle.rs)在同一同步协调锁内按顺序完成：

1. 先查保留的请求键：完整 blocks（ID、内容、metadata、顺序）相同则返回旧回执；不同则 Conflict。重放不重新装配或执行。
2. 对新键检查 Faulted、Closed、Busy、容量、Context、运行时和期限是否有效。
3. 分配 TurnId 并检查保留表内碰撞，建立 WorkRef 与该工作的取消 token、绝对期限。
4. 原子追加输入；失败返还原输入，不登记部分工作。
5. 保留原请求副本，登记 key → TurnId、Accepted 记录与活动槽，移交 Context。
6. 释放锁，在锁外启动 worker 和 supervisor，返回 WorkReceipt。

回执只表示进程内受理，当前没有落盘保证。worker 可以先于调用方实际收到回执被调度。一个实例最多一个活动工作，没有后台提交队列；每项期限从受理起算，覆盖装配和运行。

## 4. 逐工作装配：把当前工作接到真实依赖

[worker](../crates/tarz/src/session/worker.rs)持有 Context，先检查控制，再调用 `assemble(WorkRef, RunControl)`，至多一次。装配过程中取消或到期可以结束装配，Context 仍由 worker 保管。

装配返回 `(Arc<TurnRunner>, TurnRunOptions)`；返回后再次检查控制，再决定是否进入 runner。普通连接与无工作状态的实现可共享；捕获当前 WorkRef 的工具、处理器、preparer、observer 应在这个时点绑定。闭包中的配置必须成为实际执行值。

WorkRef 和控制属于单次工作，不应固化到跨工作复用的连接中。当前模型配置固定在会话构造时，尚无运行期间切换配置的契约。

## 5. Causa：一次工作中的多次模型/工具循环

Tarz 调用 registry Causa 0.0.2 的 TurnRunner::run。按实际 runner 顺序概括：

```text
检查控制与模型轮次上限
  → 为本次 Invocation 绑定工具目录与控制
  → preparer 编辑 / 选择材料，生成请求 Frame
  → gateway 模型调用
  → 检查停止原因并提交可接受的模型材料
  → EndTurn：返回 Completed
  → 新工具声明：处理批次、校验与提交工具结果
  → 下一次 Invocation，或返回中断
```

当前 CLI 的工具目录为空，preparer 未设置，使用默认材料 frame。旧材料中的 ToolCall 不会被当作本次新声明自动重跑。拒答、长度限制和各阶段错误可在结果里保留尚未提交的原输出。

Causa 只持有一次运行的 Context 与当前工具批次；不负责 Tarz 的会话目录、请求键、文件保存或工作历史。

## 6. 收尾、观察与取消

正常或受控中断返回 `TurnOutcome { context, result, uncommitted_tool_batch }`。supervisor 通过 [Shared::finish](../crates/tarz/src/session/state.rs)把实际材料及批次放回 Session，在同一协调范围发布不可变终态并清空活动槽。

观察者得到 Arc<WorkResult>，其中保留原 typed TurnResult 或错误。材料继续属于 Session；终态不等于自动保留该工作对应的 Context 快照。若 T1 后 T2 已受理，T1 的 wait 完成也不会为 owner 预约空闲材料访问。

取消只请求停止：进入 runner 后仍要等待其正常交回 Context 和批次。wait 的超时不取消工作。worker 异常终止由 supervisor 发布 Faulted，丢失 Context 则明确不可用，不从旧文件伪造当前材料。

shutdown 关闭新键受理、请求取消并等待收尾，不自动删除记录或保存文件。Drop 只请求取消，不等待；进程或运行时退出不保证收尾。

## 7. 当前保存与重建

[run_once](../crates/tarz/src/application/execution.rs)消费一个独占使用的 Session，等待终态后 shutdown，复制收尾材料，领取原未提交批次，并返回 RunReport。它是单次执行便利入口，不是长期多工作会话目录。调用期间不能通过其他 handle 并发提交。

[CLI](../crates/tarz/src/cli/execute.rs)在收到报告后，按显式 --save 调用 [material::save](../crates/tarz/src/material/file.rs)。这意味着工作完成与文件保存是独立事实：保存失败不改 Completed，但命令返回失败。当前保存位置与选择由 CLI 管理，完整的多入口会话管理尚未实现。

材料文件仅包含 Context.blocks；没有 ConversationId、工作表、原请求键、配置、取消 token、未提交批次或完整 WorkResult。文件采用同目录临时写入、内容 sync 和原子替换；没有跨文件事务、写入版本冲突检测或资产保留协议。Causa 当前 Context 本身也不携带持久化修订号。

load 验证材料后，当前 ModelConfig::session 创建新的 ConversationId 和空工作表。通用 Session::new 允许调用方传入旧 ConversationId，但同样不会恢复旧记录。导入材料继续执行，与恢复旧工作是不同操作。

## 8. 当前数据归属

| 数据 | 当前 owner / 保存位置 | 当前重启后可获得什么 |
| --- | --- | --- |
| Context | 空闲 Session 或活动 worker；可显式保存为文件 | 仅最后一次显式成功保存的 blocks |
| 请求键、原输入、WorkRef、终态、未领取批次 | Session 工作表 / 外部 Arc | 不恢复 |
| gateway、executor、runner、装配闭包 | 应用构造的进程内对象 | 需要重新构造 |
| token、deadline、通知、task、锁 | 当前运行时 | 不恢复 |
| provider/model/endpoint/凭据 | CLI 输入、环境及装配闭包 | 需要重新提供 |
| 外部媒体与 artifact | 由调用方验证和保留 | 默认文件接口拒绝；扩展接口重验可读性 |

后续要增加的 Store、活动会话目录和保存时点应在这一归属基础上设计，具体选项见 [分析附件](../.agents/pipelines/assets/tarz-agent/session-assembly-and-store.md)。
