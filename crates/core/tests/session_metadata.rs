use causa::kernel::Context;
use tars_core::{
    application::{PreparedCreate, SessionHost, text_input},
    created_at_from_id,
    definition::{CredentialRef, Provider, SessionDefinition},
    format_timestamp_utc,
};

fn test_definition() -> SessionDefinition {
    SessionDefinition {
        provider: Provider::OpenAiChat,
        model: "fixture-model".into(),
        endpoint: None,
        credential: CredentialRef::Environment("PATH".into()),
        work_timeout_secs: Some(120),
        retained_work_capacity: 8,
        max_model_rounds: 1,
        max_tool_calls: 1,
        max_tokens: None,
        reasoning_effort: None,
        extra_body: Default::default(),
        max_model_attempts: None,
        render_options: Default::default(),
        workspace_root: None,
        tools: Vec::new(),
        tool_limits: Default::default(),
        write_limits: None,
        permission_mode: None,
        command: None,
        mcp_servers: Vec::new(),
        context_window: None,
        auto_compact: true,
        compact_threshold: 70,
        prompt_cache: true,
    }
}

#[tokio::test]
async fn test_be23_title_multiline_first_nonblank() {
    let dir = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(dir.path().to_path_buf()));

    let context = Context::from_blocks(vec![text_input(
        "\n  \n  First actual line of prompt   \nSecond line",
    )])
    .unwrap();

    let session = host
        .create(PreparedCreate::persistent(test_definition(), context))
        .await
        .unwrap();

    assert_eq!(session.title(), Some("First actual line of prompt".into()));

    let summary = host.list().await.unwrap().into_iter().next().unwrap();
    let record = summary.record.unwrap();
    assert_eq!(record.title, Some("First actual line of prompt".into()));
}

#[tokio::test]
async fn test_be23_title_overlong_and_unicode_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(dir.path().to_path_buf()));

    // 40 Chinese characters (each 3 bytes in UTF-8) + 50 ASCII characters = 90 Unicode chars (> 80)
    let cjk_part = "這是一段用來測試長度超過限制的多字節繁體中文文本，確保不會在字符邊界截斷。";
    let ascii_part = "1234567890abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let full_prompt = format!("{cjk_part}{ascii_part}");
    assert!(full_prompt.chars().count() > 80);

    let context = Context::from_blocks(vec![text_input(&full_prompt)]).unwrap();
    let session = host
        .create(PreparedCreate::persistent(test_definition(), context))
        .await
        .unwrap();

    let expected_title: String = full_prompt.chars().take(80).collect();
    assert_eq!(expected_title.chars().count(), 80);
    assert_eq!(session.title(), Some(expected_title.clone()));

    // Verify it persists and reopens
    let id = session.target().conversation_id().clone();
    drop(session);

    let reopened = host.open(id).await.unwrap();
    assert_eq!(reopened.title(), Some(expected_title));
}

#[tokio::test]
async fn test_be23_title_whitespace_only_remains_permanently_none() {
    let dir = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(dir.path().to_path_buf()));

    // Initial commit with whitespace-only user message
    let context = Context::from_blocks(vec![text_input("   \t  \n  \r\n   ")]).unwrap();
    let session = host
        .create(PreparedCreate::persistent(test_definition(), context))
        .await
        .unwrap();

    assert_eq!(session.title(), None);

    let id = session.target().conversation_id().clone();
    drop(session);

    let reopened = host.open(id).await.unwrap();
    assert_eq!(reopened.title(), None);

    // A later commit with a real user message does not add a title.
    append_user(&reopened, "A real request").await;
    assert_eq!(reopened.title(), None);
    let id = reopened.target().conversation_id().clone();
    drop(reopened);
    assert_eq!(host.open(id).await.unwrap().title(), None);

    let summary = host.list().await.unwrap().into_iter().next().unwrap();
    assert_eq!(summary.record.unwrap().title, None);
}

/// Append one user block through a material edit and wait for its commit.
async fn append_user(session: &tars_core::application::ManagedSession, text: &str) {
    let snapshot = session.read_materials().unwrap();
    let mut blocks = snapshot.context.blocks().to_vec();
    blocks.push(text_input(text));
    session.edit(snapshot.version, blocks).unwrap();
    session.wait_saved().await.unwrap();
}

