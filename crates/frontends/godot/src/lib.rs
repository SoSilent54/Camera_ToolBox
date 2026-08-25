//! pongbot-calib-tool：Godot 桌面端 X5_233 标定工具（gdext 入口）。
//!
//! 全部 UI 用 Rust 代码构建（不依赖 Godot 编辑器可视化搭建）；
//! 运行：`godot --path crates/frontends/godot/godot`。

mod guide_overlay;
mod preview;
mod solve;
mod ui;
mod x5;
mod eeprom;

use godot::classes::control::LayoutPreset;
use godot::classes::{Control, IControl, Texture2D};
use godot::prelude::*;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use ui::steps::StepId;
use ui::{theme, UiState};

use camera_toolbox_app::platform::EepromInspectResult;
use camera_toolbox_core::CalibrationSolution;
use preview::StreamState;
use solve::solve_channel;
use camera_toolbox_core::BoardSpec;

/// 应用根节点：挂载 5 步向导 UI 与领域层控制器。
#[derive(GodotClass)]
#[class(init, base = Control)]
pub struct CalibApp {
    base: Base<Control>,
    ui: Option<UiState>,
    /// 后台任务结果槽：worker 线程写入，主线程 `_process` 轮询取走。
    pending_task: Option<Arc<Mutex<Option<String>>>>,
    /// 双路 RTSP 预览状态（Step 1 连通后启动）。
    streams: Option<StreamState>,
    /// 双路采集完成标记（防止重复触发完成事件）。
    preview_finished: bool,
    /// 求解结果共享槽（闭包 → 主线程回传完整 CalibrationSolution）。
    pending_solutions:
        Option<Arc<Mutex<Option<(Option<CalibrationSolution>, Option<CalibrationSolution>)>>>>,
    /// 最近一次双路求解解（EEPROM 写入使用）。
    solutions: Option<(CalibrationSolution, CalibrationSolution)>,
    /// 最近一次 EEPROM Inspect 结果（写入时校验 before 镜像）。
    eeprom_inspect: Option<EepromInspectResult>,
    /// EEPROM Inspect 结果共享槽（闭包 → 主线程）。
    pending_inspect: Option<Arc<Mutex<Option<EepromInspectResult>>>>,
    /// EEPROM 写入二次确认标志。
    write_armed: bool,
    /// 调试截图请求（`PONGBOT_SCREENSHOT` 环境变量触发，5 帧后保存）。
    screenshot: Option<ScreenshotRequest>,
}

/// 调试截图请求。
struct ScreenshotRequest {
    path: String,
    frame: u32,
}

