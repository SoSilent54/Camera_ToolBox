//! 彩色预览参数状态与右侧控制面板。

use camera_toolbox_core::{BayerPattern, CfaQuad, ColorPipelineParams, RawSpec};
use eframe::egui;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DisplayMode {
    RawMono,
    Color,
}

impl DisplayMode {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::RawMono => "Raw Mono",
            Self::Color => "Color",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ColorEditState {
    pub(crate) params: ColorPipelineParams,
    pub(crate) link_black: bool,
    pub(crate) link_green: bool,
    pub(crate) advanced_black: bool,
    pub(crate) revision: u64,
    pub(crate) submitted_revision: Option<u64>,
    pub(crate) render_error: Option<String>,
}

impl ColorEditState {
    pub(crate) fn new(spec: &RawSpec) -> Self {
        Self {
            params: ColorPipelineParams::for_spec(spec),
            link_black: true,
            link_green: true,
            advanced_black: false,
            revision: 1,
            submitted_revision: None,
            render_error: None,
        }
    }

    pub(crate) fn reset(&mut self, spec: &RawSpec) {
        self.params = ColorPipelineParams::for_spec(spec);
        self.link_black = true;
        self.link_green = true;
        self.advanced_black = false;
    }

    pub(crate) fn touch(&mut self) {
        self.revision = self.revision.saturating_add(1);
        self.render_error = None;
    }

    pub(crate) fn mark_submitted(&mut self) {
        self.submitted_revision = Some(self.revision);
        self.render_error = None;
    }

    pub(crate) fn mark_error(&mut self, message: String) {
        self.render_error = Some(message);
    }

    fn link_black_to_r(&mut self) {
        self.params.black_level = CfaQuad::splat(self.params.black_level.r);
    }

