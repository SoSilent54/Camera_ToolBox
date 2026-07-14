# 架构设计

## 目标

Camera Toolbox 用 Rust-only 路线统一 ISP 标定过程中的采集、文件传输、RAW 解码、图像分析、人工查验和后续半自动曝光闭环。当前只读阶段已经覆盖本地文件、CV610 direct TCP 与 SSH-managed 平台；寄存器写入和自动曝光仍保持关闭。

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

## 当前只读调用流

```text
PlatformProfile + Unbound/SensorMode
        │
        ▼
PlatformRegistry ──► candidate PlatformBindings
        │
        ▼
CapabilityResolver ──► Arc<TargetResolutionSnapshot>
        │                    (key + resolved handles + hashes)
        ▼
PlatformController
        ├── Local Raw Loader
        ├── CV610 Dump/Stream
        └── SSH Command/SFTP/Watcher
        │
        ▼
bounded CaptureStore ──► EphemeralAsset ──► Workspace Tab
        │
        └── explicit Save/Export only
```

Platform provider 只声明基础 transport 能力；有效能力按 `PlatformProfileId + Unbound | (SensorId, ModeId)` 解析。当前 Dump/Stream/SSH 是 `PlatformOnly`，未绑定 Sensor 时仍可用；未来寄存器等 `SensorScoped` 能力才要求精确 matrix cell。job/session 持有提交时的不可变 snapshot，切换 Sensor 不重连同一 platform，切换 platform 才替换 runtime。

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

当前只读阶段允许 `LoadRaw`、CV610 Dump/Stream、SSH typed capture/fetch/watch、SetActiveRoi 和 AnalyzeRoi。P1 才允许 `ReadRegister`、`PlanRegisterWrite`、`ApplyRegisterWrite`、`SetExposure` 等设备状态相关命令，且写入必须经过 Sensor×Platform 精确能力、allowlist、dry-run/确认、写后 readback 和 journal。

## 关键对象

| 对象 | 所属 crate | 职责 |
|---|---|---|
| `RawSpec` | `core` | 描述紧密排列 RAW 的分辨率、bit depth、packing 和 CFA。 |
| `RawFrame` | `core` | 持有已解包的 RAW 像素和对应规格。 |
| `ColorPipelineParams` / `PreparedBayer` | `core` | 校验四 CFA 通道 black/gain 与可选 finite 正 Gamma，并生成可双线性去马赛克的线性 Bayer 派生数据；不修改 `RawFrame`。GUI 将 Gamma 操作范围限制为 0.1–5.0，但不收窄 core API。 |
| `Roi` | `core` | 使用图像坐标定义统计区域。 |
| `RoiStats` | `core` | ROI 内 min/max/mean/saturation 等定量结果。 |
| `CommandEnvelope` / `PlatformController` | `app` | 编排本地 workflow、平台 job/session、取消、事件与终态。 |
| `WorkflowEvent` / `PlatformEvent` | `app` | 向前端发布按 operation/session/document 标识的结果和错误。 |
| `PlatformProfile` / `ProfileStore` | `app` | 版本化 tagged 平台配置；凭据只保存引用。 |
| `CapabilityResolutionKey` / `TargetResolutionSnapshot` | `app` | 固化 Platform 与可选 Sensor/Mode 组合、resolved handles 和配置 hashes。 |
| `EphemeralAsset` / `CaptureStore` | `core` / `app` | 网络载荷的有界内存所有权；capture/watch 不隐式落盘。 |
| `PlatformRegistry` | `adapters` | 分派独立 CV610、SSH-managed provider，不混用 variant 状态。 |
| `SensorIdentity` | `app` port | 只读身份/profile 能力，所有 sensor 端口的最小公共父能力。 |
| `CaptureBackend` | `app` port | P0 取图能力，只依赖 `SensorIdentity`，不要求寄存器写权限。 |
| `RegisterRead` | `app` port | 寄存器读取能力，用于诊断和 readback。 |
| `RegisterWrite` | `app` port | 受控寄存器写能力，P1/P2 才组合进 workflow。 |
| `ExposureControl` | `app` port | P2 语义曝光控制占位能力；当前骨架不提供默认寄存器规划实现。 |
| 具体 sensor 实现 | `adapters` | 按 sensor 型号选择性实现上述小能力 trait，隐藏寄存器表和外部采集命令差异。 |
| `RawFrameLoader` | `app` port / `adapters` impl | 本地或后续远程 RAW 帧加载端口；frontends 不直接调用具体 loader，只经 workflow 使用。 |
| `ArtifactStore` / `CaptureStore` | `app` | legacy 持久化端口与当前 bounded ephemeral source store；持久文件只由显式导出创建。 |

