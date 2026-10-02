use crate::session::{
    Session, SessionError, SubmitRejection, SubmitRequest, WorkRef, WorkResult, WorkState,
};
use causa::kernel::{
    BlockContent, BlockMeta, ContentPart, Context, ContextBlock, TextPayload, ToolBatch,
};
use causa::runtime::new_block_id;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// A frozen execution result and its independently recoverable materials.
pub struct RunReport {
    /// Identity of the accepted work.
    pub work: WorkRef,
    /// Original typed result, including stage failure inputs.
    pub result: Arc<WorkResult>,
    /// Returned materials, or an explicit loss diagnosis after worker failure.
    pub materials: Result<Context, SessionError>,
    /// Original batch whose declarations, but not results, were committed.
    pub uncommitted_tool_batch: Option<ToolBatch>,
}

/// Create a fresh identified user text block without changing existing materials.
pub fn text_input(text: impl Into<String>) -> ContextBlock {
    ContextBlock::new(
        new_block_id(),
        BlockContent::Parts(vec![ContentPart::Text(TextPayload::new(text))]),
        BlockMeta {
            source: Some("user".into()),
            ..Default::default()
        },
    )
}

/// Consume an idle session for one work, then close it after supervised completion.
/// Cancellation requests a normal stop and never drops the runner future.
/// The caller must keep polling this future until completion to await cleanup.
/// Other handles must not submit work concurrently; multi-work entry points should
/// drive Session directly. Materials are the snapshot taken after shutdown.
pub async fn run_once(
    mut session: Session,
    blocks: Vec<ContextBlock>,
    cancellation: CancellationToken,
) -> Result<RunReport, SubmitRejection> {
    let handle = session.handle();
    let receipt = handle.submit(SubmitRequest {
        request_key: uuid::Uuid::now_v7().to_string(),
        blocks,
    })?;
    let work = receipt.work;
    let mut stopping = false;
    let result = loop {
        let observation = tokio::select! {
            biased;
            result = handle.wait(&work, Duration::from_secs(60)) => result.expect("owned work is retained"),
            _ = cancellation.cancelled(), if !stopping => {
                handle.cancel(&work).expect("owned work is retained");
                stopping = true;
                continue;
            }
        };
        if let WorkState::Finished(result) = observation.state {
            break result;
        }
    };
    session.shutdown().await;
    let materials = session.with_context(|context| context.clone());
    let uncommitted_tool_batch = session
        .take_uncommitted_tool_batch(&work)
        .expect("owned work is terminal and retained");
    Ok(RunReport {
        work,
        result,
        materials,
        uncommitted_tool_batch,
    })
}
