use super::*;
use std::fs;
use std::path::Path;

fn write_skill(root: &Path, dir_name: &str, content: &str) -> std::path::PathBuf {
    let dir = root.join(dir_name);
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("SKILL.md");
    fs::write(&file, content).unwrap();
    file
}

#[test]
fn frontmatter_parses_valid_skill_and_unquoted_colons_in_description() {
    let doc = "\
---
name: rust-refactor
description: Use when: refactoring Rust modules: safely, or splitting mod.rs
license: MIT
metadata:
  author: tars
---
# Body heading

Step 1: inspect files.
";
    let parsed = parse_skill_document(doc, "rust-refactor").unwrap();
    assert_eq!(parsed.meta.name, "rust-refactor");
    assert_eq!(
        parsed.meta.description,
        "Use when: refactoring Rust modules: safely, or splitting mod.rs"
    );
    assert!(parsed.body.starts_with("# Body heading"));
}

#[test]
fn frontmatter_rejects_invalid_names_mismatches_and_bad_descriptions() {
    // Missing frontmatter
    assert_eq!(
        parse_skill_document("# No frontmatter", "alpha")
            .unwrap_err()
            .0,
        SkillDiagnosticKind::InvalidFrontmatter
    );
    // Name mismatch
    assert_eq!(
        parse_skill_document("---\nname: beta\ndescription: ok\n---\n", "alpha")
            .unwrap_err()
            .0,
        SkillDiagnosticKind::NameMismatch
    );
    // Leading/trailing hyphen, consecutive hyphens, uppercase, >64 chars
    for bad in [
        "-alpha",
        "alpha-",
        "a--b",
        "Alpha",
        "a_b",
        &"a".repeat(65),
        "",
    ] {
        let doc = format!("---\nname: {bad}\ndescription: valid desc\n---\n");
        let err = parse_skill_document(&doc, bad).unwrap_err();
        assert_eq!(err.0, SkillDiagnosticKind::InvalidName, "name={bad:?}");
    }
    // Blank description and >1024 chars
    assert_eq!(
        parse_skill_document("---\nname: alpha\ndescription: \"   \"\n---\n", "alpha")
            .unwrap_err()
            .0,
        SkillDiagnosticKind::InvalidDescription
    );
    let long_desc = "x".repeat(1025);
    let doc = format!("---\nname: alpha\ndescription: {long_desc}\n---\n");
    assert_eq!(
        parse_skill_document(&doc, "alpha").unwrap_err().0,
        SkillDiagnosticKind::InvalidDescription
    );
}

#[test]
fn multi_dir_discovery_precedence_dedup_sorting_and_resources() {
    let temp = tempfile::tempdir().unwrap();
    let dir_a = temp.path().join("a");
    let dir_b = temp.path().join("b");
    let ws = temp.path().join("workspace");
    let ws_skills = ws.join(".tars").join("skills");
    fs::create_dir_all(&dir_a).unwrap();
    fs::create_dir_all(&dir_b).unwrap();
    fs::create_dir_all(&ws_skills).unwrap();

    // dir_a has zeta, shared, and an invalid skill
    write_skill(
        &dir_a,
        "zeta",
        "---\nname: zeta\ndescription: from A\n---\nbody a\n",
    );
    write_skill(
        &dir_a,
        "shared",
        "---\nname: shared\ndescription: shared from A\n---\nold\n",
    );
    write_skill(
        &dir_a,
        "broken",
        "---\nname: wrong-name\ndescription: bad\n---\n",
    );

    // dir_b overrides shared, adds alpha
    write_skill(
        &dir_b,
        "shared",
        "---\nname: shared\ndescription: shared from B\n---\nnew b\n",
    );
    write_skill(
        &dir_b,
        "alpha",
        "---\nname: alpha\ndescription: from B\n---\nbody alpha\n",
    );
    // Resource directories under alpha (only paths listed, contents never read)
    fs::create_dir_all(dir_b.join("alpha/scripts/sub")).unwrap();
    fs::create_dir_all(dir_b.join("alpha/references")).unwrap();
    fs::create_dir_all(dir_b.join("alpha/assets")).unwrap();
    fs::write(dir_b.join("alpha/scripts/run.sh"), b"\xff\xfeinvalid utf8").unwrap();
    fs::write(dir_b.join("alpha/scripts/sub/helper.py"), b"print(1)").unwrap();
    fs::write(dir_b.join("alpha/references/guide.md"), b"ref").unwrap();
    fs::write(dir_b.join("alpha/assets/logo.bin"), [0u8, 1, 2]).unwrap();

    // workspace overrides shared with highest precedence
    write_skill(
        &ws_skills,
        "shared",
        "---\nname: shared\ndescription: shared from workspace\n---\nws body\n",
    );

    let discovery = SkillDiscovery::discover(&[dir_a, dir_b], Some(&ws), true);

    // Invalid skill produced a diagnostic without blocking valid ones
    assert_eq!(discovery.diagnostics.len(), 1);
    assert_eq!(
        discovery.diagnostics[0].kind,
        SkillDiagnosticKind::NameMismatch
    );

    let names: Vec<&str> = discovery.registry.names().collect();
    assert_eq!(names, vec!["alpha", "shared", "zeta"]);

    let shared = discovery.registry.get("shared").unwrap();
    assert_eq!(shared.description, "shared from workspace");
    assert_eq!(shared.sha256.len(), 64);

    let alpha = discovery.registry.get("alpha").unwrap();
    assert_eq!(
        alpha.resources.scripts,
        vec!["scripts/run.sh", "scripts/sub/helper.py"]
    );
    assert_eq!(alpha.resources.references, vec!["references/guide.md"]);
    assert_eq!(alpha.resources.assets, vec!["assets/logo.bin"]);

    // When include_workspace = false, dir_b wins for shared
    let no_ws = SkillDiscovery::discover(
        &[temp.path().join("a"), temp.path().join("b")],
        Some(&ws),
        false,
    );
    assert_eq!(
        no_ws.registry.get("shared").unwrap().description,
        "shared from B"
    );
}

