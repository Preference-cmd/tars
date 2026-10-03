# Tarz

Tarz 是基于 Causa 的 Agent 应用，以可安装、可运行程序交付，不发布到 crates.io。

当前已实现单会话工作闭环，并提供离线 CLI 演示：原子提交、同键重放、逐工作装配、取消与结果观察、材料编辑及保存/导入。演示使用脚本模型和真实本地工具，经过 Session 与 Causa runner 执行，不需要凭据或网络。

`tarz run` 支持真实 provider 的单次文本执行、JSON 输出、取消/期限和显式材料保存；`tarz session create/run/list` 通过共享 SessionHost 自动保存固定定义与最新材料，并可重开同一会话。`tarz-tui` 提供原生终端原型，两端共用 TOML 配置。GUI、流式展示、工具配置、父子 Agent 和安装渠道尚未实现。当前开发版本为 `0.0.0`，尚无公开发布。

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

## 单次模型执行

先在当前 shell 设置 `OPENAI_API_KEY`（OpenAI 协议）或 `ANTHROPIC_API_KEY`（Anthropic），再显式指定模型 ID：

```sh
./target/release/tarz run --provider openai-responses --model "$MODEL_ID" --input '解释这段设计' --save materials.json
printf '继续分析材料' | ./target/release/tarz run --provider openai-responses --model "$MODEL_ID" --stdin --load materials.json --save materials.json --json
```

`MODEL_ID` 由你设置为服务端可用模型。协议选项为 `openai-chat`、`openai-responses`、`anthropic`；`--endpoint` 覆盖完整 URL（包括路径），`--api-key-env` 指定其他凭据环境变量名。凭据不会由配置层写入材料或结果报告。此次执行使用空工具目录，仅输出最终结果。

输入必须二选一：`--input <text>` 或 `--stdin`（读到 EOF，UTF-8，不能全空白）。`--timeout-secs` 默认 120，范围 1–86400，从工作受理起覆盖装配和执行；不覆盖读取 stdin、加载或保存文件。

普通模式 stdout 只有完成文本，诊断走 stderr。`--json` 输出 `status`、`text`、`conversation_id`、`work_id`、`saved` 和 `error`；status 为 `not_started`、`completed`、`failed`、`cancelled` 或 `timed_out`。参数错误只走 stderr。执行结果和文件结果分开：模型完成但保存失败时仍为 `completed`，同时 `saved=false`、`error` 非空且退出码为 1。判断命令整体成功应检查退出码。

退出码：0 完成；1 配置、执行或文件失败；2 参数错误；124 工作期限；130 取消。Ctrl-C 在执行期间请求取消并等待真实材料交还；`--save` 在成功、执行失败或受控中断后保存当前材料，没有该选项则不自动保存。它不保存未提交输出或未提交工具批次，也不恢复旧工作或请求键。默认材料接口拒绝外部媒体/资产引用。

这些路径已由本地 HTTP fixture 和实际进程测试验证，不需要线上凭据；尚未做真实付费模型调用或公开安装验收。

## 保存并重开会话

```sh
# 首次创建，输出 ConversationId；创建本身不请求模型
./target/release/tarz session create --store ./tarz-data \
  --provider openai-chat --model "$MODEL_ID" --json

# 使用输出的 ID；provider/model/期限沿用保存定义
./target/release/tarz session run "$SESSION_ID" --store ./tarz-data \
  --input '继续这个会话' --json
./target/release/tarz session list --store ./tarz-data --json
```

`--store` 由参数或共享配置指定。记录保存固定定义和最新 Context；重开保持 ConversationId 与 BlockId，但不恢复旧工作、请求键或未提交输出。环境变量凭据在每次打开时重新解析，记录只保存变量引用。定义默认 120 秒期限、256 项保留工作、10 次模型调用和 64 个工具声明；详见 `tarz session --help`。

`session run` 在执行成功、失败或取消后等待自动保存。JSON 分开返回执行 `status/text/execution_error` 与 `saved/revision/save_error`，并包含 `recovery_file/recovery_error/close_error/close_outcome`。保存失败时导出当前材料到 `tarz-recovery-<uuid>.json` 后以 1 退出；默认导出至当前目录，可用 `--recovery-dir` 指定已存在的目录。导出失败单独报告，不能宣称材料已恢复。恢复文件可通过创建命令的 `--load` 导入为新会话。关闭会确认原待保存操作；`close_outcome` 明确区分已提交、未提交与仍无法确认。若关闭时确认先前结果不明的提交，`saved/revision` 更新为已确认事实，原 `save_error` 和错误退出仍保留。

存储采用本机稳定文件锁：另一进程占用同一会话时明确拒绝。坏记录在列表中单列并使命令失败，不自动覆盖。单条记录最多 64 MiB；该大小不是进程内存预算。当前范围不包含掉电持久性、网络共享盘或跨主机写入，详情见[当前边界](docs/README.md)。

## 项目入口

根目录是 Cargo workspace；`crates/tarz-core/` 提供共享应用核心与配置，`crates/tarz-cli/` 提供 `tarz`，`crates/tarz-tui/` 提供 `tarz-tui` 原型。默认 `cargo run/build` 仍选择 CLI；TUI 使用 `cargo run -p tarz-tui -- --config /path/to/config.toml`，全部构建用 `cargo build --workspace --locked`。入口各自单向依赖核心，所有包 `publish = false`。Causa 0.0.2 从 registry 消费，启用 runtime 与 providers。

CLI/TUI 共用 [TOML 配置](docs/configuration.md)，支持全局默认路径、`TARZ_CONFIG`、`--config` 和命名模型预设；显式参数优先。可从 [config.example.toml](config.example.toml) 开始填写。已有会话重开不受全局模型预设变更影响。

- [当前结构、接口边界与检查](docs/README.md)
- [共享配置说明](docs/configuration.md)
- [TUI 运行、按键与验证边界](docs/tui.md)
- [本地设计与实施入口](.agents/README.md)
- [Tarz 应用设计](.agents/pipelines/proposals/architecture/2026-10-02-tarz-agent-application.md)
- [行为验收](.agents/pipelines/assets/tarz-agent/acceptance.md)

Causa 负责 Context 和单次执行；Tarz 负责会话、工作受理、依赖装配及应用交互。

开发流程资料 `.agents/` 和辅助 `scripts/` 留在本机、Git 忽略；设计与验收链接供本地开发使用，不随应用分发。
