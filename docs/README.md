# 当前实现

2026-10-02：单会话基础实现已接通，包括内部 Session 模块、材料文件和 `tarz demo` 离线消费者。真实 provider 配置、交互式 CLI/TUI、流式展示、父子 Agent、安装和发布仍在后续范围。

## 代码结构

| 路径 | 当前职责 |
| --- | --- |
| `crates/tarz/src/main.rs`、`cli/` | 参数、运行时入口与退出码 |
| `crates/tarz/src/lib.rs` | 不可发布的内部模块入口，供 CLI 和集成测试共用 |
| `crates/tarz/src/session/` | owner/handle、受理与去重、工作监督、控制、结果保留 |
| `crates/tarz/src/material/` | Context JSON 文件、原子替换及外部引用验证 |
| `crates/tarz/src/demo/` | 脚本模型、本地工具、T1/T2/摘要/重放/保存重建 |
| `crates/tarz/tests/` | 通过内部模块公开接口的集成消费者和 CLI 测试 |
| `Cargo.toml`、`Cargo.lock` | workspace 元数据和锁定的 registry 依赖 |
| `.github/workflows/ci.yml` | 格式、静态检查、文档、测试与多平台构建/演示 |

Causa 精确依赖 0.0.2，只启用 runtime；不使用源码 include、path 或 patch 替换。内部 lib 不构成第三方 SDK。

## 单会话边界

`Session::new` 接收调用方的 ConversationId、初始 Context、异步装配回调及显式 SessionConfig，不执行模型或工具。Session owner 不可 Clone，控制当前材料、批次领取、forget 和 shutdown；可 Clone 的 SessionHandle 提供 submit/observe/wait/cancel。

每个新键的有序 blocks 原子追加一次；保留键先比较完整原参数，再检查 Busy、关闭、故障、容量和编辑条件。相同参数返回原 WorkReceipt，不同参数结构化拒绝并返还原请求。新工作分配 UUIDv7 TurnId；空 blocks 可以运行现有材料。每个 Session 最多一个活动工作，无后台提交队列。

装配回调接收本次 WorkRef 与 RunControl，返回实际使用的 runner/options。工作期限从受理开始，覆盖装配和执行。普通取消让 Causa 交回实际 Context、typed TurnResult 和未提交 ToolBatch；发布 Finished 前先归还材料。终态以 Arc 共享，后续编辑和新工作不改旧结果；owner 可以独占领取未提交批次一次。

`with_context` 只允许空闲时短暂同步访问，与受理共用锁，闭包不能重入 Session 或执行阻塞 I/O。同步闭包成功编辑不会因随后返回 Err 回滚；panic 使会话故障。worker panic/异常终止由仍存活的 supervisor 发布 Faulted，丢失材料明确返回 ContextUnavailable。监督和正常收尾要求 Tokio runtime 持续存活；提交要求启用时间驱动。无时间驱动的配置错误会被捕获并拒绝，但进程 panic hook 仍可能输出诊断。

`wait` 超时只返回当前状态，不取消工作。`shutdown` 关闭新键、请求取消并等待正常收尾；Drop 请求取消但不等待。没有自动恢复或跨进程工作去重。

## 保存与资源

`material::save/load` 保存和读回原 Context，检查 JSON 与 BlockId 唯一性。写入使用同目录临时文件、同步内容后原子替换；父目录必须存在。它不提供跨文件资产事务或掉电后目录项持久性保证。

默认拒绝媒体和 artifact 引用。`save_with_references/load_with_references` 要求应用回调实际确认每个引用可读，覆盖 Parts、ToolResult.media 和 ToolOutput.artifact；导入时重新验证。保存层不猜测 opaque ID 是文件路径或 URL，也不自动复制资产。长期资产保留由应用负责。

保留成本包括当前 Context、每项工作的原提交 blocks 副本、Arc 终态中的输出/失败输入、未领取 ToolBatch，以及调用方持有的 Arc 或显式材料副本。容量只限制工作记录数量，不限制总字节数；没有自动淘汰。owner `forget` 释放终态记录、键和未领取批次，不改当前材料，已有观察/已登记等待继续持有原结果。原键可再次受理为新工作。

## 本地检查

```sh
cargo fmt --all --check
cargo clippy --workspace --locked --all-targets -- -D warnings
cargo test --workspace --locked
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --locked --no-deps
cargo build --workspace --locked --release
./target/release/tarz demo
```

测试使用脚本 gateway、本地工具、通知/屏障与临时文件，不需要在线模型。依赖已缓存时可追加 `--offline`。当前本机验证及 S1–S15 的逐项证据见 [实施记录](../.agents/reviews/2026-10-02-single-session-foundation/README.md)。CI 配置涵盖 Linux、macOS、Windows，当前本地证据不代表远端各平台已运行。

## 开发流程入口

当前使用 Direct/Noted 本地提案流程；机器规则在 `AGENTS.local.md`，完整流程见 [.agents/pipelines/WORKFLOW.md](../.agents/pipelines/WORKFLOW.md)。设计与进度统一在 [.agents](../.agents/README.md) 维护。该目录和 scripts 被 Git 忽略，不包含在应用发布内容中。

文档/提案变更运行 `node scripts/verify-proposals.mjs --links`；校验器变更另运行 `node scripts/test-verify-proposals.mjs`。这两项不替代 Rust 或应用安装验收。
