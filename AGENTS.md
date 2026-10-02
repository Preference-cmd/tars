# AGENTS.md — Tarz

Tarz 是可安装、可运行的 Agent 应用，目标目录为 `~/project/tarz/`。
不发布 crates.io；所有 Cargo packages 必须保留 `publish = false`。

## 阅读入口

- [README.md](README.md)：构建、运行与当前可用能力。
- [docs/README.md](docs/README.md)：当前代码结构与验证命令。
- [.agents/README.md](.agents/README.md)：本地设计提案、来源与验收进度。
- [.agents/pipelines/WORKFLOW.md](.agents/pipelines/WORKFLOW.md)：Direct/Noted、提案生命周期及本地跟踪规则。

`CLAUDE.md` 指向本文件；机器流程补充在 Git 忽略的 `AGENTS.local.md`。

## 工作约定

- 根目录为 Cargo workspace，包统一放在 `crates/`；当前 `crates/tarz/` 提供 binary 和内部 lib target，供应用与集成测试共用，不承诺第三方 SDK。`application` 是 CLI/TUI/GUI 共用的配置与执行模块，当前 CLI 提供 `run` 和离线 `demo`；Causa 0.0.2 启用 runtime + providers。Session、装配与应用策略归 Tarz，Causa 提供材料和单次执行。
- 通过 registry 的 Causa 公开 facade 接线，不以 path/patch 或复制源码替代。
- 设计建议与已确认语义分开；实现前从 `.agents/README.md` 进入 Tarz 主提案与相关契约/验收，不把骨架检查视为行为验收。
- 模块采用 `foo.rs` 与 `foo/`；索引文件只保留文档、声明和重导出。
- 禁止 unsafe；新增依赖必须说明用途。提交使用 Conventional Commits。
- 当前事实放 `docs/`，提案与历史证据放本地 `.agents/`，避免两处维护同一进度。流程资料和辅助 `scripts/` 不进入应用发布。
- 验证：`cargo fmt --all --check`、`cargo clippy --workspace --locked --all-targets -- -D warnings`、`cargo test --workspace --locked`、`cargo build --workspace --locked --release`。
- 安装渠道、支持平台和发布自动化仍待确定；不将本地构建当作公开发布。
