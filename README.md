# Tarz

Tarz 是基于 Causa 的 Agent 应用，以可安装、可运行程序交付，不发布到 crates.io。

当前已实现单会话工作闭环，并提供离线 CLI 演示：原子提交、同键重放、逐工作装配、取消与结果观察、材料编辑及保存/导入。演示使用脚本模型和真实本地工具，经过 Session 与 Causa runner 执行，不需要凭据或网络。

线上模型配置、交互式对话、父子 Agent 和安装渠道尚未实现。当前开发版本为 `0.0.0`，尚无公开发布。

## 本地构建与运行

使用 Rust 1.96.0（由 `rust-toolchain.toml` 固定）：

```sh
cargo build --locked
cargo run --locked -- --help
cargo run --locked -- demo
cargo run --locked -- demo --save materials.json
cargo build --locked --release
./target/release/tarz demo
```

Windows 使用 `target\release\tarz.exe`。不带参数显示帮助；参数错误返回退出码 2，执行或文件错误返回 1。演示运行时 Ctrl-C 请求取消当前工作并等待收尾。

`demo` 执行 T1、摘要编辑、精确重放、T2，再保存材料并创建新 Session 继续执行。默认使用临时文件，结束后清理；`--save` 将材料写到指定文件，原文件会被原子替换，父目录必须已存在。文件只包含 Context，导入保留 BlockId，但不会恢复旧工作、请求键或外部副作用。应用需自行保存所需历史；未保存而被删除的材料无法恢复。

## 项目入口

根目录是 Cargo workspace；`crates/tarz/` 包含可执行程序及内部 lib target，供应用与集成测试共用。所有包 `publish = false`，不承诺第三方 SDK。Causa 0.0.2 从 registry 消费，仅启用 runtime。

- [当前结构、接口边界与检查](docs/README.md)
- [本地设计与实施入口](.agents/README.md)
- [Tarz 应用设计](.agents/pipelines/proposals/architecture/2026-10-02-tarz-agent-application.md)
- [行为验收](.agents/pipelines/assets/tarz-agent/acceptance.md)

Causa 负责 Context 和单次执行；Tarz 负责会话、工作受理、依赖装配及应用交互。

开发流程资料 `.agents/` 和辅助 `scripts/` 留在本机、Git 忽略；设计与验收链接供本地开发使用，不随应用分发。
