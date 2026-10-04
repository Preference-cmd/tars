# AGENTS.md — Tarz

Tarz is an agent application built on Causa. It ships as an application, not a
crates.io library or a public Rust SDK. All Cargo packages must remain
`publish = false`.

`CLAUDE.md` is a symlink to this file; edit this file only. Read
`AGENTS.local.md` when present for machine-local workflow rules.

## Read by task

| Task | Start here |
| --- | --- |
| Build, run, or inspect available commands | [README.md](README.md) |
| Change code or run checks | [Current implementation](docs/README.md) |
| Trace session ownership, assembly, or execution | [Session lifecycle](docs/session-lifecycle.md) |
| Change workspace tool access, limits or status | [Workspace tools](docs/workspace-tools.md), `tarz-core::workspace`, `tarz-core::tools`, `session::ToolReporter` |
| Change shared TOML configuration | [Configuration](docs/configuration.md), `tarz-core::config` |
| Run or change the terminal prototype | [TUI prototype](docs/tui.md), `crates/tarz-tui/` |
| Plan a change or inspect acceptance status | [Local design index](.agents/README.md) |
| Follow Direct/Noted or proposal lifecycle rules | [Workflow](.agents/pipelines/WORKFLOW.md) |

Keep implementation facts in `docs/`; keep proposals, acceptance progress and
historical evidence in the local `.agents/` tree. Read the relevant proposal
before implementation. Proposed structures are not current capabilities.

## Verify facts at the source

| Fact | Authority |
| --- | --- |
| Workspace members, dependencies, package metadata and features | Root and member `Cargo.toml`; resolved versions in `Cargo.lock` |
| Toolchain and components | `rust-toolchain.toml` |
| Current behavior and ownership | Relevant `crates/*/src/` code and its consumers; `docs/` is the reading guide |
| CI requirements | [.github/workflows/ci.yml](.github/workflows/ci.yml) |

Do not infer implemented behavior from a proposal or an old review. Keep the
README, current docs and affected guidance aligned when layout, commands or
features change; keep one authoritative acceptance list per proposal scope.

## Architecture and conventions

- Keep Cargo packages under `crates/`, inheriting workspace metadata.
  `tarz-core` owns shared behavior and configuration; `tarz-cli` owns the `tarz`
  binary; `tarz-tui` owns the experimental terminal consumer and `tarz-tui` binary.
  Add other entry crates only with real consumers, never placeholder UI packages.
- Use Causa's public facade. The current streaming integration has explicit user
  authorization to use the sibling path `../agent-stack-oss/crates/causa`; see
  [Current implementation](docs/README.md) for the tested source revision. Do not
  include or copy Causa sources. Returning to a published registry dependency is
  a separate delivery step. Tarz owns sessions, assembly, application policy and
  persistence; Causa supplies material contracts and individual runs.
- Keep shared application behavior independent of CLI/TUI/GUI presentation.
  Entry points adapt input, output and UI events to shared operations.
- Shared TOML supplies new-session defaults. Explicit input overrides those
  defaults; reopening always uses the saved SessionDefinition. Keep secrets as
  environment references and do not automatically rewrite global configuration.
- Use `foo.rs` plus `foo/` for library modules, never `mod.rs`. Keep index files
  limited to documentation, declarations and explicit re-exports.
- Deny unsafe code and document public library APIs. Explain new dependencies
  or an MSRV increase in the commit. Keep `Cargo.lock` committed.
- Use Conventional Commits, one logical change per commit. Local tracking and
  branch defaults belong in `AGENTS.local.md`, not in duplicated process rules.

## Validation and delivery

Use the relevant checks in [docs/README.md](docs/README.md), including formatting,
warning-free lint/docs, tests and release builds when applicable. Test observable
behavior and failure cases; build success alone does not satisfy behavior gates.
Documentation-only changes need link/proposal checks, not unrelated Rust tests.

Tests use local fixtures; do not require live model credentials. Installation,
platform support and release delivery need their own evidence. Do not treat a
local build as a published release or import Causa's library publishing workflow.
