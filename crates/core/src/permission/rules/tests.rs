use super::*;
use crate::policy::{
    PermissionDecisionKind, PermissionMode, PermissionVerdict, ToolPermissionClass, decide,
};
use std::path::Path;

fn rule(prefix: &[&str], decision: RuleDecision) -> CommandRule {
    CommandRule {
        prefix: prefix.iter().map(|s| (*s).to_owned()).collect(),
        decision,
        outside_sandbox: false,
    }
}

fn rule_outside(prefix: &[&str], decision: RuleDecision) -> CommandRule {
    CommandRule {
        prefix: prefix.iter().map(|s| (*s).to_owned()).collect(),
        decision,
        outside_sandbox: true,
    }
}

fn source(scope: RuleScope, prefix: &[&str]) -> RuleSource {
    RuleSource {
        scope,
        prefix: prefix.iter().map(|s| (*s).to_owned()).collect(),
    }
}

// --- RC01: conservative parser and prefix suggestions ---

#[test]
fn rc01_parses_supported_commands_and_connectors() {
    assert_eq!(
        parse_command("cargo test -p x"),
        Some(vec![vec![
            "cargo".into(),
            "test".into(),
            "-p".into(),
            "x".into(),
        ]])
    );
    assert_eq!(
        parse_command("git status && git diff"),
        Some(vec![
            vec!["git".into(), "status".into()],
            vec!["git".into(), "diff".into()],
        ])
    );
    assert_eq!(
        parse_command("cargo test 2>&1 | tail -n 20"),
        Some(vec![
            vec!["cargo".into(), "test".into()],
            vec!["tail".into(), "-n".into(), "20".into()],
        ])
    );
    assert_eq!(
        parse_command("echo 'a b'"),
        Some(vec![vec!["echo".into(), "a b".into()]])
    );
    assert_eq!(
        parse_command("  echo \"hello world\" ; pwd || true\n ls >/dev/null 2>/dev/null \n"),
        Some(vec![
            vec!["echo".into(), "hello world".into()],
            vec!["pwd".into()],
            vec!["true".into()],
            vec!["ls".into()],
        ])
    );
    // Word concatenation, quotes inside other quotes, and literal '$' inside single quotes
    assert_eq!(
        parse_command("--flag='a b' \"it's\" 'say \"hi\"' '$HOME'"),
        Some(vec![vec![
            "--flag=a b".into(),
            "it's".into(),
            "say \"hi\"".into(),
            "$HOME".into(),
        ]])
    );
    // Non-first '=' arguments are permitted; '~' mid-word is permitted
    assert_eq!(
        parse_command("cargo test --foo=bar && git show HEAD~1"),
        Some(vec![
            vec!["cargo".into(), "test".into(), "--foo=bar".into()],
            vec!["git".into(), "show".into(), "HEAD~1".into()],
        ])
    );
    // Quoted env-like first word is not an env assignment
    assert_eq!(
        parse_command("'X=1' cargo test"),
        Some(vec![vec!["X=1".into(), "cargo".into(), "test".into()]])
    );
    // Quoted empty string produces an empty word
    assert_eq!(
        parse_command("echo '' \"\""),
        Some(vec![vec!["echo".into(), "".into(), "".into()]])
    );
}

