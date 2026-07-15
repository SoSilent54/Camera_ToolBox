# Camera Toolbox

Rust-only ISP 标定工具箱。当前路线已锁定为：

- 唯一产品入口：`camera-toolbox`。无命令行子命令时启动 `egui/eframe` GUI；提供子命令时直接执行无头自动化分支，不初始化 X11/Wayland。
- 内部 CLI library 仅承载 agent、CI、批处理和回放逻辑，不再生成第二个可执行文件。
- 核心：UI 无关 Rust workspace，采集、RAW、分析、journal、寄存器访问都通过清晰端口隔离。

## 当前阶段

当前已完成本地 RAW、多文档工作区，以及 CV610/SSH-managed 只读采集链路。寄存器读写、自动曝光闭环和真实设备验收仍未开放。

```text
Camera Toolbox
├── crates/
│   ├── core/           # RAW/媒体/临时资产、ROI 统计与 sensor 描述
│   ├── app/            # Platform/Profile/能力矩阵、命令事件与内存预算
│   ├── adapters/       # CV610 PQTools/PQStream、SSH/SFTP、本地文件
│   └── frontends/
│       ├── cli/        # 内部无头命令 library，不生成 bin
│       └── gui/        # 唯一 camera-toolbox bin；GUI + argv 分流
├── build.sh            # local / cv610 / ssh 单平台构建入口
├── docs/
│   ├── architecture.md # 架构边界与调用流
│   └── roadmap.md      # P0 起步路线与验收
└── Cargo.toml          # workspace 统一依赖版本
```

## 快速验证

```bash
cargo fmt --all -- --check
./build.sh local debug
./build.sh cv610 debug
./build.sh ssh debug
```

`build.sh` 必须显式选择 `local`、`cv610` 或 `ssh`，并通过 `--no-default-features` 只启用对应 provider。产物分别进入 `target/<platform>/debug/camera-toolbox`；第二参数可传 `release`。项目的 `profile.dev` 仍使用优化代码生成，但 Cargo 目录名保持 `debug`。

## CI 与发布

- 每个分支 push 会分别运行 common 检查以及 `local`、`cv610`、`ssh` 三个互相隔离的 feature job；单个 job 不同时启用多个远端 provider。Clippy 仅报告 warning，不以 `-D warnings` 阻断。
- 也可在 GitHub `Actions -> CI -> Run workflow` 中手动执行同一套检查。
- 推送任意 Git tag 会为每个功能平台分别构建单一 `camera-toolbox` 可执行文件。归档命名为 `camera-toolbox-<platform>-<environment>`，其中：
  - `<platform>`：`local`、`cv610`、`ssh`；
  - `<environment>`：`macos-aarch64`、`windows-x86_64`、`linux-{x86_64|aarch64}-ubuntu{20|22}`。
- 也可在 GitHub `Actions -> Release -> Run workflow` 中填写 tag。Ubuntu 20/22 使用对应官方容器；x86_64 与 aarch64 使用匹配架构 runner。


本地 RAW 无头分析：

```bash
./build.sh local release
target/local/release/camera-toolbox analyze-raw \
  --raw <frame.raw> --width <w> --height <h> --bit-depth <n> \
  --encoding u16le --roi 0,0,<w>,<h>
```

GUI 本地 RAW 预览：

```bash
target/local/release/camera-toolbox
```

在菜单中选择 `File -> Open Raw...`，可手工填写或通过 `Select` 选择文件路径。软件会基于文件名、文件长度和有限像素样本生成 Preset，并自动应用评分最高的可加载候选；切换其他 Preset 会立即回填参数，手工修改 width、height、有效 bit depth、uint16 容器或端序后显示为 `Custom`。候选不是可靠识别，Bayer 仍须人工确认。

