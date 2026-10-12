use causa::kernel::{ModelInvokeError, ModelInvokeErrorKind, ModelOutput};

/// What a failed model call means for the user: a stable name for the kind,
/// what to do about it, and the gateway's own description of the failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelFailure {
    /// Stable lower-case name of the failure kind, for machine output.
    pub kind: &'static str,
    /// What the user can do about this kind of failure.
    pub hint: &'static str,
    /// The gateway's message on one line with control characters removed,
    /// since it can carry text chosen by the provider.
    pub detail: String,
}

impl ModelFailure {
    /// Describes a failed model call.
    pub fn of(error: &ModelInvokeError) -> Self {
        let (kind, hint) = match error.kind {
            ModelInvokeErrorKind::Transient => (
                "transient",
                "temporary provider failure; retry later or set max_model_attempts",
            ),
            ModelInvokeErrorKind::TimedOut => ("timed_out", "model request timed out"),
            ModelInvokeErrorKind::Cancelled => ("cancelled", "model request cancelled"),
            ModelInvokeErrorKind::InvalidRequest => (
                "invalid_request",
                "model request was rejected as invalid; check the model, max_tokens (required by anthropic) and reasoning effort",
            ),
            ModelInvokeErrorKind::UnknownOutcome => (
                "unknown_outcome",
                "the response stream ended early; the outcome of this request is unknown",
            ),
            ModelInvokeErrorKind::Permanent => (
                "permanent",
                "model request failed; check provider, endpoint, model and credential",
            ),
        };
        Self {
            kind,
            hint,
            detail: one_line(&error.message),
        }
    }

    /// Describes a stop whose provider value this version has no meaning for.
    /// The detail is that value, so the user can tell what the provider said.
    pub fn unrecognized_stop(output: &ModelOutput) -> Self {
        Self {
            kind: "unrecognized_stop",
            hint: "the provider stopped for a reason this version does not recognize",
            detail: match &output.raw_stop_reason {
                Some(raw) => format!("provider stop value: {}", one_line(raw)),
                None => "the provider gave no stop value".to_owned(),
            },
        }
    }
}

/// Provider text on one line with control characters removed.
fn one_line(text: &str) -> String {
    text.split(|c: char| c.is_control())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_kind_has_its_own_name_and_the_detail_is_one_safe_line() {
        let kinds = [
            (ModelInvokeErrorKind::Transient, "transient"),
            (ModelInvokeErrorKind::TimedOut, "timed_out"),
            (ModelInvokeErrorKind::Cancelled, "cancelled"),
            (ModelInvokeErrorKind::InvalidRequest, "invalid_request"),
            (ModelInvokeErrorKind::UnknownOutcome, "unknown_outcome"),
            (ModelInvokeErrorKind::Permanent, "permanent"),
        ];
        let mut hints = std::collections::BTreeSet::new();
        for (kind, name) in kinds {
            let failure = ModelFailure::of(&ModelInvokeError::new(kind, "x"));
            assert_eq!(failure.kind, name);
            assert!(hints.insert(failure.hint));
        }
        let failure = ModelFailure::of(&ModelInvokeError::new(
            ModelInvokeErrorKind::Permanent,
            "openai error 401: bad\r\nkey\u{1b}[31m red",
        ));
        assert_eq!(failure.detail, "openai error 401: bad key [31m red");
    }

    #[test]
    fn an_unrecognized_stop_reports_the_provider_value_or_its_absence() {
        use causa::kernel::{ModelResponse, ModelStopReason};
        let mut output = ModelOutput {
            response: ModelResponse { items: Vec::new() },
            usage: None,
            stop_reason: ModelStopReason::Unrecognized,
            raw_stop_reason: Some("pause\nturn".into()),
            reasoning: None,
        };
        let failure = ModelFailure::unrecognized_stop(&output);
        assert_eq!(failure.kind, "unrecognized_stop");
        assert_eq!(failure.detail, "provider stop value: pause turn");
        output.raw_stop_reason = None;
        assert_eq!(
            ModelFailure::unrecognized_stop(&output).detail,
            "the provider gave no stop value"
        );
    }
}
