//! pongbot-calib-tool：Godot 桌面端 X5_233 标定工具（gdext 入口）。
//!
//! 全部 UI 用 Rust 代码构建（不依赖 Godot 编辑器可视化搭建）；
//! 运行：`godot --path crates/frontends/godot/godot`。

mod ui;

use godot::classes::{Control, IControl};
use godot::prelude::*;

use camera_toolbox_adapters::x5_tcp_client;
use std::sync::{Arc, Mutex};
use ui::steps::StepId;
use ui::{theme, UiState};

/// 应用根节点：挂载 5 步向导 UI 与领域层控制器。
#[derive(GodotClass)]
#[class(init, base = Control)]
pub struct CalibApp {
    base: Base<Control>,
    ui: Option<UiState>,
    /// 后台 probe 结果槽：worker 线程写入，主线程 `_process` 轮询取走。
    pending_probe: Option<Arc<Mutex<Option<String>>>>,
}

#[godot_api]
impl IControl for CalibApp {
    fn ready(&mut self) {
        theme::install_cjk_font();
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
                .connect(button_callback(self.base.__constructed_gd().cast::<CalibApp>(), "on_bootstrap"));
        }
        godot_print!("pongbot-calib-tool: wizard UI ready");
    }

    /// 主线程帧回调：轮询后台 probe 结果。
    fn process(&mut self, _delta: f64) {
        let text = self
            .pending_probe
            .as_ref()
            .and_then(|slot| slot.lock().ok())
            .and_then(|mut value| value.take());
        if let Some(text) = text {
            self.pending_probe = None;
            self.finish_probe(text);
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
        let slot = Arc::new(Mutex::new(None));
        self.pending_probe = Some(Arc::clone(&slot));
        std::thread::spawn(move || {
            let result = x5_tcp_client::probe(&ip, 9073);
            let text = match result {
                Ok(summary) => format!("驱动已就绪：{summary:?}"),
                Err(error) => format!("探测失败：{error}"),
            };
            if let Ok(mut value) = slot.lock() {
                *value = Some(text);
            }
        });
    }

    /// 主线程处理 probe 结果（由 `_process` 轮询触发）。
    fn finish_probe(&mut self, text: String) {
        let ok = text.starts_with("驱动已就绪");
        let color = if ok { theme::OK } else { theme::ERR };
        self.connect_status(&text, color);
        if ok {
            if let Some(state) = self.ui.as_mut() {
                state.complete_step(StepId::Connect, "驱动已就绪 · 可连接预览");
            }
        }
    }

    /// 启动驱动：SSH 启动板端 DEMO233（下一里程碑接入真实 SSH 链路）。
    #[func]
    fn on_bootstrap(&mut self) {
        self.connect_status("启动驱动：SSH 链路将在下一里程碑接入", theme::WARN);
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