#[test]
fn missing_workspace_skills_is_silently_skipped() {
    let temp = tempfile::tempdir().unwrap();
    let ws = temp.path().join("ws");
    fs::create_dir_all(&ws).unwrap();
    let discovery = SkillDiscovery::discover(&[], Some(&ws), true);
    assert!(discovery.registry.is_empty());
    assert!(discovery.diagnostics.is_empty());
}

#[cfg(unix)]
#[test]
fn symlinked_skill_dirs_work_while_escaping_skill_md_and_resource_cycles_are_guarded() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let skills_root = temp.path().join("skills");
    let external_skills = temp.path().join("external");
    let secret = temp.path().join("outside-secret.md");
    fs::create_dir_all(&skills_root).unwrap();
    fs::create_dir_all(external_skills.join("linked-ok")).unwrap();
    fs::write(
        external_skills.join("linked-ok/SKILL.md"),
        "---\nname: linked-ok\ndescription: linked directory skill\n---\nok\n",
    )
    .unwrap();
    fs::write(
        &secret,
        "---\nname: escape\ndescription: should not read\n---\nsecret\n",
    )
    .unwrap();

    // Valid symlinked skill folder
    symlink(
        external_skills.join("linked-ok"),
        skills_root.join("linked-ok"),
    )
    .unwrap();

    // Escaping SKILL.md symlink
    let escape_dir = skills_root.join("escape");
    fs::create_dir_all(&escape_dir).unwrap();
    symlink(&secret, escape_dir.join("SKILL.md")).unwrap();

    // Skill with a cyclic symlink inside scripts/ and an escaping symlink inside references/
    let cyclic_dir = skills_root.join("cyclic");
    write_skill(
        &skills_root,
        "cyclic",
        "---\nname: cyclic\ndescription: cycle guard\n---\nok\n",
    );
    fs::create_dir_all(cyclic_dir.join("scripts")).unwrap();
    fs::create_dir_all(cyclic_dir.join("references")).unwrap();
    fs::write(cyclic_dir.join("scripts/a.sh"), b"echo a").unwrap();
    symlink(cyclic_dir.join("scripts"), cyclic_dir.join("scripts/loop")).unwrap();
    symlink(&secret, cyclic_dir.join("references/leak.md")).unwrap();

    let discovery = SkillDiscovery::discover(&[skills_root], None, false);
    assert!(discovery.registry.get("linked-ok").is_some());
    assert!(discovery.registry.get("escape").is_none());
    let cyclic = discovery.registry.get("cyclic").unwrap();
    assert_eq!(cyclic.resources.scripts, vec!["scripts/a.sh"]);
    assert!(cyclic.resources.references.is_empty());

    assert!(
        discovery
            .diagnostics
            .iter()
            .any(|d| d.kind == SkillDiagnosticKind::SymlinkEscape)
    );
}

