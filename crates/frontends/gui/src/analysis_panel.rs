//! Histogram Analysis 底部面板与展示状态。

use std::sync::Arc;

use camera_toolbox_core::{
    ChannelAnalysis8, HistogramSeries, NativeImage, RawRoiAnalysis, Roi, RoiStats,
};
use eframe::egui;
use egui_plot::{HoverPosition, Line, Plot, PlotPoint, PlotPoints, PlotUi, Points, VLine};

use crate::analysis_worker::{
    AnalysisCache, AnalysisData, AnalysisDomain, AnalysisKey, AnalysisResult,
    DisplayHistogramSeries,
};
use crate::histogram_link::{
    HistogramBinSelection, HistogramPixelSample, HistogramSeriesId, ImageHistogramHover,
};

const PANEL_DEFAULT_HEIGHT: f32 = 220.0;
const PANEL_MIN_HEIGHT: f32 = 160.0;
const PANEL_COLLAPSED_HEIGHT: f32 = 32.0;
const SERIES_STROKE_WIDTH: f32 = 1.5;
const SERIES_FILL_ALPHA: f32 = 0.08;
const PLOT_WHEEL_ZOOM_SPEED: f32 = 0.0015;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AnalysisScope {
    ActiveRoi,
    FullFrame,
}

impl AnalysisScope {
    const fn label(self) -> &'static str {
        match self {
            Self::ActiveRoi => "ROI",
            Self::FullFrame => "Full",
        }
    }

    pub(crate) const fn resolve(self, active_roi: Roi, width: u32, height: u32) -> Roi {
        match self {
            Self::ActiveRoi => active_roi,
            Self::FullFrame => Roi {
                x: 0,
                y: 0,
                width,
                height,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HistogramYMode {
    Normalized,
    Count,
}

impl HistogramYMode {
    const fn label(self) -> &'static str {
        match self {
            Self::Normalized => "Normalized",
            Self::Count => "Count",
        }
    }
}

struct PlotSeries {
    name: &'static str,
    color: egui::Color32,
    count_points: Vec<PlotPoint>,
    normalized_points: Vec<PlotPoint>,
}

struct PreparedPlot {
    series: Vec<PlotSeries>,
    saturation_x: Option<f64>,
    x_max: f64,
}

struct CurrentAnalysis {
    key: AnalysisKey,
    data: Arc<AnalysisData>,
    plot: PreparedPlot,
}

#[derive(Clone, Copy)]
struct ImagePlotMarker {
    series_index: usize,
    point: PlotPoint,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DesiredAnalysis {
    Ready,
    Pending,
    Failed,
    Submit,
}

#[derive(Default)]
pub(crate) struct AnalysisPanelResponse {
    pub(crate) selection_changed: bool,
    pub(crate) view_interacting: bool,
    pub(crate) hovered_bin: Option<HistogramBinSelection>,
}

pub(crate) struct AnalysisPanelState {
    pub(crate) expanded: bool,
    opened_once: bool,
    pub(crate) domain: AnalysisDomain,
    source_domain: AnalysisDomain,
    pub(crate) scope: AnalysisScope,
    y_mode: HistogramYMode,
    raw_visible: [bool; 5],
    display_visible: [bool; 4],
    desired_key: Option<AnalysisKey>,
    pending_key: Option<AnalysisKey>,
    error: Option<(AnalysisKey, String)>,
    current: Option<CurrentAnalysis>,
    cache: AnalysisCache,
    reset_view_requested: bool,
}

impl Default for AnalysisPanelState {
    fn default() -> Self {
        Self {
            expanded: false,
            opened_once: false,
            domain: AnalysisDomain::RawBayer,
            source_domain: AnalysisDomain::RawBayer,
            scope: AnalysisScope::ActiveRoi,
            y_mode: HistogramYMode::Normalized,
            raw_visible: [true, true, true, true, false],
            display_visible: [true; 4],
            desired_key: None,
            pending_key: None,
            error: None,
            current: None,
            cache: AnalysisCache::default(),
            reset_view_requested: false,
        }
    }
}

impl AnalysisPanelState {
    pub(crate) fn default_for_native(native: &NativeImage) -> Self {
        let source_domain = match native {
            NativeImage::Raw(_) => AnalysisDomain::RawBayer,
            NativeImage::Rgba8(_) => AnalysisDomain::SourceRgb,
            NativeImage::Yuv420Sp(_) => AnalysisDomain::SourceYuv,
        };
        Self {
            domain: source_domain,
            source_domain,
            ..Self::default()
        }
    }
    pub(crate) fn open_for_first_image(&mut self) {
        if !self.opened_once {
            self.expanded = true;
            self.opened_once = true;
        }
    }

    pub(crate) const fn panel_height(&self) -> f32 {
        if self.expanded {
            PANEL_DEFAULT_HEIGHT
        } else {
            PANEL_COLLAPSED_HEIGHT
        }
    }

    pub(crate) const fn min_height(&self) -> f32 {
        if self.expanded {
            PANEL_MIN_HEIGHT
        } else {
            PANEL_COLLAPSED_HEIGHT
        }
    }

    pub(crate) fn set_desired(&mut self, key: AnalysisKey) -> DesiredAnalysis {
        self.desired_key = Some(key);
        if self
            .current
            .as_ref()
            .is_some_and(|current| current.key == key)
        {
            return DesiredAnalysis::Ready;
        }
        if self.pending_key == Some(key) {
            return DesiredAnalysis::Pending;
        }
        if self
            .error
            .as_ref()
            .is_some_and(|(failed, _)| *failed == key)
        {
            return DesiredAnalysis::Failed;
        }
        if let Some(data) = self.cache.get(key) {
            self.install_current(key, data);
            return DesiredAnalysis::Ready;
        }
        self.current = None;
        self.pending_key = Some(key);
        self.error = None;
        DesiredAnalysis::Submit
    }

    pub(crate) fn accept_result(&mut self, result: AnalysisResult) -> Option<(Roi, RoiStats)> {
        if self.desired_key != Some(result.key) {
            return None;
        }
        self.pending_key = None;
        match result.result {
            Ok(payload) => {
                if let Some(data) = payload.chart {
                    let data = self.cache.insert(result.key, data);
                    self.install_current(result.key, data);
                }
                self.error = None;
                Some((payload.active_roi, payload.active_stats))
            }
            Err(error) => {
                self.current = None;
                self.error = Some((result.key, error));
                None
            }
        }
    }

    pub(crate) fn wait_for_source(&mut self) {
        self.desired_key = None;
        self.pending_key = None;
        self.error = None;
        self.current = None;
    }

    /// 全局 latest-wins worker 被其他文档占用后，保留 desired/UI 状态并允许切回时重提。
    pub(crate) fn mark_submission_superseded(&mut self) {
        self.pending_key = None;
    }

    #[cfg(test)]
    pub(crate) const fn pending_key(&self) -> Option<AnalysisKey> {
        self.pending_key
    }

    pub(crate) fn derived_resource_bytes(&self) -> usize {
        let plot_bytes = self.current.as_ref().map_or(0, |current| {
            current.plot.series.iter().fold(0usize, |total, series| {
                total
                    .saturating_add(
                        series
                            .count_points
                            .len()
                            .saturating_mul(std::mem::size_of::<PlotPoint>()),
                    )
                    .saturating_add(
                        series
                            .normalized_points
                            .len()
                            .saturating_mul(std::mem::size_of::<PlotPoint>()),
                    )
            })
        });
        self.cache.estimated_bytes().saturating_add(plot_bytes)
    }

    /// 保留展开、domain、scope、可见 series 等 UI 选择，只清理可重建分析结果。
    pub(crate) fn evict_derived(&mut self) {
        self.desired_key = None;
        self.pending_key = None;
        self.error = None;
        self.current = None;
        self.cache.clear();
        self.reset_view_requested = false;
    }

    pub(crate) fn has_current(&self, key: AnalysisKey) -> bool {
        self.current
            .as_ref()
            .is_some_and(|current| current.key == key)
    }

    pub(crate) fn current_key(&self) -> Option<AnalysisKey> {
        self.current.as_ref().map(|current| current.key)
    }

    fn request_reset_view(&mut self) {
        self.reset_view_requested = true;
    }

    fn take_reset_view_request(&mut self) -> bool {
        std::mem::take(&mut self.reset_view_requested)
    }

    fn install_current(&mut self, key: AnalysisKey, data: Arc<AnalysisData>) {
        let plot = prepare_plot(&data);
        self.current = Some(CurrentAnalysis { key, data, plot });
        self.request_reset_view();
    }
}

#[derive(Default)]
struct PlotInteraction {
    view_interacting: bool,
    hovered_bin: Option<HistogramBinSelection>,
}

pub(crate) fn render_analysis_panel(
    ui: &mut egui::Ui,
    state: &mut AnalysisPanelState,
    image_hover: Option<ImageHistogramHover>,
) -> AnalysisPanelResponse {
    let mut response = AnalysisPanelResponse::default();
    ui.horizontal(|ui| {
        ui.strong("Analysis");
        if state.expanded {
            ui.separator();
            let source_domain = state.source_domain;
            response.selection_changed |= ui
                .selectable_value(&mut state.domain, source_domain, source_domain.label())
                .changed();
            response.selection_changed |= ui
                .selectable_value(
                    &mut state.domain,
                    AnalysisDomain::DisplayRgb,
                    AnalysisDomain::DisplayRgb.label(),
                )
                .changed();
            ui.separator();
            response.selection_changed |= ui
                .selectable_value(
                    &mut state.scope,
                    AnalysisScope::ActiveRoi,
                    AnalysisScope::ActiveRoi.label(),
                )
                .changed();
            response.selection_changed |= ui
                .selectable_value(
                    &mut state.scope,
                    AnalysisScope::FullFrame,
                    AnalysisScope::FullFrame.label(),
                )
                .changed();
            ui.separator();
            state.reset_view_requested |= ui
                .selectable_value(
                    &mut state.y_mode,
                    HistogramYMode::Normalized,
                    HistogramYMode::Normalized.label(),
                )
                .changed();
            state.reset_view_requested |= ui
                .selectable_value(
                    &mut state.y_mode,
                    HistogramYMode::Count,
                    HistogramYMode::Count.label(),
                )
                .changed();
            if ui.button("Reset View").clicked() {
                state.request_reset_view();
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let (label, tooltip) = if state.expanded {
                ("-", "Collapse analysis")
            } else {
                ("+", "Expand analysis")
            };
            if ui.button(label).on_hover_text(tooltip).clicked() {
                state.expanded = !state.expanded;
            }
        });
    });
    if !state.expanded {
        return response;
    }

    ui.separator();
    if state.current.is_none() {
        if let Some((key, error)) = &state.error
            && state.desired_key == Some(*key)
        {
            ui.colored_label(egui::Color32::RED, format!("Analysis failed: {error}"));
        } else if state.pending_key == state.desired_key && state.pending_key.is_some() {
            ui.spinner();
            ui.label("Calculating…");
        } else if state.domain == AnalysisDomain::DisplayRgb {
            ui.label("Waiting for current color preview…");
        } else {
            ui.label("Analysis unavailable");
        }
        return response;
    }

    render_stats(ui, state);
    render_series_toggles(ui, state);
    let interaction = render_plot(ui, state, image_hover);
    response.view_interacting = interaction.view_interacting;
    response.hovered_bin = interaction.hovered_bin;
    response
}

fn render_stats(ui: &mut egui::Ui, state: &AnalysisPanelState) {
    let Some(current) = &state.current else {
        return;
    };
    let roi = current.key.roi;
    ui.horizontal_wrapped(|ui| {
        ui.label(format!(
            "{} · {} · x={} y={} {}×{}",
            current.key.domain.label(),
            state.scope.label(),
            roi.x,
            roi.y,
            roi.width,
            roi.height
        ));
        ui.separator();
        match current.data.as_ref() {
            AnalysisData::Raw(analysis) => {
                ui.label(format!(
                    "Min {} · Mean {:.2} · Max {} · At/above max {:.3}% · Overflow {}",
                    analysis.stats.min,
                    analysis.stats.mean,
                    analysis.stats.max,
                    analysis.stats.saturation_ratio() * 100.0,
                    analysis.histogram.total_overflow_count()
                ));
            }
            AnalysisData::Rgb(analysis) => {
                ui.label(format!(
                    "R {:.2} · G {:.2} · B {:.2} · A {:.2} · Pixels {}",
                    analysis.r.mean,
                    analysis.g.mean,
                    analysis.b.mean,
                    analysis.a.mean,
                    analysis.r.total_samples
                ));
            }
            AnalysisData::Yuv(analysis) => {
                ui.label(format!(
                    "Y {:.2} · U {:.2} · V {:.2} · Luma {} · Chroma {}",
                    analysis.y.mean,
                    analysis.u.mean,
                    analysis.v.mean,
                    analysis.y.total_samples,
                    analysis.u.total_samples
                ));
            }
            AnalysisData::Display(analysis) => {
                ui.label(format!(
                    "Display Y′ min {} · mean {:.2} · max {} · Pixels {} · rev {}",
                    analysis.display_stats.luma_min,
                    analysis.display_stats.luma_mean,
                    analysis.display_stats.luma_max,
                    analysis.display_stats.total_pixels,
                    current.key.source_revision.unwrap_or_default()
                ));
            }
        }
    });
}

fn render_series_toggles(ui: &mut egui::Ui, state: &mut AnalysisPanelState) {
    ui.horizontal(|ui| match state.domain {
        AnalysisDomain::RawBayer => {
            for (index, label) in ["R", "Gr", "Gb", "B", "All"].into_iter().enumerate() {
                ui.checkbox(&mut state.raw_visible[index], label);
            }
            if !state.raw_visible.iter().any(|visible| *visible) {
                state.raw_visible[0] = true;
            }
        }
        AnalysisDomain::SourceRgb => {
            for (index, label) in ["R", "G", "B", "A"].into_iter().enumerate() {
                ui.checkbox(&mut state.display_visible[index], label);
            }
            if !state.display_visible.iter().any(|visible| *visible) {
                state.display_visible[0] = true;
            }
        }
        AnalysisDomain::SourceYuv => {
            for (index, label) in ["Y", "U", "V"].into_iter().enumerate() {
                ui.checkbox(&mut state.display_visible[index], label);
            }
            if !state.display_visible[..3].iter().any(|visible| *visible) {
                state.display_visible[0] = true;
            }
        }
        AnalysisDomain::DisplayRgb => {
            for (index, label) in ["R", "G", "B", "Y′"].into_iter().enumerate() {
                ui.checkbox(&mut state.display_visible[index], label);
            }
            if !state.display_visible.iter().any(|visible| *visible) {
                state.display_visible[0] = true;
            }
        }
    });
}

fn render_plot(
    ui: &mut egui::Ui,
    state: &mut AnalysisPanelState,
    image_hover: Option<ImageHistogramHover>,
) -> PlotInteraction {
    let reset_view = state.take_reset_view_request();
    let y_mode = state.y_mode;
    let visibility = match state.domain {
        AnalysisDomain::RawBayer => state.raw_visible.as_slice(),
        AnalysisDomain::SourceRgb | AnalysisDomain::DisplayRgb => state.display_visible.as_slice(),
        AnalysisDomain::SourceYuv => &state.display_visible[..3],
    };
    let Some(current) = state.current.as_ref() else {
        return PlotInteraction::default();
    };
    let image_markers = prepare_image_plot_markers(current, y_mode, image_hover);
    let mut plot = Plot::new("analysis_histogram")
        .allow_drag(true)
        .allow_zoom(false)
        .allow_scroll(false)
        .allow_axis_zoom_drag(false)
        .allow_boxed_zoom(false)
        .allow_double_click_reset(false)
        .include_x(0.0)
        .include_x(current.plot.x_max)
        .include_y(0.0)
        .height(ui.available_height().max(100.0))
        .show_grid([true, true])
        .x_axis_formatter(|mark, _| format!("{:.0}", mark.value))
        .label_formatter(|hover| match hover {
            HoverPosition::NearDataPoint {
                plot_name,
                position,
                ..
            } => Some(histogram_tooltip(current, y_mode, plot_name, position)),
            HoverPosition::Elsewhere { .. } => None,
        });
    if reset_view {
        plot = plot.reset();
    }
    let plot_response = plot.show(ui, |plot_ui| {
        render_plot_items(plot_ui, current, y_mode, visibility, &image_markers)
    });
    let view_interacting = reset_view
        || plot_response.inner
        || plot_response
            .response
            .drag_started_by(egui::PointerButton::Primary)
        || plot_response
            .response
            .dragged_by(egui::PointerButton::Primary)
        || plot_response
            .response
            .drag_stopped_by(egui::PointerButton::Primary);
    let hovered_bin = if view_interacting {
        None
    } else {
        hovered_histogram_bin(current, &plot_response)
    };
    PlotInteraction {
        view_interacting,
        hovered_bin,
    }
}

fn render_plot_items<'a>(
    plot_ui: &mut PlotUi<'a>,
    current: &'a CurrentAnalysis,
    y_mode: HistogramYMode,
    visibility: &[bool],
    image_markers: &'a [Option<ImagePlotMarker>; 4],
) -> bool {
    let wheel_zoomed = apply_plot_wheel_zoom(plot_ui);
    for (index, series) in current.plot.series.iter().enumerate() {
        if !visibility.get(index).copied().unwrap_or(false) {
            continue;
        }
        let points = match y_mode {
            HistogramYMode::Normalized => &series.normalized_points,
            HistogramYMode::Count => &series.count_points,
        };
        let Some(series_id) = HistogramSeriesId::from_plot_index(current.key.domain, index) else {
            continue;
        };
        plot_ui.line(
            Line::new(series.name, PlotPoints::from(points.as_slice()))
                .id(histogram_plot_item_id(series_id))
                .color(series.color)
                .width(SERIES_STROKE_WIDTH)
                .fill(0.0)
                .fill_alpha(SERIES_FILL_ALPHA),
        );
    }
    if let Some(x) = current.plot.saturation_x {
        plot_ui.vline(
            VLine::new("Declared max", x)
                .color(egui::Color32::from_gray(150))
                .width(1.0),
        );
    }
    for marker in image_markers.iter().flatten() {
        if !visibility
            .get(marker.series_index)
            .copied()
            .unwrap_or(false)
        {
            continue;
        }
        let series = &current.plot.series[marker.series_index];
        plot_ui.points(
            Points::new(
                series.name,
                PlotPoints::from(std::slice::from_ref(&marker.point)),
            )
            .id(egui::Id::new((
                "image_histogram_hover",
                marker.series_index,
            )))
            .color(series.color)
            .radius(5.0)
            .filled(true)
            .allow_hover(false),
        );
    }
    wheel_zoomed
}

fn histogram_plot_item_id(series: HistogramSeriesId) -> egui::Id {
    egui::Id::new(("histogram_series", series))
}

fn hovered_histogram_bin<R>(
    current: &CurrentAnalysis,
    response: &egui_plot::PlotResponse<R>,
) -> Option<HistogramBinSelection> {
    let hovered_id = response.hovered_plot_item?;
    let series = (0..current.plot.series.len())
        .filter_map(|index| HistogramSeriesId::from_plot_index(current.key.domain, index))
        .find(|series| histogram_plot_item_id(*series) == hovered_id)?;
    let position = response.response.hover_pos()?;
    let plot_point = response.transform.value_from_position(position);
    histogram_bin_at_x(current, series, plot_point.x)
}

fn histogram_bin_at_x(
    current: &CurrentAnalysis,
    series: HistogramSeriesId,
    x: f64,
) -> Option<HistogramBinSelection> {
    if !x.is_finite() || x < 0.0 || series.domain() != current.key.domain {
        return None;
    }
    let points = &current.plot.series.get(series.plot_index())?.count_points;
    let bin_index = plot_bin_index(points, x)?;
    let (lower_code, upper_code) = match current.data.as_ref() {
        AnalysisData::Raw(analysis) => analysis.histogram.bin_code_range(bin_index)?,
        AnalysisData::Rgb(_) | AnalysisData::Yuv(_) | AnalysisData::Display(_) => {
            if bin_index >= 256 {
                return None;
            }
            let code = u16::try_from(bin_index).ok()?;
            (code, code)
        }
    };
    Some(HistogramBinSelection {
        key: current.key,
        series,
        bin_index,
        lower_code,
        upper_code,
    })
}

fn plot_bin_index(points: &[PlotPoint], x: f64) -> Option<usize> {
    let (first, last) = points.first().zip(points.last())?;
    if !x.is_finite() || x < first.x || x >= last.x {
        return None;
    }
    let boundary = points.partition_point(|point| point.x <= x);
    Some(boundary.saturating_sub(1) / 2)
}

fn tooltip_bin_index(points: &[PlotPoint], x: f64) -> Option<usize> {
    plot_bin_index(points, x).or_else(|| {
        let last = points.last()?;
        let tolerance = f64::EPSILON * last.x.abs().max(1.0);
        let last_bin = points.len().checked_div(2)?.checked_sub(1)?;
        ((x - last.x).abs() <= tolerance).then_some(last_bin)
    })
}

fn apply_plot_wheel_zoom(plot_ui: &mut PlotUi<'_>) -> bool {
    if !plot_ui.response().contains_pointer() {
        return false;
    }
    let (scroll, shift) = plot_ui
        .ctx()
        .input(|input| (input.smooth_scroll_delta, input.modifiers.shift));
    let Some(zoom) = plot_zoom_vector(scroll, shift) else {
        return false;
    };
    plot_ui.zoom_bounds_around_hovered(zoom);
    plot_ui.ctx().request_repaint();
    true
}

fn prepare_image_plot_markers(
    current: &CurrentAnalysis,
    y_mode: HistogramYMode,
    image_hover: Option<ImageHistogramHover>,
) -> [Option<ImagePlotMarker>; 4] {
    let mut markers = [None; 4];
    let Some(hover) = image_hover.filter(|hover| hover.key == current.key) else {
        return markers;
    };
    let analysis_roi = match current.data.as_ref() {
        AnalysisData::Raw(analysis) => analysis.roi,
        AnalysisData::Rgb(analysis) => analysis.roi,
        AnalysisData::Yuv(analysis) => analysis.roi,
        AnalysisData::Display(analysis) => analysis.roi,
    };
    if !analysis_roi.contains(hover.x, hover.y) {
        return markers;
    }
    let mut count = 0;
    let mut push_marker = |series_id: HistogramSeriesId, bin: usize| {
        let series_index = series_id.plot_index();
        let Some(series) = current.plot.series.get(series_index) else {
            return;
        };
        let points = match y_mode {
            HistogramYMode::Normalized => &series.normalized_points,
            HistogramYMode::Count => &series.count_points,
        };
        let Some((left, right)) = points.get(bin * 2).zip(points.get(bin * 2 + 1)) else {
            return;
        };
        markers[count] = Some(ImagePlotMarker {
            series_index,
            point: PlotPoint::new((left.x + right.x) * 0.5, left.y),
        });
        count += 1;
    };
    match (current.data.as_ref(), hover.sample) {
        (AnalysisData::Raw(analysis), HistogramPixelSample::Raw { site, value }) => {
            if let Some(bin) = analysis.histogram.bin_index(value) {
                push_marker(HistogramSeriesId::raw_site(site), bin);
                push_marker(HistogramSeriesId::RawAll, bin);
            }
        }
        (AnalysisData::Rgb(_), HistogramPixelSample::SourceRgb { r, g, b, a }) => {
            for (series_id, value) in [
                (HistogramSeriesId::SourceR, r),
                (HistogramSeriesId::SourceG, g),
                (HistogramSeriesId::SourceB, b),
                (HistogramSeriesId::SourceA, a),
            ] {
                push_marker(series_id, usize::from(value));
            }
        }
        (AnalysisData::Yuv(_), HistogramPixelSample::SourceYuv { y, u, v }) => {
            for (series_id, value) in [
                (HistogramSeriesId::SourceY, y),
                (HistogramSeriesId::SourceU, u),
                (HistogramSeriesId::SourceV, v),
            ] {
                push_marker(series_id, usize::from(value));
            }
        }
        (AnalysisData::Display(_), HistogramPixelSample::Display(sample)) => {
            for series_id in [
                HistogramSeriesId::DisplayR,
                HistogramSeriesId::DisplayG,
                HistogramSeriesId::DisplayB,
                HistogramSeriesId::DisplayLuma,
            ] {
                let bin = usize::from(sample.value(series_id).expect("Display series must map"));
                push_marker(series_id, bin);
            }
        }
        _ => {}
    }
    markers
}

fn plot_zoom_vector(scroll: egui::Vec2, shift: bool) -> Option<egui::Vec2> {
    let delta = if shift {
        if scroll.x.abs() > scroll.y.abs() {
            scroll.x
        } else {
            scroll.y
        }
    } else {
        scroll.y
    };
    if delta.abs() <= f32::EPSILON {
        return None;
    }
    let factor = (delta * PLOT_WHEEL_ZOOM_SPEED).exp();
    Some(if shift {
        egui::vec2(factor, 1.0)
    } else {
        egui::vec2(1.0, factor)
    })
}

fn prepare_plot(data: &AnalysisData) -> PreparedPlot {
    match data {
        AnalysisData::Raw(analysis) => prepare_raw_plot(analysis),
        AnalysisData::Rgb(analysis) => PreparedPlot {
            series: vec![
                prepare_channel_series("R", egui::Color32::from_rgb(244, 91, 105), &analysis.r),
                prepare_channel_series("G", egui::Color32::from_rgb(57, 193, 108), &analysis.g),
                prepare_channel_series("B", egui::Color32::from_rgb(77, 141, 255), &analysis.b),
                prepare_channel_series("A", egui::Color32::from_rgb(184, 189, 199), &analysis.a),
            ],
            saturation_x: None,
            x_max: 256.0,
        },
        AnalysisData::Yuv(analysis) => PreparedPlot {
            series: vec![
                prepare_channel_series("Y", egui::Color32::from_rgb(216, 220, 229), &analysis.y),
                prepare_channel_series("U", egui::Color32::from_rgb(77, 141, 255), &analysis.u),
                prepare_channel_series("V", egui::Color32::from_rgb(244, 91, 105), &analysis.v),
            ],
            saturation_x: None,
            x_max: 256.0,
        },
        AnalysisData::Display(analysis) => PreparedPlot {
            series: vec![
                prepare_display_series(
                    "R",
                    egui::Color32::from_rgb(244, 91, 105),
                    &analysis.histogram.r,
                ),
                prepare_display_series(
                    "G",
                    egui::Color32::from_rgb(57, 193, 108),
                    &analysis.histogram.g,
                ),
                prepare_display_series(
                    "B",
                    egui::Color32::from_rgb(77, 141, 255),
                    &analysis.histogram.b,
                ),
                prepare_display_series(
                    "Y′",
                    egui::Color32::from_rgb(216, 220, 229),
                    &analysis.histogram.luma,
                ),
            ],
            saturation_x: None,
            x_max: 256.0,
        },
    }
}

fn prepare_raw_plot(analysis: &RawRoiAnalysis) -> PreparedPlot {
    let histogram = &analysis.histogram;
    let bin_width = f64::from(histogram.bin_width());
    PreparedPlot {
        series: vec![
            prepare_raw_series(
                "R",
                egui::Color32::from_rgb(244, 91, 105),
                &histogram.channels.r,
                bin_width,
            ),
            prepare_raw_series(
                "Gr",
                egui::Color32::from_rgb(126, 217, 87),
                &histogram.channels.gr,
                bin_width,
            ),
            prepare_raw_series(
                "Gb",
                egui::Color32::from_rgb(38, 166, 154),
                &histogram.channels.gb,
                bin_width,
            ),
            prepare_raw_series(
                "B",
                egui::Color32::from_rgb(77, 141, 255),
                &histogram.channels.b,
                bin_width,
            ),
            prepare_raw_series(
                "All",
                egui::Color32::from_rgb(184, 189, 199),
                &histogram.all,
                bin_width,
            ),
        ],
        saturation_x: Some(f64::from(histogram.max_code)),
        x_max: f64::from(u32::from(histogram.max_code) + 1),
    }
}

fn prepare_raw_series(
    name: &'static str,
    color: egui::Color32,
    series: &HistogramSeries,
    bin_width: f64,
) -> PlotSeries {
    prepare_series(name, color, series.bins(), series.sample_count, bin_width)
}

fn prepare_display_series(
    name: &'static str,
    color: egui::Color32,
    series: &DisplayHistogramSeries,
) -> PlotSeries {
    prepare_series(name, color, series.bins(), series.sample_count, 1.0)
}

fn prepare_channel_series(
    name: &'static str,
    color: egui::Color32,
    series: &ChannelAnalysis8,
) -> PlotSeries {
    prepare_series(
        name,
        color,
        series.histogram.as_ref(),
        series.total_samples,
        1.0,
    )
}

#[allow(clippy::cast_precision_loss)] // Plot API 使用 f64；直方图计数仅在显示时近似转换。
fn prepare_series(
    name: &'static str,
    color: egui::Color32,
    bins: &[u64],
    sample_count: u64,
    bin_width: f64,
) -> PlotSeries {
    let mut count_points = Vec::with_capacity(bins.len() * 2);
    let mut normalized_points = Vec::with_capacity(bins.len() * 2);
    for (index, count) in bins.iter().copied().enumerate() {
        let x0 = index as f64 * bin_width;
        let x1 = (index + 1) as f64 * bin_width;
        let normalized = if sample_count == 0 {
            0.0
        } else {
            count as f64 * 100.0 / sample_count as f64
        };
        count_points.push(PlotPoint::new(x0, count as f64));
        count_points.push(PlotPoint::new(x1, count as f64));
        normalized_points.push(PlotPoint::new(x0, normalized));
        normalized_points.push(PlotPoint::new(x1, normalized));
    }
    PlotSeries {
        name,
        color,
        count_points,
        normalized_points,
    }
}

fn histogram_tooltip(
    current: &CurrentAnalysis,
    y_mode: HistogramYMode,
    name: &str,
    point: &PlotPoint,
) -> String {
    let Some(series) = current
        .plot
        .series
        .iter()
        .find(|series| series.name == name)
    else {
        return String::new();
    };
    let Some(index) = tooltip_bin_index(&series.count_points, point.x) else {
        return String::new();
    };
    let Some(count) = histogram_count(current.data.as_ref(), name, index) else {
        return String::new();
    };
    let percentage = series.normalized_points[index * 2].y;
    let lower = series.count_points[index * 2].x;
    let upper = series.count_points[index * 2 + 1].x - 1.0;
    let value = match y_mode {
        HistogramYMode::Normalized => format!("{percentage:.4}%"),
        HistogramYMode::Count => count.to_string(),
    };
    format!("{name}\nCode {lower:.0}–{upper:.0}\nCount {count}\nValue {value}")
}

fn histogram_count(data: &AnalysisData, name: &str, index: usize) -> Option<u64> {
    match data {
        AnalysisData::Raw(analysis) => match name {
            "R" => analysis.histogram.channels.r.bins().get(index).copied(),
            "Gr" => analysis.histogram.channels.gr.bins().get(index).copied(),
            "Gb" => analysis.histogram.channels.gb.bins().get(index).copied(),
            "B" => analysis.histogram.channels.b.bins().get(index).copied(),
            "All" => analysis.histogram.all.bins().get(index).copied(),
            _ => None,
        },
        AnalysisData::Rgb(analysis) => match name {
            "R" => analysis.r.histogram.get(index).copied(),
            "G" => analysis.g.histogram.get(index).copied(),
            "B" => analysis.b.histogram.get(index).copied(),
            "A" => analysis.a.histogram.get(index).copied(),
            _ => None,
        },
        AnalysisData::Yuv(analysis) => match name {
            "Y" => analysis.y.histogram.get(index).copied(),
            "U" => analysis.u.histogram.get(index).copied(),
            "V" => analysis.v.histogram.get(index).copied(),
            _ => None,
        },
        AnalysisData::Display(analysis) => match name {
            "R" => analysis.histogram.r.bins().get(index).copied(),
            "G" => analysis.histogram.g.bins().get(index).copied(),
            "B" => analysis.histogram.b.bins().get(index).copied(),
            "Y′" => analysis.histogram.luma.bins().get(index).copied(),
            _ => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camera_toolbox_core::{BayerPattern, RawFrame, RawSpec, analyze_raw_roi};

    fn assert_f64_close(actual: f64, expected: f64) {
        assert!((actual - expected).abs() <= f64::EPSILON * expected.abs().max(1.0));
    }

    fn assert_f32_close(actual: f32, expected: f32) {
        assert!((actual - expected).abs() <= f32::EPSILON * expected.abs().max(1.0));
    }

    #[test]
    fn step_points_are_precomputed_for_count_and_normalized_modes() {
        let series = prepare_series("R", egui::Color32::RED, &[1, 3], 4, 2.0);
        assert_eq!(series.count_points.len(), 4);
        assert_eq!(series.count_points[0], PlotPoint::new(0.0, 1.0));
        assert_eq!(series.count_points[1], PlotPoint::new(2.0, 1.0));
        assert_eq!(series.normalized_points[2], PlotPoint::new(2.0, 75.0));
        assert_eq!(series.normalized_points[3], PlotPoint::new(4.0, 75.0));
    }

    #[test]
    fn raw_plot_keeps_declared_max_and_overflow_outside_normal_bins() {
        let frame = RawFrame::new(
            RawSpec {
                width: 2,
                height: 1,
                bit_depth: 10,
                bayer: BayerPattern::Rggb,
            },
            vec![1023, 1024],
        )
        .unwrap();
        let analysis = analyze_raw_roi(
            &frame,
            Roi {
                x: 0,
                y: 0,
                width: 2,
                height: 1,
            },
        )
        .unwrap();
        let plot = prepare_raw_plot(&analysis);

        assert_f64_close(plot.x_max, 1024.0);
        assert_eq!(plot.saturation_x, Some(1023.0));
        assert_f64_close(plot.series[0].count_points.last().unwrap().x, plot.x_max);
        assert_eq!(analysis.histogram.total_overflow_count(), 1);
    }

    #[test]
    fn plot_wheel_zoom_targets_requested_axis_and_shift_component() {
        let vertical = plot_zoom_vector(egui::vec2(0.0, 120.0), false).unwrap();
        assert_f32_close(vertical.x, 1.0);
        assert!(vertical.y > 1.0);

        let horizontal = plot_zoom_vector(egui::vec2(120.0, 5.0), true).unwrap();
        assert!(horizontal.x > 1.0);
        assert_f32_close(horizontal.y, 1.0);

        let shift_fallback = plot_zoom_vector(egui::vec2(5.0, -120.0), true).unwrap();
        assert!(shift_fallback.x < 1.0);
        assert_f32_close(shift_fallback.y, 1.0);
    }

    #[test]
    fn reset_view_request_is_consumed_once() {
        let mut state = AnalysisPanelState::default();
        state.request_reset_view();

        assert!(state.take_reset_view_request());
        assert!(!state.take_reset_view_request());
    }

    #[test]
    fn image_hover_marker_is_suppressed_outside_analysis_roi() {
        let frame = RawFrame::new(
            RawSpec {
                width: 2,
                height: 1,
                bit_depth: 8,
                bayer: BayerPattern::Rggb,
            },
            vec![10, 20],
        )
        .unwrap();
        let roi = Roi {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        };
        let analysis = analyze_raw_roi(&frame, roi).unwrap();
        let key = AnalysisKey {
            document_id: crate::workspace::DocumentId::from_raw(7),
            generation: 7,
            source_revision: None,
            roi,
            domain: AnalysisDomain::RawBayer,
        };
        let current = CurrentAnalysis {
            key,
            plot: prepare_raw_plot(&analysis),
            data: Arc::new(AnalysisData::Raw(analysis)),
        };
        let outside = ImageHistogramHover {
            key,
            x: 1,
            y: 0,
            sample: HistogramPixelSample::Raw {
                site: frame.spec.bayer.site_at(1, 0),
                value: 20,
            },
        };
        assert!(
            prepare_image_plot_markers(&current, HistogramYMode::Count, Some(outside))
                .iter()
                .all(Option::is_none)
        );

        let inside = ImageHistogramHover {
            key,
            x: 0,
            y: 0,
            sample: HistogramPixelSample::Raw {
                site: frame.spec.bayer.site_at(0, 0),
                value: 10,
            },
        };
        let markers = prepare_image_plot_markers(&current, HistogramYMode::Count, Some(inside));
        assert_eq!(markers.iter().flatten().count(), 2);
        assert_f64_close(markers[0].unwrap().point.x, 10.5);
    }

    #[test]
    fn plot_hover_maps_x_to_exact_grouped_raw_bin_range() {
        let frame = RawFrame::new(
            RawSpec {
                width: 2,
                height: 1,
                bit_depth: 16,
                bayer: BayerPattern::Rggb,
            },
            vec![15, 16],
        )
        .unwrap();
        let roi = Roi {
            x: 0,
            y: 0,
            width: 2,
            height: 1,
        };
        let analysis = analyze_raw_roi(&frame, roi).unwrap();
        let current = CurrentAnalysis {
            key: AnalysisKey {
                document_id: crate::workspace::DocumentId::from_raw(1),
                generation: 1,
                source_revision: None,
                roi,
                domain: AnalysisDomain::RawBayer,
            },
            plot: prepare_raw_plot(&analysis),
            data: Arc::new(AnalysisData::Raw(analysis)),
        };

        let first = histogram_bin_at_x(&current, HistogramSeriesId::RawR, 15.9).unwrap();
        assert_eq!(first.bin_index, 0);
        assert_eq!((first.lower_code, first.upper_code), (0, 15));
        let second = histogram_bin_at_x(&current, HistogramSeriesId::RawGr, 16.0).unwrap();
        assert_eq!(second.bin_index, 1);
        assert_eq!((second.lower_code, second.upper_code), (16, 31));
        assert!(histogram_bin_at_x(&current, HistogramSeriesId::DisplayR, 16.0).is_none());
        assert!(histogram_bin_at_x(&current, HistogramSeriesId::RawR, 65_536.0).is_none());
        assert_eq!(
            tooltip_bin_index(&current.plot.series[0].count_points, 65_536.0),
            Some(4095)
        );
    }
}
