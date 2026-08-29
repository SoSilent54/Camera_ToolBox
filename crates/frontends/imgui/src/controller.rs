//! UI 无关的标定流程控制器。

use crate::guide_overlay::OverlayData;
use crate::observability::ObservabilityReport;
use crate::preview::{self, StreamState};
use crate::solve::{
    distortion_summary, format_intrinsics_geometry, solution_detail_summary,
    solve_channel_from_detections, view_rmse_values,
};
use crate::{eeprom, eeprom_history, theme, x5};
use camera_toolbox_app::platform::{
    DecodedVideoFrame, EepromHelperResult, EepromInspectResult, EepromSerialState,
    EepromWriteResult,
};
use camera_toolbox_core::calibration_eeprom::yg_stereo_p24c64g_v1;
use camera_toolbox_core::{
    BoardSpec, CalibrationSolution, YgStereoModuleCode, YgStereoSerialIdInput,
};
use std::ops::Range;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepId {
    Connect,
    Preview,
    Solve,
}

impl StepId {
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Connect => 0,
            Self::Preview => 1,
            Self::Solve => 2,
        }
    }
}

pub const STEP_TITLES: [&str; 3] = ["连接设备", "双路预览与采集", "求解检查与 EEPROM 写入"];

#[derive(Clone)]
pub struct CalibState {
    pub active: StepId,
    pub completed: [bool; 3],
    pub summaries: [String; 3],
    pub connect: ConnectState,
    pub preview: PreviewState,
    pub solve: SolveState,
    pub eeprom: EepromState,
    pub status_bar: String,
}

impl Default for CalibState {
    fn default() -> Self {
        Self {
            active: StepId::Connect,
            completed: [false; 3],
            summaries: [
                "未开始".to_owned(),
                "等待连接".to_owned(),
                "等待采集".to_owned(),
            ],
            connect: ConnectState::default(),
            preview: PreviewState::default(),
            solve: SolveState::default(),
            eeprom: EepromState::default(),
            status_bar: "就绪".to_owned(),
        }
    }
}

#[derive(Clone)]
pub struct ConnectState {
    pub device_ip: String,
    pub ssh_user: String,
    pub ssh_password: String,
    pub status: String,
    pub status_color: [f32; 4],
}

impl Default for ConnectState {
    fn default() -> Self {
        Self {
            device_ip: std::env::var("PONGBOT_DEVICE_IP")
                .unwrap_or_else(|_| "10.21.12.".to_owned()),
            ssh_user: "root".to_owned(),
            ssh_password: "root".to_owned(),
            status: "未连接".to_owned(),
            status_color: theme::MUTED,
        }
    }
}

#[derive(Clone)]
pub struct PreviewState {
    pub ch0: PreviewChannelState,
    pub ch3: PreviewChannelState,
}

impl Default for PreviewState {
    fn default() -> Self {
        Self {
            ch0: PreviewChannelState::new("未连接"),
            ch3: PreviewChannelState::new("未连接"),
        }
    }
}

#[derive(Clone)]
pub struct PreviewChannelState {
    pub status: String,
    pub overlay_text: String,
    pub overlay_color: [f32; 4],
    /// 采集质量快照（worker 转发，Step 2 指示器展示）。
    pub quality: preview::DatasetQuality,
}

impl PreviewChannelState {
    fn new(status: &str) -> Self {
        Self {
            status: status.to_owned(),
            overlay_text: "未连接".to_owned(),
            overlay_color: theme::WARN,
            quality: preview::DatasetQuality::default(),
        }
    }
}

#[derive(Clone)]
pub struct SolveState {
    pub board_cols: i32,
    pub board_rows: i32,
    pub square_mm: f32,
    pub ch0_result: String,
    pub ch3_result: String,
    pub ch0_rmse: Vec<f64>,
    pub ch3_rmse: Vec<f64>,
    pub ch0_limit: f64,
    pub ch3_limit: f64,
    pub ch0_detail: Option<crate::solve::SolutionDetail>,
    pub ch3_detail: Option<crate::solve::SolutionDetail>,
    pub ch0_observability: Option<ObservabilityReport>,
    pub ch3_observability: Option<ObservabilityReport>,
}

impl Default for SolveState {
    fn default() -> Self {
        Self {
            board_cols: 11,
            board_rows: 8,
            square_mm: 40.0,
            ch0_result: "CH0：待求解".to_owned(),
            ch3_result: "CH3：待求解".to_owned(),
            ch0_rmse: Vec::new(),
            ch3_rmse: Vec::new(),
            ch0_limit: 0.5,
            ch3_limit: 0.5,
            ch0_detail: None,
            ch3_detail: None,
            ch0_observability: None,
            ch3_observability: None,
        }
    }
}