#[test]
fn rc01_rejects_unmatchable_constructs() {
    let rejected = [
        "",
        "   ",
        "\n\n",
        "echo $HOME",
        "echo \"$HOME\"",
        "echo `id`",
        "echo \"`id`\"",
        "echo \\n",
        "echo \"a\\nb\"",
        "ls > out",
        "ls >> out",
        "ls > /dev/null",
        "ls 1>/dev/null",
        "ls 2>&2",
        "ls 2>&1foo",
        "ls>/dev/null",
        "2>&1",
        ">/dev/null",
        "ls && 2>&1",
        "2>&1 | ls",
        "ls < in",
        "cat <<EOF\nhi\nEOF",
        "sleep 1 &",
        "(cd x && ls)",
        "{ ls; }",
        "ls *.rs",
        "ls ?.rs",
        "ls [a-z].rs",
        "cat ~/x",
        "cd ~",
        "X=1 cargo test",
        "FOO_2='bar' cargo test",
        "X= cargo test",
        "2>&1 X=1 cargo test",
        "ls # c",
        "# comment",
        "ls#c",
        "echo 'unclosed",
        "echo \"unclosed",
        "| ls",
        "ls |",
        "ls && && pwd",
        "ls ;",
        "; ls",
        "ls &&\npwd",
        "echo a\0b",
    ];
    for cmd in rejected {
        assert_eq!(
            parse_command(cmd),
            None,
            "command should not match: {cmd:?}"
        );
    }
}

#[test]
fn rc01_suggest_command_prefixes_covers_normal_flags_wrappers_and_dedup() {
    assert_eq!(
        suggest_command_prefixes("cargo test -p x && cargo test -p y && ls -la"),
        Some(vec![vec!["cargo".into(), "test".into()], vec!["ls".into()],])
    );
    assert_eq!(
        suggest_command_prefixes("echo ''"),
        Some(vec![vec!["echo".into()]])
    );
    assert_eq!(
        suggest_command_prefixes("bash -c 'echo hi'"),
        Some(vec![vec!["bash".into(), "-c".into(), "echo hi".into()]])
    );
    assert_eq!(
        suggest_command_prefixes("/bin/bash -c 'echo hi'"),
        Some(vec![vec![
            "/bin/bash".into(),
            "-c".into(),
            "echo hi".into(),
        ]])
    );
    // Single-word interpreter/wrapper cannot be suggested as an allow rule
    assert_eq!(suggest_command_prefixes("bash"), None);
    assert_eq!(suggest_command_prefixes("/usr/bin/env"), None);
    assert_eq!(suggest_command_prefixes("cargo test && sudo"), None);
    // Unmatchable or empty word yields None
    assert_eq!(suggest_command_prefixes("echo $HOME"), None);
    assert_eq!(suggest_command_prefixes("'' foo"), None);
    assert_eq!(suggest_command_prefixes("bash ''"), None);
}

// --- RC02: matching and three-layer merging ---

#[test]
fn rc02_prefix_matching_boundaries() {
    let rules = CommandRuleSet::with_layers(
        vec![],
        vec![rule(&["cargo", "test"], RuleDecision::Allow)],
        vec![],
    );

    assert_eq!(
        rules.evaluate("cargo test -p x", false),
        Some(RuleMatch {
            decision: RuleDecision::Allow,
            source: source(RuleScope::Workspace, &["cargo", "test"]),
        })
    );
    assert_eq!(rules.evaluate("cargo testx", false), None);
    assert_eq!(rules.evaluate("/usr/bin/cargo test", false), None);
    assert_eq!(rules.evaluate("cargo test && cargo fmt", false), None);
}

#[test]
fn rc02_multi_segment_and_precedence_rules() {
    let rules = CommandRuleSet::with_layers(
        vec![],
        vec![
            rule(&["cargo", "test"], RuleDecision::Allow),
            rule(&["rm", "-rf"], RuleDecision::Deny),
            rule(&["cargo"], RuleDecision::Ask),
        ],
        vec![],
    );

    // Deny overrides Allow across segments
    assert_eq!(
        rules.evaluate("cargo test; rm -rf x", false),
        Some(RuleMatch {
            decision: RuleDecision::Deny,
            source: source(RuleScope::Workspace, &["rm", "-rf"]),
        })
    );

    // Ask overrides Allow on same/other segments
    assert_eq!(
        rules.evaluate("cargo test", false),
        Some(RuleMatch {
            decision: RuleDecision::Ask,
            source: source(RuleScope::Workspace, &["cargo"]),
        })
    );
}