#[godot_api]
impl IControl for CalibApp {
    fn ready(&mut self) {
        // 场景根 Control 由手写 .tscn 声明、无尺寸：必须显式铺满窗口。
        self.base_mut().set_anchors_and_offsets_preset(LayoutPreset::FULL_RECT);
        self.base_mut().set_size(Vector2::new(1280.0, 800.0));
        theme::install_cjk_font();
        theme::install_window_background();

        let (state, root) = UiState::build();
        self.base_mut().add_child(&root);
        self.ui = Some(state);
        if let Some(state) = self.ui.as_mut() {
            // Step 1 按钮信号接线。
            state
                .connect
                .probe_button
                .signals()
                .pressed()
                .connect(button_callback(self.base.__constructed_gd().cast::<CalibApp>(), "on_probe"));
            state
                .connect
                .bootstrap_button
                .signals()
                .pressed()
                .connect(button_callback(
                    self.base.__constructed_gd().cast::<CalibApp>(),
                    "on_bootstrap",
                ));
            // Step 2 采集按钮（双路独立开关）。
            state
                .preview
                .ch0
                .capture_button
                .signals()
                .pressed()
                .connect(button_callback(
                    self.base.__constructed_gd().cast::<CalibApp>(),
                    "on_toggle_capture_ch0",
                ));
            state
                .preview
                .ch3
                .capture_button
                .signals()
                .pressed()
                .connect(button_callback(
                    self.base.__constructed_gd().cast::<CalibApp>(),
                    "on_toggle_capture_ch3",
                ));
            // Step 3 求解按钮。
            state
                .solve
                .solve_button
                .signals()
                .pressed()
                .connect(button_callback(
                    self.base.__constructed_gd().cast::<CalibApp>(),
                    "on_solve",
                ));
            // Step 4 EEPROM 按钮。
            state
                .eeprom
                .inspect_button
                .signals()
                .pressed()
                .connect(button_callback(
                    self.base.__constructed_gd().cast::<CalibApp>(),
                    "on_eeprom_inspect",
                ));
            state
                .eeprom
                .write_button
                .signals()
                .pressed()
                .connect(button_callback(
                    self.base.__constructed_gd().cast::<CalibApp>(),
                    "on_eeprom_write",
                ));
        }
        // 调试截图：PONGBOT_SCREENSHOT 环境变量，5 帧后保存 viewport。
        if let Ok(path) = std::env::var("PONGBOT_SCREENSHOT") {
            self.screenshot = Some(ScreenshotRequest { path, frame: 0 });
            godot_print!("debug: 将在 5 帧后保存截图");
        }
        // 合成模式（无板验证）：跳过 Step 1，直接进入双预览。
        if std::env::var("PONGBOT_SYNTH").is_ok_and(|value| value == "1" || value == "board") {
            if let Some(state) = self.ui.as_mut() {
                state.complete_step(StepId::Connect, "合成模式：跳过设备连接");
            }
            self.start_previews("synth");
            // 合成模式自动开始双路采集（验证采集链路）。
            if let Some(streams) = self.streams.as_mut() {
                streams.ch0.toggle_capture(BoardSpec { inner_cols: 9, inner_rows: 6, square_size: 15.0 });
                streams.ch3.toggle_capture(BoardSpec { inner_cols: 9, inner_rows: 6, square_size: 15.0 });
            }
            if let Some(ui) = self.ui.as_mut() {
                ui.preview.ch0.capture_button.set_text("停止采集");
                ui.preview.ch3.capture_button.set_text("停止采集");
            }
        }
        godot_print!("pongbot-calib-tool: wizard UI ready");
    }
    /// 主线程帧回调：轮询后台任务结果、上传预览新帧、处理调试截图。
    fn process(&mut self, _delta: f64) {
        // 后台任务结果（probe / bootstrap）。
        let task_text = self
            .pending_task
            .as_ref()
            .and_then(|slot| slot.lock().ok())
            .and_then(|mut value| value.take());
        if let Some(text) = task_text {
            self.pending_task = None;
            self.finish_task(text);
        }

        // 预览宽高比保持（16:9）：宽度铺满可用，高度随宽度同步。
        if let Some(ui) = self.ui.as_mut() {
            for mut card in [ui.preview.ch0.view.clone(), ui.preview.ch3.view.clone()] {
                let width = card.get_size().x.max(320.0);
                let target_height = width * 9.0 / 16.0;
                card.set_custom_minimum_size(Vector2::new(0.0, target_height));
            }
        }

        // 双路预览与 guided 采集：新帧上传纹理 + 引导文本刷新。
        let capture_done = if let (Some(streams), Some(ui)) = (self.streams.as_mut(), self.ui.as_mut())
        {
            let _ = streams.ch0.pump(&mut ui.preview.ch0.texture_rect);
            let _ = streams.ch3.pump(&mut ui.preview.ch3.texture_rect);
            let (text0, count0, hold0) = streams.ch0.guide();
            if !text0.is_empty() {
                ui.preview.ch0.set_overlay(
                    &text0,
                    overlay_color(count0, hold0),
                );
            }
            let (text3, count3, hold3) = streams.ch3.guide();
            if !text3.is_empty() {
                ui.preview.ch3.set_overlay(
                    &text3,
                    overlay_color(count3, hold3),
                );
            }
            streams.both_complete()
        } else {
            false
        };
        if capture_done && !self.preview_finished {
            self.preview_finished = true;
            godot_print!("双路采集完成");
            if let Some(ui) = self.ui.as_mut() {
                ui.complete_step(StepId::Preview, "双路采集完成 · 可进入求解");
            }
            // 合成模式：采集完成后自动触发求解（验证检测/标定管线）。
            if std::env::var("PONGBOT_SYNTH").is_ok_and(|value| value == "1" || value == "board") {
                self.on_solve();
            }
        }

        // 调试截图。
        if let Some(request) = self.screenshot.as_mut() {
            request.frame += 1;
            if request.frame >= 5 {
                let request = self.screenshot.take().expect("screenshot 请求存在");
                let gd = self.base.__constructed_gd();
                let image = gd
                    .get_viewport()
                    .and_then(|viewport| viewport.get_texture())
                    .map(|texture| texture.upcast::<Texture2D>())
                    .and_then(|texture| texture.get_image());
                match image {
                    Some(image) => {
                        let result = image.save_png(request.path.as_str());
                        godot_print!("debug: 截图保存结果 {result:?} -> {}", request.path);
                    }
                    None => godot_print!("debug: 截图失败：viewport 图像不可用"),
                }
            }
        }
    }
}

