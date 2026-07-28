//! Dataset 验收阈值编辑与实时进度可视化；配置仅在当前进程内生效。

use camera_toolbox_app::{AutoAdmissionAssessment, AutoCaptureAcceptanceCriteria};
use eframe::egui;

const DEFAULT_REQUIRED_FOUND_VIEWS: &str = "3";
const DEFAULT_FIELD_COLUMNS: &str = "16";
const DEFAULT_FIELD_ROWS: &str = "9";
const DEFAULT_REQUIRED_FIELD_CELLS: &str = "30";
const DEFAULT_MIN_ADJACENT_SPACING_PX: &str = "12";
const DEFAULT_PNP_DEPTH_MIN: &str = "400";
const DEFAULT_PNP_DEPTH_MAX: &str = "2400";
const DEFAULT_PNP_DEPTH_BINS: &str = "4";
const DEFAULT_REQUIRED_DEPTH_BINS: &str = "3";
const DEFAULT_PNP_TILT_DEADBAND_DEG: &str = "5";
const DEFAULT_PNP_TILT_MAX_DEG: &str = "65";
const DEFAULT_PNP_TILT_BINS: &str = "3";
const DEFAULT_PNP_AZIMUTH_SECTORS: &str = "8";
const DEFAULT_REQUIRED_POSE_BINS: &str = "6";
const DEFAULT_PNP_MAX_RMSE_PX: &str = "1.5";
const DEFAULT_PNP_MAX_ERROR_PX: &str = "4";

/// 文本编辑状态必须保留中间输入；只有完整合法值才会被工作区自动安装。
#[derive(Clone, Debug)]
pub(crate) struct DatasetAcceptanceDraft {
    pub(crate) required_found_views: String,
    pub(crate) field_columns: String,
    pub(crate) field_rows: String,
    pub(crate) required_field_cells: String,
    pub(crate) min_adjacent_spacing_px: String,
    pub(crate) pnp_depth_min: String,
    pub(crate) pnp_depth_max: String,
    pub(crate) pnp_depth_bins: String,
    pub(crate) required_depth_bins: String,
    pub(crate) pnp_tilt_deadband_deg: String,
    pub(crate) pnp_tilt_max_deg: String,
    pub(crate) pnp_tilt_bins: String,
    pub(crate) pnp_azimuth_sectors: String,
    pub(crate) required_pose_bins: String,
    pub(crate) pnp_max_rmse_px: String,
    pub(crate) pnp_max_error_px: String,
    pub(crate) error: Option<String>,
}

impl Default for DatasetAcceptanceDraft {
    fn default() -> Self {
        Self {
            required_found_views: DEFAULT_REQUIRED_FOUND_VIEWS.to_owned(),
            field_columns: DEFAULT_FIELD_COLUMNS.to_owned(),
            field_rows: DEFAULT_FIELD_ROWS.to_owned(),
            required_field_cells: DEFAULT_REQUIRED_FIELD_CELLS.to_owned(),
            min_adjacent_spacing_px: DEFAULT_MIN_ADJACENT_SPACING_PX.to_owned(),
            pnp_depth_min: DEFAULT_PNP_DEPTH_MIN.to_owned(),
            pnp_depth_max: DEFAULT_PNP_DEPTH_MAX.to_owned(),
            pnp_depth_bins: DEFAULT_PNP_DEPTH_BINS.to_owned(),
            required_depth_bins: DEFAULT_REQUIRED_DEPTH_BINS.to_owned(),
            pnp_tilt_deadband_deg: DEFAULT_PNP_TILT_DEADBAND_DEG.to_owned(),
            pnp_tilt_max_deg: DEFAULT_PNP_TILT_MAX_DEG.to_owned(),
            pnp_tilt_bins: DEFAULT_PNP_TILT_BINS.to_owned(),
            pnp_azimuth_sectors: DEFAULT_PNP_AZIMUTH_SECTORS.to_owned(),
            required_pose_bins: DEFAULT_REQUIRED_POSE_BINS.to_owned(),
            pnp_max_rmse_px: DEFAULT_PNP_MAX_RMSE_PX.to_owned(),
            pnp_max_error_px: DEFAULT_PNP_MAX_ERROR_PX.to_owned(),
            error: None,
        }
    }
}

impl DatasetAcceptanceDraft {
    pub(crate) fn parse(&self) -> Result<AutoCaptureAcceptanceCriteria, String> {
        let required_found_views = parse_usize(
            "Required Found views",
            &self.required_found_views,
            3,
            10_000,
        )?;
        let field_columns = parse_usize("Field columns", &self.field_columns, 1, 32)?;
        let field_rows = parse_usize("Field rows", &self.field_rows, 1, 32)?;
        let field_capacity = field_columns
            .checked_mul(field_rows)
            .ok_or_else(|| "Field grid capacity overflows usize.".to_owned())?;
        let required_field_cells = parse_usize(
            "Required field cells",
            &self.required_field_cells,
            1,
            field_capacity,
        )?;
        let min_adjacent_spacing_px =
            parse_positive_f32("Minimum adjacent spacing", &self.min_adjacent_spacing_px)?;
        let pnp_depth_min = parse_positive_f64("PnP minimum depth", &self.pnp_depth_min)?;
        let pnp_depth_max = parse_positive_f64("PnP maximum depth", &self.pnp_depth_max)?;
        if pnp_depth_max <= pnp_depth_min {
            return Err("PnP maximum depth must be greater than minimum depth.".to_owned());
        }
        let pnp_depth_bins = parse_usize("PnP depth bins", &self.pnp_depth_bins, 1, 32)?;
        let required_depth_bins = parse_usize(
            "Required depth bins",
            &self.required_depth_bins,
            1,
            pnp_depth_bins,
        )?;
        let pnp_tilt_deadband_deg =
            parse_non_negative_f64("PnP tilt deadband", &self.pnp_tilt_deadband_deg)?;
        let pnp_tilt_max_deg = parse_non_negative_f64("PnP maximum tilt", &self.pnp_tilt_max_deg)?;
        if pnp_tilt_max_deg <= pnp_tilt_deadband_deg || pnp_tilt_max_deg >= 90.0 {
            return Err(
                "PnP maximum tilt must be greater than the deadband and below 90 degrees."
                    .to_owned(),
            );
        }
        let pnp_tilt_bins = parse_usize("PnP tilt bins", &self.pnp_tilt_bins, 1, 16)?;
        let pnp_azimuth_sectors =
            parse_usize("PnP azimuth sectors", &self.pnp_azimuth_sectors, 1, 32)?;
        let pose_capacity =
            pose_bin_capacity_for_values(pnp_tilt_bins, pnp_azimuth_sectors, pnp_tilt_deadband_deg)
                .ok_or_else(|| "PnP pose-bin capacity overflows usize.".to_owned())?;
        let required_pose_bins = parse_usize(
            "Required pose bins",
            &self.required_pose_bins,
            1,
            pose_capacity,
        )?;
        let pnp_max_rmse_px = parse_non_negative_f64("PnP maximum RMSE", &self.pnp_max_rmse_px)?;
        let pnp_max_error_px =
            parse_non_negative_f64("PnP maximum reprojection error", &self.pnp_max_error_px)?;
        if pnp_max_error_px < pnp_max_rmse_px {
            return Err(
                "PnP maximum reprojection error must be at least the RMSE limit.".to_owned(),
            );
        }
        Ok(AutoCaptureAcceptanceCriteria {
            required_found_views,
            field_columns,
            field_rows,
            required_field_cells,
            min_adjacent_spacing_px,
            pnp_depth_min,
            pnp_depth_max,
            pnp_depth_bins,
            required_depth_bins,
            pnp_tilt_deadband_deg,
            pnp_tilt_max_deg,
            pnp_tilt_bins,
            pnp_azimuth_sectors,
            required_pose_bins,
            pnp_max_rmse_px,
            pnp_max_error_px,
        })
    }
}

