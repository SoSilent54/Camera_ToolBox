//! Headerless YUV420SP 的内联 Decode 草稿与重解释节流。

use std::time::{Duration, Instant};

use camera_toolbox_core::{ChromaOrder, Yuv420SpSpec, YuvMatrix, YuvRange};
use eframe::egui;

const YUV_DECODE_DEBOUNCE: Duration = Duration::from_millis(250);

#[derive(Debug, Clone)]
pub(crate) struct YuvInspectorState {
    width: String,
    height: String,
    y_stride: String,
    chroma_stride: String,
    chroma_order: ChromaOrder,
    chroma_order_hint: Option<ChromaOrder>,
    matrix: YuvMatrix,
    range: YuvRange,
    revision: u64,
    attempted_revision: Option<u64>,
    submitted_revision: Option<u64>,
    submit_at: Option<Instant>,
    pending_generation: Option<u64>,
    decode_error: Option<String>,
}

impl YuvInspectorState {
    pub(crate) fn new(spec: &Yuv420SpSpec, chroma_order_hint: Option<ChromaOrder>) -> Self {
        Self {
            width: spec.width.to_string(),
            height: spec.height.to_string(),
            y_stride: spec.y_stride.to_string(),
            chroma_stride: spec.chroma_stride.to_string(),
            chroma_order: spec.chroma_order,
            chroma_order_hint,
            matrix: spec.matrix,
            range: spec.range,
            revision: 1,
            attempted_revision: Some(1),
            submitted_revision: Some(1),
            submit_at: None,
            pending_generation: None,
            decode_error: None,
        }
    }

    pub(crate) fn decode_spec(&self) -> Result<Yuv420SpSpec, String> {
        let spec = Yuv420SpSpec {
            width: parse_u32_field("width", &self.width)?,
            height: parse_u32_field("height", &self.height)?,
            y_stride: parse_usize_field("Y stride", &self.y_stride)?,
            chroma_stride: parse_usize_field("chroma stride", &self.chroma_stride)?,
            chroma_order: self.chroma_order,
            matrix: self.matrix,
            range: self.range,
        };
        if let Some(expected) = self.chroma_order_hint
            && spec.chroma_order != expected
        {
            return Err(format!(
                "this file suffix requires {:?} chroma order",
                expected
            ));
        }
        spec.validate().map_err(|error| error.to_string())?;
        Ok(spec)
    }

    fn touch(&mut self, now: Instant) {
        self.revision = self.revision.saturating_add(1);
        self.submit_at = Some(now + YUV_DECODE_DEBOUNCE);
        self.pending_generation = None;
        self.decode_error = None;
    }

    pub(crate) fn submission_due(&self, now: Instant) -> bool {
        self.submit_at.is_some_and(|deadline| now >= deadline)
            && self.attempted_revision != Some(self.revision)
            && self.submitted_revision != Some(self.revision)
    }

    pub(crate) fn mark_submitted(&mut self, generation: u64) {
        self.attempted_revision = Some(self.revision);
        self.submitted_revision = Some(self.revision);
        self.submit_at = None;
        self.pending_generation = Some(generation);
        self.decode_error = None;
    }

    pub(crate) fn mark_validation_error(&mut self, message: String) {
        self.attempted_revision = Some(self.revision);
        self.submit_at = None;
        self.pending_generation = None;
        self.decode_error = Some(message);
    }

    pub(crate) fn mark_error(&mut self, generation: u64, message: String) {
        if self.pending_generation == Some(generation) {
            self.pending_generation = None;
            self.decode_error = Some(message);
        }
    }

    pub(crate) fn mark_applied(&mut self, generation: u64) {
        if self.pending_generation == Some(generation) {
            self.pending_generation = None;
            self.decode_error = None;
        }
    }
}

pub(crate) struct YuvInspectorResponse {
    pub(crate) params_changed: bool,
}