#[test]
fn rc02_cross_layer_merging_matrix() {
    // User deny/ask cannot be overridden by workspace or session allow
    let user_deny = CommandRuleSet::with_layers(
        vec![rule(&["rm"], RuleDecision::Deny)],
        vec![rule(&["rm", "-rf"], RuleDecision::Allow)],
        vec![rule(&["rm", "-rf"], RuleDecision::Allow)],
    );
    assert_eq!(
        user_deny.evaluate("rm -rf x", false),
        Some(RuleMatch {
            decision: RuleDecision::Deny,
            source: source(RuleScope::User, &["rm"]),
        })
    );

    let user_ask = CommandRuleSet::with_layers(
        vec![rule(&["git", "push"], RuleDecision::Ask)],
        vec![rule(&["git", "push"], RuleDecision::Allow)],
        vec![rule(&["git", "push"], RuleDecision::Allow)],
    );
    assert_eq!(
        user_ask.evaluate("git push origin main", false),
        Some(RuleMatch {
            decision: RuleDecision::Ask,
            source: source(RuleScope::User, &["git", "push"]),
        })
    );

    // Workspace deny/ask tightens user allow
    let ws_tightens_deny = CommandRuleSet::with_layers(
        vec![rule(&["cargo", "test"], RuleDecision::Allow)],
        vec![rule(&["cargo", "test"], RuleDecision::Deny)],
        vec![],
    );
    assert_eq!(
        ws_tightens_deny.evaluate("cargo test", false),
        Some(RuleMatch {
            decision: RuleDecision::Deny,
            source: source(RuleScope::Workspace, &["cargo", "test"]),
        })
    );

    let ws_tightens_ask = CommandRuleSet::with_layers(
        vec![rule(&["cargo", "test"], RuleDecision::Allow)],
        vec![rule(&["cargo", "test"], RuleDecision::Ask)],
        vec![],
    );
    assert_eq!(
        ws_tightens_ask.evaluate("cargo test", false),
        Some(RuleMatch {
            decision: RuleDecision::Ask,
            source: source(RuleScope::Workspace, &["cargo", "test"]),
        })
    );

    // Workspace and session allow supplement uncovered commands alongside user allow
    let combined = CommandRuleSet::with_layers(
        vec![rule(&["cargo", "test"], RuleDecision::Allow)],
        vec![rule(&["git", "status"], RuleDecision::Allow)],
        vec![rule(&["pwd"], RuleDecision::Allow)],
    );
    assert_eq!(
        combined.evaluate("cargo test && git status && pwd", false),
        Some(RuleMatch {
            decision: RuleDecision::Allow,
            source: source(RuleScope::User, &["cargo", "test"]),
        })
    );

    // Deterministic priority: User > Workspace > Session
    let priority = CommandRuleSet::with_layers(
        vec![rule(&["a"], RuleDecision::Deny)],
        vec![rule(&["b"], RuleDecision::Deny)],
        vec![rule(&["c"], RuleDecision::Deny)],
    );
    assert_eq!(
        priority.evaluate("c && b && a", false).unwrap().source,
        source(RuleScope::User, &["a"])
    );
    assert_eq!(
        priority.evaluate("c && b", false).unwrap().source,
        source(RuleScope::Workspace, &["b"])
    );
}

