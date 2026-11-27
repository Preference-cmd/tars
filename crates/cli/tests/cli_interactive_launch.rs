use std::process::{Command, Output};

fn run(arguments: &[&str], directory: &std::path::Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tars"))
        .args(arguments)
        .current_dir(directory)
        .env("TMPDIR", directory)
        .env("TARS_HOME", directory.join("tars-home"))
        .env_remove("TARS_CONFIG")
        .output()
        .expect("run built TARS executable")
}

fn text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).unwrap()
}

#[test]
fn cc01_interactive_routing_order_help_version_and_non_tty_behavior() {
    let dir = tempfile::tempdir().unwrap();

    // Bare `tars` on non-TTY prints unified help (including new flags/commands) and exits 0.
    let bare = run(&[], dir.path());
    assert!(bare.status.success());
    let help = text(&bare.stdout);
    for expected in [
        "-c, --continue",
        "-r, --resume",
        "--workspace",
        "/clear",
        "/resume",
        "/model",
    ] {
        assert!(help.contains(expected), "missing '{expected}' in:\n{help}");
    }

    // Interactive flags paired with `--help` / `-V` in any order route to TUI option parser.
    for args in [
        vec!["-c", "--help"],
        vec!["-r", "--help"],
        vec!["--resume", "abc", "--help"],
        vec!["--workspace", dir.path().to_str().unwrap(), "--help"],
        vec!["--store", dir.path().to_str().unwrap(), "-c", "-h"],
    ] {
        let out = run(&args, dir.path());
        assert!(out.status.success(), "{args:?}: {:?}", text(&out.stderr));
        let stdout = text(&out.stdout);
        assert!(stdout.contains("--continue"), "{args:?}: {stdout}");
        assert!(stdout.contains("--resume"), "{args:?}: {stdout}");
        assert!(stdout.contains("--workspace"), "{args:?}: {stdout}");
        assert!(stdout.contains("/clear"), "{args:?}: {stdout}");
        assert!(stdout.contains("/resume"), "{args:?}: {stdout}");
        assert!(stdout.contains("/model"), "{args:?}: {stdout}");
    }

    let ver = run(&["-r", "prefix", "-V"], dir.path());
    assert!(ver.status.success());
    assert_eq!(
        text(&ver.stdout),
        format!("tars {}\n", env!("CARGO_PKG_VERSION"))
    );

    // Interactive flags on non-TTY fail with exit 1 (interactive terminal required).
    let store_dir = dir.path().join("store");
    let ws_dir = dir.path().join("ws");
    std::fs::create_dir_all(&ws_dir).unwrap();
    let tars_home = dir.path().join("tars-home");
    std::fs::create_dir_all(&tars_home).unwrap();
    std::fs::write(
        tars_home.join("config.toml"),
        "version = 1\n[profiles.fast]\nprovider = 'openai-chat'\nmodel = 'fast-model'\n",
    )
    .unwrap();
    for args in [
        vec!["-c"],
        vec!["--continue"],
        vec!["-r"],
        vec!["--resume", "abc"],
        vec!["-r", "--profile", "fast"],
        vec!["--store", store_dir.to_str().unwrap(), "-c"],
        vec!["--workspace", ws_dir.to_str().unwrap()],
    ] {
        let out = run(&args, dir.path());
        assert_eq!(
            out.status.code(),
            Some(1),
            "{args:?}: stderr={}",
            text(&out.stderr)
        );
        assert!(
            text(&out.stderr).contains("interactive terminal"),
            "{args:?}: {}",
            text(&out.stderr)
        );
    }
}

#[test]
fn cc01_print_mode_precedence_conflicts_duplicates_and_unknown_args() {
    let dir = tempfile::tempdir().unwrap();
    let store_dir = dir.path().join("empty-store");

    // `-p` / `--print` always wins over `-c` / `--resume` regardless of order.
    for args in [
        vec![
            "-p",
            "-c",
            "--no-config",
            "--store",
            store_dir.to_str().unwrap(),
            "hi",
        ],
        vec![
            "-c",
            "-p",
            "--no-config",
            "--store",
            store_dir.to_str().unwrap(),
            "hi",
        ],
        vec![
            "-p",
            "--resume",
            "deadbeef",
            "--no-config",
            "--store",
            store_dir.to_str().unwrap(),
            "hi",
        ],
        vec![
            "--resume",
            "deadbeef",
            "-p",
            "--no-config",
            "--store",
            store_dir.to_str().unwrap(),
            "hi",
        ],
    ] {
        let out = run(&args, dir.path());
        // Print mode reaches session resolution (exit 1, session not found), NEVER TUI non-TTY check.
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        let stderr = text(&out.stderr);
        assert!(
            !stderr.contains("interactive terminal"),
            "{args:?} misrouted to TUI: {stderr}"
        );
        assert!(stderr.contains("session not found"), "{args:?}: {stderr}");
    }

    // Argument conflicts, duplicates, missing values, and unknown args exit 2.
    for (args, expected) in [
        (vec!["-c", "-r"], "mutually exclusive"),
        (vec!["--continue", "--resume", "abc"], "mutually exclusive"),
        (vec!["-r", "abc", "-c"], "mutually exclusive"),
        (vec!["-c", "-c"], "specified twice"),
        (vec!["--continue", "--continue"], "specified twice"),
        (vec!["-c", "--continue"], "mutually exclusive"),
        (vec!["-r", "-r"], "specified twice"),
        (vec!["--resume", "a", "--resume", "b"], "specified twice"),
        (vec!["-r", "--resume"], "mutually exclusive"),
        (
            vec!["--workspace", "a", "--workspace", "b"],
            "specified twice",
        ),
        (vec!["--workspace"], "requires a value"),
        (vec!["--workspace", ""], "requires a value"),
        (vec!["--workspace", "--store", "x"], "requires a value"),
        (vec!["-r", ""], "must not be empty"),
        (vec!["--", "-c"], "Unsupported arguments"),
        (vec!["foo", "--help"], "Unsupported arguments"),
    ] {
        let out = run(&args, dir.path());
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?}: stdout={}, stderr={}",
            text(&out.stdout),
            text(&out.stderr)
        );
        assert!(
            text(&out.stderr).contains(expected),
            "{args:?}: expected '{expected}' in '{}'",
            text(&out.stderr)
        );
    }
}

#[test]
fn cc01_completions_include_interactive_session_and_workspace_flags() {
    let dir = tempfile::tempdir().unwrap();
    for (shell, tokens) in [
        (
            "bash",
            vec!["-c", "--continue", "-r", "--resume", "--workspace"],
        ),
        (
            "zsh",
            vec!["-c", "--continue", "-r", "--resume", "--workspace"],
        ),
        (
            "fish",
            vec!["-s c", "-l continue", "-s r", "-l resume", "-l workspace"],
        ),
    ] {
        let out = run(&["completions", shell], dir.path());
        assert!(out.status.success(), "{shell}: {}", text(&out.stderr));
        let script = text(&out.stdout);
        for token in tokens {
            assert!(
                script.contains(token),
                "{shell} completion script missing '{token}'"
            );
        }
    }
}
