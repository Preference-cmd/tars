use super::types::{ProgressUpdate, WorkProgress, WorkRef};
use causa::kernel::StreamDelta;
use causa::runtime::{RunEvent, RunObserver};
use std::sync::Arc;
use tokio::sync::broadcast;

pub(super) const PROGRESS_CAPACITY: usize = 128;
const MAX_TEXT_BYTES: usize = 16 * 1024;
const MAX_TOOL_NAME_BYTES: usize = 256;

fn bound_utf8(text: &str, max_bytes: usize) -> (String, bool) {
    let mut end = text.len().min(max_bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), end < text.len())
}

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
            RunEvent::ModelRequestReady { request, .. } => {
                (&request.invocation_id, ProgressUpdate::ModelStarted)
            }
            RunEvent::ModelDelta {
                invocation_id,
                delta: StreamDelta::TextDelta { delta },
                ..
            } => {
                let (delta, truncated) = bound_utf8(delta, MAX_TEXT_BYTES);
                (*invocation_id, ProgressUpdate::Text { delta, truncated })
            }
            RunEvent::ModelDelta {
                invocation_id,
                delta: StreamDelta::ReasoningDelta { delta },
                ..
            } => {
                if delta.is_empty() {
                    return;
                }
                let (delta, truncated) = bound_utf8(delta, MAX_TEXT_BYTES);
                (
                    *invocation_id,
                    ProgressUpdate::Reasoning { delta, truncated },
                )
            }
            RunEvent::ModelDelta {
                invocation_id,
                delta:
                    StreamDelta::ToolCallDelta {
                        call_index,
                        name_delta,
                        arguments_delta,
                        ..
                    },
                ..
            } => {
                let name = name_delta.as_deref().filter(|s| !s.is_empty());
                let arguments = arguments_delta.as_deref().filter(|s| !s.is_empty());
                if name.is_none() && arguments.is_none() {
                    return;
                }
                let (name_delta, name_truncated) = match name {
                    Some(text) => {
                        let (bounded, truncated) = bound_utf8(text, MAX_TOOL_NAME_BYTES);
                        (Some(bounded), truncated)
                    }
                    None => (None, false),
                };
                let (arguments_delta, arguments_truncated) = match arguments {
                    Some(text) => {
                        let (bounded, truncated) = bound_utf8(text, MAX_TEXT_BYTES);
                        (Some(bounded), truncated)
                    }
                    None => (None, false),
                };
                (
                    *invocation_id,
                    ProgressUpdate::ToolCallDelta {
                        call_index: *call_index,
                        name_delta,
                        arguments_delta,
                        truncated: name_truncated || arguments_truncated,
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
