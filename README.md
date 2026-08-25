# pongbot-calib-tool

X5_233 双目标定工具（Godot 桌面端）。独立于 Camera_Toolbox 的新 Rust 项目，
复用其领域层（core / app / adapters / ffmpeg-bridge / logging / i2c-helper），
前端为 Godot 4.4 + gdext（Rust 代码构建 UI，不依赖编辑器操作）。

## 定位

单页纵向向导（中文），固定 CH0 ↔ RTSP 554 ↔ i2c-4、CH3 ↔ RTSP 557 ↔ i2c-6：

1. **连接设备**：X5 IP + SSH 凭据；SSH 启动板端 DEMO233（TCP 9073），连通后自动进入双预览
2. **双路预览与采集**：双 RTSP viewer + guided auto capture（棋盘检测 + 姿态去重，引导文本叠加在画面上）
3. **求解检查**：棋盘参数 + 双路 OpenCV 标定（RMS/内参展示）
4. **EEPROM 写入**：Inspect 读取当前状态 → 二次确认 → UpdateCalibration 写入标定内参

## 构建与运行

```bash
cargo build -p pongbot-calib-tool          # gdext 扩展
cargo build -p camera-i2c-helper --release # EEPROM helper sidecar（arm64）
./run.sh                                   # 启动 Godot 窗口
```

- 原生依赖（OpenCV5/FFmpeg）：由 `.deps`（符号链接指向 Camera_Toolbox 资产）+
  `.cargo/*.local.toml` 接线，`scripts/opencv5_dependency.py` / `ffmpeg_dependency.py` 可重新 prepare
- 无板验证：`PONGBOT_SYNTH=1 ./run.sh`（合成帧：预览 + 采集引导 + 求解错误路径）
- 调试截图：`PONGBOT_SCREENSHOT=/tmp/x.png ./run.sh`（5 帧后保存 viewport 截图）

## 目录结构

```
crates/
├── core/ app/ adapters/ ffmpeg-bridge/ logging/ i2c-helper/  # 从 Camera_Toolbox 复制（未改动）
└── frontends/godot/                                          # 本工具（gdext）
    ├── src/
    │   ├── lib.rs        # GodotClass 入口 + 流程编排
    │   ├── x5.rs         # SSH 启动 DEMO233 + TCP probe
    │   ├── preview.rs    # 双 RTSP + guided 采集（worker 线程）
    │   ├── solve.rs      # 标定求解（OpenCV backend）
    │   ├── eeprom.rs     # EEPROM Inspect/写入（helper sidecar）
    │   └── ui/           # 向导面板（connect/preview/solve/eeprom）
    └── godot/            # Godot 项目（project.godot/.gdextension/场景）
```

## 验证状态

| 链路 | 状态 |
|---|---|
| 扩展加载 / UI 构建（4 步向导 + 中文字体） | ✅ headless + Xvfb 截图验证 |
| 双路预览（合成帧 → 纹理） | ✅ 像素分析验证 |
| guided 采集（检测驱动 + 姿态去重 + 引导） | ✅ 合成帧验证引导路径（检测失败不误采） |
| 标定求解（OpenCV 检测/标定管线） | ✅ 合成帧验证错误路径；真实棋盘待实机 |
| SSH 启动驱动 / RTSP / EEPROM 读写 | ⏳ 需 X5 板子（10.21.12.108）实机验证 |

## 与 Camera_Toolbox 的关系

- 领域层为**复制**（非引用），两仓库互不影响
- 板端驱动（DEMO233）部署与协议以 `X5_Driver` / `docs/x5-233-auto-capture-guide.md` 为准
- EEPROM map 为 `yg-stereo-p24c64g-v1`（core::calibration_eeprom）
