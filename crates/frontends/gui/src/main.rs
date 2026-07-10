use std::path::PathBuf;

use anyhow::{Result, anyhow};
use camera_toolbox_adapters::LocalRawLoader;
use camera_toolbox_app::{LocalRawAnalyzeRequest, Workflow};
use camera_toolbox_core::{BayerPattern, RawEncoding, RawFrame, RawSpec, Roi, RoiStats};
use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};

const MIN_ZOOM: f32 = 0.05;
const MAX_ZOOM: f32 = 64.0;
const MINIMAP_MAX_SIZE: egui::Vec2 = egui::vec2(180.0, 130.0);
const MINIMAP_MARGIN: f32 = 12.0;

fn main() -> Result<()> {
    let native_options = eframe::NativeOptions::default();
    eframe::run_native(
        "Camera Toolbox",
        native_options,
        Box::new(|_cc| Ok(Box::<CameraToolboxApp>::default())),
    )
    .map_err(|err| anyhow!(err.to_string()))?;
    Ok(())
}

struct LoadedRaw {
    path: PathBuf,
    frame: RawFrame,
    roi: Roi,
    stats: RoiStats,
    texture: TextureHandle,
}

#[derive(Default)]
struct CameraToolboxApp {
    loaded: Option<LoadedRaw>,
    viewer: ImageViewerState,
    raw_dialog: RawOpenDialogState,
    status_message: String,
}

impl eframe::App for CameraToolboxApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        egui::Panel::top("menu_bar").show(ui, |ui| {
            self.render_menu_bar(ui);
        });
        egui::Panel::bottom("status_bar").show(ui, |ui| {
            self.render_status_bar(ui);
        });
        egui::CentralPanel::default().show(ui, |ui| {
            self.render_viewer(ui);
        });
        self.render_raw_open_dialog(&ctx);
    }
}

