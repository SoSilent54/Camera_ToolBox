//! RAW 解码参数草稿与安全重新解码面板。

use camera_toolbox_app::RawDecodeParams;
use camera_toolbox_core::{BayerPattern, RawEncoding, RawSpec};
use eframe::egui;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RawStorageUi {
    UnpackedU16,
}

impl RawStorageUi {
    const fn label(self) -> &'static str {
        match self {
            Self::UnpackedU16 => "Unpacked u16",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RawEndianUi {
    Little,
}

impl RawEndianUi {
    const fn label(self) -> &'static str {
        match self {
            Self::Little => "Little-endian",
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RawInspectorResponse {
    pub(crate) apply_requested: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct RawInspectorState {
    width: String,
    height: String,
    bit_depth: String,
    storage: RawStorageUi,
    endian: RawEndianUi,
    pending_generation: Option<u64>,
    decode_error: Option<String>,
}

impl RawInspectorState {
    pub(crate) fn new(spec: &RawSpec) -> Self {
        Self {
            width: spec.width.to_string(),
            height: spec.height.to_string(),
            bit_depth: spec.bit_depth.to_string(),
            storage: RawStorageUi::UnpackedU16,
            endian: RawEndianUi::Little,
            pending_generation: None,
            decode_error: None,
        }
    }

    pub(crate) fn sync_from_spec(&mut self, spec: &RawSpec) {
        self.width = spec.width.to_string();
        self.height = spec.height.to_string();
        self.bit_depth = spec.bit_depth.to_string();
        self.storage = RawStorageUi::UnpackedU16;
        self.endian = RawEndianUi::Little;
        self.pending_generation = None;
        self.decode_error = None;
    }

    pub(crate) fn decode_params(&self, bayer: BayerPattern) -> Result<RawDecodeParams, String> {
        let width = parse_u32_field("width", &self.width)?;
        let height = parse_u32_field("height", &self.height)?;
        let bit_depth = parse_u8_field("bit depth", &self.bit_depth)?;
        Ok(RawDecodeParams {
            spec: RawSpec {
                width,
                height,
                bit_depth,
                bayer,
            },
            encoding: RawEncoding::U16Le,
        })
    }

    pub(crate) fn mark_submitted(&mut self, generation: u64) {
        self.pending_generation = Some(generation);
        self.decode_error = None;
    }

    pub(crate) fn mark_error(&mut self, generation: Option<u64>, message: String) {
        if generation.is_none() || self.pending_generation == generation {
            self.pending_generation = None;
            self.decode_error = Some(message);
        }
    }

    pub(crate) fn pending_generation(&self) -> Option<u64> {
        self.pending_generation
    }
}

pub(crate) fn render_raw_inspector(
    ui: &mut egui::Ui,
    state: &mut RawInspectorState,
    can_apply: bool,
) -> RawInspectorResponse {
    ui.separator();
    ui.heading("RAW Decode");
    egui::Grid::new("raw_inspector_grid")
        .num_columns(2)
        .spacing(egui::vec2(12.0, 8.0))
        .show(ui, |ui| {
            ui.label("Width");
            ui.add(egui::TextEdit::singleline(&mut state.width).id_source("raw_inspector_width"));
            ui.end_row();

            ui.label("Height");
            ui.add(egui::TextEdit::singleline(&mut state.height).id_source("raw_inspector_height"));
            ui.end_row();

            ui.label("Bit depth");
            ui.add(
                egui::TextEdit::singleline(&mut state.bit_depth)
                    .id_source("raw_inspector_bit_depth"),
            );
            ui.end_row();

            ui.label("Storage");
            ui.label(state.storage.label());
            ui.end_row();

            ui.label("Endian");
            ui.label(state.endian.label());
            ui.end_row();
        });

    if let Some(error) = &state.decode_error {
        ui.colored_label(egui::Color32::RED, error);
    }
    if state.pending_generation.is_some() {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Re-decoding RAW…");
        });
    }
    let apply_requested = ui
        .add_enabled(can_apply, egui::Button::new("Apply Decode"))
        .clicked();
    if !can_apply {
        ui.small("当前文档没有可复用的不可变 RAW source。 ");
    }
    RawInspectorResponse { apply_requested }
}

fn parse_u32_field(name: &str, value: &str) -> Result<u32, String> {
    let parsed = value
        .trim()
        .parse::<u32>()
        .map_err(|error| format!("invalid {name}: {error}"))?;
    if parsed == 0 {
        return Err(format!("{name} must be greater than zero"));
    }
    Ok(parsed)
}

fn parse_u8_field(name: &str, value: &str) -> Result<u8, String> {
    let parsed = value
        .trim()
        .parse::<u8>()
        .map_err(|error| format!("invalid {name}: {error}"))?;
    if parsed == 0 {
        return Err(format!("{name} must be greater than zero"));
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use camera_toolbox_core::BayerPattern;

    fn spec() -> RawSpec {
        RawSpec {
            width: 640,
            height: 480,
            bit_depth: 10,
            bayer: BayerPattern::Rggb,
        }
    }

    #[test]
    fn builds_decode_params_from_draft() {
        let state = RawInspectorState::new(&spec());
        let params = state.decode_params(BayerPattern::Bggr).unwrap();
        assert_eq!(params.spec.width, 640);
        assert_eq!(params.spec.height, 480);
        assert_eq!(params.spec.bit_depth, 10);
        assert_eq!(params.spec.bayer, BayerPattern::Bggr);
        assert_eq!(params.encoding, RawEncoding::U16Le);
    }

    #[test]
    fn submission_error_clears_only_matching_pending_generation() {
        let mut state = RawInspectorState::new(&spec());
        state.mark_submitted(7);
        state.mark_error(Some(6), "stale".to_owned());
        assert_eq!(state.pending_generation(), Some(7));
        state.mark_error(Some(7), "latest".to_owned());
        assert_eq!(state.pending_generation(), None);
    }
}