fn make_load_skill_tool(
    registry: SkillRegistry,
) -> (
    Arc<LoadSkillTool>,
    Arc<LoadedSkillsState>,
    Arc<RwLock<SkillRegistry>>,
) {
    let shared_reg = Arc::new(RwLock::new(registry));
    let loaded = Arc::new(LoadedSkillsState::new());
    let reporter = crate::session::ToolReporter::new(
        crate::session::WorkRef {
            conversation_id: "0190a000-0000-7000-8000-000000000099".into(),
            turn_id: causa::kernel::TurnId::new("t-skill"),
        },
        tokio::sync::broadcast::channel(16).0,
    );
    let tool = Arc::new(LoadSkillTool::new(
        shared_reg.clone(),
        loaded.clone(),
        reporter,
        10,
    ));
    (tool, loaded, shared_reg)
}

use std::sync::{Arc, RwLock};

#[test]
fn test_si06_static_schema_has_no_enum_and_readonly_hints() {
    let def = load_skill_tool_definition();
    assert_eq!(def.name, LOAD_SKILL_TOOL_NAME);
    let serialized = serde_json::to_string(&def.parameters).unwrap();
    assert!(
        !serialized.contains("\"enum\""),
        "load_skill schema must be purely static without enum: {serialized}"
    );
    assert_eq!(def.parameters["required"], serde_json::json!(["name"]));
    assert_eq!(def.parameters["additionalProperties"], false);
    assert_eq!(def.parameters["properties"]["name"]["type"], "string");
}

#[tokio::test]
async fn test_si06_load_skill_reads_external_dir_dedups_reloads_and_lists_available() {
    use causa::kernel::{
        BlockId, CallControl, CancellationToken, Tool, ToolCallContext, ToolCallPayload,
        ToolResultStatus,
    };

    let temp = tempfile::tempdir().unwrap();
    let external_dir = temp.path().join("external-skills");
    fs::create_dir_all(&external_dir).unwrap();

    let alpha_file = write_skill(
        &external_dir,
        "alpha",
        "---\nname: alpha\ndescription: Alpha helper\n---\n# Alpha Guide\nUse step 1.\n",
    );
    write_skill(
        &external_dir,
        "zeta",
        "---\nname: zeta\ndescription: Zeta helper\n---\n# Zeta Guide\n",
    );
    // Bundle resources with non-UTF-8 content to prove contents are never pre-read
    fs::create_dir_all(external_dir.join("alpha/scripts")).unwrap();
    fs::create_dir_all(external_dir.join("alpha/references")).unwrap();
    fs::create_dir_all(external_dir.join("alpha/assets")).unwrap();
    fs::write(
        external_dir.join("alpha/scripts/run.sh"),
        [0xff, 0xfe, 0x00],
    )
    .unwrap();
    fs::write(
        external_dir.join("alpha/references/doc.md"),
        "SECRET_REF_BODY",
    )
    .unwrap();
    fs::write(external_dir.join("alpha/assets/icon.bin"), [1u8, 2, 3]).unwrap();

    let discovery = SkillDiscovery::discover(std::slice::from_ref(&external_dir), None, false);
    let (tool, _loaded, _reg) = make_load_skill_tool(discovery.registry);
    assert_eq!(tool.hints().read_only, None);
    assert_eq!(tool.hints().destructive, None);
    assert_eq!(tool.hints().idempotent, None);
    assert_eq!(tool.hints().open_world, None);

    let control = CallControl::new(CancellationToken::new(), None);
    let make_call = |name: &str| {
        ToolCallContext::new(
            BlockId::new(uuid::Uuid::now_v7()),
            ToolCallPayload::new("load_skill", serde_json::json!({ "name": name })),
        )
    };

    // 1. Load alpha: frontmatter stripped, <skill_content> wrapped, resources listed by path only
    let call1 = make_call("alpha");
    let res1 = tool.execute(&call1, &control, None).await;
    assert_eq!(res1.status, ToolResultStatus::Succeeded);
    let text1 = res1.output.content.as_str().unwrap();
    assert!(text1.starts_with("<skill_content name=\"alpha\">"));
    assert!(text1.ends_with("</skill_content>"));
    assert!(!text1.contains("description: Alpha helper"));
    assert!(text1.contains("# Alpha Guide"));
    assert!(text1.contains("scripts/run.sh"));
    assert!(text1.contains("references/doc.md"));
    assert!(text1.contains("assets/icon.bin"));
    assert!(!text1.contains("SECRET_REF_BODY"));
    let extra1 = res1.output.meta.as_ref().unwrap().extra.as_ref().unwrap();
    assert_eq!(extra1["source"], SKILL_SOURCE);
    assert_eq!(extra1["skill"], "alpha");

    // 2. Duplicate call when unchanged: concise dedup hint citing block_id, no full body
    let res2 = tool.execute(&make_call("alpha"), &control, None).await;
    assert_eq!(res2.status, ToolResultStatus::Succeeded);
    let text2 = res2.output.content.as_str().unwrap();
    assert!(text2.contains("already loaded"));
    assert!(text2.contains(&call1.call_block_id.0.to_string()));
    assert!(!text2.contains("# Alpha Guide"));
    assert!(
        res2.output.meta.is_none(),
        "dedup response must not tag tars:skill"
    );

    // 3. Modify SKILL.md on disk -> reloads updated body with new sha256
    fs::write(
        &alpha_file,
        "---\nname: alpha\ndescription: Alpha helper\n---\n# Alpha v2\nUpdated instructions.\n",
    )
    .unwrap();
    let res3 = tool.execute(&make_call("alpha"), &control, None).await;
    assert_eq!(res3.status, ToolResultStatus::Succeeded);
    let text3 = res3.output.content.as_str().unwrap();
    assert!(text3.contains("# Alpha v2"));
    assert!(text3.contains("Updated instructions."));
    let extra3 = res3.output.meta.as_ref().unwrap().extra.as_ref().unwrap();
    assert_ne!(extra3["sha256"], extra1["sha256"]);

    // 4. Unknown skill name returns error listing available skills in lexicographical order
    let res_unknown = tool.execute(&make_call("missing"), &control, None).await;
    assert_eq!(res_unknown.status, ToolResultStatus::Failed);
    let err = res_unknown.output.content["error"].as_str().unwrap();
    assert!(err.contains("alpha, zeta"), "got: {err}");

    // 5. File exceeding 64 KiB cap is rejected at load_skill time
    let oversized = format!(
        "---\nname: alpha\ndescription: Alpha helper\n---\n{}\n",
        "x".repeat(tool::MAX_LOAD_SKILL_BYTES + 128)
    );
    fs::write(&alpha_file, oversized).unwrap();
    let res_huge = tool.execute(&make_call("alpha"), &control, None).await;
    assert_eq!(res_huge.status, ToolResultStatus::Failed);
    assert!(
        res_huge.output.content["error"]
            .as_str()
            .unwrap()
            .contains("exceeds")
    );
}