impl CameraToolboxApp {
    fn render_menu_bar(&mut self, ui: &mut egui::Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("Open Raw...").clicked() {
                    self.open_raw_file_dialog();
                    ui.close();
                }
                if ui
                    .add_enabled(self.loaded.is_some(), egui::Button::new("Close Image"))
                    .clicked()
                {
                    self.loaded = None;
                    self.viewer = ImageViewerState::default();
                    self.status_message = "Closed image".to_owned();
                    ui.close();
                }
                ui.separator();
                if ui.button("Quit").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });

            ui.menu_button("View", |ui| {
                if ui
                    .add_enabled(self.loaded.is_some(), egui::Button::new("Fit to Window"))
                    .clicked()
                {
                    self.viewer.fit_on_next_frame = true;
                    ui.close();
                }
                if ui
                    .add_enabled(
                        self.loaded.is_some(),
                        egui::Button::new("Actual Size / 100%"),
                    )
                    .clicked()
                {
                    self.viewer.zoom = 1.0;
                    self.viewer.fit_on_next_frame = false;
                    ui.close();
                }
                if ui
                    .add_enabled(self.loaded.is_some(), egui::Button::new("Zoom In"))
                    .clicked()
                {
                    self.viewer.zoom_by(1.25, None, egui::Rect::NOTHING);
                    ui.close();
                }
                if ui
                    .add_enabled(self.loaded.is_some(), egui::Button::new("Zoom Out"))
                    .clicked()
                {
                    self.viewer.zoom_by(0.8, None, egui::Rect::NOTHING);
                    ui.close();
                }
                if ui
                    .add_enabled(self.loaded.is_some(), egui::Button::new("Reset View"))
                    .clicked()
                {
                    self.viewer = ImageViewerState {
                        fit_on_next_frame: true,
                        ..Default::default()
                    };
                    ui.close();
                }
            });

            ui.menu_button("Tools", |ui| {
                ui.add_enabled(false, egui::Button::new("ROI Stats"));
                ui.add_enabled(false, egui::Button::new("Histogram"));
            });

            ui.menu_button("Help", |ui| {
                ui.label("Camera Toolbox");
                ui.label("Local RAW viewer foundation");
            });
        });
    }

    fn render_status_bar(&self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            if let Some(loaded) = &self.loaded {
                let file_name = loaded
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("<raw>");
                ui.label(file_name);
                ui.separator();
                ui.label(format!(
                    "{}x{} stride={} bit={} {} u16le",
                    loaded.frame.spec.width,
                    loaded.frame.spec.height,
                    loaded.frame.spec.stride_pixels,
                    loaded.frame.spec.bit_depth,
                    BayerUi::from(loaded.frame.spec.bayer).label()
                ));
                ui.separator();
                ui.label(format!("zoom={:.0}%", self.viewer.zoom * 100.0));
                ui.separator();
                if let Some(cursor) = self.viewer.cursor {
                    ui.label(format!(
                        "x={} y={} raw={}",
                        cursor.x, cursor.y, cursor.value
                    ));
                } else {
                    ui.label("x=- y=- raw=-");
                }
                ui.separator();
                ui.label(format!(
                    "roi={},{},{},{} min={} max={} mean={:.2} sat={}/{}",
                    loaded.roi.x,
                    loaded.roi.y,
                    loaded.roi.width,
                    loaded.roi.height,
                    loaded.stats.min,
                    loaded.stats.max,
                    loaded.stats.mean,
                    loaded.stats.saturated_pixels,
                    loaded.stats.total_pixels
                ));
            } else {
                ui.label("Ready");
                ui.separator();
                ui.label("No image loaded");
            }

            if !self.status_message.is_empty() {
                ui.separator();
                ui.label(&self.status_message);
            }
        });
    }

    fn render_viewer(&mut self, ui: &mut egui::Ui) {
        let available = ui.available_size().max(egui::vec2(1.0, 1.0));
        let (viewer_rect, response) =
            ui.allocate_exact_size(available, egui::Sense::click_and_drag());
        let painter = ui.painter_at(viewer_rect);
        painter.rect_filled(viewer_rect, 0.0, egui::Color32::from_gray(22));

        let Some(loaded) = self.loaded.as_ref() else {
            painter.text(
                viewer_rect.center(),
                egui::Align2::CENTER_CENTER,
                "File -> Open Raw...",
                egui::TextStyle::Heading.resolve(ui.style()),
                egui::Color32::GRAY,
            );
            self.viewer.cursor = None;
            return;
        };

        if self.viewer.fit_on_next_frame {
            self.viewer.fit_to_rect(
                viewer_rect,
                loaded.frame.spec.width as f32,
                loaded.frame.spec.height as f32,
            );
        }

        if response.dragged_by(egui::PointerButton::Primary) {
            self.viewer.pan += response.drag_delta();
        }

        if response.hovered() {
            let scroll_y = ui.input(|input| input.smooth_scroll_delta().y);
            if scroll_y.abs() > f32::EPSILON {
                let anchor = response.hover_pos();
                let factor = (scroll_y * 0.0015).exp();
                self.viewer.zoom_by(factor, anchor, viewer_rect);
            }
        }

        let image_rect = self.viewer.image_rect(
            viewer_rect,
            loaded.frame.spec.width as f32,
            loaded.frame.spec.height as f32,
        );
        painter.image(
            loaded.texture.id(),
            image_rect,
            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
        draw_roi_overlay(&painter, image_rect, &loaded.frame, loaded.roi);

        self.viewer.cursor = response
            .hover_pos()
            .and_then(|pos| hover_pixel(image_rect, pos, &loaded.frame));
        if let Some(cursor) = self.viewer.cursor {
            response.on_hover_text(format!(
                "x={} y={} raw={}",
                cursor.x, cursor.y, cursor.value
            ));
        }

        draw_minimap(&painter, viewer_rect, image_rect, loaded);
    }

    fn render_raw_open_dialog(&mut self, ctx: &egui::Context) {
        if !self.raw_dialog.visible {
            return;
        }

        let mut visible = self.raw_dialog.visible;
        let mut should_load = false;
        egui::Window::new("Open RAW")
            .open(&mut visible)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label("Selected file");
                ui.monospace(self.raw_dialog.path.display().to_string());
                ui.separator();

                egui::Grid::new("raw_open_grid")
                    .num_columns(2)
                    .spacing(egui::vec2(12.0, 8.0))
                    .show(ui, |ui| {
                        ui.label("Width");
                        ui.text_edit_singleline(&mut self.raw_dialog.width);
                        ui.end_row();

                        ui.label("Height");
                        ui.text_edit_singleline(&mut self.raw_dialog.height);
                        ui.end_row();

                        ui.label("Bit depth");
                        ui.text_edit_singleline(&mut self.raw_dialog.bit_depth);
                        ui.end_row();

                        ui.label("Stride pixels");
                        ui.text_edit_singleline(&mut self.raw_dialog.stride_pixels);
                        ui.end_row();

                        ui.label("Bayer");
                        egui::ComboBox::from_id_salt("raw_bayer")
                            .selected_text(self.raw_dialog.bayer.label())
                            .show_ui(ui, |ui| {
                                for bayer in BayerUi::ALL {
                                    ui.selectable_value(
                                        &mut self.raw_dialog.bayer,
                                        bayer,
                                        bayer.label(),
                                    );
                                }
                            });
                        ui.end_row();

                        ui.label("Storage");
                        egui::ComboBox::from_id_salt("raw_storage")
                            .selected_text(self.raw_dialog.storage.label())
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut self.raw_dialog.storage,
                                    RawStorageUi::UnpackedU16,
                                    RawStorageUi::UnpackedU16.label(),
                                );
                            });
                        ui.end_row();

                        ui.label("Endian");
                        egui::ComboBox::from_id_salt("raw_endian")
                            .selected_text(self.raw_dialog.endian.label())
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut self.raw_dialog.endian,
                                    EndianUi::Little,
                                    EndianUi::Little.label(),
                                );
                                ui.selectable_value(
                                    &mut self.raw_dialog.endian,
                                    EndianUi::Big,
                                    EndianUi::Big.label(),
                                );
                            });
                        ui.end_row();
                    });

                ui.label(
                    "Stride 为空时默认等于 width；当前解码只支持 unpacked u16 little-endian。",
                );
                if let Some(error) = &self.raw_dialog.error {
                    ui.colored_label(egui::Color32::RED, error);
                }

                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        self.raw_dialog.visible = false;
                    }
                    if ui.button("Open").clicked() {
                        should_load = true;
                    }
                });
            });
        self.raw_dialog.visible = visible && self.raw_dialog.visible;

        if should_load {
            match self.load_raw_from_dialog(ctx) {
                Ok(()) => self.raw_dialog.visible = false,
                Err(err) => {
                    let message = err.to_string();
                    self.raw_dialog.error = Some(message.clone());
                    self.status_message = format!("Open RAW failed: {message}");
                }
            }
        }
    }

    fn open_raw_file_dialog(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .set_title("Open RAW")
            .add_filter("RAW", &["raw", "bin"])
            .pick_file()
        else {
            self.status_message = "Open RAW cancelled".to_owned();
            return;
        };

        self.raw_dialog = RawOpenDialogState::for_path(path);
        self.status_message = "Configure RAW parameters".to_owned();
    }

    fn load_raw_from_dialog(&mut self, ctx: &egui::Context) -> Result<()> {
        let request = self.raw_dialog.to_request()?;
        let loader = LocalRawLoader;
        let report = Workflow::load_raw_and_analyze(&loader, request)?;
        let image = grayscale_preview(&report.frame);
        let texture = ctx.load_texture(
            format!("local-raw-preview:{}", report.path.display()),
            image,
            TextureOptions::NEAREST,
        );
        self.status_message = format!("Loaded {}", report.path.display());
        self.loaded = Some(LoadedRaw {
            path: report.path,
            frame: report.frame,
            roi: report.roi,
            stats: report.stats,
            texture,
        });
        self.viewer = ImageViewerState {
            fit_on_next_frame: true,
            ..Default::default()
        };
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct CursorPixel {
    x: u32,
    y: u32,
    value: u16,
}

#[derive(Clone, Copy)]
struct ImageViewerState {
    zoom: f32,
    pan: egui::Vec2,
    fit_on_next_frame: bool,
    cursor: Option<CursorPixel>,
}

impl Default for ImageViewerState {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            pan: egui::Vec2::ZERO,
            fit_on_next_frame: true,
            cursor: None,
        }
    }
}

