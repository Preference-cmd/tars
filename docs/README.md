# 当前实现

2026-10-02：项目骨架已建立。只有命令行帮助、版本输出与参数错误处理；尚未启动模型或工具，也不创建会话、配置文件或后台任务。

## 代码结构

| 路径 | 当前职责 |
| --- | --- |
| `src/main.rs` | 进程入口与退出码 |
| `src/cli.rs`、`src/cli/command.rs` | 参数处理与标准输出/错误输出 |
| `Cargo.toml`、`Cargo.lock` | 不可发布的应用包与锁定依赖 |
| `.github/workflows/ci.yml` | Rust 格式、静态检查、测试与多平台构建配置 |

Causa 0.0.2 从 crates.io 消费，只启用 runtime。当前验证依赖解析与编译，尚未调用 runner。模型/provider、MCP、异步任务、Session 等在实际接线时引入，不放置空实现。

## 本地检查

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked --release
```

骨架尚无 Rust 行为测试；`cargo test` 成功只表明测试目标编译和运行成功，不代表单会话验收完成。CLI smoke 与依赖检查证据见 [.agents/reviews](../.agents/reviews/2026-10-02-bootstrap.md)。CI 配置涵盖 Linux、macOS、Windows，但远端运行结果尚不可用。

## 下一实现边界

Session 和逐工作装配将作为内部模块实现；内部是否拆为 lib target 按测试与调用需求决定，不承诺第三方 SDK。设计、待定项与进度统一在 [.agents](../.agents/README.md) 维护。