fn parse_usize(label: &str, value: &str, minimum: usize, maximum: usize) -> Result<usize, String> {
    let parsed = value
        .trim()
        .parse::<usize>()
        .map_err(|_| format!("{label} must be an integer."))?;
    if (minimum..=maximum).contains(&parsed) {
        Ok(parsed)
    } else {
        Err(format!("{label} must be in {minimum}..={maximum}."))
    }
}

fn parse_positive_f32(label: &str, value: &str) -> Result<f32, String> {
    let parsed = value
        .trim()
        .parse::<f32>()
        .map_err(|_| format!("{label} must be a number."))?;
    if parsed.is_finite() && parsed > 0.0 {
        Ok(parsed)
    } else {
        Err(format!("{label} must be finite and positive."))
    }
}

fn parse_positive_f64(label: &str, value: &str) -> Result<f64, String> {
    let parsed = value
        .trim()
        .parse::<f64>()
        .map_err(|_| format!("{label} must be a number."))?;
    if parsed.is_finite() && parsed > 0.0 {
        Ok(parsed)
    } else {
        Err(format!("{label} must be finite and positive."))
    }
}

fn parse_non_negative_f64(label: &str, value: &str) -> Result<f64, String> {
    let parsed = value
        .trim()
        .parse::<f64>()
        .map_err(|_| format!("{label} must be a number."))?;
    if parsed.is_finite() && parsed >= 0.0 {
        Ok(parsed)
    } else {
        Err(format!("{label} must be finite and non-negative."))
    }
}

fn pose_center_bin_enabled(criteria: &AutoCaptureAcceptanceCriteria) -> bool {
    criteria.pnp_tilt_deadband_deg > 0.0
}

fn pose_bin_capacity_for_values(
    tilt_bins: usize,
    azimuth_sectors: usize,
    tilt_deadband_deg: f64,
) -> Option<usize> {
    let sector_bins = tilt_bins.checked_mul(azimuth_sectors)?;
    sector_bins.checked_add(usize::from(tilt_deadband_deg > 0.0))
}

fn pose_bin_capacity(criteria: &AutoCaptureAcceptanceCriteria) -> usize {
    pose_bin_capacity_for_values(
        criteria.pnp_tilt_bins,
        criteria.pnp_azimuth_sectors,
        criteria.pnp_tilt_deadband_deg,
    )
    .unwrap_or(0)
}

#[derive(Clone, Debug, Default)]
pub(crate) struct DatasetAcceptanceProgress {
    pub(crate) active_criteria: Option<AutoCaptureAcceptanceCriteria>,
    pub(crate) enabled_found_views: usize,
    pub(crate) required_found_views: usize,
    pub(crate) occupied_field_cells: usize,
    pub(crate) required_field_cells: usize,
    pub(crate) field_counts: Vec<usize>,
    pub(crate) field_columns: usize,
    pub(crate) field_rows: usize,
    pub(crate) depth_bin_counts: Vec<usize>,
    pub(crate) occupied_depth_bins: usize,
    pub(crate) required_depth_bins: usize,
    pub(crate) pose_bin_counts: Vec<usize>,
    pub(crate) occupied_pose_bins: usize,
    pub(crate) required_pose_bins: usize,
    pub(crate) collection_target_met: bool,
    pub(crate) found_view_gain: usize,
    pub(crate) field_gain: usize,
    pub(crate) depth_gain: usize,
    pub(crate) pose_gain: usize,
    pub(crate) score: usize,
}

impl DatasetAcceptanceProgress {
    pub(crate) fn from_assessment(assessment: &AutoAdmissionAssessment) -> Self {
        Self {
            active_criteria: assessment.active_criteria.clone(),
            enabled_found_views: assessment.enabled_found_views,
            required_found_views: assessment.required_found_views,
            occupied_field_cells: assessment.field_cells,
            required_field_cells: assessment.required_field_cells,
            field_counts: assessment.field_counts.clone(),
            field_columns: assessment.field_columns,
            field_rows: assessment.field_rows,
            depth_bin_counts: assessment.depth_bin_counts.clone(),
            occupied_depth_bins: assessment.depth_bins,
            required_depth_bins: assessment.required_depth_bins,
            pose_bin_counts: assessment.pose_bin_counts.clone(),
            occupied_pose_bins: assessment.pose_bins,
            required_pose_bins: assessment.required_pose_bins,
            collection_target_met: assessment.collection_target_met,
            found_view_gain: assessment.found_view_gain,
            field_gain: assessment.field_gain,
            depth_gain: assessment.depth_gain,
            pose_gain: assessment.pose_gain,
            score: assessment.constraint_gain,
        }
    }

    pub(crate) fn empty(criteria: &AutoCaptureAcceptanceCriteria) -> Self {
        let pose_capacity = pose_bin_capacity(criteria);
        Self {
            active_criteria: Some(criteria.clone()),
            required_found_views: criteria.required_found_views,
            required_field_cells: criteria.required_field_cells,
            field_counts: vec![0; criteria.field_columns.saturating_mul(criteria.field_rows)],
            field_columns: criteria.field_columns,
            field_rows: criteria.field_rows,
            depth_bin_counts: vec![0; criteria.pnp_depth_bins],
            required_depth_bins: criteria.required_depth_bins,
            pose_bin_counts: vec![0; pose_capacity],
            required_pose_bins: criteria.required_pose_bins,
            ..Self::default()
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DatasetAcceptancePanelState {
    pub(crate) has_live_context: bool,
    pub(crate) admission_active: bool,
    pub(crate) auto_capture_enabled: bool,
}

/// 渲染完成后返回编辑变更和展开 body 的滚动度量；调用者负责立即尝试安装完整合法配置。
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug)]
pub(crate) struct DatasetAcceptanceScrollMetrics {
    pub(crate) content_size: egui::Vec2,
    pub(crate) viewport: egui::Rect,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug)]