#[test]
fn rc02_outside_sandbox_allow_requires_every_segment_to_opt_in() {
    let rules = CommandRuleSet::with_layers(
        vec![rule_outside(&["cargo", "test"], RuleDecision::Allow)],
        vec![rule(&["git", "status"], RuleDecision::Allow)],
        vec![rule(&["rm", "-rf"], RuleDecision::Deny)],
    );

    // Sandboxed works for both
    assert_eq!(
        rules.evaluate("cargo test && git status", false),
        Some(RuleMatch {
            decision: RuleDecision::Allow,
            source: source(RuleScope::User, &["cargo", "test"]),
        })
    );
    // Outside sandbox: cargo test alone allowed
    assert_eq!(
        rules.evaluate("cargo test", true),
        Some(RuleMatch {
            decision: RuleDecision::Allow,
            source: source(RuleScope::User, &["cargo", "test"]),
        })
    );
    // Outside sandbox: second segment lacks outside_sandbox = true -> None
    assert_eq!(rules.evaluate("cargo test && git status", true), None);
    // Deny/Ask still match outside sandbox
    assert_eq!(
        rules.evaluate("rm -rf /tmp/x", true),
        Some(RuleMatch {
            decision: RuleDecision::Deny,
            source: source(RuleScope::Session, &["rm", "-rf"]),
        })
    );
}

// --- RC03: composition table across all 4 modes x 3 rule decisions x sandbox ---

#[test]
fn rc03_apply_command_rule_truth_table() {
    let src = source(RuleScope::Workspace, &["cargo", "test"]);

    for mode in [
        PermissionMode::ReadOnly,
        PermissionMode::Ask,
        PermissionMode::AcceptEdits,
        PermissionMode::FullAccess,
    ] {
        for class in [
            ToolPermissionClass::BuiltinCommand,
            ToolPermissionClass::CommandOutsideSandbox,
        ] {
            let base = decide(mode, class);

            // Deny -> Reject (RuleDenied) in every mode, including ReadOnly and FullAccess
            let denied = apply_command_rule(
                base,
                Some(RuleMatch {
                    decision: RuleDecision::Deny,
                    source: src.clone(),
                }),
            );
            assert_eq!(denied.verdict, PermissionVerdict::Reject);
            assert_eq!(denied.fixed_kind, Some(PermissionDecisionKind::RuleDenied));
            assert_eq!(denied.rule.as_ref(), Some(&src));

            // Ask -> Ask for Ask/Execute, Reject(ModeRefused) for ReadOnly
            let asked = apply_command_rule(
                base,
                Some(RuleMatch {
                    decision: RuleDecision::Ask,
                    source: src.clone(),
                }),
            );
            if mode == PermissionMode::ReadOnly {
                assert_eq!(asked.verdict, PermissionVerdict::Reject);
                assert_eq!(asked.fixed_kind, Some(PermissionDecisionKind::ModeRefused));
                assert_eq!(asked.rule, None);
            } else {
                assert_eq!(asked.verdict, PermissionVerdict::Ask);
                assert_eq!(asked.fixed_kind, None);
                assert_eq!(asked.rule.as_ref(), Some(&src));
            }

            // Allow -> Execute(RuleAllowed) from Ask, DirectExecution from Execute, ModeRefused from ReadOnly
            let allowed = apply_command_rule(
                base,
                Some(RuleMatch {
                    decision: RuleDecision::Allow,
                    source: src.clone(),
                }),
            );
            match base {
                PermissionVerdict::Reject => {
                    assert_eq!(allowed.verdict, PermissionVerdict::Reject);
                    assert_eq!(
                        allowed.fixed_kind,
                        Some(PermissionDecisionKind::ModeRefused)
                    );
                    assert_eq!(allowed.rule, None);
                }
                PermissionVerdict::Ask => {
                    assert_eq!(allowed.verdict, PermissionVerdict::Execute);
                    assert_eq!(
                        allowed.fixed_kind,
                        Some(PermissionDecisionKind::RuleAllowed)
                    );
                    assert_eq!(allowed.rule.as_ref(), Some(&src));
                }
                PermissionVerdict::Execute => {
                    assert_eq!(allowed.verdict, PermissionVerdict::Execute);
                    assert_eq!(
                        allowed.fixed_kind,
                        Some(PermissionDecisionKind::DirectExecution)
                    );
                    assert_eq!(allowed.rule, None);
                }
            }

            // None -> unchanged
            let none = apply_command_rule(base, None);
            assert_eq!(none.verdict, base);
            assert_eq!(none.rule, None);
        }
    }
}