#[derive(Clone)]
pub struct EepromState {
    pub ch0_snid: SnidDraft,
    pub ch3_snid: SnidDraft,
    pub status: String,
    pub status_ok: bool,
    pub inspect_enabled: bool,
    pub write_enabled: bool,
    pub write_armed: bool,
    pub inspect: Option<(EepromInspectDetail, EepromInspectDetail)>,
    pub last_write: Option<(EepromWriteDetail, EepromWriteDetail)>,
    pub write_history_paths: Option<(String, String)>,
}

impl Default for EepromState {
    fn default() -> Self {
        Self {
            ch0_snid: SnidDraft::default(),
            ch3_snid: SnidDraft::default(),
            status: "等待求解完成后自动读取 EEPROM。输入两路序列号后预览 SNID，再确认写入。"
                .to_owned(),
            status_ok: false,
            inspect_enabled: false,
            write_enabled: false,
            write_armed: false,
            inspect: None,
            last_write: None,
            write_history_paths: None,
        }
    }
}

/// EEPROM 读取结果的结构化展示（Step 3 列表）。
#[derive(Clone, Debug)]
pub struct EepromInspectDetail {
    pub label: String,
    pub sha8: String,
    pub flag_valid: bool,
    pub serial: String,
    pub calibration: Option<BackupCalibrationDetail>,
    pub calibration_error: Option<String>,
}

/// EEPROM 内已存标定（镜像解析）的结构化指标。
#[derive(Clone, Debug)]
pub struct BackupCalibrationDetail {
    pub width: u32,
    pub height: u32,
    pub fx: f64,
    pub fy: f64,
    pub cx: f64,
    pub cy: f64,
    pub hfov_degrees: f64,
    pub vfov_degrees: f64,
    pub optical_x_degrees: f64,
    pub optical_y_degrees: f64,
    pub distortion: Vec<f64>,
}

/// EEPROM 写入结果的结构化展示（before → after）。
#[derive(Clone, Debug)]
pub struct EepromWriteDetail {
    pub label: String,
    pub before_sha8: String,
    pub before_serial: String,
    pub after_sha8: String,
    pub after_serial: String,
    pub verified: bool,
}

#[derive(Clone)]
pub struct SnidDraft {
    pub module_index: usize,
    pub year: String,
    pub month: String,
    pub day: String,
    pub axis_index: usize,
    pub sequence: String,
    pub preview: String,
    pub preview_ok: bool,
}

impl Default for SnidDraft {
    fn default() -> Self {
        let mut out = Self {
            module_index: 0,
            year: "26".to_owned(),
            month: String::new(),
            day: String::new(),
            axis_index: 0,
            sequence: "1".to_owned(),
            preview: String::new(),
            preview_ok: false,
        };
        let _ = out.refresh_preview();
        out
    }
}

