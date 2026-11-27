# AGENTS.md — TARS

TARS is an open-source, local-first AI agent written in
Rust (single `tars` binary with interactive terminal and CLI subcommands), built on the published
`causa` crate. TARS ships as an end-user application, not a crates.io library
or a public Rust SDK. All Cargo packages must remain `publish = false`.

`CLAUDE.md` is a symlink to this file; edit this file only. `AGENTS.local.md` is
machine-local, gitignored and absent from public clones; read it when present
for machine-local workflow rules.

## Repository layout and local-only files

The tracked tree is the public project: `README.md`, `docs/`, `crates/`, the
Cargo files, `.github/workflows/` and `LICENSE`. A local checkout can also carry
material that is gitignored and absent from the public repository:

- `.agents/` — local design index, proposals, reviews and source snapshots.
- `scripts/` — local verification helpers.
- `AGENTS.local.md` — machine-local workflow rules.

Never force-add these paths, commit credentials or machine-local paths, or push
local-only content: the repository and its history are public. Links to those
paths only resolve in a local checkout; a public clone relies on `docs/` and the
code.

## Product principles and layer boundaries

- **Product quality and safety first.** Prioritize real user workflows:
  reliable persistent sessions, safe workspace operations, predictable
  permission modes, clean error recovery, and responsive CLI/TUI ergonomics.
- **Keep layers separate from Causa.** Consume `causa` only through its
  published crates.io facade (`Cargo.toml` / `Cargo.lock`). Never copy or vendor
  Causa sources, and do not switch to a path or git dependency without explicit
  user approval. TARS owns sessions, persistence, workspace tools, sandboxing,
  permission policy, compaction policy, and UI; Causa supplies material
  contracts and single-run model/protocol execution.
- **Contribute general runtime improvements upstream cleanly.** When product
  work uncovers a defect or missing primitive in Causa rather than TARS,
  separate the owning layers first: keep TARS-specific policy here and fix
  general runtime/protocol issues in the Causa repository under its rules, then
  upgrade the crates.io dependency and verify the integration in TARS.
- **Evidence over assumptions.** Treat a passing TARS integration test as
  evidence for that integration, not proof of every Causa guarantee. Do not
  claim platform support, distribution channels, or runtime guarantees without
  observable verification.

## Read by task

| Task | Start here |
| --- | --- |
| Build, run, or inspect available commands | [README.md](README.md) |
| Change code or run checks | [Current implementation](docs/README.md) |
| Trace session ownership, assembly, or execution | [Session lifecycle](docs/session-lifecycle.md) |
| Change interrupted-session continuation or material repair | [Session continuation](docs/session-continuation.md), `tars-core::application::repair` |
| Change workspace tool access, limits or status | [Workspace tools](docs/workspace-tools.md), `tars-core::workspace`, `tars-core::tools`, `session::ToolReporter` |
| Change shared TOML configuration | [Configuration](docs/configuration.md), `tars-core::config` |
| Run or change the terminal interface | [TUI guide](docs/tui.md), `crates/tui/` |
| Plan a change or inspect acceptance status (local checkout) | `.agents/README.md` |
| Follow Direct/Noted or proposal lifecycle rules (local checkout) | `.agents/pipelines/WORKFLOW.md` |

Keep implementation facts in `docs/`. Proposals, acceptance progress and
historical evidence stay in the local `.agents/` tree when present: read the
relevant proposal before implementation there. In a public clone without that
tree, `docs/` and the code are the only authority. Proposed structures are not
current capabilities.

## Verify facts at the source

| Fact | Authority |
| --- | --- |
| Workspace members, dependencies, package metadata and features | Root and member `Cargo.toml`; resolved versions in `Cargo.lock` |
| Toolchain and components | `rust-toolchain.toml` |
| Current behavior and ownership | Relevant `crates/*/src/` code and its consumers; `docs/` is the reading guide |
| CI requirements | [.github/workflows/ci.yml](.github/workflows/ci.yml) |

Do not infer implemented behavior from a proposal or an old review. Keep the
README, current docs and affected guidance aligned when layout, commands or
features change; when maintaining the local proposal tree, keep one
authoritative acceptance list per proposal scope.

## Architecture and conventions

- Keep Cargo packages under `crates/`, with directory names omitting the
  `tars-` prefix (`crates/core`, `crates/sandbox`, `crates/cli`, `crates/tui`),
  and inherit workspace metadata.
  `tars-core` owns shared behavior and configuration; `tars-sandbox` owns the
  platform sandbox policy, backends and process control and depends on neither
  Causa nor `tars-core`; `tars-cli` owns the `tars` binary (launching the
  interactive TUI or running CLI subcommands); `tars-tui` owns the native
  terminal interface library consumed by `tars-cli`. `tars-sandbox` and
  `tars-tui` are library packages, not entry binaries. Add other entry crates
  only with real consumers, never placeholder UI packages.
- Use Causa's public facade from the published crates.io release pinned in
  `Cargo.toml` and `Cargo.lock`. Do not include or copy Causa sources, and do
  not switch to a path or git dependency without the user's say-so. TARS owns
  sessions, assembly, application policy and persistence; Causa supplies
  material contracts and individual runs.
- Keep shared application behavior independent of CLI/TUI/GUI presentation.
  Entry points adapt input, output and UI events to shared operations.
- Shared TOML supplies new-session defaults. Explicit input overrides those
  defaults; reopening always uses the saved SessionDefinition. Keep secrets out
  of `config.toml` and SQLite by storing named references resolved from TARS's
  private `0600` `credentials.toml` store (with optional explicit `api_key_env` /
  `--api-key-env` environment-variable references for headless/CI), and never
  automatically rewrite global configuration without an explicit user action
  (such as `/provider`).
- Use `foo.rs` plus `foo/` for library modules, never `mod.rs`. Keep index files
  limited to documentation, declarations and explicit re-exports.
- Deny unsafe code and document public library APIs. Explain new dependencies
  or an MSRV increase in the commit. Keep `Cargo.lock` committed.
- Use Conventional Commits, one logical change per commit. Machine-local
  tracking and branch defaults belong in the gitignored `AGENTS.local.md` when
  present, not in duplicated process rules.

## Verification and delivery

Use the relevant checks in [docs/README.md](docs/README.md), including formatting,
warning-free lint/docs, tests and release builds when applicable. Test observable
behavior and failure cases; build success alone does not satisfy behavior gates.
GitHub Actions runs [ci.yml](.github/workflows/ci.yml) on every push and pull
request; keep that workflow green in addition to local checks. Documentation-only
changes need link checks, and proposal checks when the local `.agents/` tree is
present, not unrelated Rust tests.

Tests use local fixtures; do not require live model credentials. Gate tests that
need Unix workspace access or Seatbelt with `cfg` so every platform runs the
common suite. Installation, platform support and release delivery need their own
evidence. Do not treat a local build as a published release or import Causa's
library publishing workflow.
