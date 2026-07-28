//! Dataset 验收阈值编辑与实时进度可视化；配置仅在当前进程内生效。

use camera_toolbox_app::{
    AutoAdmissionAssessment, AutoAdmissionDepthRange, AutoAdmissionItemVisualization,
    AutoAdmissionPnpState, AutoCaptureAcceptanceCriteria, CalibrationItemId,
};
use eframe::egui;
use egui_plot::{
    HoverPosition, Line, Plot, PlotBounds, PlotMemory, PlotPoint, PlotPoints, PlotUi, Text,
};

const DEFAULT_FIELD_COLUMNS: &str = "16";
const DEFAULT_FIELD_ROWS: &str = "9";
const DEFAULT_FIELD_TARGET_PER_CELL: &str = "1";
const DEFAULT_MIN_ADJACENT_SPACING_PX: &str = "12";
const DEFAULT_PNP_DEPTH_MIN: &str = "400";
const DEFAULT_PNP_DEPTH_MAX: &str = "2400";
const DEFAULT_PNP_DEPTH_BINS: &str = "4";
const DEFAULT_DEPTH_TARGET_PER_BIN: &str = "1";
const DEFAULT_PNP_TILT_DEADBAND_DEG: &str = "5";
const DEFAULT_PNP_TILT_MAX_DEG: &str = "65";
const DEFAULT_PNP_TILT_BINS: &str = "3";
const DEFAULT_PNP_AZIMUTH_SECTORS: &str = "8";
const DEFAULT_POSE_TARGET_PER_BIN: &str = "1";
const DEFAULT_PNP_MAX_RMSE_PX: &str = "1.5";
const DEFAULT_PNP_MAX_ERROR_PX: &str = "4";
const DEFAULT_MINIMUM_AUTO_GAIN: &str = "1";
const DEPTH_RANGE_PLOT_HEIGHT: f32 = 96.0;
const DEPTH_BIN_BASE_PLOT_HEIGHT: f32 = 56.0;
const DEPTH_RANGE_CAP_HALF_HEIGHT: f64 = 0.38;
const DEPTH_BIN_BASE_Y: f64 = 0.35;
const DEPTH_BIN_LABEL_Y: f64 = -0.35;

/// 文本编辑状态必须保留中间输入；只有完整合法值才会被工作区自动安装。
#[derive(Clone, Debug)]
pub(crate) struct DatasetAcceptanceDraft {
    pub(crate) field_columns: String,
    pub(crate) field_rows: String,
    pub(crate) field_target_per_cell: String,
    pub(crate) min_adjacent_spacing_px: String,
    pub(crate) pnp_depth_min: String,
    pub(crate) pnp_depth_max: String,
    pub(crate) pnp_depth_bins: String,
    pub(crate) depth_target_per_bin: String,
    pub(crate) pnp_tilt_deadband_deg: String,
    pub(crate) pnp_tilt_max_deg: String,
    pub(crate) pnp_tilt_bins: String,
    pub(crate) pnp_azimuth_sectors: String,
    pub(crate) pose_target_per_bin: String,
    pub(crate) pnp_max_rmse_px: String,
    pub(crate) pnp_max_error_px: String,
    pub(crate) minimum_auto_gain: String,
    pub(crate) error: Option<String>,
}

impl Default for DatasetAcceptanceDraft {
    fn default() -> Self {
        Self {
            field_columns: DEFAULT_FIELD_COLUMNS.to_owned(),
            field_rows: DEFAULT_FIELD_ROWS.to_owned(),
            field_target_per_cell: DEFAULT_FIELD_TARGET_PER_CELL.to_owned(),
            min_adjacent_spacing_px: DEFAULT_MIN_ADJACENT_SPACING_PX.to_owned(),
            pnp_depth_min: DEFAULT_PNP_DEPTH_MIN.to_owned(),
            pnp_depth_max: DEFAULT_PNP_DEPTH_MAX.to_owned(),
            pnp_depth_bins: DEFAULT_PNP_DEPTH_BINS.to_owned(),
            depth_target_per_bin: DEFAULT_DEPTH_TARGET_PER_BIN.to_owned(),
            pnp_tilt_deadband_deg: DEFAULT_PNP_TILT_DEADBAND_DEG.to_owned(),
            pnp_tilt_max_deg: DEFAULT_PNP_TILT_MAX_DEG.to_owned(),
            pnp_tilt_bins: DEFAULT_PNP_TILT_BINS.to_owned(),
            pnp_azimuth_sectors: DEFAULT_PNP_AZIMUTH_SECTORS.to_owned(),
            pose_target_per_bin: DEFAULT_POSE_TARGET_PER_BIN.to_owned(),
            pnp_max_rmse_px: DEFAULT_PNP_MAX_RMSE_PX.to_owned(),
            pnp_max_error_px: DEFAULT_PNP_MAX_ERROR_PX.to_owned(),
            minimum_auto_gain: DEFAULT_MINIMUM_AUTO_GAIN.to_owned(),
            error: None,
        }
    }
}