pub(crate) struct DatasetAcceptanceRender {
    pub(crate) changed: bool,
    pub(crate) editing: bool,
    pub(crate) foldout_id: egui::Id,
    pub(crate) scroll_metrics: Option<DatasetAcceptanceScrollMetrics>,
}

pub(crate) fn render_dataset_acceptance(
    ui: &mut egui::Ui,
    draft: &mut DatasetAcceptanceDraft,
    progress: &DatasetAcceptanceProgress,
    state: DatasetAcceptancePanelState,
    max_body_height: f32,
) -> DatasetAcceptanceRender {
    let mut changed = false;
    let mut editing = false;
    let mut scroll_metrics = None;
    let foldout = egui::CollapsingHeader::new("Dataset acceptance")
        .id_salt("calibration_dataset_acceptance")
        .show(ui, |ui| {
            let scroll_output = egui::ScrollArea::vertical()
                .id_salt("calibration_dataset_acceptance_scroll")
                .max_height(max_body_height)
                .auto_shrink([false, false])
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded)
                .show(ui, |ui| {
                    render_runtime_state(ui, &state);
                    ui.horizontal_wrapped(|ui| {
                        ui.monospace(format!(
                            "Found {}/{} · Field {}/{} · Depth {}/{} · Pose {}/{} · Score {}",
                            progress.enabled_found_views,
                            progress.required_found_views,
                            progress.occupied_field_cells,
                            progress.required_field_cells,
                            progress.occupied_depth_bins,
                            progress.required_depth_bins,
                            progress.occupied_pose_bins,
                            progress.required_pose_bins,
                            progress.score,
                        ));
                        ui.monospace(format!(
                            "Δ Found {} · Field {} · Depth {} · Pose {}",
                            progress.found_view_gain,
                            progress.field_gain,
                            progress.depth_gain,
                            progress.pose_gain,
                        ));
                    });

                    ui.add_space(4.0);
                    ui.group(|ui| {
                        ui.strong("Found views");
                        metric_row(
                            ui,
                            "Enabled Found views",
                            progress.enabled_found_views,
                            progress.required_found_views,
                        );
                        egui::Grid::new("dataset_acceptance_found_editor")
                            .num_columns(2)
                            .spacing([8.0, 4.0])
                            .show(ui, |ui| {
                                acceptance_text_row(
                                    ui,
                                    "Found view target",
                                    &mut draft.required_found_views,
                                    &mut changed,
                                    &mut editing,
                                );
                            });
                    });

                    ui.group(|ui| {
                        ui.strong("Field coverage");
                        metric_row(
                            ui,
                            "Occupied field cells",
                            progress.occupied_field_cells,
                            progress.required_field_cells,
                        );
                        render_field_grid(ui, progress);
                        egui::Grid::new("dataset_acceptance_field_editor")
                            .num_columns(2)
                            .spacing([8.0, 4.0])
                            .show(ui, |ui| {
                                acceptance_text_row(ui, "Field columns", &mut draft.field_columns, &mut changed, &mut editing);
                                acceptance_text_row(ui, "Field rows", &mut draft.field_rows, &mut changed, &mut editing);
                                acceptance_text_row(
                                    ui,
                                    "Field cell target",
                                    &mut draft.required_field_cells,
                                    &mut changed,
                                    &mut editing,
                                );
                                acceptance_text_row(
                                    ui,
                                    "Minimum spacing (px)",
                                    &mut draft.min_adjacent_spacing_px,
                                    &mut changed,
                                    &mut editing,
                                );
                            });
                    });

                    ui.group(|ui| {
                        ui.strong("Depth coverage");
                        metric_row(
                            ui,
                            "Occupied depth bins",
                            progress.occupied_depth_bins,
                            progress.required_depth_bins,
                        );
                        if let Some(criteria) = progress.active_criteria.as_ref() {
                            render_depth_coverage(ui, progress, criteria);
                        } else {
                            ui.weak("Valid Dataset Acceptance thresholds are required to label depth intervals.");
                        }
                        egui::Grid::new("dataset_acceptance_depth_editor")
                            .num_columns(2)
                            .spacing([8.0, 4.0])
                            .show(ui, |ui| {
                                acceptance_text_row(ui, "PnP depth min", &mut draft.pnp_depth_min, &mut changed, &mut editing);
                                acceptance_text_row(ui, "PnP depth max", &mut draft.pnp_depth_max, &mut changed, &mut editing);
                                acceptance_text_row(ui, "PnP depth bins", &mut draft.pnp_depth_bins, &mut changed, &mut editing);
                                acceptance_text_row(
                                    ui,
                                    "Required depth bins",
                                    &mut draft.required_depth_bins,
                                    &mut changed,
                                    &mut editing,
                                );
                            });
                    });

                    ui.group(|ui| {
                        ui.strong("Pose coverage");
                        metric_row(
                            ui,
                            "Occupied pose bins",
                            progress.occupied_pose_bins,
                            progress.required_pose_bins,
                        );
                        if let Some(criteria) = progress.active_criteria.as_ref() {
                            render_pose_coverage(ui, progress, criteria);
                        } else {
                            ui.weak("Valid Dataset Acceptance thresholds are required to label pose regions.");
                        }
                        egui::Grid::new("dataset_acceptance_pose_editor")
                            .num_columns(2)
                            .spacing([8.0, 4.0])
                            .show(ui, |ui| {
                                acceptance_text_row(
                                    ui,
                                    "PnP tilt deadband (°)",
                                    &mut draft.pnp_tilt_deadband_deg,
                                    &mut changed,
                                    &mut editing,
                                );
                                acceptance_text_row(
                                    ui,
                                    "PnP tilt max (°)",
                                    &mut draft.pnp_tilt_max_deg,
                                    &mut changed,
                                    &mut editing,
                                );
                                acceptance_text_row(ui, "PnP tilt bins", &mut draft.pnp_tilt_bins, &mut changed, &mut editing);
                                acceptance_text_row(
                                    ui,
                                    "PnP azimuth sectors",
                                    &mut draft.pnp_azimuth_sectors,
                                    &mut changed,
                                    &mut editing,
                                );
                                acceptance_text_row(
                                    ui,
                                    "Required pose bins",
                                    &mut draft.required_pose_bins,
                                    &mut changed,
                                    &mut editing,
                                );
                            });
                    });

                    ui.group(|ui| {
                        ui.strong("PnP quality gates");
                        ui.weak(
                            "Only finite, positive-depth PnP evidence within both limits can occupy depth or pose bins.",
                        );
                        egui::Grid::new("dataset_acceptance_pnp_quality_editor")
                            .num_columns(2)
                            .spacing([8.0, 4.0])
                            .show(ui, |ui| {
                                acceptance_text_row(
                                    ui,
                                    "PnP RMSE max (px)",
                                    &mut draft.pnp_max_rmse_px,
                                    &mut changed,
                                    &mut editing,
                                );
                                acceptance_text_row(
                                    ui,
                                    "PnP max error (px)",
                                    &mut draft.pnp_max_error_px,
                                    &mut changed,
                                    &mut editing,
                                );
                            });
                    });

                    if progress.collection_target_met {
                        ui.colored_label(
                            egui::Color32::LIGHT_GREEN,
                            "Collection milestones reached (informational; not a production qualification).",
                        );
                    } else {
                        ui.weak("Depth/Pose count only current compatible PnP evidence; automatic candidates are admitted by a separate source-bound rule.");
                    }
                    if changed {
                        draft.error = None;
                    }
                    if let Some(error) = draft.error.as_deref() {
                        if editing {
                            ui.weak("Editing thresholds; last valid settings stay active until the fields are complete.");
                        } else {
                            ui.colored_label(egui::Color32::LIGHT_RED, error);
                        }
                    } else {
                        ui.weak("Valid complete changes take effect immediately and are not saved to disk.");
                    }
                    ui.weak("PnP for any Dataset source uses the current GUI K and D12 seed; an incomplete edit retains the last valid live binding.");
                });
            scroll_metrics = Some(DatasetAcceptanceScrollMetrics {
                content_size: scroll_output.content_size,
                viewport: scroll_output.inner_rect,
            });
        });
    DatasetAcceptanceRender {
        changed,
        editing,
        foldout_id: foldout.header_response.id,
        scroll_metrics,
    }
}

