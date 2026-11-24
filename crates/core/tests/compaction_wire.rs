//! Partial compaction on the wire: the fixed prefix, the brief request, the
//! merge of the brief chain and the next round after them, on all three
//! protocols.
#![cfg(any(target_os = "linux", target_os = "macos"))]

#[path = "common/workspace_http.rs"]
mod http;

use causa::kernel::{BlockContent, BlockId, BlockMeta, Context, ContextBlock, Role};
use http::{Fixture, Reply};
use serde_json::Value;
use std::time::Duration;
use tars_core::application::{PreparedCreate, SessionHost, text_input};
use tars_core::compact::{HistoryRecord, RewriteKind, SUMMARY_SOURCE};
use tars_core::definition::{CredentialRef, Provider, SessionDefinition, WorkspaceTool};
use tars_core::session::{SubmitRequest, WorkState};

const PROTOCOLS: [(&str, Provider); 3] = [
    ("openai-chat", Provider::OpenAiChat),
    ("openai-responses", Provider::OpenAiResponses),
    ("anthropic", Provider::Anthropic),
];

const BRIEF: &str = "Goal: finish the wire fixture. Progress: the marker was read once.";
const MERGED: &str = "Goal: finish the wire fixture. Progress: merged from three briefs.";
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
        temperature: None,
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
        prelude: tars_core::prelude::PreludeDefinition {
            agents_enabled: false,
            ..Default::default()
        },
        code_execution: None,
    }
}

fn block(role: Role, text: String) -> ContextBlock {
    ContextBlock::text(causa::runtime::new_block_id(), role, text)
}

/// What one work that compacts in round 2 sent and left behind.
struct Run {
    /// Request bodies: round 0, round 1, the brief request, then the merge
    /// request when the chain was seeded, and round 2.
    bodies: Vec<Value>,
    system: Vec<ContextBlock>,
    earlier: Vec<ContextBlock>,
    /// Briefs seeded into the chain before the work.
    seeded: Vec<ContextBlock>,
    request: BlockId,
    context: Context,
    /// Originals of the summary record.
    removed: Vec<BlockId>,
    ranges: usize,
    records: Vec<HistoryRecord>,
}

/// A brief as an earlier compaction would have left it (~1,400 tokens).
fn seeded_brief(index: usize) -> ContextBlock {
    ContextBlock::new(
        causa::runtime::new_block_id(),
        BlockContent::text(
            Role::User,
            format!("Seeded brief {index}. {}", "earlier work ".repeat(310)),
        ),
        BlockMeta::from_source(SUMMARY_SOURCE),
    )
}

async fn run(name: &str, provider: Provider, with_system: bool) -> Run {
    run_with(name, provider, with_system, false).await
}

