//! 彩色预览参数状态与右侧控制面板。

use camera_toolbox_core::{
    BayerPattern, CfaQuad, ColorPipelineParams, DEFAULT_DISPLAY_GAMMA, RawSpec,
};
use eframe::egui;

const MIN_DISPLAY_GAMMA: f32 = 0.1;
const MAX_DISPLAY_GAMMA: f32 = 5.0;

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
    gamma_value: f32,
    pub(crate) link_black: bool,
    pub(crate) link_green: bool,
    pub(crate) revision: u64,
    pub(crate) submitted_revision: Option<u64>,
    pub(crate) render_error: Option<String>,
}

impl ColorEditState {
    pub(crate) fn new(spec: &RawSpec) -> Self {
        let params = ColorPipelineParams::for_spec(spec);
        let gamma_value = params.display_gamma.unwrap_or(DEFAULT_DISPLAY_GAMMA);
        Self {
            params,
            gamma_value,
            link_black: true,
            link_green: true,
            revision: 1,
            submitted_revision: None,
            render_error: None,
        }
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

    /// RAW frame 替换后保留用户草稿，仅允许当前 revision 重新渲染。
    pub(crate) fn prepare_for_new_frame(&mut self) {
        self.submitted_revision = None;
        self.render_error = None;
    }

    fn link_black_to_r(&mut self) {
        self.params.black_level = CfaQuad::splat(self.params.black_level.r);
    }

    fn link_green_to_gr(&mut self) {
        self.params.gain.gb = self.params.gain.gr;
    }

    fn set_gamma_enabled(&mut self, enabled: bool) {
        self.params.display_gamma = enabled.then_some(self.gamma_value);
    }

    fn set_gamma_value(&mut self, value: f32) {
        self.gamma_value = value.clamp(MIN_DISPLAY_GAMMA, MAX_DISPLAY_GAMMA);
        if self.params.display_gamma.is_some() {
            self.params.display_gamma = Some(self.gamma_value);
        }
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
    let mut response = render_display_and_bayer(ui, state, display_mode);
    response.params_changed |= render_display_gamma(ui, state);
    response.params_changed |= render_black_levels(ui, state, source_spec.max_code_value());
    response.params_changed |= render_channel_gains(ui, state);

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

fn render_display_gamma(ui: &mut egui::Ui, state: &mut ColorEditState) -> bool {
    ui.separator();
    let mut changed = false;
    let mut enabled = state.params.display_gamma.is_some();
    if ui.checkbox(&mut enabled, "Gamma correction").changed() {
        state.set_gamma_enabled(enabled);
        changed = true;
    }

    ui.add_enabled_ui(enabled, |ui| {
        ui.horizontal(|ui| {
            ui.label("Gamma");
            let mut gamma_value = state.gamma_value;
            if ui
                .add(
                    egui::DragValue::new(&mut gamma_value)
                        .range(MIN_DISPLAY_GAMMA..=MAX_DISPLAY_GAMMA)
                        .speed(0.05)
                        .fixed_decimals(2)
                        .update_while_editing(false),
                )
                .changed()
            {
                state.set_gamma_value(gamma_value);
                changed = true;
            }
        });
    });
    changed
}

fn render_black_levels(ui: &mut egui::Ui, state: &mut ColorEditState, max_code: u16) -> bool {
    ui.separator();
    ui.label("Black level (RAW code)");
    let max_black = max_code.saturating_sub(1);
    let mut changed = false;
    let link_changed = ui
        .checkbox(&mut state.link_black, "Link R / Gr / Gb / B")
        .changed();
    if link_changed && state.link_black {
        state.link_black_to_r();
        changed = true;
    }

    if state.link_black {
        if drag_u16(ui, "All", &mut state.params.black_level.r, max_black) {
            state.link_black_to_r();
            changed = true;
        }
    } else {
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

    fn assert_gamma(actual: Option<f32>, expected: f32) {
        let actual = actual.expect("Gamma should be enabled");
        assert!((actual - expected).abs() < f32::EPSILON);
    }

    #[test]
    fn gamma_switch_restores_last_custom_exponent() {
        let mut state = ColorEditState::new(&spec());
        assert_gamma(Some(state.gamma_value), DEFAULT_DISPLAY_GAMMA);
        assert_gamma(state.params.display_gamma, DEFAULT_DISPLAY_GAMMA);

        state.set_gamma_value(1.8);
        assert_gamma(state.params.display_gamma, 1.8);
        state.set_gamma_enabled(false);
        assert_eq!(state.params.display_gamma, None);
        state.set_gamma_enabled(true);
        assert_gamma(state.params.display_gamma, 1.8);
    }

    #[test]
    fn gamma_control_value_is_clamped_to_ui_range() {
        let mut state = ColorEditState::new(&spec());
        state.set_gamma_value(0.0);
        assert_gamma(state.params.display_gamma, MIN_DISPLAY_GAMMA);
        state.set_gamma_value(10.0);
        assert_gamma(state.params.display_gamma, MAX_DISPLAY_GAMMA);
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

    #[test]
    fn preparing_for_new_frame_preserves_user_draft() {
        let mut state = ColorEditState::new(&spec());
        state.params.black_level = CfaQuad {
            r: 1,
            gr: 2,
            gb: 3,
            b: 4,
        };
        state.params.gain.r = 1.25;
        state.set_gamma_value(1.8);
        state.link_black = false;
        state.link_green = false;
        state.touch();
        state.mark_submitted();
        state.mark_error("old frame".to_owned());
        let revision = state.revision;

        state.prepare_for_new_frame();

        assert_eq!(state.params.black_level.r, 1);
        assert!((state.params.gain.r - 1.25).abs() < f32::EPSILON);
        assert_gamma(state.params.display_gamma, 1.8);
        assert!(!state.link_black);
        assert!(!state.link_green);
        assert_eq!(state.revision, revision);
        assert_eq!(state.submitted_revision, None);
        assert_eq!(state.render_error, None);
    }
}
