//! Partial compaction on the wire: the fixed prefix, the brief request and
//! the next round after a summary, on all three protocols.
#![cfg(any(target_os = "linux", target_os = "macos"))]

#[path = "common/workspace_http.rs"]
mod http;

use causa::kernel::{BlockContent, BlockId, Context, ContextBlock, Role};
use http::{Fixture, Reply};
use serde_json::Value;
use std::time::Duration;
use tars_core::application::{PreparedCreate, SessionHost, text_input};
use tars_core::compact::SUMMARY_SOURCE;
use tars_core::definition::{CredentialRef, Provider, SessionDefinition, WorkspaceTool};
use tars_core::session::{SubmitRequest, WorkState};

const PROTOCOLS: [(&str, Provider); 3] = [
    ("openai-chat", Provider::OpenAiChat),
    ("openai-responses", Provider::OpenAiResponses),
    ("anthropic", Provider::Anthropic),
];

const BRIEF: &str = "Goal: finish the wire fixture. Progress: the marker was read once.";
const REQUEST: &str = "Read the marker twice, then answer.";

fn definition(provider: Provider, endpoint: &str, root: &std::path::Path) -> SessionDefinition {
    SessionDefinition {
        provider,
        model: "compaction-wire-fixture".into(),
        endpoint: Some(endpoint.into()),
        credential: CredentialRef::Environment("PATH".into()),
        work_timeout_secs: Some(20),
        retained_work_capacity: 8,
        max_model_rounds: 4,
        max_tool_calls: 3,
        max_tokens: (provider == Provider::Anthropic).then_some(1024),
        reasoning_effort: None,
        extra_body: Default::default(),
        max_model_attempts: None,
        render_options: Default::default(),
        workspace_root: Some(root.canonicalize().unwrap()),
        tools: vec![WorkspaceTool::ReadFile],
        tool_limits: Default::default(),
        write_limits: None,
        permission_mode: None,
        command: None,
        mcp_servers: Vec::new(),
        // A 6,000-token line. The earlier conversation, the request and the
        // first read stay under it; the second read crosses it, so the
        // summary runs before round 2 with the request in progress.
        context_window: Some(12_000),
        auto_compact: true,
        compact_threshold: 50,
        prompt_cache: true,
    }
}

fn block(role: Role, text: String) -> ContextBlock {
    ContextBlock::text(causa::runtime::new_block_id(), role, text)
}

/// What one work that compacts in round 2 sent and left behind.
struct Run {
    /// Request bodies: round 0, round 1, the brief request, round 2.
    bodies: Vec<Value>,
    system: Vec<ContextBlock>,
    earlier: Vec<ContextBlock>,
    request: BlockId,
    context: Context,
    removed: Vec<BlockId>,
    ranges: usize,
}