// --- RC04: file loading, limits, strict validation, no echoed values ---

#[test]
fn rc04_missing_file_returns_none_and_valid_file_preserves_raw_text() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("permissions.toml");
    assert_eq!(load_rules_file(&missing).unwrap(), None);

    let path = dir.path().join("permissions.toml");
    let raw = "# comment\nversion = 1\n\n[[command]]\nprefix = [\"cargo\", \"test\"]\ndecision = \"allow\"\n\n[[command]]\nprefix = [\"git\", \"push\"]\ndecision = \"allow\"\noutside_sandbox = true\n\n[[command]]\nprefix = [\"rm\", \"-rf\"]\ndecision = \"deny\"\n";
    std::fs::write(&path, raw).unwrap();

    let loaded = load_rules_file(&path).unwrap().expect("loaded file");
    assert_eq!(loaded.raw_text, raw);
    assert_eq!(
        loaded.rules,
        vec![
            rule(&["cargo", "test"], RuleDecision::Allow),
            rule_outside(&["git", "push"], RuleDecision::Allow),
            rule(&["rm", "-rf"], RuleDecision::Deny),
        ]
    );
}

#[test]
fn rc04_validation_errors_report_path_and_location_without_echoing_values() {
    let secret = "SECRET_VALUE_MUST_NOT_APPEAR";
    let path = Path::new("/tmp/test-permissions.toml");

    // Syntax error with 1-based line and column
    let bad_toml =
        format!("version = 1\n[[command]]\nprefix = [\"{secret}\"]\ndecision = invalid_bare_token");
    let err = parse_rules_toml(&bad_toml, path).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("test-permissions.toml"));
    assert!(msg.contains("line 4, column 12"), "unexpected msg: {msg}");
    assert!(!msg.contains(secret));
    assert!(!msg.contains("invalid_bare_token"));

    // Unknown field
    let unknown_field = format!("version = 1\nunknown_key = \"{secret}\"\n");
    let err = parse_rules_toml(&unknown_field, path).unwrap_err();
    assert!(matches!(err, RulesFileError::Syntax { line: 2, .. }));
    assert!(!err.to_string().contains(secret));

    // Unknown field inside [[command]]
    let unknown_cmd_field = format!(
        "version = 1\n[[command]]\nprefix = [\"echo\"]\ndecision = \"allow\"\nextra = \"{secret}\"\n"
    );
    let err = parse_rules_toml(&unknown_cmd_field, path).unwrap_err();
    assert!(matches!(err, RulesFileError::Syntax { .. }));
    assert!(!err.to_string().contains(secret));

    // Duplicate key
    let dup_key = "version = 1\nversion = 1\n";
    let err = parse_rules_toml(dup_key, path).unwrap_err();
    assert!(matches!(err, RulesFileError::Syntax { .. }));

    // Unsupported version
    let bad_ver = "version = 2\n";
    let err = parse_rules_toml(bad_ver, path).unwrap_err();
    assert!(matches!(err, RulesFileError::UnsupportedVersion { .. }));
    assert!(err.to_string().contains("test-permissions.toml"));

    // Empty prefix
    let empty_prefix = "version = 1\n[[command]]\nprefix = []\ndecision = \"allow\"\n";
    let err = parse_rules_toml(empty_prefix, path).unwrap_err();
    assert!(matches!(err, RulesFileError::EmptyPrefix { index: 1, .. }));

    // Empty word and NUL word
    let empty_word = "version = 1\n[[command]]\nprefix = [\"echo\", \"\"]\ndecision = \"allow\"\n";
    let err = parse_rules_toml(empty_word, path).unwrap_err();
    assert!(matches!(
        err,
        RulesFileError::InvalidPrefixWord { index: 1, .. }
    ));

    let nul_word = "version = 1\n[[command]]\nprefix = [\"echo\\u0000x\"]\ndecision = \"allow\"\n";
    let err = parse_rules_toml(nul_word, path).unwrap_err();
    assert!(matches!(
        err,
        RulesFileError::InvalidPrefixWord { index: 1, .. }
    ));

    // outside_sandbox = true with deny or ask
    for dec in ["deny", "ask"] {
        let bad_outside = format!(
            "version = 1\n[[command]]\nprefix = [\"git\", \"push\"]\ndecision = \"{dec}\"\noutside_sandbox = true\n"
        );
        let err = parse_rules_toml(&bad_outside, path).unwrap_err();
        assert!(matches!(
            err,
            RulesFileError::OutsideSandboxRequiresAllow { index: 1, .. }
        ));
    }

    // Forbidden single-word allow across all 19 entries and path basenames
    assert_eq!(FORBIDDEN_SINGLE_WORD_ALLOW.len(), 19);
    for cmd in FORBIDDEN_SINGLE_WORD_ALLOW {
        for candidate in [*cmd, &format!("/usr/bin/{cmd}"), &cmd.to_uppercase()] {
            let toml_text = format!(
                "version = 1\n[[command]]\nprefix = [\"{candidate}\"]\ndecision = \"allow\"\n"
            );
            let err = parse_rules_toml(&toml_text, path).unwrap_err();
            let msg = err.to_string();
            assert!(
                matches!(
                    err,
                    RulesFileError::ForbiddenSingleWordAllow { index: 1, .. }
                ),
                "expected forbidden error for {candidate}: {msg}"
            );
            assert!(!msg.contains("/usr/bin"));
            if *cmd != "command" {
                assert!(!msg.contains(candidate));
            }
        }

        // Single-word deny and ask are allowed for these names
        for dec in ["deny", "ask"] {
            let toml_text =
                format!("version = 1\n[[command]]\nprefix = [\"{cmd}\"]\ndecision = \"{dec}\"\n");
            assert!(parse_rules_toml(&toml_text, path).is_ok());
        }
    }

    // Single-interpreter/wrapper allow with Windows/trailing-separator paths is also rejected
    for candidate in [
        r"C:\bin\env",
        r"bin\bash\",
        r"C:\Windows\System32\bash",
        "/usr/bin/env/",
        "/usr/bin/env/.",
        r".\bash\",
    ] {
        let toml_text =
            format!("version = 1\n[[command]]\nprefix = ['{candidate}']\ndecision = \"allow\"\n");
        let err = parse_rules_toml(&toml_text, path).unwrap_err();
        assert!(
            matches!(
                err,
                RulesFileError::ForbiddenSingleWordAllow { index: 1, .. }
            ),
            "expected forbidden error for {candidate}: {err}"
        );
        assert_eq!(suggest_command_prefixes(&format!("'{candidate}'")), None);
    }

    // Multi-word allow starting with a wrapper is allowed
    let multi_wrapper = "version = 1\n[[command]]\nprefix = [\"bash\", \"-c\", \"echo ok\"]\ndecision = \"allow\"\n";
    assert!(parse_rules_toml(multi_wrapper, path).is_ok());
}

