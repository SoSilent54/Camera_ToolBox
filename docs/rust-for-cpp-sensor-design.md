# Rust 给 C++ 工程师的 Sensor 抽象设计

## C++ 到 Rust 的对应关系

| C++ 概念 | Rust 对应 | 在本项目中的用法 |
|---|---|---|
| `class` 数据成员 | `struct` 字段 | `Imx415Sensor { profile, bus, capture }` |
| `public/private` | `pub` / 默认私有 | 对外只暴露安全方法，隐藏 fd、bus、命令细节 |
| `.hpp` 接口声明 | `trait`、`pub struct`、`pub fn` | 小能力 trait 组合，如 `CaptureBackend` / `RegisterWrite` |
| `.cpp` 方法实现 | `impl Type` / `impl Trait for Type` | 每个 sensor 型号按需实现能力 |
| 纯虚基类 | `trait` | Rust 更推荐拆成小接口，而不是一个巨大 `ISensor` |
| 虚函数多态 | `Box<dyn Trait>` / `&mut dyn Trait` | 运行时按配置选择 sensor 型号 |
| 模板多态 | `T: Trait` | 单测或固定类型流程可用 |
| 继承复用 | 组合 + trait 默认方法 | profile 规划通用寄存器写入，特殊型号覆盖 |

Rust 没有 C++ 那种类继承。统一接口用 `trait`；数据封装用 `struct`；复用优先用组合。

## 项目推荐模型

Sensor 差异不要全部塞进一个巨大 trait。寄存器表、RAW mode、曝光寄存器布局这类型号差异应尽量数据化到 `SensorProfile`；trait 只表达稳定行为。

Sensor capability traits     # 稳定行为接口，按权限拆分
├── SensorIdentity           # profile/model，只读身份能力
├── CaptureBackend           # P0 取图能力，不包含写寄存器
├── RegisterRead             # 寄存器读取/readback
├── RegisterWrite            # 受控写寄存器，P1/P2 才需要
└── ExposureControl          # 语义曝光控制，可有默认实现

SensorProfile               # 型号差异数据
├── sensor model
├── supported modes
├── RAW spec
├── register allowlist
├── exposure register layout
├── gain register layout
└── safety limits
```

## Trait：类似 C++ 纯虚接口，但建议拆小

```rust
pub trait SensorIdentity {
    fn profile(&self) -> &SensorProfile;

    fn model(&self) -> &str {
        self.profile().model.as_str()
    }
}

pub trait CaptureBackend: SensorIdentity {
    fn capture(&mut self, request: &CaptureRequest) -> Result<CaptureArtifact, SensorError>;
}

pub trait RegisterRead: SensorIdentity {
    fn read_register(&mut self, addr: RegisterAddress) -> Result<RegisterValue, SensorError>;
}

pub trait RegisterWrite: RegisterRead {
    fn write_register(&mut self, write: RegisterWriteRequest) -> Result<(), SensorError>;
}

pub trait ExposureControl: RegisterWrite {
    fn apply_exposure(&mut self, setpoint: ExposureSetpoint) -> Result<(), SensorError>;
}

// P2 若 `SensorProfile::plan_exposure` 成熟后，可再把通用逻辑做成 helper 或默认方法。
```

这样 P0 只读 workflow 只接受 `CaptureBackend`，类型层面拿不到 `RegisterWrite`。P1/P2 的手动曝光、自动曝光 workflow 再显式要求 `RegisterWrite` 或 `ExposureControl`。

注意：Rust trait object 不能直接写 `Box<dyn CaptureBackend + RegisterRead>` 组合多个非 auto trait。需要运行时多态组合能力时，应定义组合 supertrait，例如：

```rust
pub trait ReadableCaptureBackend: CaptureBackend + RegisterRead {}

impl<T> ReadableCaptureBackend for T where T: CaptureBackend + RegisterRead {}
```

若不需要 trait object，也可以用泛型约束：`T: CaptureBackend + RegisterRead`。

## Struct + impl：类似某个 sensor 的 .cpp 实现

```rust
pub struct Imx415Sensor {
    profile: SensorProfile,
    bus: I2cBus,
    capture: CaptureProcess,
}

