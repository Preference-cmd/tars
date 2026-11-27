//! Output format selection and conflict validation across CLI execution commands.

use super::configuration::CliError;
use std::ffi::OsString;
use tars_core::permission::escape_untrusted_text;

/// Output presentation format for `tars -p`, `tars run`, and `tars session run`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum OutputFormat {
    /// Final answer text on `stdout` (default).
    #[default]
    Text,
    /// Single terminal JSON object on `stdout`.
    Json,
    /// Incremental text deltas on `stdout` and escaped status lines on `stderr`.
    Stream,
    /// Compact NDJSON event stream ending with a `result` object on `stdout`.
    StreamJson,
}

/// Incremental builder that detects duplicate and conflicting format flags.
#[derive(Default)]
pub(crate) struct OutputFormatBuilder {
    chosen: Option<(OutputFormat, &'static str, String)>,
}

impl OutputFormatBuilder {
    /// Record a standalone flag (`--json`, `--stream`, or `--stream-json`).
    pub(crate) fn set_flag(&mut self, flag: &str) -> Result<(), CliError> {
        let (format, static_flag) = match flag {
            "--json" => (OutputFormat::Json, "--json"),
            "--stream" => (OutputFormat::Stream, "--stream"),
            "--stream-json" => (OutputFormat::StreamJson, "--stream-json"),
            _ => unreachable!("unsupported standalone format flag: {flag}"),
        };
        self.set(format, static_flag, static_flag.to_owned())
    }

    /// Parse and record `--output-format <text|json|stream-json>`.
    pub(crate) fn set_output_format(
        &mut self,
        args: &mut std::slice::Iter<'_, OsString>,
    ) -> Result<(), CliError> {
        let value = args
            .next()
            .ok_or_else(|| CliError::argument("option --output-format requires a value"))?;
        let raw = value
            .to_str()
            .ok_or_else(|| CliError::argument("text option must be UTF-8"))?;
        if raw.is_empty() {
            return Err(CliError::argument(
                "option --output-format must not be empty",
            ));
        }
        let format = match raw {
            "text" => OutputFormat::Text,
            "json" => OutputFormat::Json,
            "stream-json" => OutputFormat::StreamJson,
            other => {
                return Err(CliError::argument(format!(
                    "invalid --output-format '{}': expected text, json, or stream-json",
                    escape_untrusted_text(other)
                )));
            }
        };
        self.set(format, "--output-format", format!("--output-format {raw}"))
    }

    fn set(
        &mut self,
        format: OutputFormat,
        raw_flag: &'static str,
        display_flag: String,
    ) -> Result<(), CliError> {
        if let Some((_, prev_raw, prev_display)) = &self.chosen {
            if *prev_raw == raw_flag {
                return Err(CliError::argument(format!("duplicate option: {raw_flag}")));
            }
            return Err(CliError::argument(format!(
                "{prev_display} and {display_flag} are mutually exclusive"
            )));
        }
        self.chosen = Some((format, raw_flag, display_flag));
        Ok(())
    }

    /// Resolve to the selected format or `OutputFormat::Text` if none was given.
    pub(crate) fn build(self) -> OutputFormat {
        self.chosen.map(|(format, _, _)| format).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::{OutputFormat, OutputFormatBuilder};
    use std::ffi::OsString;

    fn parse(tokens: &[&str]) -> Result<OutputFormat, String> {
        let owned: Vec<OsString> = tokens.iter().map(OsString::from).collect();
        let mut builder = OutputFormatBuilder::default();
        let mut iter = owned.iter();
        while let Some(arg) = iter.next() {
            match arg.to_str().unwrap() {
                "--json" => builder
                    .set_flag("--json")
                    .map_err(|e| e.message().to_owned())?,
                "--stream" => builder
                    .set_flag("--stream")
                    .map_err(|e| e.message().to_owned())?,
                "--stream-json" => builder
                    .set_flag("--stream-json")
                    .map_err(|e| e.message().to_owned())?,
                "--output-format" => builder
                    .set_output_format(&mut iter)
                    .map_err(|e| e.message().to_owned())?,
                other => panic!("unexpected token {other}"),
            }
        }
        Ok(builder.build())
    }

    #[test]
    fn resolves_all_valid_output_formats() {
        assert_eq!(parse(&[]), Ok(OutputFormat::Text));
        assert_eq!(parse(&["--output-format", "text"]), Ok(OutputFormat::Text));
        assert_eq!(parse(&["--json"]), Ok(OutputFormat::Json));
        assert_eq!(parse(&["--output-format", "json"]), Ok(OutputFormat::Json));
        assert_eq!(parse(&["--stream"]), Ok(OutputFormat::Stream));
        assert_eq!(parse(&["--stream-json"]), Ok(OutputFormat::StreamJson));
        assert_eq!(
            parse(&["--output-format", "stream-json"]),
            Ok(OutputFormat::StreamJson)
        );
    }

    #[test]
    fn rejects_duplicates_and_conflicts_naming_exact_flags() {
        for (args, expected_parts) in [
            (vec!["--json", "--json"], vec!["duplicate option", "--json"]),
            (
                vec!["--stream", "--stream"],
                vec!["duplicate option", "--stream"],
            ),
            (
                vec!["--stream-json", "--stream-json"],
                vec!["duplicate option", "--stream-json"],
            ),
            (
                vec!["--output-format", "json", "--output-format", "text"],
                vec!["duplicate option", "--output-format"],
            ),
            (
                vec!["--json", "--stream"],
                vec!["--json", "--stream", "mutually exclusive"],
            ),
            (
                vec!["--json", "--stream-json"],
                vec!["--json", "--stream-json", "mutually exclusive"],
            ),
            (
                vec!["--stream", "--stream-json"],
                vec!["--stream", "--stream-json", "mutually exclusive"],
            ),
            (
                vec!["--output-format", "json", "--stream"],
                vec!["--output-format json", "--stream", "mutually exclusive"],
            ),
            (
                vec!["--output-format", "text", "--json"],
                vec!["--output-format text", "--json", "mutually exclusive"],
            ),
            (
                vec!["--output-format", "xml"],
                vec![
                    "invalid --output-format 'xml'",
                    "text, json, or stream-json",
                ],
            ),
            (
                vec!["--output-format"],
                vec!["option --output-format requires a value"],
            ),
            (
                vec!["--output-format", ""],
                vec!["option --output-format must not be empty"],
            ),
        ] {
            let err = parse(&args).unwrap_err();
            for part in expected_parts {
                assert!(
                    err.contains(part),
                    "args={args:?}: expected '{part}' in '{err}'"
                );
            }
        }
    }
}
