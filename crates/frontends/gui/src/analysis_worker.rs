//! RAW/Display histogram 后台分析与有界 keyed cache。

use std::{
    collections::VecDeque,
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
};

use camera_toolbox_core::{
    RawFrame, RawRoiAnalysis, Roi, RoiStats, analyze_raw_roi_with_cancel, analyze_roi_with_cancel,
};
use eframe::egui::{self, ColorImage};

use crate::{histogram_link::display_histogram_sample, workspace::DocumentId};

const DISPLAY_BIN_COUNT: usize = 256;
const DEFAULT_CACHE_CAPACITY: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum AnalysisDomain {
    RawBayer,
    DisplayRgb,
}

impl AnalysisDomain {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::RawBayer => "RAW Bayer",
            Self::DisplayRgb => "Display RGB",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct AnalysisKey {
    pub(crate) document_id: DocumentId,
    pub(crate) generation: u64,
    pub(crate) source_revision: Option<u64>,
    pub(crate) roi: Roi,
    pub(crate) domain: AnalysisDomain,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DisplayHistogramSeries {
    bins: Vec<u64>,
    pub(crate) sample_count: u64,
}

impl DisplayHistogramSeries {
    fn new() -> Self {
        Self {
            bins: vec![0; DISPLAY_BIN_COUNT],
            sample_count: 0,
        }
    }

    fn record(&mut self, value: u8) {
        self.sample_count = self.sample_count.saturating_add(1);
        self.bins[usize::from(value)] = self.bins[usize::from(value)].saturating_add(1);
    }

    pub(crate) fn bins(&self) -> &[u64] {
        &self.bins
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DisplayHistogram {
    pub(crate) r: DisplayHistogramSeries,
    pub(crate) g: DisplayHistogramSeries,
    pub(crate) b: DisplayHistogramSeries,
    pub(crate) luma: DisplayHistogramSeries,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct DisplayStats {
    pub(crate) luma_min: u8,
    pub(crate) luma_max: u8,
    pub(crate) luma_mean: f64,
    pub(crate) total_pixels: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DisplayRoiAnalysis {
    pub(crate) roi: Roi,
    pub(crate) display_stats: DisplayStats,
    pub(crate) histogram: DisplayHistogram,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum AnalysisData {
    Raw(RawRoiAnalysis),
    Display(DisplayRoiAnalysis),
}

impl AnalysisData {
    pub(crate) fn estimated_bytes(&self) -> usize {
        match self {
            Self::Raw(analysis) => {
                let histogram = &analysis.histogram;
                [
                    histogram.channels.r.bins().len(),
                    histogram.channels.gr.bins().len(),
                    histogram.channels.gb.bins().len(),
                    histogram.channels.b.bins().len(),
                    histogram.all.bins().len(),
                ]
                .into_iter()
                .fold(std::mem::size_of_val(analysis), |total, bins| {
                    total.saturating_add(bins.saturating_mul(std::mem::size_of::<u64>()))
                })
            }
            Self::Display(analysis) => [
                analysis.histogram.r.bins.len(),
                analysis.histogram.g.bins.len(),
                analysis.histogram.b.bins.len(),
                analysis.histogram.luma.bins.len(),
            ]
            .into_iter()
            .fold(std::mem::size_of_val(analysis), |total, bins| {
                total.saturating_add(bins.saturating_mul(std::mem::size_of::<u64>()))
            }),
        }
    }
}

pub(crate) struct AnalysisPayload {
    pub(crate) chart: Option<AnalysisData>,
    pub(crate) active_stats: RoiStats,
    pub(crate) active_roi: Roi,
}

pub(crate) struct AnalysisRequest {
    pub(crate) key: AnalysisKey,
    pub(crate) active_roi: Roi,
    pub(crate) compute_chart: bool,
    pub(crate) frame: Arc<RawFrame>,
    pub(crate) display_image: Option<Arc<ColorImage>>,
}

pub(crate) struct AnalysisResult {
    pub(crate) key: AnalysisKey,
    pub(crate) result: Result<AnalysisPayload, String>,
}

struct TicketedRequest {
    ticket: u64,
    request: AnalysisRequest,
}

#[derive(Default)]
struct RequestSlot {
    pending: Option<TicketedRequest>,
}

#[derive(Default)]
struct WorkerShared {
    request: Mutex<RequestSlot>,
    request_ready: Condvar,
    ready: Mutex<Option<AnalysisResult>>,
    shutdown: AtomicBool,
    desired_ticket: AtomicU64,
}

impl WorkerShared {
    fn submit(&self, request: AnalysisRequest) {
        let ticket = self.desired_ticket.fetch_add(1, Ordering::AcqRel) + 1;
        tracing::debug!(
            operation = "queue_histogram_analysis",
            ticket,
            generation = request.key.generation,
            revision = ?request.key.source_revision,
            roi_x = request.key.roi.x,
            roi_y = request.key.roi.y,
            roi_width = request.key.roi.width,
            roi_height = request.key.roi.height,
            domain = request.key.domain.label(),
            "queued histogram analysis"
        );
        lock(&self.request).pending = Some(TicketedRequest { ticket, request });
        self.request_ready.notify_one();
    }

    fn is_current(&self, ticket: u64) -> bool {
        !self.shutdown.load(Ordering::Acquire)
            && self.desired_ticket.load(Ordering::Acquire) == ticket
    }
}

pub(crate) struct AnalysisWorker {
    shared: Arc<WorkerShared>,
    thread: Option<JoinHandle<()>>,
}

impl AnalysisWorker {
    pub(crate) fn new(context: &egui::Context) -> std::io::Result<Self> {
        let shared = Arc::new(WorkerShared::default());
        let worker_shared = Arc::clone(&shared);
        let context = context.clone();
        let thread = thread::Builder::new()
            .name("raw-histogram-analysis".to_owned())
            .spawn(move || run_worker(&worker_shared, &context))?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    pub(crate) fn submit(&self, request: AnalysisRequest) {
        self.shared.submit(request);
    }

    pub(crate) fn take_ready(&self) -> Option<AnalysisResult> {
        lock(&self.shared.ready).take()
    }
}

impl Drop for AnalysisWorker {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Release);
        self.shared.desired_ticket.fetch_add(1, Ordering::AcqRel);
        lock(&self.shared.request).pending = None;
        self.shared.request_ready.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run_worker(shared: &WorkerShared, context: &egui::Context) {
    while let Some(ticketed) = wait_for_request(shared) {
        if !shared.is_current(ticketed.ticket) {
            continue;
        }
        let result = analyze_request(shared, ticketed);
        let Some(result) = result else {
            continue;
        };
        lock(&shared.ready).replace(result);
        context.request_repaint_of(egui::ViewportId::ROOT);
    }
}

fn wait_for_request(shared: &WorkerShared) -> Option<TicketedRequest> {
    let mut request = lock(&shared.request);
    loop {
        if shared.shutdown.load(Ordering::Acquire) {
            return None;
        }
        if let Some(pending) = request.pending.take() {
            return Some(pending);
        }
        request = shared
            .request_ready
            .wait(request)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
}

fn analyze_request(shared: &WorkerShared, ticketed: TicketedRequest) -> Option<AnalysisResult> {
    let TicketedRequest { ticket, request } = ticketed;
    let result = if request.compute_chart {
        match request.key.domain {
            AnalysisDomain::RawBayer => {
                analyze_raw_roi_with_cancel(&request.frame, request.key.roi, || {
                    !shared.is_current(ticket)
                })
                .map_err(|error| error.to_string())
                .and_then(|analysis| {
                    let active_stats = if request.active_roi == request.key.roi {
                        analysis.stats
                    } else {
                        analyze_active_stats(shared, ticket, &request)?
                    };
                    Ok(AnalysisPayload {
                        chart: Some(AnalysisData::Raw(analysis)),
                        active_stats,
                        active_roi: request.active_roi,
                    })
                })
            }
            AnalysisDomain::DisplayRgb => analyze_display_request(shared, ticket, &request),
        }
    } else {
        analyze_active_stats(shared, ticket, &request).map(|active_stats| AnalysisPayload {
            chart: None,
            active_stats,
            active_roi: request.active_roi,
        })
    };
    if !shared.is_current(ticket) {
        tracing::debug!(
            operation = "run_histogram_analysis",
            ticket,
            generation = request.key.generation,
            domain = request.key.domain.label(),
            "discarded cancelled histogram analysis"
        );
        return None;
    }
    Some(AnalysisResult {
        key: request.key,
        result,
    })
}

fn analyze_active_stats(
    shared: &WorkerShared,
    ticket: u64,
    request: &AnalysisRequest,
) -> Result<RoiStats, String> {
    analyze_roi_with_cancel(&request.frame, request.active_roi, || {
        !shared.is_current(ticket)
    })
    .map_err(|error| error.to_string())
}

fn analyze_display_request(
    shared: &WorkerShared,
    ticket: u64,
    request: &AnalysisRequest,
) -> Result<AnalysisPayload, String> {
    let image = request
        .display_image
        .as_ref()
        .ok_or_else(|| "display RGB analysis requires an installed color preview".to_owned())?;
    let expected_size = [
        request.frame.spec.width as usize,
        request.frame.spec.height as usize,
    ];
    if image.size != expected_size {
        return Err(format!(
            "display preview size {:?} does not match RAW size {:?}",
            image.size, expected_size
        ));
    }
    let active_stats = analyze_active_stats(shared, ticket, request)?;
    let (roi, histogram, display_stats) =
        analyze_display_histogram(image, request.key.roi, || !shared.is_current(ticket))?;
    Ok(AnalysisPayload {
        chart: Some(AnalysisData::Display(DisplayRoiAnalysis {
            roi,
            display_stats,
            histogram,
        })),
        active_stats,
        active_roi: request.active_roi,
    })
}

#[allow(clippy::cast_precision_loss)] // 均值显示为 f64；像素累计值无需保持整数逐位精度。
fn analyze_display_histogram<F>(
    image: &ColorImage,
    roi: Roi,
    mut is_cancelled: F,
) -> Result<(Roi, DisplayHistogram, DisplayStats), String>
where
    F: FnMut() -> bool,
{
    let width = u32::try_from(image.size[0]).map_err(|_| "display width exceeds u32".to_owned())?;
    let height =
        u32::try_from(image.size[1]).map_err(|_| "display height exceeds u32".to_owned())?;
    let roi = roi
        .clamped_to(width, height)
        .ok_or_else(|| "display analysis ROI is empty".to_owned())?;
    let mut histogram = DisplayHistogram {
        r: DisplayHistogramSeries::new(),
        g: DisplayHistogramSeries::new(),
        b: DisplayHistogramSeries::new(),
        luma: DisplayHistogramSeries::new(),
    };
    let mut luma_min = u8::MAX;
    let mut luma_max = u8::MIN;
    let mut luma_sum: u128 = 0;
    let mut total_pixels: u64 = 0;
    let row_width = width as usize;
    for y in roi.y..roi.y + roi.height {
        if is_cancelled() {
            return Err("analysis cancelled".to_owned());
        }
        let row_start = y as usize * row_width;
        for x in roi.x..roi.x + roi.width {
            let pixel = image.pixels[row_start + x as usize];
            let sample = display_histogram_sample(pixel);
            histogram.r.record(sample.r);
            histogram.g.record(sample.g);
            histogram.b.record(sample.b);
            histogram.luma.record(sample.luma);
            luma_min = luma_min.min(sample.luma);
            luma_max = luma_max.max(sample.luma);
            luma_sum = luma_sum.saturating_add(u128::from(sample.luma));
            total_pixels = total_pixels.saturating_add(1);
        }
    }
    Ok((
        roi,
        histogram,
        DisplayStats {
            luma_min,
            luma_max,
            luma_mean: luma_sum as f64 / total_pixels as f64,
            total_pixels,
        },
    ))
}

pub(crate) struct AnalysisCache {
    capacity: usize,
    entries: VecDeque<(AnalysisKey, Arc<AnalysisData>)>,
}

impl Default for AnalysisCache {
    fn default() -> Self {
        Self {
            capacity: DEFAULT_CACHE_CAPACITY,
            entries: VecDeque::new(),
        }
    }
}

impl AnalysisCache {
    pub(crate) fn get(&mut self, key: AnalysisKey) -> Option<Arc<AnalysisData>> {
        let index = self
            .entries
            .iter()
            .position(|(candidate, _)| *candidate == key)?;
        let entry = self.entries.remove(index)?;
        let value = Arc::clone(&entry.1);
        self.entries.push_back(entry);
        Some(value)
    }

    pub(crate) fn insert(&mut self, key: AnalysisKey, data: AnalysisData) -> Arc<AnalysisData> {
        if let Some(index) = self
            .entries
            .iter()
            .position(|(candidate, _)| *candidate == key)
        {
            self.entries.remove(index);
        }
        let value = Arc::new(data);
        self.entries.push_back((key, Arc::clone(&value)));
        while self.entries.len() > self.capacity {
            self.entries.pop_front();
        }
        value
    }

    #[cfg(test)]
    pub(crate) fn clear_generation(&mut self, generation: u64) {
        self.entries.retain(|(key, _)| key.generation != generation);
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }

    pub(crate) fn estimated_bytes(&self) -> usize {
        self.entries.iter().fold(0usize, |total, (_, data)| {
            total.saturating_add(data.estimated_bytes())
        })
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use camera_toolbox_core::{BayerPattern, RawSpec};

    fn frame() -> Arc<RawFrame> {
        Arc::new(
            RawFrame::new(
                RawSpec {
                    width: 2,
                    height: 1,
                    bit_depth: 8,
                    bayer: BayerPattern::Rggb,
                },
                vec![0, 255],
            )
            .unwrap(),
        )
    }

    fn key(generation: u64, revision: Option<u64>, domain: AnalysisDomain) -> AnalysisKey {
        AnalysisKey {
            document_id: DocumentId::from_raw(generation),
            generation,
            source_revision: revision,
            roi: Roi {
                x: 0,
                y: 0,
                width: 2,
                height: 1,
            },
            domain,
        }
    }

    fn raw_data() -> AnalysisData {
        AnalysisData::Raw(
            camera_toolbox_core::analyze_raw_roi(
                &frame(),
                Roi {
                    x: 0,
                    y: 0,
                    width: 2,
                    height: 1,
                },
            )
            .unwrap(),
        )
    }

    #[test]
    fn display_histogram_uses_exact_rgb_and_bt709_luma_bins() {
        let image = ColorImage::new(
            [2, 1],
            vec![egui::Color32::RED, egui::Color32::from_rgb(0, 255, 0)],
        );
        let (_, histogram, stats) = analyze_display_histogram(
            &image,
            Roi {
                x: 0,
                y: 0,
                width: 2,
                height: 1,
            },
            || false,
        )
        .unwrap();

        assert_eq!(histogram.r.bins()[255], 1);
        assert_eq!(histogram.g.bins()[255], 1);
        assert_eq!(histogram.b.bins()[0], 2);
        assert_eq!(histogram.luma.bins()[54], 1);
        assert_eq!(histogram.luma.bins()[182], 1);
        assert_eq!(stats.luma_min, 54);
        assert_eq!(stats.luma_max, 182);
        assert!((stats.luma_mean - 118.0).abs() < f64::EPSILON);
        assert_eq!(stats.total_pixels, 2);
    }

    #[test]
    fn cache_is_lru_bounded_and_generation_can_be_cleared() {
        let mut cache = AnalysisCache {
            capacity: 2,
            entries: VecDeque::new(),
        };
        let first = key(1, None, AnalysisDomain::RawBayer);
        let second = key(2, None, AnalysisDomain::RawBayer);
        let third = key(3, None, AnalysisDomain::RawBayer);
        cache.insert(first, raw_data());
        cache.insert(second, raw_data());
        assert!(cache.get(first).is_some());
        cache.insert(third, raw_data());

        assert_eq!(cache.len(), 2);
        assert!(cache.get(second).is_none());
        cache.clear_generation(1);
        assert!(cache.get(first).is_none());
        assert!(cache.get(third).is_some());
    }

    #[test]
    fn request_slot_and_ticket_keep_only_latest_key() {
        let shared = WorkerShared::default();
        for generation in [1, 2] {
            shared.submit(AnalysisRequest {
                key: key(generation, None, AnalysisDomain::RawBayer),
                active_roi: key(generation, None, AnalysisDomain::RawBayer).roi,
                compute_chart: true,
                frame: frame(),
                display_image: None,
            });
        }
        let pending = lock(&shared.request).pending.take().unwrap();
        assert_eq!(pending.request.key.generation, 2);
        assert!(shared.is_current(pending.ticket));
    }

    #[test]
    fn analysis_key_distinguishes_revision_roi_and_domain() {
        let raw = key(1, None, AnalysisDomain::RawBayer);
        assert_ne!(raw, key(1, Some(1), AnalysisDomain::DisplayRgb));
        let mut changed_roi = raw;
        changed_roi.roi.x = 1;
        assert_ne!(raw, changed_roi);
    }
}
