//! Color 页：X-Rite ColorChecker 24 参考版本、传统 CV 定位、D65 色准与白平衡指标展示。

use camera_toolbox_app::{
    EntryName, ExportDestination, ExportReceipt, ExportService, FileSystemError, FsControl,
};
use camera_toolbox_core::{NativeImage, Rgba8Frame, Roi};
use eframe::egui;
use serde_json::json;

use crate::workspace::{DocumentId, ImageDocument};
use std::sync::Arc;

const COLOR_CHECKER_COLUMNS: usize = 6;
const COLOR_CHECKER_ROWS: usize = 4;
const COLOR_CHECKER_PATCHES: usize = COLOR_CHECKER_COLUMNS * COLOR_CHECKER_ROWS;
const DEFAULT_PATCH_EDGE_INSET_PERCENT: f32 = 25.0;
const MIN_PATCH_EDGE_INSET_PERCENT: f32 = 0.0;
const MAX_PATCH_EDGE_INSET_PERCENT: f32 = 45.0;
const PATCH_EDGE_INSET_STEP_PERCENT: f32 = 1.0;
const MAX_NORMALIZED_GRID_RESIDUAL: f64 = 0.35;
const MIN_GRID_AREA_RATIO: f64 = 0.45;
const MAX_GRID_AREA_RATIO: f64 = 2.25;
const MAX_GRID_PROPOSALS: usize = 64;
const MAX_GRID_SEARCH_CANDIDATES: usize = 384;
const AREA_BUCKET_COUNT: usize = 12;
const SPATIAL_BUCKET_COLUMNS: usize = 16;
const SPATIAL_BUCKET_ROWS: usize = 12;
const MAX_CANDIDATES_PER_AREA_SPATIAL_BUCKET: usize = 4;
const MAX_AREA_BANDS: usize = 12;
const MAX_SPATIAL_ANCHORS_PER_AREA_BAND: usize = 48;
const PROPOSAL_SPATIAL_BUCKET_COLUMNS: usize = 8;
const PROPOSAL_SPATIAL_BUCKET_ROWS: usize = 6;
const MAX_AUTO_COLOR_LAYOUT_SCORE: f64 = 4.0;
const ADAPTIVE_PATCH_BLOCK_FRACTION: f64 = 0.02;
const ADAPTIVE_PATCH_THRESHOLD_OFFSET: u32 = 6;
const ADAPTIVE_OPEN_ELEMENT_BASE: usize = 2;
const ADAPTIVE_OPEN_ELEMENT_DIVISOR: usize = 10;
const MIN_ADAPTIVE_HOLE_AREA_FRACTION: f64 = 0.00012;
const MAX_ADAPTIVE_HOLE_AREA_FRACTION: f64 = 0.04;
const MIN_ADAPTIVE_HOLE_FILL_RATIO: f64 = 0.45;
const MIN_SPARSE_GRID_CANDIDATES: usize = 16;
const MIN_SPARSE_GRID_ROWS: usize = 3;
const MIN_SPARSE_GRID_COLUMNS: usize = 5;
const MAX_SPARSE_GRID_CELL_DISTANCE: f64 = 0.42;
const MAX_SPARSE_NORMALIZED_GRID_RESIDUAL: f64 = 0.22;
const COLOR_CHECKER_CENTER_ASPECT: f64 =
    (COLOR_CHECKER_COLUMNS - 1) as f64 / (COLOR_CHECKER_ROWS - 1) as f64;
const D65_WHITE: [f64; 3] = [0.950_47, 1.0, 1.088_83];
const D50_WHITE: [f64; 3] = [0.964_22, 1.0, 0.825_21];
const MAX_GRID_HOMOGRAPHY_RESIDUAL: f64 = 0.25;
const LAB_CHART_FULL_MIN: f64 = -128.0;
const LAB_CHART_FULL_MAX: f64 = 128.0;

#[derive(Clone, Copy, Debug)]
struct GridObservation {
    row: usize,
    column: usize,
    point: ColorImagePoint,
}
const LAB_CHART_DEFAULT_HALF_RANGE: f64 = 96.0;
const LAB_CHART_MIN_HALF_RANGE: f64 = 8.0;
const LAB_CHART_BACKGROUND_COLUMNS: usize = 32;
const LAB_CHART_BACKGROUND_ROWS: usize = 24;
const PATCH_DETAILS_COLLAPSED_RESERVE: f32 = 28.0;
const PATCH_TABLE_HEADER_HEIGHT: f32 = 22.0;
const GRAY_SWATCH_COLUMNS: usize = 6;
const GRAY_SWATCH_SPACING: f32 = 2.0;
const GRAY_SWATCH_PADDING_X: f32 = 0.5;
const GRAY_SWATCH_MAX_FONT_SIZE: f32 = 10.5;
const GRAY_SWATCH_MIN_FONT_SIZE: f32 = 8.0;
const GRAY_SWATCH_MONOSPACE_WIDTH_RATIO: f32 = 0.62;
const WHITE_BALANCE_FORMAT_NOTE: &str = "DeltaC [HSV Saturation(0-1)]";
const PATCH_COLOR_LABEL_NORMAL_FONT_SIZE: f32 = 5.5;
const PATCH_COLOR_LABEL_SELECTED_FONT_SIZE: f32 = 7.0;
const PATCH_COLOR_LABEL_MIN_CELL_SIZE: egui::Vec2 = egui::vec2(8.0, 6.0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LightSourcePreset {
    D65,
}

impl LightSourcePreset {
    const fn label(self) -> &'static str {
        match self {
            Self::D65 => "D65",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ColorChartKind {
    ColorChecker24BeforeNov2014,
    ColorChecker24Nov2014AndNewer,
}

impl ColorChartKind {
    const fn id(self) -> &'static str {
        match self {
            Self::ColorChecker24BeforeNov2014 => "xrite_colorchecker_24_before_nov_2014",
            Self::ColorChecker24Nov2014AndNewer => "xrite_colorchecker_24_nov_2014_and_newer",
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::ColorChecker24BeforeNov2014 => "ColorChecker 24（Before Nov 2014）",
            Self::ColorChecker24Nov2014AndNewer => "ColorChecker 24（Nov 2014+）",
        }
    }

    const fn reference_metadata(self) -> ColorReferenceMetadata {
        match self {
            Self::ColorChecker24BeforeNov2014 => ColorReferenceMetadata {
                id: "xrite_colorchecker_24_before_nov_2014",
                chart_name: "ColorChecker Classic 24",
                manufacturer: "X-Rite",
                formulation: "Before November 2014 edition",
                white_point: "D50",
                observer: "CIE 1931 2°",
                measurement_geometry: "45°/0°",
                measurement_condition: "X-Rite 2005 reference file; instrument/condition not specified",
                source_name: "ColorChecker24 - Before November2014 edition",
                source_url: "https://babelcolor.com/colorchecker-2.htm#CCP2_data",
                source_data_url: "https://babelcolor.com/index_htm_files/ColorChecker24_Before_Nov2014.txt",
            },
            Self::ColorChecker24Nov2014AndNewer => ColorReferenceMetadata {
                id: "xrite_colorchecker_24_nov_2014_and_newer",
                chart_name: "ColorChecker Classic 24",
                manufacturer: "X-Rite",
                formulation: "November 2014 edition and newer",
                white_point: "D50",
                observer: "CIE 1931 2°",
                measurement_geometry: "45°/0°",
                measurement_condition: "M0, filter=no, i1Pro 2 serial 1001785; file dated 2015-04-28",
                source_name: "ColorChecker24 - November2014 edition and newer",
                source_url: "https://babelcolor.com/colorchecker-2.htm#CCP2_data",
                source_data_url: "https://babelcolor.com/index_htm_files/ColorChecker24_After_Nov2014.txt",
            },
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct ColorReferenceMetadata {
    id: &'static str,
    chart_name: &'static str,
    manufacturer: &'static str,
    formulation: &'static str,
    white_point: &'static str,
    observer: &'static str,
    measurement_geometry: &'static str,
    measurement_condition: &'static str,
    source_name: &'static str,
    source_url: &'static str,
    source_data_url: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ColorInspectionAction {
    AnalyzeCurrent,
    CaptureActiveRtsp,
    StartManualCorners,
    ClearManualCorners,
    ExportMetrics,
    ExportYamlReport,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ColorInputKey {
    document_id: DocumentId,
    generation: u64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ColorPatchReference {
    name: &'static str,
    sample_name: &'static str,
    display_srgb: [u8; 3],
    source_lab: LabColor,
    comparison_lab: LabColor,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct LabColor {
    l: f64,
    a: f64,
    b: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ColorImagePoint {
    pub(crate) x: f64,
    pub(crate) y: f64,
}

impl ColorImagePoint {
    const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChartDetectionMode {
    AutoGrid,
    ManualCorners,
}

impl ChartDetectionMode {
    const fn label(self) -> &'static str {
        match self {
            Self::AutoGrid => "auto_grid",
            Self::ManualCorners => "manual_corners",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ChartPatchMeasurement {
    index: usize,
    name: &'static str,
    reference_sample_name: &'static str,
    reference_srgb: [u8; 3],
    measured_srgb: [u8; 3],
    reference_source_lab: LabColor,
    reference_lab: LabColor,
    measured_lab: LabColor,
    reference_chroma: f64,
    measured_chroma: f64,
    camera_chroma_percent: f64,
    delta_c: f64,
    delta_e: f64,
    hsv_saturation: f64,
    exposure_error_stops: f64,
    roi: Roi,
    center: ColorImagePoint,
    polygon: [ColorImagePoint; 4],
}

#[derive(Clone, Debug)]
pub(crate) struct ColorChartAnalysis {
    input: ColorInputKey,
    input_label: String,
    light_source: LightSourcePreset,
    chart_kind: ColorChartKind,
    detection_mode: ChartDetectionMode,
    image_size: [u32; 2],
    chart_roi: Roi,
    chart_corners: [ColorImagePoint; 4],
    patches: Vec<ChartPatchMeasurement>,
    source_frame: Arc<Rgba8Frame>,
    mean_delta_c: f64,
    max_delta_c: f64,
    mean_delta_e: f64,
    max_delta_e: f64,
    mean_camera_chroma_percent: f64,
    gray_mean_rgb: [f64; 3],
    gray_rg_ratio: f64,
    gray_bg_ratio: f64,
    gray_balance_error: f64,
    gray_mean_delta_c: f64,
    gray_max_delta_c: f64,
    gray_mean_hsv_saturation: f64,
    gray_max_hsv_saturation: f64,
    gray_mean_exposure_error_stops: f64,
    gray_max_abs_exposure_error_stops: f64,
    patch_edge_inset_percent: f32,
}

impl ColorChartAnalysis {
    fn from_measurements(
        input: ColorInputKey,
        input_label: String,
        light_source: LightSourcePreset,
        chart_kind: ColorChartKind,
        detection_mode: ChartDetectionMode,
        image_size: [u32; 2],
        source_frame: Arc<Rgba8Frame>,
        chart_roi: Roi,
        chart_corners: [ColorImagePoint; 4],
        patch_edge_inset_percent: f32,
        patches: Vec<ChartPatchMeasurement>,
    ) -> Result<Self, String> {
        if patches.len() != COLOR_CHECKER_PATCHES {
            return Err(format!(
                "expected {COLOR_CHECKER_PATCHES} patch measurements, got {}",
                patches.len()
            ));
        }
        let inv_patch_count = 1.0 / patches.len() as f64;
        let mean_delta_c = mean_patch_metric(&patches, |patch| patch.delta_c);
        let max_delta_c = max_patch_metric(&patches, |patch| patch.delta_c);
        let mean_delta_e = mean_patch_metric(&patches, |patch| patch.delta_e);
        let max_delta_e = max_patch_metric(&patches, |patch| patch.delta_e);
        let mean_reference_chroma = patches
            .iter()
            .map(|patch| patch.reference_chroma)
            .sum::<f64>()
            * inv_patch_count;
        let mean_measured_chroma = patches
            .iter()
            .map(|patch| patch.measured_chroma)
            .sum::<f64>()
            * inv_patch_count;
        let mean_camera_chroma_percent = if mean_reference_chroma > f64::EPSILON {
            100.0 * mean_measured_chroma / mean_reference_chroma
        } else {
            0.0
        };
        let gray = &patches[18..24];
        let gray_mean_rgb = gray.iter().fold([0.0_f64; 3], |mut total, patch| {
            total[0] += f64::from(patch.measured_srgb[0]);
            total[1] += f64::from(patch.measured_srgb[1]);
            total[2] += f64::from(patch.measured_srgb[2]);
            total
        });
        let inv_gray = 1.0 / gray.len() as f64;
        let gray_mean_rgb = [
            gray_mean_rgb[0] * inv_gray,
            gray_mean_rgb[1] * inv_gray,
            gray_mean_rgb[2] * inv_gray,
        ];
        let gray_g = gray_mean_rgb[1].max(1.0);
        let gray_rg_ratio = gray_mean_rgb[0] / gray_g;
        let gray_bg_ratio = gray_mean_rgb[2] / gray_g;
        let gray_balance_error = ((gray_rg_ratio - 1.0).abs() + (gray_bg_ratio - 1.0).abs()) * 0.5;
        let gray_mean_delta_c = mean_patch_metric(gray, |patch| patch.delta_c);
        let gray_max_delta_c = max_patch_metric(gray, |patch| patch.delta_c);
        let gray_mean_hsv_saturation =
            gray.iter().map(|patch| patch.hsv_saturation).sum::<f64>() * inv_gray;
        let gray_max_hsv_saturation = gray
            .iter()
            .map(|patch| patch.hsv_saturation)
            .fold(0.0_f64, f64::max);
        let gray_mean_exposure_error_stops = gray
            .iter()
            .map(|patch| patch.exposure_error_stops)
            .sum::<f64>()
            * inv_gray;
        let gray_max_abs_exposure_error_stops = gray
            .iter()
            .map(|patch| patch.exposure_error_stops.abs())
            .fold(0.0_f64, f64::max);
        Ok(Self {
            input,
            input_label,
            light_source,
            chart_kind,
            detection_mode,
            image_size,
            chart_roi,
            chart_corners,
            source_frame,
            patches,
            mean_delta_c,
            max_delta_c,
            mean_delta_e,
            max_delta_e,
            mean_camera_chroma_percent,
            gray_mean_rgb,
            gray_rg_ratio,
            gray_bg_ratio,
            gray_balance_error,
            gray_mean_delta_c,
            gray_max_delta_c,
            gray_mean_hsv_saturation,
            gray_max_hsv_saturation,
            gray_mean_exposure_error_stops,
            gray_max_abs_exposure_error_stops,
            patch_edge_inset_percent,
        })
    }
}

fn mean_patch_metric(
    patches: &[ChartPatchMeasurement],
    value: impl Fn(&ChartPatchMeasurement) -> f64,
) -> f64 {
    patches.iter().map(value).sum::<f64>() / patches.len().max(1) as f64
}

fn max_patch_metric(
    patches: &[ChartPatchMeasurement],
    value: impl Fn(&ChartPatchMeasurement) -> f64,
) -> f64 {
    patches.iter().map(value).fold(0.0_f64, f64::max)
}

fn normalize_patch_edge_inset_percent(percent: f32) -> f32 {
    (percent / PATCH_EDGE_INSET_STEP_PERCENT).round() * PATCH_EDGE_INSET_STEP_PERCENT
}

fn clamped_patch_edge_inset_percent(percent: f32) -> f32 {
    normalize_patch_edge_inset_percent(percent)
        .clamp(MIN_PATCH_EDGE_INSET_PERCENT, MAX_PATCH_EDGE_INSET_PERCENT)
}

fn patch_sample_fraction(patch_edge_inset_percent: f32) -> f64 {
    let fraction =
        1.0 - 2.0 * f64::from(clamped_patch_edge_inset_percent(patch_edge_inset_percent)) / 100.0;
    (fraction * 1_000_000.0).round() / 1_000_000.0
}

fn resample_analysis_with_patch_edge_inset(
    analysis: &ColorChartAnalysis,
    patch_edge_inset_percent: f32,
) -> Result<ColorChartAnalysis, String> {
    let patch_edge_inset_percent = clamped_patch_edge_inset_percent(patch_edge_inset_percent);
    let references = color_checker_references(analysis.chart_kind);
    let homography = Homography::from_unit_square(analysis.chart_corners)?;
    let patches = sample_projective_chart_patches(
        analysis.source_frame.as_ref(),
        homography,
        &references,
        patch_edge_inset_percent,
    )?;
    ColorChartAnalysis::from_measurements(
        analysis.input,
        analysis.input_label.clone(),
        analysis.light_source,
        analysis.chart_kind,
        analysis.detection_mode,
        analysis.image_size,
        Arc::clone(&analysis.source_frame),
        analysis.chart_roi,
        analysis.chart_corners,
        patch_edge_inset_percent,
        patches,
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ColorReportFormat {
    Json,
    Yaml,
}

impl ColorReportFormat {
    const fn label(self) -> &'static str {
        match self {
            Self::Json => "color metrics JSON",
            Self::Yaml => "color YAML report",
        }
    }

    const fn suggested_name(self) -> &'static str {
        match self {
            Self::Json => "color_metrics.json",
            Self::Yaml => "color_report.yaml",
        }
    }
}

pub(crate) struct ColorMetricsExport {
    format: ColorReportFormat,
    document: serde_json::Value,
}

impl ColorMetricsExport {
    fn new(document: serde_json::Value, format: ColorReportFormat) -> Self {
        Self { format, document }
    }

    pub(crate) const fn label(&self) -> &'static str {
        self.format.label()
    }

    pub(crate) const fn suggested_name(&self) -> &'static str {
        self.format.suggested_name()
    }

    pub(crate) fn save_new(
        &self,
        destination: &ExportDestination,
        name: &EntryName,
        control: &FsControl,
    ) -> Result<ExportReceipt, FileSystemError> {
        ExportService.save_new_with(destination, name, control, &mut |writer| {
            self.write_report(&mut *writer)?;
            writer.write_all(b"\n").map_err(FileSystemError::io)
        })
    }

    fn write_report(&self, writer: &mut dyn std::io::Write) -> Result<(), FileSystemError> {
        match self.format {
            ColorReportFormat::Json => {
                serde_json::to_writer_pretty(writer, &self.document).map_err(FileSystemError::io)
            }
            ColorReportFormat::Yaml => {
                serde_yaml::to_writer(writer, &self.document).map_err(FileSystemError::io)
            }
        }
    }

    #[cfg(test)]
    fn serialize_for_test(&self) -> String {
        let mut bytes = Vec::new();
        self.write_report(&mut bytes).unwrap();
        String::from_utf8(bytes).unwrap()
    }
}

#[derive(Clone, Debug)]
struct ManualCornerState {
    input: ColorInputKey,
    input_label: String,
    points: Vec<ColorImagePoint>,
}

#[derive(Clone, Copy, Debug)]
struct LabChartView {
    center_a: f64,
    center_b: f64,
    half_a: f64,
    half_b: f64,
    initialized: bool,
}

impl Default for LabChartView {
    fn default() -> Self {
        Self {
            center_a: 0.0,
            center_b: 0.0,
            half_a: LAB_CHART_DEFAULT_HALF_RANGE,
            half_b: LAB_CHART_DEFAULT_HALF_RANGE,
            initialized: false,
        }
    }
}

impl LabChartView {
    fn reset(&mut self) {
        self.initialized = false;
    }
}

pub(crate) struct ColorInspectionWorkspace {
    light_source: LightSourcePreset,
    chart_kind: ColorChartKind,
    analysis: Option<ColorChartAnalysis>,
    error: Option<String>,
    export_status: Option<String>,
    pending_export: Option<ColorMetricsExport>,
    manual_corners: Option<ManualCornerState>,
    selected_patch: Option<usize>,
    lab_chart_view: LabChartView,
    patch_details_expanded: bool,
    patch_edge_inset_percent: f32,
}

impl Default for ColorInspectionWorkspace {
    fn default() -> Self {
        Self {
            light_source: LightSourcePreset::default(),
            chart_kind: ColorChartKind::default(),
            analysis: None,
            error: None,
            export_status: None,
            pending_export: None,
            manual_corners: None,
            selected_patch: None,
            lab_chart_view: LabChartView::default(),
            patch_details_expanded: true,
            patch_edge_inset_percent: DEFAULT_PATCH_EDGE_INSET_PERCENT,
        }
    }
}

impl Default for LightSourcePreset {
    fn default() -> Self {
        Self::D65
    }
}

impl Default for ColorChartKind {
    fn default() -> Self {
        Self::ColorChecker24Nov2014AndNewer
    }
}

impl ColorInspectionWorkspace {
    pub(crate) fn render_right_panel(
        &mut self,
        ui: &mut egui::Ui,
        active_label: Option<&str>,
        can_analyze: bool,
        can_capture_rtsp: bool,
    ) -> Option<ColorInspectionAction> {
        let mut action = None;
        let has_analysis = self.current_analysis().is_some();
        let panel_height = ui.available_height().max(1.0);
        let top_max_height =
            color_sidebar_top_max_height(panel_height, has_analysis, self.patch_details_expanded);

        egui::ScrollArea::vertical()
            .id_salt("color_inspection_controls_scroll")
            .max_height(top_max_height)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let previous_light_source = self.light_source;
                let previous_chart_kind = self.chart_kind;
                let mut selected_light_source = self.light_source;
                let mut selected_chart_kind = self.chart_kind;
                let mut selected_patch_edge_inset_percent = self.patch_edge_inset_percent;

                ui.heading("Color Check");
                ui.weak("D65 光源；Color Card reference 必须匹配实体卡生产日期。PNG 文件或 RTSP Capture 后分析。");
                ui.separator();
                ui.label("Current input");
                ui.monospace(active_label.unwrap_or("No PNG or RTSP capture selected"));
                ui.separator();
                ui.label("Light source");
                egui::ComboBox::from_id_salt("color_light_source")
                    .selected_text(self.light_source.label())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut selected_light_source, LightSourcePreset::D65, "D65");
                    });
                ui.label("Color card");
                egui::ComboBox::from_id_salt("color_chart_kind")
                    .selected_text(self.chart_kind.label())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut selected_chart_kind,
                            ColorChartKind::ColorChecker24Nov2014AndNewer,
                            ColorChartKind::ColorChecker24Nov2014AndNewer.label(),
                        );
                        ui.selectable_value(
                            &mut selected_chart_kind,
                            ColorChartKind::ColorChecker24BeforeNov2014,
                            ColorChartKind::ColorChecker24BeforeNov2014.label(),
                        );
                    });
                if previous_light_source != selected_light_source
                    || previous_chart_kind != selected_chart_kind
                {
                    self.apply_reference_configuration(selected_light_source, selected_chart_kind);
                }
                ui.label("Patch inset / edge");
                let inset_changed = ui
                    .add(
                        egui::Slider::new(
                            &mut selected_patch_edge_inset_percent,
                            MIN_PATCH_EDGE_INSET_PERCENT..=MAX_PATCH_EDGE_INSET_PERCENT,
                        )
                        .suffix("%")
                        .step_by(f64::from(PATCH_EDGE_INSET_STEP_PERCENT))
                        .max_decimals(0),
                    )
                    .on_hover_text("每条色块边向内收缩的百分比；默认 25% 等价中心 50% × 50% 统计区。")
                    .changed();
                if inset_changed {
                    self.apply_patch_edge_inset_percent(selected_patch_edge_inset_percent);
                }
                let sample_fraction = patch_sample_fraction(self.patch_edge_inset_percent);
                ui.weak(format!(
                    "Sample area: {:.0}% × {:.0}%",
                    sample_fraction * 100.0,
                    sample_fraction * 100.0
                ));
                ui.add_space(6.0);
                if ui
                    .add_enabled(can_analyze, egui::Button::new("Analyze current image"))
                    .on_hover_text("全图自动搜索 24 色卡；失败后进入四角点选兜底。")
                    .clicked()
                {
                    action = Some(ColorInspectionAction::AnalyzeCurrent);
                }
                if ui
                    .add_enabled(can_analyze, egui::Button::new("Pick 4 chart corners"))
                    .on_hover_text("手动点击色卡外框四个角点；不使用矩形 ROI。")
                    .clicked()
                {
                    action = Some(ColorInspectionAction::StartManualCorners);
                }
                if ui
                    .add_enabled(
                        self.manual_corners.is_some(),
                        egui::Button::new("Clear corner picks"),
                    )
                    .clicked()
                {
                    action = Some(ColorInspectionAction::ClearManualCorners);
                }
                if ui
                    .add_enabled(can_capture_rtsp, egui::Button::new("Capture RTSP frame"))
                    .on_hover_text("将当前 RTSP 显示帧保存为临时 PNG Capture 后全图分析。")
                    .clicked()
                {
                    action = Some(ColorInspectionAction::CaptureActiveRtsp);
                }
                if ui
                    .add_enabled(
                        self.current_analysis().is_some(),
                        egui::Button::new("Export metrics JSON"),
                    )
                    .clicked()
                {
                    action = Some(ColorInspectionAction::ExportMetrics);
                }
                if ui
                    .add_enabled(
                        self.current_analysis().is_some(),
                        egui::Button::new("Export YAML Report"),
                    )
                    .clicked()
                {
                    action = Some(ColorInspectionAction::ExportYamlReport);
                }
                if let Some(manual) = self.manual_corners.as_ref() {
                    ui.separator();
                    ui.colored_label(
                        egui::Color32::YELLOW,
                        format!(
                            "Manual corner selection: click chart corners ({}/4) on {}.",
                            manual.points.len(),
                            manual.input_label
                        ),
                    );
                }
                if let Some(error) = self.error.as_deref() {
                    ui.separator();
                    ui.colored_label(egui::Color32::LIGHT_RED, error);
                }
                if let Some(status) = self.export_status.as_deref() {
                    ui.separator();
                    ui.weak(status);
                }

                if let Some(analysis) = self.current_analysis().cloned() {
                    if let Some(selected) = self.selected_patch
                        && selected >= analysis.patches.len()
                    {
                        self.selected_patch = None;
                    }
                    ui.separator();
                    render_color_accuracy(
                        ui,
                        &analysis,
                        &mut self.lab_chart_view,
                        &mut self.selected_patch,
                    );
                    ui.separator();
                    render_white_balance(ui, &analysis);
                } else {
                    ui.separator();
                    ui.weak(
                        "No color metrics yet. Open a PNG or capture an RTSP frame, then Analyze.",
                    );
                }
            });

        if let Some(analysis) = self.current_analysis().cloned() {
            ui.separator();
            let foldout = egui::CollapsingHeader::new("Patch Details")
                .id_salt("color_patch_details")
                .default_open(self.patch_details_expanded)
                .show(ui, |ui| {
                    render_patch_table(ui, &analysis, &mut self.selected_patch)
                });
            self.patch_details_expanded = !foldout.fully_closed();
        }

        action
    }

    fn apply_reference_configuration(
        &mut self,
        light_source: LightSourcePreset,
        chart_kind: ColorChartKind,
    ) {
        if self.light_source == light_source && self.chart_kind == chart_kind {
            return;
        }
        self.light_source = light_source;
        self.chart_kind = chart_kind;
        self.invalidate_analysis_for_configuration_change();
    }

    fn apply_patch_edge_inset_percent(&mut self, percent: f32) {
        let percent = clamped_patch_edge_inset_percent(percent);
        if (self.patch_edge_inset_percent - percent).abs() <= f32::EPSILON {
            return;
        }
        self.patch_edge_inset_percent = percent;
        self.pending_export = None;
        self.export_status = None;
        if let Some(current) = self.analysis.as_ref() {
            match resample_analysis_with_patch_edge_inset(current, percent) {
                Ok(analysis) => {
                    self.analysis = Some(analysis);
                    if self
                        .selected_patch
                        .is_some_and(|selected| selected >= COLOR_CHECKER_PATCHES)
                    {
                        self.selected_patch = None;
                    }
                    self.lab_chart_view.reset();
                    self.error = None;
                }
                Err(error) => {
                    self.lab_chart_view.reset();
                    self.error = Some(format!("Patch inset update failed: {error}"));
                }
            }
        }
    }

    fn invalidate_analysis_for_configuration_change(&mut self) {
        self.analysis = None;
        self.pending_export = None;
        self.export_status = None;
        self.selected_patch = None;
        self.lab_chart_view.reset();
    }

    fn current_analysis(&self) -> Option<&ColorChartAnalysis> {
        self.analysis.as_ref().filter(|analysis| {
            analysis.light_source == self.light_source
                && analysis.chart_kind == self.chart_kind
                && (analysis.patch_edge_inset_percent - self.patch_edge_inset_percent).abs()
                    <= f32::EPSILON
        })
    }

    #[must_use]
    pub(crate) const fn selected_patch_index(&self) -> Option<usize> {
        self.selected_patch
    }

    pub(crate) fn analyze_document(&mut self, document: &ImageDocument) {
        let input = ColorInputKey {
            document_id: document.id,
            generation: document.generation,
        };
        let label = document.title.clone();
        let result = analyze_native_result(
            input,
            label.clone(),
            &document.native,
            self.light_source,
            self.chart_kind,
            self.patch_edge_inset_percent,
        );
        self.install_auto_analysis(input, label, result);
    }

    pub(crate) fn analyze_frame(
        &mut self,
        document_id: DocumentId,
        generation: u64,
        label: String,
        frame: Arc<Rgba8Frame>,
    ) {
        let input = ColorInputKey {
            document_id,
            generation,
        };
        let result = analyze_rgba8_arc_with_patch_edge_inset(
            input,
            label.clone(),
            frame,
            self.light_source,
            self.chart_kind,
            self.patch_edge_inset_percent,
        );
        self.install_auto_analysis(input, label, result);
    }

    pub(crate) fn start_manual_corners(&mut self, document: &ImageDocument) {
        let input = ColorInputKey {
            document_id: document.id,
            generation: document.generation,
        };
        self.manual_corners = Some(ManualCornerState {
            input,
            input_label: document.title.clone(),
            points: Vec::with_capacity(4),
        });
        self.error = Some("Click the four outer ColorChecker corners in the Viewer.".to_owned());
    }

    pub(crate) fn clear_manual_corners(&mut self) {
        self.manual_corners = None;
        self.error = None;
    }

    pub(crate) fn handle_manual_corner_click(
        &mut self,
        document: &ImageDocument,
        point: ColorImagePoint,
    ) {
        let input = ColorInputKey {
            document_id: document.id,
            generation: document.generation,
        };
        let Some(existing) = self.manual_corners.as_ref() else {
            return;
        };
        if existing.input != input {
            self.manual_corners = Some(ManualCornerState {
                input,
                input_label: document.title.clone(),
                points: Vec::with_capacity(4),
            });
        }
        let Some(manual) = self.manual_corners.as_mut() else {
            return;
        };
        if manual.points.len() >= 4 {
            manual.points.clear();
        }
        manual.points.push(point);
        if manual.points.len() < 4 {
            self.error = Some(format!(
                "Manual corner selection: click chart corner {} of 4.",
                manual.points.len() + 1
            ));
            return;
        }
        let corners = [
            manual.points[0],
            manual.points[1],
            manual.points[2],
            manual.points[3],
        ];
        let label = manual.input_label.clone();
        let result = analyze_native_with_manual_corners(
            input,
            label,
            &document.native,
            corners,
            self.light_source,
            self.chart_kind,
            self.patch_edge_inset_percent,
        );
        match result {
            Ok(analysis) => {
                self.pending_export = None;
                self.analysis = Some(analysis);
                self.selected_patch = None;
                self.lab_chart_view.reset();
                self.error = None;
                self.export_status = None;
                self.manual_corners = None;
            }
            Err(error) => {
                self.error = Some(error);
                if let Some(manual) = self.manual_corners.as_mut() {
                    manual.points.clear();
                }
            }
        }
    }

    pub(crate) fn prepare_export(&mut self) {
        self.prepare_export_with_format(ColorReportFormat::Json);
    }

    pub(crate) fn prepare_yaml_report_export(&mut self) {
        self.prepare_export_with_format(ColorReportFormat::Yaml);
    }

    fn prepare_export_with_format(&mut self, format: ColorReportFormat) {
        let Some(analysis) = self.current_analysis() else {
            self.pending_export = None;
            self.error = Some(format!(
                "analyze an image before exporting {}",
                format.label()
            ));
            return;
        };
        let document = match format {
            ColorReportFormat::Json => export_payload(analysis),
            ColorReportFormat::Yaml => metrics_report_payload(analysis),
        };
        self.pending_export = Some(ColorMetricsExport::new(document, format));
    }

    pub(crate) fn take_export(&mut self) -> Option<ColorMetricsExport> {
        self.pending_export.take()
    }

    pub(crate) fn report_error(&mut self, error: impl Into<String>) {
        self.error = Some(error.into());
    }

    pub(crate) fn report_export_started(&mut self, label: &str, target: &str) {
        self.export_status = Some(format!("Exporting {label} to {target}"));
    }

    pub(crate) fn report_export_finished(
        &mut self,
        label: &str,
        target: &str,
        result: Result<u64, &str>,
    ) {
        match result {
            Ok(bytes) => {
                self.export_status = Some(format!("Exported {label} to {target} ({bytes} bytes)"));
                self.error = None;
            }
            Err(error) => {
                self.export_status = Some(format!("Export {label} failed for {target}"));
                self.error = Some(error.to_owned());
            }
        }
    }

    #[must_use]
    pub(crate) fn analysis_for_overlay(
        &self,
        document_id: DocumentId,
        generation: u64,
    ) -> Option<&ColorChartAnalysis> {
        self.current_analysis().filter(|analysis| {
            analysis.input.document_id == document_id && analysis.input.generation == generation
        })
    }

    #[must_use]
    pub(crate) fn manual_corners_for_overlay(
        &self,
        document_id: DocumentId,
        generation: u64,
    ) -> Option<&[ColorImagePoint]> {
        self.manual_corners
            .as_ref()
            .filter(|manual| {
                manual.input.document_id == document_id && manual.input.generation == generation
            })
            .map(|manual| manual.points.as_slice())
    }

    fn install_auto_analysis(
        &mut self,
        input: ColorInputKey,
        label: String,
        result: Result<ColorChartAnalysis, String>,
    ) {
        match result {
            Ok(analysis) => {
                self.pending_export = None;
                self.analysis = Some(analysis);
                self.selected_patch = None;
                self.lab_chart_view.reset();
                self.error = None;
                self.export_status = None;
                self.manual_corners = None;
            }
            Err(error) => {
                self.error = Some(format!(
                    "Auto chart detection failed: {error}. Click four outer corners to continue."
                ));
                self.manual_corners = Some(ManualCornerState {
                    input,
                    input_label: label,
                    points: Vec::with_capacity(4),
                });
            }
        }
    }
}

pub(crate) fn paint_color_chart_overlay(
    painter: &egui::Painter,
    image_rect: egui::Rect,
    image_size: [u32; 2],
    analysis: &ColorChartAnalysis,
    horizontal_flip: bool,
    selected_patch: Option<usize>,
) {
    let stroke = egui::Stroke::new(1.5, egui::Color32::from_rgb(255, 210, 80));
    paint_polygon(
        painter,
        image_rect,
        image_size,
        &analysis.chart_corners,
        horizontal_flip,
        stroke,
    );
    for patch in &analysis.patches {
        let selected = selected_patch == Some(patch.index);
        let patch_stroke = if selected {
            egui::Stroke::new(2.4, egui::Color32::YELLOW)
        } else {
            egui::Stroke::new(
                0.8,
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 160),
            )
        };
        let center = image_to_screen(image_rect, image_size, patch.center, horizontal_flip);
        let screen_polygon = patch
            .polygon
            .map(|point| image_to_screen(image_rect, image_size, point, horizontal_flip));
        paint_patch_color_quadrants(
            painter,
            screen_polygon,
            patch.measured_srgb,
            patch.reference_srgb,
            selected,
        );
        paint_polygon(
            painter,
            image_rect,
            image_size,
            &patch.polygon,
            horizontal_flip,
            patch_stroke,
        );
        painter.circle_filled(
            center,
            if selected { 3.2 } else { 1.8 },
            if selected {
                egui::Color32::YELLOW
            } else {
                egui::Color32::from_rgb(255, 255, 255)
            },
        );
        if selected {
            painter.text(
                center + egui::vec2(5.0, -5.0),
                egui::Align2::LEFT_BOTTOM,
                (patch.index + 1).to_string(),
                egui::FontId::monospace(12.0),
                egui::Color32::YELLOW,
            );
        }
    }
}

pub(crate) fn paint_manual_corner_overlay(
    painter: &egui::Painter,
    image_rect: egui::Rect,
    image_size: [u32; 2],
    points: &[ColorImagePoint],
    horizontal_flip: bool,
) {
    let stroke = egui::Stroke::new(1.5, egui::Color32::YELLOW);
    let screen_points = points
        .iter()
        .copied()
        .map(|point| image_to_screen(image_rect, image_size, point, horizontal_flip))
        .collect::<Vec<_>>();
    for (index, point) in screen_points.iter().copied().enumerate() {
        painter.circle_filled(point, 4.0, egui::Color32::YELLOW);
        painter.text(
            point + egui::vec2(5.0, -5.0),
            egui::Align2::LEFT_BOTTOM,
            (index + 1).to_string(),
            egui::FontId::monospace(12.0),
            egui::Color32::YELLOW,
        );
    }
    for pair in screen_points.windows(2) {
        painter.line_segment([pair[0], pair[1]], stroke);
    }
}

fn paint_polygon(
    painter: &egui::Painter,
    image_rect: egui::Rect,
    image_size: [u32; 2],
    polygon: &[ColorImagePoint; 4],
    horizontal_flip: bool,
    stroke: egui::Stroke,
) {
    let points =
        polygon.map(|point| image_to_screen(image_rect, image_size, point, horizontal_flip));
    for index in 0..4 {
        painter.line_segment([points[index], points[(index + 1) % 4]], stroke);
    }
}

fn paint_patch_color_quadrants(
    painter: &egui::Painter,
    screen_polygon: [egui::Pos2; 4],
    measured_srgb: [u8; 3],
    reference_srgb: [u8; 3],
    selected: bool,
) {
    let measured_cell = patch_grid_cell(screen_polygon, 0.0, 0.5, 0.5, 1.0);
    let reference_cell = patch_grid_cell(screen_polygon, 0.5, 1.0, 0.5, 1.0);
    paint_patch_color_cell(painter, measured_cell, measured_srgb, "Avg", selected);
    paint_patch_color_cell(painter, reference_cell, reference_srgb, "Ref", selected);

    let quad = screen_quad_visual_ordered(screen_polygon);
    let top_mid = patch_quad_lerp(quad, 0.5, 0.0);
    let bottom_mid = patch_quad_lerp(quad, 0.5, 1.0);
    let left_mid = patch_quad_lerp(quad, 0.0, 0.5);
    let right_mid = patch_quad_lerp(quad, 1.0, 0.5);
    let divider_color = if selected {
        egui::Color32::from_rgba_unmultiplied(255, 255, 0, 210)
    } else {
        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 150)
    };
    let divider_stroke = egui::Stroke::new(if selected { 1.0 } else { 0.6 }, divider_color);
    painter.line_segment([top_mid, bottom_mid], divider_stroke);
    painter.line_segment([left_mid, right_mid], divider_stroke);
}

fn paint_patch_color_cell(
    painter: &egui::Painter,
    cell: [egui::Pos2; 4],
    srgb: [u8; 3],
    label: &str,
    selected: bool,
) {
    painter.add(egui::Shape::convex_polygon(
        cell.to_vec(),
        color32_from_srgb(srgb),
        egui::Stroke::NONE,
    ));
    let bounds = egui::Rect::from_points(&cell);
    if bounds.width() < PATCH_COLOR_LABEL_MIN_CELL_SIZE.x
        || bounds.height() < PATCH_COLOR_LABEL_MIN_CELL_SIZE.y
    {
        return;
    }
    let font_size = if selected {
        PATCH_COLOR_LABEL_SELECTED_FONT_SIZE
    } else {
        PATCH_COLOR_LABEL_NORMAL_FONT_SIZE
    };
    painter.text(
        cell[0] + egui::vec2(1.0, 1.0),
        egui::Align2::LEFT_TOP,
        label,
        egui::FontId::monospace(font_size),
        swatch_text_color(srgb),
    );
}

fn patch_grid_cell(
    screen_polygon: [egui::Pos2; 4],
    x0: f32,
    x1: f32,
    y0: f32,
    y1: f32,
) -> [egui::Pos2; 4] {
    let quad = screen_quad_visual_ordered(screen_polygon);
    [
        patch_quad_lerp(quad, x0, y0),
        patch_quad_lerp(quad, x1, y0),
        patch_quad_lerp(quad, x1, y1),
        patch_quad_lerp(quad, x0, y1),
    ]
}

fn patch_quad_lerp(quad: [egui::Pos2; 4], x: f32, y: f32) -> egui::Pos2 {
    let top = quad[0].lerp(quad[1], x);
    let bottom = quad[3].lerp(quad[2], x);
    top.lerp(bottom, y)
}

fn screen_quad_visual_ordered(points: [egui::Pos2; 4]) -> [egui::Pos2; 4] {
    let mut ordered = points;
    ordered.sort_by(|a, b| a.y.total_cmp(&b.y).then_with(|| a.x.total_cmp(&b.x)));
    let mut top = [ordered[0], ordered[1]];
    let mut bottom = [ordered[2], ordered[3]];
    top.sort_by(|a, b| a.x.total_cmp(&b.x));
    bottom.sort_by(|a, b| a.x.total_cmp(&b.x));
    [top[0], top[1], bottom[1], bottom[0]]
}

fn color32_from_srgb(rgb: [u8; 3]) -> egui::Color32 {
    egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2])
}