impl DatasetAcceptanceDraft {
    pub(crate) fn parse(&self) -> Result<AutoCaptureAcceptanceCriteria, String> {
        let field_columns = parse_usize("Field columns", &self.field_columns, 1, 32)?;
        let field_rows = parse_usize("Field rows", &self.field_rows, 1, 32)?;
        let field_capacity = field_columns
            .checked_mul(field_rows)
            .ok_or_else(|| "Field grid capacity overflows usize.".to_owned())?;
        let field_target_per_cell = parse_usize(
            "Field target per cell",
            &self.field_target_per_cell,
            1,
            10_000,
        )?;
        field_capacity
            .checked_mul(field_target_per_cell)
            .ok_or_else(|| "Field quota target overflows usize.".to_owned())?;
        let min_adjacent_spacing_px =
            parse_positive_f32("Minimum adjacent spacing", &self.min_adjacent_spacing_px)?;
        let pnp_depth_min = parse_positive_f64("PnP minimum depth", &self.pnp_depth_min)?;
        let pnp_depth_max = parse_positive_f64("PnP maximum depth", &self.pnp_depth_max)?;
        if pnp_depth_max <= pnp_depth_min {
            return Err("PnP maximum depth must be greater than minimum depth.".to_owned());
        }
        let pnp_depth_bins = parse_usize("PnP depth bins", &self.pnp_depth_bins, 1, 32)?;
        let depth_target_per_bin = parse_usize(
            "Depth target per bin",
            &self.depth_target_per_bin,
            1,
            10_000,
        )?;
        pnp_depth_bins
            .checked_mul(depth_target_per_bin)
            .ok_or_else(|| "Depth quota target overflows usize.".to_owned())?;
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
        let pose_target_per_bin =
            parse_usize("Pose target per bin", &self.pose_target_per_bin, 1, 10_000)?;
        pose_capacity
            .checked_mul(pose_target_per_bin)
            .ok_or_else(|| "Pose quota target overflows usize.".to_owned())?;
        let pnp_max_rmse_px = parse_non_negative_f64("PnP maximum RMSE", &self.pnp_max_rmse_px)?;
        let pnp_max_error_px =
            parse_non_negative_f64("PnP maximum reprojection error", &self.pnp_max_error_px)?;
        if pnp_max_error_px < pnp_max_rmse_px {
            return Err(
                "PnP maximum reprojection error must be at least the RMSE limit.".to_owned(),
            );
        }
        let minimum_auto_gain = parse_usize(
            "Minimum automatic Gain",
            &self.minimum_auto_gain,
            1,
            1_000_000,
        )?;
        Ok(AutoCaptureAcceptanceCriteria {
            field_columns,
            field_rows,
            field_target_per_cell,
            min_adjacent_spacing_px,
            pnp_depth_min,
            pnp_depth_max,
            pnp_depth_bins,
            depth_target_per_bin,
            pnp_tilt_deadband_deg,
            pnp_tilt_max_deg,
            pnp_tilt_bins,
            pnp_azimuth_sectors,
            pose_target_per_bin,
            pnp_max_rmse_px,
            pnp_max_error_px,
            minimum_auto_gain,
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
    pub(crate) selected_item: Option<CalibrationItemId>,
    pub(crate) occupied_field_cells: usize,
    pub(crate) required_field_cells: usize,
    pub(crate) field_quota_filled: usize,
    pub(crate) required_field_quota: usize,
    pub(crate) field_counts: Vec<usize>,
    pub(crate) field_columns: usize,
    pub(crate) field_rows: usize,
    pub(crate) depth_bin_counts: Vec<usize>,
    pub(crate) depth_ranges: Vec<AutoAdmissionDepthRange>,
    pub(crate) item_visualizations: Vec<AutoAdmissionItemVisualization>,
    pub(crate) occupied_depth_bins: usize,
    pub(crate) required_depth_bins: usize,
    pub(crate) depth_quota_filled: usize,
    pub(crate) required_depth_quota: usize,
    pub(crate) pose_bin_counts: Vec<usize>,
    pub(crate) occupied_pose_bins: usize,
    pub(crate) required_pose_bins: usize,
    pub(crate) pose_quota_filled: usize,
    pub(crate) required_pose_quota: usize,
    pub(crate) collection_target_met: bool,
    pub(crate) field_gain: usize,
    pub(crate) depth_gain: usize,
    pub(crate) pose_gain: usize,
    pub(crate) score: usize,
}

impl DatasetAcceptanceProgress {
    pub(crate) fn from_assessment(assessment: &AutoAdmissionAssessment) -> Self {
        Self {
            active_criteria: assessment.active_criteria.clone(),
            selected_item: None,
            occupied_field_cells: assessment.field_cells,
            required_field_cells: assessment.required_field_cells,
            field_quota_filled: assessment.field_quota_filled,
            required_field_quota: assessment.required_field_quota,
            field_counts: assessment.field_counts.clone(),
            field_columns: assessment.field_columns,
            field_rows: assessment.field_rows,
            depth_bin_counts: assessment.depth_bin_counts.clone(),
            depth_ranges: assessment.depth_ranges.clone(),
            item_visualizations: assessment.item_visualizations.clone(),
            occupied_depth_bins: assessment.depth_bins,
            required_depth_bins: assessment.required_depth_bins,
            depth_quota_filled: assessment.depth_quota_filled,
            required_depth_quota: assessment.required_depth_quota,
            pose_bin_counts: assessment.pose_bin_counts.clone(),
            occupied_pose_bins: assessment.pose_bins,
            required_pose_bins: assessment.required_pose_bins,
            pose_quota_filled: assessment.pose_quota_filled,
            required_pose_quota: assessment.required_pose_quota,
            collection_target_met: assessment.collection_target_met,
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
            required_field_cells: criteria.field_columns.saturating_mul(criteria.field_rows),
            required_field_quota: criteria
                .field_columns
                .saturating_mul(criteria.field_rows)
                .saturating_mul(criteria.field_target_per_cell),
            field_counts: vec![0; criteria.field_columns.saturating_mul(criteria.field_rows)],
            field_columns: criteria.field_columns,
            field_rows: criteria.field_rows,
            depth_bin_counts: vec![0; criteria.pnp_depth_bins],
            required_depth_bins: criteria.pnp_depth_bins,
            required_depth_quota: criteria
                .pnp_depth_bins
                .saturating_mul(criteria.depth_target_per_bin),
            pose_bin_counts: vec![0; pose_capacity],
            required_pose_bins: pose_capacity,
            required_pose_quota: pose_capacity.saturating_mul(criteria.pose_target_per_bin),
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
    pub(crate) selected_item: Option<CalibrationItemId>,
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
    let mut selected_depth_item = None;
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
                            "Field quota {}/{} · Depth quota {}/{} · Pose quota {}/{} · Score {}",
                            progress.field_quota_filled,
                            progress.required_field_quota,
                            progress.depth_quota_filled,
                            progress.required_depth_quota,
                            progress.pose_quota_filled,
                            progress.required_pose_quota,
                            progress.score,
                        ));
                        ui.monospace(format!(
                            "Δ Field {} · Depth {} · Pose {}",
                            progress.field_gain,
                            progress.depth_gain,
                            progress.pose_gain,
                        ));
                    });


                    ui.group(|ui| {
                        ui.strong("Field coverage");
                        metric_row(
                            ui,
                            "Field quota",
                            progress.field_quota_filled,
                            progress.required_field_quota,
                        );
                        ui.weak(format!(
                            "Occupied field cells: {} / {}",
                            progress.occupied_field_cells, progress.required_field_cells
                        ));
                        render_field_grid(ui, progress);
                        egui::Grid::new("dataset_acceptance_field_editor")
                            .num_columns(2)
                            .spacing([8.0, 4.0])
                            .show(ui, |ui| {
                                acceptance_text_row(ui, "Field columns", &mut draft.field_columns, &mut changed, &mut editing);
                                acceptance_text_row(ui, "Field rows", &mut draft.field_rows, &mut changed, &mut editing);
                                acceptance_text_row(
                                    ui,
                                    "Field target / cell",
                                    &mut draft.field_target_per_cell,
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
                            "Depth quota",
                            progress.depth_quota_filled,
                            progress.required_depth_quota,
                        );
                        ui.weak(format!(
                            "Occupied depth bins: {} / {}",
                            progress.occupied_depth_bins, progress.required_depth_bins
                        ));
                        if let Some(criteria) = progress.active_criteria.as_ref() {
                            selected_depth_item = render_depth_coverage(ui, progress, criteria);
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
                                    "Depth target / bin",
                                    &mut draft.depth_target_per_bin,
                                    &mut changed,
                                    &mut editing,
                                );
                            });
                    });

                    ui.group(|ui| {
                        ui.strong("Pose coverage");
                        metric_row(
                            ui,
                            "Pose quota",
                            progress.pose_quota_filled,
                            progress.required_pose_quota,
                        );
                        ui.weak(format!(
                            "Occupied pose bins: {} / {}",
                            progress.occupied_pose_bins, progress.required_pose_bins
                        ));
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
                                    "Pose target / bin",
                                    &mut draft.pose_target_per_bin,
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
                                acceptance_text_row(
                                    ui,
                                    "Minimum auto Gain",
                                    &mut draft.minimum_auto_gain,
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
        selected_item: selected_depth_item,
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

fn coverage_color(count: usize, target: usize) -> egui::Color32 {
    if target == 0 {
        return egui::Color32::from_gray(42);
    }
    let ratio = (count as f32 / target as f32).clamp(0.0, 1.0);
    let red = [130.0_f32, 54.0, 54.0];
    let green = [58.0_f32, 174.0, 108.0];
    egui::Color32::from_rgb(
        (red[0] + (green[0] - red[0]) * ratio) as u8,
        (red[1] + (green[1] - red[1]) * ratio) as u8,
        (red[2] + (green[2] - red[2]) * ratio) as u8,
    )
}

fn coverage_text_color(count: usize, target: usize) -> egui::Color32 {
    if target != 0 && count as f32 / target as f32 > 0.58 {
        egui::Color32::from_rgb(12, 32, 45)
    } else {
        egui::Color32::WHITE
    }
}

fn selected_item_visualization(
    progress: &DatasetAcceptanceProgress,
) -> Option<&AutoAdmissionItemVisualization> {
    let selected = progress.selected_item?;
    progress
        .item_visualizations
        .iter()
        .find(|item| item.item_id == selected)
}

fn selected_field_cell(progress: &DatasetAcceptanceProgress, cell: usize) -> bool {
    selected_item_visualization(progress).is_some_and(|item| item.field_cells.contains(&cell))
}

fn selected_pose_bin(progress: &DatasetAcceptanceProgress) -> Option<usize> {
    selected_item_visualization(progress).and_then(|item| item.pose_bin)
}

fn selected_highlight_stroke() -> egui::Stroke {
    egui::Stroke::new(2.0, egui::Color32::from_rgb(80, 170, 255))
}

fn render_field_grid(ui: &mut egui::Ui, progress: &DatasetAcceptanceProgress) {
    ui.label(
        "Per-cell chessboard-corner count; green means the configured per-cell target is reached.",
    );
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
    let target = progress
        .active_criteria
        .as_ref()
        .map_or(1, |criteria| criteria.field_target_per_cell);
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
            painter.rect_filled(cell_rect, 1.5, coverage_color(count, target));
            if selected_field_cell(progress, cell) {
                painter.rect_stroke(
                    cell_rect.expand(0.5),
                    2.0,
                    selected_highlight_stroke(),
                    egui::StrokeKind::Inside,
                );
            }
            painter.text(
                cell_rect.center(),
                egui::Align2::CENTER_CENTER,
                count.to_string(),
                label_font.clone(),
                coverage_text_color(count, target),
            );
        }
    }
    response.on_hover_text(
        "Each cell displays the number of enabled Found chessboard corners in that image region.",
    );
    ui.weak(format!("Legend: red 0 → green {target}+ corners per cell"));
}

fn render_depth_coverage(
    ui: &mut egui::Ui,
    progress: &DatasetAcceptanceProgress,
    criteria: &AutoCaptureAcceptanceCriteria,
) -> Option<CalibrationItemId> {
    ui.label("Depth timeline: upper plot shows every image board-depth span; lower base keeps the configured corner-depth bins.");
    let bin_count = criteria.pnp_depth_bins;
    if bin_count == 0 {
        return None;
    }
    let axis = depth_timeline_axis(progress, criteria);
    let x_link = ui.id().with("dataset_acceptance_depth_timeline_x");
    let selected = render_depth_range_plot(ui, progress, &axis, x_link);
    render_depth_bin_base_plot(ui, progress, criteria, &axis, x_link);
    ui.weak(format!(
        "Upper: white capped ranges are per-image min/max board depths; blue is the selected Dataset image. Lower: red 0 → green {target}+ corner depths per configured bin.",
        target = criteria.depth_target_per_bin
    ));
    selected
}

#[derive(Clone, Copy, Debug)]
struct DepthTimelineAxis {
    min: f64,
    max: f64,
}

fn depth_timeline_axis(
    progress: &DatasetAcceptanceProgress,
    criteria: &AutoCaptureAcceptanceCriteria,
) -> DepthTimelineAxis {
    let mut minimum = criteria.pnp_depth_min;
    let mut maximum = criteria.pnp_depth_max;
    for range in &progress.depth_ranges {
        if range.minimum_depth.is_finite() && range.maximum_depth.is_finite() {
            minimum = minimum.min(range.minimum_depth.min(range.maximum_depth));
            maximum = maximum.max(range.minimum_depth.max(range.maximum_depth));
        }
    }
    if !minimum.is_finite() || !maximum.is_finite() || minimum >= maximum {
        minimum = criteria.pnp_depth_min;
        maximum = criteria.pnp_depth_max.max(criteria.pnp_depth_min + 1.0);
    }
    DepthTimelineAxis {
        min: minimum,
        max: maximum,
    }
}

fn render_depth_range_plot(
    ui: &mut egui::Ui,
    progress: &DatasetAcceptanceProgress,
    axis: &DepthTimelineAxis,
    x_link: egui::Id,
) -> Option<CalibrationItemId> {
    let plot_id = ui.make_persistent_id(egui::Id::new("dataset_acceptance_depth_ranges"));
    let state_id = plot_id.with("depth_range_view_state");
    let mut state = ui.ctx().data_mut(|data| {
        data.get_temp::<DepthRangePlotState>(state_id)
            .unwrap_or_default()
    });
    let y_bounds = depth_timeline_y_bounds(progress);
    let plot = Plot::new("dataset_acceptance_depth_ranges")
        .id(plot_id)
        .link_axis(x_link, [true, false])
        .height(DEPTH_RANGE_PLOT_HEIGHT)
        .allow_zoom([false, true])
        .allow_drag([false, true])
        .allow_scroll(false)
        .allow_axis_zoom_drag([false, true])
        .allow_boxed_zoom(false)
        .allow_double_click_reset(true)
        .auto_bounds([true, false])
        .default_y_bounds(y_bounds.min, y_bounds.max)
        .include_x(axis.min)
        .include_x(axis.max)
        .invert_y(true)
        .show_axes([false, false])
        .show_grid([true, false])
        .show_crosshair(false)
        .show_y(false)
        .set_margin_fraction(egui::vec2(0.02, 0.02))
        .label_formatter(depth_plot_label);
    let response = plot.show(ui, |plot_ui| {
        apply_depth_timeline_y_bounds(plot_ui, y_bounds, state.user_y_bounds);
        render_depth_range_items(plot_ui, progress, axis)
    });
    if response.response.double_clicked() {
        state.user_y_bounds = false;
    } else if depth_timeline_y_interacted(ui, &response.response)
        && (response.transform.bounds().height() < (y_bounds.max - y_bounds.min) - 1.0e-6
            || state.user_y_bounds)
    {
        state.user_y_bounds = true;
    }
    clamp_depth_range_plot_memory(ui.ctx(), plot_id, y_bounds, state.user_y_bounds);
    ui.ctx().data_mut(|data| data.insert_temp(state_id, state));
    response.inner
}

#[derive(Clone, Copy, Debug, Default)]
struct DepthRangePlotState {
    user_y_bounds: bool,
}

#[derive(Clone, Copy, Debug)]
struct DepthTimelineYBounds {
    min: f64,
    max: f64,
}

fn depth_timeline_y_bounds(progress: &DatasetAcceptanceProgress) -> DepthTimelineYBounds {
    let rows = progress.depth_ranges.len().max(1) as f64;
    DepthTimelineYBounds {
        min: -0.5,
        max: rows - 0.5,
    }
}

fn apply_depth_timeline_y_bounds(
    plot_ui: &mut PlotUi<'_>,
    full: DepthTimelineYBounds,
    user_y_bounds: bool,
) {
    let (minimum, maximum) = if user_y_bounds {
        let bounds = plot_ui.plot_bounds();
        clamp_depth_timeline_y_bounds(bounds.min()[1], bounds.max()[1], full)
    } else {
        (full.min, full.max)
    };
    plot_ui.set_plot_bounds_y(minimum..=maximum);
}

fn depth_timeline_y_interacted(ui: &egui::Ui, response: &egui::Response) -> bool {
    let y_zoomed = response.contains_pointer()
        && ui.input(|input| (input.zoom_delta_2d().y - 1.0).abs() > f32::EPSILON);
    response.dragged_by(egui::PointerButton::Primary) || y_zoomed
}

fn clamp_depth_range_plot_memory(
    context: &egui::Context,
    plot_id: egui::Id,
    full: DepthTimelineYBounds,
    user_y_bounds: bool,
) {
    let Some(mut memory) = PlotMemory::load(context, plot_id) else {
        return;
    };
    let current = *memory.bounds();
    let (minimum, maximum) = if user_y_bounds {
        clamp_depth_timeline_y_bounds(current.min()[1], current.max()[1], full)
    } else {
        (full.min, full.max)
    };
    if (current.min()[1] - minimum).abs() <= f64::EPSILON
        && (current.max()[1] - maximum).abs() <= f64::EPSILON
    {
        return;
    }
    memory.set_bounds(PlotBounds::from_min_max(
        [current.min()[0], minimum],
        [current.max()[0], maximum],
    ));
    memory.store(context, plot_id);
}

fn clamp_depth_timeline_y_bounds(
    minimum: f64,
    maximum: f64,
    full: DepthTimelineYBounds,
) -> (f64, f64) {
    let full_height = (full.max - full.min).max(1.0e-6);
    let height = (maximum - minimum).max(1.0e-6);
    if !minimum.is_finite() || !maximum.is_finite() || height >= full_height {
        return (full.min, full.max);
    }
    if minimum < full.min {
        return (full.min, full.min + height);
    }
    if maximum > full.max {
        return (full.max - height, full.max);
    }
    (minimum, maximum)
}

fn render_depth_range_items(
    plot_ui: &mut PlotUi<'_>,
    progress: &DatasetAcceptanceProgress,
    axis: &DepthTimelineAxis,
) -> Option<CalibrationItemId> {
    for (row, range) in progress.depth_ranges.iter().enumerate() {
        if progress.selected_item == Some(range.item_id) {
            continue;
        }
        draw_depth_range(plot_ui, range, row, false);
    }
    for (row, range) in progress.depth_ranges.iter().enumerate() {
        if progress.selected_item == Some(range.item_id) {
            draw_depth_range(plot_ui, range, row, true);
        }
    }
    if plot_ui.response().clicked() {
        plot_ui
            .pointer_coordinate()
            .and_then(|point| hit_depth_range(progress, axis, point))
    } else {
        None
    }
}

fn draw_depth_range(
    plot_ui: &mut PlotUi<'_>,
    range: &AutoAdmissionDepthRange,
    row: usize,
    selected: bool,
) {
    let y = row as f64;
    let (minimum, maximum) = ordered_depth_range(range.minimum_depth, range.maximum_depth);
    let color = if selected {
        egui::Color32::from_rgb(80, 170, 255)
    } else {
        egui::Color32::WHITE
    };
    let width = if selected { 2.6 } else { 1.6 };
    let tooltip = depth_range_tooltip(range, row);
    plot_ui.line(
        Line::new(
            tooltip.clone(),
            PlotPoints::from(vec![[minimum, y], [maximum, y]]),
        )
        .id(egui::Id::new((
            "dataset_depth_range_body",
            range.item_id.get(),
        )))
        .color(color)
        .width(width),
    );
    for (side, x) in [("min", minimum), ("max", maximum)] {
        plot_ui.line(
            Line::new(
                tooltip.clone(),
                PlotPoints::from(vec![
                    [x, y - DEPTH_RANGE_CAP_HALF_HEIGHT],
                    [x, y + DEPTH_RANGE_CAP_HALF_HEIGHT],
                ]),
            )
            .id(egui::Id::new((
                "dataset_depth_range_cap",
                range.item_id.get(),
                side,
            )))
            .color(color)
            .width(width),
        );
    }
}

fn render_depth_bin_base_plot(
    ui: &mut egui::Ui,
    progress: &DatasetAcceptanceProgress,
    criteria: &AutoCaptureAcceptanceCriteria,
    axis: &DepthTimelineAxis,
    x_link: egui::Id,
) {
    let plot = Plot::new("dataset_acceptance_depth_bin_base")
        .link_axis(x_link, [true, false])
        .height(DEPTH_BIN_BASE_PLOT_HEIGHT)
        .allow_zoom(false)
        .allow_drag(false)
        .allow_scroll(false)
        .allow_axis_zoom_drag(false)
        .allow_boxed_zoom(false)
        .allow_double_click_reset(false)
        .auto_bounds([true, false])
        .default_y_bounds(-1.0, 1.0)
        .include_x(axis.min)
        .include_x(axis.max)
        .show_axes([true, false])
        .show_grid([true, false])
        .show_crosshair(false)
        .show_y(false)
        .x_axis_formatter(|mark, _| format!("{:.0}", mark.value))
        .set_margin_fraction(egui::vec2(0.02, 0.02))
        .label_formatter(depth_plot_label);
    plot.show(ui, |plot_ui| {
        let target = criteria.depth_target_per_bin;
        for index in 0..criteria.pnp_depth_bins {
            let (lower, upper) = depth_interval_bounds(criteria, index);
            let count = progress.depth_bin_counts.get(index).copied().unwrap_or(0);
            let label = depth_interval_label(criteria, index);
            let tooltip =
                format!("Depth {label}: {count}/{target} compatible chessboard-corner depths.");
            let color = coverage_color(count, target);
            plot_ui.line(
                Line::new(
                    tooltip,
                    PlotPoints::from(vec![[lower, DEPTH_BIN_BASE_Y], [upper, DEPTH_BIN_BASE_Y]]),
                )
                .id(egui::Id::new(("dataset_depth_bin_segment", index)))
                .color(color)
                .width(4.0),
            );
            plot_ui.text(
                Text::new(
                    format!("dataset_depth_bin_count_{index}"),
                    PlotPoint::new((lower + upper) * 0.5, DEPTH_BIN_LABEL_Y),
                    format!("{count} corners"),
                )
                .color(egui::Color32::WHITE)
                .allow_hover(false),
            );
        }
        for index in 0..=criteria.pnp_depth_bins {
            let boundary = if index == criteria.pnp_depth_bins {
                criteria.pnp_depth_max
            } else {
                depth_interval_bounds(criteria, index).0
            };
            plot_ui.line(
                Line::new(
                    "Configured depth-bin boundary",
                    PlotPoints::from(vec![[boundary, 0.1], [boundary, 0.62]]),
                )
                .id(egui::Id::new(("dataset_depth_bin_boundary", index)))
                .color(egui::Color32::from_rgb(236, 72, 45))
                .width(1.8)
                .allow_hover(false),
            );
        }
    });
}

fn depth_plot_label(hover: &HoverPosition<'_>) -> Option<String> {
    match hover {
        HoverPosition::NearDataPoint { plot_name, .. } if !plot_name.is_empty() => {
            Some((*plot_name).to_owned())
        }
        HoverPosition::Elsewhere { .. } | HoverPosition::NearDataPoint { .. } => None,
    }
}

fn hit_depth_range(
    progress: &DatasetAcceptanceProgress,
    axis: &DepthTimelineAxis,
    point: PlotPoint,
) -> Option<CalibrationItemId> {
    let x_tolerance = ((axis.max - axis.min).abs() * 0.006).max(1.0e-6);
    progress
        .depth_ranges
        .iter()
        .enumerate()
        .filter_map(|(row, range)| {
            let y_distance = (point.y - row as f64).abs();
            if y_distance > 0.45 {
                return None;
            }
            let (minimum, maximum) = ordered_depth_range(range.minimum_depth, range.maximum_depth);
            if point.x < minimum - x_tolerance || point.x > maximum + x_tolerance {
                return None;
            }
            Some((y_distance, range.item_id))
        })
        .min_by(|left, right| left.0.total_cmp(&right.0))
        .map(|(_, item_id)| item_id)
}

fn depth_range_tooltip(range: &AutoAdmissionDepthRange, row: usize) -> String {
    let (minimum, maximum) = ordered_depth_range(range.minimum_depth, range.maximum_depth);
    format!(
        "Dataset item #{} · row {}\nDepth span: {:.3} .. {:.3}\nWidth: {:.3}\nPnP state: {}\nRMSE: {:.4} px · Max error: {:.4} px",
        range.item_id.get(),
        row + 1,
        minimum,
        maximum,
        maximum - minimum,
        depth_range_state_label(&range.pnp_state),
        range.reprojection_rmse,
        range.max_reprojection_error,
    )
}

fn depth_range_state_label(state: &AutoAdmissionPnpState) -> &'static str {
    match state {
        AutoAdmissionPnpState::Valid => "valid for Depth/Pose quota",
        AutoAdmissionPnpState::MissingBinding => "missing K/D binding",
        AutoAdmissionPnpState::MissingObservation => "missing PnP observation",
        AutoAdmissionPnpState::BindingGap(_) => "binding mismatch",
        AutoAdmissionPnpState::DepthGap(_) => "depth gate failed",
        AutoAdmissionPnpState::PoseGap(_) => "pose gate failed",
        AutoAdmissionPnpState::RmseReprojectionGap(_) => "RMSE gate failed",
        AutoAdmissionPnpState::MaxReprojectionGap(_) => "max reprojection gate failed",
        AutoAdmissionPnpState::Invalid(_) => "invalid PnP evidence",
    }
}

fn ordered_depth_range(first: f64, second: f64) -> (f64, f64) {
    if first <= second {
        (first, second)
    } else {
        (second, first)
    }
}

fn depth_interval_bounds(criteria: &AutoCaptureAcceptanceCriteria, index: usize) -> (f64, f64) {
    let width = (criteria.pnp_depth_max - criteria.pnp_depth_min) / criteria.pnp_depth_bins as f64;
    let lower = criteria.pnp_depth_min + width * index as f64;
    let upper = if index + 1 == criteria.pnp_depth_bins {
        criteria.pnp_depth_max
    } else {
        criteria.pnp_depth_min + width * (index + 1) as f64
    };
    (lower, upper)
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
    let target = criteria.pose_target_per_bin;
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
                coverage_color(count, target),
            );
        }
    }
    painter.add(egui::Shape::mesh(fill_mesh));

    let border = egui::Stroke::new(0.5, egui::Color32::from_gray(24));
    let selected_pose_bin = selected_pose_bin(progress);
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
            let sector_stroke = if selected_pose_bin == Some(index) {
                selected_highlight_stroke()
            } else {
                border
            };
            paint_annular_sector_boundaries(
                &painter,
                center,
                inner_radius,
                ring_outer,
                start,
                end,
                sector_stroke,
            );
            let label_radius = (inner_radius + ring_outer) * 0.5;
            painter.text(
                polar_point(center, label_radius, (start + end) * 0.5),
                egui::Align2::CENTER_CENTER,
                count.to_string(),
                egui::FontId::proportional(8.0),
                coverage_text_color(count, target),
            );
        }
    }
    // 环带内弦会略微进入理论内圆；只有存在 deadband 时才绘制中心 bin 作为遮罩。
    if center_enabled {
        painter.circle_filled(center, center_radius, coverage_color(center_count, target));
        painter.circle_stroke(center, center_radius, border);
        if selected_pose_bin == Some(0) {
            painter.circle_stroke(center, center_radius, selected_highlight_stroke());
        }
        painter.text(
            center,
            egui::Align2::CENTER_CENTER,
            center_count.to_string(),
            egui::FontId::proportional(10.0),
            coverage_text_color(center_count, target),
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
    ui.weak(format!(
        "Each sector shows its count; red 0 → green {target}+ views per pose bin."
    ));
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
    let target = criteria.pose_target_per_bin;
    egui::Grid::new("dataset_acceptance_pose_grid")
        .num_columns(8)
        .spacing([3.0, 3.0])
        .show(ui, |ui| {
            for index in 0..region_count {
                let count = progress.pose_bin_counts.get(index).copied().unwrap_or(0);
                let stroke = if selected_pose_bin(progress) == Some(index) {
                    selected_highlight_stroke()
                } else {
                    egui::Stroke::new(0.0, egui::Color32::TRANSPARENT)
                };
                ui.add(
                    egui::Button::new(format!("#{index} {count}"))
                        .fill(coverage_color(count, target))
                        .stroke(stroke),
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

#[cfg(test)]
mod tests {
    use super::*;
    use camera_toolbox_app::{
        AddCalibrationItemOutcome, CalibrationInputRevision, CalibrationSession, FileRef,
        FileSourceId, FileVersion, SourcePath,
    };
    use camera_toolbox_core::BoardSpec;

    fn test_item_id(name: &str) -> CalibrationItemId {
        let mut session = CalibrationSession::new(BoardSpec::new(2, 2, 1.0).unwrap());
        let AddCalibrationItemOutcome::Added(id) = session.add_or_refresh(
            FileRef::new(
                FileSourceId::new("dataset-acceptance-test").unwrap(),
                SourcePath::new(name).unwrap(),
            ),
            CalibrationInputRevision::File(FileVersion {
                size: 1,
                modified_millis: None,
            }),
            name.to_owned(),
        ) else {
            panic!("expected added Dataset item");
        };
        id
    }

    fn depth_smoke_progress() -> (
        AutoCaptureAcceptanceCriteria,
        DatasetAcceptanceProgress,
        Vec<CalibrationItemId>,
    ) {
        let mut criteria = DatasetAcceptanceDraft::default().parse().unwrap();
        criteria.pnp_depth_min = 400.0;
        criteria.pnp_depth_max = 700.0;
        criteria.pnp_depth_bins = 3;
        criteria.depth_target_per_bin = 2;

        let mut progress = DatasetAcceptanceProgress::empty(&criteria);
        progress.depth_bin_counts = vec![0, 2, 5];
        let items = [
            "range-a.png",
            "range-b.png",
            "range-c.png",
            "range-d.png",
            "range-e.png",
        ]
        .into_iter()
        .map(test_item_id)
        .collect::<Vec<_>>();
        for (item_id, (minimum_depth, maximum_depth)) in items.iter().copied().zip([
            (350.0, 420.0),
            (390.0, 520.0),
            (440.0, 610.0),
            (610.0, 760.0),
            (330.0, 790.0),
        ]) {
            progress.depth_ranges.push(AutoAdmissionDepthRange {
                item_id,
                minimum_depth,
                maximum_depth,
                pnp_state: AutoAdmissionPnpState::Valid,
                reprojection_rmse: 0.2,
                max_reprojection_error: 0.4,
            });
        }
        progress.selected_item = Some(items[2]);
        (criteria, progress, items)
    }

    fn depth_smoke_input(events: Vec<egui::Event>) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(720.0, 300.0),
            )),
            events,
            ..Default::default()
        }
    }

    fn pointer_button(position: egui::Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos: position,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        }
    }

    fn render_depth_smoke_frame(
        context: &egui::Context,
        progress: &DatasetAcceptanceProgress,
        criteria: &AutoCaptureAcceptanceCriteria,
        events: Vec<egui::Event>,
    ) -> (Option<CalibrationItemId>, PlotMemory, PlotMemory) {
        let mut selected = None;
        let mut range_plot_id = None;
        let mut base_plot_id = None;
        let _ = context.run_ui(depth_smoke_input(events), |ui| {
            ui.set_width(640.0);
            range_plot_id =
                Some(ui.make_persistent_id(egui::Id::new("dataset_acceptance_depth_ranges")));
            base_plot_id =
                Some(ui.make_persistent_id(egui::Id::new("dataset_acceptance_depth_bin_base")));
            selected = render_depth_coverage(ui, progress, criteria);
        });
        let range_plot = PlotMemory::load(
            context,
            range_plot_id.expect("Depth range plot id captured"),
        )
        .expect("Depth range plot memory");
        let base_plot = PlotMemory::load(
            context,
            base_plot_id.expect("Depth bin base plot id captured"),
        )
        .expect("Depth bin base plot memory");
        (selected, range_plot, base_plot)
    }

    fn assert_x_bounds_aligned(range_plot: &PlotMemory, base_plot: &PlotMemory) {
        let range_bounds = range_plot.bounds();
        let base_bounds = base_plot.bounds();
        assert!(
            (range_bounds.min()[0] - base_bounds.min()[0]).abs() < 1.0e-6,
            "range/base x-min diverged: {:?} vs {:?}",
            range_bounds,
            base_bounds
        );
        assert!(
            (range_bounds.max()[0] - base_bounds.max()[0]).abs() < 1.0e-6,
            "range/base x-max diverged: {:?} vs {:?}",
            range_bounds,
            base_bounds
        );
    }

    fn assert_y_bounds(plot: &PlotMemory, expected_minimum: f64, expected_maximum: f64) {
        let bounds = plot.bounds();
        assert!(
            (bounds.min()[1] - expected_minimum).abs() < 1.0e-6,
            "unexpected y-min: {:?}",
            bounds
        );
        assert!(
            (bounds.max()[1] - expected_maximum).abs() < 1.0e-6,
            "unexpected y-max: {:?}",
            bounds
        );
    }

    fn assert_y_bounds_inside(plot: &PlotMemory, full: DepthTimelineYBounds) {
        let bounds = plot.bounds();
        assert!(
            bounds.min()[1] >= full.min - 1.0e-6,
            "y-min escaped full range: {:?} not in {:?}",
            bounds,
            full
        );
        assert!(
            bounds.max()[1] <= full.max + 1.0e-6,
            "y-max escaped full range: {:?} not in {:?}",
            bounds,
            full
        );
    }

    #[test]
    fn acceptance_draft_parses_runtime_pnp_thresholds() {
        let criteria = DatasetAcceptanceDraft::default().parse().unwrap();
        assert_eq!(criteria.field_target_per_cell, 1);
        assert_eq!(criteria.minimum_auto_gain, 1);

        assert_eq!(
            (criteria.pnp_depth_min, criteria.pnp_depth_max),
            (400.0, 2400.0)
        );
        assert_eq!(
            (criteria.pnp_depth_bins, criteria.depth_target_per_bin),
            (4, 1)
        );
        assert_eq!(criteria.pose_target_per_bin, 1);
    }

    #[test]
    fn acceptance_draft_rejects_invalid_grid_and_pnp_ranges() {
        let mut draft = DatasetAcceptanceDraft {
            field_columns: "33".to_owned(),
            ..DatasetAcceptanceDraft::default()
        };
        assert!(draft.parse().unwrap_err().contains("1..=32"));

        draft.field_columns = "16".to_owned();
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
        draft.field_target_per_cell = "2".to_owned();
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
    fn depth_timeline_axis_and_hit_testing_include_ranges_outside_configured_base() {
        let item_id = test_item_id("range-a.png");
        let mut criteria = DatasetAcceptanceDraft::default().parse().unwrap();
        criteria.pnp_depth_min = 400.0;
        criteria.pnp_depth_max = 700.0;
        let mut progress = DatasetAcceptanceProgress::empty(&criteria);
        progress.depth_ranges.push(AutoAdmissionDepthRange {
            item_id,
            minimum_depth: 350.0,
            maximum_depth: 760.0,
            pnp_state: AutoAdmissionPnpState::Valid,
            reprojection_rmse: 0.2,
            max_reprojection_error: 0.4,
        });

        let axis = depth_timeline_axis(&progress, &criteria);
        assert_eq!(axis.min, 350.0);
        assert_eq!(axis.max, 760.0);
        assert_eq!(
            hit_depth_range(&progress, &axis, PlotPoint::new(500.0, 0.0)),
            Some(item_id)
        );
        assert_eq!(
            hit_depth_range(&progress, &axis, PlotPoint::new(500.0, 1.0)),
            None
        );
    }

    #[test]
    fn depth_timeline_default_y_bounds_follow_dataset_growth_until_user_zoom() {
        let context = egui::Context::default();
        let (criteria, mut progress, _) = depth_smoke_progress();
        let all_ranges = progress.depth_ranges.clone();
        progress.depth_ranges.truncate(2);

        let (_, two_row_plot, _) =
            render_depth_smoke_frame(&context, &progress, &criteria, Vec::new());
        assert_y_bounds(&two_row_plot, -0.5, 1.5);

        progress.depth_ranges = all_ranges;
        let (_, grown_plot, _) =
            render_depth_smoke_frame(&context, &progress, &criteria, Vec::new());
        assert_y_bounds(&grown_plot, -0.5, 4.5);

        let zoom_position = grown_plot
            .transform()
            .position_from_point(&PlotPoint::new(560.0, 1.5));
        let (_, zoomed_plot, _) = render_depth_smoke_frame(
            &context,
            &progress,
            &criteria,
            vec![
                egui::Event::PointerMoved(zoom_position),
                egui::Event::Zoom(1.2),
            ],
        );

        let new_item = test_item_id("range-f.png");
        progress.depth_ranges.push(AutoAdmissionDepthRange {
            item_id: new_item,
            minimum_depth: 500.0,
            maximum_depth: 820.0,
            pnp_state: AutoAdmissionPnpState::Valid,
            reprojection_rmse: 0.2,
            max_reprojection_error: 0.4,
        });
        let (_, manual_growth_plot, _) =
            render_depth_smoke_frame(&context, &progress, &criteria, Vec::new());
        let manual_full = depth_timeline_y_bounds(&progress);
        assert!(manual_growth_plot.bounds().height() < manual_full.max - manual_full.min);
        assert!(manual_growth_plot.bounds().height() <= zoomed_plot.bounds().height() + 1.0e-6);
    }

    #[test]
    fn depth_timeline_smoke_drags_zooms_clicks_and_keeps_x_linked() {
        let context = egui::Context::default();
        let (criteria, progress, items) = depth_smoke_progress();
        assert!(progress.depth_ranges.len() > 4);
        let axis = depth_timeline_axis(&progress, &criteria);

        let (selected, initial_range_plot, initial_base_plot) =
            render_depth_smoke_frame(&context, &progress, &criteria, Vec::new());
        assert_eq!(selected, None);
        assert_x_bounds_aligned(&initial_range_plot, &initial_base_plot);
        assert!(initial_range_plot.bounds().min()[0] <= axis.min + 1.0e-6);
        assert!(initial_range_plot.bounds().max()[0] >= axis.max - 1.0e-6);
        assert_y_bounds(&initial_range_plot, -0.5, 4.5);
        assert!(DEPTH_RANGE_CAP_HALF_HEIGHT >= 0.38);

        let zoom_position = initial_range_plot
            .transform()
            .position_from_point(&PlotPoint::new(560.0, 1.5));
        let (_, zoomed_range_plot, zoomed_base_plot) = render_depth_smoke_frame(
            &context,
            &progress,
            &criteria,
            vec![
                egui::Event::PointerMoved(zoom_position),
                egui::Event::Zoom(1.2),
            ],
        );
        assert_x_bounds_aligned(&zoomed_range_plot, &zoomed_base_plot);
        assert!(
            (zoomed_range_plot.bounds().min()[0] - initial_range_plot.bounds().min()[0]).abs()
                < 1.0e-6
        );
        assert!(
            (zoomed_range_plot.bounds().max()[0] - initial_range_plot.bounds().max()[0]).abs()
                < 1.0e-6
        );
        assert!(zoomed_range_plot.bounds().height() < initial_range_plot.bounds().height());

        let drag_start = zoomed_range_plot
            .transform()
            .position_from_point(&PlotPoint::new(560.0, 1.5));
        let drag_end = drag_start + egui::vec2(0.0, -32.0);
        let _ = render_depth_smoke_frame(
            &context,
            &progress,
            &criteria,
            vec![
                egui::Event::PointerMoved(drag_start),
                pointer_button(drag_start, true),
            ],
        );
        let (_, dragged_range_plot, dragged_base_plot) = render_depth_smoke_frame(
            &context,
            &progress,
            &criteria,
            vec![egui::Event::PointerMoved(drag_end)],
        );
        assert_x_bounds_aligned(&dragged_range_plot, &dragged_base_plot);
        assert!(
            (dragged_range_plot.bounds().min()[0] - zoomed_range_plot.bounds().min()[0]).abs()
                < 1.0e-6
        );
        assert!(
            (dragged_range_plot.bounds().max()[0] - zoomed_range_plot.bounds().max()[0]).abs()
                < 1.0e-6
        );
        assert!(
            (dragged_range_plot.bounds().min()[1] - zoomed_range_plot.bounds().min()[1]).abs()
                > 1.0e-3
        );
        assert_y_bounds_inside(&dragged_range_plot, depth_timeline_y_bounds(&progress));
        let _ = render_depth_smoke_frame(
            &context,
            &progress,
            &criteria,
            vec![pointer_button(drag_end, false)],
        );

        let full_y_bounds = depth_timeline_y_bounds(&progress);
        let far_drag_start = dragged_range_plot
            .transform()
            .position_from_point(&PlotPoint::new(560.0, 1.5));
        let far_drag_down = far_drag_start + egui::vec2(0.0, 4096.0);
        let _ = render_depth_smoke_frame(
            &context,
            &progress,
            &criteria,
            vec![
                egui::Event::PointerMoved(far_drag_start),
                pointer_button(far_drag_start, true),
            ],
        );
        let (_, clamped_down_plot, _) = render_depth_smoke_frame(
            &context,
            &progress,
            &criteria,
            vec![egui::Event::PointerMoved(far_drag_down)],
        );
        assert_y_bounds_inside(&clamped_down_plot, full_y_bounds);
        let _ = render_depth_smoke_frame(
            &context,
            &progress,
            &criteria,
            vec![pointer_button(far_drag_down, false)],
        );

        let far_drag_up_start = clamped_down_plot
            .transform()
            .position_from_point(&PlotPoint::new(560.0, 1.5));
        let far_drag_up = far_drag_up_start - egui::vec2(0.0, 4096.0);
        let _ = render_depth_smoke_frame(
            &context,
            &progress,
            &criteria,
            vec![
                egui::Event::PointerMoved(far_drag_up_start),
                pointer_button(far_drag_up_start, true),
            ],
        );
        let (_, clamped_up_plot, _) = render_depth_smoke_frame(
            &context,
            &progress,
            &criteria,
            vec![egui::Event::PointerMoved(far_drag_up)],
        );
        assert_y_bounds_inside(&clamped_up_plot, full_y_bounds);
        let _ = render_depth_smoke_frame(
            &context,
            &progress,
            &criteria,
            vec![pointer_button(far_drag_up, false)],
        );

        let click_position = dragged_range_plot
            .transform()
            .position_from_point(&PlotPoint::new(450.0, 1.0));
        assert!(
            dragged_range_plot
                .transform()
                .frame()
                .contains(click_position)
        );
        let _ = render_depth_smoke_frame(
            &context,
            &progress,
            &criteria,
            vec![
                egui::Event::PointerMoved(click_position),
                pointer_button(click_position, true),
            ],
        );
        let (clicked, clicked_range_plot, clicked_base_plot) = render_depth_smoke_frame(
            &context,
            &progress,
            &criteria,
            vec![pointer_button(click_position, false)],
        );
        assert_eq!(clicked, Some(items[1]));
        assert_x_bounds_aligned(&clicked_range_plot, &clicked_base_plot);
    }
    #[test]
    fn selected_dataset_item_marks_field_cells_pose_bin_and_depth_range() {
        let item_id = test_item_id("selected.png");
        let criteria = DatasetAcceptanceDraft::default().parse().unwrap();
        let mut progress = DatasetAcceptanceProgress::empty(&criteria);
        progress.selected_item = Some(item_id);
        progress
            .item_visualizations
            .push(AutoAdmissionItemVisualization {
                item_id,
                field_cells: vec![0, 5],
                pose_bin: Some(3),
                pnp_state: AutoAdmissionPnpState::Valid,
            });

        assert!(selected_field_cell(&progress, 0));
        assert!(selected_field_cell(&progress, 5));
        assert!(!selected_field_cell(&progress, 6));
        assert_eq!(selected_pose_bin(&progress), Some(3));
    }

    #[test]
    fn zero_deadband_pose_map_has_no_center_bin_or_circle() {
        let context = egui::Context::default();
        let mut criteria = DatasetAcceptanceDraft::default().parse().unwrap();
        criteria.pnp_tilt_deadband_deg = 0.0;
        criteria.pnp_tilt_bins = 1;
        criteria.pnp_azimuth_sectors = 4;
        criteria.pose_target_per_bin = 1;
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

    #[test]
    fn coverage_color_reaches_green_at_configured_target() {
        let red = coverage_color(0, 3);
        let mid = coverage_color(1, 3);
        let green = coverage_color(3, 3);
        let above = coverage_color(7, 3);

        assert!(red.r() > green.r());
        assert!(green.g() > red.g());
        assert!(mid.r() < red.r() && mid.r() > green.r());
        assert!(mid.g() > red.g() && mid.g() < green.g());
        assert_eq!(green, above);
    }
}
