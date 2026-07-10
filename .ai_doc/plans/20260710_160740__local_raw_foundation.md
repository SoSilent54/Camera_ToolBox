# Local RAW Foundation Plan

## Meta

- Scope: local RAW loading, ROI analysis, CLI file entry, GUI File -> Open Raw viewer.
- Excluded: sensor capture, SSH/SFTP, register read/write, exposure control, auto workflow.

## Intent

Implement the first usable non-sensor path: open a local unpacked RAW file with an explicit `RawSpec`, compute ROI statistics, and show a grayscale preview in GUI.

## Phase 1 - Context Audit

- Current `core` has `RawSpec`, `RawFrame`, ROI analysis, and validation but no byte/file decoder.
- Current CLI only has synthetic `smoke`.
- Current GUI is an egui placeholder.
- Existing sensor/capture/register abstractions stay untouched for this task.

## Phase 2 - Solution Convergence and Build Blueprint

### File-level plan

- `crates/core/src/raw.rs`: add `RawEncoding::U16Le` and byte decoder from unpacked 16-bit little-endian local RAW bytes only; packed RAW10/12 and debayer remain unsupported.
- `crates/adapters/src/local_raw.rs`: add filesystem loader for local RAW paths.
- `crates/adapters/src/lib.rs`: export local RAW loader.
- `crates/frontends/cli/src/main.rs`: add `analyze-raw` command with explicit width/height/bit-depth/stride/bayer/encoding/roi.
- `crates/frontends/gui/src/main.rs`: implement a persistent image viewer with File -> Open Raw native file selection, RAW settings dialog, zoom, pan, minimap, hover pixel, and status line.
- `Cargo.toml` / frontend manifests: add only dependencies needed for implemented behavior.
- `docs/architecture.md` / `docs/roadmap.md`: clarify local RAW first; sensor/register deferred.

### Object-level plan

- `RawEncoding`: only `U16Le`; explicitly unpacked storage only. RAW10/12 packed, MIPI packing, and debayer are rejected/deferred.
- `LocalRawLoader`: stateless adapter that reads file bytes then calls `RawFrame::from_bytes`.
- CLI command structs own parse-time parameters and convert them into `RawSpec`, `RawEncoding`, `Roi`.
- GUI app owns loaded `RawFrame`, texture, active ROI, `RawOpenDialogState`, viewer zoom/pan state, and status message.

### Function-level plan

- `RawFrame::from_bytes(spec, encoding, bytes)`: validate spec, decode bytes to `Vec<u16>`, range-check against bit depth, then call `RawFrame::new`.
- `RawEncoding::bytes_per_pixel()`: explicit byte count for size validation.
- `LocalRawLoader::load_raw_frame(path, spec, encoding)`: read bytes and decode through the app port.
- CLI `analyze_raw`: submit `LocalRawAnalyzeRequest` to `Workflow::load_raw_and_analyze`, then print deterministic text.
- GUI `File -> Open Raw...`: select a path with `rfd`, validate dialog fields, submit the same workflow request, then create the grayscale texture and reset viewer fit.

### Call flow

```text
CLI analyze-raw / GUI File -> Open Raw
   │
   ├── build LocalRawAnalyzeRequest (RawSpec + RawEncoding + Roi)
   ├── Workflow::load_raw_and_analyze
   │      └ RawFrameLoader::load_raw_frame (LocalRawLoader)
   │             ├ std::fs::read
   │             └ RawFrame::from_bytes + analyze_roi
   └── render text / egui viewer
```

### Parameters and defaults