fn render_runtime_state(ui: &mut egui::Ui, state: &DatasetAcceptancePanelState) {
    ui.weak(
        "Dataset progress aggregates every enabled Found image at the current common geometry; Local, SFTP, and RTSP provenance are equivalent.",
    );
    match (
        state.has_live_context,
        state.admission_active,
        state.auto_capture_enabled,
    ) {
        (false, _, _) => {
            ui.weak(
                "Found and field coverage can still update; Depth/Pose require a displayed live frame with a valid current K/D12 binding, then Detect.",
            );
        }
        (true, true, true) => {
            ui.colored_label(
                egui::Color32::LIGHT_GREEN,
                "Dataset progress is source-independent; automatic candidate admission is separately active for the displayed source.",
            );
        }
        (true, true, false) => {
            ui.colored_label(
                egui::Color32::YELLOW,
                "Dataset progress is active; Auto Capture is off. A later automatic candidate remains source-bound.",
            );
        }
        (true, false, _) => {
            ui.colored_label(
                egui::Color32::YELLOW,
                "Dataset Found/Field progress is available; complete valid K/D12 inputs are required for current Depth/Pose evidence.",
            );
        }
    }
}

fn metric_row(ui: &mut egui::Ui, label: &str, current: usize, target: usize) {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.monospace(format!("{current} / {target}"));
        let width = ui.available_width().max(72.0);
        ui.add_sized(
            [width, 18.0],
            egui::ProgressBar::new(progress_ratio(current, target))
                .text(format!("{current}/{target}")),
        );
    });
}

fn progress_ratio(current: usize, target: usize) -> f32 {
    if target == 0 {
        0.0
    } else {
        (current as f32 / target as f32).clamp(0.0, 1.0)
    }
}

fn acceptance_text_row(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut String,
    changed: &mut bool,
    editing: &mut bool,
) {
    ui.label(label);
    let response = ui
        .push_id(label, |ui| {
            ui.add(egui::TextEdit::singleline(value).desired_width(84.0))
        })
        .inner;
    *changed |= response.changed();
    *editing |= response.has_focus();
    ui.end_row();
}

fn coverage_color(count: usize, maximum: usize) -> egui::Color32 {
    if count == 0 || maximum == 0 {
        egui::Color32::from_gray(42)
    } else {
        let density = count as f32 / maximum as f32;
        egui::Color32::from_rgb(
            (50.0 + 35.0 * density) as u8,
            (105.0 + 120.0 * density) as u8,
            (85.0 + 45.0 * density) as u8,
        )
    }
}

fn coverage_text_color(count: usize, maximum: usize) -> egui::Color32 {
    if maximum != 0 && count as f32 / maximum as f32 > 0.6 {
        egui::Color32::from_rgb(12, 32, 45)
    } else {
        egui::Color32::WHITE
    }
}

fn render_field_grid(ui: &mut egui::Ui, progress: &DatasetAcceptanceProgress) {
    ui.label("Per-cell chessboard-corner count (shown in each cell)");
    if progress.field_columns == 0 || progress.field_rows == 0 {
        return;
    }
    let cell_edge =
        (ui.available_width().min(320.0) / progress.field_columns as f32).clamp(8.0, 18.0);
    let size = egui::vec2(
        cell_edge * progress.field_columns as f32,
        cell_edge * progress.field_rows as f32,
    );
    let label_font = egui::FontId::proportional((cell_edge * 0.58).clamp(7.0, 12.0));
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 3.0, egui::Color32::from_gray(24));
    let maximum = progress.field_counts.iter().copied().max().unwrap_or(0);
    let mut missing = Vec::new();
    for row in 0..progress.field_rows {
        for column in 0..progress.field_columns {
            let cell = row * progress.field_columns + column;
            let count = progress.field_counts.get(cell).copied().unwrap_or(0);
            let x0 = rect.left() + rect.width() * column as f32 / progress.field_columns as f32;
            let x1 =
                rect.left() + rect.width() * (column + 1) as f32 / progress.field_columns as f32;
            let y0 = rect.top() + rect.height() * row as f32 / progress.field_rows as f32;
            let y1 = rect.top() + rect.height() * (row + 1) as f32 / progress.field_rows as f32;
            let cell_rect =
                egui::Rect::from_min_max(egui::pos2(x0, y0), egui::pos2(x1, y1)).shrink(1.0);
            painter.rect_filled(cell_rect, 1.5, coverage_color(count, maximum));
            painter.text(
                cell_rect.center(),
                egui::Align2::CENTER_CENTER,
                count.to_string(),
                label_font.clone(),
                coverage_text_color(count, maximum),
            );
            if count == 0 {
                missing.push(format!("r{}c{}", row + 1, column + 1));
            }
        }
    }
    response.on_hover_text(
        "Each cell displays the number of enabled Found chessboard corners in that image region.",
    );
    render_missing_summary(ui, "Missing field cells", &missing);
    ui.weak(format!("Legend: empty / occupied / max {maximum} corners"));
}

