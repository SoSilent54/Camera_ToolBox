# Camera Toolbox

Rust-only ISP 标定工具箱。当前路线已锁定为：

- 主前端：`egui/eframe` GUI，用于图像查看、ROI、曲线和人工标定交互。
- 副前端：`ratatui/crossterm` TUI，用于 SSH/远程运维、日志、批处理和无桌面环境降级。
- 自动化入口：CLI，用于 P0 只读闭环、批处理和后续 CI/回放。
- 核心：UI 无关 Rust workspace，采集、RAW、分析、journal、寄存器访问都通过清晰端口隔离。

## 当前阶段

本仓库先落基础工程骨架和本地 RAW 最小闭环，不实现 sensor 取图、SSH/SFTP、寄存器读写等设备侧真实副作用。
```text
Camera Toolbox
├── crates/
│   ├── core/           # 领域模型、RAW 描述、ROI 统计、journal 基础类型
│   ├── app/            # 命令/事件/工作流编排边界
│   ├── adapters/       # 外部进程、SSH、文件、寄存器等适配端口
│   └── frontends/
│       ├── cli/        # headless 自动化入口
│       ├── tui/        # 运维/日志/批处理副界面
│       └── gui/        # egui 主界面
├── docs/
│   ├── architecture.md              # 架构边界与调用流
│   ├── roadmap.md                   # P0 起步路线与验收
│   └── rust-for-cpp-sensor-design.md # C++ 工程师视角的 Rust trait/profile 设计
└── Cargo.toml                       # workspace 统一依赖版本
```

## 快速验证

```bash
cargo fmt --all -- --check
cargo check --workspace
cargo test --workspace
```

## CI 与发布

- 每个分支 push 会在 Ubuntu 22 上执行格式检查、全 workspace target 编译、测试和 Clippy。当前 Clippy 仅报告既有 warning，不以 `-D warnings` 使 CI 失败。
- 推送任意 Git tag 会创建或更新同名 GitHub Release，并发布以下归档：
  - `camera-toolbox-macos-aarch64.tar.gz`
  - `camera-toolbox-windows-x86_64.zip`
  - `camera-toolbox-linux-x86_64-ubuntu20.tar.gz`
  - `camera-toolbox-linux-x86_64-ubuntu22.tar.gz`
  - `camera-toolbox-linux-aarch64-ubuntu20.tar.gz`
  - `camera-toolbox-linux-aarch64-ubuntu22.tar.gz`
- Ubuntu 20/22 Linux 归档分别在官方 `ubuntu:20.04` / `ubuntu:22.04` 容器中构建；x86_64 与 aarch64 各自使用匹配架构的 GitHub-hosted Linux runner，不依赖已不在当前 runner 标签列表中的 `ubuntu-20.04` hosted runner。


本地 RAW smoke：

```bash
cargo run -p camera-toolbox-cli -- analyze-raw \
  --raw <frame.raw> --width <w> --height <h> --bit-depth <n> \
  --encoding u16le --roi 0,0,<w>,<h>
```

GUI 本地 RAW 预览：

```bash
cargo run -p camera-toolbox-gui
```

在菜单中选择 `File -> Open Raw...`，再在设置窗口填写 width、height、bit depth、stride 和 Bayer。当前只支持 unpacked `u16` little-endian。

当前只支持已解包 `u16le` RAW。RAW10/12 packed、debayer 和复杂 manifest 后续再加。

本地 RAW 路径也走 `app::Workflow::load_raw_and_analyze` 与 `RawFrameLoader` port；CLI/GUI 不直接解码或统计 RAW。

## 设计原则

- GUI/TUI/CLI 不直接执行 SSH、采集程序或 `i2ctransfer`。
- 所有外部副作用通过 adapter 端口进入 app workflow。
- RAW 定量分析基于原始 buffer，不基于 tone-mapped preview。
- artifact、配置、分析结果和设备回执后续都要进入可审计 journal。
- `egui`、`eframe`、`egui_plot` 在 workspace 统一版本族，避免前端集成时出现两套 `egui` 类型。
