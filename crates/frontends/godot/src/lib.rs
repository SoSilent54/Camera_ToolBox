//! pongbot-calib-tool：Godot 桌面端 X5_233 标定工具（gdext 入口）。
//!
//! 全部 UI 用 Rust 代码构建（不依赖 Godot 编辑器可视化搭建）；
//! 运行：`godot --path crates/frontends/godot/godot`。

mod eeprom;
mod eeprom_history;
mod guide_overlay;
mod preview;
mod solve;
mod ui;
mod x5;

use godot::classes::control::LayoutPreset;
use godot::classes::{Control, IControl, Texture2D};
use godot::prelude::*;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use ui::steps::StepId;
use ui::{theme, UiState};

use camera_toolbox_app::platform::{
    EepromHelperResult, EepromInspectResult, EepromSerialState, EepromWriteResult,
};
use camera_toolbox_core::calibration_eeprom::yg_stereo_p24c64g_v1;
use camera_toolbox_core::{BoardSpec, CalibrationSolution};
use preview::StreamState;
use solve::{
    distortion_summary, format_intrinsics_geometry, solution_detail_summary, solve_channel,
    view_rmse_values,
};

/// 应用根节点：挂载三步向导 UI 与领域层控制器。
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
    /// 最近一次双路 EEPROM Inspect 结果（写入时校验 before 镜像）。
    eeprom_inspect: Option<(EepromInspectResult, EepromInspectResult)>,
    /// EEPROM Inspect 结果共享槽（闭包 → 主线程）。
    pending_inspect:
        Option<Arc<Mutex<Option<(Option<EepromInspectResult>, Option<EepromInspectResult>)>>>>,
    /// EEPROM 写入二次确认标志。
    write_armed: bool,
    /// Step 1 持续连接检查结果槽。
    pending_connection_probe: Option<Arc<Mutex<Option<String>>>>,
    last_connection_probe: Option<Instant>,
    /// 调试截图请求（`PONGBOT_SCREENSHOT` 环境变量触发，默认 5 帧后保存）。
    screenshot: Option<ScreenshotRequest>,
}

/// 调试截图请求。
struct ScreenshotRequest {
    path: String,
    frame: u32,
    target_frame: u32,
}

