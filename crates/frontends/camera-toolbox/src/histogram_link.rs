//! Histogram 与 Viewer 双向联动的共享类型、显示域量化和空间高亮。

use std::{
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
};

use camera_toolbox_core::{CfaSite, NativeImage, Rgba8Frame};
use eframe::egui;

use crate::analysis_worker::{AnalysisDomain, AnalysisKey};

fn spatial_highlight_color() -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(255, 205, 32, 150)
}
const OVERLAY_FILL_CHUNK_PIXELS: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum HistogramSeriesId {
    RawR,
    RawGr,
    RawGb,
    RawB,
    RawAll,
    SourceR,
    SourceG,
    SourceB,
    SourceA,
    SourceY,
    SourceU,
    SourceV,
    DisplayR,
    DisplayG,
    DisplayB,
    DisplayLuma,
}

impl HistogramSeriesId {
    pub(crate) const fn raw_site(site: CfaSite) -> Self {
        match site {
            CfaSite::R => Self::RawR,
            CfaSite::Gr => Self::RawGr,
            CfaSite::Gb => Self::RawGb,
            CfaSite::B => Self::RawB,
        }
    }

    pub(crate) const fn from_plot_index(domain: AnalysisDomain, index: usize) -> Option<Self> {
        match (domain, index) {
            (AnalysisDomain::RawBayer, 0) => Some(Self::RawR),
            (AnalysisDomain::RawBayer, 1) => Some(Self::RawGr),
            (AnalysisDomain::RawBayer, 2) => Some(Self::RawGb),
            (AnalysisDomain::RawBayer, 3) => Some(Self::RawB),
            (AnalysisDomain::RawBayer, 4) => Some(Self::RawAll),
            (AnalysisDomain::SourceRgb, 0) => Some(Self::SourceR),
            (AnalysisDomain::SourceRgb, 1) => Some(Self::SourceG),
            (AnalysisDomain::SourceRgb, 2) => Some(Self::SourceB),
            (AnalysisDomain::SourceRgb, 3) => Some(Self::SourceA),
            (AnalysisDomain::SourceYuv, 0) => Some(Self::SourceY),
            (AnalysisDomain::SourceYuv, 1) => Some(Self::SourceU),
            (AnalysisDomain::SourceYuv, 2) => Some(Self::SourceV),
            (AnalysisDomain::DisplayRgb, 0) => Some(Self::DisplayR),
            (AnalysisDomain::DisplayRgb, 1) => Some(Self::DisplayG),
            (AnalysisDomain::DisplayRgb, 2) => Some(Self::DisplayB),
            (AnalysisDomain::DisplayRgb, 3) => Some(Self::DisplayLuma),
            _ => None,
        }
    }

    pub(crate) const fn plot_index(self) -> usize {
        match self {
            Self::RawR | Self::SourceR | Self::SourceY | Self::DisplayR => 0,
            Self::RawGr | Self::SourceG | Self::SourceU | Self::DisplayG => 1,
            Self::RawGb | Self::SourceB | Self::SourceV | Self::DisplayB => 2,
            Self::RawB | Self::SourceA | Self::DisplayLuma => 3,
            Self::RawAll => 4,
        }
    }

