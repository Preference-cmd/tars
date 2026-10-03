# TUI 原型

`tarz-tui` 是 Ratatui + Crossterm 的原生终端消费者，共享 `SessionHost` 与 [TOML 配置](configuration.md)。目前是开发原型：会话创建/打开、当前材料、最终结果、多行输入、取消、保存及恢复操作已接通；没有逐 token 展示、GUI、工具配置或安装分发。

```sh
cargo build -p tarz-tui --locked
./target/debug/tarz-tui --config /path/to/config.toml
# 不使用全局配置，从新建表单填写模型：
./target/debug/tarz-tui --no-config --store ./tarz-data
```

stdin 和 stdout 都必须是交互终端。至少 30×12；创建表单建议 80×24 以上。新建会话需要 provider/model 和可解析的凭据环境变量，创建本身不调用模型。F3 重开已存会话时采用原固定定义，不套用新的全局模型预设。

| 操作 | 按键 |
| --- | --- |
| 新建/填写模型配置 | F2；Tab/Shift-Tab 换字段，Ctrl-S 创建，Esc 返回 |
| 打开选中记录 | F3 |
| 切换会话 / 刷新记录列表 | F4 / F5 |
| 换行 / 提交输入 | Enter / Ctrl-S |
| 编辑 / 撤销、重做 | 方向键、Backspace、Delete / Ctrl-Z、Ctrl-Y |
| 滚动当前材料 | PageUp / PageDown |
| 取消当前工作 | Ctrl-C |
| 保存并关闭当前会话 / 保存并退出 | Ctrl-W / Ctrl-Q |
| 重试原保存 / 导出实际材料 | Ctrl-R / Ctrl-E |
| 放弃本地未确认内容并关闭 | Ctrl-D，再明确 y 确认；n 或 Esc 取消 |

会话各自保留未提交草稿。切换视图和重绘不取消工作、不重复提交；粘贴只插入文本，控制字符会被过滤。Ctrl-Q/关闭遇到草稿先确认；未提交草稿不属于自动保存的 Context。执行和保存状态分别显示，Completed 不等于 Saved。历史材料按内容显示，不恢复旧工作表或执行旧工具。

保存失败时保留 owner 和写锁，并暂停退出；可以修复目录后 Ctrl-R，或 Ctrl-E 导出至配置的恢复目录（默认当前目录），再选择下一步。导出生成 `tarz-recovery-<uuid>.json`，不覆盖已有文件；导出失败独立报告。Ctrl-D 不撤销已落盘记录，关闭报告保留原提交是否已应用/未应用/未尝试/仍无法确认的事实。

终端读取/绘制错误或可捕获 panic 会恢复终端模式，然后尝试保存；保存失败时尝试导出实际材料，最后等待资源释放并以 1 退出，失败细节写 stderr。这是失去交互面的异常清理路径。不可捕获强退不保证清理和未保存内容可恢复。

渲染只使用缓存的可见行。材料版本变化时读取一次完整 Context，在后台生成文本和换行索引；终端变宽/变窄时重新计算索引，输入事件仍可处理。任务最多 64 个，核心另有活动实例和存储并发限制；这些计数不构成总内存预算。当前列表保留本次进程已见到的记录，F5 补入新记录，打开时重新验证实际记录。

输入组件当前使用 ratatui-textarea 0.9.2，普通左右移动、Backspace/Delete 适配了组合字符/emoji 字素边界；这不是完整 IME 编辑器保证。PTY 已覆盖 UTF-8、多行粘贴、resize、取消/关闭/保存故障和终端恢复，实际输入法候选窗、不同终端的字宽及 Linux/Windows 终端仍需人工/平台验证。远端 CI 未在本轮执行，不能把本机构建当作跨平台支持证据。

本机 debug 测量中 100 KiB/1 MiB/8 MiB 和 8×1 MiB 会话的缓存绘制 p95 约 1–3 ms，30 次绘制没有额外读取完整 Context。8 MiB 重新换行约 1.2 秒，属于后续优化点；数据只代表该次本机实验，详见[原型验证记录](../.agents/reviews/2026-10-04-tui-prototype/README.md)。