加载成功后默认显示 `Color`，右侧 `Color Processing` 面板默认展开，可从标题栏 chevron 收起为窄 rail 并随时重新展开。面板可实时调整 Bayer、R/Gr/Gb/B black level、通道 gain 与 Gamma；Gamma 默认开启并取 2.2，GUI 可在 0.1–5.0 范围调整，关闭时线性 RGB 直接量化。关闭后重新开启会恢复上次设置。默认链接四通道 black 和 Gr/Gb gain。显示链路固定为 black subtraction → `(max_code-black)` 归一化 → CFA gain → bilinear demosaic → clamp → 可选 Gamma → RGB8。`View` 菜单可切换 `Raw Mono`、`Color`。
`Tools -> Hover View` 默认开启，可关闭；开启后可选择 3×3、5×5（默认）或 7×7 RAW 邻域。鼠标进入图像即显示固定大小、跟随指针的非交互检查窗：邻域始终读取 RAW preview（Color 主视图下也不读取插值彩色纹理），中心格带十字与边框，图像边缘越界格留空。信息区显示坐标、CFA 通道、RAW 值、最后一次已安装彩色参数得到的 RGBf/RGB8、颜色色块和 ROI 统计；超 bit-depth 样本保留洋红诊断色。底部状态栏只保留文件规格、显示模式、缩放和异常摘要。
RAW 超 bit-depth 时，顶部会出现可关闭的 Warning，并在 8 秒后自动消失；其消失不会清除状态栏 `RAW range`、MAGENTA 像素或 Hover View 诊断。加载和渲染失败显示可关闭但不自动消失的 Error，同时保留对话框/面板中的就地错误。GUI 与无头 CLI 分支统一写入 console 和按日滚动的 JSONL 文件，最多保留 7 个匹配文件；可用 `RUST_LOG` 临时调整等级。日志目录由平台 `ProjectDirs` 解析；典型位置为 Linux `${XDG_STATE_HOME:-~/.local/state}/cameratoolbox/logs`、macOS `~/Library/Application Support/org.camera-toolbox.Camera-Toolbox/logs`、Windows `%LOCALAPPDATA%\camera-toolbox\Camera Toolbox\data\logs`，应以 GUI `Help -> Log directory` 显示并可复制的实际路径为准。无头命令的业务结果仍写 stdout，日志不混入该输出。

本地文件入口目前只支持紧密排列、已解包的 `u16le` Bayer RAW。CV610 Dump 另外支持协议内的 packed RAW10/RAW12、JPEG 和 NV21，并在内存中完成长度、checksum、stride 和格式校验；这不代表本地文件对话框已经支持这些 packed 格式。彩色预览不包含自动 black level/AWB、CCM、LSC、降噪或 edge-aware demosaic，因此属于 sensor RGB 查验，不代表标定后的准确 sRGB。

本地 RAW 路径也走 `app::Workflow::load_raw_and_analyze` 与 `RawFrameLoader` port；CLI/GUI 不直接解码或统计 RAW。

## 平台采集与配置

GUI 顶部依次选择 Platform Profile 和 `Sensor: Unbound` 或已配置的 Sensor/Mode。当前 Dump、Stream、SSH Command/File 都是 platform-only 能力，因此没有 Sensor 配置时仍可使用；Sensor×Platform cell 目前只用于收窄格式或补充 Bayer、方向等证据，尚不产生寄存器权限。

选择 `Device Manager...` 可新建、校验、保存、导入或导出 tagged profile：

- `Local`：只显示本地 RAW 打开入口。
- `Hisilicon CV610`：一个 host 由 Dump/Stream 共享，默认端口分别为 `4321`/`80`。
- `SSH-managed`：普通用户只需填写 host/IP、username、当前进程密码和一个绝对远程文件路径；`测试 SSH 登录与远程路径` 会按当前密码或私钥执行严格 pin 登录与 SFTP metadata 检查，Capture recipe、watcher 和限制放在折叠的 Advanced 区域。

配置文件为 versioned JSON `platform-profiles.json`，目录由 `ProjectDirs::from("io", "sosilent", "camera-toolbox")` 解析。首次启动只创建 `Local files`；网络目标必须显式配置。导入时拒绝未知 schema、未知字段、重复 ID 和无效跨引用。编辑配置不会改变已提交 job；job 持有提交时的 Platform/Sensor/matrix snapshot hashes。