    fn link_green_to_gr(&mut self) {
        self.params.gain.gb = self.params.gain.gr;
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ColorControlsResponse {
    pub(crate) params_changed: bool,
    pub(crate) mode_changed: bool,
}

pub(crate) fn render_color_controls(
    ui: &mut egui::Ui,
    state: &mut ColorEditState,
    source_spec: &RawSpec,
    display_mode: &mut DisplayMode,
    installed_revision: Option<u64>,
) -> ColorControlsResponse {
    ui.heading("Color Processing");
    let mut response = render_display_and_bayer(ui, state, display_mode);
    response.params_changed |= render_black_levels(ui, state, source_spec.max_code_value());
    response.params_changed |= render_channel_gains(ui, state);

    ui.separator();
    ui.label("Demosaic: Bilinear");
    ui.label("Transfer: sRGB");
    ui.small("输出为 sensor RGB 预览，不包含 CCM、LSC 或自动 AWB。");
    ui.separator();
    if ui.button("Reset").clicked() {
        state.reset(source_spec);
        response.params_changed = true;
    }
    render_status(ui, state, *display_mode, installed_revision);

    if response.params_changed {
        state.touch();
    }
    response
}

fn render_display_and_bayer(
    ui: &mut egui::Ui,
    state: &mut ColorEditState,
    display_mode: &mut DisplayMode,
) -> ColorControlsResponse {
    ui.separator();
    ui.label("Display");
    let mut response = ColorControlsResponse::default();
    response.mode_changed |= ui
        .radio_value(display_mode, DisplayMode::RawMono, "Raw Mono")
        .changed();
    response.mode_changed |= ui
        .radio_value(display_mode, DisplayMode::Color, "Color")
        .changed();

    ui.separator();
    ui.horizontal(|ui| {
        ui.label("Bayer");
        let before = state.params.bayer;
        egui::ComboBox::from_id_salt("color_bayer")
            .selected_text(bayer_label(state.params.bayer))
            .show_ui(ui, |ui| {
                for pattern in [
                    BayerPattern::Rggb,
                    BayerPattern::Grbg,
                    BayerPattern::Gbrg,
                    BayerPattern::Bggr,
                ] {
                    ui.selectable_value(&mut state.params.bayer, pattern, bayer_label(pattern));
                }
            });
        response.params_changed = state.params.bayer != before;
    });
    response
}

fn render_black_levels(ui: &mut egui::Ui, state: &mut ColorEditState, max_code: u16) -> bool {
    ui.separator();
    ui.label("Black level (RAW code)");
    let max_black = max_code.saturating_sub(1);
    let mut changed = false;
    if state.link_black {
        if drag_u16(ui, "All", &mut state.params.black_level.r, max_black) {
            state.link_black_to_r();
            changed = true;
        }
    } else {
        ui.small("Per-CFA values are active; expand Advanced to edit.");
    }

    let was_open = state.advanced_black;
    let advanced = egui::CollapsingHeader::new("Advanced")
        .id_salt("advanced_black_levels")
        .open(Some(was_open))
        .show(ui, |ui| {
            let link_changed = ui
                .checkbox(&mut state.link_black, "Link R / Gr / Gb / B")
                .changed();
            if link_changed && state.link_black {
                state.link_black_to_r();
                changed = true;
            }
            if !state.link_black {
                egui::Grid::new("color_black_grid")
                    .num_columns(2)
                    .show(ui, |ui| {
                        changed |= drag_u16(ui, "R", &mut state.params.black_level.r, max_black);
                        ui.end_row();
                        changed |= drag_u16(ui, "Gr", &mut state.params.black_level.gr, max_black);
                        ui.end_row();
                        changed |= drag_u16(ui, "Gb", &mut state.params.black_level.gb, max_black);
                        ui.end_row();
                        changed |= drag_u16(ui, "B", &mut state.params.black_level.b, max_black);
                        ui.end_row();
                    });
            }
        });
    if advanced.header_response.clicked() {
        state.advanced_black = !was_open;
    }
    changed
}

fn render_channel_gains(ui: &mut egui::Ui, state: &mut ColorEditState) -> bool {
    ui.separator();
    ui.label("Channel gain");
    let mut changed = false;
    let link_changed = ui.checkbox(&mut state.link_green, "Link Gr / Gb").changed();
    if link_changed && state.link_green {
        state.link_green_to_gr();
        changed = true;
    }
    egui::Grid::new("color_gain_grid")
        .num_columns(2)
        .show(ui, |ui| {
            changed |= drag_gain(ui, "R", &mut state.params.gain.r);
            ui.end_row();
            if state.link_green {
                if drag_gain(ui, "G", &mut state.params.gain.gr) {
                    state.link_green_to_gr();
                    changed = true;
                }
                ui.end_row();
            } else {
                changed |= drag_gain(ui, "Gr", &mut state.params.gain.gr);
                ui.end_row();
                changed |= drag_gain(ui, "Gb", &mut state.params.gain.gb);
                ui.end_row();
            }
            changed |= drag_gain(ui, "B", &mut state.params.gain.b);
            ui.end_row();
        });
    changed
}

fn render_status(
    ui: &mut egui::Ui,
    state: &ColorEditState,
    display_mode: DisplayMode,
    installed_revision: Option<u64>,
) {
    if let Some(error) = &state.render_error {
        ui.colored_label(egui::Color32::RED, error);
    } else if display_mode == DisplayMode::Color && installed_revision != Some(state.revision) {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Rendering color preview…");
        });
    } else if installed_revision == Some(state.revision) {
        ui.small(format!("Rendered revision {}", state.revision));
    }
}

fn drag_u16(ui: &mut egui::Ui, label: &str, value: &mut u16, max: u16) -> bool {
    ui.label(label);
    ui.add(
        egui::DragValue::new(value)
            .range(0..=max)
            .speed(1.0)
            .update_while_editing(false),
    )
    .changed()
}

fn drag_gain(ui: &mut egui::Ui, label: &str, value: &mut f32) -> bool {
    ui.label(label);
    ui.add(
        egui::DragValue::new(value)
            .range(0.0..=16.0)
            .speed(0.01)
            .fixed_decimals(3)
            .update_while_editing(false),
    )
    .changed()
}

const fn bayer_label(pattern: BayerPattern) -> &'static str {
    match pattern {
        BayerPattern::Rggb => "RGGB",
        BayerPattern::Grbg => "GRBG",
        BayerPattern::Gbrg => "GBRG",
        BayerPattern::Bggr => "BGGR",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> RawSpec {
        RawSpec {
            width: 4,
            height: 4,
            bit_depth: 10,
            bayer: BayerPattern::Gbrg,
        }
    }

    #[test]
    fn reset_restores_source_bayer_and_neutral_values() {
        let spec = spec();
        let mut state = ColorEditState::new(&spec);
        state.params.bayer = BayerPattern::Rggb;
        state.params.black_level = CfaQuad::splat(64);
        state.params.gain.r = 2.0;
        state.advanced_black = true;
        let previous_revision = state.revision;

        state.reset(&spec);

        assert_eq!(state.params, ColorPipelineParams::for_spec(&spec));
        assert!(state.link_black);
        assert!(state.link_green);
        assert!(!state.advanced_black);
        assert_eq!(state.revision, previous_revision);
    }

    #[test]
    fn relinking_uses_r_black_and_gr_gain_as_canonical_values() {
        let mut state = ColorEditState::new(&spec());
        state.params.black_level = CfaQuad {
            r: 11,
            gr: 22,
            gb: 33,
            b: 44,
        };
        state.params.gain.gr = 1.25;
        state.params.gain.gb = 2.5;

        state.link_black_to_r();
        state.link_green_to_gr();

        assert_eq!(state.params.black_level, CfaQuad::splat(11));
        assert!((state.params.gain.gb - 1.25).abs() < f32::EPSILON);
    }
}
