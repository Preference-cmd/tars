//! Static shell completion script generator (`tars completions <bash|zsh|fish>`).

use super::command::output;
use super::configuration::CliError;
use std::ffi::OsString;
use std::process::ExitCode;
use tars_core::permission::escape_untrusted_text;

pub(super) const HELP: &str = "Generate static shell completion scripts

Usage: tars completions <bash | zsh | fish>

Arguments:
  bash | zsh | fish   Target shell to generate completions for

Options:
  -h, --help          Show this help
";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Shell {
    Bash,
    Zsh,
    Fish,
}

impl Shell {
    fn parse(raw: &str) -> Result<Self, CliError> {
        match raw {
            "bash" => Ok(Self::Bash),
            "zsh" => Ok(Self::Zsh),
            "fish" => Ok(Self::Fish),
            other => Err(CliError::argument(format!(
                "unsupported shell: {} (expected bash, zsh, or fish)",
                escape_untrusted_text(other)
            ))),
        }
    }
}

pub(super) struct CompletionsCommand {
    shell: Shell,
}

impl CompletionsCommand {
    pub(super) fn parse(args: &[OsString]) -> Result<Self, CliError> {
        let mut shell: Option<Shell> = None;
        for arg in args {
            let token = arg.to_str().ok_or("argument must be UTF-8")?;
            if token.starts_with('-') {
                return Err(CliError::argument(format!(
                    "unknown option: {}",
                    escape_untrusted_text(token)
                )));
            }
            if shell.is_some() {
                return Err(CliError::argument(format!(
                    "unexpected positional argument: {}",
                    escape_untrusted_text(token)
                )));
            }
            shell = Some(Shell::parse(token)?);
        }
        let shell = shell.ok_or_else(|| {
            CliError::argument("shell argument is required (expected bash, zsh, or fish)")
        })?;
        Ok(Self { shell })
    }
}

pub(super) fn run(command: CompletionsCommand) -> ExitCode {
    let script = match command.shell {
        Shell::Bash => BASH_SCRIPT,
        Shell::Zsh => ZSH_SCRIPT,
        Shell::Fish => FISH_SCRIPT,
    };
    output(script)
}

const BASH_SCRIPT: &str = r#"# bash completion for tars

