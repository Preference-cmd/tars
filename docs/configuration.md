# 共享 TOML 配置

CLI 与 TUI 共用 `tarz-core::config`。配置在进程启动时读取为不可变默认值，用于新会话；已保存会话重开时仍采用保存的 `SessionDefinition`。修改 TOML 不热改活动会话，也不重写已有记录。

配置文件选择顺序：

1. `--config FILE`。
2. 环境变量 `TARZ_CONFIG`。
3. Unix/macOS：绝对路径 `$XDG_CONFIG_HOME/tarz/config.toml`，未设置时为 `~/.config/tarz/config.toml`；Windows：`%APPDATA%/Tarz/config.toml`。

不会自动搜索当前项目的配置文件。默认路径不存在时使用空配置；显式路径、`TARZ_CONFIG` 指向的文件不存在或配置无效时拒绝启动。`--no-config` 忽略文件及环境选择，与 `--config` 互斥。配置加载/参数错误返回退出码 2，帮助和离线 demo 不读取配置。

参照根目录的 [config.example.toml](../config.example.toml)：

```toml
version = 1
store = "data"
recovery_dir = "."
default_profile = "work"

[profiles.work]
provider = "openai-chat"
model = "your-model-id"
api_key_env = "OPENAI_API_KEY"
# endpoint = "http://127.0.0.1:8080/v1/chat/completions"
timeout_secs = 120
capacity = 256
max_model_rounds = 10
max_tool_calls = 64
```

`version` 必须为 1；`store`、`recovery_dir`、`default_profile` 和 `profiles` 可省略。每个模型预设必须包含 provider 与非空 model。provider 为 `openai-chat`、`openai-responses` 或 `anthropic`。其余字段省略时采用表中默认：期限 120 秒、保留工作 256 项、每次工作最多 10 次模型调用和 64 个工具声明；`api_key_env` 随协议默认为 OPENAI_API_KEY 或 ANTHROPIC_API_KEY。临时 `tarz run` 只保留其单次工作，容量固定为 1。

所有预设在加载时校验，未知字段、重复键、无效类型、无效预设、空路径和不支持的版本都报错。文件最多 1 MiB。解析诊断只显示行列，不回显 TOML 值；`api_key` 明文字段不被接受。真正凭据在打开会话时从指定环境变量读取。

路径不展开 `~` 或 `$VAR`。相对 `store` 和 `recovery_dir` 以 TOML 所在目录为基准；命令行相对路径以当前工作目录为基准。store 会按现有 Store 规则创建，recovery_dir 必须已存在。没有配置 store 时，持久 CLI 与 TUI 都需要显式 `--store`。

```sh
tarz run --config ./config.toml --profile work --input '分析当前问题'
tarz session create --config ./config.toml --model another-model --json
tarz session list --config ./config.toml --json
tarz-tui --config ./config.toml --profile work
```

选择 `--profile NAME` 优先于 default_profile。参数按字段覆盖所选预设；没有覆盖的 endpoint、凭据引用、期限和限制仍沿用预设。CLI 模型参数用于 `run` 与 `session create`；`session run` 不接受 `--profile` 或模型覆盖。`--store`、`--recovery-dir` 覆盖配置中的相应路径。

TUI 新建表单从所选预设填充，修改只影响本次新会话，预设中未显示的容量/调用限制原样保留。首版配置由用户编辑文件，TUI 不自动保存或覆盖全局配置。后续 GUI 应复用相同加载与解析模块。
