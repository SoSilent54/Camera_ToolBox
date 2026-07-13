# 路线图

## 选型锁定

- 语言：Rust only。
- 主 GUI：`egui/eframe`。
- 运维副界面：`ratatui/crossterm`。
- 自动化入口：CLI。
- 依赖管理：workspace `[workspace.dependencies]` 统一锁定 GUI 版本族，避免多套 `egui` 类型。

## P0：只读闭环

目标：先把本地 RAW 加载、解码、分析和 GUI 灰度/基础彩色显示串通；sensor 取图、SSH/SFTP、寄存器读写、自动曝光暂不做。

验收：

- CLI 能打开本地紧密排列的已解包 `u16le` RAW，并按显式 width/height/bit depth 分析 ROI。
- RAW spec 的字节数或 bit depth 非法时拒绝分析；像素超过当前 bit depth 时保留原值，GUI 通过 Mono 洋红、Color clamp 与 diagnostics 明确告警。
- GUI 的数据质量 Warning 支持手动关闭和 8s 自动消失，但 `RawDiagnostics`、状态栏摘要、MAGENTA 与 Hover View 诊断持续有效；GUI/CLI/TUI 对动作结果输出统一 DEBUG/WARN/ERROR JSONL 审计日志，CLI stdout 保持业务结果。
- 同一 RAW 的统计结果可重复。
- GUI 能用参数打开本地 RAW，切换 Raw Mono/Color，显示 ROI；`Tools -> Hover View` 提供即时、固定大小、可选 3×3/5×5/7×7 的 RAW 邻域检查，显示原始值、CFA、已安装彩色纹理对应的 RGB 和 ROI 统计。
- 彩色预览支持四种 Bayer、R/Gr/Gb/B black/gain、bilinear demosaic，以及默认 2.2、可在 GUI 调节并可关闭的 Gamma 显示；自动 BLC/AWB、CCM、LSC、edge-aware demosaic 显式 deferred。

## P1：受控手动操作

目标：在软件内支持人工触发的单步操作，包括重新采集、加载历史 artifact、调整 ROI、读取诊断寄存器、规划曝光寄存器写入；曝光参数仍由人决定，默认不自动闭环。

验收：

- CLI/TUI/GUI 的手动动作都映射到同一套 app command/event/state，不绕过 workflow 直接调用 adapter。
- sensor profile 声明允许寄存器、位宽、范围、group hold 策略。
- 越界写入被拒绝，写入默认 dry-run，真实 apply 需要显式确认。
- 写后 readback 和 journal 完整记录。
- 设备状态不确定时禁止继续写入或进入自动闭环。

## P2：半自动曝光闭环

目标：按 ROI/全图指标迭代曝光或增益，直到满足条件或进入明确终态。

验收：

- 支持 deadband、最大迭代、最大步长、饱和率约束。
- 能区分 `Converged`、`LimitReached`、`Oscillating`、`Timeout`、`CaptureFailed`。
- 每轮都有 capture id、参数、readback、ROI 统计和决策原因。

## P3：标定流程扩展

目标：扩展到 BLC、LSC、AWB、CCM、Noise Profile 等 ISP 标定任务。

验收：

- 多 sensor mode/profile。
- recipe 批处理。
- artifact 与结果可追溯。
- GUI/TUI/CLI 共享同一 app workflow。
