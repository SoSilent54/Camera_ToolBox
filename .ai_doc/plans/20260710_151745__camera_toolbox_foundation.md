# Camera Toolbox Foundation Plan

## Meta

- Date: 2026-07-10
- Direction: Rust-only workspace with `egui` GUI, `ratatui` TUI, and CLI frontends.
- Scope: architecture documentation and foundational project configuration.

## Intent

Create a minimal Rust workspace that preserves clean dependency direction before feature implementation starts:

```text
core <- app <- adapters <- frontends/bin assembly
```

`app` owns workflow commands/events and side-effect port traits. `adapters` implements those traits. Frontend binaries assemble concrete adapters and call `app`.

## Phase 1 - Context Audit

- Current directory was empty before initialization.
- Rust toolchain observed locally: `rustc 1.94.1`, `cargo 1.94.1`.
- Project direction locked by user: Rust only; frontends are `egui`/`tui`/`cli`.
- Main design risk: avoiding cyclic crate dependencies between workflow and adapters.

## Phase 2 - Solution Convergence and Build Blueprint

### File-level plan

- `README.md`: route, workspace layout, verification commands.
- `docs/architecture.md`: dependency direction, P0 call flow, object ownership, safety boundary.
- `docs/roadmap.md`: P0-P3 route.
- `Cargo.toml`: workspace and unified dependency versions.
- `crates/core`: pure domain types and ROI statistics.
- `crates/app`: commands, events, workflow, port traits.
- `crates/adapters`: in-memory/no-op starter adapters.
- `crates/frontends/cli`: CLI binary assembly.
- `crates/frontends/tui`: TUI binary placeholder.
- `crates/frontends/gui`: egui binary placeholder.

### Object-level plan

- `RawSpec`, `RawFrame`, `Roi`, `RoiStats`, `SensorProfile`, register/exposure value types live in `core`.
- `CommandEnvelope`, `WorkflowEvent`, `Workflow`, `CaptureBackend`, `ArtifactStore`, small sensor capability traits live in `app`.
- `ArtifactError` belongs to the artifact port; `AppError` converts it at workflow boundary.
- `SyntheticCaptureAdapter`, `MemoryArtifactStore` live in `adapters`.
- Frontends only construct commands, assemble adapters, and render results.

### Function-level plan

- `Roi::clamped_to`: clamp ROI to image bounds and reject empty regions.
- `analyze_roi`: compute min/max/mean/saturation over RAW buffer.
- `Workflow::run_capture_and_analyze`: execute one P0-style capture/analyze pass through injected `CaptureBackend` and `ArtifactStore`.
- CLI `main`: runs a synthetic smoke path; TUI/GUI `main` are buildable placeholders.

### Call flow

```text
frontend main ──► CommandEnvelope
   │
   ▼
Workflow::run_capture_and_analyze
   │    ├ CaptureBackend::capture
   │    ├ ArtifactStore::load_raw
   │    └ camera_toolbox_core::analyze_roi
   ▼
WorkflowEvent / AnalysisReport
```

### Parameters/defaults

No device/runtime defaults are introduced in this foundation step. Placeholder frontends and synthetic adapters avoid encoding false hardware assumptions.

### Verification plan

- `cargo fmt --all -- --check`
- `cargo check --workspace`
- `cargo test --workspace`

## Phase 3 - Implementation and Validation

- Created Rust workspace and member crates.
- Implemented core RAW validation, ROI statistics, sensor profile/register write validation, small capability traits, P0 workflow, synthetic adapters, and three frontend binaries.
- Final validation executed after the latest `CommandEnvelope`/formatting edits:
  - `cargo fmt --all -- --check`: passed.
  - `cargo check --workspace`: passed.
  - `cargo test --workspace`: passed, 2 tests.
  - `cargo run -p camera-toolbox-cli -- smoke`: passed, printed synthetic ROI stats.

## Phase 4 - Acceptance, Local Commit, and Delivery

Ready for user review. No local commit requested.
