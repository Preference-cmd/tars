use causa::kernel::{ModelInvokeError, ModelInvokeErrorKind, ModelOutput};

/// What a failed model call means for the user: a stable name for the kind,
/// what to do about it, and the gateway's own description of the failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelFailure {
    /// Stable lower-case name of the failure kind, for machine output.
    pub kind: &'static str,
    /// What the user can do about this kind of failure.
    pub hint: &'static str,
    /// The gateway's message. It can carry text chosen by the provider; the
    /// gateway keeps that text short and on one line.
    pub detail: String,
}

impl ModelFailure {
    /// Describes a failed model call. A refusal is named by its cause when
    /// the gateway reported an HTTP status or a provider error code.
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
            ModelInvokeErrorKind::Undecodable => (
                "undecodable",
                "the provider's response could not be read; check that the endpoint speaks the selected protocol, otherwise report it with the detail below",
            ),
            ModelInvokeErrorKind::Permanent => match refusal(error) {
                Refusal::Credential => (
                    "credential_rejected",
                    "the provider rejected the credential; check the API key and its access to this model",
                ),
                Refusal::NotFound => (
                    "not_found",
                    "the provider found no such model or path; check the model name and the endpoint URL",
                ),
                Refusal::Other => (
                    "permanent",
                    "model request was refused; check provider, endpoint, model and credential",
                ),
            },
        };
        Self {
            kind,
            hint,
            detail: error.message.clone(),
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

/// Why a provider refused, as far as the status or error code tells.
enum Refusal {
    Credential,
    NotFound,
    Other,
}

fn refusal(error: &ModelInvokeError) -> Refusal {
    match (error.status, error.code.as_deref()) {
        (Some(401 | 403), _) | (None, Some("authentication_error" | "permission_error")) => {
            Refusal::Credential
        }
        (Some(404), _) | (None, Some("not_found_error" | "model_not_found")) => Refusal::NotFound,
        _ => Refusal::Other,
    }
}

/// A raw provider value on one line with control characters removed.
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
    fn each_cause_has_its_own_name_and_hint() {
        let new = ModelInvokeError::new;
        let refused = |status| new(ModelInvokeErrorKind::Permanent, "x").with_status(status);
        let coded = |code| new(ModelInvokeErrorKind::Permanent, "x").with_code(code);
        let cases = [
            (new(ModelInvokeErrorKind::Transient, "x"), "transient"),
            (new(ModelInvokeErrorKind::TimedOut, "x"), "timed_out"),
            (new(ModelInvokeErrorKind::Cancelled, "x"), "cancelled"),
            (
                new(ModelInvokeErrorKind::InvalidRequest, "x"),
                "invalid_request",
            ),
            (
                new(ModelInvokeErrorKind::UnknownOutcome, "x"),
                "unknown_outcome",
            ),
            (new(ModelInvokeErrorKind::Undecodable, "x"), "undecodable"),
            (refused(401), "credential_rejected"),
            (refused(404), "not_found"),
            (refused(418), "permanent"),
        ];
        let mut hints = std::collections::BTreeSet::new();
        for (error, name) in cases {
            let failure = ModelFailure::of(&error);
            assert_eq!(failure.kind, name);
            assert!(hints.insert(failure.hint), "{name} repeats a hint");
        }
        // The same causes are recognized by status or, inside a stream, by code.
        assert_eq!(ModelFailure::of(&refused(403)).kind, "credential_rejected");
        assert_eq!(
            ModelFailure::of(&coded("authentication_error")).kind,
            "credential_rejected"
        );
        assert_eq!(
            ModelFailure::of(&coded("not_found_error")).kind,
            "not_found"
        );
        assert_eq!(ModelFailure::of(&coded("other_error")).kind, "permanent");
        // The status wins over a code that names another cause.
        assert_eq!(
            ModelFailure::of(&refused(404).with_code("authentication_error")).kind,
            "not_found"
        );
        // The gateway's message is shown as given.
        let failure = ModelFailure::of(&new(
            ModelInvokeErrorKind::Permanent,
            "openai error 401: bad key",
        ));
        assert_eq!(failure.detail, "openai error 401: bad key");
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
