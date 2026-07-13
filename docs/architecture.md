# 架构设计

## 目标

Camera Toolbox 用 Rust-only 路线统一 ISP 标定过程中的采集、文件传输、RAW 解码、图像分析、人工查验和后续半自动曝光闭环。首版先实现 P0 只读闭环，避免在数据链路未稳定前引入寄存器写入风险。

## 分层

```text
frontends
├── gui      (egui/eframe, 主图像交互)
├── tui      (ratatui/crossterm, 运维副界面)
└── cli      (批处理/自动化入口)
   │
   ▼
app         (CommandEnvelope, WorkflowEvent, workflow 编排与 port trait)
   │
   ├── 调用 core 做纯计算
   └── 只依赖 port trait，不依赖具体 adapters
       │
       ▼
adapters    (实现 app port trait：进程、SSH/SFTP、文件、寄存器 helper)
   │
   ▼
core        (RAW 描述、ROI、统计、journal 类型、领域校验)
```

## 依赖方向

- `core` 不依赖 `app`、`adapters` 或任何前端。
- `app` 可依赖 `core`，并拥有命令、事件、workflow 与外部副作用 port trait。
- `adapters` 可依赖 `app` 和 `core`，只实现 port trait，不被 `app` 反向依赖。
- `frontends/*` 负责组装具体 adapters 并注入 `app`，可依赖 `app`、`core`、`adapters`。

Rust workspace 不能形成 crate 循环，因此禁止 `app -> adapters -> app` 的双向依赖。运行时调用关系是 workflow 调用 trait object；编译期依赖关系是 adapters 实现 app 中定义的 trait。

## P0 只读闭环调用流

```text
CLI/TUI/GUI ──► CommandEnvelope / LocalRawAnalyzeRequest
   │
   ▼
Workflow::load_raw_and_analyze
   │    ├ RawFrameLoader port 调用 LocalRawLoader adapter
   │    ├ core raw 按显式 `RawSpec` 校验紧密布局字节数、bit depth 和像素范围
   │    ├ core analysis 计算 ROI 统计
   │    └ GUI preview 使用 report.frame 显示 `width x height` 完整有效区域
   ▼
WorkflowEvent stream
   │
   ├── CLI 输出结构化结果
   ├── TUI 显示进度和日志
   └── GUI 更新图像、ROI 和曲线
```

## 手动操作与 workflow 的关系

半自动 workflow 不是唯一入口。所有人工按钮、表单和命令行动作也必须建模为 `CommandEnvelope`，由同一个 app controller/workflow 执行并产出 `WorkflowEvent`，前端不得绕过 app 层直接调用 adapter。

```text
Atomic Manual Commands                 Macro Workflows
├── ManualCapture                       ├── CaptureAndAnalyze
├── LoadRaw                             ├── ManualExposureStep
├── LoadArtifact                        └── AutoExposureConverge
├── SetActiveRoi
├── AnalyzeRoi
├── ReadRegister
├── PlanRegisterWrite
├── ApplyRegisterWrite
└── SetExposure
       │
       ▼
app controller / workflow  (统一状态机、权限校验、journal、错误语义)
```

手动操作是一等公民的可审计原子命令；半自动 workflow 是这些原子命令的受控编排，而不是另一套隐藏入口。这样 GUI/TUI/CLI 可以支持人工调试、单步验证和脚本批处理，同时不把采集、寄存器访问、曝光状态机散落到三个前端。

P0 允许的手动命令只包含只读动作，例如 `ManualCapture`、`LoadRaw`、`LoadArtifact`、`SetActiveRoi`、`AnalyzeRoi`。P1 才允许 `ReadRegister`、`PlanRegisterWrite`、`ApplyRegisterWrite`、`SetExposure` 等设备状态相关命令，且写入必须经过 profile allowlist、dry-run/确认、写后 readback 和 journal。

## 关键对象

| 对象 | 所属 crate | 职责 |
|---|---|---|
| `RawSpec` | `core` | 描述紧密排列 RAW 的分辨率、bit depth、packing 和 CFA。 |
| `RawFrame` | `core` | 持有已解包的 RAW 像素和对应规格。 |
| `Roi` | `core` | 使用图像坐标定义统计区域。 |
| `RoiStats` | `core` | ROI 内 min/max/mean/saturation 等定量结果。 |
| `CommandEnvelope` | `app` | UI/CLI 提交给 workflow 的统一命令封装。 |
| `WorkflowEvent` | `app` | workflow 对前端发布的进度、结果、错误事件。 |
| `SensorIdentity` | `app` port | 只读身份/profile 能力，所有 sensor 端口的最小公共父能力。 |
| `CaptureBackend` | `app` port | P0 取图能力，只依赖 `SensorIdentity`，不要求寄存器写权限。 |
| `RegisterRead` | `app` port | 寄存器读取能力，用于诊断和 readback。 |
| `RegisterWrite` | `app` port | 受控寄存器写能力，P1/P2 才组合进 workflow。 |
| `ExposureControl` | `app` port | P2 语义曝光控制占位能力；当前骨架不提供默认寄存器规划实现。 |
| 具体 sensor 实现 | `adapters` | 按 sensor 型号选择性实现上述小能力 trait，隐藏寄存器表和外部采集命令差异。 |
| `RawFrameLoader` | `app` port / `adapters` impl | 本地或后续远程 RAW 帧加载端口；frontends 不直接调用具体 loader，只经 workflow 使用。 |
| `ArtifactStore` | `app` port / `adapters` impl | artifact 持久化、hash 校验和索引能力端口。 |

P0 workflow 的类型约束只能要求 `CaptureBackend` 和 `ArtifactStore`，不能要求 `RegisterWrite`。P1/P2 的手动曝光和自动曝光 workflow 再显式组合 `RegisterRead + RegisterWrite` 或 `ExposureControl`。这样只读流程在类型层面拿不到写寄存器能力。

## 参数与默认值

当前基础骨架不引入运行时配置默认值。后续 P0 引入设备 profile、RAW spec、artifact 根目录和超时时，需要在本表补齐默认值、单位、范围和影响路径。

| Parameter | Location/Scope | Type | Unit | Default | Valid Range | Meaning | Effect Path | Default Rationale | Impact of Increase/Decrease | Compatibility |
|---|---|---|---|---|---|---|---|---|---|---|
| 无 | 当前骨架 | - | - | - | - | 仅创建类型和 crate 边界 | 无运行时行为 | 避免在真实设备信息缺失时固化错误默认值 | 无 | 不影响 |

## 安全边界

- P0 只读，不写 sensor 寄存器。
- P1 才引入 `RegisterAdapter` 的真实实现，默认必须只读或 dry-run。
- `i2ctransfer` 不允许由 UI 拼接任意命令。
- SSH 断线、采集失败、hash mismatch、RAW spec mismatch 都必须进入明确错误状态。
- GUI hover 的像素值必须来自 RAW buffer，不从显示 texture 反查。