    pub(crate) const fn domain(self) -> AnalysisDomain {
        match self {
            Self::RawR | Self::RawGr | Self::RawGb | Self::RawB | Self::RawAll => {
                AnalysisDomain::RawBayer
            }
            Self::SourceR | Self::SourceG | Self::SourceB | Self::SourceA => {
                AnalysisDomain::SourceRgb
            }
            Self::SourceY | Self::SourceU | Self::SourceV => AnalysisDomain::SourceYuv,
            Self::DisplayR | Self::DisplayG | Self::DisplayB | Self::DisplayLuma => {
                AnalysisDomain::DisplayRgb
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DisplayHistogramSample {
    pub(crate) r: u8,
    pub(crate) g: u8,
    pub(crate) b: u8,
    pub(crate) luma: u8,
}

impl DisplayHistogramSample {
    pub(crate) const fn value(self, series: HistogramSeriesId) -> Option<u8> {
        match series {
            HistogramSeriesId::DisplayR => Some(self.r),
            HistogramSeriesId::DisplayG => Some(self.g),
            HistogramSeriesId::DisplayB => Some(self.b),
            HistogramSeriesId::DisplayLuma => Some(self.luma),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HistogramPixelSample {
    Raw { site: CfaSite, value: u16 },
    SourceRgb { r: u8, g: u8, b: u8, a: u8 },
    SourceYuv { y: u8, u: u8, v: u8 },
    Display(DisplayHistogramSample),
}

impl HistogramPixelSample {
    pub(crate) const fn value(self, series: HistogramSeriesId) -> Option<u8> {
        match (self, series) {
            (Self::SourceRgb { r, .. }, HistogramSeriesId::SourceR) => Some(r),
            (Self::SourceRgb { g, .. }, HistogramSeriesId::SourceG) => Some(g),
            (Self::SourceRgb { b, .. }, HistogramSeriesId::SourceB) => Some(b),
            (Self::SourceRgb { a, .. }, HistogramSeriesId::SourceA) => Some(a),
            (Self::SourceYuv { y, .. }, HistogramSeriesId::SourceY) => Some(y),
            (Self::SourceYuv { u, .. }, HistogramSeriesId::SourceU) => Some(u),
            (Self::SourceYuv { v, .. }, HistogramSeriesId::SourceV) => Some(v),
            (Self::Display(sample), series) => sample.value(series),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ImageHistogramHover {
    pub(crate) key: AnalysisKey,
    pub(crate) x: u32,
    pub(crate) y: u32,
    pub(crate) sample: HistogramPixelSample,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct HistogramBinSelection {
    pub(crate) key: AnalysisKey,
    pub(crate) series: HistogramSeriesId,
    pub(crate) bin_index: usize,
    pub(crate) lower_code: u16,
    pub(crate) upper_code: u16,
}

impl HistogramBinSelection {
    fn matches_code(self, code: u16) -> bool {
        code >= self.lower_code && code <= self.upper_code
    }
}

#[derive(Debug)]
pub(crate) struct SpatialMask {
    pub(crate) width: u32,
    pub(crate) height: u32,
    bits: Vec<u64>,
    pub(crate) matched_pixels: u64,
}

impl SpatialMask {
    #[cfg(test)]
    pub(crate) fn is_set(&self, index: usize) -> bool {
        self.bits
            .get(index / 64)
            .is_some_and(|word| word & (1u64 << (index % 64)) != 0)
    }
}

impl SpatialMask {
    fn estimated_bytes(&self) -> usize {
        self.bits.len().saturating_mul(std::mem::size_of::<u64>())
    }
}

fn spatial_mask_image_with_cancel<F>(
    mask: &SpatialMask,
    highlight_color: egui::Color32,
    mut is_cancelled: F,
) -> Option<Result<egui::ColorImage, String>>
where
    F: FnMut() -> bool,
{
    let Ok(width) = usize::try_from(mask.width) else {
        return Some(Err("spatial mask width exceeds host limits".to_owned()));
    };
    let Ok(height) = usize::try_from(mask.height) else {
        return Some(Err("spatial mask height exceeds host limits".to_owned()));
    };
    let Some(pixel_count) = width.checked_mul(height) else {
        return Some(Err("spatial overlay dimensions overflow".to_owned()));
    };
    let mut pixels = Vec::with_capacity(pixel_count);
    while pixels.len() < pixel_count {
        if is_cancelled() {
            return None;
        }
        let end = pixels
            .len()
            .saturating_add(OVERLAY_FILL_CHUNK_PIXELS)
            .min(pixel_count);
        pixels.resize(end, egui::Color32::TRANSPARENT);
    }
    for (word_index, word) in mask.bits.iter().copied().enumerate() {
        if word_index % 1024 == 0 && is_cancelled() {
            return None;
        }
        let mut remaining = word;
        while remaining != 0 {
            let bit = usize::try_from(remaining.trailing_zeros()).expect("bit index fits usize");
            let Some(index) = word_index
                .checked_mul(64)
                .and_then(|base| base.checked_add(bit))
            else {
                return Some(Err("spatial overlay index overflow".to_owned()));
            };
            if index < pixel_count {
                pixels[index] = highlight_color;
            }
            remaining &= remaining - 1;
        }
    }
    Some(Ok(egui::ColorImage::new([width, height], pixels)))
}

#[cfg(test)]
fn spatial_mask_image(
    mask: &SpatialMask,
    highlight_color: egui::Color32,
) -> Option<egui::ColorImage> {
    spatial_mask_image_with_cancel(mask, highlight_color, || false)?.ok()
}

fn spatial_highlight_payload<F>(
    mask: SpatialMask,
    is_cancelled: F,
) -> Option<Result<SpatialHighlightPayload, String>>
where
    F: FnMut() -> bool,
{
    if mask.matched_pixels == 0 {
        return Some(Ok(SpatialHighlightPayload {
            mask: Arc::new(mask),
            overlay_image: None,
        }));
    }
    let image =
        match spatial_mask_image_with_cancel(&mask, spatial_highlight_color(), is_cancelled)? {
            Ok(image) => image,
            Err(error) => return Some(Err(error)),
        };
    Some(Ok(SpatialHighlightPayload {
        mask: Arc::new(mask),
        overlay_image: Some(Arc::new(image)),
    }))
}

pub(crate) struct SpatialHighlight {
    pub(crate) selection: HistogramBinSelection,
    pub(crate) mask: Arc<SpatialMask>,
    pub(crate) overlay_image: Option<Arc<egui::ColorImage>>,
}

impl SpatialHighlight {
    pub(crate) fn estimated_bytes(&self) -> usize {
        let overlay_bytes = self.overlay_image.as_ref().map_or(0, |image| {
            // CPU ColorImage 与 Viewer 上传后的 GPU texture 各保守计一份 RGBA。
            image
                .pixels
                .len()
                .saturating_mul(std::mem::size_of::<egui::Color32>().saturating_mul(2))
        });
        self.mask.estimated_bytes().saturating_add(overlay_bytes)
    }
}

/// Display RGB analysis 的不可变采样源；避免为静态图像额外复制 ColorImage。
pub(crate) enum DisplayHistogramImage {
    Color(Arc<egui::ColorImage>),
    Rgba8(Arc<Rgba8Frame>),
}

impl DisplayHistogramImage {
    fn dimensions(&self) -> [u32; 2] {
        match self {
            Self::Color(image) => [image.size[0] as u32, image.size[1] as u32],
            Self::Rgba8(frame) => [frame.width, frame.height],
        }
    }

    fn sample(&self, x: u32, y: u32) -> Option<DisplayHistogramSample> {
        match self {
            Self::Color(image) => {
                let index = y as usize * image.size[0] + x as usize;
                image
                    .pixels
                    .get(index)
                    .copied()
                    .map(display_histogram_sample)
            }
            Self::Rgba8(frame) => frame
                .pixel(x, y)
                .map(|[r, g, b, _]| display_histogram_sample(egui::Color32::from_rgb(r, g, b))),
        }
    }
}

pub(crate) struct SpatialHighlightRequest {
    pub(crate) selection: HistogramBinSelection,
    pub(crate) native: NativeImage,
    pub(crate) display_image: Option<DisplayHistogramImage>,
}

pub(crate) struct SpatialHighlightPayload {
    pub(crate) mask: Arc<SpatialMask>,
    pub(crate) overlay_image: Option<Arc<egui::ColorImage>>,
}

pub(crate) struct SpatialHighlightResult {
    pub(crate) selection: HistogramBinSelection,
    pub(crate) result: Result<SpatialHighlightPayload, String>,
}

struct TicketedRequest {
    ticket: u64,
    request: SpatialHighlightRequest,
}

#[derive(Default)]
struct RequestSlot {
    pending: Option<TicketedRequest>,
}

#[derive(Default)]
struct WorkerShared {
    request: Mutex<RequestSlot>,
    request_ready: Condvar,
    ready: Mutex<Option<SpatialHighlightResult>>,
    shutdown: AtomicBool,
    desired_ticket: AtomicU64,
}

impl WorkerShared {
    fn is_current(&self, ticket: u64) -> bool {
        !self.shutdown.load(Ordering::Acquire)
            && self.desired_ticket.load(Ordering::Acquire) == ticket
    }
}

pub(crate) struct SpatialHighlightWorker {
    shared: Arc<WorkerShared>,
    thread: Option<JoinHandle<()>>,
}

impl SpatialHighlightWorker {
    pub(crate) fn new(context: &egui::Context) -> std::io::Result<Self> {
        let shared = Arc::new(WorkerShared::default());
        let worker_shared = Arc::clone(&shared);
        let context = context.clone();
        let thread = thread::Builder::new()
            .name("histogram-spatial-highlight".to_owned())
            .spawn(move || run_spatial_worker(&worker_shared, &context))?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    pub(crate) fn submit(&self, request: SpatialHighlightRequest) {
        let ticket = self.shared.desired_ticket.fetch_add(1, Ordering::AcqRel) + 1;
        lock(&self.shared.request).pending = Some(TicketedRequest { ticket, request });
        lock(&self.shared.ready).take();
        self.shared.request_ready.notify_one();
    }

    pub(crate) fn take_ready(&self) -> Option<SpatialHighlightResult> {
        lock(&self.shared.ready).take()
    }
}

impl Drop for SpatialHighlightWorker {
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

fn run_spatial_worker(shared: &WorkerShared, context: &egui::Context) {
    while let Some(ticketed) = wait_for_spatial_request(shared) {
        if !shared.is_current(ticketed.ticket) {
            continue;
        }
        let selection = ticketed.request.selection;
        let Some(mask_result) =
            build_spatial_mask(&ticketed.request, || !shared.is_current(ticketed.ticket))
        else {
            continue;
        };
        let Some(result) = (match mask_result {
            Ok(mask) => spatial_highlight_payload(mask, || !shared.is_current(ticketed.ticket)),
            Err(error) => Some(Err(error)),
        }) else {
            continue;
        };
        if !shared.is_current(ticketed.ticket) {
            continue;
        }
        lock(&shared.ready).replace(SpatialHighlightResult { selection, result });
        context.request_repaint_of(egui::ViewportId::ROOT);
    }
}

fn wait_for_spatial_request(shared: &WorkerShared) -> Option<TicketedRequest> {
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

fn build_spatial_mask<F>(
    request: &SpatialHighlightRequest,
    mut is_cancelled: F,
) -> Option<Result<SpatialMask, String>>
where
    F: FnMut() -> bool,
{
    let selection = request.selection;
    if selection.series.domain() != selection.key.domain {
        return Some(Err("histogram series/domain mismatch".to_owned()));
    }
    let [width, height] = request.native.dimensions();
    if selection.key.roi.clamped_to(width, height) != Some(selection.key.roi) {
        return Some(Err("histogram ROI is outside the source frame".to_owned()));
    }
    if selection.key.domain == AnalysisDomain::DisplayRgb {
        let Some(image) = request.display_image.as_ref() else {
            return Some(Err("display histogram source is unavailable".to_owned()));
        };
        if image.dimensions() != [width, height] {
            return Some(Err("display histogram source dimensions changed".to_owned()));
        }
    }
    let pixel_count = match (width as usize).checked_mul(height as usize) {
        Some(pixel_count) => pixel_count,
        None => {
            return Some(Err(
                "source dimensions overflow host address space".to_owned()
            ));
        }
    };
    let mut mask = SpatialMask {
        width,
        height,
        bits: vec![0; pixel_count.div_ceil(64)],
        matched_pixels: 0,
    };
    let roi = selection.key.roi;
    for y in roi.y..roi.y + roi.height {
        if is_cancelled() {
            return None;
        }
        for x in roi.x..roi.x + roi.width {
            let index = y as usize * width as usize + x as usize;
            let matches = match (&request.native, selection.key.domain) {
                (NativeImage::Raw(frame), AnalysisDomain::RawBayer) => {
                    let site_matches = selection.series == HistogramSeriesId::RawAll
                        || selection.series
                            == HistogramSeriesId::raw_site(frame.spec.bayer.site_at(x, y));
                    site_matches && selection.matches_code(frame.pixels()[index])
                }
                (NativeImage::Rgba8(frame), AnalysisDomain::SourceRgb) => frame
                    .pixel(x, y)
                    .and_then(|[r, g, b, a]| {
                        HistogramPixelSample::SourceRgb { r, g, b, a }.value(selection.series)
                    })
                    .is_some_and(|value| selection.matches_code(u16::from(value))),
                (NativeImage::Yuv420Sp(frame), AnalysisDomain::SourceYuv) => frame
                    .sample(x, y)
                    .and_then(|(y, u, v)| {
                        HistogramPixelSample::SourceYuv { y, u, v }.value(selection.series)
                    })
                    .is_some_and(|value| selection.matches_code(u16::from(value))),
                (_, AnalysisDomain::DisplayRgb) => request
                    .display_image
                    .as_ref()
                    .expect("validated above")
                    .sample(x, y)
                    .and_then(|sample| sample.value(selection.series))
                    .is_some_and(|value| selection.matches_code(u16::from(value))),
                _ => {
                    return Some(Err(
                        "histogram domain does not match native image".to_owned()
                    ));
                }
            };
            if matches {
                mask.bits[index / 64] |= 1u64 << (index % 64);
                mask.matched_pixels = mask.matched_pixels.saturating_add(1);
            }
        }
    }
    Some(Ok(mask))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) fn display_histogram_sample(pixel: egui::Color32) -> DisplayHistogramSample {
    let weighted =
        u32::from(pixel.r()) * 2126 + u32::from(pixel.g()) * 7152 + u32::from(pixel.b()) * 722;
    DisplayHistogramSample {
        r: pixel.r(),
        g: pixel.g(),
        b: pixel.b(),
        luma: u8::try_from((weighted + 5000) / 10_000)
            .expect("BT.709 weighted u8 channels stay within u8"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camera_toolbox_core::RawFrame;

    #[test]
    fn display_sample_uses_exact_bt709_integer_quantization() {
        let sample = display_histogram_sample(egui::Color32::from_rgb(255, 0, 0));
        assert_eq!(sample.r, 255);
        assert_eq!(sample.g, 0);
        assert_eq!(sample.b, 0);
        assert_eq!(sample.luma, 54);
    }

    fn test_frame() -> Arc<RawFrame> {
        Arc::new(
            RawFrame::new(
                camera_toolbox_core::RawSpec {
                    width: 2,
                    height: 2,
                    bit_depth: 8,
                    bayer: camera_toolbox_core::BayerPattern::Rggb,
                },
                vec![10, 10, 10, 20],
            )
            .unwrap(),
        )
    }

    fn selection(
        domain: AnalysisDomain,
        series: HistogramSeriesId,
        roi: camera_toolbox_core::Roi,
        code: u16,
    ) -> HistogramBinSelection {
        HistogramBinSelection {
            key: AnalysisKey {
                document_id: crate::workspace::DocumentId::from_raw(1),
                generation: 1,
                source_revision: (domain == AnalysisDomain::DisplayRgb).then_some(3),
                roi,
                domain,
            },
            series,
            bin_index: usize::from(code),
            lower_code: code,
            upper_code: code,
        }
    }

    #[test]
    fn raw_spatial_mask_is_full_frame_one_bit_and_respects_cfa_series() {
        let frame = test_frame();
        let roi = camera_toolbox_core::Roi {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        };
        let request = SpatialHighlightRequest {
            selection: selection(AnalysisDomain::RawBayer, HistogramSeriesId::RawR, roi, 10),
            native: NativeImage::Raw(frame),
            display_image: None,
        };
        let mask = build_spatial_mask(&request, || false).unwrap().unwrap();

        assert_eq!((mask.width, mask.height), (2, 2));
        assert_eq!(mask.bits.len(), 1);
        assert_eq!(mask.matched_pixels, 1);
        assert!(mask.is_set(0));
        assert!(!mask.is_set(1));
        assert!(!mask.is_set(2));
        let color = egui::Color32::from_rgba_unmultiplied(255, 210, 0, 150);
        let overlay = spatial_mask_image(&mask, color).unwrap();
        assert_eq!(overlay.pixels[0], color);
        assert_eq!(overlay.pixels[1], egui::Color32::TRANSPARENT);
    }

    #[test]
    fn spatial_mask_excludes_matching_pixels_outside_roi() {
        let frame = test_frame();
        let roi = camera_toolbox_core::Roi {
            x: 1,
            y: 0,
            width: 1,
            height: 1,
        };
        let request = SpatialHighlightRequest {
            selection: selection(AnalysisDomain::RawBayer, HistogramSeriesId::RawAll, roi, 10),
            native: NativeImage::Raw(frame),
            display_image: None,
        };
        let mask = build_spatial_mask(&request, || false).unwrap().unwrap();

        assert_eq!(mask.matched_pixels, 1);
        assert!(!mask.is_set(0));
        assert!(mask.is_set(1));
        assert!(!mask.is_set(2));
    }

    #[test]
    fn display_spatial_mask_reuses_histogram_luma_quantization() {
        let frame = test_frame();
        let image = Arc::new(egui::ColorImage::new(
            [2, 2],
            vec![
                egui::Color32::from_rgb(255, 0, 0),
                egui::Color32::BLACK,
                egui::Color32::from_rgb(255, 0, 0),
                egui::Color32::WHITE,
            ],
        ));
        let roi = camera_toolbox_core::Roi {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        };
        let request = SpatialHighlightRequest {
            selection: selection(
                AnalysisDomain::DisplayRgb,
                HistogramSeriesId::DisplayLuma,
                roi,
                54,
            ),
            native: NativeImage::Raw(frame),
            display_image: Some(DisplayHistogramImage::Color(image)),
        };
        let mask = build_spatial_mask(&request, || false).unwrap().unwrap();

        assert_eq!(mask.matched_pixels, 2);
        assert!(mask.is_set(0));
        assert!(mask.is_set(2));
        assert!(!mask.is_set(3));
    }

    #[test]
    fn static_display_spatial_mask_samples_display_rgba_without_copying() {
        let display = Arc::new(
            Rgba8Frame::tight(
                2,
                2,
                Arc::from([
                    10_u8, 1, 2, 255, 20, 3, 4, 255, 10, 5, 6, 255, 30, 7, 8, 255,
                ]),
            )
            .unwrap(),
        );
        let roi = camera_toolbox_core::Roi {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        };
        let request = SpatialHighlightRequest {
            selection: selection(
                AnalysisDomain::DisplayRgb,
                HistogramSeriesId::DisplayR,
                roi,
                10,
            ),
            native: NativeImage::Rgba8(Arc::clone(&display)),
            display_image: Some(DisplayHistogramImage::Rgba8(display)),
        };
        let mask = build_spatial_mask(&request, || false).unwrap().unwrap();

        assert_eq!(mask.matched_pixels, 2);
        assert!(mask.is_set(0));
        assert!(mask.is_set(2));
        assert!(!mask.is_set(1));
    }

    #[test]
    fn empty_bin_payload_keeps_mask_without_full_frame_overlay() {
        let frame = test_frame();
        let roi = camera_toolbox_core::Roi {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        };
        let request = SpatialHighlightRequest {
            selection: selection(AnalysisDomain::RawBayer, HistogramSeriesId::RawAll, roi, 99),
            native: NativeImage::Raw(frame),
            display_image: None,
        };
        let mask = build_spatial_mask(&request, || false).unwrap().unwrap();
        let payload = spatial_highlight_payload(mask, || false).unwrap().unwrap();

        assert_eq!(payload.mask.matched_pixels, 0);
        assert!(payload.overlay_image.is_none());
    }

    #[test]
    fn spatial_mask_build_observes_cancellation_before_rows() {
        let frame = test_frame();
        let roi = camera_toolbox_core::Roi {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        };
        let request = SpatialHighlightRequest {
            selection: selection(AnalysisDomain::RawBayer, HistogramSeriesId::RawAll, roi, 10),
            native: NativeImage::Raw(frame),
            display_image: None,
        };

        assert!(build_spatial_mask(&request, || true).is_none());
    }

    #[test]
    fn source_rgb_spatial_mask_matches_native_channel_codes() {
        let native = NativeImage::Rgba8(Arc::new(
            camera_toolbox_core::Rgba8Frame::tight(
                2,
                2,
                Arc::from([
                    10_u8, 1, 2, 255, 20, 3, 4, 255, 10, 5, 6, 255, 30, 7, 8, 255,
                ]),
            )
            .unwrap(),
        ));
        let roi = camera_toolbox_core::Roi {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        };
        let request = SpatialHighlightRequest {
            selection: selection(
                AnalysisDomain::SourceRgb,
                HistogramSeriesId::SourceR,
                roi,
                10,
            ),
            native,
            display_image: None,
        };
        let mask = build_spatial_mask(&request, || false).unwrap().unwrap();

        assert_eq!(mask.matched_pixels, 2);
        assert!(mask.is_set(0));
        assert!(mask.is_set(2));
        assert!(!mask.is_set(1));
    }

    #[test]
    fn source_yuv_spatial_mask_matches_native_chroma_codes() {
        let spec = camera_toolbox_core::Yuv420SpSpec {
            width: 2,
            height: 2,
            y_stride: 2,
            chroma_stride: 2,
            chroma_order: camera_toolbox_core::ChromaOrder::Uv,
            matrix: camera_toolbox_core::YuvMatrix::Bt601,
            range: camera_toolbox_core::YuvRange::Limited,
        };
        let native = NativeImage::Yuv420Sp(Arc::new(
            camera_toolbox_core::Yuv420SpFrame::from_contiguous(
                spec,
                Arc::new(vec![10, 20, 30, 40, 50, 60]),
            )
            .unwrap(),
        ));
        let roi = camera_toolbox_core::Roi {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        };
        let request = SpatialHighlightRequest {
            selection: selection(
                AnalysisDomain::SourceYuv,
                HistogramSeriesId::SourceU,
                roi,
                50,
            ),
            native,
            display_image: None,
        };
        let mask = build_spatial_mask(&request, || false).unwrap().unwrap();

        assert_eq!(mask.matched_pixels, 4);
        for index in 0..4 {
            assert!(mask.is_set(index));
        }
    }
}
