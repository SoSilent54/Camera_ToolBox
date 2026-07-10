# 路线图

## 选型锁定

- 语言：Rust only。
- 主 GUI：`egui/eframe`。
- 运维副界面：`ratatui/crossterm`。
- 自动化入口：CLI。
- 依赖管理：workspace `[workspace.dependencies]` 统一锁定 GUI 版本族，避免多套 `egui` 类型。

## P0：只读闭环

目标：先把采集、拉取、解码、分析、展示和 journal 串通，不做自动曝光和寄存器写入。

验收：

- 能从 CLI 触发一次只读流程。
- 采集/传输/分析失败有结构化错误。
- RAW spec 不匹配时拒绝分析。
- 同一 RAW 的统计结果可重复。
- GUI 原型能显示真实最大尺寸样本图、hover 像素值、ROI 和直方图。

## P1：受控手动曝光

目标：在软件内受控读写曝光相关寄存器，但仍由人决定参数。

验收：

- sensor profile 声明允许寄存器、位宽、范围、group hold 策略。
- 越界写入被拒绝。
- 写后 readback 和 journal 完整记录。
- 设备状态不确定时禁止继续自动写入。

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
