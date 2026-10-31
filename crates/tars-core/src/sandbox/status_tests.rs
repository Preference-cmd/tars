use super::backend::{BackendId, BackendReport, Capability, Limitation, SandboxUnavailable};
use super::status::SandboxStatus;

fn report(file_read: Capability, descendant_lifecycle: Capability) -> BackendReport {
    BackendReport::new(
        BackendId::SEATBELT,
        file_read,
        Capability::Full,
        Capability::Full,
        Capability::Full,
        descendant_lifecycle,
    )
}

const PARTIAL: &str = "partial (a descendant that starts a new session or process group can leave \
                       the process group)";

#[test]
fn capability_display_names_every_shape() {
    assert_eq!(Capability::Full.to_string(), "full");
    assert_eq!(
        Capability::Partial(Limitation::ProcessGroupEscape).to_string(),
        PARTIAL
    );
    assert_eq!(
        Capability::Partial(Limitation::Other("best effort".into())).to_string(),
        "partial (best effort)"
    );
    assert_eq!(
        Capability::Unsupported("not implemented".into()).to_string(),
        "unsupported (not implemented)"
    );
}

#[test]
fn available_display_lists_every_dimension_and_brief_omits_full_ones() {
    let status = SandboxStatus::Available(report(
        Capability::Full,
        Capability::Partial(Limitation::ProcessGroupEscape),
    ));
    assert!(status.is_available());
    assert_eq!(
        status.to_string(),
        format!(
            "seatbelt: file read full; file write full; network full; signal isolation full; \
             descendant lifecycle {PARTIAL}"
        )
    );
    assert_eq!(
        status.brief(),
        format!("seatbelt: descendant lifecycle {PARTIAL}")
    );
}

#[test]
fn all_full_brief_collapses_to_one_phrase() {
    let status = SandboxStatus::Available(report(Capability::Full, Capability::Full));
    assert_eq!(status.brief(), "seatbelt: all dimensions full");
}

#[test]
fn unavailable_display_and_brief_name_the_backend_and_reason() {
    let status = SandboxStatus::Unavailable(SandboxUnavailable::new(
        BackendId::SEATBELT,
        "policy rejected",
    ));
    assert!(!status.is_available());
    assert_eq!(
        status.to_string(),
        "seatbelt sandbox is unavailable: policy rejected"
    );
    assert_eq!(status.brief(), status.to_string());
}

#[test]
fn unavailable_backend_never_repeats_unavailable() {
    let status = SandboxStatus::Unavailable(SandboxUnavailable::new(
        BackendId::UNAVAILABLE,
        "no backend for this platform",
    ));
    assert_eq!(status.to_string(), "no backend for this platform");
    assert_eq!(status.brief(), status.to_string());
}

#[test]
fn unsupported_and_partial_read_dimensions_are_reported_verbatim() {
    let status = SandboxStatus::Available(report(
        Capability::Unsupported("read isolation is not implemented".into()),
        Capability::Full,
    ));
    assert_eq!(
        status.to_string(),
        "seatbelt: file read unsupported (read isolation is not implemented); file write full; \
         network full; signal isolation full; descendant lifecycle full"
    );
    assert_eq!(
        status.brief(),
        "seatbelt: file read unsupported (read isolation is not implemented)"
    );
}