fn image_to_screen(
    image_rect: egui::Rect,
    image_size: [u32; 2],
    point: ColorImagePoint,
    horizontal_flip: bool,
) -> egui::Pos2 {
    let width = f64::from(image_size[0].max(1));
    let height = f64::from(image_size[1].max(1));
    let normalized_x = (point.x / width).clamp(0.0, 1.0) as f32;
    let normalized_x = if horizontal_flip {
        1.0 - normalized_x
    } else {
        normalized_x
    };
    let normalized_y = (point.y / height).clamp(0.0, 1.0) as f32;
    egui::pos2(
        image_rect.left() + normalized_x * image_rect.width(),
        image_rect.top() + normalized_y * image_rect.height(),
    )
}

fn analyze_native_result(
    input: ColorInputKey,
    input_label: String,
    native: &NativeImage,
    light_source: LightSourcePreset,
    chart_kind: ColorChartKind,
    patch_edge_inset_percent: f32,
) -> Result<ColorChartAnalysis, String> {
    let NativeImage::Rgba8(frame) = native else {
        return Err("Color page accepts PNG / RTSP Capture RGBA images only".to_owned());
    };
    analyze_rgba8_arc_with_patch_edge_inset(
        input,
        input_label,
        Arc::clone(frame),
        light_source,
        chart_kind,
        patch_edge_inset_percent,
    )
}

fn analyze_native_with_manual_corners(
    input: ColorInputKey,
    input_label: String,
    native: &NativeImage,
    corners: [ColorImagePoint; 4],
    light_source: LightSourcePreset,
    chart_kind: ColorChartKind,
    patch_edge_inset_percent: f32,
) -> Result<ColorChartAnalysis, String> {
    let NativeImage::Rgba8(frame) = native else {
        return Err("Color page accepts PNG / RTSP Capture RGBA images only".to_owned());
    };
    analyze_rgba8_arc_with_corners_with_patch_edge_inset(
        input,
        input_label,
        Arc::clone(frame),
        corners,
        ChartDetectionMode::ManualCorners,
        light_source,
        chart_kind,
        patch_edge_inset_percent,
    )
}

#[cfg(test)]
fn analyze_rgba8(
    input: ColorInputKey,
    input_label: String,
    frame: &Rgba8Frame,
    light_source: LightSourcePreset,
    chart_kind: ColorChartKind,
) -> Result<ColorChartAnalysis, String> {
    analyze_rgba8_with_patch_edge_inset(
        input,
        input_label,
        frame,
        light_source,
        chart_kind,
        DEFAULT_PATCH_EDGE_INSET_PERCENT,
    )
}

#[cfg(test)]
fn analyze_rgba8_with_patch_edge_inset(
    input: ColorInputKey,
    input_label: String,
    frame: &Rgba8Frame,
    light_source: LightSourcePreset,
    chart_kind: ColorChartKind,
    patch_edge_inset_percent: f32,
) -> Result<ColorChartAnalysis, String> {
    analyze_rgba8_arc_with_patch_edge_inset(
        input,
        input_label,
        Arc::new(frame.clone()),
        light_source,
        chart_kind,
        patch_edge_inset_percent,
    )
}

fn analyze_rgba8_arc_with_patch_edge_inset(
    input: ColorInputKey,
    input_label: String,
    source_frame: Arc<Rgba8Frame>,
    light_source: LightSourcePreset,
    chart_kind: ColorChartKind,
    patch_edge_inset_percent: f32,
) -> Result<ColorChartAnalysis, String> {
    let patch_edge_inset_percent = clamped_patch_edge_inset_percent(patch_edge_inset_percent);
    let proposals = detect_chart_corner_proposals_auto(source_frame.as_ref())?;
    let mut best: Option<(f64, ColorChartAnalysis)> = None;
    let mut best_rejected_score = f64::INFINITY;
    let mut last_error = None;
    for proposal in proposals {
        match analyze_rgba8_arc_with_corners_with_patch_edge_inset(
            input,
            input_label.clone(),
            Arc::clone(&source_frame),
            proposal.corners,
            ChartDetectionMode::AutoGrid,
            light_source,
            chart_kind,
            patch_edge_inset_percent,
        ) {
            Ok(analysis) => {
                let color_layout_score = orientation_score(&analysis.patches);
                if color_layout_score <= MAX_AUTO_COLOR_LAYOUT_SCORE {
                    if best
                        .as_ref()
                        .is_none_or(|(best_score, _)| color_layout_score < *best_score)
                    {
                        best = Some((color_layout_score, analysis));
                    }
                } else {
                    best_rejected_score = best_rejected_score.min(color_layout_score);
                }
            }
            Err(error) => last_error = Some(error),
        }
    }
    if let Some((_, analysis)) = best {
        Ok(analysis)
    } else if best_rejected_score.is_finite() {
        Err(format!(
            "auto chart color layout score {best_rejected_score:.2} exceeds limit {MAX_AUTO_COLOR_LAYOUT_SCORE:.2}"
        ))
    } else {
        Err(last_error.unwrap_or_else(|| {
            "auto chart detection produced no usable ColorChecker grid".to_owned()
        }))
    }
}
#[cfg(test)]
fn analyze_rgba8_with_corners(
    input: ColorInputKey,
    input_label: String,
    frame: &Rgba8Frame,
    corners: [ColorImagePoint; 4],
    detection_mode: ChartDetectionMode,
    light_source: LightSourcePreset,
    chart_kind: ColorChartKind,
) -> Result<ColorChartAnalysis, String> {
    analyze_rgba8_with_corners_with_patch_edge_inset(
        input,
        input_label,
        frame,
        corners,
        detection_mode,
        light_source,
        chart_kind,
        DEFAULT_PATCH_EDGE_INSET_PERCENT,
    )
}

#[cfg(test)]
fn analyze_rgba8_with_corners_with_patch_edge_inset(
    input: ColorInputKey,
    input_label: String,
    frame: &Rgba8Frame,
    corners: [ColorImagePoint; 4],
    detection_mode: ChartDetectionMode,
    light_source: LightSourcePreset,
    chart_kind: ColorChartKind,
    patch_edge_inset_percent: f32,
) -> Result<ColorChartAnalysis, String> {
    analyze_rgba8_arc_with_corners_with_patch_edge_inset(
        input,
        input_label,
        Arc::new(frame.clone()),
        corners,
        detection_mode,
        light_source,
        chart_kind,
        patch_edge_inset_percent,
    )
}

fn analyze_rgba8_arc_with_corners_with_patch_edge_inset(
    input: ColorInputKey,
    input_label: String,
    source_frame: Arc<Rgba8Frame>,
    corners: [ColorImagePoint; 4],
    detection_mode: ChartDetectionMode,
    light_source: LightSourcePreset,
    chart_kind: ColorChartKind,
    patch_edge_inset_percent: f32,
) -> Result<ColorChartAnalysis, String> {
    let patch_edge_inset_percent = clamped_patch_edge_inset_percent(patch_edge_inset_percent);
    let frame = source_frame.as_ref();
    let corners = canonicalize_corners(corners)?;
    let references = color_checker_references(chart_kind);
    let mut best: Option<(f64, [ColorImagePoint; 4], Vec<ChartPatchMeasurement>)> = None;
    for ordered in corner_orientation_variants(corners) {
        let homography = Homography::from_unit_square(ordered)?;
        let patches = sample_projective_chart_patches(
            frame,
            homography,
            &references,
            patch_edge_inset_percent,
        )?;
        let score = orientation_score(&patches);
        if best
            .as_ref()
            .is_none_or(|(best_score, _, _)| score < *best_score)
        {
            best = Some((score, ordered, patches));
        }
    }
    let Some((_, chart_corners, patches)) = best else {
        return Err("manual corners did not produce a valid 6x4 ColorChecker grid".to_owned());
    };
    let chart_roi = bounding_roi(&chart_corners, frame.width, frame.height)
        .ok_or_else(|| "detected chart corners are outside the image".to_owned())?;
    ColorChartAnalysis::from_measurements(
        input,
        input_label,
        light_source,
        chart_kind,
        detection_mode,
        [frame.width, frame.height],
        Arc::clone(&source_frame),
        chart_roi,
        chart_corners,
        patch_edge_inset_percent,
        patches,
    )
}

