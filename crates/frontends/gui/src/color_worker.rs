//! 后台 Bayer 彩色纹理重建；输入与输出均为 latest-wins 单槽。

use std::{
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
};

use camera_toolbox_core::{
    ColorPipelineParams, ColorRenderDiagnostics, ColorRenderError, DisplayTransform, PreparedBayer,
    RawFrame,
};
use eframe::egui::{self, ColorImage};

pub(crate) struct ColorRenderRequest {
    pub(crate) frame_generation: u64,
    pub(crate) revision: u64,
    pub(crate) frame: Arc<RawFrame>,
    pub(crate) params: ColorPipelineParams,
}

pub(crate) struct RenderedColorImage {
    pub(crate) image: ColorImage,
    pub(crate) diagnostics: ColorRenderDiagnostics,
}

pub(crate) struct ColorRenderResult {
    pub(crate) frame_generation: u64,
    pub(crate) revision: u64,
    pub(crate) params: ColorPipelineParams,
    pub(crate) rendered: Result<RenderedColorImage, String>,
}

struct TicketedRequest {
    ticket: u64,
    request: ColorRenderRequest,
}

#[derive(Default)]
struct RequestSlot {
    pending: Option<TicketedRequest>,
}

#[derive(Default)]
struct WorkerShared {
    request: Mutex<RequestSlot>,
    request_ready: Condvar,
    ready: Mutex<Option<ColorRenderResult>>,
    shutdown: AtomicBool,
    desired_ticket: AtomicU64,
}

impl WorkerShared {
    fn submit(&self, request: ColorRenderRequest) {
        let ticket = self.desired_ticket.fetch_add(1, Ordering::AcqRel) + 1;
        lock(&self.request).pending = Some(TicketedRequest { ticket, request });
        self.request_ready.notify_one();
    }

    fn take_ready(&self) -> Option<ColorRenderResult> {
        lock(&self.ready).take()
    }

    fn is_current(&self, ticket: u64) -> bool {
        !self.shutdown.load(Ordering::Acquire)
            && self.desired_ticket.load(Ordering::Acquire) == ticket
    }
}

pub(crate) struct ColorRenderWorker {
    shared: Arc<WorkerShared>,
    thread: Option<JoinHandle<()>>,
}