_tars_completions() {
    local cur prev words cword
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev=""
    if [[ $COMP_CWORD -gt 0 ]]; then
        prev="${COMP_WORDS[COMP_CWORD-1]}"
    fi

    local top_commands="run session tools doctor completions"
    local top_flags="-p --print -c --continue -r --resume --stream --stream-json --output-format --json --image --config --no-config --profile --store --recovery-dir --workspace --tools --permission-mode --code-execution --no-code-execution --provider --model --api-key-env --endpoint --input --stdin --timeout-secs --max-tokens --reasoning-effort --extra-body --render-options --max-model-attempts --context-window --auto-compact --prompt-cache --command-network --mcp --system --system-file --dynamic-context --dynamic-context-file --skill-dir --no-skills --load --save --trust-workspace-rules --trust-workspace-agents -h --help -V --version"
    local session_commands="create run list show compact history repair rename delete fork export-materials export-markdown import-legacy"
    local tools_commands="status install"
    local shells="bash zsh fish"

    case "$prev" in
        --output-format)
            COMPREPLY=( $(compgen -W "text json stream-json" -- "$cur") )
            return 0
            ;;
        --permission-mode)
            COMPREPLY=( $(compgen -W "read-only ask accept-edits full-access" -- "$cur") )
            return 0
            ;;
        --provider)
            COMPREPLY=( $(compgen -W "openai-chat openai-responses anthropic" -- "$cur") )
            return 0
            ;;
        --command-network)
            COMPREPLY=( $(compgen -W "none open" -- "$cur") )
            return 0
            ;;
    esac

    local cmd=""
    local subcmd=""
    local i=1
    while [[ $i -lt $COMP_CWORD ]]; do
        local word="${COMP_WORDS[$i]}"
        case "$word" in
            --config|--profile|--store|--recovery-dir|--workspace|--tools|--permission-mode|--resume|--output-format|--image|--input|--provider|--model|--api-key-env|--endpoint|--timeout-secs|--max-tokens|--reasoning-effort|--extra-body|--render-options|--max-model-attempts|--context-window|--auto-compact|--prompt-cache|--command-network|--mcp|--system|--system-file|--dynamic-context|--dynamic-context-file|--skill-dir|--load|--save|--agent|--last|--title|--expected-revision|--record|--output)
                i=$((i + 2))
                continue
                ;;
            -*)
                i=$((i + 1))
                continue
                ;;
            *)
                if [[ -z "$cmd" ]]; then
                    cmd="$word"
                elif [[ -z "$subcmd" ]]; then
                    subcmd="$word"
                fi
                i=$((i + 1))
                ;;
        esac
    done

    case "$cmd" in
        session)
            if [[ -z "$subcmd" && "$cur" != -* ]]; then
                COMPREPLY=( $(compgen -W "$session_commands" -- "$cur") )
                return 0
            fi
            local session_flags="--store --config --no-config --json --agent --last --output --record --recovery-dir --input --stdin --image --stream --stream-json --output-format --permission-mode --trust-workspace-rules --trust-workspace-agents --title --clear --dry-run --apply --expected-revision --provider --model --profile --workspace --tools --code-execution --no-code-execution -h --help"
            COMPREPLY=( $(compgen -W "$session_flags" -- "$cur") )
            return 0
            ;;
        tools)
            if [[ -z "$subcmd" && "$cur" != -* ]]; then
                COMPREPLY=( $(compgen -W "$tools_commands" -- "$cur") )
                return 0
            fi
            if [[ "$subcmd" == "install" && "$cur" != -* ]]; then
                COMPREPLY=( $(compgen -W "rg" -- "$cur") )
                return 0
            fi
            COMPREPLY=( $(compgen -W "--json --yes --config --no-config -h --help" -- "$cur") )
            return 0
            ;;
        doctor)
            COMPREPLY=( $(compgen -W "--config --no-config --profile --store --recovery-dir --workspace --json -h --help" -- "$cur") )
            return 0
            ;;
        completions)
            if [[ -z "$subcmd" && "$cur" != -* ]]; then
                COMPREPLY=( $(compgen -W "$shells" -- "$cur") )
                return 0
            fi
            COMPREPLY=( $(compgen -W "-h --help" -- "$cur") )
            return 0
            ;;
        run)
            COMPREPLY=( $(compgen -W "$top_flags" -- "$cur") )
            return 0
            ;;
        *)
            if [[ "$cur" == -* ]]; then
                COMPREPLY=( $(compgen -W "$top_flags" -- "$cur") )
            else
                COMPREPLY=( $(compgen -W "$top_commands" -- "$cur") )
            fi
            return 0
            ;;
    esac
}

complete -F _tars_completions tars
"#;

const ZSH_SCRIPT: &str = r#"#compdef tars