impl ImageViewerState {
    fn zoom_by(&mut self, factor: f32, anchor: Option<egui::Pos2>, viewer_rect: egui::Rect) {
        let old_zoom = self.zoom;
        self.zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let Some(anchor) = anchor else {
            return;
        };
        let scale = self.zoom / old_zoom;
        let local_anchor = anchor - viewer_rect.min;
        self.pan = local_anchor + (self.pan - local_anchor) * scale;
    }

    fn fit_to_rect(&mut self, rect: egui::Rect, image_width: f32, image_height: f32) {
        let zoom_x = rect.width() / image_width.max(1.0);
        let zoom_y = rect.height() / image_height.max(1.0);
        self.zoom = zoom_x.min(zoom_y).clamp(MIN_ZOOM, MAX_ZOOM);
        let image_size = egui::vec2(image_width * self.zoom, image_height * self.zoom);
        self.pan = rect.center().to_vec2() - image_size * 0.5 - rect.min.to_vec2();
        self.fit_on_next_frame = false;
    }

    fn image_rect(
        &self,
        viewer_rect: egui::Rect,
        image_width: f32,
        image_height: f32,
    ) -> egui::Rect {
        egui::Rect::from_min_size(
            viewer_rect.min + self.pan,
            egui::vec2(image_width * self.zoom, image_height * self.zoom),
        )
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BayerUi {
    Rggb,
    Grbg,
    Gbrg,
    Bggr,
}

impl BayerUi {
    const ALL: [Self; 4] = [Self::Rggb, Self::Grbg, Self::Gbrg, Self::Bggr];

    const fn label(self) -> &'static str {
        match self {
            Self::Rggb => "rggb",
            Self::Grbg => "grbg",
            Self::Gbrg => "gbrg",
            Self::Bggr => "bggr",
        }
    }
}

impl From<BayerUi> for BayerPattern {
    fn from(value: BayerUi) -> Self {
        match value {
            BayerUi::Rggb => Self::Rggb,
            BayerUi::Grbg => Self::Grbg,
            BayerUi::Gbrg => Self::Gbrg,
            BayerUi::Bggr => Self::Bggr,
        }
    }
}

impl From<BayerPattern> for BayerUi {
    fn from(value: BayerPattern) -> Self {
        match value {
            BayerPattern::Rggb => Self::Rggb,
            BayerPattern::Grbg => Self::Grbg,
            BayerPattern::Gbrg => Self::Gbrg,
            BayerPattern::Bggr => Self::Bggr,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RawStorageUi {
    UnpackedU16,
}

impl RawStorageUi {
    const fn label(self) -> &'static str {
        match self {
            Self::UnpackedU16 => "unpacked u16",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EndianUi {
    Little,
    Big,
}

impl EndianUi {
    const fn label(self) -> &'static str {
        match self {
            Self::Little => "little endian",
            Self::Big => "big endian (unsupported)",
        }
    }
}

struct RawOpenDialogState {
    visible: bool,
    path: PathBuf,
    width: String,
    height: String,
    bit_depth: String,
    stride_pixels: String,
    bayer: BayerUi,
    storage: RawStorageUi,
    endian: EndianUi,
    error: Option<String>,
}

impl Default for RawOpenDialogState {
    fn default() -> Self {
        Self {
            visible: false,
            path: PathBuf::new(),
            width: String::new(),
            height: String::new(),
            bit_depth: "10".to_owned(),
            stride_pixels: String::new(),
            bayer: BayerUi::Rggb,
            storage: RawStorageUi::UnpackedU16,
            endian: EndianUi::Little,
            error: None,
        }
    }
}

impl RawOpenDialogState {
    fn for_path(path: PathBuf) -> Self {
        Self {
            visible: true,
            path,
            ..Default::default()
        }
    }

    fn to_request(&self) -> Result<LocalRawAnalyzeRequest> {
        if self.endian != EndianUi::Little {
            return Err(anyhow!("only little-endian unpacked u16 RAW is supported"));
        }
        if self.storage != RawStorageUi::UnpackedU16 {
            return Err(anyhow!("only unpacked u16 RAW storage is supported"));
        }

        let width = parse_u32_field("width", &self.width)?;
        let height = parse_u32_field("height", &self.height)?;
        let bit_depth = parse_u8_field("bit depth", &self.bit_depth)?;
        let stride_pixels = if self.stride_pixels.trim().is_empty() {
            width
        } else {
            parse_u32_field("stride pixels", &self.stride_pixels)?
        };
        if stride_pixels < width {
            return Err(anyhow!("stride pixels must be >= width"));
        }

        let spec = RawSpec {
            width,
            height,
            bit_depth,
            stride_pixels,
            bayer: self.bayer.into(),
        };
        Ok(LocalRawAnalyzeRequest {
            path: self.path.clone(),
            spec,
            encoding: RawEncoding::U16Le,
            roi: Roi {
                x: 0,
                y: 0,
                width,
                height,
            },
        })
    }
}

fn parse_u32_field(name: &str, value: &str) -> Result<u32> {
    let parsed = value
        .trim()
        .parse::<u32>()
        .map_err(|err| anyhow!("invalid {name}: {err}"))?;
    if parsed == 0 {
        return Err(anyhow!("{name} must be > 0"));
    }
    Ok(parsed)
}

fn parse_u8_field(name: &str, value: &str) -> Result<u8> {
    let parsed = value
        .trim()
        .parse::<u8>()
        .map_err(|err| anyhow!("invalid {name}: {err}"))?;
    if !(1..=16).contains(&parsed) {
        return Err(anyhow!("{name} must be in 1..=16"));
    }
    Ok(parsed)
}

fn grayscale_preview(frame: &RawFrame) -> ColorImage {
    let width = frame.spec.width as usize;
    let height = frame.spec.height as usize;
    let stride = frame.spec.stride_pixels as usize;
    let max_code = u32::from(frame.spec.max_code_value()).max(1);
    let pixels = frame.pixels();
    let mut gray = Vec::with_capacity(width * height);

    for y in 0..height {
        let row_start = y * stride;
        for x in 0..width {
            let value = u32::from(pixels[row_start + x]);
            gray.push(((value * 255) / max_code) as u8);
        }
    }
    ColorImage::from_gray([width, height], &gray)
}

fn hover_pixel(image_rect: egui::Rect, pos: egui::Pos2, frame: &RawFrame) -> Option<CursorPixel> {
    if !image_rect.contains(pos) || image_rect.width() <= 0.0 || image_rect.height() <= 0.0 {
        return None;
    }
    let width = frame.spec.width;
    let height = frame.spec.height;
    let x = (((pos.x - image_rect.left()) / image_rect.width()) * width as f32).floor() as u32;
    let y = (((pos.y - image_rect.top()) / image_rect.height()) * height as f32).floor() as u32;
    if x >= width || y >= height {
        return None;
    }
    let index = y as usize * frame.spec.stride_pixels as usize + x as usize;
    Some(CursorPixel {
        x,
        y,
        value: frame.pixels()[index],
    })
}

fn draw_roi_overlay(painter: &egui::Painter, image_rect: egui::Rect, frame: &RawFrame, roi: Roi) {
    let Some(roi) = roi.clamped_to(frame.spec.width, frame.spec.height) else {
        return;
    };
    let scale_x = image_rect.width() / frame.spec.width as f32;
    let scale_y = image_rect.height() / frame.spec.height as f32;
    let min = egui::pos2(
        image_rect.left() + roi.x as f32 * scale_x,
        image_rect.top() + roi.y as f32 * scale_y,
    );
    let max = egui::pos2(
        image_rect.left() + (roi.x + roi.width) as f32 * scale_x,
        image_rect.top() + (roi.y + roi.height) as f32 * scale_y,
    );
    painter.rect_stroke(
        egui::Rect::from_min_max(min, max),
        0.0,
        egui::Stroke::new(1.0, egui::Color32::GREEN),
        egui::StrokeKind::Outside,
    );
}

fn draw_minimap(
    painter: &egui::Painter,
    viewer_rect: egui::Rect,
    image_rect: egui::Rect,
    loaded: &LoadedRaw,
) {
    let image_width = loaded.frame.spec.width as f32;
    let image_height = loaded.frame.spec.height as f32;
    if image_width <= 0.0 || image_height <= 0.0 {
        return;
    }

    let fit = (MINIMAP_MAX_SIZE.x / image_width)
        .min(MINIMAP_MAX_SIZE.y / image_height)
        .min(1.0);
    let minimap_size = egui::vec2(image_width * fit, image_height * fit).max(egui::vec2(1.0, 1.0));
    let minimap_rect = egui::Rect::from_min_size(
        egui::pos2(
            viewer_rect.right() - minimap_size.x - MINIMAP_MARGIN,
            viewer_rect.bottom() - minimap_size.y - MINIMAP_MARGIN,
        ),
        minimap_size,
    );

    painter.rect_filled(
        minimap_rect.expand(4.0),
        2.0,
        egui::Color32::from_black_alpha(180),
    );
    painter.image(
        loaded.texture.id(),
        minimap_rect,
        egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
        egui::Color32::WHITE,
    );

    let visible_min = screen_to_image(viewer_rect.min, image_rect, image_width, image_height);
    let visible_max = screen_to_image(viewer_rect.max, image_rect, image_width, image_height);
    let min = egui::pos2(
        minimap_rect.left() + visible_min.x * fit,
        minimap_rect.top() + visible_min.y * fit,
    );
    let max = egui::pos2(
        minimap_rect.left() + visible_max.x * fit,
        minimap_rect.top() + visible_max.y * fit,
    );
    painter.rect_stroke(
        egui::Rect::from_min_max(min, max),
        0.0,
        egui::Stroke::new(1.0, egui::Color32::YELLOW),
        egui::StrokeKind::Outside,
    );
}

fn screen_to_image(
    pos: egui::Pos2,
    image_rect: egui::Rect,
    image_width: f32,
    image_height: f32,
) -> egui::Pos2 {
    let x =
        (((pos.x - image_rect.left()) / image_rect.width()) * image_width).clamp(0.0, image_width);
    let y = (((pos.y - image_rect.top()) / image_rect.height()) * image_height)
        .clamp(0.0, image_height);
    egui::pos2(x, y)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn padded_frame() -> RawFrame {
        RawFrame::new(
            RawSpec {
                width: 2,
                height: 2,
                bit_depth: 10,
                stride_pixels: 3,
                bayer: BayerPattern::Rggb,
            },
            vec![0, 1023, 777, 512, 256, 888],
        )
        .unwrap()
    }

    #[test]
    fn grayscale_preview_ignores_stride_padding() {
        let image = grayscale_preview(&padded_frame());

        assert_eq!(image.size, [2, 2]);
        assert_eq!(image.pixels[0], egui::Color32::from_gray(0));
        assert_eq!(image.pixels[1], egui::Color32::from_gray(255));
        assert_eq!(image.pixels[2], egui::Color32::from_gray(127));
        assert_eq!(image.pixels[3], egui::Color32::from_gray(63));
    }

    #[test]
    fn hover_pixel_uses_stride_for_row_indexing() {
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(20.0, 20.0));
        let hovered = hover_pixel(rect, egui::pos2(15.0, 15.0), &padded_frame()).unwrap();

        assert_eq!((hovered.x, hovered.y, hovered.value), (1, 1, 256));
    }

    #[test]
    fn raw_dialog_defaults_stride_to_width() {
        let dialog = RawOpenDialogState {
            visible: true,
            path: PathBuf::from("frame.raw"),
            width: "4".to_owned(),
            height: "3".to_owned(),
            bit_depth: "10".to_owned(),
            stride_pixels: String::new(),
            bayer: BayerUi::Rggb,
            storage: RawStorageUi::UnpackedU16,
            endian: EndianUi::Little,
            error: None,
        };

        let request = dialog.to_request().unwrap();

        assert_eq!(request.spec.stride_pixels, 4);
        assert_eq!(request.roi.width, 4);
        assert_eq!(request.roi.height, 3);
    }

    #[test]
    fn raw_dialog_rejects_unsupported_big_endian() {
        let dialog = RawOpenDialogState {
            endian: EndianUi::Big,
            ..RawOpenDialogState::for_path(PathBuf::from("frame.raw"))
        };

        let err = dialog.to_request().unwrap_err();

        assert!(err.to_string().contains("little-endian"));
    }
}