async fn run(name: &str, provider: Provider, with_system: bool) -> Run {
    let root = tempfile::tempdir().unwrap();
    // One long line, about 1,000 tokens per read.
    std::fs::write(
        root.path().join("marker.txt"),
        format!("{}\n", "m".repeat(3_000)),
    )
    .unwrap();
    let fixture = Fixture::new(
        name,
        vec![
            Reply::Read("marker.txt"),
            Reply::Read("marker.txt"),
            Reply::Summary(BRIEF),
            Reply::Answer,
        ],
        None,
    );
    let system = if with_system {
        vec![
            block(Role::System, "You are the compaction wire fixture.".into()),
            block(Role::System, "Project rules: answer in one line.".into()),
        ]
    } else {
        Vec::new()
    };
    let earlier = vec![
        block(Role::User, "earlier question ".repeat(620)),
        block(Role::Assistant, "earlier answer".into()),
    ];
    let mut blocks = system.clone();
    blocks.extend(earlier.iter().cloned());
    let store = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(store.path().to_owned()));
    let session = host
        .create(PreparedCreate::persistent(
            definition(provider, &fixture.endpoint, root.path()),
            Context::from_blocks(blocks).unwrap(),
        ))
        .await
        .unwrap();
    let request = text_input(REQUEST);
    let request_id = request.id();
    let work = session
        .submit(SubmitRequest {
            request_key: "wire".into(),
            blocks: vec![request],
        })
        .unwrap()
        .work;
    let observation = session.wait(&work, Duration::from_secs(20)).await.unwrap();
    assert!(
        matches!(observation.state, WorkState::Finished(_)),
        "{name}: {:?} after {:?}",
        observation.state,
        fixture.requests.lock().unwrap()
    );
    session.wait_saved().await.unwrap();
    let bodies: Vec<Value> = fixture
        .raw_requests
        .lock()
        .unwrap()
        .iter()
        .map(|raw| serde_json::from_slice(raw).unwrap())
        .collect();
    assert_eq!(bodies.len(), 4, "{name}: two reads, one brief, one answer");
    let history = session.history();
    assert_eq!(history.len(), 1, "{name}");
    let record = session.read_history_record(&history[0].record_id).unwrap();
    Run {
        bodies,
        system,
        earlier,
        request: request_id,
        context: session.read_materials().unwrap().context,
        removed: record
            .replacements
            .iter()
            .flat_map(|replacement| replacement.originals.iter().map(ContextBlock::id))
            .collect(),
        ranges: record.replacements.len(),
    }
}

/// The body with every `cache_control` member removed: breakpoints move
/// with the end of the request and are not part of its content.
fn without_cache_control(mut value: Value) -> Value {
    fn strip(value: &mut Value) {
        match value {
            Value::Object(map) => {
                map.remove("cache_control");
                map.values_mut().for_each(strip);
            }
            Value::Array(items) => items.iter_mut().for_each(strip),
            _ => {}
        }
    }
    strip(&mut value);
    value
}

/// The rendered system prompt: Anthropic `system`, Responses
/// `instructions`, or Chat's leading system messages.
fn system_prompt(provider: Provider, body: &Value) -> Value {
    let body = without_cache_control(body.clone());
    match provider {
        Provider::Anthropic => body.get("system").cloned().unwrap_or(Value::Null),
        Provider::OpenAiResponses => body.get("instructions").cloned().unwrap_or(Value::Null),
        _ => Value::Array(
            body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .take_while(|message| message["role"] == "system")
                .cloned()
                .collect(),
        ),
    }
}

/// The conversation items: `messages`, or Responses `input`.
fn items(provider: Provider, body: &Value) -> Vec<Value> {
    let body = without_cache_control(body.clone());
    let key = if provider == Provider::OpenAiResponses {
        "input"
    } else {
        "messages"
    };
    body[key].as_array().unwrap().clone()
}