impl SnidDraft {
    pub const MODULES: [&'static str; 2] = ["233", "235"];
    pub const AXES: [&'static str; 5] = ["0 - 未分类", "1 - L0", "2 - L1", "3 - R0", "4 - R1"];

    #[must_use]
    pub fn module_code(&self) -> YgStereoModuleCode {
        match self.module_index {
            1 => YgStereoModuleCode::Model235,
            _ => YgStereoModuleCode::Model233,
        }
    }

    pub fn serial_number(&self) -> Result<String, String> {
        let input = YgStereoSerialIdInput::new(
            self.module_code(),
            parse_two_digit_year(&self.year)?,
            parse_decimal_field("月份", &self.month)?,
            parse_decimal_field("日期", &self.day)?,
            u8::try_from(self.axis_index).unwrap_or(0),
            parse_decimal_field("序列号", &self.sequence)?,
        );
        input.serial_number().map_err(|error| error.to_string())
    }

    pub fn refresh_preview(&mut self) -> Result<String, String> {
        match self.serial_number() {
            Ok(serial) => {
                self.preview = format!("预览 SNID：{serial}");
                self.preview_ok = true;
                Ok(serial)
            }
            Err(error) => {
                self.preview = format!("SNID 未完成：{error}");
                self.preview_ok = false;
                Err(error)
            }
        }
    }
}

enum WorkerResult {
    Probe {
        text: String,
        ok: bool,
        host: String,
    },
    Bootstrap {
        text: String,
        ok: bool,
        host: String,
    },
    Solve {
        text: String,
        solutions: Option<(CalibrationSolution, CalibrationSolution)>,
        observability: Option<(Option<ObservabilityReport>, Option<ObservabilityReport>)>,
    },
    Inspect {
        text: String,
        inspect: Option<(EepromInspectResult, EepromInspectResult)>,
    },
    Write {
        text: String,
        ok: bool,
        write_results: Option<(EepromWriteDetail, EepromWriteDetail)>,
        history_paths: Option<(String, String)>,
        inspect: Option<(EepromInspectDetail, EepromInspectDetail)>,
    },
}

pub struct CalibController {
    pub state: CalibState,
    pending_task: Option<Arc<Mutex<Option<WorkerResult>>>>,
    streams: Option<StreamState>,
    preview_finished: bool,
    solve_after_preview_ready: bool,
    solutions: Option<(CalibrationSolution, CalibrationSolution)>,
    eeprom_inspect: Option<(EepromInspectResult, EepromInspectResult)>,
    pending_connection_probe: Option<Arc<Mutex<Option<String>>>>,
    last_connection_probe: Option<Instant>,
    ch0_overlay_slot: Arc<Mutex<Option<OverlayData>>>,
    ch3_overlay_slot: Arc<Mutex<Option<OverlayData>>>,
}

impl CalibController {
    #[must_use]
    pub fn new() -> Self {
        let mut out = Self {
            state: CalibState::default(),
            pending_task: None,
            streams: None,
            preview_finished: false,
            solve_after_preview_ready: false,
            solutions: None,
            eeprom_inspect: None,
            pending_connection_probe: None,
            last_connection_probe: None,
            ch0_overlay_slot: Arc::new(Mutex::new(None)),
            ch3_overlay_slot: Arc::new(Mutex::new(None)),
        };
        if std::env::var("PONGBOT_SYNTH").is_ok_and(|value| value == "1" || value == "board") {
            out.complete_step(StepId::Connect, "合成模式：跳过设备连接");
            out.start_previews("synth");
        }
        out
    }

    #[must_use]
    pub fn is_busy(&self) -> bool {
        self.pending_task.is_some()
    }

    pub fn tick(&mut self) {
        self.finish_pending_task();
        self.poll_connection_probe();
        self.refresh_preview_guides();
        if self
            .streams
            .as_ref()
            .is_some_and(StreamState::both_complete)
            && !self.preview_finished
        {
            self.preview_finished = true;
            self.complete_step(StepId::Preview, "双路采集完成 · 可进入求解");
            self.solve_after_preview_ready = true;
        }
        self.try_auto_solve_after_preview();
    }

    pub fn poll_frame(&mut self, channel: u16) -> Option<Arc<DecodedVideoFrame>> {
        let streams = self.streams.as_mut()?;
        match channel {
            0 => streams.ch0.poll_frame(),
            3 => streams.ch3.poll_frame(),
            _ => None,
        }
    }

    #[must_use]
    pub fn overlay(&self, channel: u16) -> Option<OverlayData> {
        let streams = self.streams.as_ref()?;
        match channel {
            0 => streams.ch0.overlay(),
            3 => streams.ch3.overlay(),
            _ => None,
        }
    }

    pub fn refresh_snid_previews(&mut self) {
        let _ = self.state.eeprom.ch0_snid.refresh_preview();
        let _ = self.state.eeprom.ch3_snid.refresh_preview();
    }

    pub fn probe(&mut self) {
        let host = self.state.connect.device_ip.trim().to_owned();
        if host.is_empty() {
            self.set_connect_status("请输入设备 IP", theme::WARN);
            return;
        }
        self.set_connect_status(&format!("正在探测 {host}:9073 …"), theme::MUTED);
        self.spawn_task(move || {
            let result = x5::probe(&host, 9073);
            match result {
                Ok(summary) => WorkerResult::Probe {
                    text: format!("驱动已就绪：{summary:?}"),
                    ok: true,
                    host,
                },
                Err(error) => WorkerResult::Probe {
                    text: format!("探测失败：{error}"),
                    ok: false,
                    host,
                },
            }
        });
    }

    pub fn bootstrap(&mut self) {
        let host = self.state.connect.device_ip.trim().to_owned();
        let user = self.state.connect.ssh_user.trim().to_owned();
        let password = self.state.connect.ssh_password.clone();
        if host.is_empty() {
            self.set_connect_status("请输入设备 IP", theme::WARN);
            return;
        }
        self.set_connect_status(
            &format!("正在通过 SSH 启动 {host} 上的 DEMO233 …"),
            theme::MUTED,
        );
        self.spawn_task(move || {
            let result = x5::bootstrap_driver(&host, 22, &user, &password, 9073);
            match result {
                Ok(summary) => WorkerResult::Bootstrap {
                    text: format!("驱动已就绪：{summary:?}"),
                    ok: true,
                    host,
                },
                Err(error) => WorkerResult::Bootstrap {
                    text: format!("启动失败：{error}"),
                    ok: false,
                    host,
                },
            }
        });
    }

    pub fn solve(&mut self) -> bool {
        let Some(streams) = self.streams.as_ref() else {
            self.set_connect_status("请先完成双路采集", theme::WARN);
            return false;
        };
        let board = self.current_board();
        if board.validate().is_err() {
            self.set_connect_status("棋盘参数非法", theme::ERR);
            return false;
        }
        self.solve_after_preview_ready = false;
        let ch0_detections = streams.ch0.captured_detections();
        let ch3_detections = streams.ch3.captured_detections();
        self.state.solve.ch0_result = "CH0：求解中…".to_owned();
        self.state.solve.ch3_result = "CH3：求解中…".to_owned();
        self.state.solve.ch0_rmse.clear();
        self.state.solve.ch3_rmse.clear();
        self.state.solve.ch0_observability = None;
        self.state.solve.ch3_observability = None;
        self.spawn_task(move || {
            let r0 = solve_channel_from_detections(0, &ch0_detections, board, None);
            let r1 = solve_channel_from_detections(3, &ch3_detections, board, None);
            let solutions = match (&r0, &r1) {
                (Ok(a), Ok(b)) => Some((a.solution.clone(), b.solution.clone())),
                _ => None,
            };
            let observability = match (&r0, &r1) {
                (Ok(a), Ok(b)) => Some((a.observability.clone(), b.observability.clone())),
                _ => None,
            };
            let text = match (r0, r1) {
                (Ok(a), Ok(b)) => format!("{}\n{}", a.summary(), b.summary()),
                (Ok(a), Err(e)) => format!("{}\nCH3 失败：{e}", a.summary()),
                (Err(e), Ok(b)) => format!("CH0 失败：{e}\n{}", b.summary()),
                (Err(e0), Err(e1)) => format!("CH0 失败：{e0}\nCH3 失败：{e1}"),
            };
            WorkerResult::Solve {
                text,
                solutions,
                observability,
            }
        });
        true
    }
    pub fn skip_preview(&mut self) {
        self.preview_finished = true;
        self.complete_step(StepId::Preview, "Step 2 已跳过 · 直接进入 Step 3");
        self.solve_after_preview_ready = true;
        self.try_auto_solve_after_preview();
    }

    fn try_auto_solve_after_preview(&mut self) {
        if !self.solve_after_preview_ready || self.is_busy() {
            return;
        }
        if !self
            .streams
            .as_ref()
            .is_some_and(StreamState::both_complete)
        {
            return;
        }
        if self.current_board().validate().is_err() {
            return;
        }
        let started = self.solve();
        debug_assert!(
            started,
            "auto solve preconditions should guarantee solve start"
        );
    }

    pub fn inspect_eeprom(&mut self) {
        let Some(helper) = eeprom::locate_helper() else {
            self.set_eeprom_status(
                "未找到 camera-i2c-helper（先 cargo build -p camera-i2c-helper --release）",
                false,
            );
            return;
        };
        let (host, user, password) = self.connect_credentials();
        if host.trim().is_empty() {
            self.set_eeprom_status("请先在 Step 1 填写设备 IP", false);
            return;
        }
        self.set_eeprom_status("正在读取 CH0/CH3 EEPROM 状态…", false);
        self.spawn_task(move || {
            let helper: Arc<[u8]> = helper.into();
            let ch0 = eeprom::inspect(&host, &user, &password, 4, Arc::clone(&helper));
            let ch3 = eeprom::inspect(&host, &user, &password, 6, helper);
            match (ch0, ch3) {
                (Ok(EepromHelperResult::Inspect(a)), Ok(EepromHelperResult::Inspect(b))) => {
                    let text = format!(
                        "EEPROM 状态：\n{}\n{}",
                        inspect_summary("CH0/i2c-4", &a),
                        inspect_summary("CH3/i2c-6", &b)
                    );
                    WorkerResult::Inspect {
                        text,
                        inspect: Some((a, b)),
                    }
                }
                (a, b) => WorkerResult::Inspect {
                    text: format!(
                        "读取 EEPROM 失败：CH0={}；CH3={}",
                        inspect_result_label(&a),
                        inspect_result_label(&b)
                    ),
                    inspect: None,
                },
            }
        });
    }

    pub fn write_eeprom(&mut self) {
        self.refresh_snid_previews();
        let Some(serial_pair) = self.serial_pair() else {
            self.set_eeprom_status("两路 SNID 尚未完成", false);
            return;
        };
        let (serial0, serial3) = match serial_pair {
            Ok(pair) => pair,
            Err(error) => {
                self.set_eeprom_status(&error, false);
                return;
            }
        };
        if !self.state.eeprom.write_armed {
            self.state.eeprom.write_armed = true;
            let mut note = format!("请确认 SNID：CH0={serial0}，CH3={serial3}；再次点击开始写入。");
            if let Some((inspect0, inspect3)) = &self.eeprom_inspect {
                let dev0 = serial_state_label(&inspect0.state.serial);
                let dev3 = serial_state_label(&inspect3.state.serial);
                if dev0 != serial0 || dev3 != serial3 {
                    note.push_str(&format!(
                        "\n设备当前 SN：CH0={dev0}，CH3={dev3}；写入将覆盖为输入值。"
                    ));
                }
            }
            self.set_eeprom_status(&note, true);
            return;
        }
        self.state.eeprom.write_armed = false;
        let Some(helper) = eeprom::locate_helper() else {
            self.set_eeprom_status("未找到 camera-i2c-helper", false);
            return;
        };
        let Some((solution0, solution3)) = self.solutions.clone() else {
            self.set_eeprom_status("请先完成标定求解（Step 3）", false);
            return;
        };
        let Some((inspect0, inspect3)) = self.eeprom_inspect.clone() else {
            self.set_eeprom_status("请先读取 EEPROM 状态", false);
            return;
        };
        let before0 = inspect0.state.image_sha256.clone();
        let before3 = inspect3.state.image_sha256.clone();
        let (host, user, password) = self.connect_credentials();
        self.set_eeprom_status("正在写入 CH0/CH3 EEPROM…", false);
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
                        (Ok(path0), Ok(path3)) => WorkerResult::Write {
                            text: format!(
                                "写入 EEPROM 成功：\n待写入标定：\n{}\n{}\n写入状态：\n{}\n{}\nwrite_history：\nCH0 {path0}\nCH3 {path3}",
                                solution_detail_summary("CH0", &solution0),
                                solution_detail_summary("CH3", &solution3),
                                write_summary("CH0/i2c-4", &a),
                                write_summary("CH3/i2c-6", &b)
                            ),
                            ok: true,
                            write_results: Some((
                                write_detail("CH0/i2c-4", &a),
                                write_detail("CH3/i2c-6", &b),
                            )),
                            history_paths: Some((path0, path3)),
                            // 用写入内容构造结构化状态：与“读取 EEPROM 状态”同款表格。
                            inspect: Some((
                                inspect_detail_from_write(
                                    "CH0/i2c-4",
                                    &serial0,
                                    &a.after.image_sha256,
                                    &solution0,
                                ),
                                inspect_detail_from_write(
                                    "CH3/i2c-6",
                                    &serial3,
                                    &b.after.image_sha256,
                                    &solution3,
                                ),
                            )),
                        },
                        (a_history, b_history) => WorkerResult::Write {
                            text: format!(
                                "写入 EEPROM 成功，但保存 write_history 失败：CH0={}；CH3={}",
                                history_result_label(&a_history),
                                history_result_label(&b_history)
                            ),
                            ok: false,
                            write_results: Some((
                                write_detail("CH0/i2c-4", &a),
                                write_detail("CH3/i2c-6", &b),
                            )),
                            history_paths: None,
                            inspect: None,
                        },
                    }
                }
                (a, b) => WorkerResult::Write {
                    text: format!(
                        "写入 EEPROM 失败：CH0={}；CH3={}",
                        provision_result_label(&a),
                        provision_result_label(&b)
                    ),
                    ok: false,
                    write_results: None,
                    history_paths: None,
                    inspect: None,
                },
            }
        });
    }

    pub fn reset_flow(&mut self) {
        self.streams = None;
        self.pending_task = None;
        self.solutions = None;
        self.eeprom_inspect = None;
        self.preview_finished = false;
        self.solve_after_preview_ready = false;
        self.state.completed = [false; 3];
        self.state.active = StepId::Connect;
        self.state.summaries = [
            "未开始".to_owned(),
            "等待连接".to_owned(),
            "等待采集".to_owned(),
        ];
        self.state.preview = PreviewState::default();
        self.state.solve = SolveState::default();
        self.state.eeprom.status_ok = true;
        self.state.eeprom.status =
            "已 Reset：保留设备 IP/SSH 与 SNID 输入；已清空 dataset 与标定结果。".to_owned();
        self.state.eeprom.inspect_enabled = false;
        self.state.eeprom.write_enabled = false;
        self.state.eeprom.write_armed = false;
        if let Ok(mut slot) = self.ch0_overlay_slot.lock() {
            *slot = None;
        }
        if let Ok(mut slot) = self.ch3_overlay_slot.lock() {
            *slot = None;
        }
        self.start_connection_probe();
    }

    fn current_board(&self) -> BoardSpec {
        BoardSpec {
            inner_cols: self.state.solve.board_cols.clamp(2, 256) as u16,
            inner_rows: self.state.solve.board_rows.clamp(2, 256) as u16,
            square_size: f64::from(self.state.solve.square_mm.max(0.1)),
        }
    }

    fn connect_credentials(&self) -> (String, String, String) {
        (
            self.state.connect.device_ip.trim().to_owned(),
            self.state.connect.ssh_user.trim().to_owned(),
            self.state.connect.ssh_password.clone(),
        )
    }

    fn serial_pair(&mut self) -> Option<Result<(String, String), String>> {
        let ch0 = match self.state.eeprom.ch0_snid.refresh_preview() {
            Ok(serial) => serial,
            Err(error) => return Some(Err(format!("CH0 SNID：{error}"))),
        };
        let ch3 = match self.state.eeprom.ch3_snid.refresh_preview() {
            Ok(serial) => serial,
            Err(error) => return Some(Err(format!("CH3 SNID：{error}"))),
        };
        if ch0 == ch3 {
            return Some(Err(
                "CH0 与 CH3 SNID 不能相同；两颗 EEPROM 需要不同序列号".to_owned()
            ));
        }
        Some(Ok((ch0, ch3)))
    }

    fn start_previews(&mut self, host: &str) {
        if self.streams.is_some() {
            return;
        }
        let host = host.trim();
        if host.is_empty() {
            return;
        }
        let mut streams = StreamState::start(
            host,
            Arc::clone(&self.ch0_overlay_slot),
            Arc::clone(&self.ch3_overlay_slot),
        );
        let board = self.current_board();
        streams.ch0.start_detect(board);
        streams.ch3.start_detect(board);
        streams.ch0.start_capture();
        streams.ch3.start_capture();
        self.state.preview.ch0.status = format!("rtsp://{host}:554/PRR");
        self.state.preview.ch3.status = format!("rtsp://{host}:557/PRR");
        self.state.preview.ch0.overlay_text = "guide auto_capture 启动中…".to_owned();
        self.state.preview.ch3.overlay_text = "guide auto_capture 启动中…".to_owned();
        self.streams = Some(streams);
    }

    fn refresh_preview_guides(&mut self) {
        let Some(streams) = self.streams.as_ref() else {
            return;
        };
        let (text0, _count0, hold0, quality0) = streams.ch0.guide();
        if !text0.is_empty() {
            self.state.preview.ch0.overlay_text = text0;
            self.state.preview.ch0.overlay_color = overlay_color(&quality0, hold0);
        }
        self.state.preview.ch0.quality = quality0;
        let (text3, _count3, hold3, quality3) = streams.ch3.guide();
        if !text3.is_empty() {
            self.state.preview.ch3.overlay_text = text3;
            self.state.preview.ch3.overlay_color = overlay_color(&quality3, hold3);
        }
        self.state.preview.ch3.quality = quality3;
    }

    fn finish_pending_task(&mut self) {
        let result = self
            .pending_task
            .as_ref()
            .and_then(|slot| slot.lock().ok())
            .and_then(|mut value| value.take());
        let Some(result) = result else {
            return;
        };
        self.pending_task = None;
        match result {
            WorkerResult::Probe { text, ok, host } | WorkerResult::Bootstrap { text, ok, host } => {
                self.set_connect_status(&text, if ok { theme::OK } else { theme::ERR });
                if ok {
                    self.complete_step(StepId::Connect, "驱动已就绪 · 可连接预览");
                    self.start_previews(&host);
                }
            }
            WorkerResult::Solve {
                text,
                solutions,
                observability,
            } => {
                tracing::info!("求解结果：{text}");
                match solutions {
                    Some((solution0, solution3)) => {
                        self.state.solve.ch0_detail = Some(
                            crate::solve::SolutionDetail::from_solution("CH0", &solution0),
                        );
                        self.state.solve.ch3_detail = Some(
                            crate::solve::SolutionDetail::from_solution("CH3", &solution3),
                        );
                        self.state.solve.ch0_result =
                            self.state.solve.ch0_detail.as_ref().map_or_else(
                                String::new,
                                crate::solve::SolutionDetail::summary_text,
                            );
                        self.state.solve.ch3_result =
                            self.state.solve.ch3_detail.as_ref().map_or_else(
                                String::new,
                                crate::solve::SolutionDetail::summary_text,
                            );
                        self.state.solve.ch0_rmse = view_rmse_values(&solution0);
                        self.state.solve.ch3_rmse = view_rmse_values(&solution3);
                        self.state.solve.ch0_limit = solution0.rms_error.max(0.5);
                        self.state.solve.ch3_limit = solution3.rms_error.max(0.5);
                        if let Some((obs0, obs3)) = observability {
                            self.state.solve.ch0_observability = obs0;
                            self.state.solve.ch3_observability = obs3;
                        }
                        self.solutions = Some((solution0, solution3));
                        self.complete_step(
                            StepId::Solve,
                            "两路标定完成 · 等待双路 SNID 与 EEPROM 写入",
                        );
                        self.state.eeprom.inspect_enabled = true;
                        self.state.eeprom.write_enabled = false;
                        self.inspect_eeprom();
                    }
                    None => {
                        self.state.solve.ch0_result = text;
                        self.state.solve.ch3_result = "CH3：见上方错误详情".to_owned();
                        self.state.solve.ch0_detail = None;
                        self.state.solve.ch3_detail = None;
                        self.state.solve.ch0_rmse.clear();
                        self.state.solve.ch3_rmse.clear();
                        self.state.solve.ch0_observability = None;
                        self.state.solve.ch3_observability = None;
                    }
                }
            }
            WorkerResult::Inspect { text, inspect } => {
                let ok = inspect.is_some();
                if let Some(pair) = inspect {
                    self.eeprom_inspect = Some(pair);
                    self.state.eeprom.inspect = Some((
                        inspect_detail("CH0/i2c-4", &self.eeprom_inspect.as_ref().unwrap().0),
                        inspect_detail("CH3/i2c-6", &self.eeprom_inspect.as_ref().unwrap().1),
                    ));
                    self.refresh_snid_previews();
                }
                self.set_eeprom_status(&text, ok);
                self.state.eeprom.write_enabled = ok;
            }
            WorkerResult::Write {
                text,
                ok,
                write_results,
                history_paths,
                inspect,
            } => {
                tracing::info!("{text}");
                if write_results.is_some() {
                    self.state.eeprom.last_write = write_results;
                    self.state.eeprom.write_history_paths = history_paths;
                }
                // 写入成功后用写入内容刷新结构化状态（与读取 EEPROM 同款表格）。
                if let Some(pair) = inspect {
                    self.state.eeprom.inspect = Some(pair);
                }
                self.set_eeprom_status(&text, ok);
                if ok {
                    self.complete_step(StepId::Solve, "EEPROM 写入完成");
                }
            }
        }
    }

    fn spawn_task(&mut self, task: impl FnOnce() -> WorkerResult + Send + 'static) {
        if self.pending_task.is_some() {
            self.state.status_bar = "已有后台任务执行中，请等待完成".to_owned();
            return;
        }
        let slot = Arc::new(Mutex::new(None));
        self.pending_task = Some(Arc::clone(&slot));
        std::thread::spawn(move || {
            let result = task();
            if let Ok(mut value) = slot.lock() {
                *value = Some(result);
            }
        });
    }

    fn poll_connection_probe(&mut self) {
        let probe_text = self
            .pending_connection_probe
            .as_ref()
            .and_then(|slot| slot.lock().ok())
            .and_then(|mut value| value.take());
        if let Some(text) = probe_text {
            self.pending_connection_probe = None;
            if text.starts_with("连接正常") {
                self.set_connect_status(&text, theme::OK);
                // 自动连接成功与手动探测一致：进入第二步并启动双路预览。
                if self.streams.is_none() && !self.state.completed[StepId::Connect.index()] {
                    self.complete_step(StepId::Connect, "驱动已就绪 · 可连接预览");
                    let host = self.state.connect.device_ip.clone();
                    self.start_previews(&host);
                }
            } else {
                self.set_connect_status(&text, theme::WARN);
            }
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
        let host = self.state.connect.device_ip.trim().to_owned();
        if host.is_empty() {
            return;
        }
        self.last_connection_probe = Some(Instant::now());
        let slot = Arc::new(Mutex::new(None));
        self.pending_connection_probe = Some(Arc::clone(&slot));
        std::thread::spawn(move || {
            let text = match x5::probe(&host, 9073) {
                Ok(summary) => format!("连接正常：TCP 9073 可用 · {summary:?}"),
                Err(error) => format!("持续检查：设备/驱动未就绪（{error}）"),
            };
            if let Ok(mut value) = slot.lock() {
                *value = Some(text);
            }
        });
    }

    fn complete_step(&mut self, step: StepId, summary: &str) {
        self.state.completed[step.index()] = true;
        self.state.summaries[step.index()] = summary.to_owned();
        self.state.active = match step {
            StepId::Connect => StepId::Preview,
            StepId::Preview | StepId::Solve => StepId::Solve,
        };
        self.state.status_bar = summary.to_owned();
    }

    fn set_connect_status(&mut self, text: &str, color: [f32; 4]) {
        self.state.connect.status = text.to_owned();
        self.state.connect.status_color = color;
        self.state.status_bar = text.to_owned();
    }

    fn set_eeprom_status(&mut self, text: &str, ok: bool) {
        self.state.eeprom.status = text.to_owned();
        self.state.eeprom.status_ok = ok;
        self.state.status_bar = text.lines().next().unwrap_or(text).to_owned();
    }
}

