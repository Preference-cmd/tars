# Tarz

Tarz 是基于 Causa 的 Agent 应用，以可安装、可运行程序交付，不发布到 crates.io。

当前已实现单会话工作闭环，并提供离线 CLI 演示：原子提交、同键重放、逐工作装配、取消与结果观察、材料编辑及保存/导入。演示使用脚本模型和真实本地工具，经过 Session 与 Causa runner 执行，不需要凭据或网络。

`tarz run` 支持真实 provider 的单次文本执行、JSON 输出、取消/期限和显式材料保存；`tarz session create/run/list` 通过共享 SessionHost 自动保存固定定义与最新材料，并可重开同一会话。`tarz-tui` 提供原生终端原型与临时流式正文预览，两端共用 TOML 配置，并可显式选择工作区和三种只读工具。GUI、文件写入、命令执行、MCP 配置、父子 Agent 和安装渠道尚未实现。当前开发版本为 `0.0.0`，尚无公开发布。

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

`MODEL_ID` 由你设置为服务端可用模型。协议选项为 `openai-chat`、`openai-responses`、`anthropic`；`--endpoint` 覆盖完整 URL（包括路径），`--api-key-env` 指定其他凭据环境变量名。凭据不会由配置层写入材料或结果报告。默认工具目录为空；使用 `--workspace DIR --tools list_directory,read_file,search_text` 启用只读能力，仍仅输出最终结果。访问范围、限额和旧记录导出见[工作区工具](docs/workspace-tools.md)。

输入必须二选一：`--input <text>` 或 `--stdin`（读到 EOF，UTF-8，不能全空白）。`--timeout-secs` 默认 120，范围 1–86400，从工作受理起覆盖装配和执行；不覆盖读取 stdin、加载或保存文件。

普通模式 stdout 只有完成文本，诊断走 stderr。`--json` 输出 `status`、`text`、`conversation_id`、`work_id`、`saved` 和 `error`；status 为 `not_started`、`completed`、`failed`、`cancelled` 或 `timed_out`。模型调用失败或以不认识的停止值结束时另有 `error_kind`（`transient`、`timed_out`、`cancelled`、`invalid_request`、`unknown_outcome`、凭据被拒的 `credential_rejected`、模型或路径不存在的 `not_found`、其他拒绝的 `permanent`、服务已应答但响应无法读取的 `undecodable`、本版本不认识的失败类别 `unclassified`，或服务返回了本版本不认识的停止值时的 `unrecognized_stop`，此时 `error_detail` 是该停止值）和 `error_detail`（网关给出的原始说明，可能含服务端返回的文本），其余情况两者为 null；普通模式下 `error_detail` 以 `Tarz: detail: …` 写入 stderr。参数错误只走 stderr。执行结果和文件结果分开：模型完成但保存失败时仍为 `completed`，同时 `saved=false`、`error` 非空且退出码为 1。判断命令整体成功应检查退出码。

退出码：0 完成；1 配置、执行或文件失败；2 参数错误；124 工作期限；130 取消。Ctrl-C 在执行期间请求取消并等待真实材料交还；`--save` 在成功、执行失败或受控中断后保存当前材料，没有该选项则不自动保存。它不保存未提交输出或未提交工具批次，也不恢复旧工作或请求键。默认材料接口拒绝外部媒体/资产引用。

这些路径由本地 HTTP fixture 和实际进程测试验证，不需要线上凭据；另已完成用户配置服务的最小请求与三工具往返实测。线上样本不代替中断/竞态验证，公开安装尚未验收。

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

`session run` 在执行成功、失败或取消后等待自动保存。JSON 分开返回执行 `status/text/execution_error/error_kind/error_detail` 与 `saved/revision/save_error`，并包含 `recovery_file/recovery_error/close_error/close_outcome`。保存失败时导出当前材料到 `tarz-recovery-<uuid>.json` 后以 1 退出；默认导出至当前目录，可用 `--recovery-dir` 指定已存在的目录。导出失败单独报告，不能宣称材料已恢复。恢复文件可通过创建命令的 `--load` 导入为新会话。关闭会确认原待保存操作；`close_outcome` 明确区分已提交、未提交与仍无法确认。若关闭时确认先前结果不明的提交，`saved/revision` 更新为已确认事实，原 `save_error` 和错误退出仍保留。

存储采用本机稳定文件锁：另一进程占用同一会话时明确拒绝。坏记录在列表中单列并使命令失败，不自动覆盖。单条记录最多 64 MiB；该大小不是进程内存预算。当前范围不包含掉电持久性、网络共享盘或跨主机写入，详情见[当前边界](docs/README.md)。

取消后有未配对工具声明时，新输入会原样保留并提示处理材料。TUI 用 F7 或 `/repair` 预览采纳原结果或移除未配对声明，再 Enter 应用并保存。CLI 用 `session repair ID --store DIR --json` 查看，显式 `--apply --expected-revision N` 应用移除模式。处理不重跑旧工具，原工作状态保持；保存后另行发送新输入。条件、重开与原批次保留范围见[中断后继续](docs/session-continuation.md)。

## 项目入口

根目录是 Cargo workspace；`crates/tarz-core/` 提供共享应用核心与配置，`crates/tarz-cli/` 提供 `tarz`，`crates/tarz-tui/` 提供 `tarz-tui` 原型。默认 `cargo run/build` 仍选择 CLI；TUI 使用 `cargo run -p tarz-tui -- --config /path/to/config.toml`，全部构建用 `cargo build --workspace --locked`。入口各自单向依赖核心，所有包 `publish = false`。本轮按用户授权通过 `../agent-stack-oss/crates/causa` 接入 Causa 最新开发实现，启用 runtime 与 providers；构建要求同级目录存在 Causa checkout，当前验证基线为 `9f955b4`。Cargo.lock 不固定本地路径中的源码提交。恢复 registry 依赖和独立分发另行处理。

CLI/TUI 共用 [TOML 配置](docs/configuration.md)，支持全局默认路径、`TARZ_CONFIG`、`--config` 和命名模型预设；显式参数优先。可从 [config.example.toml](config.example.toml) 开始填写。已有会话重开不受全局模型预设变更影响。

- [当前结构、接口边界与检查](docs/README.md)
- [共享配置说明](docs/configuration.md)
- [工作区只读工具与记录迁移](docs/workspace-tools.md)
- [中断后的材料处理与继续](docs/session-continuation.md)
- [TUI 运行、按键与验证边界](docs/tui.md)
- [本地设计与实施入口](.agents/README.md)
- [Tarz 应用设计](.agents/pipelines/proposals/architecture/2026-10-02-tarz-agent-application.md)
- [行为验收](.agents/pipelines/assets/tarz-agent/acceptance.md)

Causa 负责 Context 和单次执行；Tarz 负责会话、工作受理、依赖装配及应用交互。

开发流程资料 `.agents/` 和辅助 `scripts/` 留在本机、Git 忽略；设计与验收链接供本地开发使用，不随应用分发。