impl SensorIdentity for Imx415Sensor {
    fn profile(&self) -> &SensorProfile {
        &self.profile
    }
}

impl CaptureBackend for Imx415Sensor {
    fn capture(&mut self, request: &CaptureRequest) -> Result<CaptureArtifact, SensorError> {
        self.capture.run(request)
    }
}

impl RegisterRead for Imx415Sensor {
    fn read_register(&mut self, addr: RegisterAddress) -> Result<RegisterValue, SensorError> {
        self.bus.read(addr)
    }
}

impl RegisterWrite for Imx415Sensor {
    fn write_register(&mut self, write: RegisterWriteRequest) -> Result<(), SensorError> {
        self.profile.validate_write(&write)?;
        self.bus.write(write)
    }
}
```

## 运行时多态：类似 `std::unique_ptr<ICaptureBackend>`

P0 只读流程只需要取图能力：

```rust
pub fn open_capture_backend(config: &SensorConfig) -> Result<Box<dyn CaptureBackend>, SensorError> {
    match config.model.as_str() {
        "imx415" => Ok(Box::new(Imx415Sensor::open(config)?)),
        "ov9281" => Ok(Box::new(Ov9281Sensor::open(config)?)),
        other => Err(SensorError::UnsupportedModel(other.to_owned())),
    }
}
```

P1/P2 如果需要写寄存器，入口类型再升级为 `Box<dyn ExposureControl>` 或泛型组合约束。

```rust
pub fn run_capture(sensor: &mut dyn CaptureBackend) -> Result<CaptureArtifact, SensorError> {
    sensor.capture(&CaptureRequest::default())
}
```

这类比 C++：

```cpp
std::unique_ptr<ICaptureBackend> sensor = openCaptureBackend(config);
sensor->capture(request);
```

## 编译期多态：类似 C++ template

如果类型固定，可以用泛型：

```rust
pub fn run_capture<T: CaptureBackend>(sensor: &mut T) -> Result<CaptureArtifact, SensorError> {
    sensor.capture(&CaptureRequest::default())
}
```

本项目 P0 更常用 `Box<dyn CaptureBackend>`；后续写寄存器流程再显式要求写能力，避免只读链路误拿写权限。

## Crate 放置建议

```text
crates/core/src/sensor/
├── types.rs       # RegisterAddress, RegisterValue, CaptureRequest
├── profile.rs     # SensorProfile, RegisterMap, ExposureRegisterLayout
└── exposure.rs    # ExposureSetpoint, ExposurePlan

crates/app/src/ports/
└── sensor.rs      # SensorIdentity / CaptureBackend / RegisterRead / RegisterWrite / ExposureControl

crates/adapters/src/sensors/
├── imx415.rs      # struct Imx415Sensor + impl SensorIdentity/CaptureBackend/...
└── ov9281.rs      # struct Ov9281Sensor + impl SensorIdentity/CaptureBackend/...
```

依赖方向保持单向：

```text
core  <-  app  <-  adapters  <-  frontends
```

- `core`：纯数据、纯计算、profile 校验，不碰设备 IO。
- `app`：定义 port trait 和 workflow。
- `adapters`：每个 sensor 型号实现 trait。
- `frontends`：根据配置组装具体实现并注入 workflow。

## 设计禁忌

不要把每个寄存器都设计成 trait 方法：

```rust
// 不推荐
trait SensorBackend {
    fn write_reg_0x3000(&mut self, value: u32);
    fn write_reg_0x3001(&mut self, value: u32);
    fn write_imx415_exposure_high(&mut self, value: u32);
}
```

推荐把寄存器表数据化，通过 profile 规划受控写入，并只在写能力 workflow 中暴露：

```rust
let plan = sensor.profile().plan_exposure(setpoint)?;
for write in plan {
    sensor.write_register(write)?;
}
```

这样新增 sensor 型号时，优先新增 profile；只有采集命令、总线访问或写入时序确实不同，才新增具体 `struct XxxSensor` 实现。