pub(crate) fn render_yuv_inspector(
    ui: &mut egui::Ui,
    state: &mut YuvInspectorState,
    can_apply: bool,
) -> YuvInspectorResponse {
    ui.separator();
    ui.heading("YUV Decode");
    let mut changed = false;
    egui::Grid::new("yuv_inspector_grid")
        .num_columns(2)
        .spacing(egui::vec2(12.0, 8.0))
        .show(ui, |ui| {
            changed |= text_row(ui, "Width", &mut state.width, "yuv_inspector_width");
            changed |= text_row(ui, "Height", &mut state.height, "yuv_inspector_height");
            changed |= text_row(
                ui,
                "Y stride (bytes)",
                &mut state.y_stride,
                "yuv_inspector_y_stride",
            );
            changed |= text_row(
                ui,
                "Chroma stride (bytes)",
                &mut state.chroma_stride,
                "yuv_inspector_chroma_stride",
            );
            ui.label("Chroma order");
            egui::ComboBox::from_id_salt("yuv_inspector_chroma_order")
                .selected_text(match state.chroma_order {
                    ChromaOrder::Uv => "UV / NV12",
                    ChromaOrder::Vu => "VU / NV21",
                })
                .show_ui(ui, |ui| {
                    changed |= ui
                        .selectable_value(&mut state.chroma_order, ChromaOrder::Uv, "UV / NV12")
                        .changed();
                    changed |= ui
                        .selectable_value(&mut state.chroma_order, ChromaOrder::Vu, "VU / NV21")
                        .changed();
                });
            ui.end_row();
            ui.label("Matrix");
            egui::ComboBox::from_id_salt("yuv_inspector_matrix")
                .selected_text(match state.matrix {
                    YuvMatrix::Bt601 => "BT.601",
                    YuvMatrix::Bt709 => "BT.709",
                })
                .show_ui(ui, |ui| {
                    changed |= ui
                        .selectable_value(&mut state.matrix, YuvMatrix::Bt601, "BT.601")
                        .changed();
                    changed |= ui
                        .selectable_value(&mut state.matrix, YuvMatrix::Bt709, "BT.709")
                        .changed();
                });
            ui.end_row();
            ui.label("Range");
            egui::ComboBox::from_id_salt("yuv_inspector_range")
                .selected_text(match state.range {
                    YuvRange::Limited => "Limited",
                    YuvRange::Full => "Full",
                })
                .show_ui(ui, |ui| {
                    changed |= ui
                        .selectable_value(&mut state.range, YuvRange::Limited, "Limited")
                        .changed();
                    changed |= ui
                        .selectable_value(&mut state.range, YuvRange::Full, "Full")
                        .changed();
                });
            ui.end_row();
        });
    if let Some(expected) = state.chroma_order_hint {
        ui.small(format!(
            "File suffix requires {:?}; incompatible drafts stay unapplied.",
            expected
        ));
    }
    if changed {
        state.touch(Instant::now());
        ui.ctx().request_repaint_after(YUV_DECODE_DEBOUNCE);
    }
    if let Some(error) = &state.decode_error {
        ui.colored_label(egui::Color32::RED, error);
    }
    if state.pending_generation.is_some() {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Re-decoding YUV…");
        });
    } else if can_apply {
        ui.small("Valid changes are applied automatically.");
    } else {
        ui.small("当前文档没有可复用的不可变 YUV source。");
    }
    YuvInspectorResponse {
        params_changed: changed,
    }
}

fn text_row(ui: &mut egui::Ui, label: &str, value: &mut String, id: &str) -> bool {
    ui.label(label);
    let changed = ui
        .add(egui::TextEdit::singleline(value).id_source(id))
        .changed();
    ui.end_row();
    changed
}

fn parse_u32_field(name: &str, value: &str) -> Result<u32, String> {
    value
        .trim()
        .parse::<u32>()
        .map_err(|error| format!("invalid {name}: {error}"))
        .and_then(|parsed| {
            (parsed > 0)
                .then_some(parsed)
                .ok_or_else(|| format!("{name} must be greater than zero"))
        })
}

fn parse_usize_field(name: &str, value: &str) -> Result<usize, String> {
    value
        .trim()
        .parse::<usize>()
        .map_err(|error| format!("invalid {name}: {error}"))
        .and_then(|parsed| {
            (parsed > 0)
                .then_some(parsed)
                .ok_or_else(|| format!("{name} must be greater than zero"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> Yuv420SpSpec {
        Yuv420SpSpec {
            width: 640,
            height: 480,
            y_stride: 640,
            chroma_stride: 640,
            chroma_order: ChromaOrder::Uv,
            matrix: YuvMatrix::Bt601,
            range: YuvRange::Limited,
        }
    }

    #[test]
    fn builds_yuv_spec_from_draft() {
        let state = YuvInspectorState::new(&spec(), None);
        assert_eq!(state.decode_spec().unwrap(), spec());
    }

    #[test]
    fn suffix_hint_rejects_incompatible_draft_order() {
        let mut state = YuvInspectorState::new(&spec(), Some(ChromaOrder::Uv));
        state.chroma_order = ChromaOrder::Vu;
        assert!(state.decode_spec().is_err());
    }

    #[test]
    fn changed_revision_is_debounced() {
        let now = Instant::now();
        let mut state = YuvInspectorState::new(&spec(), None);
        state.touch(now);
        assert!(!state.submission_due(now));
        assert!(state.submission_due(now + YUV_DECODE_DEBOUNCE));
    }

    #[test]
    fn changed_draft_clears_inflight_generation_before_debounce() {
        let now = Instant::now();
        let mut state = YuvInspectorState::new(&spec(), None);
        state.mark_submitted(7);
        state.touch(now);

        assert_eq!(state.pending_generation, None);
        assert!(state.submission_due(now + YUV_DECODE_DEBOUNCE));
    }
}