### CV610

- Dump 使用一次一连接的 PQTools TCP 4321；当前 profile host 必须是数值 IPv4/IPv6 地址。`Auto` 和 `DirectOnly` 都只发送已验证的直接请求；只有显式注入的 `ValidatedRecipe` 才会执行额外初始化。
- Stream 使用独立 PQStream TCP 80 会话，支持有界 RTP/H.264/H.265 解析、H.265 live preview、显式 recording 和异步关闭。FFmpeg 不可用时仍可接收/显式录制，但不会伪装为可预览。
- Dump/SSH fetch 先成为有上限的内存 `EphemeralAsset`，不会创建 `.part`、wire dump 或 manifest 临时文件。默认单次上限 256 MiB、全局未保存 source 上限 1 GiB；超限明确失败，不回退磁盘。
- `Save/Export` 只写用户选择的新目标并拒绝覆盖既有文件；写入失败会尽力删除该不完整目标，不创建隐藏 staging 文件。

### SSH-managed

#### GUI 快速配置

1. 选择 `Device Manager...` → `New SSH-managed`。
2. 填写 `Host / IP`、SSH port（默认 `22`）、`Username`。
3. `Client authentication` 默认选择 `Password`；密码只进入当前进程内存，永不写入 profile、日志或导出文件。重启后再次编辑该 profile、输入密码并保存即可重新注册。
4. 在 `Remote file` 粘贴一个绝对文件路径，例如 `/userdata/capture/frame.raw`。GUI 自动保存为受限 root `/userdata/capture` 与 literal basename `frame.raw`，选中 profile 时直接回填 `Fetch and Open` 路径。
5. 点击 `测试 SSH 登录与远程路径`：
   - 已保存 profile 含 server host-key pin 时，直接使用该 pin 和当前选择的 Password/SSH private key 登录；russh 握手仍会严格拒绝不匹配的服务器 key；
   - 新 profile 尚无 pin 时，先在不发送账号密码的情况下读取 server key；已知 key 自动继续，未知 key 显示 fingerprint，核对并点击 `信任并测试` 后继续登录，发生 key 变化则硬阻断；
   - 登录成功后规范化远程路径并执行 SFTP `stat`，显示路径、大小和 mtime。它验证 metadata 可见性，不读取或下载文件内容；
   - 测试只使用当前表单选择的认证方式，不会在密码与私钥之间 fallback，也不要求先保存 profile。
6. 点击 `Validate and Save`。新 profile 的 ID/display name 留空时由 `username@host` 稳定生成。

`SSH private key file` 是第二、可选的**客户端登录方式**，不是 server host key。GUI 自动发现 `~/.ssh` 中权限安全的 OpenSSH 私钥，也可通过文件选择器指定；profile 只保存 `key-file:/absolute/path`，不会保存私钥内容。密码与私钥不会互相 fallback，每次操作只使用当前选中的一种认证方式。

默认 profile 是 Fetch/Watch-only：`capture_recipe` 为空时仍正常绑定 SFTP `RemoteFile`，`Fetch and Open` 与 Watch 不受影响；`Remote Capture` 明确禁用。只有部署了远端采集程序时，才在 `Advanced` 中启用 Capture automation，并配置 typed recipe。普通 SSH `exec` 仍是默认命令路径；程序和 argv 布局只能来自部署时注册的 typed allowlist recipe，UI 不接受任意 shell command。可选 `CTARGV1 subsystem` 和 `Event subsystem` 只用于已部署的 helper；Event 留空时使用限定目录/glob 的稳定性 polling。

底层 profile 仍只保存 credential reference：

- `session:<id>`：GUI 密码登录自动生成，只存在于当前进程；
- `key-file:/absolute/path`：操作时读取经过权限、大小与 OpenSSH 格式检查的客户端私钥。

