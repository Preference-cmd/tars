use super::*;
use causa::kernel::CancellationToken;
use std::sync::Arc;
use std::time::Duration;

fn sample_event(
    event_id: i64,
    priority: EventPriority,
    coalesce_key: Option<&str>,
    payload_json: &str,
) -> InboxEvent {
    InboxEvent {
        event_id,
        session_id: "01900000-0000-7000-8000-000000000001".into(),
        target_agent_id: "01900000-0000-7000-8000-000000000001".into(),
        priority,
        source_kind: EventSourceKind::AgentOperation,
        source_id: format!("op-{event_id}"),
        coalesce_key: coalesce_key.map(str::to_owned),
        wake_policy: WakePolicy::WakeIfIdle,
        payload_json: payload_json.to_owned(),
        state: EventDeliveryState::Pending,
        claimed_op_id: None,
        created_at_ms: 1_000 + event_id as u64,
        delivered_at_ms: None,
    }
}

#[test]
fn enum_roundtrips_and_status_predicates() {
    for interaction in [AgentInteraction::SingleWork, AgentInteraction::Reusable] {
        assert_eq!(
            interaction.as_str().parse::<AgentInteraction>().unwrap(),
            interaction
        );
    }
    for mode in [
        OperationExecutionMode::Sync,
        OperationExecutionMode::Background,
    ] {
        assert_eq!(
            mode.as_str().parse::<OperationExecutionMode>().unwrap(),
            mode
        );
    }
    for policy in [OnParentComplete::Continue, OnParentComplete::Cancel] {
        assert_eq!(policy.as_str().parse::<OnParentComplete>().unwrap(), policy);
    }
    for status in [
        OperationStatus::Creating,
        OperationStatus::Running,
        OperationStatus::Completed,
        OperationStatus::Cancelled,
        OperationStatus::Failed,
        OperationStatus::Deadline,
        OperationStatus::Faulted,
        OperationStatus::Interrupted,
    ] {
        assert_eq!(status.as_str().parse::<OperationStatus>().unwrap(), status);
        assert_eq!(status.is_active(), !status.is_terminal());
    }
    assert!(OperationStatus::Creating.is_active());
    assert!(OperationStatus::Running.is_active());
    assert!(OperationStatus::Completed.is_terminal());
    assert!(OperationStatus::Cancelled.is_terminal());
    assert!(OperationStatus::Interrupted.is_terminal());

    assert!("bogus".parse::<OperationStatus>().is_err());
    assert!("bogus".parse::<EventPriority>().is_err());
    assert!("bogus".parse::<EventSourceKind>().is_err());
    assert!("bogus".parse::<WakePolicy>().is_err());
    assert!("bogus".parse::<EventDeliveryState>().is_err());
    assert!("bogus".parse::<BackgroundTaskKind>().is_err());
}

#[test]
fn coalesce_inbox_events_orders_by_priority_and_fifo_and_drops_superseded_keys() {
    let events = vec![
        sample_event(1, EventPriority::Low, None, "{}"),
        sample_event(2, EventPriority::Normal, Some("agent-a"), "{\"v\":1}"),
        sample_event(3, EventPriority::Urgent, None, "{}"),
        sample_event(4, EventPriority::Normal, None, "{}"),
        sample_event(5, EventPriority::Urgent, Some("agent-a"), "{\"v\":2}"),
        sample_event(6, EventPriority::Low, Some("agent-b"), "{\"v\":1}"),
        sample_event(7, EventPriority::Low, Some("agent-b"), "{\"v\":2}"),
    ];

    let coalesced = coalesce_inbox_events(events);
    let kept_ids: Vec<i64> = coalesced.kept.iter().map(|e| e.event_id).collect();
    // Urgent: 3, 5; Normal: 4; Low: 1, 7
    assert_eq!(kept_ids, vec![3, 5, 4, 1, 7]);
    assert_eq!(coalesced.dropped_ids, vec![2, 6]);
}

