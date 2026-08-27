//! pongbot-calib-tool 原生 Dear ImGui 桌面前端。
//!
//! 技术栈：winit（事件循环）+ glutin/glutin-winit（OpenGL 窗口与 context）+
//! glow（GL 调用）+ imgui-rs / imgui-winit-support / imgui-glow-renderer（UI）。
//! 双路 RTSP 预览帧保持原始 RGBA8；连续密度场单独平滑采样为 RGBA heatmap 纹理，
//! 再以同一 fitted rectangle 叠加，guide overlay 仍由 ImGui draw list 绘制。

use glow::HasContext;
use glutin::{
    config::ConfigTemplateBuilder,
    context::{ContextAttributesBuilder, NotCurrentGlContext, PossiblyCurrentContext},
    display::{GetGlDisplay, GlDisplay},
    surface::{GlSurface, Surface, SurfaceAttributesBuilder, SwapInterval, WindowSurface},
};
use imgui::{Condition, TextureId};
use imgui_glow_renderer::Renderer as ImGuiGlowRenderer;
use imgui_winit_support::{HiDpiMode, WinitPlatform};

use pongbot_calib_tool::controller::{CalibController, STEP_TITLES, SnidDraft, StepId};
use pongbot_calib_tool::guide_overlay::{DensityHeatmap, OverlayData};
use pongbot_calib_tool::theme;
use raw_window_handle::HasWindowHandle;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Instant;
use winit::{
    dpi::LogicalSize,
    event::{Event, WindowEvent},
    event_loop::EventLoop,
    window::{Window, WindowAttributes},
};