#[test]
fn rc04_enforces_256kib_and_1000_rule_limits() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("permissions.toml");

    // 256 KiB exact limit passes (e.g., comment padding), 256 KiB + 1 fails
    let header = "version = 1\n";
    let pad_ok = "#".repeat(MAX_RULES_FILE_BYTES - header.len());
    std::fs::write(&path, format!("{header}{pad_ok}")).unwrap();
    assert!(load_rules_file(&path).unwrap().is_some());

    let pad_too_big = "#".repeat(MAX_RULES_FILE_BYTES - header.len() + 1);
    std::fs::write(&path, format!("{header}{pad_too_big}")).unwrap();
    let err = load_rules_file(&path).unwrap_err();
    assert!(matches!(err, RulesFileError::TooLarge { .. }));

    // 1000 rules pass, 1001 rules fail
    let one_rule = "[[command]]\nprefix = [\"echo\"]\ndecision = \"allow\"\n";
    let ok_1000 = format!("version = 1\n{}", one_rule.repeat(MAX_RULES_COUNT));
    assert_eq!(
        parse_rules_toml(&ok_1000, &path).unwrap().len(),
        MAX_RULES_COUNT
    );

    let bad_1001 = format!("version = 1\n{}", one_rule.repeat(MAX_RULES_COUNT + 1));
    let err = parse_rules_toml(&bad_1001, &path).unwrap_err();
    assert!(matches!(err, RulesFileError::TooManyRules { .. }));
}

