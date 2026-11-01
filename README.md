# TARS

TARS 是 Causa 的验证项目：用一个真实的 Agent 应用检验 Causa 的公开契约，再把使用中发现的问题反馈给 Causa，推动它改进。验证为主，应用是实现验证的载体。TARS 以可安装、可运行的程序交付，不发布到 crates.io。

## 目的

- **验证。** Causa 只提供上下文材料的契约和单次执行，会话、保存、工具策略和界面都留给宿主。TARS 就是这样一个宿主，在真实条件下使用这些契约：持久会话、取消后继续、工作区工具与写入确认，以及在三种协议的真实端点上运行。它要回答的是：这些契约放进真实应用里够不够用、行为对不对。
- **改进。** 发现的问题先判断属于 TARS、Causa 还是两者的接线。属于 Causa 的，在 Causa 仓库按它的规则修改并发布，TARS 再升级依赖接入，并重跑发现问题的那项检查。例如 Causa 0.0.7 的 Chat 输出上限字段来自 TARS 的真实端点测试，0.1.2 的工具提示与处理器数据来自 TARS 的写入工具设计。
- **应用是载体。** TARS 是能实际使用的 CLI 和 TUI 应用，因为只有真实使用才能产生有效的证据。安排工作时，优先做能检验 Causa 的能力；只为分发服务的工作（安装渠道、GUI、更多平台）排在后面。

边界：

- TARS 只经 Causa 的公开 facade 使用 crates.io 上已发布的版本，不复制或引入 Causa 源码，所以验证的就是外部用户实际拿到的东西。
- 改进 Causa 时不把 TARS 自己的策略放进去。例如权限模式归 TARS，Causa 只携带工具声明的提示和每轮的宿主数据，模式的词汇和决定仍由 TARS 定。
- TARS 的测试通过，只说明这一处接线可行，不能代替 Causa 自己的测试。

## 当前状态

当前已实现单会话工作闭环，并提供离线 CLI 演示：原子提交、同键重放、逐工作装配、取消与结果观察、材料编辑及保存/导入。演示使用脚本模型和真实本地工具，经过 Session 与 Causa runner 执行，不需要凭据或网络。

`tars run` 支持真实 provider 的单次文本执行、JSON 输出、取消/期限和显式材料保存；`tars session create/run/list` 通过共享 SessionHost 自动保存固定定义与最新材料，并可重开同一会话。`tars-tui` 提供原生终端原型与临时流式正文预览，两端共用 TOML 配置，并可显式选择工作区和工作区工具。MCP 独立配置文件 `mcp.json`（支持 `${VAR}` 展开与字面密钥检查）、预设/CLI/TUI 选择、定义快照和运行期工具已接通（只接通工具，不含 resources/prompts/sampling），MCP 调用按 `mcp.json` 的 `approval` 设置和当前权限模式执行、确认（终端或 TUI 确认面板）或拒绝，真实第三方 MCP 服务验证待补；内置 `run_command` 在 macOS Seatbelt 沙箱中执行（默认无网络，工作区可写、`.git` 钩子与配置只读、凭据位置不可读写），单条命令可带理由请求在沙箱外运行，按权限模式询问或直接执行；TUI `/status` 与 CLI `session create` 显示沙箱后端和能力，Linux/Windows 暂无该工具；GUI、父子 Agent 和安装渠道尚未实现；CLI 与 TUI 可以写入文件：`ask` 模式在终端/确认面板确认，非终端时拒绝；`accept-edits` 和 `full-access` 不询问文件写入，命令的沙箱外请求在 `accept-edits` 下仍询问；`tars-tui` 已接通写入确认面板、命令确认面板、`/mode` 命令和 `Shift-Tab` 模式切换。当前开发版本为 `0.0.0`，尚无公开发布。

## 本地构建与运行

使用 Rust 1.96.0（由 `rust-toolchain.toml` 固定）：

```sh
cargo build --locked
cargo run --locked -- --help
cargo run --locked -- demo
cargo run --locked -- demo --save materials.json
cargo build --locked --release
./target/release/tars demo
```

Windows 使用 `target\release\tars.exe`。不带参数显示帮助；参数错误返回退出码 2，执行或文件错误返回 1。演示运行时 Ctrl-C 请求取消当前工作并等待收尾。