#[tokio::test]
async fn test_si08_load_skill_blocks_path_traversal_and_runtime_symlink_escape() {
    use causa::kernel::{
        BlockId, CallControl, CancellationToken, Tool, ToolCallContext, ToolCallPayload,
        ToolResultStatus,
    };

    let temp = tempfile::tempdir().unwrap();
    let skills_root = temp.path().join("skills");
    fs::create_dir_all(&skills_root).unwrap();
    let skill_file = write_skill(
        &skills_root,
        "safe",
        "---\nname: safe\ndescription: Safe skill\n---\nSafe body\n",
    );
    let outside_secret = temp.path().join("secret.txt");
    fs::write(&outside_secret, "TOP_SECRET_VALUE").unwrap();

    let discovery = SkillDiscovery::discover(std::slice::from_ref(&skills_root), None, false);
    let (tool, _loaded, _reg) = make_load_skill_tool(discovery.registry);
    let control = CallControl::new(CancellationToken::new(), None);
    let make_call = |name: &str| {
        ToolCallContext::new(
            BlockId::new(uuid::Uuid::now_v7()),
            ToolCallPayload::new("load_skill", serde_json::json!({ "name": name })),
        )
    };

    // Traversal names fail allowlist lookup without touching filesystem
    for bad in [
        "../secret.txt",
        "../../etc/passwd",
        "safe/../secret.txt",
        "",
    ] {
        let res = tool.execute(&make_call(bad), &control, None).await;
        assert_eq!(res.status, ToolResultStatus::Failed);
        assert!(
            res.output.content["error"]
                .as_str()
                .unwrap()
                .contains("safe")
        );
    }

    // Runtime symlink swap after discovery is blocked
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        fs::remove_file(&skill_file).unwrap();
        symlink(&outside_secret, &skill_file).unwrap();
        let res = tool.execute(&make_call("safe"), &control, None).await;
        assert_eq!(res.status, ToolResultStatus::Failed);
        let msg = res.output.content["error"].as_str().unwrap();
        assert!(msg.contains("outside"), "got: {msg}");
        assert!(!res.output.content.to_string().contains("TOP_SECRET_VALUE"));
    }
}