#[derive(Clone, Copy, Debug)]
struct PatchCandidate {
    id: usize,
    center: ColorImagePoint,
    area: u32,
}

#[derive(Clone, Copy, Debug)]
struct GridFit {
    corners: [ColorImagePoint; 4],
    residual: f64,
    area_ratio: f64,
}

#[derive(Clone, Copy, Debug)]
struct CandidateProjectionRect {
    center: ColorImagePoint,
    axis_u: ColorImagePoint,
    axis_v: ColorImagePoint,
    min_u: f64,
    max_u: f64,
    min_v: f64,
    max_v: f64,
}

#[derive(Clone, Copy, Debug)]
struct Homography {
    a: f64,
    b: f64,
    c: f64,
    d: f64,
    e: f64,
    f: f64,
    g: f64,
    h: f64,
}

impl Homography {
    fn from_unit_square(corners: [ColorImagePoint; 4]) -> Result<Self, String> {
        let [p0, p1, p2, p3] = corners;
        let dx1 = p1.x - p2.x;
        let dy1 = p1.y - p2.y;
        let dx2 = p3.x - p2.x;
        let dy2 = p3.y - p2.y;
        let sx = p0.x - p1.x + p2.x - p3.x;
        let sy = p0.y - p1.y + p2.y - p3.y;
        let denominator = dx1 * dy2 - dx2 * dy1;
        if denominator.abs() < 1.0e-9 {
            return Err("chart corners are projectively degenerate".to_owned());
        }
        let g = (sx * dy2 - dx2 * sy) / denominator;
        let h = (dx1 * sy - sx * dy1) / denominator;
        Ok(Self {
            a: p1.x - p0.x + g * p1.x,
            b: p3.x - p0.x + h * p3.x,
            c: p0.x,
            d: p1.y - p0.y + g * p1.y,
            e: p3.y - p0.y + h * p3.y,
            f: p0.y,
            g,
            h,
        })
    }

    fn map(self, u: f64, v: f64) -> Option<ColorImagePoint> {
        let denominator = self.g * u + self.h * v + 1.0;
        if denominator.abs() < 1.0e-9 {
            return None;
        }
        Some(ColorImagePoint::new(
            (self.a * u + self.b * v + self.c) / denominator,
            (self.d * u + self.e * v + self.f) / denominator,
        ))
    }
}

fn detect_chart_corner_proposals_auto(frame: &Rgba8Frame) -> Result<Vec<GridFit>, String> {
    let candidates = detect_adaptive_hole_patch_candidates(frame)?;
    let mut proposals = Vec::new();
    if candidates.len() >= COLOR_CHECKER_PATCHES {
        proposals.extend(fit_grid_corner_proposals(&candidates));
    }
    if candidates.len() >= MIN_SPARSE_GRID_CANDIDATES {
        proposals.extend(fit_sparse_grid_corner_proposals(&candidates));
    }
    if proposals.is_empty() {
        Err(format!(
            "found {} adaptive hole candidates; no usable 6x4 ColorChecker grid was found",
            candidates.len()
        ))
    } else {
        Ok(retain_diverse_grid_proposals(proposals))
    }
}

/// 借鉴 Macduff：每通道自适应暗阈值取 OR，opening 后把被黑色基板/间隙包围的洞当作 patch 候选。
fn detect_adaptive_hole_patch_candidates(
    frame: &Rgba8Frame,
) -> Result<Vec<PatchCandidate>, String> {
    let width = usize::try_from(frame.width).map_err(|_| "image width is too large".to_owned())?;
    let height =
        usize::try_from(frame.height).map_err(|_| "image height is too large".to_owned())?;
    let pixel_count = width
        .checked_mul(height)
        .ok_or_else(|| "image is too large for adaptive patch detection".to_owned())?;
    let block_size = adaptive_patch_block_size(width, height);
    let dark_mask = adaptive_dark_mask(frame, width, height, block_size)?;
    let opened = morphology_open_bool(
        &dark_mask,
        width,
        height,
        adaptive_open_element_size(block_size),
    )?;
    let mut visited = vec![false; pixel_count];
    let mut candidates = Vec::new();
    let image_area = pixel_count as f64;
    let min_area = (image_area * MIN_ADAPTIVE_HOLE_AREA_FRACTION).max(16.0) as u32;
    let max_area = (image_area * MAX_ADAPTIVE_HOLE_AREA_FRACTION).max(f64::from(min_area));
    for start in 0..pixel_count {
        if visited[start] || opened[start] {
            continue;
        }
        let mut stack = vec![start];
        visited[start] = true;
        let mut area = 0_u32;
        let mut min_x = frame.width;
        let mut min_y = frame.height;
        let mut max_x = 0_u32;
        let mut max_y = 0_u32;
        let mut touches_border = false;
        while let Some(index) = stack.pop() {
            let x = (index % width) as u32;
            let y = (index / width) as u32;
            touches_border |= x == 0 || y == 0 || x + 1 >= frame.width || y + 1 >= frame.height;
            area = area.saturating_add(1);
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
            for neighbor in component_neighbors(index, width, height) {
                if neighbor == index || visited[neighbor] || opened[neighbor] {
                    continue;
                }
                visited[neighbor] = true;
                stack.push(neighbor);
            }
        }
        if touches_border || area < min_area || f64::from(area) > max_area {
            continue;
        }
        let bbox_width = max_x.saturating_sub(min_x).saturating_add(1);
        let bbox_height = max_y.saturating_sub(min_y).saturating_add(1);
        if bbox_width < 4 || bbox_height < 4 {
            continue;
        }
        let aspect = f64::from(bbox_width) / f64::from(bbox_height.max(1));
        if !(0.40..=2.50).contains(&aspect) {
            continue;
        }
        let fill = f64::from(area) / f64::from(bbox_width.saturating_mul(bbox_height).max(1));
        if fill < MIN_ADAPTIVE_HOLE_FILL_RATIO {
            continue;
        }
        let id = candidates.len();
        candidates.push(PatchCandidate {
            id,
            center: ColorImagePoint::new(
                (f64::from(min_x) + f64::from(max_x) + 1.0) * 0.5,
                (f64::from(min_y) + f64::from(max_y) + 1.0) * 0.5,
            ),
            area,
        });
    }
    candidates.sort_by(|a, b| b.area.cmp(&a.area).then_with(|| a.id.cmp(&b.id)));
    Ok(candidates)
}

fn adaptive_patch_block_size(width: usize, height: usize) -> usize {
    let base = ((width.min(height) as f64) * ADAPTIVE_PATCH_BLOCK_FRACTION).round() as usize;
    (base.max(3)) | 1
}

fn adaptive_open_element_size(block_size: usize) -> usize {
    (block_size / ADAPTIVE_OPEN_ELEMENT_DIVISOR + ADAPTIVE_OPEN_ELEMENT_BASE).max(1)
}

fn adaptive_dark_mask(
    frame: &Rgba8Frame,
    width: usize,
    height: usize,
    block_size: usize,
) -> Result<Vec<bool>, String> {
    let pixel_count = width
        .checked_mul(height)
        .ok_or_else(|| "image is too large for adaptive thresholding".to_owned())?;
    let mut mask = vec![false; pixel_count];
    let radius = block_size / 2;
    for channel in 0..3 {
        let integral = channel_integral(frame, width, height, channel)?;
        let integral_width = width + 1;
        for y in 0..height {
            let y0 = y.saturating_sub(radius);
            let y1 = y.saturating_add(radius + 1).min(height);
            for x in 0..width {
                let Some(rgba) = rgba_components_at(frame, x, y) else {
                    continue;
                };
                if rgba[3] == 0 {
                    continue;
                }
                let x0 = x.saturating_sub(radius);
                let x1 = x.saturating_add(radius + 1).min(width);
                let count = ((x1 - x0) * (y1 - y0)) as u64;
                let sum = u64::from(rect_sum_u32(&integral, integral_width, x0, y0, x1, y1));
                let value = u64::from(rgba[channel]);
                if (value + u64::from(ADAPTIVE_PATCH_THRESHOLD_OFFSET)) * count <= sum {
                    mask[y * width + x] = true;
                }
            }
        }
    }
    Ok(mask)
}

fn channel_integral(
    frame: &Rgba8Frame,
    width: usize,
    height: usize,
    channel: usize,
) -> Result<Vec<u32>, String> {
    let integral_width = width
        .checked_add(1)
        .ok_or_else(|| "adaptive integral width overflowed".to_owned())?;
    let integral_height = height
        .checked_add(1)
        .ok_or_else(|| "adaptive integral height overflowed".to_owned())?;
    let mut integral = vec![0_u32; integral_width * integral_height];
    for y in 0..height {
        let mut row_sum = 0_u32;
        for x in 0..width {
            let value = rgba_components_at(frame, x, y)
                .filter(|rgba| rgba[3] != 0)
                .map_or(255, |rgba| rgba[channel]);
            row_sum = row_sum.saturating_add(u32::from(value));
            let index = (y + 1) * integral_width + x + 1;
            integral[index] = integral[index - integral_width].saturating_add(row_sum);
        }
    }
    Ok(integral)
}

fn morphology_open_bool(
    mask: &[bool],
    width: usize,
    height: usize,
    element_size: usize,
) -> Result<Vec<bool>, String> {
    let eroded = morphology_erode_bool(mask, width, height, element_size)?;
    morphology_dilate_bool(&eroded, width, height, element_size)
}

fn morphology_erode_bool(
    mask: &[bool],
    width: usize,
    height: usize,
    element_size: usize,
) -> Result<Vec<bool>, String> {
    let integral = bool_integral(mask, width, height)?;
    let integral_width = width + 1;
    let mut output = vec![false; mask.len()];
    for y in 0..height {
        for x in 0..width {
            let (x0, y0, x1, y1) = morphology_window(x, y, width, height, element_size);
            let area = ((x1 - x0) * (y1 - y0)) as u32;
            output[y * width + x] = rect_sum_u32(&integral, integral_width, x0, y0, x1, y1) == area;
        }
    }
    Ok(output)
}

fn morphology_dilate_bool(
    mask: &[bool],
    width: usize,
    height: usize,
    element_size: usize,
) -> Result<Vec<bool>, String> {
    let integral = bool_integral(mask, width, height)?;
    let integral_width = width + 1;
    let mut output = vec![false; mask.len()];
    for y in 0..height {
        for x in 0..width {
            let (x0, y0, x1, y1) = morphology_window(x, y, width, height, element_size);
            output[y * width + x] = rect_sum_u32(&integral, integral_width, x0, y0, x1, y1) > 0;
        }
    }
    Ok(output)
}

fn morphology_window(
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    element_size: usize,
) -> (usize, usize, usize, usize) {
    let before = element_size / 2;
    let after = element_size.saturating_sub(before).max(1);
    (
        x.saturating_sub(before),
        y.saturating_sub(before),
        x.saturating_add(after).min(width),
        y.saturating_add(after).min(height),
    )
}

fn bool_integral(mask: &[bool], width: usize, height: usize) -> Result<Vec<u32>, String> {
    let integral_width = width
        .checked_add(1)
        .ok_or_else(|| "binary integral width overflowed".to_owned())?;
    let integral_height = height
        .checked_add(1)
        .ok_or_else(|| "binary integral height overflowed".to_owned())?;
    if mask.len() != width.saturating_mul(height) {
        return Err("binary mask dimensions do not match".to_owned());
    }
    let mut integral = vec![0_u32; integral_width * integral_height];
    for y in 0..height {
        let mut row_sum = 0_u32;
        for x in 0..width {
            row_sum = row_sum.saturating_add(u32::from(mask[y * width + x]));
            let index = (y + 1) * integral_width + x + 1;
            integral[index] = integral[index - integral_width].saturating_add(row_sum);
        }
    }
    Ok(integral)
}

fn rect_sum_u32(
    integral: &[u32],
    integral_width: usize,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
) -> u32 {
    integral[y1 * integral_width + x1]
        .saturating_add(integral[y0 * integral_width + x0])
        .saturating_sub(integral[y0 * integral_width + x1])
        .saturating_sub(integral[y1 * integral_width + x0])
}

fn rgba_components_at(frame: &Rgba8Frame, x: usize, y: usize) -> Option<[u8; 4]> {
    if x >= frame.width as usize || y >= frame.height as usize {
        return None;
    }
    let start = y
        .checked_mul(frame.stride)?
        .checked_add(x.checked_mul(4)?)?;
    let pixels = frame.pixels();
    (start + 3 < pixels.len()).then(|| {
        [
            pixels[start],
            pixels[start + 1],
            pixels[start + 2],
            pixels[start + 3],
        ]
    })
}

fn component_neighbors(index: usize, width: usize, height: usize) -> [usize; 4] {
    let x = index % width;
    let y = index / width;
    [
        if x > 0 { index - 1 } else { index },
        if x + 1 < width { index + 1 } else { index },
        if y > 0 { index - width } else { index },
        if y + 1 < height { index + width } else { index },
    ]
}

fn fit_grid_corner_proposals(candidates: &[PatchCandidate]) -> Vec<GridFit> {
    use std::collections::HashSet;

    let search_candidates = bounded_grid_search_candidates(candidates);
    if search_candidates.len() < COLOR_CHECKER_PATCHES {
        return Vec::new();
    }
    let mut proposals = Vec::new();
    let mut seen_subsets = HashSet::new();
    for area_band in area_similar_bands(&search_candidates) {
        if area_band.len() < COLOR_CHECKER_PATCHES {
            continue;
        }
        for spatial_anchor in spatial_anchor_candidates(&area_band) {
            let mut selected = area_band.clone();
            selected.sort_by(|a, b| {
                squared_distance(a.center, spatial_anchor.center)
                    .total_cmp(&squared_distance(b.center, spatial_anchor.center))
                    .then_with(|| b.area.cmp(&a.area))
                    .then_with(|| a.id.cmp(&b.id))
            });
            selected.truncate(COLOR_CHECKER_PATCHES);
            let mut key = selected
                .iter()
                .map(|candidate| candidate.id)
                .collect::<Vec<_>>();
            key.sort_unstable();
            if !seen_subsets.insert(key) {
                continue;
            }
            if let Ok(fit) = fit_exact_grid_corners(&selected) {
                proposals.push(fit);
            }
        }
    }
    proposals.sort_by(grid_fit_ordering);
    proposals
}

fn fit_sparse_grid_corner_proposals(candidates: &[PatchCandidate]) -> Vec<GridFit> {
    use std::collections::HashSet;

    let search_candidates = bounded_grid_search_candidates(candidates);
    if search_candidates.len() < MIN_SPARSE_GRID_CANDIDATES {
        return Vec::new();
    }
    let mut proposals = Vec::new();
    let mut seen_subsets = HashSet::new();
    for area_band in area_similar_bands_with_min(&search_candidates, MIN_SPARSE_GRID_CANDIDATES) {
        if area_band.len() < MIN_SPARSE_GRID_CANDIDATES {
            continue;
        }
        for spatial_anchor in spatial_anchor_candidates(&area_band) {
            let mut selected = area_band.clone();
            selected.sort_by(|a, b| {
                squared_distance(a.center, spatial_anchor.center)
                    .total_cmp(&squared_distance(b.center, spatial_anchor.center))
                    .then_with(|| b.area.cmp(&a.area))
                    .then_with(|| a.id.cmp(&b.id))
            });
            let max_prefix = COLOR_CHECKER_PATCHES.min(selected.len());
            if max_prefix < MIN_SPARSE_GRID_CANDIDATES {
                continue;
            }
            for prefix_len in (MIN_SPARSE_GRID_CANDIDATES..=max_prefix).rev() {
                let prefix = &selected[..prefix_len];
                let mut key = prefix
                    .iter()
                    .map(|candidate| candidate.id)
                    .collect::<Vec<_>>();
                key.sort_unstable();
                if !seen_subsets.insert(key) {
                    continue;
                }
                if let Ok(fit) = fit_sparse_grid_corners(prefix) {
                    proposals.push(fit);
                }
            }
        }
    }
    proposals.sort_by(grid_fit_ordering);
    proposals
}

fn retain_diverse_grid_proposals(mut proposals: Vec<GridFit>) -> Vec<GridFit> {
    proposals.sort_by(grid_fit_ordering);
    if proposals.len() <= MAX_GRID_PROPOSALS {
        return proposals;
    }

    let (min_x, max_x, min_y, max_y) = grid_fit_center_bounds(&proposals);
    let bucket_count = PROPOSAL_SPATIAL_BUCKET_COLUMNS * PROPOSAL_SPATIAL_BUCKET_ROWS;
    let mut bucket_best = vec![None::<usize>; bucket_count];
    for (index, proposal) in proposals.iter().enumerate() {
        let center = grid_fit_center(proposal);
        let column = bucket_index(center.x, min_x, max_x, PROPOSAL_SPATIAL_BUCKET_COLUMNS);
        let row = bucket_index(center.y, min_y, max_y, PROPOSAL_SPATIAL_BUCKET_ROWS);
        let bucket = row * PROPOSAL_SPATIAL_BUCKET_COLUMNS + column;
        if bucket_best[bucket].is_none() {
            bucket_best[bucket] = Some(index);
        }
    }

    let mut retained_indices = bucket_best.into_iter().flatten().collect::<Vec<_>>();
    retained_indices.sort_by(|a, b| grid_fit_ordering(&proposals[*a], &proposals[*b]));
    retained_indices.truncate(MAX_GRID_PROPOSALS);

    let mut selected = vec![false; proposals.len()];
    for index in &retained_indices {
        selected[*index] = true;
    }
    for index in 0..proposals.len() {
        if retained_indices.len() == MAX_GRID_PROPOSALS {
            break;
        }
        if !selected[index] {
            retained_indices.push(index);
            selected[index] = true;
        }
    }

    let mut retained = retained_indices
        .into_iter()
        .map(|index| proposals[index])
        .collect::<Vec<_>>();
    retained.sort_by(grid_fit_ordering);
    retained
}

fn grid_fit_center_bounds(proposals: &[GridFit]) -> (f64, f64, f64, f64) {
    proposals.iter().fold(
        (
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ),
        |(min_x, max_x, min_y, max_y), proposal| {
            let center = grid_fit_center(proposal);
            (
                min_x.min(center.x),
                max_x.max(center.x),
                min_y.min(center.y),
                max_y.max(center.y),
            )
        },
    )
}

fn grid_fit_center(proposal: &GridFit) -> ColorImagePoint {
    mean_point(proposal.corners.into_iter())
}

fn grid_fit_ordering(a: &GridFit, b: &GridFit) -> std::cmp::Ordering {
    a.residual
        .total_cmp(&b.residual)
        .then_with(|| a.area_ratio.total_cmp(&b.area_ratio))
}

fn fit_sparse_grid_corners(selected: &[PatchCandidate]) -> Result<GridFit, String> {
    if selected.len() < MIN_SPARSE_GRID_CANDIDATES {
        return Err("not enough sparse patch candidates for 6x4 grid".to_owned());
    }
    let area_ratio = sparse_grid_area_ratio(selected)?;
    let rect = min_area_projection_rect(selected)?;
    let spacing_u = (rect.max_u - rect.min_u) / (COLOR_CHECKER_COLUMNS - 1) as f64;
    let spacing_v = (rect.max_v - rect.min_v) / (COLOR_CHECKER_ROWS - 1) as f64;
    if spacing_u <= f64::EPSILON || spacing_v <= f64::EPSILON {
        return Err("sparse grid spacing is degenerate".to_owned());
    }

    let mut occupied = [None::<(f64, ColorImagePoint)>; COLOR_CHECKER_PATCHES];
    for candidate in selected {
        let delta = sub(candidate.center, rect.center);
        let u = dot(delta, rect.axis_u);
        let v = dot(delta, rect.axis_v);
        let column = ((u - rect.min_u) / spacing_u).round() as isize;
        let row = ((v - rect.min_v) / spacing_v).round() as isize;
        if row < 0
            || row >= COLOR_CHECKER_ROWS as isize
            || column < 0
            || column >= COLOR_CHECKER_COLUMNS as isize
        {
            continue;
        }
        let expected_u = rect.min_u + column as f64 * spacing_u;
        let expected_v = rect.min_v + row as f64 * spacing_v;
        let error_u = (u - expected_u) / spacing_u;
        let error_v = (v - expected_v) / spacing_v;
        let cell_distance = (error_u * error_u + error_v * error_v).sqrt();
        if cell_distance > MAX_SPARSE_GRID_CELL_DISTANCE {
            continue;
        }
        let cell = row as usize * COLOR_CHECKER_COLUMNS + column as usize;
        if occupied[cell].is_none_or(|(best_distance, _)| cell_distance < best_distance) {
            occupied[cell] = Some((cell_distance, candidate.center));
        }
    }

    let observations = occupied
        .iter()
        .enumerate()
        .filter_map(|(cell, entry)| {
            entry.map(|(cell_distance, point)| {
                (
                    cell_distance,
                    GridObservation {
                        row: cell / COLOR_CHECKER_COLUMNS,
                        column: cell % COLOR_CHECKER_COLUMNS,
                        point,
                    },
                )
            })
        })
        .collect::<Vec<_>>();
    let occupied_count = observations.len();
    if occupied_count < MIN_SPARSE_GRID_CANDIDATES {
        return Err(format!(
            "sparse grid covers only {occupied_count} ColorChecker cells"
        ));
    }
    let row_count = (0..COLOR_CHECKER_ROWS)
        .filter(|row| {
            observations
                .iter()
                .any(|(_, observation)| observation.row == *row)
        })
        .count();
    if row_count < MIN_SPARSE_GRID_ROWS {
        return Err(format!("sparse grid covers only {row_count} row(s)"));
    }
    let column_count = (0..COLOR_CHECKER_COLUMNS)
        .filter(|column| {
            observations
                .iter()
                .any(|(_, observation)| observation.column == *column)
        })
        .count();
    if column_count < MIN_SPARSE_GRID_COLUMNS {
        return Err(format!("sparse grid covers only {column_count} column(s)"));
    }
    let squared_error = observations
        .iter()
        .map(|(cell_distance, _)| cell_distance * cell_distance)
        .sum::<f64>();
    let residual = (squared_error / occupied_count.max(1) as f64).sqrt();
    if residual > MAX_SPARSE_NORMALIZED_GRID_RESIDUAL {
        return Err(format!(
            "sparse 6x4 patch grid residual {residual:.3} is too high"
        ));
    }
    let observations = observations
        .into_iter()
        .map(|(_, observation)| observation)
        .collect::<Vec<_>>();
    let corners = fit_grid_outer_corners_from_observations(&observations)?;
    Ok(GridFit {
        corners,
        residual,
        area_ratio,
    })
}

fn sparse_grid_area_ratio(selected: &[PatchCandidate]) -> Result<f64, String> {
    let mut areas = selected
        .iter()
        .map(|candidate| candidate.area.max(1))
        .collect::<Vec<_>>();
    areas.sort_unstable();
    let min_area = f64::from(areas[0]);
    let median_area = f64::from(areas[areas.len() / 2]);
    let max_area = f64::from(*areas.last().expect("selected grid has areas"));
    if min_area < median_area * MIN_GRID_AREA_RATIO || max_area > median_area * MAX_GRID_AREA_RATIO
    {
        return Err(format!(
            "sparse patch area ratio {:.2} is too wide",
            max_area / min_area
        ));
    }
    Ok(max_area / min_area)
}