#[godot_api]
impl IControl for CalibApp {
    fn ready(&mut self) {
        // 场景根 Control 由手写 .tscn 声明、无尺寸：必须显式铺满窗口。
        self.base_mut()
            .set_anchors_and_offsets_preset(LayoutPreset::FULL_RECT);
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
                .connect(button_callback(
                    self.base.__constructed_gd().cast::<CalibApp>(),
                    "on_probe",
                ));
            state
                .connect
                .bootstrap_button
                .signals()
                .pressed()
                .connect(button_callback(
                    self.base.__constructed_gd().cast::<CalibApp>(),
                    "on_bootstrap",
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
            // Step 3 EEPROM / Reset 按钮。
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
            state
                .eeprom
                .reset_button
                .signals()
                .pressed()
                .connect(button_callback(
                    self.base.__constructed_gd().cast::<CalibApp>(),
                    "on_reset_flow",
                ));
        }
        // 调试截图：PONGBOT_SCREENSHOT 环境变量，默认 5 帧后保存 viewport。
        if let Ok(path) = std::env::var("PONGBOT_SCREENSHOT") {
            let target_frame = std::env::var("PONGBOT_SCREENSHOT_FRAME")
                .ok()
                .and_then(|value| value.parse::<u32>().ok())
                .unwrap_or(5)
                .max(1);
            self.screenshot = Some(ScreenshotRequest {
                path,
                frame: 0,
                target_frame,
            });
            godot_print!("debug: 将在 {target_frame} 帧后保存截图");
        }
        // 合成模式（无板验证）：跳过 Step 1，直接进入双预览。
        if std::env::var("PONGBOT_SYNTH").is_ok_and(|value| value == "1" || value == "board") {
            if let Some(state) = self.ui.as_mut() {
                state.complete_step(StepId::Connect, "合成模式：跳过设备连接");
            }
            self.start_previews("synth");
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

        self.poll_connection_probe();

        // 预览宽高比保持（16:9）：宽度铺满可用，高度随宽度同步。
        if let Some(ui) = self.ui.as_mut() {
            for mut card in [ui.preview.ch0.view.clone(), ui.preview.ch3.view.clone()] {
                let width = card.get_size().x.max(320.0);
                let target_height = width * 9.0 / 16.0;
                card.set_custom_minimum_size(Vector2::new(0.0, target_height));
            }
        }

        // 双路预览与 guided 采集：新帧上传纹理 + 引导文本刷新。
        let capture_done =
            if let (Some(streams), Some(ui)) = (self.streams.as_mut(), self.ui.as_mut()) {
                let _ = streams.ch0.pump(&mut ui.preview.ch0.texture_rect);
                let _ = streams.ch3.pump(&mut ui.preview.ch3.texture_rect);
                // GuideOverlay 的绘制数据由 worker 写入 Arc 槽；Godot 自定义 Control
                // 不会因 Rust 共享槽变化自动重绘，必须主线程逐帧请求刷新。
                ui.preview.ch0.guide_overlay.queue_redraw();
                ui.preview.ch3.guide_overlay.queue_redraw();
                let (text0, count0, hold0) = streams.ch0.guide();
                if !text0.is_empty() {
                    ui.preview
                        .ch0
                        .set_overlay(&text0, overlay_color(count0, hold0));
                }
                let (text3, count3, hold3) = streams.ch3.guide();
                if !text3.is_empty() {
                    ui.preview
                        .ch3
                        .set_overlay(&text3, overlay_color(count3, hold3));
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
            // guide auto_capture 完成后自动触发双路求解。
            self.on_solve();
        }

        // 调试截图。
        if let Some(request) = self.screenshot.as_mut() {
            request.frame += 1;
            if request.frame >= request.target_frame {
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
            .unwrap_or((11, 8, 40.0));
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

    /// 读取双路 EEPROM 状态（Inspect）。
    #[func]
    fn on_eeprom_inspect(&mut self) {
        let Some(helper) = eeprom::locate_helper() else {
            if let Some(ui) = self.ui.as_mut() {
                ui.eeprom.set_status(
                    "未找到 camera-i2c-helper（先 cargo build -p camera-i2c-helper --release）",
                    false,
                );
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
            ui.eeprom.set_status("正在读取 CH0/CH3 EEPROM 状态…", false);
        }
        let inspect_slot = Arc::new(Mutex::new(None));
        self.pending_inspect = Some(Arc::clone(&inspect_slot));
        self.spawn_task(move || {
            let helper: Arc<[u8]> = helper.into();
            let ch0 = eeprom::inspect(&host, &user, &password, 4, Arc::clone(&helper));
            let ch3 = eeprom::inspect(&host, &user, &password, 6, helper);
            let mut inspect0 = None;
            let mut inspect3 = None;
            let text = match (ch0, ch3) {
                (Ok(EepromHelperResult::Inspect(a)), Ok(EepromHelperResult::Inspect(b))) => {
                    inspect0 = Some(a.clone());
                    inspect3 = Some(b.clone());
                    format!(
                        "EEPROM 状态：\n{}\n{}",
                        inspect_summary("CH0/i2c-4", &a),
                        inspect_summary("CH3/i2c-6", &b)
                    )
                }
                (a, b) => format!(
                    "读取 EEPROM 失败：CH0={}；CH3={}",
                    inspect_result_label(&a),
                    inspect_result_label(&b)
                ),
            };
            if let Ok(mut slot) = inspect_slot.lock() {
                *slot = Some((inspect0, inspect3));
            }
            text
        });
    }

    /// 写入双路标定结果（FullProvision：FLAG + 内参 + SN；二次确认）。
    #[func]
    fn on_eeprom_write(&mut self) {
        let Some((serial0, serial3)) = self
            .ui
            .as_mut()
            .map(|ui| ui.eeprom.serial_pair())
            .transpose()
            .unwrap_or_else(|error| {
                if let Some(ui) = self.ui.as_mut() {
                    ui.eeprom.set_status(&error, false);
                }
                None
            })
        else {
            return;
        };
        if !self.write_armed {
            self.write_armed = true;
            if let Some(ui) = self.ui.as_mut() {
                ui.eeprom.write_button.set_text("确认写入 CH0/CH3？");
                ui.eeprom.set_status(
                    &format!("请确认 SNID：CH0={serial0}，CH3={serial3}；再次点击开始写入。"),
                    true,
                );
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
        let Some((solution0, solution3)) = self.solutions.clone() else {
            if let Some(ui) = self.ui.as_mut() {
                ui.eeprom.set_status("请先完成标定求解（Step 3）", false);
            }
            return;
        };
        let Some((inspect0, inspect3)) = self.eeprom_inspect.clone() else {
            if let Some(ui) = self.ui.as_mut() {
                ui.eeprom.set_status("请先读取 EEPROM 状态", false);
            }
            return;
        };
        let before0 = inspect0.state.image_sha256.clone();
        let before3 = inspect3.state.image_sha256.clone();
        let (host, user, password) = self.connect_credentials();
        if let Some(ui) = self.ui.as_mut() {
            ui.eeprom.set_status("正在写入 CH0/CH3 EEPROM…", false);
        }
        self.spawn_task(move || {
            let helper: Arc<[u8]> = helper.into();
            let ch0 = eeprom::provision_full_calibration(
                &host,
                &user,
                &password,
                4,
                Arc::clone(&helper),
                &solution0,
                &serial0,
                &before0,
            );
            let ch3 = eeprom::provision_full_calibration(
                &host, &user, &password, 6, helper, &solution3, &serial3, &before3,
            );
            match (ch0, ch3) {
                (Ok(EepromHelperResult::Provision(a)), Ok(EepromHelperResult::Provision(b))) => {
                    let h0 = eeprom_history::persist_write_history(
                        "CH0/i2c-4",
                        4,
                        &serial0,
                        &solution0,
                        &a,
                    );
                    let h3 = eeprom_history::persist_write_history(
                        "CH3/i2c-6",
                        6,
                        &serial3,
                        &solution3,
                        &b,
                    );
                    match (h0, h3) {
                        (Ok(path0), Ok(path3)) => format!(
                            "写入 EEPROM 成功：\n待写入标定：\n{}\n{}\n写入状态：\n{}\n{}\nwrite_history：\nCH0 {path0}\nCH3 {path3}",
                            solution_detail_summary("CH0", &solution0),
                            solution_detail_summary("CH3", &solution3),
                            write_summary("CH0/i2c-4", &a),
                            write_summary("CH3/i2c-6", &b)
                        ),
                        (a_history, b_history) => format!(
                            "写入 EEPROM 成功，但保存 write_history 失败：CH0={}；CH3={}",
                            history_result_label(&a_history),
                            history_result_label(&b_history)
                        ),
                    }
                }
                (a, b) => format!(
                    "写入 EEPROM 失败：CH0={}；CH3={}",
                    provision_result_label(&a),
                    provision_result_label(&b)
                ),
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
            ui.solve.clear_charts();
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
        } else if text.contains("求解完成")
            || text.contains("CH0 失败")
            || text.contains("CH3 失败")
        {
            // 双路求解结果：成功时用完整解渲染几何摘要和单图 RMSE 柱状图。
            godot_print!("求解结果：{text}");
            let mut both_ok = false;
            let mut solved_pair = None;
            if let Some(slot) = self.pending_solutions.take() {
                if let Ok(mut value) = slot.lock() {
                    if let Some((s0, s1)) = value.take() {
                        match (s0, s1) {
                            (Some(a), Some(b)) => {
                                self.solutions = Some((a.clone(), b.clone()));
                                solved_pair = Some((a, b));
                                both_ok = true;
                            }
                            _ => {}
                        }
                    }
                }
            }
            if let Some(ui) = self.ui.as_mut() {
                if let Some((solution0, solution3)) = solved_pair.as_ref() {
                    ui.solve.set_result(
                        ui.solve.ch0_result.clone(),
                        &solution_detail_summary("CH0", solution0),
                    );
                    ui.solve.set_result(
                        ui.solve.ch3_result.clone(),
                        &solution_detail_summary("CH3", solution3),
                    );
                    ui.solve.set_chart(
                        ui.solve.ch0_chart.clone(),
                        view_rmse_values(solution0),
                        solution0.rms_error.max(0.5),
                    );
                    ui.solve.set_chart(
                        ui.solve.ch3_chart.clone(),
                        view_rmse_values(solution3),
                        solution3.rms_error.max(0.5),
                    );
                } else {
                    ui.solve.set_result(ui.solve.ch0_result.clone(), &text);
                    ui.solve
                        .set_result(ui.solve.ch3_result.clone(), "CH3：见上方错误详情");
                    ui.solve.clear_charts();
                }
            }
            if both_ok && self.solutions.is_some() {
                if let Some(ui) = self.ui.as_mut() {
                    ui.complete_step(StepId::Solve, "两路标定完成 · 等待双路 SNID 与 EEPROM 写入");
                    ui.eeprom.inspect_button.set_disabled(false);
                    ui.eeprom.write_button.set_disabled(true);
                }
                self.on_eeprom_inspect();
            }
        } else if text.contains("EEPROM 状态") || text.contains("读取 EEPROM 失败") {
            // EEPROM Inspect 结果。
            if let Some(slot) = self.pending_inspect.take() {
                if let Ok(mut value) = slot.lock() {
                    if let Some((Some(a), Some(b))) = value.take() {
                        self.eeprom_inspect = Some((a, b));
                    }
                }
            }
            if let Some(ui) = self.ui.as_mut() {
                let ok = text.contains("EEPROM 状态") && self.eeprom_inspect.is_some();
                if ok {
                    ui.eeprom.refresh_snid_previews();
                }
                ui.eeprom.set_status(&text, ok);
                ui.eeprom.write_button.set_disabled(!ok);
            }
        } else if text.contains("写入 EEPROM") {
            // EEPROM 写入结果。
            godot_print!("{text}");
            if let Some(ui) = self.ui.as_mut() {
                let ok = text.contains("写入 EEPROM 成功") && !text.contains("write_history 失败");
                ui.eeprom.set_status(&text, ok);
                if ok {
                    ui.complete_step(StepId::Solve, "EEPROM 写入完成");
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
        state.ch0.start_capture();
        state.ch3.start_capture();
        if let Some(ui) = self.ui.as_mut() {
            ui.preview.ch0.set_status(&format!("rtsp://{host}:554/PRR"));
            ui.preview.ch3.set_status(&format!("rtsp://{host}:557/PRR"));
            ui.preview
                .ch0
                .set_overlay("guide auto_capture 启动中…", theme::MUTED);
            ui.preview
                .ch3
                .set_overlay("guide auto_capture 启动中…", theme::MUTED);
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

    /// Reset 当前工件流程，保留预输入连接信息和 SNID 批次字段。
    #[func]
    fn on_reset_flow(&mut self) {
        self.streams = None;
        self.pending_task = None;
        self.pending_solutions = None;
        self.solutions = None;
        self.eeprom_inspect = None;
        self.pending_inspect = None;
        self.preview_finished = false;
        self.write_armed = false;
        if let Some(ui) = self.ui.as_mut() {
            if let Ok(mut slot) = ui.overlay_slots.0.lock() {
                *slot = None;
            }
            if let Ok(mut slot) = ui.overlay_slots.1.lock() {
                *slot = None;
            }
            ui.reset_flow();
        }
        self.start_connection_probe();
    }

    fn poll_connection_probe(&mut self) {
        let probe_text = self
            .pending_connection_probe
            .as_ref()
            .and_then(|slot| slot.lock().ok())
            .and_then(|mut value| value.take());
        if let Some(text) = probe_text {
            self.pending_connection_probe = None;
            let color = if text.starts_with("连接正常") {
                theme::OK
            } else {
                theme::WARN
            };
            self.connect_status(&text, color);
        }
        let should_probe = self.pending_connection_probe.is_none()
            && self.streams.is_none()
            && self
                .last_connection_probe
                .is_none_or(|last| last.elapsed() >= Duration::from_secs(2));
        if should_probe {
            self.start_connection_probe();
        }
    }

    fn start_connection_probe(&mut self) {
        if self.pending_connection_probe.is_some() {
            return;
        }
        let Some(ui) = self.ui.as_ref() else {
            return;
        };
        let host = ui.connect.device_ip.get_text().to_string();
        if host.trim().is_empty() {
            return;
        }
        self.last_connection_probe = Some(Instant::now());
        let slot = Arc::new(Mutex::new(None));
        self.pending_connection_probe = Some(Arc::clone(&slot));
        std::thread::spawn(move || {
            let text = match x5::probe(host.trim(), 9073) {
                Ok(summary) => format!("连接正常：TCP 9073 可用 · {summary:?}"),
                Err(error) => format!("持续检查：设备/驱动未就绪（{error}）"),
            };
            if let Ok(mut value) = slot.lock() {
                *value = Some(text);
            }
        });
    }
}

fn serial_state_label(serial: &EepromSerialState) -> String {
    match serial {
        EepromSerialState::Empty => "空".to_owned(),
        EepromSerialState::Valid { value } => value.clone(),
        EepromSerialState::Invalid { raw_hex, .. } => format!("无效：{raw_hex}"),
    }
}
fn inspect_summary(label: &str, inspect: &EepromInspectResult) -> String {
    let calibration = eeprom_backup_calibration_summary(&inspect.backup)
        .unwrap_or_else(|error| format!("EEPROM 标定解析失败：{error}"));
    format!(
        "{label} before={} FLAG={} SN={}\n{}",
        &inspect.state.image_sha256[..8.min(inspect.state.image_sha256.len())],
        if inspect.state.flag_valid {
            "有效"
        } else {
            "无效"
        },
        serial_state_label(&inspect.state.serial),
        calibration
    )
}

fn eeprom_backup_calibration_summary(backup: &[u8]) -> Result<String, String> {
    let map = yg_stereo_p24c64g_v1();
    let image_size = storage_field_range(map, "image_size")?;
    let camera = storage_field_range(map, "camera_matrix")?;
    let distortion = storage_field_range(map, "distortion")?;
    let width = read_u32_le(backup, image_size.start, "width")?;
    let height = read_u32_le(backup, image_size.start + 4, "height")?;
    let fx = f64::from(read_f32_le(backup, camera.start, "fx")?);
    let fy = f64::from(read_f32_le(backup, camera.start + 4, "fy")?);
    let cx = f64::from(read_f32_le(backup, camera.start + 8, "cx")?);
    let cy = f64::from(read_f32_le(backup, camera.start + 12, "cy")?);
    let mut distortion_values = Vec::with_capacity(12);
    let distortion_count = distortion.len() / std::mem::size_of::<f32>();
    for index in 0..distortion_count.min(12) {
        distortion_values.push(f64::from(read_f32_le(
            backup,
            distortion.start + index * std::mem::size_of::<f32>(),
            "distortion",
        )?));
    }
    Ok(format!(
        "当前 EEPROM 标定：{}\n畸变：{}",
        format_intrinsics_geometry(width, height, fx, fy, cx, cy),
        distortion_summary(&distortion_values)
    ))
}

fn storage_field_range(
    map: &camera_toolbox_core::calibration_eeprom::CalibrationStorageMap,
    name: &str,
) -> Result<std::ops::Range<usize>, String> {
    let field = map
        .fields
        .iter()
        .find(|field| field.name == name)
        .ok_or_else(|| format!("EEPROM map 缺少字段 {name}"))?;
    let start = usize::from(field.offset);
    let end = start + usize::from(field.byte_len);
    Ok(start..end)
}

fn read_u32_le(bytes: &[u8], offset: usize, name: &str) -> Result<u32, String> {
    let data = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| format!("EEPROM {name} 字段越界"))?;
    let mut value = [0_u8; 4];
    value.copy_from_slice(data);
    Ok(u32::from_le_bytes(value))
}

fn read_f32_le(bytes: &[u8], offset: usize, name: &str) -> Result<f32, String> {
    let data = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| format!("EEPROM {name} 字段越界"))?;
    let mut value = [0_u8; 4];
    value.copy_from_slice(data);
    let value = f32::from_le_bytes(value);
    if value.is_finite() {
        Ok(value)
    } else {
        Err(format!("EEPROM {name} 字段不是有限数"))
    }
}

fn write_summary(label: &str, result: &EepromWriteResult) -> String {
    format!(
        "{label} before={} SN={} → after={} SN={} verified={}",
        &result.before.image_sha256[..8.min(result.before.image_sha256.len())],
        serial_state_label(&result.before.serial),
        &result.after.image_sha256[..8.min(result.after.image_sha256.len())],
        serial_state_label(&result.after.serial),
        result.bytewise_verified
    )
}

fn inspect_result_label(result: &Result<EepromHelperResult, String>) -> String {
    match result {
        Ok(EepromHelperResult::Inspect(_)) => "ok".to_owned(),
        Ok(_) => "unexpected helper result".to_owned(),
        Err(error) => error.clone(),
    }
}

fn provision_result_label(result: &Result<EepromHelperResult, String>) -> String {
    match result {
        Ok(EepromHelperResult::Provision(_)) => "ok".to_owned(),
        Ok(_) => "unexpected helper result".to_owned(),
        Err(error) => error.clone(),
    }
}

fn history_result_label(result: &Result<String, String>) -> String {
    match result {
        Ok(path) => path.clone(),
        Err(error) => error.clone(),
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
