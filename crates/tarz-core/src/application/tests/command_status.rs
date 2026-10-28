use super::support::*;
use crate::sandbox::{BackendId, SandboxStatus, SandboxUnavailable};

/// Display-only coverage: the unavailable status is injected at the fixture
/// seam. The available status is propagated by real assembly in
/// `tests/command_status.rs`, and the failed-probe tool test proves repeated
/// rejection; assembly propagation of a failed probe is not injectable here.
#[tokio::test]
async fn managed_session_reports_an_injected_unavailable_command_status() {
    let host = SessionHost::new(None);
    let runner = Arc::new(TurnRunner::new(
        Arc::new(Gateway {
            calls: Arc::new(AtomicUsize::new(0)),
            entered: None,
            release: None,
            panic: false,
        }),
        Arc::new(ToolExecutor::new(vec![], ToolExecutorOptions::default()).unwrap()),
    ));
    *host.inner.assemble.lock().unwrap() = Some(Arc::new(move |definition, id, context| {
        let runner = runner.clone();
        let options = definition.run_options();
        let session = Session::new(
            id,
            context,
            move |_, _| {
                let runner = runner.clone();
                let options = options.clone();
                async move { Ok((runner, options)) }
            },
            definition.session_config(),
        );
        Ok(
            session.with_command_status(SandboxStatus::Unavailable(SandboxUnavailable::new(
                BackendId::UNAVAILABLE,
                "no backend for this platform",
            ))),
        )
    }));

    let session = host
        .create(PreparedCreate::transient(definition(), Context::default()))
        .await
        .unwrap();
    let status = session.command_status().expect("command status");
    assert!(!status.is_available());
    assert_eq!(status.to_string(), "no backend for this platform");
    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}