fn render_depth_coverage(
    ui: &mut egui::Ui,
    progress: &DatasetAcceptanceProgress,
    criteria: &AutoCaptureAcceptanceCriteria,
) {
    ui.label("Depth intervals (configured board units; each cell counts chessboard-corner depths; final interval includes its maximum)");
    let bin_count = criteria.pnp_depth_bins;
    if bin_count == 0 {
        return;
    }
    let columns = bin_count.min(8).max(1);
    let rows = (bin_count + columns - 1) / columns;
    let size = egui::vec2(
        ui.available_width().clamp(180.0, 460.0),
        (46.0 * rows as f32).clamp(46.0, 184.0),
    );
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 3.0, egui::Color32::from_gray(24));
    let maximum = progress.depth_bin_counts.iter().copied().max().unwrap_or(0);
    let mut missing = Vec::new();
    for index in 0..bin_count {
        let count = progress.depth_bin_counts.get(index).copied().unwrap_or(0);
        let row = index / columns;
        let column = index % columns;
        let x0 = rect.left() + rect.width() * column as f32 / columns as f32;
        let x1 = rect.left() + rect.width() * (column + 1) as f32 / columns as f32;
        let y0 = rect.top() + rect.height() * row as f32 / rows as f32;
        let y1 = rect.top() + rect.height() * (row + 1) as f32 / rows as f32;
        let cell_rect =
            egui::Rect::from_min_max(egui::pos2(x0, y0), egui::pos2(x1, y1)).shrink(1.0);
        let label = depth_interval_label(criteria, index);
        painter.rect_filled(cell_rect, 1.5, coverage_color(count, maximum));
        painter.text(
            egui::pos2(cell_rect.center().x, cell_rect.top() + 11.0),
            egui::Align2::CENTER_CENTER,
            label.clone(),
            egui::FontId::proportional(9.0),
            coverage_text_color(count, maximum),
        );
        painter.text(
            egui::pos2(cell_rect.center().x, cell_rect.bottom() - 11.0),
            egui::Align2::CENTER_CENTER,
            format!("{count} corners"),
            egui::FontId::proportional(10.0),
            coverage_text_color(count, maximum),
        );
        ui.interact(
            cell_rect,
            ui.id().with(("dataset_acceptance_depth_bin", index)),
            egui::Sense::hover(),
        )
        .on_hover_text(format!(
            "Depth {label}: {count} compatible chessboard-corner depths."
        ));
        if count == 0 {
            missing.push(label);
        }
    }
    render_missing_summary(ui, "Missing depth intervals", &missing);
}

fn depth_interval_label(criteria: &AutoCaptureAcceptanceCriteria, index: usize) -> String {
    let width = (criteria.pnp_depth_max - criteria.pnp_depth_min) / criteria.pnp_depth_bins as f64;
    let lower = criteria.pnp_depth_min + width * index as f64;
    let upper = if index + 1 == criteria.pnp_depth_bins {
        criteria.pnp_depth_max
    } else {
        criteria.pnp_depth_min + width * (index + 1) as f64
    };
    let close = if index + 1 == criteria.pnp_depth_bins {
        ']'
    } else {
        ')'
    };
    format!("[{lower:.0}, {upper:.0}{close}")
}

fn render_pose_coverage(
    ui: &mut egui::Ui,
    progress: &DatasetAcceptanceProgress,
    criteria: &AutoCaptureAcceptanceCriteria,
) {
    if pose_center_bin_enabled(criteria) {
        ui.label(
            "Pose regions: center is front-parallel; OpenCV azimuth: 0° right (+x), 90° down (+y).",
        );
    } else {
        ui.label(
            "Pose regions: no center deadband; rings start at 0° tilt. OpenCV azimuth: 0° right (+x), 90° down (+y).",
        );
    }
    let region_count = criteria
        .pnp_tilt_bins
        .saturating_mul(criteria.pnp_azimuth_sectors);
    if region_count > 32 {
        render_pose_grid(ui, progress, criteria);
    } else {
        render_pose_polar_map(ui, progress, criteria);
    }
    let missing = (0..pose_bin_capacity(criteria))
        .filter(|index| progress.pose_bin_counts.get(*index).copied().unwrap_or(0) == 0)
        .map(|index| pose_bin_label(criteria, index))
        .collect::<Vec<_>>();
    render_missing_summary(ui, "Missing pose regions", &missing);
}

// 最少八段使单扇区的每个弧段不超过 45°，即使只有一个 azimuth sector 也保持凸四边形。
const POSE_ARC_SUBDIVISIONS: usize = 8;

fn render_pose_polar_map(
    ui: &mut egui::Ui,
    progress: &DatasetAcceptanceProgress,
    criteria: &AutoCaptureAcceptanceCriteria,
) {
    let side = ui.available_width().clamp(160.0, 220.0);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 3.0, egui::Color32::from_gray(24));
    let center = rect.center();
    let outer_radius = side * 0.43;
    let center_enabled = pose_center_bin_enabled(criteria);
    let center_radius = if center_enabled {
        outer_radius / (criteria.pnp_tilt_bins + 1) as f32
    } else {
        0.0
    };
    let maximum = progress.pose_bin_counts.iter().copied().max().unwrap_or(0);
    let center_count = progress.pose_bin_counts.first().copied().unwrap_or(0);
    let sector_offset = usize::from(center_enabled);
    let sector_count = criteria
        .pnp_tilt_bins
        .saturating_mul(criteria.pnp_azimuth_sectors);
    let mut fill_mesh = egui::Mesh::default();
    fill_mesh.reserve_vertices(sector_count * POSE_ARC_SUBDIVISIONS * 4);
    fill_mesh.reserve_triangles(sector_count * POSE_ARC_SUBDIVISIONS * 2);
    for tilt_bin in 0..criteria.pnp_tilt_bins {
        let inner_radius = center_radius
            + (outer_radius - center_radius) * tilt_bin as f32 / criteria.pnp_tilt_bins as f32;
        let ring_outer = center_radius
            + (outer_radius - center_radius) * (tilt_bin + 1) as f32
                / criteria.pnp_tilt_bins as f32;
        for sector in 0..criteria.pnp_azimuth_sectors {
            let index = sector_offset + tilt_bin * criteria.pnp_azimuth_sectors + sector;
            let count = progress.pose_bin_counts.get(index).copied().unwrap_or(0);
            let start = sector as f32 / criteria.pnp_azimuth_sectors as f32 * std::f32::consts::TAU;
            let end =
                (sector + 1) as f32 / criteria.pnp_azimuth_sectors as f32 * std::f32::consts::TAU;
            append_annular_sector(
                &mut fill_mesh,
                center,
                inner_radius,
                ring_outer,
                start,
                end,
                coverage_color(count, maximum),
            );
        }
    }
    painter.add(egui::Shape::mesh(fill_mesh));

    let border = egui::Stroke::new(0.5, egui::Color32::from_gray(24));
    for tilt_bin in 0..criteria.pnp_tilt_bins {
        let inner_radius = center_radius
            + (outer_radius - center_radius) * tilt_bin as f32 / criteria.pnp_tilt_bins as f32;
        let ring_outer = center_radius
            + (outer_radius - center_radius) * (tilt_bin + 1) as f32
                / criteria.pnp_tilt_bins as f32;
        for sector in 0..criteria.pnp_azimuth_sectors {
            let index = sector_offset + tilt_bin * criteria.pnp_azimuth_sectors + sector;
            let count = progress.pose_bin_counts.get(index).copied().unwrap_or(0);
            let start = sector as f32 / criteria.pnp_azimuth_sectors as f32 * std::f32::consts::TAU;
            let end =
                (sector + 1) as f32 / criteria.pnp_azimuth_sectors as f32 * std::f32::consts::TAU;
            paint_annular_sector_boundaries(
                &painter,
                center,
                inner_radius,
                ring_outer,
                start,
                end,
                border,
            );
            let label_radius = (inner_radius + ring_outer) * 0.5;
            painter.text(
                polar_point(center, label_radius, (start + end) * 0.5),
                egui::Align2::CENTER_CENTER,
                count.to_string(),
                egui::FontId::proportional(8.0),
                coverage_text_color(count, maximum),
            );
        }
    }
    // 环带内弦会略微进入理论内圆；只有存在 deadband 时才绘制中心 bin 作为遮罩。
    if center_enabled {
        painter.circle_filled(center, center_radius, coverage_color(center_count, maximum));
        painter.circle_stroke(center, center_radius, border);
        painter.text(
            center,
            egui::Align2::CENTER_CENTER,
            center_count.to_string(),
            egui::FontId::proportional(10.0),
            coverage_text_color(center_count, maximum),
        );
    }
    if let Some(position) = response.hover_pos()
        && let Some(index) = pose_bin_at(position, center, center_radius, outer_radius, criteria)
    {
        let count = progress.pose_bin_counts.get(index).copied().unwrap_or(0);
        response.on_hover_text(format!(
            "{}: {count} compatible PnP observations.",
            pose_bin_label(criteria, index)
        ));
    }
    ui.weak("Each sector shows its count; hover a region for its tilt and azimuth interval.");
}