`demo` 执行 T1、摘要编辑、精确重放、T2，再保存材料并创建新 Session 继续执行。默认使用临时文件，结束后清理；`--save` 将材料写到指定文件，原文件会被原子替换，父目录必须已存在。文件只包含 Context，导入保留 BlockId，但不会恢复旧工作、请求键或外部副作用。应用需自行保存所需历史；未保存而被删除的材料无法恢复。

## 单次模型执行

先在当前 shell 设置 `OPENAI_API_KEY`（OpenAI 协议）或 `ANTHROPIC_API_KEY`（Anthropic），再显式指定模型 ID：

```sh
./target/release/tars run --provider openai-responses --model "$MODEL_ID" --input '解释这段设计' --save materials.json
printf '继续分析材料' | ./target/release/tars run --provider openai-responses --model "$MODEL_ID" --stdin --load materials.json --save materials.json --json
```

`MODEL_ID` 由你设置为服务端可用模型。协议选项为 `openai-chat`、`openai-responses`、`anthropic`；`--endpoint` 覆盖完整 URL（包括路径），`--api-key-env` 指定其他凭据环境变量名。凭据不会由配置层写入材料或结果报告。默认工具目录为空；使用 `--workspace DIR --tools list_directory,read_file,search_text,write_file,edit_file,run_command` 启用工作区工具能力，仍仅输出最终结果。`run_command` 默认在沙箱内启动 `/bin/bash -c`，命令超时默认 120 秒、最大 600 秒；`--command-network none|open` 选择默认拒绝或放行出站网络。命令可带 `outside_sandbox` 与理由请求在沙箱外运行，按权限模式确认或直接执行。写入已有文件前必须先 `read_file`；`--permission-mode` 设置初始权限模式，`ask` 在非终端直接拒绝写入。访问范围、确认、限额和旧记录导出见[工作区工具](docs/workspace-tools.md)。

输入必须二选一：`--input <text>` 或 `--stdin`（读到 EOF，UTF-8，不能全空白）。`--timeout-secs` 默认 120，范围 1–86400，从工作受理起覆盖装配和执行；不覆盖读取 stdin、加载或保存文件。

普通模式 stdout 只有完成文本，诊断走 stderr。`--json` 输出 `status`、`text`、`conversation_id`、`work_id`、`saved` 和 `error`；status 为 `not_started`、`completed`、`failed`、`cancelled` 或 `timed_out`。模型调用失败或以不认识的停止值结束时另有 `error_kind`（`transient`、`timed_out`、`cancelled`、`invalid_request`、`unknown_outcome`、凭据被拒的 `credential_rejected`、模型或路径不存在的 `not_found`、其他拒绝的 `permanent`、服务已应答但响应无法读取的 `undecodable`、本版本不认识的失败类别 `unclassified`，或服务返回了本版本不认识的停止值时的 `unrecognized_stop`，此时 `error_detail` 是该停止值）和 `error_detail`（网关给出的原始说明，可能含服务端返回的文本），其余情况两者为 null；普通模式下 `error_detail` 以 `TARS: detail: …` 写入 stderr。参数错误只走 stderr。执行结果和文件结果分开：模型完成但保存失败时仍为 `completed`，同时 `saved=false`、`error` 非空且退出码为 1。判断命令整体成功应检查退出码。

退出码：0 完成；1 配置、执行或文件失败；2 参数错误；124 工作期限；130 取消。Ctrl-C 在执行期间请求取消并等待真实材料交还；`--save` 在成功、执行失败或受控中断后保存当前材料，没有该选项则不自动保存。它不保存未提交输出或未提交工具批次，也不恢复旧工作或请求键。默认材料接口拒绝外部媒体/资产引用。

这些路径由本地 HTTP fixture 和实际进程测试验证，不需要线上凭据；另已完成用户配置服务的最小请求、三工具往返和带确认写入的 BA09 三协议实测。线上样本不代替中断/竞态验证，公开安装尚未验收。

## 保存并重开会话

```sh
# 首次创建，输出 ConversationId；创建本身不请求模型
./target/release/tars session create --store ./tars-data \
  --provider openai-chat --model "$MODEL_ID" --json

# 使用输出的 ID；provider/model/期限沿用保存定义
./target/release/tars session run "$SESSION_ID" --store ./tars-data \
  --input '继续这个会话' --json
./target/release/tars session list --store ./tars-data --json
```