fn min_area_projection_rect(
    selected: &[PatchCandidate],
) -> Result<CandidateProjectionRect, String> {
    let center = mean_point(selected.iter().map(|candidate| candidate.center));
    let mut best: Option<(f64, CandidateProjectionRect)> = None;
    for (lhs_index, lhs) in selected.iter().enumerate() {
        for rhs in selected.iter().skip(lhs_index + 1) {
            let delta = sub(rhs.center, lhs.center);
            let length = norm(delta);
            if length <= 1.0 {
                continue;
            }
            let axis = scale(delta, 1.0 / length);
            let rect = candidate_projection_rect(selected, center, axis)?;
            let spread_u = rect.max_u - rect.min_u;
            let spread_v = rect.max_v - rect.min_v;
            if spread_u <= f64::EPSILON || spread_v <= f64::EPSILON {
                continue;
            }
            let aspect = spread_u / spread_v;
            if !(0.80..=3.50).contains(&aspect) {
                continue;
            }
            let ratio_penalty = (aspect / COLOR_CHECKER_CENTER_ASPECT).ln().abs();
            let score = spread_u * spread_v * (1.0 + ratio_penalty * 0.25);
            if best
                .as_ref()
                .is_none_or(|(best_score, _)| score < *best_score)
            {
                best = Some((score, rect));
            }
        }
    }
    best.map(|(_, rect)| rect)
        .ok_or_else(|| "could not fit a sparse ColorChecker projection rectangle".to_owned())
}

fn candidate_projection_rect(
    selected: &[PatchCandidate],
    center: ColorImagePoint,
    axis: ColorImagePoint,
) -> Result<CandidateProjectionRect, String> {
    let axis_v = ColorImagePoint::new(-axis.y, axis.x);
    let (min_u, max_u, min_v, max_v) = projection_bounds(selected, center, axis, axis_v);
    let spread_u = max_u - min_u;
    let spread_v = max_v - min_v;
    if spread_u >= spread_v {
        Ok(CandidateProjectionRect {
            center,
            axis_u: axis,
            axis_v,
            min_u,
            max_u,
            min_v,
            max_v,
        })
    } else {
        Ok(CandidateProjectionRect {
            center,
            axis_u: axis_v,
            axis_v: ColorImagePoint::new(-axis.x, -axis.y),
            min_u: min_v,
            max_u: max_v,
            min_v: -max_u,
            max_v: -min_u,
        })
    }
}

fn projection_bounds(
    selected: &[PatchCandidate],
    center: ColorImagePoint,
    axis_u: ColorImagePoint,
    axis_v: ColorImagePoint,
) -> (f64, f64, f64, f64) {
    selected.iter().fold(
        (
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ),
        |(min_u, max_u, min_v, max_v), candidate| {
            let delta = sub(candidate.center, center);
            let u = dot(delta, axis_u);
            let v = dot(delta, axis_v);
            (min_u.min(u), max_u.max(u), min_v.min(v), max_v.max(v))
        },
    )
}

fn bounded_grid_search_candidates(candidates: &[PatchCandidate]) -> Vec<PatchCandidate> {
    if candidates.len() <= MAX_GRID_SEARCH_CANDIDATES {
        return candidates.to_vec();
    }
    let (min_x, max_x, min_y, max_y) = candidate_center_bounds(candidates);
    let min_log_area = candidates
        .iter()
        .map(|candidate| area_log(candidate.area))
        .fold(f64::INFINITY, f64::min);
    let max_log_area = candidates
        .iter()
        .map(|candidate| area_log(candidate.area))
        .fold(f64::NEG_INFINITY, f64::max);
    let spatial_cells = SPATIAL_BUCKET_COLUMNS * SPATIAL_BUCKET_ROWS;
    let mut cells = vec![Vec::<PatchCandidate>::new(); AREA_BUCKET_COUNT * spatial_cells];
    for candidate in candidates {
        let area_bucket = bucket_index(
            area_log(candidate.area),
            min_log_area,
            max_log_area,
            AREA_BUCKET_COUNT,
        );
        let column = bucket_index(candidate.center.x, min_x, max_x, SPATIAL_BUCKET_COLUMNS);
        let row = bucket_index(candidate.center.y, min_y, max_y, SPATIAL_BUCKET_ROWS);
        let cell_index = area_bucket * spatial_cells + row * SPATIAL_BUCKET_COLUMNS + column;
        cells[cell_index].push(*candidate);
    }

    let mut per_area = vec![Vec::<PatchCandidate>::new(); AREA_BUCKET_COUNT];
    for area_bucket in 0..AREA_BUCKET_COUNT {
        let base = area_bucket * spatial_cells;
        for cell_offset in 0..spatial_cells {
            let cell = &mut cells[base + cell_offset];
            cell.sort_by(|a, b| b.area.cmp(&a.area).then_with(|| a.id.cmp(&b.id)));
            cell.truncate(MAX_CANDIDATES_PER_AREA_SPATIAL_BUCKET);
        }
        for rank in 0..MAX_CANDIDATES_PER_AREA_SPATIAL_BUCKET {
            for cell_offset in 0..spatial_cells {
                if let Some(candidate) = cells[base + cell_offset].get(rank) {
                    per_area[area_bucket].push(*candidate);
                }
            }
        }
    }

    use std::collections::HashSet;

    let mut retained = Vec::with_capacity(MAX_GRID_SEARCH_CANDIDATES);
    let mut retained_ids = HashSet::with_capacity(MAX_GRID_SEARCH_CANDIDATES);
    let per_bucket_quota = (MAX_GRID_SEARCH_CANDIDATES / AREA_BUCKET_COUNT)
        .max(COLOR_CHECKER_PATCHES)
        .max(1);
    for bucket in &per_area {
        let mut sampled = Vec::with_capacity(per_bucket_quota.min(bucket.len()));
        append_evenly_sampled(bucket, per_bucket_quota, &mut sampled);
        for candidate in sampled {
            if retained_ids.insert(candidate.id) {
                retained.push(candidate);
                if retained.len() == MAX_GRID_SEARCH_CANDIDATES {
                    retained.sort_by_key(|candidate| candidate.id);
                    return retained;
                }
            }
        }
    }

    let mut offsets = vec![0_usize; per_area.len()];
    while retained.len() < MAX_GRID_SEARCH_CANDIDATES {
        let mut progressed = false;
        for (bucket_index, bucket) in per_area.iter().enumerate() {
            while let Some(candidate) = bucket.get(offsets[bucket_index]).copied() {
                offsets[bucket_index] += 1;
                if retained_ids.insert(candidate.id) {
                    retained.push(candidate);
                    progressed = true;
                    break;
                }
            }
            if retained.len() == MAX_GRID_SEARCH_CANDIDATES {
                break;
            }
        }
        if !progressed {
            break;
        }
    }
    retained.sort_by_key(|candidate| candidate.id);
    retained
}

fn append_evenly_sampled<T: Copy>(candidates: &[T], count: usize, output: &mut Vec<T>) {
    if candidates.is_empty() || count == 0 {
        return;
    }
    if candidates.len() <= count {
        output.extend_from_slice(candidates);
        return;
    }
    if count == 1 {
        output.push(candidates[0]);
        return;
    }
    let last = candidates.len() - 1;
    let denominator = count - 1;
    let mut previous = None;
    for index in 0..count {
        let sample = index * last / denominator;
        if previous == Some(sample) {
            continue;
        }
        output.push(candidates[sample]);
        previous = Some(sample);
    }
}

fn area_similar_bands(candidates: &[PatchCandidate]) -> Vec<Vec<PatchCandidate>> {
    area_similar_bands_with_min(candidates, COLOR_CHECKER_PATCHES)
}

fn area_similar_bands_with_min(
    candidates: &[PatchCandidate],
    min_count: usize,
) -> Vec<Vec<PatchCandidate>> {
    use std::collections::HashSet;

    let mut sorted = candidates.to_vec();
    sorted.sort_by(|a, b| b.area.cmp(&a.area).then_with(|| a.id.cmp(&b.id)));
    let band_count = MAX_AREA_BANDS.min(sorted.len()).max(1);
    let mut bands = Vec::new();
    let mut seen_bands = HashSet::new();
    for band_index in 0..band_count {
        let anchor_index = if band_count == 1 {
            0
        } else {
            band_index * (sorted.len() - 1) / (band_count - 1)
        };
        let anchor_area = f64::from(sorted[anchor_index].area.max(1));
        let min_area = anchor_area * MIN_GRID_AREA_RATIO;
        let max_area = anchor_area * MAX_GRID_AREA_RATIO;
        let band = sorted
            .iter()
            .copied()
            .filter(|candidate| {
                let area = f64::from(candidate.area);
                area >= min_area && area <= max_area
            })
            .collect::<Vec<_>>();
        if band.len() < min_count {
            continue;
        }
        let mut key = band
            .iter()
            .map(|candidate| candidate.id)
            .collect::<Vec<_>>();
        key.sort_unstable();
        if seen_bands.insert(key) {
            bands.push(band);
        }
    }
    bands
}

fn spatial_anchor_candidates(area_band: &[PatchCandidate]) -> Vec<PatchCandidate> {
    if area_band.len() <= MAX_SPATIAL_ANCHORS_PER_AREA_BAND {
        return area_band.to_vec();
    }
    let (min_x, max_x, min_y, max_y) = candidate_center_bounds(area_band);
    let spatial_cells = SPATIAL_BUCKET_COLUMNS * SPATIAL_BUCKET_ROWS;
    let mut cells = vec![None::<PatchCandidate>; spatial_cells];
    for candidate in area_band {
        let column = bucket_index(candidate.center.x, min_x, max_x, SPATIAL_BUCKET_COLUMNS);
        let row = bucket_index(candidate.center.y, min_y, max_y, SPATIAL_BUCKET_ROWS);
        let cell_index = row * SPATIAL_BUCKET_COLUMNS + column;
        if cells[cell_index]
            .is_none_or(|current| candidate.area > current.area || candidate.id < current.id)
        {
            cells[cell_index] = Some(*candidate);
        }
    }
    let anchors = cells.into_iter().flatten().collect::<Vec<_>>();
    if anchors.len() <= MAX_SPATIAL_ANCHORS_PER_AREA_BAND {
        anchors
    } else {
        let mut retained = Vec::with_capacity(MAX_SPATIAL_ANCHORS_PER_AREA_BAND);
        append_evenly_sampled(&anchors, MAX_SPATIAL_ANCHORS_PER_AREA_BAND, &mut retained);
        retained
    }
}

fn candidate_center_bounds(candidates: &[PatchCandidate]) -> (f64, f64, f64, f64) {
    candidates.iter().fold(
        (
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ),
        |(min_x, max_x, min_y, max_y), candidate| {
            (
                min_x.min(candidate.center.x),
                max_x.max(candidate.center.x),
                min_y.min(candidate.center.y),
                max_y.max(candidate.center.y),
            )
        },
    )
}

fn area_log(area: u32) -> f64 {
    f64::from(area.max(1)).ln()
}

fn bucket_index(value: f64, min: f64, max: f64, bucket_count: usize) -> usize {
    if bucket_count <= 1 || !value.is_finite() || !min.is_finite() || !max.is_finite() {
        return 0;
    }
    let span = max - min;
    if span <= f64::EPSILON {
        return 0;
    }
    let normalized = ((value - min) / span).clamp(0.0, 1.0 - f64::EPSILON);
    (normalized * bucket_count as f64) as usize
}

fn fit_exact_grid_corners(selected: &[PatchCandidate]) -> Result<GridFit, String> {
    if selected.len() != COLOR_CHECKER_PATCHES {
        return Err("not enough patch candidates for 6x4 grid".to_owned());
    }
    let area_ratio = grid_area_ratio(selected)?;
    let center = mean_point(selected.iter().map(|candidate| candidate.center));
    let (mut axis_u, mut axis_v) = principal_axes(selected, center);
    if projection_spread(selected, center, axis_v) > projection_spread(selected, center, axis_u) {
        std::mem::swap(&mut axis_u, &mut axis_v);
    }
    let mut projected = selected
        .iter()
        .map(|candidate| {
            let delta = sub(candidate.center, center);
            (dot(delta, axis_u), dot(delta, axis_v), candidate.center)
        })
        .collect::<Vec<_>>();
    projected.sort_by(|a, b| a.1.total_cmp(&b.1));
    let mut rows = Vec::with_capacity(COLOR_CHECKER_ROWS);
    for row in 0..COLOR_CHECKER_ROWS {
        let start = row * COLOR_CHECKER_COLUMNS;
        let end = start + COLOR_CHECKER_COLUMNS;
        let mut items = projected[start..end]
            .iter()
            .map(|(u, _, point)| (*u, *point))
            .collect::<Vec<_>>();
        items.sort_by(|a, b| a.0.total_cmp(&b.0));
        rows.push(
            items
                .into_iter()
                .map(|(_, point)| point)
                .collect::<Vec<_>>(),
        );
    }
    let residual = normalized_grid_residual(&rows);
    if residual > MAX_NORMALIZED_GRID_RESIDUAL {
        return Err(format!(
            "normalized 6x4 patch grid residual {residual:.3} is too high"
        ));
    }
    let observations = grid_observations_from_rows(&rows);
    let corners = fit_grid_outer_corners_from_observations(&observations)?;
    Ok(GridFit {
        corners,
        residual,
        area_ratio,
    })
}
fn grid_area_ratio(selected: &[PatchCandidate]) -> Result<f64, String> {
    let mut areas = selected
        .iter()
        .map(|candidate| candidate.area.max(1))
        .collect::<Vec<_>>();
    areas.sort_unstable();
    let min_area = f64::from(areas[0]);
    let median_area = f64::from(areas[areas.len() / 2]);
    let max_area = f64::from(*areas.last().expect("selected grid has areas"));
    if min_area < median_area * MIN_GRID_AREA_RATIO || max_area > median_area * MAX_GRID_AREA_RATIO
    {
        return Err(format!(
            "patch area ratio {:.2} is too wide",
            max_area / min_area
        ));
    }
    Ok(max_area / min_area)
}

fn grid_observations_from_rows(rows: &[Vec<ColorImagePoint>]) -> Vec<GridObservation> {
    rows.iter()
        .enumerate()
        .flat_map(|(row, points)| {
            points
                .iter()
                .copied()
                .enumerate()
                .map(move |(column, point)| GridObservation { row, column, point })
        })
        .collect()
}

fn fit_grid_outer_corners_from_observations(
    observations: &[GridObservation],
) -> Result<[ColorImagePoint; 4], String> {
    if observations.len() < 8 {
        return Err("not enough grid observations for homography fit".to_owned());
    }
    let image_center = mean_point(observations.iter().map(|observation| observation.point));
    let max_radius = observations
        .iter()
        .map(|observation| norm(sub(observation.point, image_center)))
        .fold(0.0_f64, f64::max);
    let image_scale = max_radius.max(1.0);
    let coefficients = solve_grid_projective_coefficients(observations, image_center, image_scale)?;
    let residual = grid_homography_residual(observations, coefficients, image_center, image_scale)?;
    if residual > MAX_GRID_HOMOGRAPHY_RESIDUAL {
        return Err(format!(
            "grid homography residual {residual:.3} is too high"
        ));
    }
    let corners = [
        map_grid_homography(coefficients, image_center, image_scale, 0.0, 0.0)
            .ok_or_else(|| "grid homography top-left corner is invalid".to_owned())?,
        map_grid_homography(coefficients, image_center, image_scale, 1.0, 0.0)
            .ok_or_else(|| "grid homography top-right corner is invalid".to_owned())?,
        map_grid_homography(coefficients, image_center, image_scale, 1.0, 1.0)
            .ok_or_else(|| "grid homography bottom-right corner is invalid".to_owned())?,
        map_grid_homography(coefficients, image_center, image_scale, 0.0, 1.0)
            .ok_or_else(|| "grid homography bottom-left corner is invalid".to_owned())?,
    ];
    if corners
        .iter()
        .any(|corner| !corner.x.is_finite() || !corner.y.is_finite())
    {
        return Err("grid homography produced non-finite chart corners".to_owned());
    }
    if polygon_area(corners).abs() < 16.0 || !is_convex_quad(corners) {
        return Err("grid homography produced degenerate chart corners".to_owned());
    }
    Ok(corners)
}

fn solve_grid_projective_coefficients(
    observations: &[GridObservation],
    image_center: ColorImagePoint,
    image_scale: f64,
) -> Result<[f64; 8], String> {
    let mut normal = [[0.0_f64; 9]; 8];
    for observation in observations {
        let u = (observation.column as f64 + 0.5) / COLOR_CHECKER_COLUMNS as f64;
        let v = (observation.row as f64 + 0.5) / COLOR_CHECKER_ROWS as f64;
        let x = (observation.point.x - image_center.x) / image_scale;
        let y = (observation.point.y - image_center.y) / image_scale;
        if !u.is_finite() || !v.is_finite() || !x.is_finite() || !y.is_finite() {
            return Err("grid observation contains non-finite coordinates".to_owned());
        }
        let rows = [
            ([u, v, 1.0, 0.0, 0.0, 0.0, -x * u, -x * v], x),
            ([0.0, 0.0, 0.0, u, v, 1.0, -y * u, -y * v], y),
        ];
        for (row, target) in rows {
            for i in 0..8 {
                for j in 0..8 {
                    normal[i][j] += row[i] * row[j];
                }
                normal[i][8] += row[i] * target;
            }
        }
    }
    solve_augmented_8x8(normal).ok_or_else(|| "grid homography solve is degenerate".to_owned())
}

fn solve_augmented_8x8(mut matrix: [[f64; 9]; 8]) -> Option<[f64; 8]> {
    for pivot in 0..8 {
        let mut best_row = pivot;
        let mut best_value = matrix[pivot][pivot].abs();
        for (row, values) in matrix.iter().enumerate().skip(pivot + 1) {
            let value = values[pivot].abs();
            if value > best_value {
                best_row = row;
                best_value = value;
            }
        }
        if !best_value.is_finite() || best_value < 1.0e-10 {
            return None;
        }
        if best_row != pivot {
            matrix.swap(pivot, best_row);
        }
        let pivot_value = matrix[pivot][pivot];
        for column in pivot..9 {
            matrix[pivot][column] /= pivot_value;
        }
        for row in 0..8 {
            if row == pivot {
                continue;
            }
            let factor = matrix[row][pivot];
            if factor.abs() <= f64::EPSILON {
                continue;
            }
            for column in pivot..9 {
                matrix[row][column] -= factor * matrix[pivot][column];
            }
        }
    }
    let mut solution = [0.0_f64; 8];
    for row in 0..8 {
        solution[row] = matrix[row][8];
        if !solution[row].is_finite() {
            return None;
        }
    }
    Some(solution)
}

fn grid_homography_residual(
    observations: &[GridObservation],
    coefficients: [f64; 8],
    image_center: ColorImagePoint,
    image_scale: f64,
) -> Result<f64, String> {
    let spacing = observation_grid_spacing(observations).max(1.0);
    let mut squared = 0.0_f64;
    for observation in observations {
        let u = (observation.column as f64 + 0.5) / COLOR_CHECKER_COLUMNS as f64;
        let v = (observation.row as f64 + 0.5) / COLOR_CHECKER_ROWS as f64;
        let predicted = map_grid_homography(coefficients, image_center, image_scale, u, v)
            .ok_or_else(|| "grid homography mapped an observation behind infinity".to_owned())?;
        let error = norm(sub(predicted, observation.point)) / spacing;
        if !error.is_finite() {
            return Err("grid homography residual is non-finite".to_owned());
        }
        squared += error * error;
    }
    Ok((squared / observations.len().max(1) as f64).sqrt())
}

fn map_grid_homography(
    coefficients: [f64; 8],
    image_center: ColorImagePoint,
    image_scale: f64,
    u: f64,
    v: f64,
) -> Option<ColorImagePoint> {
    let [a, b, c, d, e, f, g, h] = coefficients;
    let denominator = g * u + h * v + 1.0;
    if !denominator.is_finite() || denominator.abs() < 1.0e-9 {
        return None;
    }
    let x = (a * u + b * v + c) / denominator;
    let y = (d * u + e * v + f) / denominator;
    if !x.is_finite() || !y.is_finite() {
        return None;
    }
    Some(ColorImagePoint::new(
        x * image_scale + image_center.x,
        y * image_scale + image_center.y,
    ))
}

fn observation_grid_spacing(observations: &[GridObservation]) -> f64 {
    let mut cells = [None::<ColorImagePoint>; COLOR_CHECKER_PATCHES];
    for observation in observations {
        cells[observation.row * COLOR_CHECKER_COLUMNS + observation.column] =
            Some(observation.point);
    }
    let mut distances = Vec::new();
    for row in 0..COLOR_CHECKER_ROWS {
        for column in 0..COLOR_CHECKER_COLUMNS {
            let Some(point) = cells[row * COLOR_CHECKER_COLUMNS + column] else {
                continue;
            };
            if column + 1 < COLOR_CHECKER_COLUMNS
                && let Some(next) = cells[row * COLOR_CHECKER_COLUMNS + column + 1]
            {
                distances.push(norm(sub(next, point)));
            }
            if row + 1 < COLOR_CHECKER_ROWS
                && let Some(next) = cells[(row + 1) * COLOR_CHECKER_COLUMNS + column]
            {
                distances.push(norm(sub(next, point)));
            }
        }
    }
    if distances.is_empty() {
        return 1.0;
    }
    distances.sort_by(f64::total_cmp);
    distances[distances.len() / 2]
}

fn mean_point(points: impl Iterator<Item = ColorImagePoint>) -> ColorImagePoint {
    let mut total = ColorImagePoint::new(0.0, 0.0);
    let mut count = 0.0_f64;
    for point in points {
        total = add(total, point);
        count += 1.0;
    }
    scale(total, 1.0 / count.max(1.0))
}

fn principal_axes(
    candidates: &[PatchCandidate],
    center: ColorImagePoint,
) -> (ColorImagePoint, ColorImagePoint) {
    let mut xx = 0.0;
    let mut xy = 0.0;
    let mut yy = 0.0;
    for candidate in candidates {
        let delta = sub(candidate.center, center);
        xx += delta.x * delta.x;
        xy += delta.x * delta.y;
        yy += delta.y * delta.y;
    }
    let angle = 0.5 * (2.0 * xy).atan2(xx - yy);
    let u = ColorImagePoint::new(angle.cos(), angle.sin());
    let v = ColorImagePoint::new(-u.y, u.x);
    (u, v)
}

fn projection_spread(
    candidates: &[PatchCandidate],
    center: ColorImagePoint,
    axis: ColorImagePoint,
) -> f64 {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for candidate in candidates {
        let value = dot(sub(candidate.center, center), axis);
        min = min.min(value);
        max = max.max(value);
    }
    max - min
}

fn normalized_grid_residual(rows: &[Vec<ColorImagePoint>]) -> f64 {
    let spacing = median_adjacent_patch_spacing(rows).max(1.0);
    grid_residual_pixels(rows) / spacing
}

fn grid_residual_pixels(rows: &[Vec<ColorImagePoint>]) -> f64 {
    let top_left = rows[0][0];
    let top_right = rows[0][COLOR_CHECKER_COLUMNS - 1];
    let bottom_left = rows[COLOR_CHECKER_ROWS - 1][0];
    let bottom_right = rows[COLOR_CHECKER_ROWS - 1][COLOR_CHECKER_COLUMNS - 1];
    let mut squared = 0.0;
    let mut count = 0.0;
    for (row_index, row) in rows.iter().enumerate() {
        let v = row_index as f64 / (COLOR_CHECKER_ROWS - 1) as f64;
        for (column_index, actual) in row.iter().copied().enumerate() {
            let u = column_index as f64 / (COLOR_CHECKER_COLUMNS - 1) as f64;
            let expected = bilerp(top_left, top_right, bottom_right, bottom_left, u, v);
            let error = norm(sub(actual, expected));
            squared += error * error;
            count += 1.0;
        }
    }
    (squared / count).sqrt()
}

fn median_adjacent_patch_spacing(rows: &[Vec<ColorImagePoint>]) -> f64 {
    let mut distances = Vec::with_capacity(
        COLOR_CHECKER_ROWS * (COLOR_CHECKER_COLUMNS - 1)
            + (COLOR_CHECKER_ROWS - 1) * COLOR_CHECKER_COLUMNS,
    );
    for row in rows {
        for pair in row.windows(2) {
            distances.push(norm(sub(pair[1], pair[0])));
        }
    }
    for row_pair in rows.windows(2) {
        for column in 0..COLOR_CHECKER_COLUMNS {
            distances.push(norm(sub(row_pair[1][column], row_pair[0][column])));
        }
    }
    distances.sort_by(f64::total_cmp);
    distances[distances.len() / 2]
}

fn canonicalize_corners(points: [ColorImagePoint; 4]) -> Result<[ColorImagePoint; 4], String> {
    let center = mean_point(points.into_iter());
    let mut ordered = points;
    ordered.sort_by(|a, b| {
        (a.y - center.y)
            .atan2(a.x - center.x)
            .total_cmp(&(b.y - center.y).atan2(b.x - center.x))
    });
    if polygon_area(ordered).abs() < 16.0 {
        return Err("chart corners enclose too little area".to_owned());
    }
    if !is_convex_quad(ordered) {
        return Err(
            "chart corners must form a convex non-self-intersecting quadrilateral".to_owned(),
        );
    }
    Ok(ordered)
}

