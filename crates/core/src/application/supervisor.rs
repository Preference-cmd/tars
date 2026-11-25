use super::entry::Entry;
use super::host::HostInner;
use super::types::*;
use super::{opening, saving};
use futures_util::FutureExt;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

pub(super) async fn supervise(host: Arc<HostInner>, entry: Arc<Entry>) {
    let initialized = AssertUnwindSafe(opening::initialize(&host, &entry))
        .catch_unwind()
        .await;
    match initialized {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            {
                let mut entries = host.entries.lock().expect("directory has no callbacks");
                let mut state = entry.lock();
                state.error = Some(error);
                state.lifecycle = if state.closing.is_some() {
                    Lifecycle::Closed
                } else {
                    Lifecycle::Failed
                };
                state.close_in_flight = false;
                state.writer = None;
                if entries
                    .get(&entry.target.conversation_id)
                    .is_some_and(|current| current.target == entry.target)
                {
                    entries.remove(&entry.target.conversation_id);
                }
            }
            entry.changed();
            return;
        }
        Err(_) => isolate(&entry, "opening supervisor panicked").await,
    }
    loop {
        match AssertUnwindSafe(drive(&host, &entry)).catch_unwind().await {
            Ok(()) => return,
            Err(_) => isolate(&entry, "owner supervisor panicked").await,
        }
    }
}

async fn isolate(entry: &Entry, message: &str) {
    let pending_worker = {
        let mut state = entry.lock();
        state.lifecycle = Lifecycle::Faulted;
        state.error = Some(ApplicationError::Faulted(message.into()));
        state.fault = state.error.clone();
        state.close_in_flight = state.closing == Some(ClosePolicy::DiscardLocal);
        state.save_requested = false;
        state
            .handle
            .clone()
            .and_then(|handle| handle.begin_shutdown().map(|work| (handle, work)))
    };
    entry.changed();
    if let Some((handle, work)) = pending_worker {
        let _ = handle.wait_finished(&work).await;
    }
    // Keep the owner and write guard in the isolated slot until explicit close.
    // There is no arbitrary future cancellation or reliance on Session::drop.
}

async fn drive(host: &Arc<HostInner>, entry: &Arc<Entry>) {
    loop {
        let wake = entry.wake.notified();
        tokio::pin!(wake);
        wake.as_mut().enable();
        let active = {
            let state = entry.lock();
            if state.fault.is_none()
                && state.lifecycle != Lifecycle::Faulted
                && state.pending.is_none()
            {
                state
                    .active
                    .clone()
                    .map(|work| (state.handle.clone().expect("active handle"), work))
            } else {
                None
            }
        };
        if let Some((handle, work)) = active {
            #[cfg(test)]
            let result = tokio::select! {
                result = handle.wait_finished(&work) => result,
                _ = entry.panic_owner.notified() => panic!("injected owner supervisor panic"),
            };
            #[cfg(not(test))]
            let result = handle.wait_finished(&work).await;
            let observation = handle.peek_observation(&work).ok();
            let mut sync_info = None;
            let mut cancel_root_children = false;
            {
                let mut state = entry.lock();
                let captured = result.map_err(ApplicationError::from).and_then(|_| {
                    let owner = state.owner.as_mut().expect("active owner");
                    let history = owner.history_entries();
                    let usage = owner.session_usage();
                    // Even transient instances verify actual material return.
                    let context = owner
                        .with_context(|ctx| ctx.clone())
                        .map_err(ApplicationError::from)?;
                    Ok((context, history, usage))
                });
                match captured {
                    Ok((context, history, usage)) => {
                        state.generation = state
                            .generation
                            .checked_add(1)
                            .expect("material generation exhausted");
                        let generation = state.generation;
                        let persistent = state.persistent;
                        if persistent {
                            if let Err(error) = saving::prepare(entry, &mut state) {
                                state.lifecycle = Lifecycle::Faulted;
                                state.error = Some(error);
                                state.fault = state.error.clone();
                                state.close_in_flight =
                                    state.closing == Some(ClosePolicy::DiscardLocal);
                            }
                        } else {
                            state.active = None;
                        }
                        if let Some(obs) = observation
                            && let crate::session::WorkState::Finished(work_res) = obs.state
                        {
                            sync_info =
                                Some((work_res, context, history, usage, generation, persistent));
                        }
                    }
                    Err(error) => {
                        state.lifecycle = Lifecycle::Faulted;
                        state.error = Some(error);
                        state.fault = state.error.clone();
                        state.close_in_flight = state.closing == Some(ClosePolicy::DiscardLocal);
                        cancel_root_children = true;
                    }
                }
            }
            if let Some(coord) = entry.coordinator.get() {
                if let Some((work_res, context, history, usage, generation, persistent)) = sync_info
                {
                    let consumed = if persistent {
                        Default::default()
                    } else {
                        coord.entry_inbox().take_consumed()
                    };
                    coord.sync_entry_after_work_finished(
                        &work, &work_res, context, history, usage, generation, None, &consumed,
                    );
                } else if cancel_root_children {
                    coord.cancel_children_of_root_work(&work);
                }
            }
            entry.changed();
            continue;
        }
        let should_save = {
            let state = entry.lock();
            state.save_requested
                && state.pending.is_some()
                && state.closing != Some(ClosePolicy::DiscardLocal)
                && state.lifecycle != Lifecycle::Faulted
                && state.fault.is_none()
        };
        if should_save {
            saving::save(host, entry).await;
            if let Some(coord) = entry.coordinator.get() {
                coord.state_changed.notify_waiters();
            }
            continue;
        }
        let close = {
            let state = entry.lock();
            state.closing.filter(|_| state.close_in_flight)
        };
        if let Some(policy) = close {
            #[cfg(test)]
            {
                let gate = entry.before_close.lock().expect("test hook").take();
                if let Some((entered, release)) = gate {
                    entered.notify_one();
                    release.notified().await;
                }
            }
            // If an owner fault occurred, begin_shutdown still confirms its real
            // worker stopped before the write guard can ever be released.
            let active = {
                let state = entry.lock();
                state
                    .handle
                    .clone()
                    .and_then(|handle| handle.begin_shutdown().map(|work| (handle, work)))
            };
            if let Some((handle, work)) = active {
                let _ = handle.wait_finished(&work).await;
            }
            if let Some(coord) = entry.coordinator.get() {
                coord.wait_all_tasks_settled().await;
            }
            if super::closing::finish(host, entry, policy).await {
                return;
            }
            continue;
        }
        wake.await;
    }
}