fn parse_two_digit_year(text: &str) -> Result<u16, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("年份必填".to_owned());
    }
    if trimmed.len() != 2 || !trimmed.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("年份必须是两位数字，例如 26".to_owned());
    }
    trimmed
        .parse::<u16>()
        .map_err(|_| "年份必须是两位数字，例如 26".to_owned())
}

fn parse_decimal_field<T>(label: &str, text: &str) -> Result<T, String>
where
    T: std::str::FromStr,
{
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(format!("{label}必填"));
    }
    trimmed
        .parse::<T>()
        .map_err(|_| format!("{label}必须是十进制数字"))
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

/// 解析 EEPROM 镜像中的已存标定，返回结构化指标。
fn parse_eeprom_backup(backup: &[u8]) -> Result<BackupCalibrationDetail, String> {
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
    let half_w = f64::from(width) * 0.5;
    let half_h = f64::from(height) * 0.5;
    Ok(BackupCalibrationDetail {
        width,
        height,
        fx,
        fy,
        cx,
        cy,
        hfov_degrees: 2.0 * (half_w / fx).atan().to_degrees(),
        vfov_degrees: 2.0 * (half_h / fy).atan().to_degrees(),
        optical_x_degrees: ((cx - half_w) / fx).atan().to_degrees(),
        optical_y_degrees: ((cy - half_h) / fy).atan().to_degrees(),
        distortion: distortion_values,
    })
}

