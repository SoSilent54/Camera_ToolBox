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
cargo check --workspace --all-targets --release --locked
cargo test --workspace --release --locked
```

项目将 `profile.dev` 配置为 release 等价代码生成，因此普通 `cargo build` / `cargo run` 也会启用优化并关闭 debug assertions；Cargo 仍会把这类普通命令的产物放在 `target/debug`。下列运行示例显式使用 `--release`，正式产物位于 `target/release`。

## CI 与发布

- 每个分支 push 会在 Ubuntu 22 上执行格式检查、全 workspace target 编译、测试和 Clippy。当前 Clippy 仅报告既有 warning，不以 `-D warnings` 使 CI 失败。
- 也可在 GitHub `Actions -> CI -> Run workflow` 中手动执行同一套 CI 检查。
- 推送任意 Git tag 会创建或更新同名 GitHub Release，并发布以下归档：
  - `camera-toolbox-macos-aarch64.tar.gz`
  - `camera-toolbox-windows-x86_64.zip`
  - `camera-toolbox-linux-x86_64-ubuntu20.tar.gz`
  - `camera-toolbox-linux-x86_64-ubuntu22.tar.gz`
  - `camera-toolbox-linux-aarch64-ubuntu20.tar.gz`
  - `camera-toolbox-linux-aarch64-ubuntu22.tar.gz`
- 也可在 GitHub `Actions -> Release -> Run workflow` 中选择待构建的分支或 ref，并填写必填 `tag`；Release 会使用该 tag，并以本次运行的提交 SHA 为目标。
- Ubuntu 20/22 Linux 归档分别在官方 `ubuntu:20.04` / `ubuntu:22.04` 容器中构建；x86_64 与 aarch64 各自使用匹配架构的 GitHub-hosted Linux runner，不依赖已不在当前 runner 标签列表中的 `ubuntu-20.04` hosted runner。


本地 RAW smoke：

```bash
cargo run --release -p camera-toolbox-cli -- analyze-raw \
  --raw <frame.raw> --width <w> --height <h> --bit-depth <n> \
  --encoding u16le --roi 0,0,<w>,<h>
```

GUI 本地 RAW 预览：

```bash
cargo run --release -p camera-toolbox-gui
```

在菜单中选择 `File -> Open Raw...`，可手工填写或通过 `Select` 选择文件路径。软件会基于文件名、文件长度和有限像素样本生成 Preset，并自动应用评分最高的可加载候选；切换其他 Preset 会立即回填参数，手工修改 width、height、有效 bit depth、uint16 容器或端序后显示为 `Custom`。候选不是可靠识别，Bayer 仍须人工确认。

加载成功后默认显示 `Color`，右侧 `Color Processing` 面板默认展开，可从标题栏 chevron 收起为窄 rail 并随时重新展开。面板可实时调整 Bayer、R/Gr/Gb/B black level、通道 gain 与 Gamma；Gamma 默认开启并取 2.2，GUI 可在 0.1–5.0 范围调整，关闭时线性 RGB 直接量化。关闭后重新开启会恢复上次设置。默认链接四通道 black 和 Gr/Gb gain。显示链路固定为 black subtraction → `(max_code-black)` 归一化 → CFA gain → bilinear demosaic → clamp → 可选 Gamma → RGB8。`View` 菜单可切换 `Raw Mono`、`Color`。
`Tools -> Hover View` 默认开启，可关闭；开启后可选择 3×3、5×5（默认）或 7×7 RAW 邻域。鼠标进入图像即显示固定大小、跟随指针的非交互检查窗：邻域始终读取 RAW preview（Color 主视图下也不读取插值彩色纹理），中心格带十字与边框，图像边缘越界格留空。信息区显示坐标、CFA 通道、RAW 值、最后一次已安装彩色参数得到的 RGBf/RGB8、颜色色块和 ROI 统计；超 bit-depth 样本保留洋红诊断色。底部状态栏只保留文件规格、显示模式、缩放和异常摘要。

当前只支持紧密排列、已解包的 `u16le` Bayer RAW。彩色预览不包含自动 black level/AWB、CCM、LSC、降噪或 edge-aware demosaic，因此属于 sensor RGB 查验，不代表标定后的准确 sRGB。带行 padding、RAW10/12 packed 和复杂 manifest 后续再加。

本地 RAW 路径也走 `app::Workflow::load_raw_and_analyze` 与 `RawFrameLoader` port；CLI/GUI 不直接解码或统计 RAW。

## 设计原则

- GUI/TUI/CLI 不直接执行 SSH、采集程序或 `i2ctransfer`。
- 所有外部副作用通过 adapter 端口进入 app workflow。
- RAW 定量分析基于原始 buffer，不基于 tone-mapped preview。
- artifact、配置、分析结果和设备回执后续都要进入可审计 journal。
- `egui`、`eframe`、`egui_plot` 在 workspace 统一版本族，避免前端集成时出现两套 `egui` 类型。
