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