fn eeprom_backup_calibration_summary(backup: &[u8]) -> Result<String, String> {
    let detail = parse_eeprom_backup(backup)?;
    Ok(format!(
        "当前 EEPROM 标定：{}\n畸变：{}",
        format_intrinsics_geometry(
            detail.width,
            detail.height,
            detail.fx,
            detail.fy,
            detail.cx,
            detail.cy
        ),
        distortion_summary(&detail.distortion)
    ))
}

/// EEPROM 读取结果的结构化展示。
fn inspect_detail(label: &str, inspect: &EepromInspectResult) -> EepromInspectDetail {
    let calibration = parse_eeprom_backup(&inspect.backup);
    EepromInspectDetail {
        label: label.to_owned(),
        sha8: short_sha8(&inspect.state.image_sha256),
        flag_valid: inspect.state.flag_valid,
        serial: serial_state_label(&inspect.state.serial),
        calibration: calibration.clone().ok(),
        calibration_error: calibration
            .err()
            .map(|error| format!("EEPROM 标定解析失败：{error}")),
    }
}

/// 写入成功后用写入内容构造 EEPROM 状态：与“读取 EEPROM 状态”同款结构化表格。
fn inspect_detail_from_write(
    label: &str,
    serial: &str,
    after_sha8: &str,
    solution: &CalibrationSolution,
) -> EepromInspectDetail {
    let detail = crate::solve::SolutionDetail::from_solution(label, solution);
    EepromInspectDetail {
        label: label.to_owned(),
        sha8: short_sha8(after_sha8),
        flag_valid: true,
        serial: serial.to_owned(),
        calibration: Some(BackupCalibrationDetail {
            width: solution.image_size.width,
            height: solution.image_size.height,
            fx: detail.fx,
            fy: detail.fy,
            cx: detail.cx,
            cy: detail.cy,
            hfov_degrees: detail.hfov_degrees,
            vfov_degrees: detail.vfov_degrees,
            optical_x_degrees: detail.optical_x_degrees,
            optical_y_degrees: detail.optical_y_degrees,
            distortion: detail.distortion.clone(),
        }),
        calibration_error: None,
    }
}