fn main() {
    // CI 冒烟/用户查版本：--version 直接退出，不初始化窗口与 GL。
    if std::env::args().any(|arg| arg == "--version" || arg == "-V") {
        println!("pongbot-calib-tool {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    init_tracing();
    if let Err(error) = run() {
        eprintln!("pongbot-calib-tool 启动失败：{error}");
        std::process::exit(1);
    }
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

fn run() -> Result<(), String> {
    let (event_loop, window, surface, context) = create_window()?;
    let (mut winit_platform, mut imgui_context) = imgui_init(&window)?;
    let gl = glow_context(&context);
    let clear_color = imgui_context.style()[imgui::StyleColor::WindowBg];

    let mut textures = imgui::Textures::<glow::Texture>::default();
    let mut renderer = ImGuiGlowRenderer::new(&gl, &mut imgui_context, &mut textures, true)
        .map_err(|error| format!("ImGui renderer 初始化失败：{error}"))?;

    let mut app = App::new();
    let mut last_frame = Instant::now();

    #[allow(deprecated)]
    event_loop
        .run(move |event, window_target| match event {
            Event::NewEvents(_) => {
                let now = Instant::now();
                imgui_context
                    .io_mut()
                    .update_delta_time(now.duration_since(last_frame));
                last_frame = now;
            }
            Event::AboutToWait => {
                winit_platform
                    .prepare_frame(imgui_context.io_mut(), &window)
                    .expect("prepare_frame 失败");
                window.request_redraw();
            }
            Event::WindowEvent {
                event: WindowEvent::RedrawRequested,
                ..
            } => {
                unsafe {
                    gl.clear_color(
                        clear_color[0],
                        clear_color[1],
                        clear_color[2],
                        clear_color[3],
                    );
                    gl.clear(glow::COLOR_BUFFER_BIT);
                }
                let ui = imgui_context.frame();
                app.frame(&gl, &mut textures);
                app.show(&ui);
                winit_platform.prepare_render(ui, &window);
                let draw_data = imgui_context.render();
                if let Err(error) = renderer.render(&gl, &textures, draw_data) {
                    tracing::error!("ImGui 渲染失败：{error}");
                }
                surface.swap_buffers(&context).expect("swap_buffers 失败");
            }
            Event::WindowEvent {
                event: WindowEvent::Resized(size),
                ..
            } => {
                if size.width > 0 && size.height > 0 {
                    surface.resize(
                        &context,
                        NonZeroU32::new(size.width).expect("窗口宽度非零"),
                        NonZeroU32::new(size.height).expect("窗口高度非零"),
                    );
                }
                winit_platform.handle_event(imgui_context.io_mut(), &window, &event);
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => window_target.exit(),
            Event::LoopExiting => {
                app.destroy(&gl);
                renderer.destroy(&gl);
            }
            event => {
                winit_platform.handle_event(imgui_context.io_mut(), &window, &event);
            }
        })
        .map_err(|error| format!("事件循环错误：{error}"))
}

fn create_window() -> Result<
    (
        EventLoop<()>,
        Window,
        Surface<WindowSurface>,
        PossiblyCurrentContext,
    ),
    String,
> {
    let event_loop = EventLoop::new().map_err(|error| format!("EventLoop 创建失败：{error}"))?;
    let window_attributes = WindowAttributes::default()
        .with_title("X5 双目标定工具")
        .with_inner_size(LogicalSize::new(1440, 900));
    let (window, cfg) = glutin_winit::DisplayBuilder::new()
        .with_window_attributes(Some(window_attributes))
        .build(&event_loop, ConfigTemplateBuilder::new(), |mut configs| {
            configs.next().expect("没有可用的 GL 配置")
        })
        .map_err(|error| format!("OpenGL 窗口创建失败：{error}"))?;
    let window = window.expect("glutin 未创建窗口");

    let context_attribs = ContextAttributesBuilder::new().build(Some(
        window.window_handle().expect("window handle").as_raw(),
    ));
    let context = unsafe {
        cfg.display()
            .create_context(&cfg, &context_attribs)
            .map_err(|error| format!("OpenGL context 创建失败：{error}"))?
    };
    let surface_attribs = SurfaceAttributesBuilder::<WindowSurface>::new().build(
        window.window_handle().expect("window handle").as_raw(),
        NonZeroU32::new(1440).expect("初始宽度"),
        NonZeroU32::new(900).expect("初始高度"),
    );

    let surface = unsafe {
        cfg.display()
            .create_window_surface(&cfg, &surface_attribs)
            .map_err(|error| format!("OpenGL surface 创建失败：{error}"))?
    };

    let context = context
        .make_current(&surface)
        .map_err(|error| format!("make_current 失败：{error}"))?;
    surface
        .set_swap_interval(
            &context,
            SwapInterval::Wait(NonZeroU32::new(1).expect("swap interval")),
        )
        .map_err(|error| format!("设置垂直同步失败：{error}"))?;

    Ok((event_loop, window, surface, context))
}

fn glow_context(context: &PossiblyCurrentContext) -> glow::Context {
    unsafe {
        glow::Context::from_loader_function_cstr(|symbol| {
            context.display().get_proc_address(symbol).cast()
        })
    }
}

fn imgui_init(window: &Window) -> Result<(WinitPlatform, imgui::Context), String> {
    let mut imgui_context = imgui::Context::create();
    imgui_context.set_ini_filename(None);

    let mut winit_platform = WinitPlatform::new(&mut imgui_context);
    winit_platform.attach_window(imgui_context.io_mut(), window, HiDpiMode::Rounded);

    theme::install_fonts(&mut imgui_context);

    imgui_context.io_mut().font_global_scale = (1.0 / winit_platform.hidpi_factor()) as f32;
    Ok((winit_platform, imgui_context))
}

/// RGBA 图像的 GPU texture 生命周期；视频与密度 heatmap 分别持有实例。
struct VideoTexture {
    gl_texture: Option<glow::Texture>,
    id: Option<TextureId>,
    width: u32,
    height: u32,
}

impl VideoTexture {
    fn new() -> Self {
        Self {
            gl_texture: None,
            id: None,
            width: 0,
            height: 0,
        }
    }

    /// 上传一张 RGBA8 图像；尺寸变化时重分配，否则只更新像素内容。
    fn upload_rgba(
        &mut self,
        gl: &glow::Context,
        textures: &mut imgui::Textures<glow::Texture>,
        width: u32,
        height: u32,
        rgba: &[u8],
        internal_format: u32,
    ) -> Result<(), String> {
        let expected_len =
            rgba_byte_len(width, height).ok_or_else(|| "RGBA 图像尺寸溢出".to_owned())?;
        if rgba.len() != expected_len {
            return Err(format!(
                "RGBA8 载荷长度不匹配：expected {expected_len}, got {}",
                rgba.len()
            ));
        }
        unsafe {
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
            if self.gl_texture.is_none() {
                let texture = gl
                    .create_texture()
                    .map_err(|error| format!("创建 GL texture 失败：{error}"))?;
                gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                gl.tex_parameter_i32(
                    glow::TEXTURE_2D,
                    glow::TEXTURE_MIN_FILTER,
                    glow::LINEAR as i32,
                );
                gl.tex_parameter_i32(
                    glow::TEXTURE_2D,
                    glow::TEXTURE_MAG_FILTER,
                    glow::LINEAR as i32,
                );
                gl.tex_parameter_i32(
                    glow::TEXTURE_2D,
                    glow::TEXTURE_WRAP_S,
                    glow::CLAMP_TO_EDGE as i32,
                );
                gl.tex_parameter_i32(
                    glow::TEXTURE_2D,
                    glow::TEXTURE_WRAP_T,
                    glow::CLAMP_TO_EDGE as i32,
                );
                let id = textures.insert(texture);
                self.gl_texture = Some(texture);
                self.id = Some(id);
            }
            gl.bind_texture(glow::TEXTURE_2D, self.gl_texture);
            if self.width != width || self.height != height {
                gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    internal_format as i32,
                    width as i32,
                    height as i32,
                    0,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    Some(rgba),
                );
                self.width = width;
                self.height = height;
            } else {
                gl.tex_sub_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    0,
                    0,
                    width as i32,
                    height as i32,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(rgba),
                );
            }
            let error = gl.get_error();
            if error != glow::NO_ERROR {
                return Err(format!("GL 错误 {error:#x}"));
            }
        }
        Ok(())
    }

    fn destroy(&mut self, gl: &glow::Context) {
        if let Some(texture) = self.gl_texture.take() {
            unsafe { gl.delete_texture(texture) };
        }
        self.id = None;
        self.width = 0;
        self.height = 0;
    }
}

/// 每路独立的平滑密度纹理及其源快照身份缓存。
struct HeatmapTexture {
    texture: VideoTexture,
    source_samples: Option<Arc<[f32]>>,
    source_cols: usize,
    source_rows: usize,
    rgba: Vec<u8>,
}

impl HeatmapTexture {
    fn new() -> Self {
        Self {
            texture: VideoTexture::new(),
            source_samples: None,
            source_cols: 0,
            source_rows: 0,
            rgba: Vec::new(),
        }
    }

    fn source_matches(&self, heatmap: &DensityHeatmap, width: u32, height: u32) -> bool {
        self.texture.width == width
            && self.texture.height == height
            && self.source_cols == heatmap.cols
            && self.source_rows == heatmap.rows
            && self
                .source_samples
                .as_ref()
                .is_some_and(|samples| Arc::ptr_eq(samples, &heatmap.samples))
    }

    fn image_id(&self, heatmap: &DensityHeatmap, width: u32, height: u32) -> Option<TextureId> {
        self.source_matches(heatmap, width, height)
            .then_some(self.texture.id)
            .flatten()
    }

    fn destroy(&mut self, gl: &glow::Context) {
        self.texture.destroy(gl);
        self.source_samples = None;
        self.rgba.clear();
    }
}

/// 上次绘制的活跃步骤；变化时触发向导滚动。
struct App {
    controller: CalibController,
    video: [VideoTexture; 2],
    heatmap: [HeatmapTexture; 2],
    /// 上次绘制的活跃步骤；变化时触发向导滚动。
    last_active: StepId,
    /// 待滚动的步骤锚点（绘制完成后消费）。
    pending_scroll: Option<usize>,
}

impl App {
    fn new() -> Self {
        Self {
            controller: CalibController::new(),
            video: [VideoTexture::new(), VideoTexture::new()],
            heatmap: [HeatmapTexture::new(), HeatmapTexture::new()],
            last_active: StepId::Connect,
            pending_scroll: None,
        }
    }

    fn destroy(&mut self, gl: &glow::Context) {
        for slot in &mut self.video {
            slot.destroy(gl);
        }
        for slot in &mut self.heatmap {
            slot.destroy(gl);
        }
    }

    /// 每帧推进控制器，视频帧照常上传；热力图只在密度 Arc 身份或视频尺寸改变时上传。
    fn frame(&mut self, gl: &glow::Context, textures: &mut imgui::Textures<glow::Texture>) {
        self.controller.tick();
        self.update_video(gl, textures, 0, 0);
        self.update_video(gl, textures, 1, 3);
        self.update_heatmap(gl, textures, 0);
        self.update_heatmap(gl, textures, 1);
    }

    fn update_video(
        &mut self,
        gl: &glow::Context,
        textures: &mut imgui::Textures<glow::Texture>,
        slot_index: usize,
        channel: u16,
    ) {
        let Some(frame) = self.controller.poll_frame(channel) else {
            return;
        };
        let width = frame.width;
        let height = frame.height;
        if let Err(error) = self.video[slot_index].upload_rgba(
            gl,
            textures,
            width,
            height,
            &frame.rgba,
            // FFmpeg 输出的是显示编码后的 RGBA8；仅标记为 sRGB，不改写视频字节。
            glow::SRGB8_ALPHA8,
        ) {
            tracing::warn!(
                channel,
                width,
                height,
                actual_bytes = frame.rgba.len(),
                "视频纹理上传失败：{error}"
            );
        }
    }

    fn update_heatmap(
        &mut self,
        gl: &glow::Context,
        textures: &mut imgui::Textures<glow::Texture>,
        slot_index: usize,
    ) {
        let (width, height) = {
            let video = &self.video[slot_index];
            (video.width, video.height)
        };
        if width == 0 || height == 0 {
            return;
        }
        let heatmap = match slot_index {
            0 => self.controller.state.preview.ch0.quality.heatmap.clone(),
            _ => self.controller.state.preview.ch3.quality.heatmap.clone(),
        };
        if !heatmap.is_valid() {
            return;
        }
        let slot = &mut self.heatmap[slot_index];
        if slot.source_matches(&heatmap, width, height) {
            return;
        }
        if !rasterize_density_heatmap(&heatmap, width, height, &mut slot.rgba) {
            return;
        }
        match slot
            .texture
            .upload_rgba(gl, textures, width, height, &slot.rgba, glow::SRGB8_ALPHA8)
        {
            Ok(()) => {
                slot.source_samples = Some(heatmap.samples);
                slot.source_cols = heatmap.cols;
                slot.source_rows = heatmap.rows;
            }
            Err(error) => tracing::warn!(slot_index, width, height, "热力图纹理上传失败：{error}"),
        }
    }

    fn show(&mut self, ui: &imgui::Ui) {
        // 主窗口每帧强制占满 viewport：系统窗口缩放时内部布局跟随，不会被拖动。
        let display = ui.io().display_size;
        ui.window("X5 双目标定工具")
            .position([0.0, 0.0], Condition::Always)
            .size(display, Condition::Always)
            .movable(false)
            .resizable(false)
            .collapsible(false)
            .build(|| {
                let active = self.controller.state.active;
                if active != self.last_active {
                    self.last_active = active;
                    self.pending_scroll = Some(active.index());
                }
                let mut anchors = [0.0_f32; 3];
                for index in 0..3 {
                    anchors[index] = ui.cursor_pos()[1];
                    let open = active.index() == index;
                    self.show_step_header(ui, index, open);
                    if open {
                        ui.separator();
                        match index {
                            0 => self.show_connect_step(ui),
                            1 => self.show_preview_step(ui),
                            _ => {
                                self.show_solve_step(ui);
                                self.show_eeprom_step(ui);
                            }
                        }
                    }
                    ui.separator();
                }
                ui.text_colored(theme::MUTED, &self.controller.state.status_bar);
                // 活跃步骤变化后滚动到对应区域（连接成功 → 自动进入第二步）。
                if let Some(index) = self.pending_scroll.take() {
                    ui.set_scroll_y(anchors[index].max(0.0));
                }
            });
    }

    /// 折叠面板头：活跃步骤展开（▼），其余收起（▶）；完成步骤绿色打勾。
    fn show_step_header(&mut self, ui: &imgui::Ui, index: usize, open: bool) {
        let completed = self.controller.state.completed[index];
        let color = if completed {
            theme::OK
        } else if open {
            theme::ACCENT
        } else {
            theme::MUTED
        };
        let arrow = if open { "v" } else { ">" };
        let mark = if completed { " ✓" } else { "" };
        ui.text_colored(
            color,
            format!("{arrow} Step {} · {}{mark}", index + 1, STEP_TITLES[index]),
        );
    }

    fn show_connect_step(&mut self, ui: &imgui::Ui) {
        let busy = self.controller.is_busy();

        let mut ip = self.controller.state.connect.device_ip.clone();
        ui.input_text("设备 IP", &mut ip).build();
        self.controller.state.connect.device_ip = ip;

        let mut user = self.controller.state.connect.ssh_user.clone();
        ui.input_text("SSH 用户", &mut user).build();
        self.controller.state.connect.ssh_user = user;

        let mut password = self.controller.state.connect.ssh_password.clone();
        ui.input_text("SSH 密码", &mut password)
            .password(true)
            .build();
        self.controller.state.connect.ssh_password = password;

        ui.same_line();
        if ui.button("探测驱动 (TCP 9073)") && !busy {
            self.controller.probe();
        }
        ui.same_line();
        if ui.button("SSH 启动驱动") && !busy {
            self.controller.bootstrap();
        }
        ui.text_colored(
            self.controller.state.connect.status_color,
            &self.controller.state.connect.status,
        );
    }

    fn show_preview_step(&mut self, ui: &imgui::Ui) {
        let avail = ui.content_region_avail();
        let card_width = (avail[0] - 12.0) * 0.5;
        let card_height = (card_width * 9.0 / 16.0 + 46.0).max(160.0);

        ui.child_window("##ch0_card")
            .size([card_width, card_height])
            .build(|| self.show_preview_card(ui, 0, 0, "CH0 · RTSP 554 · i2c-4"));
        ui.same_line();
        ui.child_window("##ch3_card")
            .size([card_width, card_height])
            .build(|| self.show_preview_card(ui, 1, 3, "CH3 · RTSP 557 · i2c-6"));
        ui.separator();
        // 仅显示连续空间状态与用户要求保留的距离/倾斜条。
        let panel_height = 178.0;
        ui.child_window("##ch0_coverage")
            .size([card_width, panel_height])
            .build(|| self.show_coverage_panel(ui, 0));
        ui.same_line();
        ui.child_window("##ch3_coverage")
            .size([card_width, panel_height])
            .build(|| self.show_coverage_panel(ui, 1));
    }

    /// 紧凑质量面板：中心、四边、四角，以及保留的距离和倾斜覆盖。
    fn show_coverage_panel(&mut self, ui: &imgui::Ui, index: usize) {
        let quality = if index == 0 {
            self.controller.state.preview.ch0.quality.clone()
        } else {
            self.controller.state.preview.ch3.quality.clone()
        };
        let label = if index == 0 { "CH0" } else { "CH3" };
        ui.text_colored(theme::ACCENT, format!("{label} 采集质量"));
        let width = (ui.content_region_avail()[0] - 110.0).max(120.0);
        self.quality_progress_bar(
            ui,
            "中心",
            quality.center_progress(),
            width,
            if quality.center_complete() {
                "已覆盖"
            } else {
                "待覆盖"
            },
        );
        self.quality_progress_bar(
            ui,
            "边缘",
            quality.edge_progress(),
            width,
            &format!("{}/4", quality.covered_edges()),
        );
        self.quality_progress_bar(
            ui,
            "四角",
            quality.corner_progress(),
            width,
            &format!("{}/4", quality.covered_corners()),
        );
        self.retained_coverage_bar(ui, "距离", &quality.depth, width);
        self.retained_coverage_bar(ui, "倾斜", &quality.skew, width);
        if quality.is_complete() {
            ui.text_colored(theme::OK, "质量完成，即将自动进入求解");
        }
    }

    /// 连续质量的单条紧凑进度条。
    fn quality_progress_bar(
        &self,
        ui: &imgui::Ui,
        label: &str,
        progress: f32,
        width: f32,
        status: &str,
    ) {
        ui.text(label);
        const LABEL_COLUMN: f32 = 76.0;
        ui.same_line();
        let cursor = ui.cursor_pos();
        if cursor[0] < LABEL_COLUMN {
            ui.set_cursor_pos([LABEL_COLUMN, cursor[1]]);
        }
        let draw = ui.get_window_draw_list();
        let origin = ui.cursor_screen_pos();
        let height = 14.0;
        let progress = progress
            .is_finite()
            .then_some(progress.clamp(0.0, 1.0))
            .unwrap_or(0.0);
        draw.add_rect(
            [origin[0], origin[1]],
            [origin[0] + width, origin[1] + height],
            [0.16, 0.17, 0.22, 1.0],
        )
        .filled(true)
        .build();
        if progress > 0.0 {
            draw.add_rect(
                [origin[0], origin[1]],
                [origin[0] + width * progress, origin[1] + height],
                theme::OK,
            )
            .filled(true)
            .build();
        }
        draw.add_rect(
            [origin[0], origin[1]],
            [origin[0] + width, origin[1] + height],
            [0.85, 0.88, 0.95, 0.8],
        )
        .build();
        ui.same_line();
        ui.text_disabled(status);
    }

    /// 保留的距离/倾斜四档条；X/Y 位置条已被连续空间分析取代。
    fn retained_coverage_bar(&self, ui: &imgui::Ui, label: &str, bins: &[bool], width: f32) {
        ui.text(label);
        const LABEL_COLUMN: f32 = 76.0;
        ui.same_line();
        let cursor = ui.cursor_pos();
        if cursor[0] < LABEL_COLUMN {
            ui.set_cursor_pos([LABEL_COLUMN, cursor[1]]);
        }
        let draw = ui.get_window_draw_list();
        let origin = ui.cursor_screen_pos();
        let height = 14.0;
        let n = bins.len().max(1) as f32;
        let segment = width / n;
        draw.add_rect(
            [origin[0], origin[1]],
            [origin[0] + width, origin[1] + height],
            [0.16, 0.17, 0.22, 1.0],
        )
        .filled(true)
        .build();
        for (index, filled) in bins.iter().enumerate() {
            let x0 = origin[0] + index as f32 * segment;
            if *filled {
                draw.add_rect(
                    [x0, origin[1]],
                    [x0 + segment, origin[1] + height],
                    theme::OK,
                )
                .filled(true)
                .build();
            }
            draw.add_line(
                [x0, origin[1]],
                [x0, origin[1] + height],
                [0.5, 0.55, 0.65, 0.5],
            )
            .build();
        }
        draw.add_rect(
            [origin[0], origin[1]],
            [origin[0] + width, origin[1] + height],
            [0.85, 0.88, 0.95, 0.8],
        )
        .build();
        let filled = bins.iter().filter(|filled| **filled).count();
        ui.same_line();
        ui.text_disabled(format!("{filled}/{}", bins.len()));
    }

    fn show_preview_card(&mut self, ui: &imgui::Ui, slot_index: usize, channel: u16, label: &str) {
        ui.text_colored(theme::ACCENT, label);
        let channel_state = match channel {
            0 => &self.controller.state.preview.ch0,
            _ => &self.controller.state.preview.ch3,
        };
        ui.text_disabled(&channel_state.status);
        ui.separator();
        let area = ui.content_region_avail();
        let slot = &self.video[slot_index];

        match (slot.id, slot.width, slot.height) {
            (Some(id), width, height) if width > 0 && height > 0 => {
                let origin = ui.cursor_screen_pos();
                let (offset_min, offset_max) = fit_rect(area, width, height);
                let min = [origin[0] + offset_min[0], origin[1] + offset_min[1]];
                let max = [origin[0] + offset_max[0], origin[1] + offset_max[1]];
                let size = [max[0] - min[0], max[1] - min[1]];
                let heatmap_id = self.heatmap[slot_index].image_id(
                    &channel_state.quality.heatmap,
                    width,
                    height,
                );
                // 严格同一 fit_rect：video Image → 平滑 heatmap Image → 既有 draw-list overlay。
                ui.set_cursor_screen_pos(min);
                imgui::Image::new(id, size).build(ui);
                if let Some(heatmap_id) = heatmap_id {
                    ui.set_cursor_screen_pos(min);
                    imgui::Image::new(heatmap_id, size).build(ui);
                }
                let draw = ui.get_window_draw_list();
                if let Some(overlay) = self.controller.overlay(channel) {
                    draw_overlay(&draw, &overlay, min, max);
                }
                draw.add_text(
                    [min[0] + 8.0, min[1] + 8.0],
                    channel_state.overlay_color,
                    &channel_state.overlay_text,
                );
            }
            _ => {
                ui.text("等待预览帧…");
            }
        }
    }

    fn show_solve_step(&mut self, ui: &imgui::Ui) {
        let busy = self.controller.is_busy();

        let mut cols = self.controller.state.solve.board_cols;
        ui.input_int("内角点列数", &mut cols).build();
        self.controller.state.solve.board_cols = cols;

        let mut rows = self.controller.state.solve.board_rows;
        ui.input_int("内角点行数", &mut rows).build();
        self.controller.state.solve.board_rows = rows;

        let mut square_mm = self.controller.state.solve.square_mm;
        ui.input_float("方格尺寸 (mm)", &mut square_mm).build();
        self.controller.state.solve.square_mm = square_mm;

        ui.same_line();
        if ui.button("开始求解") && !busy {
            self.controller.solve();
        }

        ui.separator();
        let avail = ui.content_region_avail();
        let card_width = (avail[0] - 12.0) * 0.5;
        ui.child_window("##solve_ch0_card")
            .size([card_width, 340.0])
            .build(|| self.show_solve_card(ui, 0));
        ui.same_line();
        ui.child_window("##solve_ch3_card")
            .size([card_width, 340.0])
            .build(|| self.show_solve_card(ui, 1));
    }

    fn show_solve_card(&mut self, ui: &imgui::Ui, index: usize) {
        let label = if index == 0 { "CH0" } else { "CH3" };
        ui.text_colored(theme::ACCENT, &format!("{label} 标定结果"));
        let detail = if index == 0 {
            self.controller.state.solve.ch0_detail.clone()
        } else {
            self.controller.state.solve.ch3_detail.clone()
        };
        let rmse: Vec<f32> = if index == 0 {
            self.controller
                .state
                .solve
                .ch0_rmse
                .iter()
                .map(|value| *value as f32)
                .collect()
        } else {
            self.controller
                .state
                .solve
                .ch3_rmse
                .iter()
                .map(|value| *value as f32)
                .collect()
        };
        let fallback = if index == 0 {
            self.controller.state.solve.ch0_result.clone()
        } else {
            self.controller.state.solve.ch3_result.clone()
        };
        let Some(detail) = detail else {
            ui.text_wrapped(&fallback);
            return;
        };
        ui.text_colored(theme::OK, "求解完成");
        self.key_value_list(
            ui,
            &[
                ("有效帧", format!("{}", detail.view_count)),
                ("总体 RMS", format!("{:.3} px", detail.rms)),
                ("单图最大", format!("{:.3} px", detail.max_view_rmse)),
            ],
        );
        ui.separator();
        self.key_value_list(
            ui,
            &[
                ("fx", format!("{:.2}", detail.fx)),
                ("fy", format!("{:.2}", detail.fy)),
                ("cx", format!("{:.2}", detail.cx)),
                ("cy", format!("{:.2}", detail.cy)),
                ("H FOV", format!("{:.2}°", detail.hfov_degrees)),
                ("V FOV", format!("{:.2}°", detail.vfov_degrees)),
                ("光心偏移 x", format!("{:+.3}°", detail.optical_x_degrees)),
                ("光心偏移 y", format!("{:+.3}°", detail.optical_y_degrees)),
            ],
        );
        ui.separator();
        let names = [
            "k1", "k2", "p1", "p2", "k3", "k4", "k5", "k6", "s1", "s2", "s3", "s4",
        ];
        let mut rows: Vec<(String, String)> = Vec::new();
        for (name, value) in names.iter().zip(detail.distortion.iter().copied()) {
            if rows.len() < 6 {
                rows.push(((*name).to_owned(), format!("{value:.4}")));
            }
        }
        self.key_value_list(
            ui,
            &rows
                .iter()
                .map(|(k, v)| (k.as_str(), v.clone()))
                .collect::<Vec<_>>(),
        );
        if !rmse.is_empty() {
            ui.separator();
            ui.text(&format!(
                "单图重投影 RMSE（最大 {:.2} px）",
                detail.max_view_rmse
            ));
            ui.plot_histogram(
                if index == 0 {
                    "##rmse_ch0"
                } else {
                    "##rmse_ch3"
                },
                &rmse,
            )
            .scale_min(0.0)
            .scale_max((detail.max_view_rmse as f32 * 1.15).max(0.05))
            .graph_size([ui.content_region_avail()[0], 96.0])
            .build();
        }
    }

    fn key_value_list(&self, ui: &imgui::Ui, rows: &[(&str, String)]) {
        ui.columns(2, "##kv_list", false);
        for (key, value) in rows {
            ui.text(*key);
            ui.next_column();
            ui.text(value);
            ui.next_column();
        }
        ui.columns(1, "##kv_list_end", false);
    }

    fn show_eeprom_step(&mut self, ui: &imgui::Ui) {
        ui.text_colored(theme::ACCENT, "EEPROM 写入");
        let busy = self.controller.is_busy();
        let avail = ui.content_region_avail();
        let editor_width = (avail[0] - 12.0) * 0.5;

        ui.child_window("##snid_ch0")
            .size([editor_width, 230.0])
            .build(|| self.show_snid_editor(ui, 0, "CH0 SNID"));
        ui.same_line();
        ui.child_window("##snid_ch3")
            .size([editor_width, 230.0])
            .build(|| self.show_snid_editor(ui, 1, "CH3 SNID"));

        ui.separator();
        if ui.button("读取 EEPROM 状态") && !busy {
            self.controller.inspect_eeprom();
        }
        ui.same_line();
        if ui.button("写入 EEPROM") && !busy {
            self.controller.write_eeprom();
        }
        ui.same_line();
        if ui.button("Reset 流程") {
            self.controller.reset_flow();
        }
        if self.controller.state.eeprom.write_armed {
            ui.text_colored(theme::WARN, "⚠ 已预检 SNID，再次点击“写入 EEPROM”执行烧录");
        }
        ui.separator();

        let avail = ui.content_region_avail();
        let card_width = (avail[0] - 12.0) * 0.5;
        ui.child_window("##eeprom_ch0_card")
            .size([card_width, 500.0])
            .build(|| self.show_eeprom_card(ui, 0));
        ui.same_line();
        ui.child_window("##eeprom_ch3_card")
            .size([card_width, 500.0])
            .build(|| self.show_eeprom_card(ui, 1));

        ui.separator();
        ui.text_wrapped(&self.controller.state.eeprom.status);
    }

    fn show_eeprom_card(&mut self, ui: &imgui::Ui, index: usize) {
        let label = if index == 0 { "CH0" } else { "CH3" };
        ui.text_colored(theme::ACCENT, &format!("{label} EEPROM 状态"));
        let inspect = self.controller.state.eeprom.inspect.clone();
        let last_write = self.controller.state.eeprom.last_write.clone();
        let history_paths = self.controller.state.eeprom.write_history_paths.clone();
        match &inspect {
            Some((a, b)) => {
                let detail = if index == 0 { a } else { b };
                self.key_value_list(
                    ui,
                    &[(
                        "FLAG",
                        if detail.flag_valid {
                            "有效".to_owned()
                        } else {
                            "无效".to_owned()
                        },
                    )],
                );
                if let Some(calibration) = &detail.calibration {
                    ui.separator();
                    self.key_value_list(
                        ui,
                        &[
                            ("fx", format!("{:.2}", calibration.fx)),
                            ("fy", format!("{:.2}", calibration.fy)),
                            ("cx", format!("{:.2}", calibration.cx)),
                            ("cy", format!("{:.2}", calibration.cy)),
                            ("H FOV", format!("{:.2}°", calibration.hfov_degrees)),
                            ("V FOV", format!("{:.2}°", calibration.vfov_degrees)),
                            (
                                "光心偏移 x",
                                format!("{:+.3}°", calibration.optical_x_degrees),
                            ),
                            (
                                "光心偏移 y",
                                format!("{:+.3}°", calibration.optical_y_degrees),
                            ),
                        ],
                    );
                    ui.separator();
                    let names = [
                        "k1", "k2", "p1", "p2", "k3", "k4", "k5", "k6", "s1", "s2", "s3", "s4",
                    ];
                    let mut rows: Vec<(String, String)> = Vec::new();
                    for (name, value) in names.iter().zip(calibration.distortion.iter().copied()) {
                        if rows.len() < 6 {
                            rows.push(((*name).to_owned(), format!("{value:.4}")));
                        }
                    }
                    self.key_value_list(
                        ui,
                        &rows
                            .iter()
                            .map(|(k, v)| (k.as_str(), v.clone()))
                            .collect::<Vec<_>>(),
                    );
                } else if let Some(error) = &detail.calibration_error {
                    ui.text_wrapped(error);
                }
            }
            None => {
                ui.text_colored(theme::MUTED, "未读取");
            }
        }
        ui.separator();
        match &last_write {
            Some((a, b)) => {
                let detail = if index == 0 { a } else { b };
                ui.text_colored(theme::OK, "最近写入结果");
                self.key_value_list(
                    ui,
                    &[
                        (
                            "hash",
                            format!("{} -> {}", detail.before_sha8, detail.after_sha8),
                        ),
                        (
                            "SN",
                            format!("{} -> {}", detail.before_serial, detail.after_serial),
                        ),
                        (
                            "逐字节校验",
                            if detail.verified {
                                "通过".to_owned()
                            } else {
                                "失败".to_owned()
                            },
                        ),
                    ],
                );
                if let Some((path0, path3)) = &history_paths {
                    let path = if index == 0 { path0 } else { path3 };
                    ui.text_wrapped(&format!("write_history：{path}"));
                }
            }
            None => {
                ui.text_colored(theme::MUTED, "最近写入结果：未写入");
            }
        }
    }

    fn show_snid_editor(&mut self, ui: &imgui::Ui, index: usize, title: &str) {
        ui.text(title);
        let (mut module, mut year, mut month, mut day, mut axis, mut sequence) = {
            let draft = if index == 0 {
                &self.controller.state.eeprom.ch0_snid
            } else {
                &self.controller.state.eeprom.ch3_snid
            };
            (
                draft.module_index,
                draft.year.clone(),
                draft.month.clone(),
                draft.day.clone(),
                draft.axis_index,
                draft.sequence.clone(),
            )
        };
        let mut changed = false;
        changed |= ui.combo_simple_string("型号", &mut module, &SnidDraft::MODULES);
        changed |= ui.input_text("年份 (YY)", &mut year).build();
        changed |= ui.input_text("月份", &mut month).build();
        changed |= ui.input_text("日期", &mut day).build();
        changed |= ui.combo_simple_string("光轴等级", &mut axis, &SnidDraft::AXES);
        // 序列号步进按钮：十进制 -1 / +1，有效范围 1..=3844（两位 base62 上限）。
        ui.set_next_item_width(120.0);
        changed |= ui.input_text("序列号", &mut sequence).build();
        ui.same_line();
        if ui.small_button(format!("-1##seq_{index}")) {
            if let Ok(value) = sequence.trim().parse::<u16>() {
                if value > 1 {
                    sequence = (value - 1).to_string();
                    changed = true;
                }
            }
        }
        ui.same_line();
        if ui.small_button(format!("+1##seq_{index}")) {
            if let Ok(value) = sequence.trim().parse::<u16>() {
                if value < 3844 {
                    sequence = (value + 1).to_string();
                    changed = true;
                }
            }
        }
        if changed {
            let draft = if index == 0 {
                &mut self.controller.state.eeprom.ch0_snid
            } else {
                &mut self.controller.state.eeprom.ch3_snid
            };
            draft.module_index = module;
            draft.year = year;
            draft.month = month;
            draft.day = day;
            draft.axis_index = axis;
            draft.sequence = sequence;
            self.controller.refresh_snid_previews();
        }
        let (preview, preview_ok) = {
            let draft = if index == 0 {
                &self.controller.state.eeprom.ch0_snid
            } else {
                &self.controller.state.eeprom.ch3_snid
            };
            (draft.preview.clone(), draft.preview_ok)
        };
        ui.text_colored(if preview_ok { theme::OK } else { theme::WARN }, preview);
    }
}

fn rgba_byte_len(width: u32, height: u32) -> Option<usize> {
    usize::try_from(width)
        .ok()?
        .checked_mul(usize::try_from(height).ok()?)
        .and_then(|pixels| pixels.checked_mul(4))
}

/// 零密度仍保留微弱红色，既能表意又不会遮挡原始视频；充分密度更不透明。
const HEATMAP_ZERO_ALPHA: f32 = 0.16;
const HEATMAP_SUFFICIENT_ALPHA: f32 = 0.44;

/// 将低分辨率连续密度场按双线性插值平滑采样为与视频同尺寸的 RGBA8 纹理。
fn rasterize_density_heatmap(
    heatmap: &DensityHeatmap,
    width: u32,
    height: u32,
    output: &mut Vec<u8>,
) -> bool {
    if !heatmap.is_valid() {
        return false;
    }
    let (Some(byte_len), Ok(output_width), Ok(output_height)) = (
        rgba_byte_len(width, height),
        usize::try_from(width),
        usize::try_from(height),
    ) else {
        return false;
    };
    if output_width == 0 || output_height == 0 {
        return false;
    }
    output.resize(byte_len, 0);
    let width_f = width as f32;
    let height_f = height as f32;
    for y in 0..output_height {
        let v = (y as f32 + 0.5) / height_f;
        for x in 0..output_width {
            let u = (x as f32 + 0.5) / width_f;
            let density = sample_density_bilinear(heatmap, u, v);
            let red_to_green = [
                theme::ERR[0] + (theme::OK[0] - theme::ERR[0]) * density,
                theme::ERR[1] + (theme::OK[1] - theme::ERR[1]) * density,
                theme::ERR[2] + (theme::OK[2] - theme::ERR[2]) * density,
            ];
            let alpha =
                HEATMAP_ZERO_ALPHA + (HEATMAP_SUFFICIENT_ALPHA - HEATMAP_ZERO_ALPHA) * density;
            let index = (y * output_width + x) * 4;
            output[index] = rgba_channel(red_to_green[0]);
            output[index + 1] = rgba_channel(red_to_green[1]);
            output[index + 2] = rgba_channel(red_to_green[2]);
            output[index + 3] = rgba_channel(alpha);
        }
    }
    true
}

/// 以密度网格单元中心为采样点，在边界钳制后进行双线性插值。
fn sample_density_bilinear(heatmap: &DensityHeatmap, u: f32, v: f32) -> f32 {
    let max_col = heatmap.cols.saturating_sub(1);
    let max_row = heatmap.rows.saturating_sub(1);
    let source_x = (u * heatmap.cols as f32 - 0.5).clamp(0.0, max_col as f32);
    let source_y = (v * heatmap.rows as f32 - 0.5).clamp(0.0, max_row as f32);
    let left = source_x.floor() as usize;
    let top = source_y.floor() as usize;
    let right = (left + 1).min(max_col);
    let bottom = (top + 1).min(max_row);
    let tx = source_x - left as f32;
    let ty = source_y - top as f32;
    let at = |col: usize, row: usize| heatmap.samples[row * heatmap.cols + col];
    let upper = at(left, top) + (at(right, top) - at(left, top)) * tx;
    let lower = at(left, bottom) + (at(right, bottom) - at(left, bottom)) * tx;
    let density = upper + (lower - upper) * ty;
    if density.is_finite() {
        density.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn rgba_channel(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// 在 `avail` 区域内按原比例缩放显示 `width×height` 图像，返回窗口坐标矩形。
fn fit_rect(avail: [f32; 2], width: u32, height: u32) -> ([f32; 2], [f32; 2]) {
    let frame_width = width.max(1) as f32;
    let frame_height = height.max(1) as f32;
    let scale = (avail[0] / frame_width).min(avail[1] / frame_height);
    let draw_width = frame_width * scale;
    let draw_height = frame_height * scale;
    let x0 = (avail[0] - draw_width) * 0.5;
    let y0 = (avail[1] - draw_height) * 0.5;
    ([x0, y0], [x0 + draw_width, y0 + draw_height])
}

/// 用 ImGui draw list 在图像矩形上绘制采集 overlay（检测框 + hold 状态）。
fn draw_overlay(draw: &imgui::DrawListMut, overlay: &OverlayData, min: [f32; 2], max: [f32; 2]) {
    let image_width = overlay.image_width.max(1.0);
    let image_height = overlay.image_height.max(1.0);
    let px_to_screen = |point: [f32; 2]| -> [f32; 2] {
        [
            min[0] + point[0] / image_width * (max[0] - min[0]),
            min[1] + point[1] / image_height * (max[1] - min[1]),
        ]
    };

    if let Some(outline) = overlay.detected_outline_px {
        let corners: Vec<[f32; 2]> = outline.iter().map(|point| px_to_screen(*point)).collect();
        for index in 0..4 {
            draw.add_line(corners[index], corners[(index + 1) % 4], theme::ACCENT)
                .thickness(2.0)
                .build();
        }
    }

    if let Some(status) = &overlay.status {
        let text = format!("hold {}/{}", status.hold_frames, status.hold_target);
        let color = if status.hold_frames >= status.hold_target {
            theme::OK
        } else if status.hold_frames > 0 {
            theme::WARN
        } else {
            theme::ACCENT
        };
        draw.add_text([min[0] + 8.0, min[1] + 30.0], color, text);
    }

    // 成功拍摄帧的棋盘内角点网格：金色连线 + 绿点（确认触发瞬间采到的姿态）。
    if let Some((cols, rows, points)) = &overlay.captured_corners_px {
        if *cols > 1 && *rows > 1 && points.len() == cols * rows {
            let screen: Vec<[f32; 2]> = points.iter().map(|point| px_to_screen(*point)).collect();
            let grid_color = [1.0, 0.78, 0.35, 0.9];
            for row in 0..*rows {
                for col in 0..*cols {
                    let index = row * cols + col;
                    if col + 1 < *cols {
                        draw.add_line(screen[index], screen[index + 1], grid_color)
                            .thickness(1.2)
                            .build();
                    }
                    if row + 1 < *rows {
                        draw.add_line(screen[index], screen[index + *cols], grid_color)
                            .thickness(1.2)
                            .build();
                    }
                }
            }
            for point in &screen {
                draw.add_circle(*point, 2.5, theme::OK)
                    .thickness(1.6)
                    .build();
            }
        }
    }
}