#[tokio::test]
async fn title_is_set_by_the_first_commit_with_a_user_block_and_never_changes() {
    let dir = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(dir.path().to_path_buf()));
    let session = host
        .create(PreparedCreate::persistent(
            test_definition(),
            Context::new(),
        ))
        .await
        .unwrap();
    session.wait_saved().await.unwrap();
    assert_eq!(session.title(), None);

    append_user(&session, "First request\nmore detail").await;
    assert_eq!(session.title(), Some("First request".into()));

    // Replacing every block keeps the stored title.
    let snapshot = session.read_materials().unwrap();
    session
        .edit(snapshot.version, vec![text_input("Something else")])
        .unwrap();
    session.wait_saved().await.unwrap();
    assert_eq!(session.title(), Some("First request".into()));

    let id = session.target().conversation_id().clone();
    drop(session);
    let summary = host.list().await.unwrap().into_iter().next().unwrap();
    assert_eq!(summary.record.unwrap().title, Some("First request".into()));
    assert_eq!(
        host.open(id).await.unwrap().title(),
        Some("First request".into())
    );
}

#[tokio::test]
async fn every_commit_advances_the_last_activity_time() {
    let dir = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(dir.path().to_path_buf()));
    let session = host
        .create(PreparedCreate::persistent(
            test_definition(),
            Context::new(),
        ))
        .await
        .unwrap();
    session.wait_saved().await.unwrap();
    let created = session.created_at_ms().unwrap();
    let mut previous = session.updated_at_ms();
    assert!(previous >= created);

    for index in 0..3 {
        // Make sure the clock moves so a stale time would be visible.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        append_user(&session, &format!("message {index}")).await;
        let current = session.updated_at_ms();
        assert!(current > previous, "{current} must be after {previous}");
        previous = current;
    }

    let summary = host.list().await.unwrap().into_iter().next().unwrap();
    assert_eq!(summary.record.unwrap().updated_at_ms, previous);
}

#[tokio::test]
async fn test_be23_title_whitespace_collapsing() {
    let dir = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(dir.path().to_path_buf()));

    let context = Context::from_blocks(vec![text_input(
        "   hello \t\t  world   this    is   a \t test   ",
    )])
    .unwrap();

    let session = host
        .create(PreparedCreate::persistent(test_definition(), context))
        .await
        .unwrap();

    assert_eq!(session.title(), Some("hello world this is a test".into()));
}

#[tokio::test]
async fn test_be23_updated_at_ms_on_create_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(dir.path().to_path_buf()));

    let context = Context::from_blocks(vec![text_input("First message")]).unwrap();
    let session = host
        .create(PreparedCreate::persistent(test_definition(), context))
        .await
        .unwrap();

    let t1 = session.updated_at_ms();
    assert!(t1 > 0);

    let id = session.target().conversation_id().clone();
    drop(session);

    let reopened = host.open(id).await.unwrap();
    let t2 = reopened.updated_at_ms();
    assert_eq!(t2, t1);

    let summary = host.list().await.unwrap().into_iter().next().unwrap();
    assert_eq!(summary.record.unwrap().updated_at_ms, t1);
}

#[test]
fn test_be23_created_at_from_id_and_time_format() {
    let v7 = uuid::Uuid::now_v7();
    let id_str = v7.to_string();
    let timestamp = created_at_from_id(&id_str);
    assert!(timestamp.is_some());
    let ts = timestamp.unwrap();
    assert!(ts > 1_700_000_000_000); // After year 2023

    let formatted = format_timestamp_utc(ts);
    assert!(formatted.ends_with(" UTC"));
    assert_eq!(formatted.len(), 23); // YYYY-MM-DD HH:MM:SS UTC

    // Non-v7 UUID returns None
    let v4 = uuid::Uuid::new_v4().to_string();
    assert_eq!(created_at_from_id(&v4), None);
    assert_eq!(created_at_from_id("not-a-uuid"), None);
}