#[godot_api]
impl CalibApp {
    /// 读取状态：worker 线程探测 X5 TCP（纯 Rust，不触碰 Godot 对象）。
    #[func]
    fn on_probe(&mut self) {
        let ip = self
            .ui
            .as_ref()
            .map(|state| state.connect.device_ip.get_text().to_string())
            .unwrap_or_default();
        let ip = ip.trim().to_owned();
        if ip.is_empty() {
            self.connect_status("请输入设备 IP", theme::WARN);
            return;
        }
        self.connect_status(&format!("正在探测 {ip}:9073 …"), theme::MUTED);
        self.spawn_task(move || {
            let result = x5::probe(&ip, 9073);
            match result {
                Ok(summary) => format!("驱动已就绪：{summary:?}"),
                Err(error) => format!("探测失败：{error}"),
            }
        });
    }

    /// 启动驱动：SSH 启动板端 DEMO233 并等待 TCP 9073 就绪（worker 线程）。
    #[func]
    fn on_bootstrap(&mut self) {
        let (host, user, password) = {
            let Some(state) = self.ui.as_mut() else {
                return;
            };
            (
                state.connect.device_ip.get_text().to_string(),
                state.connect.ssh_user.get_text().to_string(),
                state.connect.ssh_password.get_text().to_string(),
            )
        };
        let host = host.trim().to_owned();
        let user = user.trim().to_owned();
        if host.is_empty() {
            self.connect_status("请输入设备 IP", theme::WARN);
            return;
        }
        self.connect_status(
            &format!("正在通过 SSH 启动 {host} 上的 DEMO233 …"),
            theme::MUTED,
        );
        self.spawn_task(move || {
            let result = x5::bootstrap_driver(&host, 22, &user, &password, 9073);
            match result {
                Ok(summary) => format!("驱动已就绪：{summary:?}"),
                Err(error) => format!("启动失败：{error}"),
            }
        });
    }

    /// CH0 采集开关（guided：按当前棋盘参数启动）。
    #[func]
    fn on_toggle_capture_ch0(&mut self) {
        let board = self.current_board();
        let on = self
            .streams
            .as_mut()
            .map(|streams| streams.ch0.toggle_capture(board))
            .unwrap_or(false);
        if let Some(ui) = self.ui.as_mut() {
            ui.preview.ch0.capture_button.set_text(if on { "停止采集" } else { "开始采集" });
            if on {
                ui.preview.ch0.set_overlay("采集中…", theme::OK);
            }
        }
    }

    /// CH3 采集开关（guided：按当前棋盘参数启动）。
    #[func]
    fn on_toggle_capture_ch3(&mut self) {
        let board = self.current_board();
        let on = self
            .streams
            .as_mut()
            .map(|streams| streams.ch3.toggle_capture(board))
            .unwrap_or(false);
        if let Some(ui) = self.ui.as_mut() {
            ui.preview.ch3.capture_button.set_text(if on { "停止采集" } else { "开始采集" });
            if on {
                ui.preview.ch3.set_overlay("采集中…", theme::OK);
            }
        }
    }

    /// 从 Step 3 面板读取棋盘参数。
    fn current_board(&self) -> BoardSpec {
        let (cols, rows, square_mm) = self
            .ui
            .as_ref()
            .map(|ui| {
                (
                    ui.solve.board_cols.get_value() as u16,
                    ui.solve.board_rows.get_value() as u16,
                    ui.solve.square_mm.get_value(),
                )
            })
            .unwrap_or((9, 6, 15.0));
        BoardSpec {
            inner_cols: cols,
            inner_rows: rows,
            square_size: square_mm,
        }
    }