/// One work that compacts in round 2. With `chain`, two briefs of about
/// 1,400 tokens each are seeded after the system blocks; with the new brief
/// the chain is over its budget, (6,000 − prefix) / 2, and is merged.
async fn run_with(name: &str, provider: Provider, with_system: bool, chain: bool) -> Run {
    let root = tempfile::tempdir().unwrap();
    // One long line, about 1,000 tokens per read.
    std::fs::write(
        root.path().join("marker.txt"),
        format!("{}\n", "m".repeat(3_000)),
    )
    .unwrap();
    let mut replies = vec![
        Reply::Read("marker.txt"),
        Reply::Read("marker.txt"),
        Reply::Summary(BRIEF),
    ];
    if chain {
        replies.push(Reply::Summary(MERGED));
    }
    replies.push(Reply::Answer);
    let fixture = Fixture::new(name, replies, None);
    let system = if with_system {
        vec![
            block(Role::System, "You are the compaction wire fixture.".into()),
            block(Role::System, "Project rules: answer in one line.".into()),
        ]
    } else {
        Vec::new()
    };
    let seeded: Vec<ContextBlock> = if chain {
        (0..2).map(seeded_brief).collect()
    } else {
        Vec::new()
    };
    // The seeded chain takes the place of part of the earlier question, so
    // the rounds cross the line at the same point.
    let earlier = vec![
        block(
            Role::User,
            "earlier question ".repeat(if chain { 120 } else { 620 }),
        ),
        block(Role::Assistant, "earlier answer".into()),
    ];
    let mut blocks = system.clone();
    blocks.extend(seeded.iter().cloned());
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
    let briefs = if chain { 2 } else { 1 };
    assert_eq!(
        bodies.len(),
        3 + briefs,
        "{name}: two reads, the brief requests, one answer"
    );
    let history = session.history();
    assert_eq!(history.len(), briefs, "{name}");
    let records: Vec<HistoryRecord> = history
        .iter()
        .map(|entry| session.read_history_record(&entry.record_id).unwrap())
        .collect();
    let record = &records[0];
    Run {
        bodies,
        system,
        earlier,
        seeded,
        request: request_id,
        context: session.read_materials().unwrap().context,
        removed: record
            .replacements
            .iter()
            .flat_map(|replacement| replacement.originals.iter().map(ContextBlock::id))
            .collect(),
        ranges: record.replacements.len(),
        records,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_be13_a_chain_over_its_budget_is_merged_and_the_next_round_renders() {
    for (name, provider) in PROTOCOLS {
        let run = run_with(name, provider, true, true).await;
        let kinds: Vec<RewriteKind> = run.records.iter().map(|record| record.kind).collect();
        assert_eq!(
            kinds,
            [RewriteKind::Summarize, RewriteKind::Merge],
            "{name}"
        );

        // [system, system, declaration, merged brief, request, call, result, answer]
        let blocks = run.context.blocks();
        assert_eq!(blocks.len(), 8, "{name}: {blocks:?}");
        assert!(
            matches!(blocks[2].content(), BlockContent::ToolChange(_)),
            "{name}"
        );
        assert_eq!(blocks[4].id(), run.request, "{name}");
        let merged = serde_json::to_string(&blocks[3]).unwrap();
        assert!(merged.contains(MERGED), "{name}");
        let merge = &run.records[1];
        for old in &run.seeded {
            assert!(
                merged.contains(&format!("recall id={}/{}", merge.record_id, old.id().0)),
                "{name}: the merged brief points to each old one"
            );
        }
        // The merge record holds the chain: both seeded briefs, the
        // declaration the summary added and its brief.
        let originals = &merge.replacements[0].originals;
        assert_eq!(originals.len(), 4, "{name}");
        assert_eq!(originals[..2], run.seeded[..], "{name}");

        // The merge request is the head and the instruction; the next round
        // went out with the merged brief only and was accepted.
        let merge_request = run.bodies[3].to_string();
        assert!(
            merge_request.contains("merging the working briefs"),
            "{name}"
        );
        assert!(merge_request.contains("Seeded brief 1"), "{name}");
        assert!(!merge_request.contains("earlier question"), "{name}");
        let round_two = run.bodies[4].to_string();
        assert!(round_two.contains(MERGED), "{name}");
        assert!(!round_two.contains("Seeded brief"), "{name}");
        assert!(!round_two.contains(BRIEF), "{name}");
        assert!(
            round_two.find(MERGED).unwrap() < round_two.find(REQUEST).unwrap(),
            "{name}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_be27_transient_trimming_drops_middle_system_note_and_keeps_fixed_prefix_on_wire() {
    for (name, provider) in PROTOCOLS {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("marker.txt"), "marker-ok\n").unwrap();
        let fixture = Fixture::new(name, vec![Reply::Read("marker.txt"), Reply::Answer], None);
        let sys0 = block(Role::System, "You are the compaction wire fixture.".into());
        let sys1 = block(Role::System, "Project rules: answer in one line.".into());
        let earlier_user = block(Role::User, "earlier question ".repeat(600));
        let earlier_assistant = block(Role::Assistant, "earlier answer".into());
        let middle_note = block(
            Role::System,
            "repair note: earlier unpaired call removed".into(),
        );
        let blocks = vec![
            sys0.clone(),
            sys1.clone(),
            earlier_user.clone(),
            earlier_assistant,
            middle_note.clone(),
        ];

        let mut def = definition(provider, &fixture.endpoint, root.path());
        def.context_window = Some(2_000);
        def.max_tokens = Some(500);
        let host = SessionHost::new(None);
        let session = host
            .create(PreparedCreate::transient(
                def,
                Context::from_blocks(blocks).unwrap(),
            ))
            .await
            .unwrap();
        let request = text_input(REQUEST);
        let request_id = request.id();
        let work = session
            .submit(SubmitRequest {
                request_key: "be27-transient-wire".into(),
                blocks: vec![request],
            })
            .unwrap()
            .work;
        let observation = session.wait(&work, Duration::from_secs(20)).await.unwrap();
        assert!(
            matches!(observation.state, WorkState::Finished(_)),
            "{name}: {:?}",
            observation.state
        );

        let bodies: Vec<Value> = fixture
            .raw_requests
            .lock()
            .unwrap()
            .iter()
            .map(|raw| serde_json::from_slice(raw).unwrap())
            .collect();
        assert_eq!(bodies.len(), 2, "{name}: zero brief calls in transient");

        for body in &bodies {
            let prompt = system_prompt(provider, body);
            let prompt_text = prompt.to_string();
            assert!(
                prompt_text.contains("You are the compaction wire fixture.")
                    && prompt_text.contains("Project rules: answer in one line."),
                "{name}: fixed prefix preserved: {prompt}"
            );
            assert!(
                !prompt_text.contains("repair note"),
                "{name}: middle system note must not enter leading system prompt: {prompt}"
            );
            let serialized = body.to_string();
            assert!(
                !serialized.contains("repair note"),
                "{name}: middle system note dropped with earlier conversation"
            );
            assert!(
                !serialized.contains("earlier question"),
                "{name}: earlier conversation dropped"
            );
            assert!(serialized.contains(REQUEST), "{name}: U protected");
            assert!(serialized.contains("read_file"), "{name}: bound tools stay");
            assert!(
                !serialized.contains("\"recall\""),
                "{name}: no recall in transient"
            );
        }

        // Compare rendered system prompt against a clean untrimmed frame containing only [sys0, sys1, U].
        let baseline_fixture = Fixture::new(name, vec![Reply::Answer], None);
        let mut baseline_def = definition(provider, &baseline_fixture.endpoint, root.path());
        baseline_def.context_window = None;
        baseline_def.max_tokens = Some(500);
        let baseline_session = host
            .create(PreparedCreate::transient(
                baseline_def,
                Context::from_blocks(vec![sys0, sys1]).unwrap(),
            ))
            .await
            .unwrap();
        let baseline_work = baseline_session
            .submit(SubmitRequest {
                request_key: "be27-baseline".into(),
                blocks: vec![text_input(REQUEST)],
            })
            .unwrap()
            .work;
        let baseline_obs = baseline_session
            .wait(&baseline_work, Duration::from_secs(20))
            .await
            .unwrap();
        assert!(matches!(baseline_obs.state, WorkState::Finished(_)));
        let baseline_body: Value =
            serde_json::from_slice(&baseline_fixture.raw_requests.lock().unwrap()[0]).unwrap();
        assert_eq!(
            system_prompt(provider, &bodies[0]),
            system_prompt(provider, &baseline_body),
            "{name}: leading system prompt must match Context fixed prefix byte-for-byte"
        );

        // Session Context itself stays completely untouched.
        let final_blocks = session.read_materials().unwrap().context;
        assert!(
            final_blocks
                .blocks()
                .iter()
                .any(|b| b.id() == earlier_user.id())
        );
        assert!(
            final_blocks
                .blocks()
                .iter()
                .any(|b| b.id() == middle_note.id())
        );
        assert!(final_blocks.blocks().iter().any(|b| b.id() == request_id));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_be25_the_system_prompt_and_tools_render_the_same_after_a_merge() {
    for (name, provider) in PROTOCOLS {
        let run = run_with(name, provider, true, true).await;
        let blocks = run.context.blocks();
        assert_eq!(
            &blocks[..2],
            &run.system[..],
            "{name}: same ids and content"
        );
        for record in &run.records {
            for replacement in &record.replacements {
                for system in &run.system {
                    assert!(
                        !replacement
                            .originals
                            .iter()
                            .any(|block| block.id() == system.id()),
                        "{name}"
                    );
                }
            }
        }
        let prompt = system_prompt(provider, &run.bodies[1]);
        assert!(
            prompt
                .to_string()
                .contains("You are the compaction wire fixture."),
            "{name}: {prompt}"
        );
        // Before the summary, the merge request and the round after it.
        for body in &run.bodies[3..] {
            assert_eq!(prompt, system_prompt(provider, body), "{name}");
            assert_eq!(
                without_cache_control(run.bodies[1]["tools"].clone()),
                without_cache_control(body["tools"].clone()),
                "{name}: the tool declarations render the same"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn round_zero_large_work_input_preserves_prior_turn_prefix_on_wire() {
    const LARGE_U2_MARKER: &str = "round-two-large-input-marker";
    for (name, provider) in PROTOCOLS {
        let root = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(
            name,
            vec![
                Reply::Answer,         // Turn 1 round 0
                Reply::Summary(BRIEF), // Turn 2 round 0 compaction brief
                Reply::Answer,         // Turn 2 round 0 conversation
            ],
            None,
        );
        let sys0 = block(Role::System, "You are the compaction wire fixture.".into());
        let sys1 = block(Role::System, "Project rules: answer in one line.".into());
        let earlier_u = block(Role::User, "earlier question ".repeat(620));
        let earlier_a = block(Role::Assistant, "earlier answer".into());

        let store = tempfile::tempdir().unwrap();
        let host = SessionHost::new(Some(store.path().to_owned()));
        let session = host
            .create(PreparedCreate::persistent(
                definition(provider, &fixture.endpoint, root.path()),
                Context::from_blocks(vec![sys0.clone(), sys1.clone(), earlier_u, earlier_a])
                    .unwrap(),
            ))
            .await
            .unwrap();

        // Turn 1: stays below the 6,000-token trigger line, no compaction.
        let turn1 = session
            .submit(SubmitRequest {
                request_key: "turn-1".into(),
                blocks: vec![text_input("Turn 1 short question.")],
            })
            .unwrap()
            .work;
        let obs1 = session.wait(&turn1, Duration::from_secs(20)).await.unwrap();
        assert!(matches!(obs1.state, WorkState::Finished(_)), "{name}");
        session.wait_saved().await.unwrap();
        assert!(
            session.history().is_empty(),
            "{name}: turn 1 must not compact"
        );

        // Turn 2 Round 0: large U2 (> retained_tail_budget ~1,400 tokens) crosses the trigger line.
        let large_u2_text = format!("{LARGE_U2_MARKER} {}", "x".repeat(8_000));
        let u2 = text_input(&large_u2_text);
        let u2_id = u2.id();
        let turn2 = session
            .submit(SubmitRequest {
                request_key: "turn-2".into(),
                blocks: vec![u2],
            })
            .unwrap()
            .work;
        let obs2 = session.wait(&turn2, Duration::from_secs(20)).await.unwrap();
        assert!(matches!(obs2.state, WorkState::Finished(_)), "{name}");
        session.wait_saved().await.unwrap();

        let bodies: Vec<Value> = fixture
            .raw_requests
            .lock()
            .unwrap()
            .iter()
            .map(|raw| serde_json::from_slice(raw).unwrap())
            .collect();
        assert_eq!(
            bodies.len(),
            3,
            "{name}: turn 1, brief request, turn 2 conversation"
        );

        let turn1_body = &bodies[0];
        let brief_body = &bodies[1];
        let turn2_body = &bodies[2];

        // Brief request matches Turn 1 conversation request + Turn 1 reply, with NO U2 anywhere.
        let brief_serialized = brief_body.to_string();
        assert!(
            !brief_serialized.contains(LARGE_U2_MARKER),
            "{name}: brief request must not contain U2"
        );
        assert!(
            !brief_serialized.contains("request still in progress"),
            "{name}: round-0 brief instruction must not claim request in progress"
        );
        assert_eq!(
            system_prompt(provider, brief_body),
            system_prompt(provider, turn1_body),
            "{name}: brief system prompt matches Turn 1"
        );
        assert_eq!(
            without_cache_control(brief_body["tools"].clone()),
            without_cache_control(turn1_body["tools"].clone()),
            "{name}: brief tools match Turn 1"
        );

        let turn1_items = items(provider, turn1_body);
        let brief_items = without_instruction(items(provider, brief_body));
        assert_eq!(
            brief_items.len(),
            turn1_items.len() + 1,
            "{name}: Turn 1 items plus Turn 1 assistant reply"
        );
        assert_eq!(
            brief_items[..turn1_items.len()],
            turn1_items[..],
            "{name}: brief prefix matches Turn 1 request items byte-for-byte"
        );
        assert!(
            brief_items
                .last()
                .unwrap()
                .to_string()
                .contains("checked workspace"),
            "{name}: brief ends with Turn 1 assistant reply"
        );

        // Turn 2 conversation request carries [brief, U2] and succeeds.
        let turn2_serialized = turn2_body.to_string();
        let brief_pos = turn2_serialized.find(BRIEF).expect("brief in turn 2");
        let u2_pos = turn2_serialized
            .find(LARGE_U2_MARKER)
            .expect("U2 in turn 2");
        assert!(
            brief_pos < u2_pos,
            "{name}: brief precedes U2 in Turn 2 request"
        );
        assert!(
            !turn2_serialized.contains("earlier question"),
            "{name}: earlier question compacted away"
        );

        // U2 keeps its original BlockId in the final Context.
        let final_context = session.read_materials().unwrap().context;
        let final_blocks = final_context.blocks();
        assert!(
            final_blocks.iter().any(|b| b.id() == u2_id),
            "{name}: U2 keeps its original BlockId"
        );
        assert_eq!(final_blocks.len(), 6, "{name}: {final_blocks:?}");
        assert_eq!(&final_blocks[..2], &[sys0, sys1], "{name}");
        assert!(
            matches!(final_blocks[2].content(), BlockContent::ToolChange(_)),
            "{name}"
        );
        assert_eq!(
            final_blocks[3].meta().source.as_deref(),
            Some(SUMMARY_SOURCE),
            "{name}"
        );
        assert_eq!(final_blocks[4].id(), u2_id, "{name}");
    }
}

fn read_tool_pair(path: &str, body_bytes: usize, marker: &str) -> (ContextBlock, ContextBlock) {
    let call_id = causa::runtime::new_block_id();
    let call = ContextBlock::new(
        call_id,
        BlockContent::ToolCall(causa::kernel::ToolCallPayload::new(
            "read_file",
            serde_json::json!({ "path": path, "start_line": 1, "max_lines": 100 }),
        )),
        BlockMeta::default(),
    );
    let result = ContextBlock::new(
        causa::runtime::new_block_id(),
        BlockContent::ToolResult(causa::kernel::ToolResultPayload::new(
            call_id,
            causa::kernel::ToolResultStatus::Succeeded,
            causa::kernel::ToolOutput::new(serde_json::json!({
                "path": path,
                "start_line": 1,
                "end_line": 100,
                "text": format!("{marker} {}", "m".repeat(body_bytes)),
            })),
        )),
        BlockMeta::default(),
    );
    (call, result)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_mc09_hot_and_cold_micro_compaction_on_all_three_wire_protocols() {
    for (name, provider) in PROTOCOLS {
        // 1. Hot-cache micro-compaction -> recall -> subsequent Summarize across all 3 protocols.
        let root = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(
            name,
            vec![
                Reply::RecallFirstCleared,
                Reply::Answer,
                Reply::Summary(BRIEF),
                Reply::Answer,
            ],
            None,
        );

        let mut def = definition(provider, &fixture.endpoint, root.path());
        def.context_window = Some(10_000);
        def.compact_threshold = 70;
        def.max_tokens = (provider == Provider::Anthropic).then_some(1024);

        let sys = block(
            Role::System,
            "You are the micro-compaction wire fixture.".into(),
        );
        let mut blocks = vec![sys, block(Role::User, "read four files".into())];
        for idx in 0..4 {
            let (call, result) = read_tool_pair(
                &format!("src/wire_{idx}.rs"),
                4_800,
                &format!("MC09_HOT_ORIGINAL_{idx}"),
            );
            blocks.push(call);
            blocks.push(result);
        }
        blocks.push(block(Role::Assistant, "done reading".into()));

        let store = tempfile::tempdir().unwrap();
        let host = SessionHost::new(Some(store.path().to_owned()));
        let session = host
            .create(PreparedCreate::persistent(
                def,
                Context::from_blocks(blocks).unwrap(),
            ))
            .await
            .unwrap();

        let turn1 = session
            .submit(SubmitRequest {
                request_key: "mc09-hot-turn".into(),
                blocks: vec![text_input("Recall the first read output and answer.")],
            })
            .unwrap()
            .work;
        let obs1 = session.wait(&turn1, Duration::from_secs(20)).await.unwrap();
        assert!(
            matches!(obs1.state, WorkState::Finished(_)),
            "{name}: {obs1:?}"
        );
        session.wait_saved().await.unwrap();

        assert_eq!(
            session.history().len(),
            1,
            "{name}: Turn 1 micro-compacted once"
        );
        assert_eq!(
            session.history()[0].kind,
            RewriteKind::MicroCompact,
            "{name}"
        );

        // Turn 2: large user message pushes over threshold -> falls back to Summarize.
        let turn2 = session
            .submit(SubmitRequest {
                request_key: "mc09-hot-summarize-turn".into(),
                blocks: vec![text_input(format!("Summarize now {}", "s".repeat(22_000)))],
            })
            .unwrap()
            .work;
        let obs2 = session.wait(&turn2, Duration::from_secs(20)).await.unwrap();
        assert!(
            matches!(obs2.state, WorkState::Finished(_)),
            "{name}: {obs2:?}"
        );
        session.wait_saved().await.unwrap();

        let history = session.history();
        assert_eq!(history.len(), 2, "{name}: MicroCompact then Summarize");
        assert_eq!(history[0].kind, RewriteKind::MicroCompact, "{name}");
        assert_eq!(history[1].kind, RewriteKind::Summarize, "{name}");

        let bodies: Vec<Value> = fixture
            .raw_requests
            .lock()
            .unwrap()
            .iter()
            .map(|raw| serde_json::from_slice(raw).unwrap())
            .collect();
        assert_eq!(
            bodies.len(),
            4,
            "{name}: Turn 1 R0 (micro-compacted), Turn 1 R1 (after recall), Turn 2 brief, Turn 2 R0"
        );
        let r0_str = bodies[0].to_string();
        assert!(r0_str.contains("[cleared tool_result:"), "{name}: {r0_str}");
        assert!(r0_str.contains("recall id="), "{name}: {r0_str}");
        assert!(!r0_str.contains("MC09_HOT_ORIGINAL_0"), "{name}: {r0_str}");
        assert!(
            bodies[1].to_string().contains("MC09_HOT_ORIGINAL_0"),
            "{name}: recall returned original output"
        );
        assert!(
            bodies[2]
                .to_string()
                .contains("Prepare a continuation brief"),
            "{name}: Turn 2 ran Summarize brief"
        );
        assert!(
            bodies[3].to_string().contains(BRIEF),
            "{name}: Turn 2 R0 sent brief"
        );
        host.close(&session.target(), tars_core::application::ClosePolicy::Save)
            .await
            .unwrap();

        // 2. Cold-cache micro-compaction after simulated >= 5 min idle gap across all 3 protocols.
        let cold_root = tempfile::tempdir().unwrap();
        let cold_fixture = Fixture::new(name, vec![Reply::Answer, Reply::Answer], None);
        let mut cold_def = definition(provider, &cold_fixture.endpoint, cold_root.path());
        cold_def.context_window = Some(20_000);
        cold_def.compact_threshold = 70;
        cold_def.max_tokens = (provider == Provider::Anthropic).then_some(1024);

        let mut cold_blocks = vec![
            block(Role::System, "You are the cold-cache wire fixture.".into()),
            block(Role::User, "read cold files".into()),
        ];
        for idx in 0..4 {
            let (call, res) = read_tool_pair(
                &format!("src/cold_{idx}.rs"),
                4_500,
                &format!("MC09_COLD_ORIGINAL_{idx}"),
            );
            cold_blocks.push(call);
            cold_blocks.push(res);
        }
        cold_blocks.push(block(Role::Assistant, "cold reads done".into()));

        let cold_store = tempfile::tempdir().unwrap();
        let cold_host = SessionHost::new(Some(cold_store.path().to_owned()));
        let cold_session = cold_host
            .create(PreparedCreate::persistent(
                cold_def,
                Context::from_blocks(cold_blocks).unwrap(),
            ))
            .await
            .unwrap();
        let cold_id = cold_session.target().conversation_id().clone();

        let warm = cold_session
            .submit(SubmitRequest {
                request_key: "mc09-warm".into(),
                blocks: vec![text_input("warm call")],
            })
            .unwrap()
            .work;
        let warm_obs = cold_session
            .wait(&warm, Duration::from_secs(20))
            .await
            .unwrap();
        assert!(matches!(warm_obs.state, WorkState::Finished(_)), "{name}");
        cold_session.wait_saved().await.unwrap();
        assert!(cold_session.history().is_empty(), "{name}");
        cold_host
            .close(
                &cold_session.target(),
                tars_core::application::ClosePolicy::Save,
            )
            .await
            .unwrap();

        // Age persisted last_call timestamp by 6 minutes in SQLite tars.db.
        let conn = rusqlite::Connection::open(cold_store.path().join("tars.db")).unwrap();
        let usage_json: String = conn
            .query_row(
                "SELECT usage_json FROM sessions WHERE session_id = ?1",
                rusqlite::params![cold_id.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        let mut usage_val: Value = serde_json::from_str(&usage_json).unwrap();
        let prev_ts = usage_val["last_call"]["timestamp_ms"]
            .as_u64()
            .expect("persisted last_call timestamp_ms");
        usage_val["last_call"]["timestamp_ms"] =
            serde_json::json!(prev_ts.saturating_sub(6 * 60 * 1_000));
        let aged_usage = serde_json::to_string(&usage_val).unwrap();
        conn.execute(
            "UPDATE sessions SET usage_json = ?1 WHERE session_id = ?2",
            rusqlite::params![&aged_usage, cold_id.as_str()],
        )
        .unwrap();
        conn.execute(
            "UPDATE agents SET usage_json = ?1 WHERE session_id = ?2 AND agent_id = ?2",
            rusqlite::params![&aged_usage, cold_id.as_str()],
        )
        .unwrap();
        drop(conn);

        let reopened = cold_host.open(cold_id).await.unwrap();
        let cold_turn = reopened
            .submit(SubmitRequest {
                request_key: "mc09-cold".into(),
                blocks: vec![text_input("cold resume")],
            })
            .unwrap()
            .work;
        let cold_obs = reopened
            .wait(&cold_turn, Duration::from_secs(20))
            .await
            .unwrap();
        assert!(matches!(cold_obs.state, WorkState::Finished(_)), "{name}");
        reopened.wait_saved().await.unwrap();

        let reopened_history = reopened.history();
        assert_eq!(reopened_history.len(), 1, "{name}");
        assert_eq!(
            reopened_history[0].kind,
            RewriteKind::MicroCompact,
            "{name}"
        );

        let cold_bodies: Vec<Value> = cold_fixture
            .raw_requests
            .lock()
            .unwrap()
            .iter()
            .map(|raw| serde_json::from_slice(raw).unwrap())
            .collect();
        assert_eq!(cold_bodies.len(), 2, "{name}: warm + cold (0 brief calls)");
        let cold_req_str = cold_bodies[1].to_string();
        assert!(cold_req_str.contains("[cleared tool_result:"), "{name}");
        assert!(!cold_req_str.contains("MC09_COLD_ORIGINAL_0"), "{name}");

        cold_host
            .close(
                &reopened.target(),
                tars_core::application::ClosePolicy::Save,
            )
            .await
            .unwrap();
    }
}
