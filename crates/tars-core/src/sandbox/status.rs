use super::backend::{BackendReport, Capability, SandboxUnavailable};

/// The probed state of the command sandbox for one session.
///
/// The status is captured once when a session is assembled and does not change
/// while the session is open. Callers display the full report with
/// [`std::fmt::Display`] or the short form with [`brief`](Self::brief), and
/// branch on [`is_available`](Self::is_available); the variant payloads carry
/// only the backend's own vocabulary.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SandboxStatus {
    /// The backend was probed and can enforce its reported capabilities.
    Available(BackendReport),
    /// The platform has no backend or the backend probe failed.
    Unavailable(SandboxUnavailable),
}

impl SandboxStatus {
    /// Whether the sandbox can be used for a command.
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available(_))
    }

    /// The short form: the backend and only the dimensions that are not full,
    /// or the unavailable reason. All-full reports collapse to one phrase.
    pub fn brief(&self) -> String {
        match self {
            Self::Available(report) => {
                let details: Vec<String> = dimensions(report)
                    .into_iter()
                    .filter(|(_, capability)| **capability != Capability::Full)
                    .map(|(dimension, capability)| format!("{dimension} {capability}"))
                    .collect();
                if details.is_empty() {
                    format!("{}: all dimensions full", report.backend)
                } else {
                    format!("{}: {}", report.backend, details.join("; "))
                }
            }
            Self::Unavailable(error) => error.to_string(),
        }
    }
}

impl std::fmt::Display for SandboxStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Available(report) => {
                write!(formatter, "{}:", report.backend)?;
                for (index, (dimension, capability)) in dimensions(report).into_iter().enumerate() {
                    let separator = if index == 0 { " " } else { "; " };
                    write!(formatter, "{separator}{dimension} {capability}")?;
                }
                Ok(())
            }
            Self::Unavailable(error) => error.fmt(formatter),
        }
    }
}

fn dimensions(report: &BackendReport) -> [(&'static str, &Capability); 5] {
    [
        ("file read", &report.file_read),
        ("file write", &report.file_write),
        ("network", &report.network),
        ("signal isolation", &report.signal_isolation),
        ("descendant lifecycle", &report.descendant_lifecycle),
    ]
}