    /// 读取 Step 1 的连接凭据。
    fn connect_credentials(&self) -> (String, String, String) {
        self.ui
            .as_ref()
            .map(|ui| {
                (
                    ui.connect.device_ip.get_text().to_string(),
                    ui.connect.ssh_user.get_text().to_string(),
                    ui.connect.ssh_password.get_text().to_string(),
                )
            })
            .unwrap_or_default()
    }

    /// 读取当前 EEPROM 状态（Inspect）。
    #[func]
    fn on_eeprom_inspect(&mut self) {
        let Some(helper) = eeprom::locate_helper() else {
            if let Some(ui) = self.ui.as_mut() {
                ui.eeprom.set_status("未找到 camera-i2c-helper（先 cargo build -p camera-i2c-helper）", false);
            }
            return;
        };
        let (host, user, password) = self.connect_credentials();
        if host.trim().is_empty() {
            if let Some(ui) = self.ui.as_mut() {
                ui.eeprom.set_status("请先在 Step 1 填写设备 IP", false);
            }
            return;
        }
        if let Some(ui) = self.ui.as_mut() {
            ui.eeprom.set_status("正在读取 EEPROM 状态…", false);
        }
        let inspect_slot = Arc::new(Mutex::new(None));
        self.pending_inspect = Some(Arc::clone(&inspect_slot));
        self.spawn_task(move || {
            let result = eeprom::inspect(&host, &user, &password, 4, helper.into());
            match result {
                Ok(camera_toolbox_app::platform::EepromHelperResult::Inspect(inspect)) => {
                    if let Ok(mut slot) = inspect_slot.lock() {
                        *slot = Some(inspect.clone());
                    }
                    let serial = match &inspect.state.serial {
                        camera_toolbox_app::platform::EepromSerialState::Valid { value } => {
                            value.clone()
                        }
                        camera_toolbox_app::platform::EepromSerialState::Empty => {
                            "（空）".to_owned()
                        }
                        camera_toolbox_app::platform::EepromSerialState::Invalid {
                            raw_hex,
                            ..
                        } => format!("（无效：{raw_hex}）"),
                    };
                    format!(
                        "EEPROM 状态：镜像 {} · FLAG {} · SN {}",
                        &inspect.state.image_sha256[..8.min(inspect.state.image_sha256.len())],
                        if inspect.state.flag_valid { "有效" } else { "无效" },
                        serial
                    )
                }
                Ok(_) => "EEPROM 状态：未知响应".to_owned(),
                Err(error) => format!("读取 EEPROM 失败：{error}"),
            }
        });
    }

    /// 写入标定结果（UpdateCalibration，二次确认）。
    #[func]
    fn on_eeprom_write(&mut self) {
        if !self.write_armed {
            self.write_armed = true;
            if let Some(ui) = self.ui.as_mut() {
                ui.eeprom
                    .write_button
                    .set_text("确认写入？（将覆盖内参区）");
            }
            return;
        }
        self.write_armed = false;
        if let Some(ui) = self.ui.as_mut() {
            ui.eeprom.write_button.set_text("写入标定结果");
        }
        let Some(helper) = eeprom::locate_helper() else {
            if let Some(ui) = self.ui.as_mut() {
                ui.eeprom.set_status("未找到 camera-i2c-helper", false);
            }
            return;
        };
        let Some((solution, _)) = self.solutions.clone() else {
            if let Some(ui) = self.ui.as_mut() {
                ui.eeprom.set_status("请先完成标定求解（Step 3）", false);
            }
            return;
        };
        let Some(inspect) = self.eeprom_inspect.clone() else {
            if let Some(ui) = self.ui.as_mut() {
                ui.eeprom.set_status("请先读取 EEPROM 状态", false);
            }
            return;
        };
        let serial = match &inspect.state.serial {
            camera_toolbox_app::platform::EepromSerialState::Valid { value } => value.clone(),
            camera_toolbox_app::platform::EepromSerialState::Empty => {
                if let Some(ui) = self.ui.as_mut() {
                    ui.eeprom.set_status("EEPROM 无 SN，无法写入", false);
                }
                return;
            }
            camera_toolbox_app::platform::EepromSerialState::Invalid { .. } => {
                if let Some(ui) = self.ui.as_mut() {
                    ui.eeprom.set_status("EEPROM SN 无效，无法写入", false);
                }
                return;
            }
        };
        let before_sha256 = inspect.state.image_sha256.clone();
        let (host, user, password) = self.connect_credentials();
        if let Some(ui) = self.ui.as_mut() {
            ui.eeprom.set_status("正在写入 EEPROM…", false);
        }
        self.spawn_task(move || {
            let result = eeprom::provision_calibration(
                &host, &user, &password, 4, helper.into(), &solution, &serial, &before_sha256,
            );
            match result {
                Ok(_) => "写入 EEPROM 成功".to_owned(),
                Err(error) => format!("写入 EEPROM 失败：{error}"),
            }
        });
    }