#[test]
fn truncate_and_slice_utf8_page_handle_multibyte_boundaries_and_64k_cap() {
    let text = "a你🙂b界🚀c";
    let (prefix, truncated) = truncate_utf8_boundary(text, 5);
    assert!(truncated);
    // 'a' (1) + '你' (3) = 4; '🙂' starts at 4 and takes 4 bytes (ends at 8)
    assert_eq!(prefix, "a你");

    let (full, not_truncated) = truncate_utf8_boundary(text, text.len());
    assert!(!not_truncated);
    assert_eq!(full, text);

    // Mid-codepoint offset alignment and forward progress even with limit_bytes = 1 on 4-byte emoji
    let p0 = slice_utf8_page(text, 0, 2);
    assert_eq!(p0.content, "a");
    assert_eq!(p0.offset_bytes, 0);
    assert_eq!(p0.next_offset_bytes, 1);
    assert!(p0.has_more);

    // Offset 2 lands inside '你' (bytes 1..4), so start advances to byte 4 ('🙂')
    let p1 = slice_utf8_page(text, 2, 1);
    assert_eq!(p1.offset_bytes, 4);
    assert_eq!(p1.content, "🙂");
    assert_eq!(p1.next_offset_bytes, 8);
    assert!(p1.has_more);

    // limit_bytes = 0 returns empty slice at aligned offset
    let zero_limit = slice_utf8_page(text, 2, 0);
    assert_eq!(zero_limit.content, "");
    assert_eq!(zero_limit.offset_bytes, 4);
    assert_eq!(zero_limit.next_offset_bytes, 4);
    assert!(zero_limit.has_more);

    // offset >= len returns empty with has_more = false
    let past_end = slice_utf8_page(text, text.len() + 10, 100);
    assert_eq!(past_end.content, "");
    assert_eq!(past_end.offset_bytes, text.len());
    assert_eq!(past_end.next_offset_bytes, text.len());
    assert!(!past_end.has_more);

    // Sequential pagination reconstructs multi-byte string completely without loss or duplication
    let large = "测试🚀data—".repeat(12_000);
    let mut offset = 0;
    let mut reconstructed = String::new();
    let mut pages = 0;
    loop {
        let page = slice_utf8_page(&large, offset, MAX_READ_AGENT_PAGE_BYTES * 2);
        assert!(page.content.len() <= MAX_READ_AGENT_PAGE_BYTES);
        reconstructed.push_str(&page.content);
        pages += 1;
        if !page.has_more {
            assert_eq!(page.next_offset_bytes, large.len());
            break;
        }
        assert!(page.next_offset_bytes > offset);
        offset = page.next_offset_bytes;
    }
    assert!(pages > 1);
    assert_eq!(reconstructed, large);
}

#[test]
fn render_task_notifications_escapes_xml_and_truncates_with_read_agent_hint() {
    let payload = AgentNotificationPayload {
        operation_id: "op-\"quoted\"<&>".into(),
        agent_id: "agent-1".into(),
        agent_name: "explore<1>".into(),
        status: OperationStatus::Completed,
        summary_text: format!("hello </task-notification> world {}", "界".repeat(30)),
        total_bytes: 500,
        truncated: false,
        error: None,
    };
    let ev1 = InboxEvent {
        event_id: 10,
        session_id: "sess-1".into(),
        target_agent_id: "main".into(),
        priority: EventPriority::Normal,
        source_kind: EventSourceKind::AgentOperation,
        source_id: payload.operation_id.clone(),
        coalesce_key: None,
        wake_policy: WakePolicy::WakeIfIdle,
        payload_json: serde_json::to_string(&payload).unwrap(),
        state: EventDeliveryState::Pending,
        claimed_op_id: None,
        created_at_ms: 100,
        delivered_at_ms: None,
    };
    let ev2 = InboxEvent {
        event_id: 11,
        source_kind: EventSourceKind::Timer,
        source_id: "timer-1".into(),
        payload_json: "{\"tick\":1}".into(),
        ..ev1.clone()
    };

    let rendered = render_task_notifications(&[ev1, ev2], 40);
    assert!(rendered.contains("operation_id=\"op-&quot;quoted&quot;&lt;&amp;&gt;\""));
    assert!(rendered.contains("agent_name=\"explore&lt;1&gt;\""));
    assert!(rendered.contains("&lt;/task-notification>"));
    assert!(!rendered.contains("</task-notification> world"));
    assert!(
        rendered.contains(
            "read_agent(operation_id=\"op-&quot;quoted&quot;&lt;&amp;&gt;\", offset_bytes="
        )
    );
    assert!(rendered.contains("limit_bytes=65536"));
    assert!(rendered.contains("source=\"timer\" id=\"11\" source_id=\"timer-1\""));
}