fn corner_orientation_variants(base: [ColorImagePoint; 4]) -> Vec<[ColorImagePoint; 4]> {
    let mut variants = Vec::with_capacity(8);
    for start in 0..4 {
        variants.push([
            base[start],
            base[(start + 1) % 4],
            base[(start + 2) % 4],
            base[(start + 3) % 4],
        ]);
        variants.push([
            base[start],
            base[(start + 3) % 4],
            base[(start + 2) % 4],
            base[(start + 1) % 4],
        ]);
    }
    variants
}

fn sample_projective_chart_patches(
    frame: &Rgba8Frame,
    homography: Homography,
    references: &[ColorPatchReference; COLOR_CHECKER_PATCHES],
    patch_edge_inset_percent: f32,
) -> Result<Vec<ChartPatchMeasurement>, String> {
    let inset = f64::from(clamped_patch_edge_inset_percent(patch_edge_inset_percent) / 100.0);
    let mut patches = Vec::with_capacity(COLOR_CHECKER_PATCHES);
    for row in 0..COLOR_CHECKER_ROWS {
        for column in 0..COLOR_CHECKER_COLUMNS {
            let index = row * COLOR_CHECKER_COLUMNS + column;
            let u0 = (column as f64 + inset) / COLOR_CHECKER_COLUMNS as f64;
            let u1 = (column as f64 + 1.0 - inset) / COLOR_CHECKER_COLUMNS as f64;
            let v0 = (row as f64 + inset) / COLOR_CHECKER_ROWS as f64;
            let v1 = (row as f64 + 1.0 - inset) / COLOR_CHECKER_ROWS as f64;
            let polygon = [
                homography.map(u0, v0),
                homography.map(u1, v0),
                homography.map(u1, v1),
                homography.map(u0, v1),
            ];
            let polygon = [
                polygon[0].ok_or_else(|| format!("patch {} corner is invalid", index + 1))?,
                polygon[1].ok_or_else(|| format!("patch {} corner is invalid", index + 1))?,
                polygon[2].ok_or_else(|| format!("patch {} corner is invalid", index + 1))?,
                polygon[3].ok_or_else(|| format!("patch {} corner is invalid", index + 1))?,
            ];
            let center = homography
                .map(
                    (column as f64 + 0.5) / COLOR_CHECKER_COLUMNS as f64,
                    (row as f64 + 0.5) / COLOR_CHECKER_ROWS as f64,
                )
                .ok_or_else(|| format!("patch {} center is invalid", index + 1))?;
            let roi = bounding_roi(&polygon, frame.width, frame.height)
                .ok_or_else(|| format!("patch {} sampling polygon is outside image", index + 1))?;
            let measured = mean_rgb_polygon(frame, polygon, roi)
                .ok_or_else(|| format!("patch {} has no valid samples", index + 1))?;
            let reference = references[index];
            let measured_lab = srgb_to_lab(measured);
            let reference_lab = reference.comparison_lab;
            let reference_chroma = lab_chroma(reference_lab);
            let measured_chroma = lab_chroma(measured_lab);
            let camera_chroma_percent = if reference_chroma > f64::EPSILON {
                100.0 * measured_chroma / reference_chroma
            } else {
                0.0
            };
            let delta_c = delta_c(measured_lab, reference_lab);
            let delta_e = delta_e(measured_lab, reference_lab);
            let hsv_saturation = hsv_saturation(measured);
            let exposure_error_stops = exposure_error_stops(measured, reference_lab);
            patches.push(ChartPatchMeasurement {
                index,
                name: reference.name,
                reference_sample_name: reference.sample_name,
                reference_srgb: reference.display_srgb,
                measured_srgb: measured,
                reference_source_lab: reference.source_lab,
                reference_lab,
                measured_lab,
                reference_chroma,
                measured_chroma,
                camera_chroma_percent,
                delta_c,
                delta_e,
                hsv_saturation,
                exposure_error_stops,
                roi,
                center,
                polygon,
            });
        }
    }
    Ok(patches)
}

fn mean_rgb_polygon(
    frame: &Rgba8Frame,
    polygon: [ColorImagePoint; 4],
    roi: Roi,
) -> Option<[u8; 3]> {
    let mut total = [0_u64; 3];
    let mut count = 0_u64;
    let y_end = roi.y.saturating_add(roi.height).min(frame.height);
    let x_end = roi.x.saturating_add(roi.width).min(frame.width);
    for y in roi.y..y_end {
        for x in roi.x..x_end {
            let point = ColorImagePoint::new(f64::from(x) + 0.5, f64::from(y) + 0.5);
            if !point_in_convex_quad(point, polygon) {
                continue;
            }
            let [r, g, b, a] = frame.pixel(x, y).expect("polygon ROI was clamped to image");
            if a == 0 {
                continue;
            }
            total[0] = total[0].saturating_add(u64::from(r));
            total[1] = total[1].saturating_add(u64::from(g));
            total[2] = total[2].saturating_add(u64::from(b));
            count = count.saturating_add(1);
        }
    }
    (count > 0).then(|| {
        [
            (total[0] / count) as u8,
            (total[1] / count) as u8,
            (total[2] / count) as u8,
        ]
    })
}

fn orientation_score(patches: &[ChartPatchMeasurement]) -> f64 {
    let gray = &patches[18..24];
    let measured_gray = gray.iter().fold([0.0_f64; 3], |mut total, patch| {
        total[0] += f64::from(patch.measured_srgb[0]);
        total[1] += f64::from(patch.measured_srgb[1]);
        total[2] += f64::from(patch.measured_srgb[2]);
        total
    });
    let reference_gray = gray.iter().fold([0.0_f64; 3], |mut total, patch| {
        total[0] += f64::from(patch.reference_srgb[0]);
        total[1] += f64::from(patch.reference_srgb[1]);
        total[2] += f64::from(patch.reference_srgb[2]);
        total
    });
    let inv_gray = 1.0 / gray.len() as f64;
    let scales = [
        reference_gray[0] * inv_gray / (measured_gray[0] * inv_gray).max(1.0),
        reference_gray[1] * inv_gray / (measured_gray[1] * inv_gray).max(1.0),
        reference_gray[2] * inv_gray / (measured_gray[2] * inv_gray).max(1.0),
    ];
    let mut score = 0.0;
    for patch in patches {
        let measured = [
            f64::from(patch.measured_srgb[0]) * scales[0],
            f64::from(patch.measured_srgb[1]) * scales[1],
            f64::from(patch.measured_srgb[2]) * scales[2],
        ];
        let reference = patch.reference_srgb.map(f64::from);
        let measured_chroma = chromaticity(measured);
        let reference_chroma = chromaticity(reference);
        score += (measured_chroma[0] - reference_chroma[0]).abs();
        score += (measured_chroma[1] - reference_chroma[1]).abs();
        score += (measured_chroma[2] - reference_chroma[2]).abs();
    }
    for pair in gray.windows(2) {
        let first = luma(pair[0].measured_srgb);
        let second = luma(pair[1].measured_srgb);
        if second > first {
            score += (second - first) / 16.0;
        }
    }
    score
}

fn chromaticity(rgb: [f64; 3]) -> [f64; 3] {
    let sum = (rgb[0] + rgb[1] + rgb[2]).max(1.0);
    [rgb[0] / sum, rgb[1] / sum, rgb[2] / sum]
}

fn luma(rgb: [u8; 3]) -> f64 {
    0.2126 * f64::from(rgb[0]) + 0.7152 * f64::from(rgb[1]) + 0.0722 * f64::from(rgb[2])
}

fn bounding_roi(points: &[ColorImagePoint], width: u32, height: u32) -> Option<Roi> {
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for point in points {
        min_x = min_x.min(point.x);
        min_y = min_y.min(point.y);
        max_x = max_x.max(point.x);
        max_y = max_y.max(point.y);
    }
    Roi {
        x: min_x.floor().max(0.0) as u32,
        y: min_y.floor().max(0.0) as u32,
        width: (max_x.ceil() - min_x.floor()).max(1.0) as u32,
        height: (max_y.ceil() - min_y.floor()).max(1.0) as u32,
    }
    .clamped_to(width, height)
}

fn point_in_convex_quad(point: ColorImagePoint, polygon: [ColorImagePoint; 4]) -> bool {
    let mut positive = false;
    let mut negative = false;
    for index in 0..4 {
        let edge = sub(polygon[(index + 1) % 4], polygon[index]);
        let to_point = sub(point, polygon[index]);
        let cross = cross(edge, to_point);
        positive |= cross > 1.0e-6;
        negative |= cross < -1.0e-6;
        if positive && negative {
            return false;
        }
    }
    true
}

fn is_convex_quad(points: [ColorImagePoint; 4]) -> bool {
    point_in_convex_quad(points[0], points)
        && polygon_area(points).abs() > 16.0
        && !segments_intersect(points[0], points[1], points[2], points[3])
        && !segments_intersect(points[1], points[2], points[3], points[0])
}

fn segments_intersect(
    a0: ColorImagePoint,
    a1: ColorImagePoint,
    b0: ColorImagePoint,
    b1: ColorImagePoint,
) -> bool {
    let da = sub(a1, a0);
    let db = sub(b1, b0);
    let denominator = cross(da, db);
    if denominator.abs() < 1.0e-9 {
        return false;
    }
    let delta = sub(b0, a0);
    let t = cross(delta, db) / denominator;
    let u = cross(delta, da) / denominator;
    (0.0..=1.0).contains(&t) && (0.0..=1.0).contains(&u)
}

fn polygon_area(points: [ColorImagePoint; 4]) -> f64 {
    let mut area = 0.0;
    for index in 0..4 {
        let a = points[index];
        let b = points[(index + 1) % 4];
        area += a.x * b.y - b.x * a.y;
    }
    area * 0.5
}

fn add(a: ColorImagePoint, b: ColorImagePoint) -> ColorImagePoint {
    ColorImagePoint::new(a.x + b.x, a.y + b.y)
}

fn sub(a: ColorImagePoint, b: ColorImagePoint) -> ColorImagePoint {
    ColorImagePoint::new(a.x - b.x, a.y - b.y)
}

fn scale(point: ColorImagePoint, factor: f64) -> ColorImagePoint {
    ColorImagePoint::new(point.x * factor, point.y * factor)
}

fn dot(a: ColorImagePoint, b: ColorImagePoint) -> f64 {
    a.x * b.x + a.y * b.y
}

fn cross(a: ColorImagePoint, b: ColorImagePoint) -> f64 {
    a.x * b.y - a.y * b.x
}

fn norm(point: ColorImagePoint) -> f64 {
    dot(point, point).sqrt()
}

fn squared_distance(a: ColorImagePoint, b: ColorImagePoint) -> f64 {
    let delta = sub(a, b);
    dot(delta, delta)
}

fn bilerp(
    top_left: ColorImagePoint,
    top_right: ColorImagePoint,
    bottom_right: ColorImagePoint,
    bottom_left: ColorImagePoint,
    u: f64,
    v: f64,
) -> ColorImagePoint {
    add(
        add(
            scale(top_left, (1.0 - u) * (1.0 - v)),
            scale(top_right, u * (1.0 - v)),
        ),
        add(
            scale(bottom_right, u * v),
            scale(bottom_left, (1.0 - u) * v),
        ),
    )
}

fn color_checker_references(
    chart_kind: ColorChartKind,
) -> [ColorPatchReference; COLOR_CHECKER_PATCHES] {
    const PATCH_NAMES: [(&str, &str); COLOR_CHECKER_PATCHES] = [
        ("A1", "Dark Skin"),
        ("B1", "Light Skin"),
        ("C1", "Blue Sky"),
        ("D1", "Foliage"),
        ("E1", "Blue Flower"),
        ("F1", "Bluish Green"),
        ("A2", "Orange"),
        ("B2", "Purplish Blue"),
        ("C2", "Moderate Red"),
        ("D2", "Purple"),
        ("E2", "Yellow Green"),
        ("F2", "Orange Yellow"),
        ("A3", "Blue"),
        ("B3", "Green"),
        ("C3", "Red"),
        ("D3", "Yellow"),
        ("E3", "Magenta"),
        ("F3", "Cyan"),
        ("A4", "White"),
        ("B4", "Neutral 8"),
        ("C4", "Neutral 6.5"),
        ("D4", "Neutral 5"),
        ("E4", "Neutral 3.5"),
        ("F4", "Black"),
    ];
    const BEFORE_NOV_2014_LAB_D50: [LabColor; COLOR_CHECKER_PATCHES] = [
        lab(37.986, 13.555, 14.059),
        lab(65.711, 18.130, 17.810),
        lab(49.927, -4.880, -21.905),
        lab(43.139, -13.095, 21.905),
        lab(55.112, 8.844, -25.399),
        lab(70.719, -33.397, -0.199),
        lab(62.661, 36.067, 57.096),
        lab(40.020, 10.410, -45.964),
        lab(51.124, 48.239, 16.248),
        lab(30.325, 22.976, -21.587),
        lab(72.532, -23.709, 57.255),
        lab(71.941, 19.363, 67.857),
        lab(28.778, 14.179, -50.297),
        lab(55.261, -38.342, 31.370),
        lab(42.101, 53.378, 28.190),
        lab(81.733, 4.039, 79.819),
        lab(51.935, 49.986, -14.574),
        lab(51.038, -28.631, -28.638),
        lab(96.539, -0.425, 1.186),
        lab(81.257, -0.638, -0.335),
        lab(66.766, -0.734, -0.504),
        lab(50.867, -0.153, -0.270),
        lab(35.656, -0.421, -1.231),
        lab(20.461, -0.079, -0.973),
    ];
    const NOV_2014_AND_NEWER_LAB_D50: [LabColor; COLOR_CHECKER_PATCHES] = [
        lab(37.540, 14.370, 14.920),
        lab(64.660, 19.270, 17.500),
        lab(49.320, -3.820, -22.540),
        lab(43.460, -12.740, 22.720),
        lab(54.940, 9.610, -24.790),
        lab(70.480, -32.260, -0.370),
        lab(62.730, 35.830, 56.500),
        lab(39.430, 10.750, -45.170),
        lab(50.570, 48.640, 16.670),
        lab(30.100, 22.540, -20.870),
        lab(71.770, -24.130, 58.190),
        lab(71.510, 18.240, 67.370),
        lab(28.370, 15.420, -49.800),
        lab(54.380, -39.720, 32.270),
        lab(42.430, 51.050, 28.620),
        lab(81.800, 2.670, 80.410),
        lab(50.630, 51.280, -14.120),
        lab(49.570, -29.710, -28.320),
        lab(95.190, -1.030, 2.930),
        lab(81.290, -0.570, 0.440),
        lab(66.890, -0.750, -0.060),
        lab(50.760, -0.130, 0.140),
        lab(35.630, -0.460, -0.480),
        lab(20.640, 0.070, -0.460),
    ];

    let source_labs = match chart_kind {
        ColorChartKind::ColorChecker24BeforeNov2014 => BEFORE_NOV_2014_LAB_D50,
        ColorChartKind::ColorChecker24Nov2014AndNewer => NOV_2014_AND_NEWER_LAB_D50,
    };
    std::array::from_fn(|index| {
        let (sample_name, name) = PATCH_NAMES[index];
        let source_lab = source_labs[index];
        ColorPatchReference {
            name,
            sample_name,
            display_srgb: lab_d50_to_display_srgb(source_lab),
            source_lab,
            comparison_lab: lab_d50_to_lab_d65(source_lab),
        }
    })
}

const fn lab(l: f64, a: f64, b: f64) -> LabColor {
    LabColor { l, a, b }
}

fn srgb_to_lab(srgb: [u8; 3]) -> LabColor {
    xyz_to_lab(srgb_to_xyz_d65(srgb), D65_WHITE)
}

fn srgb_to_xyz_d65(srgb: [u8; 3]) -> [f64; 3] {
    let r = srgb_channel_to_linear(srgb[0]);
    let g = srgb_channel_to_linear(srgb[1]);
    let b = srgb_channel_to_linear(srgb[2]);
    [
        0.412_456_4 * r + 0.357_576_1 * g + 0.180_437_5 * b,
        0.212_672_9 * r + 0.715_152_2 * g + 0.072_175 * b,
        0.019_333_9 * r + 0.119_192 * g + 0.950_304_1 * b,
    ]
}

fn lab_d50_to_lab_d65(lab: LabColor) -> LabColor {
    let xyz_d50 = lab_to_xyz(lab, D50_WHITE);
    xyz_to_lab(adapt_xyz_d50_to_d65(xyz_d50), D65_WHITE)
}

fn lab_d50_to_display_srgb(lab: LabColor) -> [u8; 3] {
    let xyz_d50 = lab_to_xyz(lab, D50_WHITE);
    xyz_d65_to_srgb(adapt_xyz_d50_to_d65(xyz_d50))
}

fn lab_d65_to_display_srgb(lab: LabColor) -> [u8; 3] {
    xyz_d65_to_srgb(lab_to_xyz(lab, D65_WHITE))
}

fn lab_to_xyz(lab: LabColor, white: [f64; 3]) -> [f64; 3] {
    let fy = (lab.l + 16.0) / 116.0;
    let fx = fy + lab.a / 500.0;
    let fz = fy - lab.b / 200.0;
    [
        white[0] * lab_inverse_f(fx),
        white[1] * lab_inverse_f(fy),
        white[2] * lab_inverse_f(fz),
    ]
}

fn xyz_to_lab(xyz: [f64; 3], white: [f64; 3]) -> LabColor {
    let fx = lab_f(xyz[0] / white[0]);
    let fy = lab_f(xyz[1] / white[1]);
    let fz = lab_f(xyz[2] / white[2]);
    LabColor {
        l: 116.0 * fy - 16.0,
        a: 500.0 * (fx - fy),
        b: 200.0 * (fy - fz),
    }
}

fn adapt_xyz_d50_to_d65(xyz: [f64; 3]) -> [f64; 3] {
    [
        0.955_576_6 * xyz[0] - 0.023_039_3 * xyz[1] + 0.063_163_6 * xyz[2],
        -0.028_289_5 * xyz[0] + 1.009_941_6 * xyz[1] + 0.021_007_7 * xyz[2],
        0.012_298_2 * xyz[0] - 0.020_483_0 * xyz[1] + 1.329_909_8 * xyz[2],
    ]
}

fn xyz_d65_to_srgb(xyz: [f64; 3]) -> [u8; 3] {
    [
        linear_to_srgb_channel(3.240_454_2 * xyz[0] - 1.537_138_5 * xyz[1] - 0.498_531_4 * xyz[2]),
        linear_to_srgb_channel(-0.969_266_0 * xyz[0] + 1.876_010_8 * xyz[1] + 0.041_556_0 * xyz[2]),
        linear_to_srgb_channel(0.055_643_4 * xyz[0] - 0.204_025_9 * xyz[1] + 1.057_225_2 * xyz[2]),
    ]
}