    /// 执行标定：对双路 dataset 分别求解（后台线程）。
    #[func]
    fn on_solve(&mut self) {
        let Some(streams) = self.streams.as_ref() else {
            self.connect_status("请先完成双路采集", theme::WARN);
            return;
        };
        let (cols, rows, square_mm) = {
            let Some(ui) = self.ui.as_mut() else {
                return;
            };
            (
                ui.solve.board_cols.get_value() as u16,
                ui.solve.board_rows.get_value() as u16,
                ui.solve.square_mm.get_value(),
            )
        };
        let board = BoardSpec {
            inner_cols: cols,
            inner_rows: rows,
            square_size: square_mm,
        };
        if board.validate().is_err() {
            self.connect_status("棋盘参数非法", theme::ERR);
            return;
        }
        let ch0_frames = streams.ch0.captured_frames();
        let ch3_frames = streams.ch3.captured_frames();
        if let Some(ui) = self.ui.as_mut() {
            ui.solve.ch0_result.set_text("CH0：求解中…");
            ui.solve.ch3_result.set_text("CH3：求解中…");
            ui.solve
                .ch0_result
                .add_theme_color_override("font_color", theme::MUTED);
            ui.solve
                .ch3_result
                .add_theme_color_override("font_color", theme::MUTED);
        }
        let solution_slot = Arc::new(Mutex::new(None));
        self.pending_solutions = Some(Arc::clone(&solution_slot));
        self.spawn_task(move || {
            let r0 = solve_channel(0, &ch0_frames, board);
            let r1 = solve_channel(3, &ch3_frames, board);
            let pair = (
                r0.as_ref().ok().map(|r| r.solution.clone()),
                r1.as_ref().ok().map(|r| r.solution.clone()),
            );
            if let Ok(mut slot) = solution_slot.lock() {
                *slot = Some(pair);
            }
            match (r0, r1) {
                (Ok(a), Ok(b)) => format!("{}\n{}", a.summary(), b.summary()),
                (Ok(a), Err(e)) => format!("{}\nCH3 失败：{e}", a.summary()),
                (Err(e), Ok(b)) => format!("CH0 失败：{e}\n{}", b.summary()),
                (Err(e0), Err(e1)) => format!("CH0 失败：{e0}\nCH3 失败：{e1}"),
            }
        });
    }