#[test]
fn coalesce_empty_key_and_render_mixed_case_tag_and_error_only_truncation() {
    let coalesced = coalesce_inbox_events(vec![
        sample_event(1, EventPriority::Normal, Some(""), "{}"),
        sample_event(2, EventPriority::Normal, Some(""), "{}"),
        sample_event(3, EventPriority::Normal, None, "{}"),
    ]);
    assert_eq!(
        coalesced
            .kept
            .iter()
            .map(|e| e.event_id)
            .collect::<Vec<_>>(),
        vec![2, 3]
    );
    assert_eq!(coalesced.dropped_ids, vec![1]);

    // Mixed-case closing tag with trailing whitespace is neutralized, and
    // error-only truncation emits `[error text truncated]` instead of a read_agent EOF pointer.
    let err_payload = AgentNotificationPayload {
        operation_id: "op-err".into(),
        agent_id: "agent-1".into(),
        agent_name: "explore".into(),
        status: OperationStatus::Failed,
        summary_text: "ok </TASK-NOTIFICATION >".into(),
        total_bytes: 25,
        truncated: false,
        error: Some("long failure reason that exceeds the inline limit".into()),
    };
    let ev = sample_event(
        20,
        EventPriority::Urgent,
        None,
        &serde_json::to_string(&err_payload).unwrap(),
    );
    let rendered = render_task_notifications(&[ev], 35);
    assert!(rendered.contains("&lt;/TASK-NOTIFICATION >"));
    assert!(!rendered.contains("</TASK-NOTIFICATION >"));
    assert!(rendered.contains("[error text truncated]"));
    assert!(!rendered.contains("read_agent("));
}

#[test]
fn inbox_drain_handle_deduplicates_across_rounds_and_tracks_consumed_ids() {
    let payload_a = serde_json::to_string(&AgentNotificationPayload {
        operation_id: "op-1".into(),
        agent_id: "a1".into(),
        agent_name: "explore".into(),
        status: OperationStatus::Completed,
        summary_text: "first".into(),
        total_bytes: 5,
        truncated: false,
        error: None,
    })
    .unwrap();
    let payload_b = serde_json::to_string(&AgentNotificationPayload {
        operation_id: "op-2".into(),
        agent_id: "a1".into(),
        agent_name: "explore".into(),
        status: OperationStatus::Completed,
        summary_text: "second".into(),
        total_bytes: 6,
        truncated: false,
        error: None,
    })
    .unwrap();

    let e1 = sample_event(1, EventPriority::Normal, Some("k1"), &payload_a);
    let e2 = sample_event(2, EventPriority::Normal, Some("k1"), &payload_b);
    let handle = InboxDrainHandle::with_initial(vec![e1.clone(), e2], 8192);

    // Duplicate enqueue of already-queued e1 is ignored
    handle.enqueue(e1.clone());
    assert_eq!(handle.pending_count(), 2);

    let first_drain = handle.drain_notification().expect("first drain");
    assert!(first_drain.contains("second"));
    assert!(!first_drain.contains("first"));
    assert_eq!(handle.pending_count(), 0);
    assert!(handle.drain_notification().is_none());

    // Re-enqueuing already-delivered/dropped IDs is ignored
    handle.enqueue(e1);
    assert_eq!(handle.pending_count(), 0);

    let consumed = handle.take_consumed();
    assert_eq!(consumed.delivered_ids, vec![2]);
    assert_eq!(consumed.dropped_ids, vec![1]);
    assert_eq!(handle.consumed_snapshot(), ConsumedInboxEvents::default());

    // Round 2: new event arrives
    let e3 = sample_event(3, EventPriority::Urgent, None, &payload_a);
    handle.enqueue(e3);
    let second_drain = handle.drain_notification().expect("second drain");
    assert!(second_drain.contains("first"));
    assert_eq!(handle.consumed_snapshot().delivered_ids, vec![3]);
}