/// 将带内孔的环形扇区拆为凸四边形，避免 `convex_polygon` 的扇形三角化跨越中心孔。
fn append_annular_sector(
    mesh: &mut egui::Mesh,
    center: egui::Pos2,
    inner_radius: f32,
    outer_radius: f32,
    start: f32,
    end: f32,
    color: egui::Color32,
) {
    for segment in 0..POSE_ARC_SUBDIVISIONS {
        let segment_start = start + (end - start) * segment as f32 / POSE_ARC_SUBDIVISIONS as f32;
        let segment_end =
            start + (end - start) * (segment + 1) as f32 / POSE_ARC_SUBDIVISIONS as f32;
        let quad = annular_sector_quad(
            center,
            inner_radius,
            outer_radius,
            segment_start,
            segment_end,
        );
        let first = mesh.vertices.len() as u32;
        for point in quad {
            mesh.colored_vertex(point, color);
        }
        mesh.add_triangle(first, first + 1, first + 2);
        mesh.add_triangle(first, first + 2, first + 3);
    }
}

fn paint_annular_sector_boundaries(
    painter: &egui::Painter,
    center: egui::Pos2,
    inner_radius: f32,
    outer_radius: f32,
    start: f32,
    end: f32,
    border: egui::Stroke,
) {
    painter.line_segment(
        [
            polar_point(center, inner_radius, start),
            polar_point(center, outer_radius, start),
        ],
        border,
    );
    for segment in 0..POSE_ARC_SUBDIVISIONS {
        let segment_start = start + (end - start) * segment as f32 / POSE_ARC_SUBDIVISIONS as f32;
        let segment_end =
            start + (end - start) * (segment + 1) as f32 / POSE_ARC_SUBDIVISIONS as f32;
        let quad = annular_sector_quad(
            center,
            inner_radius,
            outer_radius,
            segment_start,
            segment_end,
        );
        painter.line_segment([quad[0], quad[1]], border);
        painter.line_segment([quad[3], quad[2]], border);
    }
}

fn annular_sector_quad(
    center: egui::Pos2,
    inner_radius: f32,
    outer_radius: f32,
    start: f32,
    end: f32,
) -> [egui::Pos2; 4] {
    [
        polar_point(center, outer_radius, start),
        polar_point(center, outer_radius, end),
        polar_point(center, inner_radius, end),
        polar_point(center, inner_radius, start),
    ]
}

fn polar_point(center: egui::Pos2, radius: f32, angle: f32) -> egui::Pos2 {
    egui::pos2(
        center.x + radius * angle.cos(),
        center.y + radius * angle.sin(),
    )
}

fn pose_bin_at(
    position: egui::Pos2,
    center: egui::Pos2,
    center_radius: f32,
    outer_radius: f32,
    criteria: &AutoCaptureAcceptanceCriteria,
) -> Option<usize> {
    let x = position.x - center.x;
    let y = position.y - center.y;
    let radius = (x * x + y * y).sqrt();
    if radius > outer_radius {
        return None;
    }
    if pose_center_bin_enabled(criteria) && radius < center_radius {
        return Some(0);
    }
    let normalized_radius = (radius - center_radius) / (outer_radius - center_radius);
    let tilt_bin = (normalized_radius * criteria.pnp_tilt_bins as f32)
        .floor()
        .min((criteria.pnp_tilt_bins - 1) as f32) as usize;
    let angle = y.atan2(x).to_degrees().rem_euclid(360.0);
    let sector = (angle / 360.0 * criteria.pnp_azimuth_sectors as f32)
        .floor()
        .min((criteria.pnp_azimuth_sectors - 1) as f32) as usize;
    Some(
        usize::from(pose_center_bin_enabled(criteria))
            + tilt_bin * criteria.pnp_azimuth_sectors
            + sector,
    )
}

fn render_pose_grid(
    ui: &mut egui::Ui,
    progress: &DatasetAcceptanceProgress,
    criteria: &AutoCaptureAcceptanceCriteria,
) {
    ui.weak(
        "Using a labeled grid because this pose configuration has more than 32 annular sectors.",
    );
    let region_count = pose_bin_capacity(criteria);
    let maximum = progress.pose_bin_counts.iter().copied().max().unwrap_or(0);
    egui::Grid::new("dataset_acceptance_pose_grid")
        .num_columns(8)
        .spacing([3.0, 3.0])
        .show(ui, |ui| {
            for index in 0..region_count {
                let count = progress.pose_bin_counts.get(index).copied().unwrap_or(0);
                ui.add(
                    egui::Button::new(format!("#{index} {count}"))
                        .fill(coverage_color(count, maximum)),
                )
                .on_hover_text(format!(
                    "{}: {count} compatible PnP observations.",
                    pose_bin_label(criteria, index)
                ));
                if (index + 1) % 8 == 0 {
                    ui.end_row();
                }
            }
        });
}