fn storage_field_range(
    map: &camera_toolbox_core::calibration_eeprom::CalibrationStorageMap,
    name: &str,
) -> Result<Range<usize>, String> {
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

/// EEPROM 写入结果文本摘要（状态栏/日志用）。
fn write_summary(label: &str, result: &EepromWriteResult) -> String {
    format!(
        "{label} before={} SN={} → after={} SN={} verified={}",
        short_sha8(&result.before.image_sha256),
        serial_state_label(&result.before.serial),
        short_sha8(&result.after.image_sha256),
        serial_state_label(&result.after.serial),
        result.bytewise_verified
    )
}

/// EEPROM 写入结果的结构化展示（before → after）。
fn write_detail(label: &str, result: &EepromWriteResult) -> EepromWriteDetail {
    EepromWriteDetail {
        label: label.to_owned(),
        before_sha8: short_sha8(&result.before.image_sha256),
        before_serial: serial_state_label(&result.before.serial),
        after_sha8: short_sha8(&result.after.image_sha256),
        after_serial: serial_state_label(&result.after.serial),
        verified: result.bytewise_verified,
    }
}

fn short_sha8(sha256: &str) -> String {
    sha256[..8.min(sha256.len())].to_owned()
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

fn overlay_color(quality: &preview::DatasetQuality, hold: u8) -> [f32; 4] {
    if quality.is_complete() || hold >= preview::HOLD_TARGET {
        theme::OK
    } else if hold > 0 {
        theme::WARN
    } else {
        theme::ACCENT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_preview_advances_to_solve() {
        let mut controller = CalibController::new();
        assert_eq!(controller.state.active, StepId::Connect);
        assert!(!controller.state.completed[StepId::Preview.index()]);

        controller.skip_preview();

        assert!(controller.state.completed[StepId::Preview.index()]);
        assert_eq!(controller.state.active, StepId::Solve);
        assert!(controller.solve_after_preview_ready);
        assert_eq!(
            controller.state.summaries[StepId::Preview.index()],
            "Step 2 已跳过 · 直接进入 Step 3"
        );
    }
}