只读平台 job 的类型约束要求 `Arc<TargetResolutionSnapshot>`，因此前端不能绕过 resolver 直接使用 candidate handles。P1/P2 的寄存器与曝光 workflow 再显式组合 `SensorScoped` 的 `RegisterRead + RegisterWrite` 或 `ExposureControl`；缺少精确 Sensor/Mode×Platform cell 时不得暴露这些能力。

## 参数与默认值

以下是当前实现的关键安全默认值；完整协议参数与证据边界见 `.ai_doc/plans/20260714_153337__cv610_platform_integration__cv610_capture_streaming.md`。

| Parameter | Scope | Default | Meaning |
|---|---|---|---|
| Dump / Stream port | CV610 profile | 4321 / 80 | PQTools one-shot 与 PQStream 独立 TCP endpoint。 |
| Per-operation / global source budget | CaptureStore | 256 MiB / 1 GiB | 接收前预留；超限失败且禁止 disk spill。 |
| Live close deadline | GUI/controller | 5 s | 异步关闭宽限；超时 kill sidecar 并记录 Forced。 |
| SSH stable samples / interval | SSH profile | 2 / 500 ms | 无 producer marker 时的 size+mtime 稳定判断。 |
| Passive watcher auto-open | SSH profile | false | 默认只进入 Assets，不抢 active Tab。 |
| Sensor selection | Capability resolver | Unbound | 当前 platform-only 能力无需 Sensor 即可使用。 |

## 安全边界

- P0 只读，不写 sensor 寄存器。
- P1 才引入 `RegisterAdapter` 的真实实现，默认必须只读或 dry-run。
- `i2ctransfer` 不允许由 UI 拼接任意命令。
- SSH 断线、采集失败、hash mismatch、RAW spec mismatch 都必须进入明确错误状态。
- CV610/SSH 网络 source 必须先进入有界内存；capture/watch 禁止 `.part`、wire/manifest 临时文件和 disk spill。显式导出只允许新目标，拒绝覆盖已有文件。
- SSH command 只接受部署 allowlist recipe 与 typed 参数。普通 SSH exec 对每个 argv 做 POSIX shell-safe 编码；可选 CTARGV1/event subsystem 必须显式配置和探测。
- SSH 必须严格比对 profile 中的 OpenSSH host public key；profile 只保存 `key-file:` 或 `session:` credential reference。
- GUI Hover View 的 RAW 邻域直接采样 raw preview texture；正常样本为灰度，超 bit-depth 样本使用洋红诊断色，图像外邻域格明确留空，绝不通过边界 clamp 伪造样本。
- Hover View 偏好由 app 级状态持有，不随加载、关闭或 Reset View 重置；其 foreground `Area` 必须不可交互，不能拦截 viewer 的 hover、缩放或拖拽。
- 彩色纹理、rendered params、revision 与 diagnostics 必须同批安装；Hover View 的 RGB/CFA 只按已安装参数计算，pending 参数不得提前影响显示读数，首次渲染错误必须显示 unavailable 而不是 rendering。
- `RawDiagnostics` 是当前 RAW 生命周期内持续有效的领域事实；顶部 Notification 只是可关闭/超时的视图，关闭或过期不得清除 status badge、MAGENTA、Hover View 或 diagnostics。
- `NotificationCenter` 分离当前可见项和按 generation/attempt scope 的已见 tombstone：关闭/过期仅移除可见项，同 key 不复现；关闭或替换图片时同时回收旧 scope 的两者，防止无界增长。
- DEBUG/WARN/ERROR 由 frontend/action 或 adapter 副作用边界记录一次；core 仅返回 typed error/diagnostics。前端共用 logging crate，以 console + daily JSONL 输出、7-file 上限和 `RUST_LOG` filter 审计事件；日志初始化失败仅回退 stderr，不得阻止应用启动。