_tars() {
    local -a top_commands session_commands tools_commands shells
    top_commands=(
        'run:Execute one transient work with a model'
        'session:Manage and run persistent sessions'
        'tools:Check or install external tools'
        'doctor:Run read-only environment and configuration diagnostics'
        'completions:Generate static shell completion scripts'
    )
    session_commands=(
        'create:Create a persistent session'
        'run:Run one turn on a saved session'
        'list:List sessions in a store'
        'show:Inspect a saved session or agent'
        'compact:Summarize and replace context'
        'history:Print expanded full conversation history'
        'repair:Inspect or remove unpaired tool declarations'
        'rename:Update or clear a session title'
        'delete:Remove an unlocked session from the store'
        'fork:Fork a session into an isolated copy'
        'export-materials:Export validated Context materials'
        'export-markdown:Export conversation as GitHub-Flavored Markdown'
        'import-legacy:Import a tars-session-v2.4 JSON record'
    )
    tools_commands=(
        'status:Show external tool and sandbox status'
        'install:Download and install managed tools'
    )
    shells=(
        'bash:Generate Bash completions'
        'zsh:Generate Zsh completions'
        'fish:Generate Fish completions'
    )

    local -a top_flags
    top_flags=(
        '-p[Run headlessly in print mode]'
        '--print[Run headlessly in print mode]'
        '-c[Continue most recently updated session]'
        '--continue[Continue most recently updated session]'
        '-r[Resume a saved session by ID, prefix, or open picker]::selector:'
        '--resume[Continue a saved session by ID, prefix, or open picker]::selector:'
        '--stream[Stream incremental response text to stdout]'
        '--stream-json[Stream NDJSON progress events and result to stdout]'
        '--output-format[Output format]:format:(text json stream-json)'
        '--json[Output structured JSON]'
        '--image[Attach an image file]:file:_files'
        '--config[Shared TOML configuration file]:file:_files'
        '--no-config[Ignore global configuration]'
        '--profile[Named model preset]:profile:'
        '--store[Session store directory]:dir:_files -/'
        '--recovery-dir[Recovery export directory]:dir:_files -/'
        '--workspace[Workspace root directory]:dir:_files -/'
        '--tools[Comma-separated workspace tools]:tools:'
        '--permission-mode[Permission mode]:mode:(read-only ask accept-edits full-access)'
        '--code-execution[Enable sandboxed run_code]'
        '--no-code-execution[Disable run_code]'
        '-h[Show help]'
        '--help[Show help]'
        '-V[Show version]'
        '--version[Show version]'
    )

    local cmd=""
    local subcmd=""
    local i=2
    while (( i < CURRENT )); do
        local word="${words[$i]}"
        case "$word" in
            --config|--profile|--store|--recovery-dir|--workspace|--tools|--permission-mode|--resume|--output-format|--image|--input|--provider|--model|--api-key-env|--endpoint|--timeout-secs|--max-tokens|--reasoning-effort|--extra-body|--render-options|--max-model-attempts|--context-window|--auto-compact|--prompt-cache|--command-network|--mcp|--system|--system-file|--dynamic-context|--dynamic-context-file|--skill-dir|--load|--save|--agent|--last|--title|--expected-revision|--record|--output)
                (( i += 2 ))
                continue
                ;;
            -*)
                (( i++ ))
                continue
                ;;
            *)
                if [[ -z "$cmd" ]]; then
                    cmd="$word"
                elif [[ -z "$subcmd" ]]; then
                    subcmd="$word"
                fi
                (( i++ ))
                ;;
        esac
    done

    case "$cmd" in
        session)
            if [[ -z "$subcmd" && "${words[CURRENT]}" != -* ]]; then
                _describe -t session-commands 'session subcommands' session_commands
                return
            fi
            _arguments -s \
                '--store[Local store directory]:dir:_files -/' \
                '--config[Shared TOML configuration file]:file:_files' \
                '--no-config[Ignore global configuration]' \
                '--json[Structured JSON output]' \
                '--agent[Agent ID or unique name]:agent:' \
                '--last[Newest N blocks]:count:' \
                '--output[New output file path]:file:_files' \
                '--record[Legacy record file]:file:_files' \
                '--recovery-dir[Recovery directory]:dir:_files -/' \
                '--input[User prompt text]:text:' \
                '--stdin[Read prompt from stdin]' \
                '--image[Attach an image file]:file:_files' \
                '--stream[Stream incremental text]' \
                '--stream-json[Stream NDJSON events]' \
                '--output-format[Output format]:format:(text json stream-json)' \
                '--permission-mode[Permission mode]:mode:(read-only ask accept-edits full-access)' \
                '--trust-workspace-rules[Trust workspace permissions.toml]' \
                '--trust-workspace-agents[Trust workspace agents]' \
                '--title[Session title]:title:' \
                '--clear[Clear session title]' \
                '--dry-run[Preview repair]' \
                '--apply[Apply repair]' \
                '--expected-revision[Expected revision]:rev:' \
                '-h[Show help]' \
                '--help[Show help]'
            ;;
        tools)
            if [[ -z "$subcmd" && "${words[CURRENT]}" != -* ]]; then
                _describe -t tools-commands 'tools subcommands' tools_commands
                return
            fi
            if [[ "$subcmd" == "install" && "${words[CURRENT]}" != -* ]]; then
                compadd -- rg
                return
            fi
            _arguments -s \
                '--json[Structured JSON output]' \
                '--yes[Confirm download without prompting]' \
                '--config[Shared TOML configuration file]:file:_files' \
                '--no-config[Ignore global configuration]' \
                '-h[Show help]' \
                '--help[Show help]'
            ;;
        doctor)
            _arguments -s \
                '--config[Shared TOML configuration file]:file:_files' \
                '--no-config[Ignore global configuration]' \
                '--profile[Named model preset]:profile:' \
                '--store[Session store directory]:dir:_files -/' \
                '--recovery-dir[Recovery export directory]:dir:_files -/' \
                '--workspace[Workspace root directory]:dir:_files -/' \
                '--json[Structured JSON output]' \
                '-h[Show help]' \
                '--help[Show help]'
            ;;
        completions)
            if [[ -z "$subcmd" && "${words[CURRENT]}" != -* ]]; then
                _describe -t shells 'shell' shells
                return
            fi
            _arguments -s '-h[Show help]' '--help[Show help]'
            ;;
        run)
            _arguments -s $top_flags
            ;;
        *)
            if [[ "${words[CURRENT]}" == -* ]]; then
                _arguments -s $top_flags
            else
                _describe -t commands 'tars commands' top_commands
            fi
            ;;
    esac
}