`--store` 由参数或共享配置指定。记录保存固定定义和最新 Context；重开保持 ConversationId 与 BlockId，但不恢复旧工作、请求键或未提交输出。环境变量凭据在每次打开时重新解析，记录只保存变量引用。定义默认 120 秒期限、256 项保留工作、10 次模型调用和 64 个工具声明；详见 `tars session --help`。

`session run` 在执行成功、失败或取消后等待自动保存。JSON 分开返回执行 `status/text/execution_error/error_kind/error_detail` 与 `saved/revision/save_error`，并包含 `recovery_file/recovery_importable/recovery_error/close_error/close_outcome` 与 `save_wait_abandoned`。保存失败时导出当前材料到 `tars-recovery-<uuid>.json` 后以 1 退出；默认导出至当前目录，可用 `--recovery-dir` 指定已存在的目录。保存等待期间第一次 Ctrl-C 提示，第二次停止等待并按同一失败路径导出。导出失败单独报告，不能宣称材料已恢复。恢复文件可通过创建命令的 `--load` 导入为新会话；超过 64 MiB 时文件仍完整写出，但当前版本不能直接导入，结果会明确报告。关闭会确认原待保存操作；`close_outcome` 明确区分已提交、未提交与仍无法确认。若关闭时确认先前结果不明的提交，`saved/revision` 更新为已确认事实，原 `save_error` 和错误退出仍保留。

存储采用本机稳定文件锁：另一进程占用同一会话时明确拒绝。坏记录在列表中单列并使命令失败，不自动覆盖。单条记录最多 64 MiB；该大小不是进程内存预算。当前范围不包含掉电持久性、网络共享盘或跨主机写入，详情见[当前边界](docs/README.md)。

取消后有未配对工具声明时，新输入会原样保留并提示处理材料。TUI 用 F7 或 `/repair` 预览采纳原结果或移除未配对声明，再 Enter 应用并保存。CLI 用 `session repair ID --store DIR --json` 查看，显式 `--apply --expected-revision N` 应用移除模式。处理不重跑旧工具，原工作状态保持；保存后另行发送新输入。条件、重开与原批次保留范围见[中断后继续](docs/session-continuation.md)。

## 项目入口

根目录是 Cargo workspace，目录名省略 `tars-` 前缀：`crates/core/`（包 `tars-core`）提供共享应用核心与配置，`crates/sandbox/`（`tars-sandbox`）提供平台沙箱策略、后端与进程控制，`crates/cli/`（`tars-cli`）提供 `tars`，`crates/tui/`（`tars-tui`）提供 `tars-tui` 原型。默认 `cargo run/build` 仍选择 CLI；TUI 使用 `cargo run -p tars-tui -- --config /path/to/config.toml`，全部构建用 `cargo build --workspace --locked`。入口各自单向依赖核心，`tars-core` 单向依赖库包 `tars-sandbox` 并以 `pub use tars_sandbox as sandbox` 保留 `tars_core::sandbox` 路径，CLI/TUI 不直接依赖它；所有包 `publish = false`。Causa 以 crates.io 上发布的 `causa = "0.1"` 接入，启用 runtime 与 providers，具体版本由 Cargo.lock 固定；构建不再需要同级目录的 Causa checkout。独立分发另行处理。

CLI/TUI 共用 [TOML 配置](docs/configuration.md)，支持全局默认路径、`TARS_CONFIG`、`--config` 和命名模型预设；显式参数优先。可从 [config.example.toml](config.example.toml) 开始填写；MCP 服务器定义见 [mcp.example.json](mcp.example.json) 和[配置说明](docs/configuration.md#mcp-服务器)。已有会话重开不受全局模型预设变更影响。

- [当前结构、接口边界与检查](docs/README.md)
- [共享配置说明](docs/configuration.md)
- [工作区工具：读写、确认与记录迁移](docs/workspace-tools.md)
- [中断后的材料处理与继续](docs/session-continuation.md)
- [TUI 运行、按键与验证边界](docs/tui.md)
- [本地设计与实施入口](.agents/README.md)
- [TARS 应用设计](.agents/pipelines/proposals/architecture/2026-10-02-tars-agent-application.md)
- [行为验收](.agents/pipelines/assets/tars-agent/acceptance.md)

Causa 负责 Context 和单次执行；TARS 负责会话、工作受理、依赖装配及应用交互。

开发流程资料 `.agents/` 和辅助 `scripts/` 留在本机、Git 忽略；设计与验收链接供本地开发使用，不随应用分发。

## 许可证

本项目以 [MIT 许可证](LICENSE) 发布。