生产 SSH capture recipe 从以下完整环境变量组加载；全部缺失表示没有部署 recipe，只有部分字段则启动时明确报错：

```bash
CAMERA_TOOLBOX_SSH_RECIPE_ID=rdk-x5-vin-raw
CAMERA_TOOLBOX_SSH_RECIPE_PROGRAM=/absolute/path/to/capture-program
CAMERA_TOOLBOX_SSH_RECIPE_OUTPUT_ROOT=/absolute/remote/root
CAMERA_TOOLBOX_SSH_RECIPE_FORMATS=raw12,nv21
CAMERA_TOOLBOX_SSH_RECIPE_OUTPUT_DIR_FLAG=--output-dir
CAMERA_TOOLBOX_SSH_RECIPE_FORMAT_FLAG=--format
CAMERA_TOOLBOX_SSH_RECIPE_ONCE_FLAG=--once
CAMERA_TOOLBOX_SSH_RECIPE_PATH_STDOUT=true
```

该程序成功时必须在 stdout 返回一个 UTF-8 artifact path line；返回路径仍会经过远端根目录、稳定性、大小、hash 与内存预算校验。被动 watcher 默认只更新 Assets，不抢占当前 Tab。

### 统一二进制的无头命令分支

`camera-toolbox` 的 GUI 与无头命令使用同一 `ProfileStore → PlatformRegistry → CapabilityResolver → TargetResolutionSnapshot → PlatformController` 路径。只要存在命令行子命令，程序就直接执行 CLI library 并退出，不初始化 eframe；业务结果为确定性 JSON stdout，typed terminal failure 返回非零状态。

```bash
# Local build：profile 管理与无网络 probe
./build.sh local release
target/local/release/camera-toolbox profile list
target/local/release/camera-toolbox profile validate
target/local/release/camera-toolbox platform probe --platform <platform-id>

# CV610 build：still Dump / 有限时长 Stream recording
./build.sh cv610 release
target/cv610/release/camera-toolbox \
  cv610 dump --platform <platform-id> --kind raw12 --output <new-file.raw>
target/cv610/release/camera-toolbox \
  stream-record --platform <platform-id> --duration 10 \
  --quota-bytes 536870912 --annexb-output <new-file.h265> \
  --timestamp-output <new-file.jsonl>

# SSH build：typed capture recipe / 显式远程文件 fetch
./build.sh ssh release
target/ssh/release/camera-toolbox \
  ssh capture --platform <platform-id> --format raw12
target/ssh/release/camera-toolbox \
  ssh fetch --platform <platform-id> --remote-path </remote/file> \
  --format raw12-packed --output <new-file.raw>
```

`--sensor-id` 与 `--mode-id` 必须成对出现；都不提供时使用 `Sensor: Unbound`。所有平台命令均支持 `--profile-store <path>` 覆盖默认项目配置文件。某个单平台构建仍可读取含其他平台 variant 的 profile store，但 bind 未编译 provider 时会明确返回 `platform provider is not compiled or registered`。


### 验收限制

- Rust protocol fixtures、本地 TCP fake server、SSH state machine 和 GUI smoke 已通过；当前尚未连接真实 CV610 或 SSH 设备完成端到端验收。
- CV610 cold boot 初始化、Dump `0xEE` 错误、RAW metadata Bayer enum、YUV 其他 enum/range/matrix、真实 H.264、Dump+Stream 并发和自动重连仍保持 Unknown。
- RDK X5 仅提供未完成的 SSH-managed 模板，不代表已部署或已验收；host key、recipe、远端路径和采集格式必须来自实际设备证据。

## 设计原则

- GUI 与无头命令分支不直接执行 SSH、采集程序或 `i2ctransfer`。
- 所有外部副作用通过 adapter 端口进入 app workflow。
- RAW 定量分析基于原始 buffer，不基于 tone-mapped preview。
- artifact、配置、分析结果和设备回执后续都要进入可审计 journal。
- `egui`、`eframe`、`egui_plot` 在 workspace 统一版本族，避免前端集成时出现两套 `egui` 类型。