/// Remove the trailing brief instruction from the items of a brief request.
///
/// Anthropic joins the instruction to the preceding user turn as one more
/// content part; the other protocols send it as its own item.
fn without_instruction(mut items: Vec<Value>) -> Vec<Value> {
    let is_instruction = |value: &Value| value.to_string().contains("Prepare a continuation brief");
    let last = items.last_mut().expect("brief request items");
    if let Some(parts) = last.get_mut("content").and_then(Value::as_array_mut)
        && parts.len() > 1
        && parts.last().is_some_and(is_instruction)
        && !parts[..parts.len() - 1].iter().any(is_instruction)
    {
        parts.pop();
        return items;
    }
    assert!(is_instruction(last), "the instruction comes last: {last}");
    items.pop();
    items
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_be12_a_round_two_summary_keeps_the_request_and_the_next_round_renders() {
    for (name, provider) in PROTOCOLS {
        let run = run(name, provider, true).await;
        let blocks = run.context.blocks();
        // [system, system, declaration, brief, request, call, result, answer]
        assert_eq!(blocks.len(), 8, "{name}: {blocks:?}");
        assert!(
            matches!(blocks[2].content(), BlockContent::ToolChange(_)),
            "{name}"
        );
        assert_eq!(
            blocks[3].meta().source.as_deref(),
            Some(SUMMARY_SOURCE),
            "{name}"
        );
        assert_eq!(
            blocks[4].id(),
            run.request,
            "{name}: the request keeps its id"
        );
        assert!(
            matches!(blocks[5].content(), BlockContent::ToolCall(_)),
            "{name}"
        );
        assert!(
            matches!(blocks[6].content(), BlockContent::ToolResult(_)),
            "{name}"
        );
        let brief = serde_json::to_string(&blocks[3]).unwrap();
        assert!(brief.contains(BRIEF), "{name}");
        assert!(
            !brief.contains(REQUEST),
            "{name}: the brief does not copy the request"
        );
        // Both segments around the request went into one record.
        assert_eq!(run.ranges, 2, "{name}");
        assert_eq!(run.removed.len(), 6, "{name}: {:?}", run.removed);
        assert_eq!(
            run.removed[..2],
            [run.earlier[0].id(), run.earlier[1].id()],
            "{name}"
        );
        assert_eq!(
            run.removed[2], run.request,
            "{name}: put back after the brief"
        );

        // The next round went out with the brief before the request and the
        // provider accepted it: the work finished with the fixture answer.
        let round_two = run.bodies[3].to_string();
        let brief_at = round_two.find("Goal: finish the wire fixture").unwrap();
        let request_at = round_two.find(REQUEST).unwrap();
        assert!(brief_at < request_at, "{name}");
        assert!(!round_two.contains("earlier question"), "{name}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_be25_the_system_prompt_and_tools_render_the_same_before_and_after_a_summary() {
    for (name, provider) in PROTOCOLS {
        let run = run(name, provider, true).await;
        let blocks = run.context.blocks();
        assert_eq!(
            &blocks[..2],
            &run.system[..],
            "{name}: same ids and content"
        );
        for system in &run.system {
            assert!(!run.removed.contains(&system.id()), "{name}");
        }
        let (before, after) = (&run.bodies[1], &run.bodies[3]);
        let prompt = system_prompt(provider, before);
        assert!(
            prompt
                .to_string()
                .contains("You are the compaction wire fixture."),
            "{name}: {prompt}"
        );
        assert_eq!(prompt, system_prompt(provider, after), "{name}");
        assert_eq!(prompt, system_prompt(provider, &run.bodies[0]), "{name}");
        assert_eq!(
            without_cache_control(before["tools"].clone()),
            without_cache_control(after["tools"].clone()),
            "{name}: the tool declarations render the same"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_be25_a_context_without_system_blocks_renders_no_system_prompt_after_a_summary() {
    for (name, provider) in PROTOCOLS {
        let run = run(name, provider, false).await;
        for body in &run.bodies {
            match provider {
                Provider::Anthropic => assert!(body.get("system").is_none(), "{name}: {body}"),
                Provider::OpenAiResponses => {
                    assert!(body.get("instructions").is_none(), "{name}: {body}")
                }
                _ => assert!(
                    body["messages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .all(|message| message["role"] != "system"),
                    "{name}: {body}"
                ),
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_be26_the_brief_request_is_the_previous_round_up_to_the_cut_point() {
    for (name, provider) in PROTOCOLS {
        let run = run(name, provider, true).await;
        let (previous, brief) = (&run.bodies[1], &run.bodies[2]);
        // Round 1 sent everything before the second call, which is where the
        // retained tail starts: the brief request repeats it item for item,
        // the request in progress at the same position, then the instruction.
        assert_eq!(
            without_instruction(items(provider, brief)),
            items(provider, previous),
            "{name}"
        );
        assert_eq!(
            system_prompt(provider, brief),
            system_prompt(provider, previous),
            "{name}"
        );
        assert_eq!(
            without_cache_control(brief["tools"].clone()),
            without_cache_control(previous["tools"].clone()),
            "{name}"
        );
        assert_eq!(brief["model"], previous["model"], "{name}");
    }
}
