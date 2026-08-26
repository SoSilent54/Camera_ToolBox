# pongbot-calib-tool

X5_233 双目标定工具（原生 Dear ImGui 桌面前端）。独立于 Camera_Toolbox 的新 Rust 项目，
复用其领域层（core / app / adapters / ffmpeg-bridge / logging / i2c-helper），
前端为 Rust 原生 Dear ImGui（winit + glutin + glow + imgui-rs，无外部编辑器/运行时）。

## 定位

单页纵向向导（中文），固定 CH0 ↔ RTSP 554 ↔ i2c-4、CH3 ↔ RTSP 557 ↔ i2c-6：

1. **连接设备**：X5 IP + SSH 凭据；SSH 启动板端 DEMO233（TCP 9073），连通后自动进入双预览
2. **双路预览与采集**：双 RTSP viewer + guided auto capture（棋盘检测 + 姿态去重，引导文本叠加在画面上）
3. **求解检查**：棋盘参数 + 双路 OpenCV 标定（RMS/内参/单图 RMSE 柱状图展示）
4. **EEPROM 写入**：Inspect 读取当前状态 → SNID 预览 → 二次确认 → UpdateCalibration 写入标定内参

## 构建与运行

```bash
cargo build -p pongbot-calib-tool          # 原生 Dear ImGui 二进制
cargo build -p camera-i2c-helper --release # EEPROM helper sidecar（arm64）
./run.sh                                   # 启动 ImGui 窗口
```

- 原生依赖（OpenCV5/FFmpeg）：由 `.deps`（符号链接指向 Camera_Toolbox 资产）+
  `.cargo/*.local.toml` 接线，`scripts/opencv5_dependency.py` / `ffmpeg_dependency.py` 可重新 prepare
- 无板验证：`PONGBOT_SYNTH=1 ./run.sh`（合成帧：预览 + 采集引导 + 求解错误路径）
- 中文字体：启动时从系统加载 CJK 字体（Noto Sans CJK → 文泉驿 → Droid fallback）

## 目录结构

```
crates/
├── core/ app/ adapters/ ffmpeg-bridge/ logging/ i2c-helper/  # 从 Camera_Toolbox 复制（未改动）
└── frontends/imgui/                                          # 本工具（原生 Dear ImGui）
    ├── src/
    │   ├── main.rs        # winit/glutin/glow + ImGui 窗口与交互（纹理上传、overlay 绘制）
    │   ├── lib.rs         # UI 无关业务模块集合（控制器 + 预览/求解/EEPROM）
    │   ├── controller.rs  # 强类型流程控制器（连接/采集/求解/写入状态机）
    │   ├── x5.rs          # SSH 启动 DEMO233 + TCP probe
    │   ├── preview.rs     # 双 RTSP + guided 采集（worker 线程）
    │   ├── solve.rs       # 标定求解（OpenCV backend）
    │   ├── eeprom.rs      # EEPROM Inspect/写入（helper sidecar）
    │   ├── guide_overlay.rs # overlay 绘制数据（UI 无关 DTO）
    │   └── theme.rs       # ImGui 深色主题 + CJK 字体
    └── tests/             # 检测链路集成测试（OpenCV 棋盘检测）
```

## 验证状态

| 链路 | 状态 |
|---|---|
| 窗口 / UI 构建（三步向导 + 中文字体） | ✅ Xvfb 下启动验证 |
| 双路预览（合成帧 → GL 纹理） | ✅ 合成帧验证 |
| guided 采集（检测驱动 + 姿态去重 + 引导） | ✅ 合成帧验证引导路径（检测失败不误采） |
| 标定求解（OpenCV 检测/标定管线） | ✅ 合成帧验证错误路径；真实棋盘待实机 |
| SSH 启动驱动 / RTSP / EEPROM 读写 | ⏳ 需 X5 板子（10.21.12.108）实机验证 |

## 与 Camera_Toolbox 的关系

- 领域层为**复制**（非引用），两仓库互不影响
- 板端驱动（DEMO233）部署与协议以 `X5_Driver` / `docs/x5-233-auto-capture-guide.md` 为准
- EEPROM map 为 `yg-stereo-p24c64g-v1`（core::calibration_eeprom）
