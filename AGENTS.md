# AGENTS.md — Tarz

Tarz 是可安装、可运行的 Agent 应用，目标目录为 `~/projects/tarz/`。
不发布 crates.io；所有 Cargo packages 必须保留 `publish = false`。

## 阅读入口

- [README.md](README.md)：构建、运行与当前可用能力。
- [docs/README.md](docs/README.md)：当前代码结构与验证命令。
- [.agents/README.md](.agents/README.md)：设计提案、来源与验收进度。

## 工作约定

- 当前单个 binary target；Session、装配与应用策略归 Tarz，Causa 提供材料和单次执行。
- 通过 registry 的 Causa 公开 facade 接线，不以 path/patch 或复制源码替代。
- 设计建议与已确认语义分开；实现前读单会话提案，不把骨架检查视为行为验收。
- 模块采用 `foo.rs` 与 `foo/`；索引文件只保留文档、声明和重导出。
- 禁止 unsafe；新增依赖必须说明用途。提交使用 Conventional Commits。
- 当前事实放 `docs/`，提案与历史证据放 `.agents/`，避免两处维护同一进度。
- 验证：`cargo fmt --all --check`、`cargo clippy --locked --all-targets -- -D warnings`、`cargo test --locked`、`cargo build --locked --release`。
- 安装渠道、支持平台和发布自动化仍待确定；不将本地构建当作公开发布。