#[test]
fn format_command_rule_block_roundtrips_through_parser() {
    let rules = vec![
        CommandRule {
            prefix: vec!["cargo".into(), "test".into()],
            decision: RuleDecision::Allow,
            outside_sandbox: false,
        },
        CommandRule {
            prefix: vec!["git".into(), "push".into()],
            decision: RuleDecision::Allow,
            outside_sandbox: true,
        },
        CommandRule {
            prefix: vec!["echo".into(), "a \"quoted\" \\ line\nnext".into()],
            decision: RuleDecision::Ask,
            outside_sandbox: false,
        },
    ];

    let mut text = String::from("version = 1\n\n");
    for r in &rules {
        text.push_str(&format_command_rule_block(r));
        text.push('\n');
    }

    let parsed = parse_rules_toml(&text, Path::new("permissions.toml")).unwrap();
    assert_eq!(parsed, rules);
}

#[test]
fn rc08_rule_source_display_and_permission_decision_rule_summary() {
    use crate::permission::{PermissionDecision, PermissionMode};

    let user_src = source(RuleScope::User, &["rm", "-rf"]);
    let ws_src = source(RuleScope::Workspace, &["cargo", "test"]);
    let session_src = source(RuleScope::Session, &["git", "push"]);

    assert_eq!(user_src.to_string(), "user rule `rm -rf`");
    assert_eq!(ws_src.to_string(), "workspace rule `cargo test`");
    assert_eq!(session_src.to_string(), "session rule `git push`");

    let allowed = PermissionDecision {
        mode: PermissionMode::Ask,
        revision: 0,
        kind: PermissionDecisionKind::RuleAllowed,
        rule: Some(ws_src.clone()),
    };
    assert_eq!(
        allowed.rule_summary().as_deref(),
        Some("allowed by workspace rule `cargo test`")
    );

    let denied = PermissionDecision {
        mode: PermissionMode::FullAccess,
        revision: 0,
        kind: PermissionDecisionKind::RuleDenied,
        rule: Some(user_src),
    };
    assert_eq!(
        denied.rule_summary().as_deref(),
        Some("denied by user rule `rm -rf`")
    );

    let asked_allow = PermissionDecision {
        mode: PermissionMode::FullAccess,
        revision: 0,
        kind: PermissionDecisionKind::AskedAndAllowed,
        rule: Some(session_src.clone()),
    };
    assert_eq!(
        asked_allow.rule_summary().as_deref(),
        Some("asked by session rule `git push` (allowed)")
    );

    let asked_deny = PermissionDecision {
        mode: PermissionMode::FullAccess,
        revision: 0,
        kind: PermissionDecisionKind::AskedAndDenied,
        rule: Some(session_src),
    };
    assert_eq!(
        asked_deny.rule_summary().as_deref(),
        Some("asked by session rule `git push` (denied)")
    );

    let direct = PermissionDecision {
        mode: PermissionMode::Ask,
        revision: 0,
        kind: PermissionDecisionKind::DirectExecution,
        rule: None,
    };
    assert_eq!(direct.rule_summary(), None);
}