impl ColorRenderWorker {
    pub(crate) fn new(context: &egui::Context) -> std::io::Result<Self> {
        let shared = Arc::new(WorkerShared::default());
        let worker_shared = Arc::clone(&shared);
        let context = context.clone();
        let thread = thread::Builder::new()
            .name("raw-color-render".to_owned())
            .spawn(move || run_worker(&worker_shared, &context))?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    pub(crate) fn submit(&self, request: ColorRenderRequest) {
        self.shared.submit(request);
    }

    pub(crate) fn cancel(&self) {
        self.shared.desired_ticket.fetch_add(1, Ordering::AcqRel);
        lock(&self.shared.request).pending = None;
        lock(&self.shared.ready).take();
        self.shared.request_ready.notify_all();
    }

    pub(crate) fn take_ready(&self) -> Option<ColorRenderResult> {
        self.shared.take_ready()
    }
}

impl Drop for ColorRenderWorker {
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
        let result = render_request(shared, ticketed);
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

fn render_request(shared: &WorkerShared, ticketed: TicketedRequest) -> Option<ColorRenderResult> {
    let TicketedRequest { ticket, request } = ticketed;
    let rendered = render_color_image(&request.frame, &request.params, || {
        !shared.is_current(ticket)
    });
    if !shared.is_current(ticket) {
        return None;
    }
    let rendered = match rendered {
        Ok(rendered) => Ok(rendered),
        Err(ColorRenderError::Cancelled) => return None,
        Err(error) => Err(error.to_string()),
    };
    Some(ColorRenderResult {
        frame_generation: request.frame_generation,
        revision: request.revision,
        params: request.params,
        rendered,
    })
}

fn render_color_image<F>(
    frame: &RawFrame,
    params: &ColorPipelineParams,
    mut is_cancelled: F,
) -> Result<RenderedColorImage, ColorRenderError>
where
    F: FnMut() -> bool,
{
    let prepared = PreparedBayer::new_with_cancel(frame, params, &mut is_cancelled)?;
    let display_transform = DisplayTransform::new(params.display_gamma)?;
    let width = prepared.width() as usize;
    let height = prepared.height() as usize;
    let pixel_count = width
        .checked_mul(height)
        .ok_or(camera_toolbox_core::RawFrameError::SizeOverflow)?;
    let mut pixels = vec![egui::Color32::BLACK; pixel_count];
    let mut diagnostics = ColorRenderDiagnostics::new(prepared.diagnostics());

    for y in 0..prepared.height() {
        if is_cancelled() {
            return Err(ColorRenderError::Cancelled);
        }
        let row_start = y as usize * width;
        for x in 0..prepared.width() {
            let (linear, missing) = prepared.linear_rgb_at(x, y)?;
            let (rgb8, clipped) = display_transform.encode(linear);
            diagnostics.record_pixel(missing, clipped);
            pixels[row_start + x as usize] = egui::Color32::from_rgb(rgb8.r, rgb8.g, rgb8.b);
        }
    }

    Ok(RenderedColorImage {
        image: ColorImage::new([width, height], pixels),
        diagnostics,
    })
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
    use std::time::{Duration, Instant};

    fn request(revision: u64) -> ColorRenderRequest {
        ColorRenderRequest {
            frame_generation: 7,
            revision,
            frame: Arc::new(
                RawFrame::new(
                    RawSpec {
                        width: 2,
                        height: 2,
                        bit_depth: 10,
                        bayer: BayerPattern::Rggb,
                    },
                    vec![1023, 512, 512, 0],
                )
                .unwrap(),
            ),
            params: ColorPipelineParams::for_spec(&RawSpec {
                width: 2,
                height: 2,
                bit_depth: 10,
                bayer: BayerPattern::Rggb,
            }),
        }
    }

    #[test]
    fn pending_request_slot_keeps_only_latest_revision() {
        let shared = WorkerShared::default();
        shared.submit(request(1));
        shared.submit(request(2));

        let pending = lock(&shared.request).pending.take().unwrap();
        assert_eq!(pending.request.revision, 2);
        assert_eq!(
            pending.ticket,
            shared.desired_ticket.load(Ordering::Acquire)
        );
    }

    #[test]
    fn ready_result_slot_keeps_only_latest_result() {
        let shared = WorkerShared::default();
        for revision in [1, 2] {
            lock(&shared.ready).replace(ColorRenderResult {
                frame_generation: 7,
                revision,
                params: request(revision).params,
                rendered: Err("test".to_owned()),
            });
        }

        assert_eq!(shared.take_ready().unwrap().revision, 2);
        assert!(shared.take_ready().is_none());
    }

    #[test]
    fn renders_color_and_accumulates_diagnostics() {
        let request = request(1);
        let rendered = render_color_image(&request.frame, &request.params, || false).unwrap();

        assert_eq!(rendered.image.size, [2, 2]);
        assert_eq!(rendered.diagnostics.prepare.out_of_range_samples, 0);
        assert_eq!(rendered.diagnostics.missing_neighbor_channels, 0);
    }

    #[test]
    fn rendering_uses_request_display_gamma_modes() {
        let default_request = request(1);
        let default =
            render_color_image(&default_request.frame, &default_request.params, || false).unwrap();

        let mut custom_request = request(2);
        custom_request.params.display_gamma = Some(1.4);
        let custom =
            render_color_image(&custom_request.frame, &custom_request.params, || false).unwrap();

        let mut linear_request = request(3);
        linear_request.params.display_gamma = None;
        let linear =
            render_color_image(&linear_request.frame, &linear_request.params, || false).unwrap();

        assert_ne!(custom.image.pixels, default.image.pixels);
        assert_ne!(custom.image.pixels, linear.image.pixels);
        assert_ne!(default.image.pixels, linear.image.pixels);
    }

    #[test]
    fn render_honors_preparation_cancellation() {
        let request = request(1);
        assert!(matches!(
            render_color_image(&request.frame, &request.params, || true),
            Err(ColorRenderError::Cancelled)
        ));
    }

    #[test]
    fn background_worker_eventually_publishes_latest_revision() {
        let context = egui::Context::default();
        let worker = ColorRenderWorker::new(&context).unwrap();
        for revision in 1..=10 {
            worker.submit(request(revision));
        }

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut published_revision = None;
        while Instant::now() < deadline {
            if let Some(result) = worker.take_ready() {
                published_revision = Some(result.revision);
                if result.revision == 10 {
                    break;
                }
            }
            thread::sleep(Duration::from_millis(5));
        }

        assert_eq!(published_revision, Some(10));
    }
}
