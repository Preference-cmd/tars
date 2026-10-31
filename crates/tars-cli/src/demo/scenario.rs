use super::fixtures::{Counters, input, session};
use causa::kernel::{BlockContent, Context, ToolResultStatus};
use causa::runtime::TurnResult;
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tars_core::material;
use tars_core::session::{
    Session, SessionError, SessionHandle, SubmitRequest, WorkRef, WorkResult, WorkState,
};

type Failure = Box<dyn Error + Send + Sync>;

pub(crate) async fn run(destination: Option<PathBuf>) -> Result<String, Failure> {
    let temporary = if destination.is_none() {
        Some(tempfile::tempdir()?)
    } else {
        None
    };
    let path = destination.clone().unwrap_or_else(|| {
        temporary
            .as_ref()
            .expect("temporary directory")
            .path()
            .join("materials.json")
    });
    let counters = Arc::new(Counters::default());
    let initial = Context::from_blocks(vec![
        input("Instructions"),
        input("Material A"),
        input("Material B"),
    ])?;
    let mut owner = session(initial, counters.clone());
    let first = exercise(&mut owner).await;
    owner.shutdown().await;
    let (context, old_work, reply) = first?;
    // Snapshot and disk I/O happen after with_context has released its lock.
    material::save(&path, &context)?;
    let restored = material::load(&path)?;
    require(
        restored.blocks() == context.blocks(),
        "material round-trip changed blocks",
    )?;
    let mut rebuilt = session(restored, counters.clone());
    let handle = rebuilt.handle();
    let continued = async {
        require(
            matches!(
                handle.observe(&old_work),
                Err(SessionError::NotFound { .. })
            ),
            "import restored an old work record",
        )?;
        let work = handle
            .submit(SubmitRequest {
                request_key: "k1".into(),
                blocks: vec![],
            })?
            .work;
        completed(&handle, &work).await
    }
    .await;
    rebuilt.shutdown().await;
    let after_reload = continued?;
    require(
        counters.tools.load(Ordering::SeqCst) == 1,
        "imported tool declaration was replayed",
    )?;
    let saved = destination.map_or_else(
        || "Temporary material round-trip verified.".to_owned(),
        |p| format!("Materials saved to {}.", p.display()),
    );
    Ok(format!(
        "Offline demo — scripted model, local tool, no network.\n{reply}\nExact-key replay reused the first receipt.\nRebuilt session: {after_reload}\n{} model calls; {} tool call.\n{saved}\n",
        counters.models.load(Ordering::SeqCst),
        counters.tools.load(Ordering::SeqCst),
    ))
}

async fn exercise(owner: &mut Session) -> Result<(Context, WorkRef, String), Failure> {
    let handle = owner.handle();
    let request = SubmitRequest {
        request_key: "k1".into(),
        blocks: vec![input("First input")],
    };
    let receipt = handle.submit(request.clone())?;
    let first = completed(&handle, &receipt.work).await?;
    require(
        owner.with_context(|context| {
            context.blocks().iter().any(|block| match block.content() {
                BlockContent::ToolResult(result) => {
                    result.status == ToolResultStatus::Succeeded
                        && result.output.content["scope_matches"] == true
                        && result.output.content["work"].as_str()
                            == Some(receipt.work.turn_id.0.as_str())
                }
                _ => false,
            })
        })?,
        "local tool did not succeed with the current work binding",
    )?;
    owner.with_context(|context| {
        context
            .edit()
            .replace(1..3, [input("Summary of A and B")])
            .commit()
    })??;
    let replay = handle.submit(request)?;
    require(replay == receipt, "exact replay allocated a new work")?;
    let second = handle.submit(SubmitRequest {
        request_key: "k2".into(),
        blocks: vec![input("Second input")],
    })?;
    let second_reply = completed(&handle, &second.work).await?;
    let copied = owner.with_context(|context| context.clone())?;
    let still_first = completed(&handle, &receipt.work).await?;
    require(
        first == still_first,
        "old result changed after material editing",
    )?;
    Ok((copied, receipt.work, second_reply))
}

async fn completed(handle: &SessionHandle, work: &WorkRef) -> Result<String, Failure> {
    let observation = tokio::select! {
        result = handle.wait(work, Duration::from_secs(15)) => result?,
        signal = tokio::signal::ctrl_c() => {
            signal?;
            handle.cancel(work)?;
            handle.wait(work, Duration::from_secs(15)).await?
        },
    };
    match observation.state {
        WorkState::Finished(result) => match result.as_ref() {
            WorkResult::Run(TurnResult::Completed { final_output }) => {
                Ok(final_output.response.text())
            }
            other => Err(std::io::Error::other(format!("demo work stopped: {other:?}")).into()),
        },
        _ => Err(std::io::Error::other("demo work did not finish before the wait timeout").into()),
    }
}

fn require(condition: bool, message: &str) -> Result<(), Failure> {
    if condition {
        Ok(())
    } else {
        Err(std::io::Error::other(message.to_owned()).into())
    }
}