    /// 主线程处理后台任务结果（由 `_process` 轮询触发）。
    fn finish_task(&mut self, text: String) {
        if text.starts_with("驱动已就绪") {
            if let Some(state) = self.ui.as_mut() {
                state.complete_step(StepId::Connect, "驱动已就绪 · 可连接预览");
            }
            let host = self
                .ui
                .as_ref()
                .map(|state| state.connect.device_ip.get_text().to_string())
                .unwrap_or_default();
            self.start_previews(&host);
            self.connect_status(&text, theme::OK);
        } else if text.contains("求解完成") {
            // 双路求解结果：CH0 / CH3 分行展示。
            godot_print!("求解结果：{text}");
            let lines: Vec<&str> = text.lines().collect();
            let mut both_ok = true;
            if let Some(ui) = self.ui.as_mut() {
                let ch0_text = lines.first().copied().unwrap_or("CH0：无结果");
                let ch3_text = lines.get(1).copied().unwrap_or("CH3：无结果");
                ui.solve.set_result(ui.solve.ch0_result.clone(), ch0_text);
                ui.solve.set_result(ui.solve.ch3_result.clone(), ch3_text);
                both_ok = !ch0_text.contains("失败") && !ch3_text.contains("失败");
            }
            // 取回完整标定解（EEPROM 写入使用）。
            if let Some(slot) = self.pending_solutions.take() {
                if let Ok(mut value) = slot.lock() {
                    if let Some((s0, s1)) = value.take() {
                        if let (Some(a), Some(b)) = (s0, s1) {
                            self.solutions = Some((a, b));
                        }
                    }
                }
            }
            if both_ok && self.solutions.is_some() {
                if let Some(ui) = self.ui.as_mut() {
                    ui.complete_step(StepId::Solve, "两路标定完成 · 可写入 EEPROM");
                    ui.eeprom.write_button.set_disabled(false);
                }
            }
        } else if text.contains("EEPROM 状态") || text.contains("读取 EEPROM 失败") {
            // EEPROM Inspect 结果。
            if let Some(ui) = self.ui.as_mut() {
                let ok = text.contains("EEPROM 状态");
                ui.eeprom.set_status(&text, ok);
                if ok {
                    ui.eeprom
                        .write_button
                        .set_disabled(self.eeprom_inspect.is_none());
                }
            }
        } else if text.contains("写入 EEPROM") {
            // EEPROM 写入结果。
            godot_print!("{text}");
            if let Some(ui) = self.ui.as_mut() {
                let ok = text.contains("写入成功");
                ui.eeprom.set_status(&text, ok);
                if ok {
                    ui.complete_step(StepId::Eeprom, "EEPROM 写入完成");
                }
            }
        } else {
            // probe/bootstrap 失败等：显示到 Step 1 状态行。
            godot_print!("任务结果（失败）：{text}");
            self.connect_status(&text, theme::ERR);
        }
    }

    /// 启动双路预览（Step 1 连通后自动调用；合成模式用于无板验证）。
    fn start_previews(&mut self, host: &str) {
        if self.streams.is_some() {
            return;
        }
        let host = host.trim().to_owned();
        if host.is_empty() {
            return;
        }
        let (slot0, slot1) = self
            .ui
            .as_ref()
            .map(|ui| (ui.overlay_slots.0.clone(), ui.overlay_slots.1.clone()))
            .unwrap_or_default();
        let mut state = StreamState::start(&host, slot0, slot1);
        let board = self.current_board();
        state.ch0.start_detect(board);
        state.ch3.start_detect(board);
        if let Some(ui) = self.ui.as_mut() {
            ui.preview.ch0.set_status(&format!("rtsp://{host}:554/PRR"));
            ui.preview.ch3.set_status(&format!("rtsp://{host}:557/PRR"));
            ui.preview.ch0.set_overlay("预览启动中…", theme::MUTED);
            ui.preview.ch3.set_overlay("预览启动中…", theme::MUTED);
        }
        self.streams = Some(state);
    }

    /// 派发后台任务：结果经共享槽回主线程。
    fn spawn_task(&mut self, task: impl FnOnce() -> String + Send + 'static) {
        let slot = Arc::new(Mutex::new(None));
        self.pending_task = Some(Arc::clone(&slot));
        std::thread::spawn(move || {
            let text = task();
            if let Ok(mut value) = slot.lock() {
                *value = Some(text);
            }
        });
    }

    fn connect_status(&mut self, text: &str, color: godot::builtin::Color) {
        if let Some(state) = self.ui.as_mut() {
            state.connect.set_status(text, color);
        }
    }
}

/// pressed 信号回调包装：转发到主线程上的 `#[func]` 方法。
fn button_callback(base: Gd<CalibApp>, method: &'static str) -> impl FnMut() + 'static {
    let mut base = base;
    move || {
        base.call_deferred(method, &[]);
    }
}

struct CalibExtension;

/// GDExtension 入口：宏自动注册所有 `#[derive(GodotClass)]` 类型。
#[gdextension]
unsafe impl ExtensionLibrary for CalibExtension {}

/// overlay 引导文本颜色：hold 达标/采集完成绿，hold 进行中黄，其余蓝。
fn overlay_color(count: usize, hold: u8) -> godot::builtin::Color {
    if count >= preview::CAPTURE_TARGET || hold >= preview::HOLD_TARGET {
        theme::OK
    } else if hold > 0 {
        theme::WARN
    } else {
        theme::ACCENT
    }
}