compdef _tars tars
"#;

const FISH_SCRIPT: &str = r#"# fish completion for tars

set -l top_cmds run session tools doctor completions
set -l session_cmds create run list show compact history repair rename delete fork export-materials export-markdown import-legacy
set -l tools_cmds status install

complete -c tars -f

complete -c tars -n "not __fish_seen_subcommand_from $top_cmds" -a run -d "Execute one transient work with a model"
complete -c tars -n "not __fish_seen_subcommand_from $top_cmds" -a session -d "Save and reopen persistent sessions"
complete -c tars -n "not __fish_seen_subcommand_from $top_cmds" -a tools -d "Check or install external tools"
complete -c tars -n "not __fish_seen_subcommand_from $top_cmds" -a doctor -d "Run read-only diagnostics"
complete -c tars -n "not __fish_seen_subcommand_from $top_cmds" -a completions -d "Generate shell completion script"

complete -c tars -n "not __fish_seen_subcommand_from $top_cmds; or __fish_seen_subcommand_from run" -s p -l print -d "Run headlessly in print mode"
complete -c tars -n "not __fish_seen_subcommand_from $top_cmds" -s c -l continue -d "Continue latest saved session"
complete -c tars -n "not __fish_seen_subcommand_from $top_cmds" -s r -l resume -d "Resume saved session by ID, prefix, or open picker"
complete -c tars -n "not __fish_seen_subcommand_from $top_cmds; or __fish_seen_subcommand_from run" -l stream -d "Stream incremental response text"
complete -c tars -n "not __fish_seen_subcommand_from $top_cmds; or __fish_seen_subcommand_from run" -l stream-json -d "Stream NDJSON progress events"
complete -c tars -n "not __fish_seen_subcommand_from $top_cmds; or __fish_seen_subcommand_from run" -l output-format -r -a "text json stream-json" -d "Output format"
complete -c tars -l json -d "Output structured JSON"
complete -c tars -n "not __fish_seen_subcommand_from $top_cmds; or __fish_seen_subcommand_from run" -l image -r -F -d "Attach a local image file"
complete -c tars -l config -r -F -d "Shared TOML configuration file"
complete -c tars -l no-config -d "Ignore global configuration"
complete -c tars -l profile -r -d "Named model preset"
complete -c tars -l store -r -a "(__fish_complete_directories)" -d "Session store directory"
complete -c tars -l recovery-dir -r -a "(__fish_complete_directories)" -d "Recovery export directory"
complete -c tars -l workspace -r -a "(__fish_complete_directories)" -d "Workspace root directory"
complete -c tars -l tools -r -d "Comma-separated workspace tools"
complete -c tars -l permission-mode -r -a "read-only ask accept-edits full-access" -d "Permission mode"
complete -c tars -l code-execution -d "Enable sandboxed run_code"
complete -c tars -l no-code-execution -d "Disable run_code"
complete -c tars -s h -l help -d "Show help"
complete -c tars -s V -l version -d "Show version"