| Parameter | Location/Scope | Type | Unit | Default | Valid Range | Meaning | Effect Path | Default Rationale | Impact of Increase/Decrease | Compatibility |
|---|---|---|---|---|---|---|---|---|---|---|
| width | CLI `--width` / GUI RAW dialog | `u32` | pixel | none | `>0` | active image width | `RawSpec::validate` | RAW has no header; require an explicit value | wrong value rejects or misinterprets rows | additive CLI flag and GUI field |
| height | CLI `--height` / GUI RAW dialog | `u32` | pixel | none | `>0` | image height | `RawSpec::validate` | RAW has no header; require an explicit value | wrong value rejects or truncates expected size | additive |
| bit depth | CLI `--bit-depth` / GUI RAW dialog | `u8` | bit | `10` in GUI | `1..=16` | valid sample range | decoder range check | UI default covers a common RAW format but remains explicit | wrong value changes saturation/range validation | additive |
| stride pixels | CLI `--stride-pixels` / GUI RAW dialog | `u32` | pixel | `width` | `>=width` | row stride in decoded pixels | size and ROI indexing | tightly packed local files are common first case | larger supports padded rows; smaller invalid | additive |
| Bayer | CLI `--bayer` / GUI RAW dialog | enum | pattern | `rggb` | `rggb/grbg/gbrg/bggr` | CFA metadata | hover/channel labeling later | common default; display is grayscale now | affects metadata, later debayer | additive |
| encoding / endian | CLI `--encoding` / GUI RAW dialog | enum | bytes | `u16le` / little | unpacked `u16le` only | unpacked storage encoding | byte decoder | keep first display path deterministic and avoid packed RAW decode ambiguity | other encodings and big-endian are rejected | additive |
| ROI | CLI `--roi` / GUI initial load | pixel rect | pixel | full image | non-empty after clamp | analysis region | `analyze_roi` | full image gives useful first result | smaller focuses stats | additive |

### Verification plan

- Unit tests for byte-size mismatch, bit-depth range violation, and U16LE decode.
- `cargo fmt --all -- --check`.
- `cargo check --workspace`.
- `cargo test --workspace`.
- Create a tiny local RAW fixture and run CLI `analyze-raw` against it.

## Phase 3 - Implementation and Validation

- Implemented `RawEncoding::U16Le` and `RawFrame::from_bytes` for unpacked local RAW bytes.
- Added `RawFrameLoader` app port and `Workflow::load_raw_and_analyze`; `LocalRawLoader` implements the port and returns `RawFrameLoadError` without a parallel frontend-facing error path.
- Added CLI `analyze-raw` command with explicit `--width`, `--height`, `--bit-depth`, optional `--stride-pixels`, `--bayer`, `--encoding u16le`, and `--roi`; CLI now calls `Workflow::load_raw_and_analyze`.
- Replaced GUI placeholder and launch arguments with a persistent workbench: `File -> Open Raw...` opens the native file picker and RAW settings dialog; loaded images support grayscale preview, ROI overlay, hover pixel, mouse-wheel zoom, drag pan, minimap viewport, and status line. GUI calls `Workflow::load_raw_and_analyze`; preview and hover index rows by `stride_pixels` and only display the active `width x height` region.
- Updated README, architecture, and roadmap to state local RAW first, route frontends through app workflow, and defer sensor/register/packed RAW/debayer work.

Validation executed:

- `cargo fmt --all -- --check`: passed on current code.
- `cargo check --workspace`: passed on current code.
- `cargo test --workspace`: passed on current code, 10 tests.
- `cargo build -p camera-toolbox-gui`: passed; GUI window launch not exercised because it requires a local X11/Wayland session.
- `cargo run -p camera-toolbox-cli -- analyze-raw --raw target/local_raw_fixture_2x2_u10.raw --width 2 --height 2 --bit-depth 10 --encoding u16le --roi 0,0,2,2`: passed, output `min=0 max=1023 mean=384.00 saturated=1/4`.

Residual scope limits:

- GUI compile and unit-test paths are verified; native file picker and actual window interaction remain dependent on a local X11/Wayland session.
- Only unpacked `u16le` RAW is supported. RAW10/12 packed, debayer, JSON manifests, and sensor IO are intentionally unsupported in this slice.

## Phase 4 - Acceptance, Local Commit, and Delivery

Local commit requested; the final delivery records the commit ID.
