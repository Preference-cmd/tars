use super::state::{Record, Shared};
use super::types::{WorkResult, WorkState, WorkerFault};
use causa::kernel::{Context, ControlError, ToolBatch};
use causa::runtime::RunControl;
use std::any::Any;
use std::sync::Arc;
use tokio::runtime::Handle;

struct Returned {
    context: Context,
    uncommitted: Option<ToolBatch>,
    result: WorkResult,
}

pub(super) fn start(runtime: Handle, shared: Arc<Shared>, record: Arc<Record>, context: Context) {
    let worker_shared = shared.clone();
    let worker_record = record.clone();
    let task = runtime.spawn(async move { run(worker_shared, worker_record, context).await });
    #[cfg(test)]
    {
        *record.abort.lock().expect("test hook has no callbacks") = Some(task.abort_handle());
    }
    runtime.spawn(async move {
        match task.await {
            Ok(returned) => {
                record.tools.drain().await;
                record.tools.freeze();
                #[cfg(test)]
                {
                    let gate = record
                        .before_publish
                        .lock()
                        .expect("test hook has no callbacks")
                        .clone();
                    if let Some((selected, release)) = gate {
                        selected.notify_one();
                        release.notified().await;
                    }
                }
                shared.finish(
                    &record,
                    Some(returned.context),
                    returned.uncommitted,
                    returned.result,
                );
            }
            Err(error) => {
                // Fault selection is preserved while tracked filesystem jobs
                // observe cancellation and actually release their resources.
                record.control.cancellation_token().cancel();
                record.tools.drain().await;
                record.tools.freeze();
                let fault = if error.is_panic() {
                    WorkerFault::Panicked {
                        message: panic_message(error.into_panic()),
                    }
                } else {
                    WorkerFault::Cancelled
                };
                shared.finish(&record, None, None, WorkResult::Faulted(fault));
            }
        }
    });
}

async fn run(shared: Arc<Shared>, record: Arc<Record>, context: Context) -> Returned {
    let control = &record.control;
    if let Err(error) = control.call_control().check() {
        return stopped(context, error);
    }
    record.publish(WorkState::Running);
    let assembled = tokio::select! {
        biased;
        error = wait_for_stop(control) => return stopped(context, error),
        result = (shared.assemble)(record.work.clone(), control.clone(), record.tools.clone()) => result,
    };
    // Control wins when the assembly result and cancellation are selected in the
    // same progression. Once a terminal result is selected, it stays frozen.
    if let Err(error) = control.call_control().check() {
        return stopped(context, error);
    }
    let (runner, mut options) = match assembled {
        Ok(assembled) => assembled,
        Err(error) => {
            return Returned {
                context,
                uncommitted: None,
                result: WorkResult::AssemblyFailed(error),
            };
        }
    };
    options.observer = Some(super::progress::observer(
        shared.progress.clone(),
        record.work.clone(),
        options.observer.take(),
        record.tools.clone(),
    ));
    // Ordinary cancellation must let Causa return owned context and the full U.
    // Do not race and drop this future against cancellation or a deadline here.
    let outcome = runner
        .run_streaming(
            record.work.turn_id.clone(),
            context,
            options,
            control.clone(),
        )
        .await;
    Returned {
        context: outcome.context,
        uncommitted: outcome.uncommitted_tool_batch,
        result: WorkResult::Run(outcome.result),
    }
}

fn stopped(context: Context, error: ControlError) -> Returned {
    Returned {
        context,
        uncommitted: None,
        result: WorkResult::StoppedBeforeRun(error),
    }
}

async fn wait_for_stop(control: &RunControl) -> ControlError {
    match control.deadline() {
        Some(deadline) => tokio::select! {
            biased;
            _ = control.cancellation_token().cancelled() => ControlError::Cancelled,
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => ControlError::TimedOut,
        },
        None => {
            control.cancellation_token().cancelled().await;
            ControlError::Cancelled
        }
    }
}

fn panic_message(payload: Box<dyn Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else {
        "non-text panic payload".to_owned()
    }
}