complete -c tars -n "__fish_seen_subcommand_from session; and not __fish_seen_subcommand_from $session_cmds" -a create -d "Create a persistent session"
complete -c tars -n "__fish_seen_subcommand_from session; and not __fish_seen_subcommand_from $session_cmds" -a run -d "Run one turn on a saved session"
complete -c tars -n "__fish_seen_subcommand_from session; and not __fish_seen_subcommand_from $session_cmds" -a list -d "List sessions in a store"
complete -c tars -n "__fish_seen_subcommand_from session; and not __fish_seen_subcommand_from $session_cmds" -a show -d "Inspect a saved session or agent"
complete -c tars -n "__fish_seen_subcommand_from session; and not __fish_seen_subcommand_from $session_cmds" -a compact -d "Summarize and replace context"
complete -c tars -n "__fish_seen_subcommand_from session; and not __fish_seen_subcommand_from $session_cmds" -a history -d "Print expanded conversation history"
complete -c tars -n "__fish_seen_subcommand_from session; and not __fish_seen_subcommand_from $session_cmds" -a repair -d "Inspect or remove unpaired tool declarations"
complete -c tars -n "__fish_seen_subcommand_from session; and not __fish_seen_subcommand_from $session_cmds" -a rename -d "Update or clear session title"
complete -c tars -n "__fish_seen_subcommand_from session; and not __fish_seen_subcommand_from $session_cmds" -a delete -d "Delete an unlocked session"
complete -c tars -n "__fish_seen_subcommand_from session; and not __fish_seen_subcommand_from $session_cmds" -a fork -d "Fork a session into an isolated copy"
complete -c tars -n "__fish_seen_subcommand_from session; and not __fish_seen_subcommand_from $session_cmds" -a export-materials -d "Export validated Context materials"
complete -c tars -n "__fish_seen_subcommand_from session; and not __fish_seen_subcommand_from $session_cmds" -a export-markdown -d "Export conversation as Markdown"
complete -c tars -n "__fish_seen_subcommand_from session; and not __fish_seen_subcommand_from $session_cmds" -a import-legacy -d "Import a tars-session-v2.4 JSON record"

complete -c tars -n "__fish_seen_subcommand_from session" -l agent -r -d "Agent UUID or unique name"
complete -c tars -n "__fish_seen_subcommand_from session" -l last -r -d "Newest N blocks"
complete -c tars -n "__fish_seen_subcommand_from session" -l output -r -F -d "New output file path"

complete -c tars -n "__fish_seen_subcommand_from tools; and not __fish_seen_subcommand_from $tools_cmds" -a status -d "Show external tool and sandbox status"
complete -c tars -n "__fish_seen_subcommand_from tools; and not __fish_seen_subcommand_from $tools_cmds" -a install -d "Download and install managed tools"
complete -c tars -n "__fish_seen_subcommand_from tools; and __fish_seen_subcommand_from install" -a rg -d "Install managed ripgrep"
complete -c tars -n "__fish_seen_subcommand_from tools" -l yes -d "Confirm download without prompting"

complete -c tars -n "__fish_seen_subcommand_from completions; and not __fish_seen_subcommand_from bash zsh fish" -a "bash zsh fish" -d "Target shell"
"#;

#[cfg(test)]
mod tests {
    use super::CompletionsCommand;
    use std::ffi::OsString;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn completions_parser_accepts_supported_shells_and_rejects_invalid_args() {
        for shell in ["bash", "zsh", "fish"] {
            assert!(CompletionsCommand::parse(&os(&[shell])).is_ok());
        }
        for (args, expected) in [
            (
                vec![],
                "shell argument is required (expected bash, zsh, or fish)",
            ),
            (
                vec!["powershell"],
                "unsupported shell: powershell (expected bash, zsh, or fish)",
            ),
            (vec!["--bogus"], "unknown option: --bogus"),
            (
                vec!["bash", "extra"],
                "unexpected positional argument: extra",
            ),
        ] {
            let err = CompletionsCommand::parse(&os(&args))
                .err()
                .expect("completions parse must fail");
            assert!(err.is_argument(), "args={args:?}");
            assert!(
                err.message().contains(expected),
                "args={args:?}: expected '{expected}' in '{}'",
                err.message()
            );
        }
    }
}