fn pose_bin_label(criteria: &AutoCaptureAcceptanceCriteria, index: usize) -> String {
    let sector_offset = usize::from(pose_center_bin_enabled(criteria));
    if pose_center_bin_enabled(criteria) && index == 0 {
        return format!("Center: tilt < {:.1}°", criteria.pnp_tilt_deadband_deg);
    }
    let offset = index.saturating_sub(sector_offset);
    let tilt_bin = offset / criteria.pnp_azimuth_sectors;
    let sector = offset % criteria.pnp_azimuth_sectors;
    let tilt_min = if pose_center_bin_enabled(criteria) {
        criteria.pnp_tilt_deadband_deg
    } else {
        0.0
    };
    let span = (criteria.pnp_tilt_max_deg - tilt_min) / criteria.pnp_tilt_bins as f64;
    let tilt_lower = tilt_min + span * tilt_bin as f64;
    let tilt_upper = if tilt_bin + 1 == criteria.pnp_tilt_bins {
        criteria.pnp_tilt_max_deg
    } else {
        tilt_min + span * (tilt_bin + 1) as f64
    };
    let azimuth_lower = 360.0 * sector as f64 / criteria.pnp_azimuth_sectors as f64;
    let azimuth_upper = 360.0 * (sector + 1) as f64 / criteria.pnp_azimuth_sectors as f64;
    let tilt_close = if tilt_bin + 1 == criteria.pnp_tilt_bins {
        ']'
    } else {
        ')'
    };
    format!(
        "Tilt [{tilt_lower:.1}°, {tilt_upper:.1}°{tilt_close} · azimuth [{azimuth_lower:.0}°, {azimuth_upper:.0}°)"
    )
}

