# Tarz

Tarz 是基于 Causa 的 Agent 应用，以可安装、可运行程序交付，不发布到 crates.io。

当前为项目骨架：支持帮助与版本输出，已接入 registry Causa 0.0.2 的 runtime 依赖。交互会话、模型配置、工具执行与材料保存尚未实现。

根目录为 Cargo workspace，包统一放在 `crates/`；当前应用包为 `crates/tarz/`。依赖锁文件与构建产物目录位于 workspace 根目录。

## 本地构建与运行

使用 Rust 1.96.0（由 `rust-toolchain.toml` 固定）：

```sh
cargo build --locked
cargo run --locked -- --help
cargo run --locked -- --version
cargo build --locked --release
./target/release/tarz --help
```

Windows 使用 `target\release\tarz.exe`。不带参数显示帮助；不支持的参数返回退出码 2。
当前开发版本为 `0.0.0`，尚无公开发布或安装渠道。

## 项目入口

- [当前结构与检查](docs/README.md)
- [本地设计与实施入口](.agents/README.md)
- [首阶段单会话方案](.agents/pipelines/proposals/architecture/2026-10-02-single-session-work-loop.md)
- [行为验收](.agents/pipelines/assets/single-session/acceptance.md)

Causa 负责 Context 和单次执行；Tarz 负责会话、工作受理、依赖装配及应用交互。

开发流程资料 `.agents/` 和辅助 `scripts/` 留在本机、Git 忽略；上面的设计与验收链接供本地开发使用，不随应用分发。
