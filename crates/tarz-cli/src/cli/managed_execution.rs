use causa::runtime::new_block_id;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tarz_core::application::{ApplicationError, ManagedSession, SaveStatus, text_input};
use tarz_core::session::{SubmitRequest, WorkRef, WorkResult, WorkState};

pub(super) struct WorkReport {
    pub work: WorkRef,
    pub result: Arc<WorkResult>,
    pub save: Result<SaveStatus, ApplicationError>,
    pub signal_failed: bool,
}

pub(super) fn input_text(input: Option<String>) -> Result<String, &'static str> {
    let text = match input {
        Some(text) => text,
        None => {
            let mut text = String::new();
            io::stdin()
                .read_to_string(&mut text)
                .map_err(|_| "cannot read UTF-8 stdin")?;
            text
        }
    };
    if text.trim().is_empty() {
        return Err("input must not be blank");
    }
    Ok(text)
}

pub(super) async fn work(session: &ManagedSession, input: String) -> Result<WorkReport, String> {
    let receipt = session
        .submit(SubmitRequest {
            request_key: new_block_id().0.to_string(),
            blocks: vec![text_input(input)],
        })
        .map_err(|error| error.to_string())?;
    let mut signal_failed = false;
    let mut stopping = false;
    let result = loop {
        let observation = tokio::select! {
            biased;
            observation = session.wait(&receipt.work, Duration::from_secs(60)) => {
                observation.map_err(|error| error.to_string())?
            },
            signal = tokio::signal::ctrl_c(), if !stopping => {
                signal_failed = signal.is_err();
                session.cancel(&receipt.work).map_err(|error| error.to_string())?;
                stopping = true;
                continue;
            }
        };
        if let WorkState::Finished(result) = observation.state {
            break result;
        }
    };
    // A raw Finished observation precedes persistence. Keep the runtime alive
    // through the independent host save barrier, including cancelled work.
    let save = session.wait_saved().await;
    Ok(WorkReport {
        work: receipt.work,
        result,
        save,
        signal_failed,
    })
}

pub(super) fn recovery(
    session: &ManagedSession,
    directory: &Path,
) -> Result<PathBuf, &'static str> {
    let snapshot = session
        .read_materials()
        .map_err(|_| "returned materials are unavailable for recovery")?;
    let path = directory.join(format!("tarz-recovery-{}.json", new_block_id().0));
    tarz_core::material::save_new(&path, &snapshot.context)
        .map_err(|_| "recovery material export failed")?;
    Ok(path)
}