fn render_missing_summary(ui: &mut egui::Ui, label: &str, missing: &[String]) {
    if missing.is_empty() {
        ui.colored_label(egui::Color32::LIGHT_GREEN, format!("{label}: none"));
    } else {
        ui.horizontal_wrapped(|ui| {
            ui.strong(format!("{label}:"));
            ui.label(missing.join(" · "));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acceptance_draft_parses_runtime_pnp_thresholds() {
        let criteria = DatasetAcceptanceDraft::default().parse().unwrap();
        assert_eq!(criteria.required_found_views, 3);
        assert_eq!((criteria.field_columns, criteria.field_rows), (16, 9));
        assert_eq!(criteria.required_field_cells, 30);
        assert_eq!(
            (criteria.pnp_depth_min, criteria.pnp_depth_max),
            (400.0, 2400.0)
        );
        assert_eq!(
            (criteria.pnp_depth_bins, criteria.required_depth_bins),
            (4, 3)
        );
        assert_eq!(criteria.required_pose_bins, 6);
    }

    #[test]
    fn acceptance_draft_rejects_invalid_grid_and_pnp_ranges() {
        let mut draft = DatasetAcceptanceDraft {
            required_field_cells: "145".to_owned(),
            ..DatasetAcceptanceDraft::default()
        };
        assert!(draft.parse().unwrap_err().contains("1..=144"));

        draft.required_field_cells = "30".to_owned();
        draft.pnp_depth_max = "400".to_owned();
        assert!(draft.parse().unwrap_err().contains("greater than minimum"));

        draft.pnp_depth_max = "2400".to_owned();
        draft.pnp_max_error_px = "1.0".to_owned();
        assert!(draft.parse().unwrap_err().contains("at least the RMSE"));
    }

    #[test]
    fn acceptance_foldout_stays_open_across_progress_and_draft_changes() {
        let context = egui::Context::default();
        let criteria = DatasetAcceptanceDraft::default().parse().unwrap();
        let mut draft = DatasetAcceptanceDraft::default();
        let mut progress = DatasetAcceptanceProgress::empty(&criteria);
        let state = DatasetAcceptancePanelState {
            has_live_context: true,
            admission_active: true,
            auto_capture_enabled: true,
        };
        let mut foldout_id = None;
        let _ = context.run_ui(egui::RawInput::default(), |ui| {
            foldout_id =
                Some(render_dataset_acceptance(ui, &mut draft, &progress, state, 160.0).foldout_id);
        });
        let foldout_id = foldout_id.expect("Dataset Acceptance foldout id");
        let mut collapsing = egui::collapsing_header::CollapsingState::load_with_default_open(
            &context, foldout_id, false,
        );
        collapsing.set_open(true);
        collapsing.store(&context);

        progress.occupied_field_cells = 1;
        progress.occupied_depth_bins = 1;
        progress.occupied_pose_bins = 1;
        draft.required_found_views = "4".to_owned();
        let _ = context.run_ui(egui::RawInput::default(), |ui| {
            let _ = render_dataset_acceptance(ui, &mut draft, &progress, state, 160.0);
        });

        assert!(
            egui::collapsing_header::CollapsingState::load(&context, foldout_id)
                .expect("Dataset Acceptance foldout state")
                .is_open()
        );
    }

    #[test]
    fn acceptance_scroll_area_constrains_expanded_content() {
        let context = egui::Context::default();
        let criteria = DatasetAcceptanceDraft::default().parse().unwrap();
        let mut draft = DatasetAcceptanceDraft::default();
        let progress = DatasetAcceptanceProgress::empty(&criteria);
        let state = DatasetAcceptancePanelState {
            has_live_context: true,
            admission_active: true,
            auto_capture_enabled: true,
        };
        let mut foldout_id = None;
        let _ = context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(320.0, 320.0),
                )),
                ..Default::default()
            },
            |ui| {
                foldout_id = Some(
                    render_dataset_acceptance(ui, &mut draft, &progress, state, 96.0).foldout_id,
                );
            },
        );
        let foldout_id = foldout_id.expect("Dataset Acceptance foldout id");
        let mut collapsing = egui::collapsing_header::CollapsingState::load_with_default_open(
            &context, foldout_id, false,
        );
        collapsing.set_open(true);
        collapsing.store(&context);

        let mut rendered = None;
        let _ = context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(320.0, 320.0),
                )),
                ..Default::default()
            },
            |ui| {
                rendered = Some(render_dataset_acceptance(
                    ui, &mut draft, &progress, state, 96.0,
                ));
            },
        );
        let scroll_metrics = rendered
            .expect("Dataset Acceptance render output")
            .scroll_metrics
            .expect("expanded Dataset Acceptance scroll metrics");
        assert!(scroll_metrics.content_size.y > scroll_metrics.viewport.height());
        assert!(scroll_metrics.viewport.height() <= 96.0 + f32::EPSILON);
    }

    #[test]
    fn coverage_labels_preserve_final_depth_boundary_and_pose_convention() {
        let criteria = DatasetAcceptanceDraft::default().parse().unwrap();
        assert_eq!(depth_interval_label(&criteria, 0), "[400, 900)");
        assert_eq!(depth_interval_label(&criteria, 3), "[1900, 2400]");
        assert_eq!(pose_bin_label(&criteria, 0), "Center: tilt < 5.0°");
        assert!(pose_bin_label(&criteria, 1).contains("azimuth [0°, 45°)"));
    }

    #[test]
    fn zero_deadband_pose_map_has_no_center_bin_or_circle() {
        let context = egui::Context::default();
        let mut criteria = DatasetAcceptanceDraft::default().parse().unwrap();
        criteria.pnp_tilt_deadband_deg = 0.0;
        criteria.pnp_tilt_bins = 1;
        criteria.pnp_azimuth_sectors = 4;
        criteria.required_pose_bins = 4;
        let mut progress = DatasetAcceptanceProgress::empty(&criteria);
        progress.pose_bin_counts = vec![1, 2, 3, 4];

        assert_eq!(pose_bin_capacity(&criteria), 4);
        assert!(!pose_bin_label(&criteria, 0).contains("Center"));

        let output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(320.0, 320.0),
                )),
                ..Default::default()
            },
            |ui| render_pose_polar_map(ui, &progress, &criteria),
        );
        assert!(!output.shapes.iter().any(|clipped| {
            matches!(
                &clipped.shape,
                egui::epaint::Shape::Circle(circle)
                    if circle.fill != egui::Color32::TRANSPARENT
            )
        }));
    }

    #[test]
    fn pose_polar_map_uses_opencv_image_axes() {
        let mut criteria = DatasetAcceptanceDraft::default().parse().unwrap();
        criteria.pnp_tilt_bins = 1;
        criteria.pnp_azimuth_sectors = 4;
        let center = egui::pos2(100.0, 100.0);
        let center_radius = 10.0;
        let outer_radius = 50.0;
        let sample_radius = 30.0;
        let assert_pos_close = |actual: egui::Pos2, expected: egui::Pos2| {
            assert!(
                (actual.x - expected.x).abs() < 1.0e-4,
                "x: {actual:?} != {expected:?}"
            );
            assert!(
                (actual.y - expected.y).abs() < 1.0e-4,
                "y: {actual:?} != {expected:?}"
            );
        };

        assert_pos_close(
            polar_point(center, sample_radius, 0.0),
            egui::pos2(130.0, 100.0),
        );
        assert_pos_close(
            polar_point(center, sample_radius, std::f32::consts::FRAC_PI_2),
            egui::pos2(100.0, 130.0),
        );
        assert_pos_close(
            polar_point(center, sample_radius, std::f32::consts::PI),
            egui::pos2(70.0, 100.0),
        );
        assert_pos_close(
            polar_point(center, sample_radius, std::f32::consts::PI * 1.5),
            egui::pos2(100.0, 70.0),
        );

        assert_eq!(
            pose_bin_at(
                egui::pos2(130.0, 100.0),
                center,
                center_radius,
                outer_radius,
                &criteria,
            ),
            Some(1)
        );
        assert_eq!(
            pose_bin_at(
                egui::pos2(100.0, 130.0),
                center,
                center_radius,
                outer_radius,
                &criteria,
            ),
            Some(2)
        );
        assert_eq!(
            pose_bin_at(
                egui::pos2(70.0, 100.0),
                center,
                center_radius,
                outer_radius,
                &criteria,
            ),
            Some(3)
        );
        assert_eq!(
            pose_bin_at(
                egui::pos2(100.0, 70.0),
                center,
                center_radius,
                outer_radius,
                &criteria,
            ),
            Some(4)
        );
    }

    #[test]
    fn annular_sector_tessellation_preserves_hole_for_one_and_two_sectors() {
        let center = egui::pos2(0.0, 0.0);
        for sectors in [1_usize, 2] {
            let mut mesh = egui::Mesh::default();
            append_annular_sector(
                &mut mesh,
                center,
                10.0,
                20.0,
                0.0,
                std::f32::consts::TAU / sectors as f32,
                egui::Color32::WHITE,
            );
            assert!(mesh.is_valid());
            assert_eq!(mesh.vertices.len(), POSE_ARC_SUBDIVISIONS * 4);
            assert_eq!(mesh.indices.len(), POSE_ARC_SUBDIVISIONS * 6);
            for (quad_index, indices) in mesh.indices.chunks_exact(6).enumerate() {
                let first = (quad_index * 4) as u32;
                assert_eq!(
                    indices,
                    &[first, first + 1, first + 2, first, first + 2, first + 3]
                );
            }
            assert!(mesh.vertices.iter().all(|vertex| {
                let radius = (vertex.pos - center).length();
                (10.0 - 1.0e-4..=20.0 + 1.0e-4).contains(&radius)
            }));
            for quad in mesh.vertices.chunks_exact(4) {
                let turn = |first: egui::Pos2, second: egui::Pos2, third: egui::Pos2| {
                    let first_edge = second - first;
                    let second_edge = third - second;
                    first_edge.x * second_edge.y - first_edge.y * second_edge.x
                };
                let turns = [
                    turn(quad[0].pos, quad[1].pos, quad[2].pos),
                    turn(quad[1].pos, quad[2].pos, quad[3].pos),
                    turn(quad[2].pos, quad[3].pos, quad[0].pos),
                    turn(quad[3].pos, quad[0].pos, quad[1].pos),
                ];
                let positive = turns[0].is_sign_positive();
                assert!(
                    turns.iter().all(|value| {
                        value.abs() > 1.0e-4 && value.is_sign_positive() == positive
                    })
                );
            }
        }
    }

    #[test]
    fn polar_map_draws_center_mask_after_ring_mesh_and_boundaries() {
        let context = egui::Context::default();
        let mut criteria = DatasetAcceptanceDraft::default().parse().unwrap();
        criteria.pnp_tilt_bins = 1;
        criteria.pnp_azimuth_sectors = 1;
        let mut progress = DatasetAcceptanceProgress::empty(&criteria);
        progress.pose_bin_counts = vec![7, 3];
        let output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(320.0, 320.0),
                )),
                ..Default::default()
            },
            |ui| render_pose_polar_map(ui, &progress, &criteria),
        );

        let mesh_index = output
            .shapes
            .iter()
            .position(|clipped| matches!(&clipped.shape, egui::epaint::Shape::Mesh(_)))
            .expect("polar map mesh");
        let last_boundary_index = output
            .shapes
            .iter()
            .rposition(|clipped| {
                matches!(
                    &clipped.shape,
                    egui::epaint::Shape::LineSegment { stroke, .. }
                        if (stroke.width - 0.5).abs() <= f32::EPSILON
                )
            })
            .expect("annular boundary");
        let center_fill_index = output
            .shapes
            .iter()
            .position(|clipped| {
                matches!(
                    &clipped.shape,
                    egui::epaint::Shape::Circle(circle)
                        if circle.fill != egui::Color32::TRANSPARENT
                )
            })
            .expect("center fill");
        let center_stroke_index = output
            .shapes
            .iter()
            .position(|clipped| {
                matches!(
                    &clipped.shape,
                    egui::epaint::Shape::Circle(circle)
                        if circle.fill == egui::Color32::TRANSPARENT
                            && (circle.stroke.width - 0.5).abs() <= f32::EPSILON
                )
            })
            .expect("center stroke");
        let center_text_index = output
            .shapes
            .iter()
            .position(|clipped| {
                matches!(
                    &clipped.shape,
                    egui::epaint::Shape::Text(text) if text.galley.job.text == "7"
                )
            })
            .expect("center count");

        assert!(mesh_index < last_boundary_index);
        assert!(last_boundary_index < center_fill_index);
        assert!(center_fill_index < center_stroke_index);
        assert!(center_stroke_index < center_text_index);
    }
}