#[test]
fn session_bus_broadcasts_and_tolerates_lag() {
    let bus = SessionBus::new(2);
    assert_eq!(bus.receiver_count(), 0);
    // Publishing with zero subscribers is a no-op
    bus.publish(SessionEvent::SessionLifecycleChanged {
        session_id: "s1".into(),
        lifecycle: "open".into(),
    });

    let mut rx = bus.subscribe();
    assert_eq!(bus.receiver_count(), 1);
    for i in 0..4 {
        bus.publish(SessionEvent::SessionLifecycleChanged {
            session_id: "s1".into(),
            lifecycle: format!("state-{i}"),
        });
    }
    let lagged = rx.try_recv().unwrap_err();
    assert!(matches!(
        lagged,
        tokio::sync::broadcast::error::TryRecvError::Lagged(2)
    ));
    let ev = rx.try_recv().unwrap();
    assert!(matches!(
        &*ev,
        SessionEvent::SessionLifecycleChanged { lifecycle, .. } if lifecycle == "state-2"
    ));
}

#[tokio::test]
async fn background_task_registry_supervision_cascades_and_waits_for_finite_settlement() {
    let registry = Arc::new(BackgroundTaskRegistry::new());

    let tok_child_cancel = CancellationToken::new();
    let tok_child_continue = CancellationToken::new();
    let tok_grandchild_via_terminal = CancellationToken::new();
    let tok_grandchild_under_cancel = CancellationToken::new();
    let tok_daemon = CancellationToken::new();

    registry
        .register(BackgroundTaskRegistration {
            task_id: "op-term-mid".into(),
            kind: BackgroundTaskKind::AgentOperation,
            owner_agent_id: "parent-a".into(),
            parent_operation_id: Some("op-root".into()),
            policy: TaskLifecyclePolicy::Finite {
                on_parent_complete: OnParentComplete::Continue,
            },
            status: OperationStatus::Running,
            cancel_token: CancellationToken::new(),
            created_at_ms: 10,
        })
        .unwrap();
    assert!(matches!(
        registry.register(BackgroundTaskRegistration {
            task_id: "op-term-mid".into(),
            kind: BackgroundTaskKind::AgentOperation,
            owner_agent_id: "parent-a".into(),
            parent_operation_id: None,
            policy: TaskLifecyclePolicy::Daemon,
            status: OperationStatus::Running,
            cancel_token: CancellationToken::new(),
            created_at_ms: 10,
        }),
        Err(BackgroundRegistryError::DuplicateTask(_))
    ));

    // Mark intermediate node terminal Completed; late Cancelled cannot overwrite it
    assert!(
        registry
            .transition("op-term-mid", OperationStatus::Completed)
            .unwrap()
    );
    assert!(
        !registry
            .transition("op-term-mid", OperationStatus::Cancelled)
            .unwrap()
    );
    assert_eq!(
        registry
            .snapshot()
            .into_iter()
            .find(|s| s.task_id == "op-term-mid")
            .unwrap()
            .status,
        OperationStatus::Completed
    );

    // Grandchild hangs off the already-terminal intermediate node (AC07 traversal invariant)
    registry
        .register(BackgroundTaskRegistration {
            task_id: "op-grandchild-mid".into(),
            kind: BackgroundTaskKind::AgentOperation,
            owner_agent_id: "mid".into(),
            parent_operation_id: Some("op-term-mid".into()),
            policy: TaskLifecyclePolicy::Finite {
                on_parent_complete: OnParentComplete::Continue,
            },
            status: OperationStatus::Running,
            cancel_token: tok_grandchild_via_terminal.clone(),
            created_at_ms: 20,
        })
        .unwrap();

    registry
        .register(BackgroundTaskRegistration {
            task_id: "op-cancel-child".into(),
            kind: BackgroundTaskKind::AgentOperation,
            owner_agent_id: "parent-a".into(),
            parent_operation_id: Some("op-normal-parent".into()),
            policy: TaskLifecyclePolicy::Finite {
                on_parent_complete: OnParentComplete::Cancel,
            },
            status: OperationStatus::Creating,
            cancel_token: tok_child_cancel.clone(),
            created_at_ms: 30,
        })
        .unwrap();
    registry
        .register(BackgroundTaskRegistration {
            task_id: "op-cancel-grandchild".into(),
            kind: BackgroundTaskKind::AgentOperation,
            owner_agent_id: "op-cancel-child-agent".into(),
            parent_operation_id: Some("op-cancel-child".into()),
            policy: TaskLifecyclePolicy::Finite {
                on_parent_complete: OnParentComplete::Continue,
            },
            status: OperationStatus::Running,
            cancel_token: tok_grandchild_under_cancel.clone(),
            created_at_ms: 31,
        })
        .unwrap();
    registry
        .register(BackgroundTaskRegistration {
            task_id: "op-continue-child".into(),
            kind: BackgroundTaskKind::AgentOperation,
            owner_agent_id: "parent-a".into(),
            parent_operation_id: Some("op-normal-parent".into()),
            policy: TaskLifecyclePolicy::Finite {
                on_parent_complete: OnParentComplete::Continue,
            },
            status: OperationStatus::Running,
            cancel_token: tok_child_continue.clone(),
            created_at_ms: 32,
        })
        .unwrap();
    registry
        .register(BackgroundTaskRegistration {
            task_id: "daemon-1".into(),
            kind: BackgroundTaskKind::Timer,
            owner_agent_id: "parent-a".into(),
            parent_operation_id: None,
            policy: TaskLifecyclePolicy::Daemon,
            status: OperationStatus::Running,
            cancel_token: tok_daemon.clone(),
            created_at_ms: 40,
        })
        .unwrap();

    // Sibling count excludes daemon and optional excluded task_id
    assert_eq!(registry.active_sibling_count("parent-a", None), 2);
    assert_eq!(
        registry.active_sibling_count("parent-a", Some("op-continue-child")),
        1
    );
    assert_eq!(registry.active_finite_count(), 4);

    // Normal completion of op-normal-parent cancels OnParentComplete::Cancel subtree, spares Continue
    let normal_cancelled = registry.cancel_for_parent_normal_completion("op-normal-parent");
    assert_eq!(
        normal_cancelled,
        vec!["op-cancel-child", "op-cancel-grandchild"]
    );
    assert!(tok_child_cancel.is_cancelled());
    assert!(tok_grandchild_under_cancel.is_cancelled());
    assert!(!tok_child_continue.is_cancelled());

    // Cascading cancel of op-root traverses through terminal op-term-mid to cancel op-grandchild-mid
    let cascaded = registry.cancel_descendants_of_operation("op-root");
    assert_eq!(cascaded, vec!["op-grandchild-mid"]);
    assert!(tok_grandchild_via_terminal.is_cancelled());

    // wait_all_finite_settled unblocks when finite tasks settle even while daemon remains active
    let reg_wait = registry.clone();
    let waiter = tokio::spawn(async move {
        reg_wait.wait_all_finite_settled().await;
    });

    registry
        .transition("op-grandchild-mid", OperationStatus::Cancelled)
        .unwrap();
    registry
        .transition("op-cancel-child", OperationStatus::Cancelled)
        .unwrap();
    registry
        .transition("op-cancel-grandchild", OperationStatus::Cancelled)
        .unwrap();
    registry
        .transition("op-continue-child", OperationStatus::Completed)
        .unwrap();

    tokio::time::timeout(Duration::from_secs(2), waiter)
        .await
        .expect("finite settlement wait did not time out")
        .unwrap();

    // cancel_all cancels remaining active Daemon tasks on session close
    assert!(!tok_daemon.is_cancelled());
    let all_cancelled = registry.cancel_all();
    assert_eq!(all_cancelled, vec!["daemon-1"]);
    assert!(tok_daemon.is_cancelled());
}
