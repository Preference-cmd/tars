mod common;
use causa::{kernel::*, runtime::*};
use common::execution::run_once;
use common::*;
use std::sync::Arc;
use tarz_core::application::text_input;
use tarz_core::session::*;
use tokio::sync::Notify;

#[tokio::test]
async fn application_cancellation_waits_for_assembly_cleanup_and_retains_typed_result() {
    let started = Arc::new(Notify::new());
    let notify = started.clone();
    let initial = text_input("initial");
    let input = text_input("submitted");
    let session = Session::new(
        "app".into(),
        Context::from_blocks(vec![initial.clone()]).unwrap(),
        move |_, _| {
            let notify = notify.clone();
            async move {
                notify.notify_one();
                std::future::pending::<
                    Result<
                        (Arc<TurnRunner>, TurnRunOptions),
                        Box<dyn std::error::Error + Send + Sync>,
                    >,
                >()
                .await
            }
        },
        config(1),
    );
    let handle = session.handle();
    let cancel = CancellationToken::new();
    let task = tokio::spawn(run_once(session, vec![input.clone()], cancel.clone()));
    started.notified().await;
    cancel.cancel();
    let report = tokio::time::timeout(BOUND, task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(
        *report.result,
        WorkResult::StoppedBeforeRun(ControlError::Cancelled)
    ));
    assert_eq!(report.materials.unwrap().blocks(), &[initial, input]);
    assert!(report.uncommitted_tool_batch.is_none());
    assert!(matches!(
        handle.submit(request("new", vec![])).unwrap_err().reason,
        SessionError::Closed
    ));
}

#[tokio::test]
async fn application_preserves_uncommitted_model_output_and_materials() {
    let mut output = final_output("uncommitted answer");
    output.stop_reason = ModelStopReason::MaxTokens;
    let session = simple_session(Context::default(), 1, Gateway::new(vec![output]));
    let input = text_input("question");
    let report = run_once(session, vec![input.clone()], CancellationToken::new())
        .await
        .unwrap();
    let WorkResult::Run(TurnResult::Interrupted {
        cause: TurnInterruption::ModelMaxTokens { output, .. },
    }) = report.result.as_ref()
    else {
        panic!("{:?}", report.result)
    };
    assert_eq!(output.response.text.0, "uncommitted answer");
    assert_eq!(report.materials.unwrap().blocks(), &[input]);
}