fn srgb_channel_to_linear(value: u8) -> f64 {
    let channel = f64::from(value) / 255.0;
    if channel <= 0.040_45 {
        channel / 12.92
    } else {
        ((channel + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb_channel(channel: f64) -> u8 {
    let channel = channel.clamp(0.0, 1.0);
    let encoded = if channel <= 0.003_130_8 {
        12.92 * channel
    } else {
        1.055 * channel.powf(1.0 / 2.4) - 0.055
    };
    (encoded * 255.0).round().clamp(0.0, 255.0) as u8
}

fn lab_f(value: f64) -> f64 {
    if value > 0.008_856 {
        value.cbrt()
    } else {
        7.787 * value + 16.0 / 116.0
    }
}

fn lab_inverse_f(value: f64) -> f64 {
    let cube = value * value * value;
    if cube > 0.008_856 {
        cube
    } else {
        (value - 16.0 / 116.0) / 7.787
    }
}

fn delta_e(actual: LabColor, expected: LabColor) -> f64 {
    let dl = actual.l - expected.l;
    let da = actual.a - expected.a;
    let db = actual.b - expected.b;
    (dl * dl + da * da + db * db).sqrt()
}

fn delta_c(actual: LabColor, expected: LabColor) -> f64 {
    let da = actual.a - expected.a;
    let db = actual.b - expected.b;
    (da * da + db * db).sqrt()
}

fn lab_chroma(lab: LabColor) -> f64 {
    (lab.a * lab.a + lab.b * lab.b).sqrt()
}

fn hsv_saturation(rgb: [u8; 3]) -> f64 {
    let [r, g, b] = rgb.map(|channel| f64::from(channel) / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    if max <= f64::EPSILON {
        0.0
    } else {
        (max - min) / max
    }
}

fn exposure_error_stops(measured_srgb: [u8; 3], reference_lab: LabColor) -> f64 {
    let measured_y = srgb_to_xyz_d65(measured_srgb)[1].max(1.0e-9);
    let reference_y = lab_to_xyz(reference_lab, D65_WHITE)[1].max(1.0e-9);
    (measured_y / reference_y).log2()
}

fn export_payload(analysis: &ColorChartAnalysis) -> serde_json::Value {
    let reference = analysis.chart_kind.reference_metadata();
    json!({
        "schema": "camera-toolbox/color-inspection/v1",
        "input": {
            "document_id": analysis.input.document_id.to_string(),
            "generation": analysis.input.generation,
            "label": analysis.input_label,
            "width": analysis.image_size[0],
            "height": analysis.image_size[1],
        },
        "configuration": {
            "light_source": analysis.light_source.label(),
            "chart_kind": {
                "id": analysis.chart_kind.id(),
                "label": analysis.chart_kind.label(),
            },
            "reference": reference_metadata_json(reference),
            "comparison_lab_white_point": "D65",
            "chromatic_adaptation": "Bradford D50→D65",
            "patch_edge_inset_percent": analysis.patch_edge_inset_percent,
            "patch_sample_fraction": patch_sample_fraction(analysis.patch_edge_inset_percent),
        },
        "detection": {
            "mode": analysis.detection_mode.label(),
            "chart_roi": roi_json(analysis.chart_roi),
            "chart_corners": analysis.chart_corners.map(point_json),
            "layout": {"columns": COLOR_CHECKER_COLUMNS, "rows": COLOR_CHECKER_ROWS},
        },
        "metrics": metrics_report_payload(analysis),
        "patches": analysis.patches.iter().map(|patch| {
            json!({
                "index": patch.index + 1,
                "sample_name": patch.reference_sample_name,
                "name": patch.name,
                "roi": roi_json(patch.roi),
                "center": point_json(patch.center),
                "polygon": patch.polygon.map(point_json),
                "reference_display_srgb": patch.reference_srgb,
                "measured_srgb": patch.measured_srgb,
                "reference_lab_d50_authoritative": lab_json(patch.reference_source_lab),
                "reference_lab_d65_comparison": lab_json(patch.reference_lab),
                "measured_lab_d65": lab_json(patch.measured_lab),
                "reference_chroma_d65": patch.reference_chroma,
                "measured_chroma_d65": patch.measured_chroma,
                "camera_chroma_percent": patch.camera_chroma_percent,
                "delta_c_d65": patch.delta_c,
                "delta_e_d65": patch.delta_e,
                "hsv_saturation": patch.hsv_saturation,
                "exposure_error_stops": patch.exposure_error_stops,
            })
        }).collect::<Vec<_>>(),
    })
}

fn metrics_report_payload(analysis: &ColorChartAnalysis) -> serde_json::Value {
    json!({
        "mean_delta_c_d65": analysis.mean_delta_c,
        "max_delta_c_d65": analysis.max_delta_c,
        "mean_delta_e_d65": analysis.mean_delta_e,
        "max_delta_e_d65": analysis.max_delta_e,
        "mean_camera_chroma_percent": analysis.mean_camera_chroma_percent,
        "gray_mean_rgb": analysis.gray_mean_rgb,
        "gray_rg_ratio": analysis.gray_rg_ratio,
        "gray_bg_ratio": analysis.gray_bg_ratio,
        "gray_balance_error": analysis.gray_balance_error,
        "gray_mean_delta_c_d65": analysis.gray_mean_delta_c,
        "gray_max_delta_c_d65": analysis.gray_max_delta_c,
        "gray_mean_hsv_saturation": analysis.gray_mean_hsv_saturation,
        "gray_max_hsv_saturation": analysis.gray_max_hsv_saturation,
        "gray_mean_exposure_error_stops": analysis.gray_mean_exposure_error_stops,
        "gray_max_abs_exposure_error_stops": analysis.gray_max_abs_exposure_error_stops,
    })
}

fn roi_json(roi: Roi) -> serde_json::Value {
    json!({"x": roi.x, "y": roi.y, "width": roi.width, "height": roi.height})
}

fn point_json(point: ColorImagePoint) -> serde_json::Value {
    json!({"x": point.x, "y": point.y})
}

fn lab_json(lab: LabColor) -> serde_json::Value {
    json!({"l": lab.l, "a": lab.a, "b": lab.b})
}

fn reference_metadata_json(metadata: ColorReferenceMetadata) -> serde_json::Value {
    json!({
        "id": metadata.id,
        "chart_name": metadata.chart_name,
        "manufacturer": metadata.manufacturer,
        "formulation": metadata.formulation,
        "white_point": metadata.white_point,
        "observer": metadata.observer,
        "measurement_geometry": metadata.measurement_geometry,
        "measurement_condition": metadata.measurement_condition,
        "source_name": metadata.source_name,
        "source_url": metadata.source_url,
        "source_data_url": metadata.source_data_url,
    })
}

fn render_color_accuracy(
    ui: &mut egui::Ui,
    analysis: &ColorChartAnalysis,
    view: &mut LabChartView,
    selected_patch: &mut Option<usize>,
) {
    ui.heading("Color Accuracy");
    render_lab_ab_chart(ui, analysis, view, selected_patch);
    ui.label(format!(
        "Delta C: mean {:.2}, max {:.2}",
        analysis.mean_delta_c, analysis.max_delta_c
    ));
    ui.label(format!(
        "Delta E: mean {:.2}, max {:.2}",
        analysis.mean_delta_e, analysis.max_delta_e
    ));
    ui.label(format!(
        "Saturation: {:.1}%",
        analysis.mean_camera_chroma_percent
    ));
}

fn render_lab_ab_chart(
    ui: &mut egui::Ui,
    analysis: &ColorChartAnalysis,
    view: &mut LabChartView,
    selected_patch: &mut Option<usize>,
) {
    if !view.initialized {
        initialize_lab_chart_view(view, analysis);
    }
    let height = 300.0;
    let width = ui.available_width().max(1.0);
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::click_and_drag());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, egui::Color32::from_gray(18));
    let plot = rect.shrink2(egui::vec2(44.0, 30.0));

    if response.dragged_by(egui::PointerButton::Primary)
        && response
            .interact_pointer_pos()
            .is_some_and(|position| plot.contains(position))
    {
        let delta = response.drag_delta();
        view.center_a -= f64::from(delta.x) * 2.0 * view.half_a / f64::from(plot.width().max(1.0));
        view.center_b += f64::from(delta.y) * 2.0 * view.half_b / f64::from(plot.height().max(1.0));
        clamp_lab_chart_view(view);
    }

    if response.hovered()
        && let Some(pointer) = response.hover_pos()
        && plot.contains(pointer)
    {
        let scroll_y = ui.input(|input| input.smooth_scroll_delta().y);
        if scroll_y.abs() > f32::EPSILON {
            let (pointer_a, pointer_b) = plot_to_lab_ab(plot, pointer, view);
            let x_fraction = f64::from((pointer.x - plot.left()) / plot.width().max(1.0));
            let y_fraction = f64::from((plot.bottom() - pointer.y) / plot.height().max(1.0));
            let factor = (scroll_y * 0.0015).exp();
            view.half_a = (view.half_a / f64::from(factor))
                .clamp(LAB_CHART_MIN_HALF_RANGE, full_lab_half_range());
            view.half_b = (view.half_b / f64::from(factor))
                .clamp(LAB_CHART_MIN_HALF_RANGE, full_lab_half_range());
            view.center_a = pointer_a - (x_fraction - 0.5) * 2.0 * view.half_a;
            view.center_b = pointer_b - (y_fraction - 0.5) * 2.0 * view.half_b;
            clamp_lab_chart_view(view);
        }
    }

    paint_lab_background(&painter, plot, view);
    paint_lab_grid_axes(ui, &painter, plot, view);

    if response.clicked_by(egui::PointerButton::Primary)
        && let Some(position) = response.interact_pointer_pos()
        && plot.contains(position)
        && let Some(index) = nearest_lab_patch_index(analysis, plot, view, position)
    {
        *selected_patch = Some(index);
    }

    for patch in &analysis.patches {
        let selected = *selected_patch == Some(patch.index);
        let reference = lab_to_plot(plot, patch.reference_lab, view);
        let measured = lab_to_plot(plot, patch.measured_lab, view);
        let reference_color = egui::Color32::from_rgb(
            patch.reference_srgb[0],
            patch.reference_srgb[1],
            patch.reference_srgb[2],
        );
        let measured_color = egui::Color32::from_rgb(
            patch.measured_srgb[0],
            patch.measured_srgb[1],
            patch.measured_srgb[2],
        );
        let stroke_color = if selected {
            egui::Color32::YELLOW
        } else {
            egui::Color32::from_rgba_unmultiplied(235, 235, 235, 170)
        };
        paint_lab_arrow(
            &painter,
            reference,
            measured,
            egui::Stroke::new(1.0, stroke_color),
        );
        let square = egui::Rect::from_center_size(reference, egui::vec2(8.0, 8.0));
        painter.rect_filled(square, 0.5, reference_color);
        painter.rect_stroke(
            square,
            0.5,
            egui::Stroke::new(if selected { 2.0 } else { 1.2 }, egui::Color32::WHITE),
            egui::StrokeKind::Inside,
        );
        painter.circle_filled(measured, 4.0, measured_color);
        painter.circle_stroke(
            measured,
            5.5,
            egui::Stroke::new(if selected { 2.0 } else { 1.2 }, egui::Color32::BLACK),
        );
        if selected {
            painter.circle_stroke(measured, 8.0, egui::Stroke::new(1.5, egui::Color32::YELLOW));
        }
        painter.text(
            measured + egui::vec2(6.0, -6.0),
            egui::Align2::LEFT_BOTTOM,
            (patch.index + 1).to_string(),
            egui::TextStyle::Small.resolve(ui.style()),
            if selected {
                egui::Color32::YELLOW
            } else {
                egui::Color32::WHITE
            },
        );
    }

    if response.hovered()
        && let Some(pointer) = response.hover_pos()
        && plot.contains(pointer)
    {
        let (a, b) = plot_to_lab_ab(plot, pointer, view);
        painter.text(
            plot.right_top() + egui::vec2(-4.0, 4.0),
            egui::Align2::RIGHT_TOP,
            format!("a* {a:.1}, b* {b:.1}"),
            egui::TextStyle::Small.resolve(ui.style()),
            egui::Color32::WHITE,
        );
    }
}

fn initialize_lab_chart_view(view: &mut LabChartView, analysis: &ColorChartAnalysis) {
    let mut min_a = 0.0_f64;
    let mut max_a = 0.0_f64;
    let mut min_b = 0.0_f64;
    let mut max_b = 0.0_f64;
    for patch in &analysis.patches {
        for lab in [patch.reference_lab, patch.measured_lab] {
            min_a = min_a.min(lab.a);
            max_a = max_a.max(lab.a);
            min_b = min_b.min(lab.b);
            max_b = max_b.max(lab.b);
        }
    }
    view.center_a = ((min_a + max_a) * 0.5).clamp(LAB_CHART_FULL_MIN, LAB_CHART_FULL_MAX);
    view.center_b = ((min_b + max_b) * 0.5).clamp(LAB_CHART_FULL_MIN, LAB_CHART_FULL_MAX);
    view.half_a = (((max_a - min_a) * 0.65).max(LAB_CHART_DEFAULT_HALF_RANGE * 0.5))
        .clamp(LAB_CHART_MIN_HALF_RANGE, full_lab_half_range());
    view.half_b = (((max_b - min_b) * 0.65).max(LAB_CHART_DEFAULT_HALF_RANGE * 0.5))
        .clamp(LAB_CHART_MIN_HALF_RANGE, full_lab_half_range());
    view.initialized = true;
    clamp_lab_chart_view(view);
}

fn full_lab_half_range() -> f64 {
    (LAB_CHART_FULL_MAX - LAB_CHART_FULL_MIN) * 0.5
}

fn clamp_lab_chart_view(view: &mut LabChartView) {
    view.half_a = view
        .half_a
        .clamp(LAB_CHART_MIN_HALF_RANGE, full_lab_half_range());
    view.half_b = view
        .half_b
        .clamp(LAB_CHART_MIN_HALF_RANGE, full_lab_half_range());
    view.center_a = view.center_a.clamp(
        LAB_CHART_FULL_MIN + view.half_a,
        LAB_CHART_FULL_MAX - view.half_a,
    );
    view.center_b = view.center_b.clamp(
        LAB_CHART_FULL_MIN + view.half_b,
        LAB_CHART_FULL_MAX - view.half_b,
    );
}

fn lab_chart_bounds(view: &LabChartView) -> (f64, f64, f64, f64) {
    (
        view.center_a - view.half_a,
        view.center_a + view.half_a,
        view.center_b - view.half_b,
        view.center_b + view.half_b,
    )
}

fn lab_to_plot(plot: egui::Rect, lab: LabColor, view: &LabChartView) -> egui::Pos2 {
    let (min_a, max_a, min_b, max_b) = lab_chart_bounds(view);
    let x = plot.left() + ((lab.a - min_a) / (max_a - min_a)).clamp(0.0, 1.0) as f32 * plot.width();
    let y =
        plot.bottom() - ((lab.b - min_b) / (max_b - min_b)).clamp(0.0, 1.0) as f32 * plot.height();
    egui::pos2(x, y)
}

fn plot_to_lab_ab(plot: egui::Rect, position: egui::Pos2, view: &LabChartView) -> (f64, f64) {
    let (min_a, max_a, min_b, max_b) = lab_chart_bounds(view);
    let a = min_a + f64::from((position.x - plot.left()) / plot.width().max(1.0)) * (max_a - min_a);
    let b =
        min_b + f64::from((plot.bottom() - position.y) / plot.height().max(1.0)) * (max_b - min_b);
    (a, b)
}

fn paint_lab_background(painter: &egui::Painter, plot: egui::Rect, view: &LabChartView) {
    let (min_a, max_a, min_b, max_b) = lab_chart_bounds(view);
    let cell_width = plot.width() / LAB_CHART_BACKGROUND_COLUMNS as f32;
    let cell_height = plot.height() / LAB_CHART_BACKGROUND_ROWS as f32;
    for row in 0..LAB_CHART_BACKGROUND_ROWS {
        for column in 0..LAB_CHART_BACKGROUND_COLUMNS {
            let a = min_a
                + (column as f64 + 0.5) / LAB_CHART_BACKGROUND_COLUMNS as f64 * (max_a - min_a);
            let b = max_b - (row as f64 + 0.5) / LAB_CHART_BACKGROUND_ROWS as f64 * (max_b - min_b);
            let [r, g, bl] = lab_d65_to_display_srgb(LabColor { l: 70.0, a, b });
            let min = egui::pos2(
                plot.left() + column as f32 * cell_width,
                plot.top() + row as f32 * cell_height,
            );
            let max = egui::pos2(min.x + cell_width + 0.5, min.y + cell_height + 0.5);
            painter.rect_filled(
                egui::Rect::from_min_max(min, max),
                0.0,
                egui::Color32::from_rgb(r, g, bl).gamma_multiply(0.62),
            );
        }
    }
    painter.rect_stroke(
        plot,
        1.0,
        egui::Stroke::new(1.0, egui::Color32::from_gray(95)),
        egui::StrokeKind::Inside,
    );
}

fn paint_lab_grid_axes(
    ui: &egui::Ui,
    painter: &egui::Painter,
    plot: egui::Rect,
    view: &LabChartView,
) {
    let (min_a, max_a, min_b, max_b) = lab_chart_bounds(view);
    let a_step = nice_lab_tick_step(max_a - min_a);
    let b_step = nice_lab_tick_step(max_b - min_b);
    let font = egui::TextStyle::Small.resolve(ui.style());
    let mut tick = (min_a / a_step).ceil() * a_step;
    while tick <= max_a + a_step * 0.5 {
        let x = plot.left() + ((tick - min_a) / (max_a - min_a)) as f32 * plot.width();
        let is_axis = tick.abs() <= a_step * 0.25;
        painter.line_segment(
            [egui::pos2(x, plot.top()), egui::pos2(x, plot.bottom())],
            egui::Stroke::new(
                if is_axis { 1.3 } else { 0.6 },
                egui::Color32::from_rgba_unmultiplied(
                    255,
                    255,
                    255,
                    if is_axis { 170 } else { 70 },
                ),
            ),
        );
        painter.text(
            egui::pos2(x, plot.bottom() + 4.0),
            egui::Align2::CENTER_TOP,
            format_tick(tick),
            font.clone(),
            egui::Color32::WHITE,
        );
        tick += a_step;
    }
    let mut tick = (min_b / b_step).ceil() * b_step;
    while tick <= max_b + b_step * 0.5 {
        let y = plot.bottom() - ((tick - min_b) / (max_b - min_b)) as f32 * plot.height();
        let is_axis = tick.abs() <= b_step * 0.25;
        painter.line_segment(
            [egui::pos2(plot.left(), y), egui::pos2(plot.right(), y)],
            egui::Stroke::new(
                if is_axis { 1.3 } else { 0.6 },
                egui::Color32::from_rgba_unmultiplied(
                    255,
                    255,
                    255,
                    if is_axis { 170 } else { 70 },
                ),
            ),
        );
        painter.text(
            egui::pos2(plot.left() - 4.0, y),
            egui::Align2::RIGHT_CENTER,
            format_tick(tick),
            font.clone(),
            egui::Color32::WHITE,
        );
        tick += b_step;
    }
    painter.text(
        plot.right_bottom() + egui::vec2(0.0, 18.0),
        egui::Align2::RIGHT_TOP,
        "a*",
        font.clone(),
        egui::Color32::WHITE,
    );
    painter.text(
        plot.left_top() - egui::vec2(8.0, 12.0),
        egui::Align2::RIGHT_TOP,
        "b*",
        font,
        egui::Color32::WHITE,
    );
}

fn nice_lab_tick_step(span: f64) -> f64 {
    let raw = (span / 5.0).max(1.0);
    let base = 10.0_f64.powf(raw.log10().floor());
    let normalized = raw / base;
    let multiplier = if normalized <= 1.0 {
        1.0
    } else if normalized <= 2.0 {
        2.0
    } else if normalized <= 5.0 {
        5.0
    } else {
        10.0
    };
    multiplier * base
}

fn format_tick(value: f64) -> String {
    if value.abs() >= 10.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

fn nearest_lab_patch_index(
    analysis: &ColorChartAnalysis,
    plot: egui::Rect,
    view: &LabChartView,
    position: egui::Pos2,
) -> Option<usize> {
    analysis
        .patches
        .iter()
        .filter_map(|patch| {
            let reference = lab_to_plot(plot, patch.reference_lab, view);
            let measured = lab_to_plot(plot, patch.measured_lab, view);
            let distance =
                screen_distance(position, reference).min(screen_distance(position, measured));
            (distance <= 14.0).then_some((patch.index, distance))
        })
        .min_by(|(_, lhs), (_, rhs)| lhs.total_cmp(rhs))
        .map(|(index, _)| index)
}

fn screen_distance(a: egui::Pos2, b: egui::Pos2) -> f32 {
    let dx = a.x - b.x;
    let dy = a.y - b.y;
    (dx * dx + dy * dy).sqrt()
}

fn paint_lab_arrow(
    painter: &egui::Painter,
    start: egui::Pos2,
    end: egui::Pos2,
    stroke: egui::Stroke,
) {
    painter.line_segment([start, end], stroke);
    let dx = end.x - start.x;
    let dy = end.y - start.y;
    let length = (dx * dx + dy * dy).sqrt();
    if length <= 3.0 {
        return;
    }
    let dir = egui::vec2(dx / length, dy / length);
    let normal = egui::vec2(-dir.y, dir.x);
    let head = 6.0;
    let left = end - dir * head + normal * (head * 0.45);
    let right = end - dir * head - normal * (head * 0.45);
    painter.line_segment([end, left], stroke);
    painter.line_segment([end, right], stroke);
}

fn color_sidebar_top_max_height(
    panel_height: f32,
    has_analysis: bool,
    patch_details_expanded: bool,
) -> f32 {
    let reserved =
        patch_details_reserved_height(panel_height, has_analysis, patch_details_expanded);
    (panel_height - reserved).max(1.0)
}

fn patch_details_reserved_height(
    panel_height: f32,
    has_analysis: bool,
    patch_details_expanded: bool,
) -> f32 {
    if !has_analysis {
        return 0.0;
    }
    if patch_details_expanded {
        (panel_height * 0.38).clamp(
            PATCH_DETAILS_COLLAPSED_RESERVE + PATCH_TABLE_HEADER_HEIGHT + 1.0,
            260.0,
        )
    } else {
        PATCH_DETAILS_COLLAPSED_RESERVE
    }
}

fn patch_table_body_height(available_height: f32) -> f32 {
    (available_height - PATCH_TABLE_HEADER_HEIGHT).max(1.0)
}

fn render_white_balance(ui: &mut egui::Ui, analysis: &ColorChartAnalysis) {
    ui.heading("White Balance");
    ui.weak(WHITE_BALANCE_FORMAT_NOTE);
    render_single_line_metric(ui, white_balance_exposure_label(analysis));
    render_single_line_metric(ui, white_balance_gray_error_label(analysis));
    render_gray_patch_swatches(ui, &analysis.patches[18..24]);
}

fn white_balance_exposure_label(analysis: &ColorChartAnalysis) -> String {
    format!(
        "Exp μ/max: {:+.2}/{:.2}EV",
        analysis.gray_mean_exposure_error_stops, analysis.gray_max_abs_exposure_error_stops
    )
}

fn white_balance_gray_error_label(analysis: &ColorChartAnalysis) -> String {
    format!(
        "Gray μ/max: {}/{}",
        format_gray_swatch_error(
            analysis.gray_mean_delta_c,
            analysis.gray_mean_hsv_saturation
        ),
        format_gray_swatch_error(analysis.gray_max_delta_c, analysis.gray_max_hsv_saturation)
    )
}

fn render_single_line_metric(ui: &mut egui::Ui, text: String) {
    let font_size = single_line_metric_font_size(ui.available_width(), &text);
    ui.add(
        egui::Label::new(egui::RichText::new(text).monospace().size(font_size))
            .wrap_mode(egui::TextWrapMode::Extend)
            .selectable(false),
    );
}

fn single_line_metric_font_size(available_width: f32, label: &str) -> f32 {
    (available_width / (label.chars().count().max(1) as f32 * GRAY_SWATCH_MONOSPACE_WIDTH_RATIO))
        .clamp(7.0, 11.0)
}

#[cfg(test)]
fn estimated_single_line_metric_width(label: &str, font_size: f32) -> f32 {
    label.chars().count() as f32 * font_size * GRAY_SWATCH_MONOSPACE_WIDTH_RATIO
}

fn render_gray_patch_swatches(ui: &mut egui::Ui, patches: &[ChartPatchMeasurement]) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = GRAY_SWATCH_SPACING;
        let cell_width = gray_swatch_cell_width(ui.available_width());
        for patch in patches.iter().take(GRAY_SWATCH_COLUMNS) {
            let size = egui::vec2(cell_width, 30.0);
            let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
            let fill = egui::Color32::from_rgb(
                patch.measured_srgb[0],
                patch.measured_srgb[1],
                patch.measured_srgb[2],
            );
            let painter = ui.painter_at(rect);
            painter.rect_filled(rect, 2.0, fill);
            painter.rect_stroke(
                rect,
                2.0,
                egui::Stroke::new(1.0, egui::Color32::from_gray(90)),
                egui::StrokeKind::Inside,
            );
            let label = format_gray_swatch_chip_label(patch.delta_c, patch.hsv_saturation);
            let font_size = gray_swatch_label_font_size(cell_width, &label);
            ui.put(
                rect.shrink2(egui::vec2(GRAY_SWATCH_PADDING_X, 0.0)),
                egui::Label::new(
                    egui::RichText::new(label)
                        .monospace()
                        .size(font_size)
                        .color(swatch_text_color(patch.measured_srgb)),
                )
                .wrap_mode(egui::TextWrapMode::Extend)
                .selectable(false),
            );
        }
    });
}

fn gray_swatch_cell_width(available_width: f32) -> f32 {
    ((available_width - GRAY_SWATCH_SPACING * (GRAY_SWATCH_COLUMNS.saturating_sub(1) as f32))
        / GRAY_SWATCH_COLUMNS as f32)
        .max(1.0)
}

fn format_gray_swatch_error(delta_c: f64, hsv_saturation: f64) -> String {
    format!("{delta_c:.1}[{hsv_saturation:.2}]")
}

fn format_gray_swatch_chip_label(delta_c: f64, hsv_saturation: f64) -> String {
    format!("{delta_c:.1}\n{hsv_saturation:.2}")
}

fn gray_swatch_label_font_size(cell_width: f32, label: &str) -> f32 {
    let usable_width = (cell_width - GRAY_SWATCH_PADDING_X * 2.0).max(1.0);
    let max_line_chars = label
        .lines()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(1)
        .max(1) as f32;
    (usable_width / (max_line_chars * GRAY_SWATCH_MONOSPACE_WIDTH_RATIO))
        .clamp(GRAY_SWATCH_MIN_FONT_SIZE, GRAY_SWATCH_MAX_FONT_SIZE)
}

#[cfg(test)]
fn estimated_gray_swatch_label_width(label: &str, font_size: f32) -> f32 {
    label
        .lines()
        .map(|line| line.chars().count() as f32 * font_size * GRAY_SWATCH_MONOSPACE_WIDTH_RATIO)
        .fold(0.0_f32, f32::max)
}

fn swatch_text_color(rgb: [u8; 3]) -> egui::Color32 {
    let luminance =
        0.2126 * f64::from(rgb[0]) + 0.7152 * f64::from(rgb[1]) + 0.0722 * f64::from(rgb[2]);
    if luminance > 150.0 {
        egui::Color32::BLACK
    } else {
        egui::Color32::WHITE
    }
}

fn render_patch_table(
    ui: &mut egui::Ui,
    analysis: &ColorChartAnalysis,
    selected_patch: &mut Option<usize>,
) {
    use egui_extras::{Column, TableBuilder};

    let body_height = patch_table_body_height(ui.available_height());
    TableBuilder::new(ui)
        .id_salt("color_patch_metrics")
        .striped(true)
        .resizable(true)
        .max_scroll_height(body_height)
        .min_scrolled_height(1.0)
        .column(Column::initial(34.0).at_least(30.0))
        .column(Column::remainder().at_least(88.0).clip(true))
        .column(Column::initial(46.0).at_least(40.0))
        .column(Column::initial(46.0).at_least(40.0))
        .column(Column::initial(66.0).at_least(54.0))
        .column(Column::initial(64.0).at_least(52.0))
        .column(Column::initial(64.0).at_least(52.0))
        .column(Column::initial(78.0).at_least(64.0))
        .header(22.0, |mut header| {
            header.col(|ui| {
                ui.strong("#");
            });
            header.col(|ui| {
                ui.strong("Patch");
            });
            header.col(|ui| {
                ui.strong("ΔC");
            });
            header.col(|ui| {
                ui.strong("ΔE");
            });
            header.col(|ui| {
                ui.strong("Chroma%");
            });
            header.col(|ui| {
                ui.strong("HSV S");
            });
            header.col(|ui| {
                ui.strong("Stops");
            });
            header.col(|ui| {
                ui.strong("RGB");
            });
        })
        .body(|body| {
            body.rows(24.0, analysis.patches.len(), |mut row| {
                let patch = &analysis.patches[row.index()];
                let selected = *selected_patch == Some(patch.index);
                row.set_selected(selected);
                row.col(|ui| {
                    if ui
                        .selectable_label(selected, (patch.index + 1).to_string())
                        .clicked()
                    {
                        *selected_patch = Some(patch.index);
                    }
                });
                row.col(|ui| {
                    if ui
                        .selectable_label(
                            selected,
                            format!("{} {}", patch.reference_sample_name, patch.name),
                        )
                        .clicked()
                    {
                        *selected_patch = Some(patch.index);
                    }
                });
                row.col(|ui| {
                    ui.label(format!("{:.1}", patch.delta_c));
                });
                row.col(|ui| {
                    ui.label(format!("{:.1}", patch.delta_e));
                });
                row.col(|ui| {
                    ui.label(format!("{:.0}%", patch.camera_chroma_percent));
                });
                row.col(|ui| {
                    ui.label(format!("{:.3}", patch.hsv_saturation));
                });
                row.col(|ui| {
                    ui.label(format!("{:+.2}", patch.exposure_error_stops));
                });
                row.col(|ui| {
                    let [r, g, b] = patch.measured_srgb;
                    ui.label(format!("{r},{g},{b}"));
                });
            });
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_sidebar_for_test(
        context: &egui::Context,
        workspace: &mut ColorInspectionWorkspace,
        viewport: egui::Vec2,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        let mut input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, viewport)),
            ..Default::default()
        };
        input.events = events;
        context.run_ui(input, |ui| {
            workspace.render_right_panel(ui, Some("synthetic.png"), true, false);
        })
    }

    fn accessibility_text(output: &egui::FullOutput) -> String {
        output
            .platform_output
            .accesskit_update
            .as_ref()
            .expect("accessibility tree is enabled")
            .nodes
            .iter()
            .filter_map(|(_, node)| node.label().or_else(|| node.value()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[allow(clippy::cast_possible_truncation)]
    fn accesskit_rect_center(rect: egui::accesskit::Rect) -> egui::Pos2 {
        egui::pos2(
            ((rect.x0 + rect.x1) * 0.5) as f32,
            ((rect.y0 + rect.y1) * 0.5) as f32,
        )
    }

    fn accesskit_bounds(output: &egui::FullOutput, label: &str) -> egui::accesskit::Rect {
        output
            .platform_output
            .accesskit_update
            .as_ref()
            .expect("accessibility tree is enabled")
            .nodes
            .iter()
            .find_map(|(_, node)| {
                (node.label() == Some(label) || node.value() == Some(label))
                    .then(|| node.bounds())
                    .flatten()
            })
            .unwrap_or_else(|| panic!("accessibility node {label:?} is visible"))
    }

    fn assert_metric_label_single_line(
        output: &egui::FullOutput,
        label: &str,
        viewport_width: f32,
    ) {
        let bounds = accesskit_bounds(output, label);
        assert!(
            bounds.y1 - bounds.y0 <= 20.0,
            "white-balance line {label} should stay single-line, bounds {bounds:?}"
        );
        assert!(
            bounds.x0 >= -0.5 && bounds.x1 <= f64::from(viewport_width) + 0.5,
            "white-balance line {label} should stay within {viewport_width}px sidebar, bounds {bounds:?}"
        );
        let font_size = single_line_metric_font_size(viewport_width, label);
        assert!(
            estimated_single_line_metric_width(label, font_size) <= viewport_width,
            "white-balance line {label} should fit {viewport_width}px at {font_size}px"
        );
    }

    fn synthetic_analysis_for_test() -> ColorChartAnalysis {
        let frame = synthetic_projective_color_checker([
            ColorImagePoint::new(64.0, 58.0),
            ColorImagePoint::new(308.0, 42.0),
            ColorImagePoint::new(330.0, 208.0),
            ColorImagePoint::new(52.0, 226.0),
        ]);
        analyze_rgba8(
            ColorInputKey {
                document_id: DocumentId::from_raw(101),
                generation: 1,
            },
            "synthetic.png".to_owned(),
            &frame,
            LightSourcePreset::D65,
            ColorChartKind::ColorChecker24Nov2014AndNewer,
        )
        .unwrap()
    }

    fn rgba8_frame_from_png_path(path: &std::path::Path) -> Rgba8Frame {
        let image = image::ImageReader::open(path)
            .unwrap_or_else(|error| panic!("open PNG fixture {path:?}: {error}"))
            .decode()
            .unwrap_or_else(|error| panic!("decode PNG fixture {path:?}: {error}"))
            .to_rgba8();
        let (width, height) = image.dimensions();
        Rgba8Frame::tight(width, height, std::sync::Arc::from(image.into_raw())).unwrap()
    }

    #[test]
    fn real_d65_black_substrate_png_is_auto_detected() {
        let path = std::path::Path::new(
            "/media/psf/Home/Downloads/24色卡图/D65的yuv和png图/capture_ch0_20000101_021410_966_seq000003_ts946692850924044835_1920x1080.png",
        );
        if !path.exists() {
            eprintln!("skip missing local D65 ColorChecker fixture: {path:?}");
            return;
        }
        let frame = rgba8_frame_from_png_path(path);
        let adaptive_candidates = detect_adaptive_hole_patch_candidates(&frame).unwrap();
        assert!(
            adaptive_candidates.len() >= MIN_SPARSE_GRID_CANDIDATES,
            "adaptive hole detector should recover enough black-substrate patch holes; adaptive candidates={}",
            adaptive_candidates.len()
        );
        let analysis = analyze_rgba8(
            ColorInputKey {
                document_id: DocumentId::from_raw(946_692_850_924_044_835),
                generation: 3,
            },
            path.file_name().unwrap().to_string_lossy().into_owned(),
            &frame,
            LightSourcePreset::D65,
            ColorChartKind::ColorChecker24Nov2014AndNewer,
        )
        .unwrap();

        assert_eq!(analysis.detection_mode, ChartDetectionMode::AutoGrid);
        assert_eq!(analysis.patches.len(), COLOR_CHECKER_PATCHES);
        assert!(
            (560..=680).contains(&analysis.chart_roi.x),
            "chart x ROI should cover the real black-substrate card, got {:?}",
            analysis.chart_roi
        );
        assert!(
            (210..=330).contains(&analysis.chart_roi.y),
            "chart y ROI should cover the real black-substrate card, got {:?}",
            analysis.chart_roi
        );
        assert!(
            (700..=850).contains(&analysis.chart_roi.width),
            "chart width should cover the real black-substrate card, got {:?}",
            analysis.chart_roi
        );
        assert!(
            (420..=560).contains(&analysis.chart_roi.height),
            "chart height should cover the real black-substrate card, got {:?}",
            analysis.chart_roi
        );
        assert!(analysis.mean_delta_e.is_finite());
    }

    #[test]
    fn adaptive_hole_primary_detects_sparse_black_substrate() {
        let frame = synthetic_sparse_color_checker_on_dark_substrate([
            ColorImagePoint::new(72.0, 54.0),
            ColorImagePoint::new(326.0, 62.0),
            ColorImagePoint::new(314.0, 224.0),
            ColorImagePoint::new(58.0, 216.0),
        ]);
        let candidates = detect_adaptive_hole_patch_candidates(&frame).unwrap();
        assert!(
            (MIN_SPARSE_GRID_CANDIDATES..COLOR_CHECKER_PATCHES).contains(&candidates.len()),
            "synthetic sparse black-substrate fixture should require sparse grid fitting; candidates={}",
            candidates.len()
        );

        let input = ColorInputKey {
            document_id: DocumentId::from_raw(102),
            generation: 9,
        };
        let analysis = analyze_rgba8(
            input,
            "sparse-dark-substrate.png".to_owned(),
            &frame,
            LightSourcePreset::D65,
            ColorChartKind::ColorChecker24Nov2014AndNewer,
        )
        .unwrap();

        assert_eq!(analysis.detection_mode, ChartDetectionMode::AutoGrid);
        assert_eq!(analysis.patches.len(), COLOR_CHECKER_PATCHES);
        assert!(
            analysis.chart_roi.x <= 80 && analysis.chart_roi.y <= 65,
            "detected ROI should include the sparse synthetic chart, got {:?}",
            analysis.chart_roi
        );
    }

    #[test]
    fn color_overlay_paints_patch_grid_avg_left_ref_right_lower_cells_with_labels() {
        let context = egui::Context::default();
        let mut analysis = synthetic_analysis_for_test();
        analysis.patches[0].measured_srgb = [3, 17, 29];
        analysis.patches[0].reference_srgb = [251, 241, 7];

        let image_rect = egui::Rect::from_min_size(egui::pos2(8.0, 8.0), egui::vec2(384.0, 256.0));
        let output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(420.0, 300.0),
                )),
                ..Default::default()
            },
            |ui| {
                let painter = ui.painter_at(image_rect);
                paint_color_chart_overlay(
                    &painter,
                    image_rect,
                    [384, 256],
                    &analysis,
                    false,
                    Some(0),
                );
            },
        );

        let measured_color = color32_from_srgb(analysis.patches[0].measured_srgb);
        let reference_color = color32_from_srgb(analysis.patches[0].reference_srgb);
        let filled_paths_for = |color| {
            output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::Shape::Path(path) if path.closed && path.fill == color => {
                        Some(egui::Rect::from_points(&path.points))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let measured_cells = filled_paths_for(measured_color);
        let reference_cells = filled_paths_for(reference_color);
        assert_eq!(
            measured_cells.len(),
            1,
            "measured Avg fill should be painted once"
        );
        assert_eq!(
            reference_cells.len(),
            1,
            "reference Ref fill should be painted once"
        );

        let screen_polygon = analysis.patches[0]
            .polygon
            .map(|point| image_to_screen(image_rect, [384, 256], point, false));
        let patch_bounds = egui::Rect::from_points(&screen_polygon);
        let measured_cell = measured_cells[0];
        let reference_cell = reference_cells[0];
        assert!(
            measured_cell.center().x < patch_bounds.center().x,
            "Avg 色块应位于 2×2 patch 网格左下角: patch={patch_bounds:?}, avg={measured_cell:?}"
        );
        assert!(
            reference_cell.center().x > patch_bounds.center().x,
            "Ref 色块应位于 2×2 patch 网格右下角: patch={patch_bounds:?}, ref={reference_cell:?}"
        );
        assert!(
            measured_cell.center().y > patch_bounds.center().y
                && reference_cell.center().y > patch_bounds.center().y,
            "Avg/Ref 色块都应位于已有线框的下半区: patch={patch_bounds:?}, avg={measured_cell:?}, ref={reference_cell:?}"
        );
        assert!(
            (measured_cell.right() - patch_bounds.center().x).abs() <= 1.0
                && (reference_cell.left() - patch_bounds.center().x).abs() <= 1.0,
            "Avg/Ref 色块应共享 2×2 网格竖向中线: patch={patch_bounds:?}, avg={measured_cell:?}, ref={reference_cell:?}"
        );

        let overlay_text = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) => Some((text.galley.text().to_owned(), text.pos)),
                _ => None,
            })
            .collect::<Vec<_>>();
        let avg_label = overlay_text
            .iter()
            .find_map(|(text, pos)| {
                (text == "Avg" && measured_cell.expand(2.0).contains(*pos)).then_some(*pos)
            })
            .unwrap_or_else(|| {
                panic!("Avg label should be painted in Avg cell; labels={overlay_text:?}")
            });
        let ref_label = overlay_text
            .iter()
            .find_map(|(text, pos)| {
                (text == "Ref" && reference_cell.expand(2.0).contains(*pos)).then_some(*pos)
            })
            .unwrap_or_else(|| {
                panic!("Ref label should be painted in Ref cell; labels={overlay_text:?}")
            });
        assert!(
            avg_label.x <= measured_cell.left() + 4.0 && avg_label.y <= measured_cell.top() + 4.0,
            "Avg label should sit at the cell top-left corner: avg_cell={measured_cell:?}, label={avg_label:?}"
        );
        assert!(
            ref_label.x <= reference_cell.left() + 4.0 && ref_label.y <= reference_cell.top() + 4.0,
            "Ref label should sit at the cell top-left corner: ref_cell={reference_cell:?}, label={ref_label:?}"
        );
    }
    #[test]
    fn color_sidebar_height_math_keeps_collapsed_patch_details_compact() {
        let panel_height = 260.0;
        let expanded_top = color_sidebar_top_max_height(panel_height, true, true);
        let collapsed_top = color_sidebar_top_max_height(panel_height, true, false);

        assert!(
            collapsed_top > expanded_top,
            "collapsed Patch Details should return height to upper sections"
        );
        assert!(
            patch_details_reserved_height(panel_height, true, false)
                <= PATCH_DETAILS_COLLAPSED_RESERVE
        );
        assert!(patch_table_body_height(80.0) < 200.0);
        assert_eq!(patch_table_body_height(10.0), 1.0);
        let narrow_cell_width = gray_swatch_cell_width(280.0);
        for label in ["0.0\n0.00", "99.9\n1.00", "100.0\n1.00"] {
            let font_size = gray_swatch_label_font_size(narrow_cell_width, label);
            assert!(
                font_size >= 10.0,
                "gray swatch label {label:?} should use visibly larger font at 280px, got {font_size}px"
            );
            assert!(
                estimated_gray_swatch_label_width(label, font_size)
                    <= narrow_cell_width - GRAY_SWATCH_PADDING_X * 2.0 + 0.5,
                "gray swatch label {label} should fit 280px sidebar cell width {narrow_cell_width} at {font_size}px"
            );
        }
    }

    #[test]
    fn color_sidebar_analysis_uses_bounded_patch_details_in_short_viewport() {
        let context = egui::Context::default();
        context.all_styles_mut(|style| style.animation_time = 0.0);
        context.enable_accesskit();
        let viewport = egui::vec2(280.0, 260.0);
        let analysis = synthetic_analysis_for_test();
        let exposure_label = white_balance_exposure_label(&analysis);
        let gray_error_label = white_balance_gray_error_label(&analysis);
        let gray_labels = analysis.patches[18..24]
            .iter()
            .map(|patch| format_gray_swatch_chip_label(patch.delta_c, patch.hsv_saturation))
            .collect::<Vec<_>>();
        let mut workspace = ColorInspectionWorkspace {
            analysis: Some(analysis),
            ..Default::default()
        };

        let expanded = render_sidebar_for_test(&context, &mut workspace, viewport, Vec::new());
        let expanded_text = accessibility_text(&expanded);
        assert!(expanded_text.contains("Color Accuracy"));
        assert!(expanded_text.contains("White Balance"));
        assert!(expanded_text.contains("Patch Details"));
        assert!(expanded_text.contains("Delta C"));
        assert!(expanded_text.contains("Delta E"));
        assert!(expanded_text.contains("Saturation"));
        assert!(!expanded_text.contains("Gray patches:"));
        assert!(!expanded_text.contains("CIELAB a*"));
        assert!(expanded_text.contains(WHITE_BALANCE_FORMAT_NOTE));
        assert!(expanded_text.contains("Patch inset / edge"));
        assert!(expanded_text.contains("Sample area: 50% × 50%"));
        assert!(expanded_text.contains(&exposure_label));
        assert!(expanded_text.contains(&gray_error_label));
        for label in [&exposure_label, &gray_error_label] {
            assert_metric_label_single_line(&expanded, label, viewport.x);
        }
        let viewport_360 = egui::vec2(360.0, 260.0);
        let context_360 = egui::Context::default();
        context_360.all_styles_mut(|style| style.animation_time = 0.0);
        context_360.enable_accesskit();
        let analysis_360 = synthetic_analysis_for_test();
        let exposure_label_360 = white_balance_exposure_label(&analysis_360);
        let gray_error_label_360 = white_balance_gray_error_label(&analysis_360);
        let mut workspace_360 = ColorInspectionWorkspace {
            analysis: Some(analysis_360),
            ..Default::default()
        };
        let expanded_360 =
            render_sidebar_for_test(&context_360, &mut workspace_360, viewport_360, Vec::new());
        for label in [&exposure_label_360, &gray_error_label_360] {
            assert!(accessibility_text(&expanded_360).contains(label));
            assert_metric_label_single_line(&expanded_360, label, viewport_360.x);
        }
        assert!(expanded_text.contains("A1 Dark Skin"));
        for label in &gray_labels {
            assert!(
                expanded_text.contains(label),
                "gray swatch error label {label} is visible"
            );
        }
        let gray_label_y = gray_labels
            .iter()
            .map(|label| accesskit_rect_center(accesskit_bounds(&expanded, label)).y)
            .collect::<Vec<_>>();
        let gray_min_y = gray_label_y.iter().copied().fold(f32::INFINITY, f32::min);
        let gray_max_y = gray_label_y
            .iter()
            .copied()
            .fold(f32::NEG_INFINITY, f32::max);
        assert!(
            gray_max_y - gray_min_y <= 2.0,
            "gray swatches should render as one row: {gray_label_y:?}"
        );

        let patch_header = accesskit_rect_center(accesskit_bounds(&expanded, "Patch Details"));
        render_sidebar_for_test(
            &context,
            &mut workspace,
            viewport,
            vec![egui::Event::PointerMoved(patch_header)],
        );
        render_sidebar_for_test(
            &context,
            &mut workspace,
            viewport,
            vec![egui::Event::PointerButton {
                pos: patch_header,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            }],
        );
        render_sidebar_for_test(
            &context,
            &mut workspace,
            viewport,
            vec![egui::Event::PointerButton {
                pos: patch_header,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            }],
        );
        let collapsed = render_sidebar_for_test(&context, &mut workspace, viewport, Vec::new());
        let collapsed_text = accessibility_text(&collapsed);
        assert!(!workspace.patch_details_expanded);
        assert!(collapsed_text.contains("Color Accuracy"));
        assert!(collapsed_text.contains("White Balance"));
        assert!(collapsed_text.contains("Patch Details"));
        assert!(!collapsed_text.contains("A1 Dark Skin"));
    }

    #[test]
    fn lab_delta_is_zero_for_same_color() {
        let white = srgb_to_lab([243, 243, 242]);
        assert!(delta_e(white, white) < 1.0e-9);
    }

    #[test]
    fn color_error_helpers_match_requested_formulas() {
        let actual = lab(50.0, 3.0, 4.0);
        let expected = lab(44.0, -1.0, 1.0);

        assert!((delta_c(actual, expected) - 5.0).abs() < 1.0e-9);
        assert!((delta_e(actual, expected) - 61.0_f64.sqrt()).abs() < 1.0e-9);
        assert!((hsv_saturation([128, 64, 64]) - 0.5).abs() < 1.0e-9);
        assert!(exposure_error_stops([255, 255, 255], lab(100.0, 0.0, 0.0)).abs() < 0.02);
    }

    #[test]
    fn srgb_d65_white_maps_near_lab_neutral() {
        let white = srgb_to_lab([255, 255, 255]);
        assert!((white.l - 100.0).abs() < 0.02, "L* was {}", white.l);
        assert!(white.a.abs() < 0.02, "a* was {}", white.a);
        assert!(white.b.abs() < 0.02, "b* was {}", white.b);
    }

    #[test]
    fn bradford_adapts_d50_white_to_d65_white() {
        let adapted = adapt_xyz_d50_to_d65(D50_WHITE);
        for (actual, expected) in adapted.into_iter().zip(D65_WHITE) {
            assert!((actual - expected).abs() < 1.0e-6);
        }
    }

    #[test]
    fn color_checker_reference_versions_are_auditable() {
        let old = color_checker_references(ColorChartKind::ColorChecker24BeforeNov2014);
        let new = color_checker_references(ColorChartKind::ColorChecker24Nov2014AndNewer);

        assert_eq!(
            ColorChartKind::default(),
            ColorChartKind::ColorChecker24Nov2014AndNewer
        );
        assert_eq!(old[0].sample_name, "A1");
        assert_eq!(old[0].name, "Dark Skin");
        assert!((old[0].source_lab.l - 37.986).abs() < 1.0e-9);
        assert!((old[0].source_lab.a - 13.555).abs() < 1.0e-9);
        assert!((old[0].source_lab.b - 14.059).abs() < 1.0e-9);
        assert!((new[0].source_lab.l - 37.540).abs() < 1.0e-9);
        assert!((new[0].source_lab.a - 14.370).abs() < 1.0e-9);
        assert!((new[0].source_lab.b - 14.920).abs() < 1.0e-9);

        let derived_from_display = srgb_to_lab(new[0].display_srgb);
        assert!(
            delta_e(new[0].comparison_lab, derived_from_display) < 0.5,
            "display RGB should round-trip near D65 comparison Lab after 8-bit quantization"
        );

        let metadata = ColorChartKind::ColorChecker24Nov2014AndNewer.reference_metadata();
        assert_eq!(metadata.manufacturer, "X-Rite");
        assert_eq!(metadata.white_point, "D50");
        assert_eq!(
            metadata.source_name,
            "ColorChecker24 - November2014 edition and newer"
        );
        assert_eq!(
            metadata.source_data_url,
            "https://babelcolor.com/index_htm_files/ColorChecker24_After_Nov2014.txt"
        );
    }

    #[test]
    fn synthetic_color_checker_is_detected_and_exported() {
        let frame = synthetic_projective_color_checker([
            ColorImagePoint::new(64.0, 58.0),
            ColorImagePoint::new(308.0, 42.0),
            ColorImagePoint::new(330.0, 208.0),
            ColorImagePoint::new(52.0, 226.0),
        ]);
        let input = ColorInputKey {
            document_id: DocumentId::from_raw(7),
            generation: 3,
        };
        let analysis = analyze_rgba8(
            input,
            "synthetic.png".to_owned(),
            &frame,
            LightSourcePreset::D65,
            ColorChartKind::ColorChecker24Nov2014AndNewer,
        )
        .unwrap();
        assert_eq!(analysis.detection_mode, ChartDetectionMode::AutoGrid);
        assert_eq!(analysis.patches.len(), COLOR_CHECKER_PATCHES);
        assert!(
            analysis.mean_delta_e < 6.0,
            "mean ΔE was {}",
            analysis.mean_delta_e
        );
        assert!(
            analysis.max_delta_e < 16.0,
            "max ΔE was {}",
            analysis.max_delta_e
        );
        let payload = export_payload(&analysis);
        assert_eq!(payload["schema"], "camera-toolbox/color-inspection/v1");
        assert_eq!(payload["detection"]["mode"], "auto_grid");
        assert_eq!(
            payload["configuration"]["chart_kind"]["id"],
            "xrite_colorchecker_24_nov_2014_and_newer"
        );
        assert_eq!(
            payload["configuration"]["reference"]["manufacturer"],
            "X-Rite"
        );
        assert_eq!(payload["configuration"]["reference"]["white_point"], "D50");
        assert_eq!(
            payload["configuration"]["comparison_lab_white_point"],
            "D65"
        );
        assert_eq!(
            analysis.patch_edge_inset_percent,
            DEFAULT_PATCH_EDGE_INSET_PERCENT
        );
        assert_eq!(payload["configuration"]["patch_edge_inset_percent"], 25.0);
        assert_eq!(payload["configuration"]["patch_sample_fraction"], 0.5);
        assert_eq!(
            payload["patches"].as_array().unwrap().len(),
            COLOR_CHECKER_PATCHES
        );
        let patch0 = payload["patches"][0].as_object().unwrap();
        assert_eq!(
            patch0.get("sample_name").and_then(|value| value.as_str()),
            Some("A1")
        );
        assert!(
            (patch0["reference_lab_d50_authoritative"]["l"]
                .as_f64()
                .unwrap()
                - 37.54)
                .abs()
                < 1.0e-9
        );
        assert!(
            patch0
                .get("reference_display_srgb")
                .is_some_and(|value| value.is_array())
        );
        assert!(patch0.get("polygon").is_some_and(|value| value.is_array()));
        assert!(payload["metrics"].get("mean_delta_c_d65").is_some());
        assert!(payload["metrics"].get("max_delta_c_d65").is_some());
        assert!(payload["metrics"].get("mean_delta_e_d65").is_some());
        assert!(payload["metrics"].get("max_delta_e_d65").is_some());
        assert!(
            payload["metrics"]
                .get("mean_camera_chroma_percent")
                .is_some()
        );
        assert!(payload["metrics"].get("gray_mean_hsv_saturation").is_some());
        assert!(
            payload["metrics"]
                .get("gray_mean_exposure_error_stops")
                .is_some()
        );
        assert!(patch0.get("delta_c_d65").is_some());
        assert!(patch0.get("delta_e_d65").is_some());
        assert!(patch0.get("camera_chroma_percent").is_some());
        assert!(patch0.get("hsv_saturation").is_some());
        assert!(patch0.get("exposure_error_stops").is_some());
        assert!(
            (analysis.patches[0].delta_c
                - delta_c(
                    analysis.patches[0].measured_lab,
                    analysis.patches[0].reference_lab
                ))
            .abs()
                < 1.0e-9
        );
        assert!(
            (analysis.patches[0].exposure_error_stops
                - exposure_error_stops(
                    analysis.patches[0].measured_srgb,
                    analysis.patches[0].reference_lab
                ))
            .abs()
                < 1.0e-9
        );
        let mut chart_view = LabChartView::default();
        initialize_lab_chart_view(&mut chart_view, &analysis);
        let plot = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(320.0, 240.0));
        let measured = lab_to_plot(plot, analysis.patches[0].measured_lab, &chart_view);
        assert_eq!(
            nearest_lab_patch_index(&analysis, plot, &chart_view, measured),
            Some(0)
        );
        assert!(patch0.get("reference_srgb").is_none());
        assert!(patch0.get("reference_lab").is_none());
        let metrics_payload = metrics_report_payload(&analysis);
        let yaml =
            ColorMetricsExport::new(metrics_payload, ColorReportFormat::Yaml).serialize_for_test();
        let yaml_payload: serde_json::Value = serde_yaml::from_str(&yaml).unwrap();
        let yaml_object = yaml_payload
            .as_object()
            .expect("YAML report must be a flat metrics object");
        let expected_keys = std::collections::BTreeSet::from([
            "mean_delta_c_d65",
            "max_delta_c_d65",
            "mean_delta_e_d65",
            "max_delta_e_d65",
            "mean_camera_chroma_percent",
            "gray_mean_rgb",
            "gray_rg_ratio",
            "gray_bg_ratio",
            "gray_balance_error",
            "gray_mean_delta_c_d65",
            "gray_max_delta_c_d65",
            "gray_mean_hsv_saturation",
            "gray_max_hsv_saturation",
            "gray_mean_exposure_error_stops",
            "gray_max_abs_exposure_error_stops",
        ]);
        let actual_keys = yaml_object
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(actual_keys, expected_keys);
        for key in expected_keys {
            if key == "gray_mean_rgb" {
                assert_eq!(yaml_object[key].as_array().unwrap().len(), 3);
            } else {
                assert!(yaml_object[key].is_number(), "{key} must be numeric");
            }
        }
    }

    #[test]
    fn patch_edge_inset_changes_sampling_polygon_and_export_metadata() {
        let corners = [
            ColorImagePoint::new(64.0, 58.0),
            ColorImagePoint::new(308.0, 42.0),
            ColorImagePoint::new(330.0, 208.0),
            ColorImagePoint::new(52.0, 226.0),
        ];
        let frame = synthetic_projective_color_checker(corners);
        let input = ColorInputKey {
            document_id: DocumentId::from_raw(27),
            generation: 2,
        };
        let default_analysis = analyze_rgba8(
            input,
            "default-inset.png".to_owned(),
            &frame,
            LightSourcePreset::D65,
            ColorChartKind::ColorChecker24Nov2014AndNewer,
        )
        .unwrap();
        let inset_analysis = analyze_rgba8_with_patch_edge_inset(
            input,
            "custom-inset.png".to_owned(),
            &frame,
            LightSourcePreset::D65,
            ColorChartKind::ColorChecker24Nov2014AndNewer,
            35.0,
        )
        .unwrap();
        assert_eq!(inset_analysis.patch_edge_inset_percent, 35.0);
        assert!(
            polygon_area(inset_analysis.patches[0].polygon).abs()
                < polygon_area(default_analysis.patches[0].polygon).abs() * 0.6,
            "larger edge inset should shrink the sampling polygon"
        );
        let manual_analysis = analyze_rgba8_with_corners_with_patch_edge_inset(
            input,
            "manual-custom-inset.png".to_owned(),
            &frame,
            corners,
            ChartDetectionMode::ManualCorners,
            LightSourcePreset::D65,
            ColorChartKind::ColorChecker24Nov2014AndNewer,
            35.0,
        )
        .unwrap();
        assert_eq!(
            manual_analysis.detection_mode,
            ChartDetectionMode::ManualCorners
        );
        assert_eq!(manual_analysis.patch_edge_inset_percent, 35.0);
        let payload = export_payload(&inset_analysis);
        assert_eq!(payload["configuration"]["patch_edge_inset_percent"], 35.0);
        assert_eq!(payload["configuration"]["patch_sample_fraction"], 0.3);
    }

    #[test]
    fn patch_edge_inset_change_resamples_current_analysis_without_reselecting_corners() {
        let analysis = synthetic_analysis_for_test();
        let original_corners = analysis.chart_corners;
        let original_detection_mode = analysis.detection_mode;
        let original_area = polygon_area(analysis.patches[0].polygon).abs();
        let mut workspace = ColorInspectionWorkspace {
            analysis: Some(analysis),
            selected_patch: Some(3),
            ..Default::default()
        };
        workspace.prepare_export();
        assert!(workspace.current_analysis().is_some());
        assert!(workspace.pending_export.is_some());

        workspace.apply_patch_edge_inset_percent(35.0);

        assert_eq!(workspace.patch_edge_inset_percent, 35.0);
        assert!(workspace.pending_export.is_none());
        assert_eq!(workspace.selected_patch, Some(3));
        assert!(workspace.error.is_none());
        let updated = workspace
            .current_analysis()
            .expect("inset slider should resample immediately without requiring corner picks");
        assert_eq!(updated.patch_edge_inset_percent, 35.0);
        assert_eq!(updated.chart_corners, original_corners);
        assert_eq!(updated.detection_mode, original_detection_mode);
        assert!(
            polygon_area(updated.patches[0].polygon).abs() < original_area * 0.6,
            "larger edge inset should shrink the sampling polygon"
        );
        let payload = export_payload(updated);
        assert_eq!(payload["configuration"]["patch_edge_inset_percent"], 35.0);
        assert_eq!(payload["configuration"]["patch_sample_fraction"], 0.3);

        let manual_input = ColorInputKey {
            document_id: DocumentId::from_raw(301),
            generation: 7,
        };
        let mut picking_workspace = ColorInspectionWorkspace {
            manual_corners: Some(ManualCornerState {
                input: manual_input,
                input_label: "manual.png".to_owned(),
                points: vec![
                    ColorImagePoint::new(1.0, 2.0),
                    ColorImagePoint::new(3.0, 4.0),
                ],
            }),
            ..Default::default()
        };
        picking_workspace.apply_patch_edge_inset_percent(35.0);
        assert_eq!(picking_workspace.patch_edge_inset_percent, 35.0);
        let manual = picking_workspace
            .manual_corners
            .as_ref()
            .expect("unfinished manual corner picks must survive inset slider changes");
        assert_eq!(manual.input, manual_input);
        assert_eq!(manual.points.len(), 2);
        assert!(picking_workspace.analysis.is_none());
    }

    #[test]
    fn chart_kind_change_invalidates_analysis_and_pending_export() {
        let frame = synthetic_projective_color_checker([
            ColorImagePoint::new(64.0, 58.0),
            ColorImagePoint::new(308.0, 42.0),
            ColorImagePoint::new(330.0, 208.0),
            ColorImagePoint::new(52.0, 226.0),
        ]);
        let input = ColorInputKey {
            document_id: DocumentId::from_raw(17),
            generation: 1,
        };
        let analysis = analyze_rgba8(
            input,
            "synthetic.png".to_owned(),
            &frame,
            LightSourcePreset::D65,
            ColorChartKind::ColorChecker24Nov2014AndNewer,
        )
        .unwrap();

        let mut workspace = ColorInspectionWorkspace {
            analysis: Some(analysis.clone()),
            ..Default::default()
        };
        workspace.prepare_export();
        assert!(workspace.current_analysis().is_some());
        assert!(workspace.pending_export.is_some());

        workspace.apply_reference_configuration(
            LightSourcePreset::D65,
            ColorChartKind::ColorChecker24BeforeNov2014,
        );
        assert!(workspace.analysis.is_none());
        assert!(workspace.pending_export.is_none());
        assert!(workspace.current_analysis().is_none());

        workspace.analysis = Some(analysis);
        workspace.prepare_export();
        assert!(workspace.pending_export.is_none());
        assert!(workspace.error.as_deref().is_some_and(|error| {
            error.contains("analyze an image before exporting color metrics")
        }));
    }

    #[test]
    fn projective_color_checker_detection_is_scale_invariant() {
        let base_corners = [
            ColorImagePoint::new(64.0, 58.0),
            ColorImagePoint::new(308.0, 42.0),
            ColorImagePoint::new(330.0, 208.0),
            ColorImagePoint::new(52.0, 226.0),
        ];
        for (scale_factor, width, height) in [(1.0, 420, 300), (2.0, 840, 600), (4.0, 1680, 1200)] {
            let corners = base_corners.map(|point| scale(point, scale_factor));
            let frame = synthetic_color_checker_with_layout(width, height, corners, 0.72);
            let input = ColorInputKey {
                document_id: DocumentId::from_raw(width as u64),
                generation: height as u64,
            };
            let analysis = analyze_rgba8(
                input,
                format!("synthetic-scale-{scale_factor:.0}.png"),
                &frame,
                LightSourcePreset::D65,
                ColorChartKind::ColorChecker24Nov2014AndNewer,
            )
            .unwrap();

            assert_eq!(analysis.detection_mode, ChartDetectionMode::AutoGrid);
            assert_eq!(analysis.patches.len(), COLOR_CHECKER_PATCHES);
            assert!(
                analysis.mean_delta_e < 6.0,
                "scale {scale_factor} mean ΔE was {}",
                analysis.mean_delta_e
            );
        }
    }

    #[test]
    fn dark_substrate_with_different_scene_background_is_detected() {
        let frame = synthetic_color_checker_on_dark_substrate([
            ColorImagePoint::new(72.0, 54.0),
            ColorImagePoint::new(326.0, 62.0),
            ColorImagePoint::new(314.0, 224.0),
            ColorImagePoint::new(58.0, 216.0),
        ]);
        let input = ColorInputKey {
            document_id: DocumentId::from_raw(10),
            generation: 6,
        };
        let analysis = analyze_rgba8(
            input,
            "dark-substrate.png".to_owned(),
            &frame,
            LightSourcePreset::D65,
            ColorChartKind::ColorChecker24Nov2014AndNewer,
        )
        .unwrap();

        assert_eq!(analysis.detection_mode, ChartDetectionMode::AutoGrid);
        assert_eq!(analysis.patches.len(), COLOR_CHECKER_PATCHES);
        assert!(
            analysis.mean_delta_e < 6.0,
            "mean ΔE was {}",
            analysis.mean_delta_e
        );
    }

    #[test]
    fn small_chart_survives_larger_background_components() {
        let frame = synthetic_small_color_checker_with_large_clutter();
        let input = ColorInputKey {
            document_id: DocumentId::from_raw(11),
            generation: 7,
        };
        let analysis = analyze_rgba8(
            input,
            "small-chart-clutter.png".to_owned(),
            &frame,
            LightSourcePreset::D65,
            ColorChartKind::ColorChecker24Nov2014AndNewer,
        )
        .unwrap();

        assert_eq!(analysis.detection_mode, ChartDetectionMode::AutoGrid);
        assert!(
            analysis.chart_roi.x > 360,
            "detected the larger clutter grid instead of the small chart: {:?}",
            analysis.chart_roi
        );
        assert!(
            analysis.mean_delta_e < 6.0,
            "mean ΔE was {}",
            analysis.mean_delta_e
        );
    }

    #[test]
    fn hundreds_of_adaptive_candidates_are_bounded_before_grid_search() {
        let frame = synthetic_small_color_checker_with_hundreds_of_clutter();
        let candidates = detect_adaptive_hole_patch_candidates(&frame).unwrap();
        assert!(
            candidates.len() > MAX_GRID_SEARCH_CANDIDATES,
            "candidate count {} did not exercise bounded retention",
            candidates.len()
        );
        let retained = bounded_grid_search_candidates(&candidates);
        assert_eq!(
            retained.len(),
            MAX_GRID_SEARCH_CANDIDATES,
            "retention refill must keep the bounded set full"
        );
        let input = ColorInputKey {
            document_id: DocumentId::from_raw(12),
            generation: 8,
        };
        let analysis = analyze_rgba8(
            input,
            "small-chart-hundreds-clutter.png".to_owned(),
            &frame,
            LightSourcePreset::D65,
            ColorChartKind::ColorChecker24Nov2014AndNewer,
        )
        .unwrap();

        assert_eq!(analysis.detection_mode, ChartDetectionMode::AutoGrid);
        assert!(
            analysis.chart_roi.x > 700,
            "detected clutter instead of the small chart: {:?}",
            analysis.chart_roi
        );
    }

    #[test]
    fn automatic_failure_then_manual_four_corner_success() {
        let corners = [
            ColorImagePoint::new(50.0, 40.0),
            ColorImagePoint::new(290.0, 44.0),
            ColorImagePoint::new(288.0, 204.0),
            ColorImagePoint::new(48.0, 200.0),
        ];
        let frame = synthetic_manual_only_flat_chart(corners);
        let input = ColorInputKey {
            document_id: DocumentId::from_raw(8),
            generation: 4,
        };
        let automatic = analyze_rgba8(
            input,
            "manual-only.png".to_owned(),
            &frame,
            LightSourcePreset::D65,
            ColorChartKind::ColorChecker24Nov2014AndNewer,
        )
        .unwrap_err();
        assert!(
            automatic.contains("adaptive hole candidates"),
            "automatic failure was {automatic}"
        );

        let analysis = analyze_rgba8_with_corners(
            input,
            "manual-only.png".to_owned(),
            &frame,
            corners,
            ChartDetectionMode::ManualCorners,
            LightSourcePreset::D65,
            ColorChartKind::ColorChecker24Nov2014AndNewer,
        )
        .unwrap();

        assert_eq!(analysis.detection_mode, ChartDetectionMode::ManualCorners);
        assert_eq!(analysis.patches.len(), COLOR_CHECKER_PATCHES);
        assert!(analysis.mean_delta_e.is_finite());
    }

    #[test]
    fn invalid_manual_corners_are_rejected() {
        let frame = synthetic_contiguous_color_checker([
            ColorImagePoint::new(50.0, 40.0),
            ColorImagePoint::new(290.0, 44.0),
            ColorImagePoint::new(288.0, 204.0),
            ColorImagePoint::new(48.0, 200.0),
        ]);
        let input = ColorInputKey {
            document_id: DocumentId::from_raw(9),
            generation: 5,
        };
        let error = analyze_rgba8_with_corners(
            input,
            "bad-corners.png".to_owned(),
            &frame,
            [
                ColorImagePoint::new(10.0, 10.0),
                ColorImagePoint::new(12.0, 10.0),
                ColorImagePoint::new(12.0, 11.0),
                ColorImagePoint::new(10.0, 11.0),
            ],
            ChartDetectionMode::ManualCorners,
            LightSourcePreset::D65,
            ColorChartKind::ColorChecker24Nov2014AndNewer,
        )
        .unwrap_err();
        assert!(error.contains("too little area"), "error was {error}");
    }

    fn synthetic_projective_color_checker(corners: [ColorImagePoint; 4]) -> Rgba8Frame {
        synthetic_color_checker_with_layout(420, 300, corners, 0.72)
    }

    fn synthetic_contiguous_color_checker(corners: [ColorImagePoint; 4]) -> Rgba8Frame {
        synthetic_color_checker_with_layout(340, 240, corners, 1.0)
    }

    fn synthetic_color_checker_with_layout(
        width: u32,
        height: u32,
        corners: [ColorImagePoint; 4],
        fill_fraction: f64,
    ) -> Rgba8Frame {
        let mut pixels = vec![18_u8; width as usize * height as usize * 4];
        for chunk in pixels.chunks_exact_mut(4) {
            chunk[0] = 18;
            chunk[1] = 18;
            chunk[2] = 18;
            chunk[3] = 255;
        }
        paint_projective_color_checker(&mut pixels, width, height, corners, fill_fraction);
        Rgba8Frame::tight(width, height, std::sync::Arc::from(pixels)).unwrap()
    }

    fn synthetic_manual_only_flat_chart(corners: [ColorImagePoint; 4]) -> Rgba8Frame {
        let width = 340_u32;
        let height = 240_u32;
        let mut pixels = vec![18_u8; width as usize * height as usize * 4];
        for chunk in pixels.chunks_exact_mut(4) {
            chunk[0] = 18;
            chunk[1] = 18;
            chunk[2] = 18;
            chunk[3] = 255;
        }
        paint_polygon_rgb(&mut pixels, width, height, corners, [96, 96, 96]);
        Rgba8Frame::tight(width, height, std::sync::Arc::from(pixels)).unwrap()
    }

    fn synthetic_color_checker_on_dark_substrate(corners: [ColorImagePoint; 4]) -> Rgba8Frame {
        let width = 420_u32;
        let height = 300_u32;
        let mut pixels = vec![0_u8; width as usize * height as usize * 4];
        for chunk in pixels.chunks_exact_mut(4) {
            chunk[0] = 96;
            chunk[1] = 74;
            chunk[2] = 54;
            chunk[3] = 255;
        }
        paint_polygon_rgb(&mut pixels, width, height, corners, [30, 31, 33]);
        paint_projective_color_checker(&mut pixels, width, height, corners, 0.72);
        Rgba8Frame::tight(width, height, std::sync::Arc::from(pixels)).unwrap()
    }

    fn synthetic_sparse_color_checker_on_dark_substrate(
        corners: [ColorImagePoint; 4],
    ) -> Rgba8Frame {
        let width = 420_u32;
        let height = 300_u32;
        let mut pixels = vec![0_u8; width as usize * height as usize * 4];
        for chunk in pixels.chunks_exact_mut(4) {
            chunk[0] = 96;
            chunk[1] = 74;
            chunk[2] = 54;
            chunk[3] = 255;
        }
        let substrate = [30, 31, 33];
        paint_polygon_rgb(&mut pixels, width, height, corners, substrate);
        paint_projective_color_checker(&mut pixels, width, height, corners, 0.72);
        paint_projective_color_checker_patch_rgb(
            &mut pixels,
            width,
            height,
            corners,
            0,
            0,
            0.72,
            substrate,
        );
        paint_projective_color_checker_patch_rgb(
            &mut pixels,
            width,
            height,
            corners,
            3,
            5,
            0.72,
            substrate,
        );
        Rgba8Frame::tight(width, height, std::sync::Arc::from(pixels)).unwrap()
    }

    fn synthetic_small_color_checker_with_large_clutter() -> Rgba8Frame {
        let width = 640_u32;
        let height = 420_u32;
        let mut pixels = vec![18_u8; width as usize * height as usize * 4];
        for chunk in pixels.chunks_exact_mut(4) {
            chunk[0] = 18;
            chunk[1] = 18;
            chunk[2] = 18;
            chunk[3] = 255;
        }
        for row in 0..COLOR_CHECKER_ROWS {
            for column in 0..COLOR_CHECKER_COLUMNS {
                let x = 40 + column as u32 * 54;
                let y = 40 + row as u32 * 42;
                let rgb = [
                    70 + row as u8 * 11,
                    96 + column as u8 * 5,
                    135_u8.saturating_sub(row as u8 * 8),
                ];
                paint_rect_rgb(&mut pixels, width, x, y, 34, 26, rgb);
            }
        }
        paint_projective_color_checker(
            &mut pixels,
            width,
            height,
            [
                ColorImagePoint::new(390.0, 250.0),
                ColorImagePoint::new(570.0, 242.0),
                ColorImagePoint::new(578.0, 360.0),
                ColorImagePoint::new(386.0, 368.0),
            ],
            0.72,
        );
        Rgba8Frame::tight(width, height, std::sync::Arc::from(pixels)).unwrap()
    }

    fn synthetic_small_color_checker_with_hundreds_of_clutter() -> Rgba8Frame {
        let width = 960_u32;
        let height = 720_u32;
        let mut pixels = vec![18_u8; width as usize * height as usize * 4];
        for chunk in pixels.chunks_exact_mut(4) {
            chunk[0] = 18;
            chunk[1] = 18;
            chunk[2] = 18;
            chunk[3] = 255;
        }
        for row in 0..18_u32 {
            for column in 0..24_u32 {
                let x = 24 + column * 30;
                let y = 24 + row * 24;
                let rgb = [
                    62 + ((row * 7 + column * 3) % 120) as u8,
                    78 + ((row * 5 + column * 11) % 110) as u8,
                    70 + ((row * 13 + column * 2) % 120) as u8,
                ];
                paint_rect_rgb(&mut pixels, width, x, y, 20, 18, rgb);
            }
        }
        paint_projective_color_checker(
            &mut pixels,
            width,
            height,
            [
                ColorImagePoint::new(760.0, 520.0),
                ColorImagePoint::new(900.0, 514.0),
                ColorImagePoint::new(906.0, 615.0),
                ColorImagePoint::new(756.0, 622.0),
            ],
            0.72,
        );
        Rgba8Frame::tight(width, height, std::sync::Arc::from(pixels)).unwrap()
    }

    fn paint_polygon_rgb(
        pixels: &mut [u8],
        width: u32,
        height: u32,
        polygon: [ColorImagePoint; 4],
        rgb: [u8; 3],
    ) {
        let roi = bounding_roi(&polygon, width, height).unwrap();
        let y_end = roi.y.saturating_add(roi.height).min(height);
        let x_end = roi.x.saturating_add(roi.width).min(width);
        for y in roi.y..y_end {
            for x in roi.x..x_end {
                let point = ColorImagePoint::new(f64::from(x) + 0.5, f64::from(y) + 0.5);
                if point_in_convex_quad(point, polygon) {
                    paint_pixel(pixels, width, x, y, rgb);
                }
            }
        }
    }

    fn paint_projective_color_checker(
        pixels: &mut [u8],
        width: u32,
        height: u32,
        corners: [ColorImagePoint; 4],
        fill_fraction: f64,
    ) {
        let references = color_checker_references(ColorChartKind::default());
        let homography = Homography::from_unit_square(corners).unwrap();
        let inset = ((1.0 - fill_fraction) * 0.5).max(0.0);
        for row in 0..COLOR_CHECKER_ROWS {
            for column in 0..COLOR_CHECKER_COLUMNS {
                let patch = references[row * COLOR_CHECKER_COLUMNS + column];
                let u0 = (column as f64 + inset) / COLOR_CHECKER_COLUMNS as f64;
                let u1 = (column as f64 + 1.0 - inset) / COLOR_CHECKER_COLUMNS as f64;
                let v0 = (row as f64 + inset) / COLOR_CHECKER_ROWS as f64;
                let v1 = (row as f64 + 1.0 - inset) / COLOR_CHECKER_ROWS as f64;
                let polygon = [
                    homography.map(u0, v0).unwrap(),
                    homography.map(u1, v0).unwrap(),
                    homography.map(u1, v1).unwrap(),
                    homography.map(u0, v1).unwrap(),
                ];
                let roi = bounding_roi(&polygon, width, height).unwrap();
                let y_end = roi.y.saturating_add(roi.height).min(height);
                let x_end = roi.x.saturating_add(roi.width).min(width);
                for y in roi.y..y_end {
                    for x in roi.x..x_end {
                        let point = ColorImagePoint::new(f64::from(x) + 0.5, f64::from(y) + 0.5);
                        if point_in_convex_quad(point, polygon) {
                            paint_pixel(pixels, width, x, y, patch.display_srgb);
                        }
                    }
                }
            }
        }
    }

    fn paint_projective_color_checker_patch_rgb(
        pixels: &mut [u8],
        width: u32,
        height: u32,
        corners: [ColorImagePoint; 4],
        row: usize,
        column: usize,
        fill_fraction: f64,
        rgb: [u8; 3],
    ) {
        let homography = Homography::from_unit_square(corners).unwrap();
        let inset = ((1.0 - fill_fraction) * 0.5).max(0.0);
        let u0 = (column as f64 + inset) / COLOR_CHECKER_COLUMNS as f64;
        let u1 = (column as f64 + 1.0 - inset) / COLOR_CHECKER_COLUMNS as f64;
        let v0 = (row as f64 + inset) / COLOR_CHECKER_ROWS as f64;
        let v1 = (row as f64 + 1.0 - inset) / COLOR_CHECKER_ROWS as f64;
        let polygon = [
            homography.map(u0, v0).unwrap(),
            homography.map(u1, v0).unwrap(),
            homography.map(u1, v1).unwrap(),
            homography.map(u0, v1).unwrap(),
        ];
        paint_polygon_rgb(pixels, width, height, polygon, rgb);
    }

    fn paint_rect_rgb(
        pixels: &mut [u8],
        width: u32,
        x: u32,
        y: u32,
        rect_width: u32,
        rect_height: u32,
        rgb: [u8; 3],
    ) {
        for yy in y..y.saturating_add(rect_height) {
            for xx in x..x.saturating_add(rect_width) {
                paint_pixel(pixels, width, xx, yy, rgb);
            }
        }
    }

    fn paint_pixel(pixels: &mut [u8], width: u32, x: u32, y: u32, rgb: [u8; 3]) {
        let offset = (y as usize * width as usize + x as usize) * 4;
        pixels[offset] = rgb[0];
        pixels[offset + 1] = rgb[1];
        pixels[offset + 2] = rgb[2];
        pixels[offset + 3] = 255;
    }
}
