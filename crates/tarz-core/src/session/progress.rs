use super::types::{ProgressUpdate, WorkProgress, WorkRef};
use causa::kernel::StreamDelta;
use causa::runtime::{RunEvent, RunObserver};
use std::sync::Arc;
use tokio::sync::broadcast;

pub(super) const PROGRESS_CAPACITY: usize = 128;
const MAX_TEXT_BYTES: usize = 16 * 1024;

pub(super) fn observer(
    sender: broadcast::Sender<Arc<WorkProgress>>,
    work: WorkRef,
    previous: Option<Arc<RunObserver>>,
    tools: super::ToolReporter,
) -> Arc<RunObserver> {
    Arc::new(move |event| {
        if let Some(previous) = &previous {
            previous(event);
        }
        tools.observe(event);
        // Broadcast is advisory. Without a subscriber, do not allocate or copy
        // an owned preview from the runner's borrowed observation.
        if sender.receiver_count() == 0 {
            return;
        }
        let (invocation_id, update) = match event {
            RunEvent::ModelRequestReady { request } => {
                (&request.invocation_id, ProgressUpdate::ModelStarted)
            }
            RunEvent::ModelDelta {
                invocation_id,
                delta: StreamDelta::TextDelta { delta },
            } => {
                let mut end = delta.len().min(MAX_TEXT_BYTES);
                while !delta.is_char_boundary(end) {
                    end -= 1;
                }
                (
                    *invocation_id,
                    ProgressUpdate::Text {
                        delta: delta[..end].to_owned(),
                        truncated: end < delta.len(),
                    },
                )
            }
            RunEvent::ModelOutput { invocation_id, .. } => {
                (*invocation_id, ProgressUpdate::ModelOutputReady)
            }
            _ => return,
        };
        // No await, task, history or admission coupling. Slow receivers detect
        // Lagged and still obtain the reliable result through wait/observe.
        let _ = sender.send(Arc::new(WorkProgress {
            work: work.clone(),
            invocation_id: invocation_id.clone(),
            update,
        }));
    })
}
