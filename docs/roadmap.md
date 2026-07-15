# 路线图

## 选型锁定

- 语言：Rust only。
- 唯一产品二进制：`camera-toolbox`；无参数启动 `egui/eframe` GUI，有子命令进入无头自动化分支。
- 内部 CLI library：复用 app workflow，且 argv 分流发生在 eframe 初始化前。
- 产品构建：默认同时编译 Local、CV610、SSH-managed；叶子 Cargo feature 仅作为 provider 实现与重依赖隔离边界。发布按 Windows/macOS/Linux 原生 OS/架构 runner 拆分，不按功能 provider 拆分。
- 依赖管理：workspace `[workspace.dependencies]` 统一锁定 GUI 版本族，避免多套 `egui` 类型。

## P0：只读闭环

目标：打通本地 RAW 与受控远端只读采集；当前已实现 CV610 direct TCP、SSH-managed、内存资产和多文档 GUI，寄存器读写与自动曝光仍不进入本阶段。

验收：

- CLI 能打开本地紧密排列的已解包 `u16le` RAW，并按显式 width/height/bit depth 分析 ROI。
- RAW spec 的字节数或 bit depth 非法时拒绝分析；像素超过当前 bit depth 时保留原值，GUI 通过 Mono 洋红、Color clamp 与 diagnostics 明确告警。
- 数据质量 Warning 支持手动关闭和 8s 自动消失，但 `RawDiagnostics`、状态栏摘要、MAGENTA 与 Hover View 诊断持续有效；GUI 与无头命令分支对动作结果输出统一 DEBUG/WARN/ERROR JSONL 审计日志，无头命令 stdout 保持业务结果。
- 同一 RAW 的统计结果可重复。
- GUI 能用参数打开本地 RAW，切换 Raw Mono/Color，显示 ROI；`Tools -> Hover View` 提供即时、固定大小、可选 3×3/5×5/7×7 的 RAW 邻域检查，显示原始值、CFA、已安装彩色纹理对应的 RGB 和 ROI 统计。
- 彩色预览支持四种 Bayer、R/Gr/Gb/B black/gain、bilinear demosaic，以及默认 2.2、可在 GUI 调节并可关闭的 Gamma 显示；自动 BLC/AWB、CCM、LSC、edge-aware demosaic 显式 deferred。
- CV610 PQTools Dump 支持 RAW10/RAW12/JPEG/NV21；PQStream 支持有界 H.264/H.265 transport、H.265 preview 与显式 recording，协议 fixture 和 fake server 已验证。
- SSH-managed 默认采用进程内密码登录，客户端私钥文件降为显式第二方式；server host key 通过异步无认证扫描与 `known_hosts` 精确匹配，未知 fingerprint 必须显式确认、变更 key 硬阻断。Fetch/Watch-only profile 不再要求 capture recipe；远端载荷先进入有界内存，不产生 capture 临时文件。
- GUI 使用 Platform/Sensor 独立选择和多文档 Tab；未绑定 Sensor 时 platform-only 能力仍可用，同一 Platform 切换 Sensor 不重连既有 runtime。
- 统一二进制的无头分支已提供 versioned profile list/validate、Platform/Sensor probe、CV610 Dump/有限录制和 SSH typed capture/fetch。GUI 与无头分支通过同一 resolver/controller/event 路径；跨入口契约测试用同一 profile fixture 校验 `Unbound` 与 resolved capability/evidence 一致。
- CV610、SSH 与 RDK X5 真实设备端到端验收仍是明确未完成项，不将 fixture 结果提升为实机能力。

## P1：受控手动操作

目标：在软件内支持人工触发的单步操作，包括重新采集、加载历史 artifact、调整 ROI、读取诊断寄存器、规划曝光寄存器写入；曝光参数仍由人决定，默认不自动闭环。

验收：

- GUI 与无头命令分支的手动动作都映射到同一套 app command/event/state，不绕过 workflow 直接调用 adapter。
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
- GUI 与无头命令分支共享同一 app workflow。
