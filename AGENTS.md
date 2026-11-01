# AGENTS.md — TARS

TARS is the validation project for Causa: a real agent application that
exercises Causa's public contracts and feeds what it finds back into Causa.
Validation comes first; the application is how it is done. TARS ships as an
application, not a crates.io library or a public Rust SDK. All Cargo packages
must remain `publish = false`.

`CLAUDE.md` is a symlink to this file; edit this file only. Read
`AGENTS.local.md` when present for machine-local workflow rules.

## Purpose

- **Validate.** Use Causa the way an outside host does, through the public
  facade and the published crates.io release, under real conditions:
  persistent sessions, continuation after a cancel, workspace tools with
  write confirmation, and live runs on the three wire protocols.
- **Improve.** For each problem found, decide whether it belongs to TARS,
  Causa or their integration. A Causa change is made in the Causa repository
  under its rules and released there; TARS then adopts it by upgrading the
  dependency and reruns the checks that found the problem.
- **Order work by what it validates.** TARS has to be usable, because only
  real use produces evidence. Prefer work that exercises Causa; work that only
  serves distribution (install channels, GUI, more platforms) comes later.
- **Keep the layers apart.** Do not move TARS policy into Causa. Ask Causa for
  the general fact or channel a host needs and keep the vocabulary and the
  decision in TARS: TARS's permission modes stay here, while Causa carries
  only tool hints and per-run host data. A passing TARS test is evidence for
  that integration, not proof of a Causa guarantee.

## Read by task

| Task | Start here |
| --- | --- |
| Build, run, or inspect available commands | [README.md](README.md) |
| Change code or run checks | [Current implementation](docs/README.md) |
| Trace session ownership, assembly, or execution | [Session lifecycle](docs/session-lifecycle.md) |
| Change interrupted-session continuation or material repair | [Session continuation](docs/session-continuation.md), `tars-core::application::repair` |
| Change workspace tool access, limits or status | [Workspace tools](docs/workspace-tools.md), `tars-core::workspace`, `tars-core::tools`, `session::ToolReporter` |
| Change shared TOML configuration | [Configuration](docs/configuration.md), `tars-core::config` |
| Run or change the terminal prototype | [TUI prototype](docs/tui.md), `crates/tui/` |
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

- Keep Cargo packages under `crates/`, with directory names omitting the
  `tars-` prefix (`crates/core`, `crates/sandbox`, `crates/cli`, `crates/tui`),
  and inherit workspace metadata.
  `tars-core` owns shared behavior and configuration; `tars-sandbox` owns the
  platform sandbox policy, backends and process control and depends on neither
  Causa nor `tars-core`; `tars-cli` owns the `tars` binary; `tars-tui` owns the
  experimental terminal consumer and `tars-tui` binary. `tars-sandbox` is a
  library package, not an entry crate. Add other entry crates only with real
  consumers, never placeholder UI packages.
- Use Causa's public facade from the published crates.io release pinned in
  `Cargo.toml` and `Cargo.lock`. Do not include or copy Causa sources, and do
  not switch to a path or git dependency without the user's say-so. TARS owns
  sessions, assembly, application policy and persistence; Causa supplies
  material contracts and individual runs.
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
