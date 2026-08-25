# X5_233 自动快门标定最简操作指南

## 目的与适用范围

本文用于指导现场操作人员按固定步骤完成：

1. 安装并启动 X5 端 `DEMO233_TCP_08_10` 标定驱动；
2. 在 Camera Toolbox 里连接 X5_233_Driver；
3. 用 Auto Capture 自动采集棋盘图；
4. 执行内参标定并检查结果；
5. 将结果写入 EEPROM。

本文只覆盖当前 SC233 / X5_233 标定链路。不要把这里的 EEPROM 写入流程用于未确认型号或未确认 I²C bus 的设备。

---

## 1. 一句话理解整条链路

```text
X5 板端 DEMO233_TCP_08_10
  ├── RTSP 预览：给人看画面，也给软件找棋盘姿态
  └── TCP 9073：给软件读状态、开关 RTSP、抓同源 YUV/RAW 帧

Camera Toolbox
  ├── X5_233_Driver：连接板端驱动
  └── Calibration：自动快门、标定、导出、EEPROM 写入
```

通道固定记法：

| 通道 | 相机 | EEPROM I²C bus | 常用用途 | RTSP 地址 |
|---|---|---:|---|---|
| CH0 | cam0 | `4` | 左/第一路标定 | `rtsp://<X5_IP>:554/PRR` |
| CH3 | cam1 | `6` | 右/第二路标定 | `rtsp://<X5_IP>:557/PRR` |
| CH1 | cam0 低曝光 | `4` | 一般不用于标定 | `rtsp://<X5_IP>:555/PRR` |
| CH4 | cam1 低曝光 | `6` | 一般不用于标定 | `rtsp://<X5_IP>:558/PRR` |

---

## 2. 安装 X5 端驱动

### 2.1 驱动压缩包

当前新版驱动只用于标定验证，尚未做过完整运行测试。安装时不要覆盖板端原有 `/opt/DEMO233`。

工程人员提供的驱动压缩包文件名：

```text
DEMO233_TCP_08_10.tar
```

压缩包内可执行文件名：

```text
DEMO233_TCP_08_10
```

当前已知 sha256：

```text
# 压缩包 DEMO233_TCP_08_10.tar
29666a86c1090bdfd5d9012403a088d9ea3add6c9210f47749f619aa73624429

# 压缩包内 DEMO233_TCP_08_10 可执行文件
9ff0ea4727910c26c25d6d06665e38fe097e6bcddc76be82af5f186cd1b41d80
```

### 2.2 上传驱动压缩包并在 X5 板端解压

先用任意可用方式把 `DEMO233_TCP_08_10.tar` 上传到 X5 板端目录：

```text
/opt/DEMO233_TCP_08_10.tar
```

上传方式不限，可以使用 SFTP、scp、U 盘、网页文件管理器或工程人员提供的远程工具。本文不限定具体上传工具。

上传完成后，登录 X5 板端，在板端执行：

```bash
cd /opt
tar -xf DEMO233_TCP_08_10.tar
chmod 755 DEMO233_TCP_08_10
sha256sum DEMO233_TCP_08_10
```

确认输出的可执行文件 sha256 与上面的 `DEMO233_TCP_08_10` 一致。

注意：不要执行 `cp DEMO233_TCP_08_10 DEMO233`，也不要删除或覆盖原有 `/opt/DEMO233`。新版驱动仅作为本次标定程序使用。

---

## 3. 启动 X5 端驱动

SSH 登录 X5 板端：

```bash
ssh root@<X5_IP>
```

启动标定模式。以下命令必须在板端 root shell 中执行：

```bash
cd /opt

echo 1 > /sys/kernel/debug/isp/tune

echo 750000000 > /sys/kernel/debug/clk/isp_axi_clk/clk_rate
echo 750000000 > /sys/kernel/debug/clk/isp_core_clk/clk_rate
echo userspace > /sys/devices/system/cpu/cpufreq/policy0/scaling_governor
echo 1500000 > /sys/devices/system/cpu/cpufreq/policy0/scaling_setspeed

export DUAL_ISP_CREATE_HIGH_FIRST=1
export SC233_VENC_BITRATE=6000

LD_LIBRARY_PATH=/usr/hobot/lib:/usr/hobot/lib/sensor:/usr/lib:/lib:/lib64 \
SC233_CALIBRATION_MODE=1 \
./DEMO233_TCP_08_10
```

看到以下状态即可继续：

- TCP 控制监听 `0.0.0.0:9073`；
- CH0 RTSP 监听 `554`；
- CH3 RTSP 监听 `557`；
- 画面帧率日志持续刷新。

如果需要确认端口：

```bash
ss -lntp | grep -E '9073|554|557'
```

不要关闭这个终端。标定期间 `DEMO233_TCP_08_10` 必须保持运行。

---

## 4. 打开 Camera Toolbox 并连接驱动

### 4.1 打开软件

在电脑端打开工程人员提供的 `camera-toolbox` 程序。若使用命令行启动，进入程序所在目录后执行：

```bash
./camera-toolbox
```

如果工程人员提供的是 `target/release/camera-toolbox` 或 `target/debug/camera-toolbox`，也可以直接运行对应文件；本文不限定电脑端文件存放路径。

### 4.2 切到标定工作区

在 GUI 顶部选择：

```text
Calibration
```

左侧 Workspace Explorer 选择：

```text
X5_233_Driver
```

### 4.3 填写 X5 信息

在 `Device / Control` 中填写：

| 项 | 填写 |
|---|---|
| Host / IP | X5 板端 IP，例如 `10.21.12.108` |
| TCP port | `9073` |

![image-20260810100115590](/Users/sosilent/Library/Application Support/typora-user-images/image-20260810100115590.png)

点击：

```text
Read TCP Status
```

成功时，下面 `TCP Status` 会显示协议、RTSP 通道和 ring 状态。

![image-20260810100220384](/Users/sosilent/Library/Application Support/typora-user-images/image-20260810100220384.png)

### 4.4 打开 RTSP 预览

在 `RTSP Preview` 中保持默认：

| 项 | 推荐值 |
|---|---:|
| Configure encoder before RTSP connect | 勾选 |
| Width | `1920` |
| Height | `1080` |
| FPS | `60` |
| Bitrate kbps | `12000` |
| Channels | 勾选 CH0 和 CH3 |

点击：

```text
Connect selected RTSP
```

成功后，Active Streams 会出现 CH0/CH3 live stream，画面开始刷新。

---

## 5. 标定前准备

在 `Intrinsic Calibration` 区域确认棋盘参数：

| 项 | 含义 |
|---|---|
| Inner corners | 棋盘内角点数量，例如 `11 × 8`，按实际棋盘填写 |
| Square size (mm) | 每个格子的边长，单位 mm |

填写后点击：

```text
Apply board
```

初始内参建议先保持：

```text
Auto initial intrinsics
```

这样软件会自动使用：

- `fx = fy = 900 px`
- `cx = width / 2`
- `cy = height / 2`
- `D12 = 0`

如果已有上一轮好结果，可在成功标定后点击：

```text
Use result as initial K+D12
```

它只把当前结果回填为下一轮自动快门的初值，不会写 EEPROM。

---

## 6. 自动快门：两个模式怎么选

打开：

```text
Auto Capture
```

里面有两个 `Trigger mode`。

### 6.1 Calibration session：每路流独立

`Calibration session` 是一组独立的标定数据集。每个 session 都有自己的：

- Dataset 图片列表；
- Auto Capture 开关和模式；
- Gain / Guided 状态；
- 标定结果和导出状态。

当前 GUI 支持多个 Calibration session 同时存在：

```text
Manual / Files          # 手动导入文件用
X5_233_Driver CH0       # CH0 live stream 独立 session
X5_233_Driver CH3       # CH3 live stream 独立 session
```

创建 CH0 / CH3 session 的常用方式：

1. 先在 `Active Streams` 里确认 CH0 / CH3 都已经有画面；
2. 保持顶部工作区为 `Calibration`；
3. 分别在 CH0、CH3 的 `Actions` 列点击 `Capture` 一次；
4. 上方出现 `Calibration session:` 标签后，可以在 `... CH0` 和 `... CH3` 之间切换。

重要规则：

- CH0 和 CH3 是两个独立 session，互不共用 Dataset；
- 切换标签不会停止另一个 session 的自动快门；
- 只要对应 live stream 还在刷新、对应 session 已开启自动快门，它就可以继续取图；
- 本指南按 X5_233 双目流程说明：可以同时自动快门取图的 live session 是 `2` 个，CH0 一个、CH3 一个。

Dataset gain 模式下，CH0 和 CH3 可以同时开启：

```text
CH0 session: Trigger mode = Dataset gain, Enable Dataset-gain auto capture = 勾选
CH3 session: Trigger mode = Dataset gain, Enable Dataset-gain auto capture = 勾选
```

这样移动棋盘时，两个 session 会分别按各自画面的 Gain 判断是否入库。CH0 达标只写入 CH0 Dataset，CH3 达标只写入 CH3 Dataset。

Guided preset pose 也属于单个 session 的状态。实际操作中建议先完成一路，再切到另一路重新 Start guided，避免操作员同时跟两套引导姿态。

### 6.2 Dataset gain：自由移动采集

选择：

```text
Trigger mode = Dataset gain
Enable Dataset-gain auto capture = 勾选
```

适合：

- 操作员知道要补哪些画面；
- 想快速采集一组数据；
- 不想一步一步跟引导框。

工作原理：

```text
软件看到一帧 RTSP 画面
  └── 检测棋盘
      └── 估算棋盘姿态 PnP
          └── 计算这张图对当前数据集有多少新增价值 Gain
              ├── Gain 不够：丢弃，不入库
              └── Gain 足够：通过 X5 TCP 抓同源 YUV，再确认并入库
```

默认只有候选帧的 `Gain >= 0.3` 才会自动入库。

### 6.3 Guided preset pose：按引导姿态采集

选择：

```text
Trigger mode = Guided preset pose
Start guided
```

适合：

- 需要按固定步骤操作；
- 需要标准化采集；
- 需要补齐边缘、四角、远近、倾斜姿态。

工作原理：

```text
软件显示当前目标姿态 Step
  └── 操作员移动棋盘到引导框附近
      └── 软件检测棋盘姿态误差
          └── 姿态误差达标后开始 Hold
              └── 连续稳定 4 帧且抖动不过大
                  └── 选 Hold 中最稳定的一帧入库
```

Guided 模式不看 `Minimum auto Gain`，它看的是“是否对准当前引导姿态”。

界面上会看到类似：

```text
Step 3 / 45 · Upper right · mid tilt
Pose error 0.42/1.00 · hold 2/4
```

含义：

- `Pose error` 小于 `1.00`：姿态合格；
- `hold 2/4`：已经稳定 2 帧，还要继续保持；
- 到 `hold 4/4` 后软件自动抓最稳的一帧。

---

## 7. Gain 计算方式

软件希望标定图不要都挤在中间、同一距离、同一角度。因此每张图分三项给分：

| 分数 | 看什么 | 操作上怎么提高 |
|---|---|---|
| Field Gain | 棋盘角点覆盖画面哪些区域 | 把棋盘移到边缘、四角，不要只拍中心 |
| Depth Gain | 棋盘离相机的远近是否多样 | 拍近一点、远一点 |
| Pose Gain | 棋盘倾斜方向和角度是否多样 | 上下左右倾斜，不要全正对镜头 |

真实计算按“目标区域是否还缺样本”来算，不是简单看棋盘位置是否不同。每个区域都有目标次数，已经达到目标的区域会封顶，后续同区域样本不再增加 Gain。

定义：

| 符号 | 含义 |
|---|---|
| `N_corner` | 棋盘内角点总数，即 `Inner corners` 的列数 × 行数 |
| `C_r` | 某个区域 `r` 在当前 Dataset 中已经累计的数量 |
| `T` | 该区域目标数量，例如 `Field target / cell`、`Depth target / bin`、`Pose target / bin` |
| `I_r` | 当前候选图给区域 `r` 带来的新增数量 |

单个区域的原始增益：

```text
raw_gain_r = min(C_r + I_r, T) - min(C_r, T)
```

整张候选图的三项增益：

```text
Field Gain = sum(raw_field_gain_r) / N_corner
Depth Gain = sum(raw_depth_gain_r) / N_corner
Pose Gain  = sum(raw_pose_gain_r)
```

最终用于 Dataset gain 自动入库的分数：

```text
Gain = (Field Gain + Depth Gain + Pose Gain) / 3
```

三项含义：

- Field：把图像分成 `field_columns × field_rows` 网格；当前候选图中每个棋盘角点落入哪个网格，就给该网格增加对应角点数；达到 `Field target / cell` 后封顶。
- Depth：用 PnP 估计每个棋盘角点在相机坐标系下的 Z 深度，落入哪个 depth bin 就给该 bin 增加角点数；达到 `Depth target / bin` 后封顶。
- Pose：用 PnP 估计棋盘法向的 tilt / azimuth，落入一个 pose bin；该图给这个 pose bin 增加 `1` 个 view；达到 `Pose target / bin` 后封顶。

关键规则：

- Field / Depth 按 `N_corner` 归一化；Pose 按 view 计数，不除以角点数。
- 某个区域已经够了，再拍同样位置不会继续加分。
- PnP 必须能得到当前 K/D 绑定下的有限姿态，且棋盘角点深度为正；Depth / Pose 不再使用 RMSE 或最大单点重投影误差门限。
- `Dataset gain` 模式用这个 Gain 决定是否自动入库，默认要求 `Gain >= 0.3`。
- `Guided preset pose` 模式不按 Gain 入库，而按引导姿态和稳定 Hold 入库。

默认 Dataset Acceptance：

| 项 | 默认值 |
|---|---:|
| Field grid | `16 × 9` |
| Field target / cell | `1` |
| 最小相邻角点距离 | `12 px` |
| Depth range | `400–2400 mm` |
| Depth bins | `4` |
| Tilt deadband | `5°` |
| Tilt max | `65°` |
| Tilt bins | `3` |
| Azimuth sectors | `8` |
| Minimum auto Gain | `0.3` |

操作建议：

1. 先用 Guided 跑一轮完整采集；
2. 如果 Dataset Acceptance 仍显示缺口，再切 Dataset gain 自由补拍；
3. 优先补红/黄提示的边缘、角点、远近和倾斜方向。

---

## 8. 执行标定并检查结果

当 Dataset 里有足够多 `Found` 图片后，点击：

```text
Calibrate
```

完成后查看：

- `RMS xxxx px`
- `Calibration result`
- 每张图的重投影误差；
- Heatmap / Dataset Acceptance 覆盖状态。

判断原则：

- RMS 越小通常越好，但不能只看 RMS；
- 还要看边缘/四角是否有覆盖；
- 还要看远近和倾斜是否有覆盖；
- 某些单张图误差明显大时，禁用或删除后重新 Calibrate。

可以导出：

| 按钮 | 结果 |
|---|---|
| Export JSON | 完整审计结果，包含 Dataset 和完整 D12 |
| Export YAML Result | OpenCV 风格结果，包含完整 D12 |

注意：EEPROM 不保存完整 D12。EEPROM 只写 D8：

```text
D_JSON / D_YAML = k1,k2,p1,p2,k3,k4,k5,k6,s1,s2,s3,s4
D_EEPROM        = k1,k2,p1,p2,k3,k4,k5,k6, 0, 0, 0, 0
```

因此 EEPROM 结果不能直接等同于 YAML 完整结果。

---

## 9. EEPROM 写入步骤

危险提示：EEPROM 写入会修改物理模块。写入过程中不要断电、不要拔线、不要断 SSH。

### 9.1 写入前条件

必须同时满足：

1. Calibration 已经成功得到当前结果，或已经 `Load Result` 加载 YAML；
2. 左侧 Explorer / X5_233_Driver 有可用 SSH/SFTP 控制源；
3. 已确认目标 I²C bus；
4. SNID 信息填写完整。

### 9.2 填 SNID

在 `EEPROM Provisioning` 中填写：

| 项 | 说明 |
|---|---|
| Module | 通常选 `233` |
| Ship date | 年/月/日，例如年填 `26` |
| Optical axis class | 未分类可选 `0 - unclassified`；量产按实际分类选 L0/L1/R0/R1 |
| Sequence | 十进制序号 |

确认界面显示：

```text
Converted SNID <生成的序列号>
```

### 9.3 选择 EEPROM 目标

在 `EEPROM SSH Target` 中：

1. 确认 `SSH/SFTP control` 是当前 X5；
2. 等待或点击 `Refresh bus list`；
3. 选择正确 `I²C bus`；
4. 点击 `Use SSH/SFTP control for EEPROM`。

软件会在每次 EEPROM 操作前上传 helper 到：

```text
/usr/local/libexec/camera-toolbox-eeprom-helper
```

### 9.4 先读 EEPROM

点击：

```text
Read
```

读成功后检查：

- `Device: FLAG=...`
- `SN=...`
- `SHA-256: ...`
- `EEPROM read fields`

如果已有不同 SN，界面会要求勾选：

```text
I confirm replacing the existing different or damaged serial number
```

确认要覆盖时才勾。

### 9.5 选择写入模式

| 模式 | 用途 |
|---|---|
| Full provision | 首次写入整份 EEPROM，包括 SN 和标定数据 |
| Update calibration only | 只更新标定数据，不改已有 SN |

新模块通常用：

```text
Full provision
```

已有 SN 且只换标定结果时用：

```text
Update calibration only
```

### 9.6 确认写入

点击：

```text
Write...
```

弹窗 `Confirm EEPROM write` 出现后，最后确认：

- Target 是目标板；
- Serial 是要写入的 SNID；
- Expected before 是刚刚读取到的 SHA-256。

确认无误后点击：

```text
Write and verify
```

写入前软件会先检查本机程序工作目录下的写入记录目录：

```text
write_history/
```

规则：

- 每次 EEPROM 写入成功后，都会生成一份 `write_history/*.yaml` 记录文件；
- 记录内容包含目标板、I²C bus、SNID、写入模式、写入字段、写入前后 SHA-256、校验结果等信息；
- 如果写入过程中 helper 返回失败，软件也会尽量保存失败审计记录；
- 再次写入前，软件会扫描既有 `write_history/*.yaml` / `*.json`；
- 如果发现已有记录里的 SNID 与本次 SNID 完全相同，软件会拒绝开始 EEPROM 写入，避免同一 SNID 被重复烧录；
- 如果记录文件名已被占用但内容不是同一个 SNID，软件也会拒绝写入，需先让工程人员归档或修复该记录文件。

因此，看到“已有 write history”类错误时，不要删除记录后强行重写；先确认是否真的需要重新烧录同一模块。

成功状态：

```text
EEPROM write and bytewise verification succeeded; write history saved as <audit_file>.
```

如果出现 UNKNOWN 状态，不要重试；先重新 Read，保留日志，再让工程人员处理。

---

## 10. 常见问题

### 10.1 Read TCP Status 失败

检查板端：

```bash
ps | grep DEMO233_TCP_08_10
ss -lntp | grep 9073
```

常见原因：

- `DEMO233_TCP_08_10` 没启动；
- IP 填错；
- TCP port 不是 `9073`；
- 网络不通。

### 10.2 RTSP 连接失败

检查板端：

```bash
ss -lntp | grep -E '554|557'
```

GUI 里先点：

```text
Read TCP Status
```

再点：

```text
Connect selected RTSP
```

如果还不行，先点 `Stop selected RTSP`，再重新连接。

### 10.3 Dataset gain 不自动拍

通常是 Gain 不够。操作上这样补：

- 棋盘移到画面边缘；
- 棋盘移到四角；
- 改变远近；
- 上下左右倾斜；
- 避免一直拍同一位置。

### 10.4 Guided hold 到不了 4/4

通常是姿态没对准或手抖：

- 看 `Pose error` 是否小于 `1.00`；
- 保持棋盘静止；
- 避免反光、模糊、遮挡；
- 不要在 Hold 期间移动棋盘。

### 10.5 EEPROM Write 按钮不可点

逐项检查：

- 是否已有标定结果；
- SNID 是否完整；
- 是否已经选定 SSH/SFTP 控制源；
- 是否已经选择 I²C bus；
- 是否已经先 Read；
- 如果已有不同 SN，是否勾选覆盖确认。

---

## 11. 操作员最短流程清单

```text
1. 板端安装 /opt/DEMO233_TCP_08_10，不覆盖 /opt/DEMO233
2. 板端启动：SC233_CALIBRATION_MODE=1 ./DEMO233_TCP_08_10
3. GUI 打开 camera-toolbox
4. 选择 Calibration + X5_233_Driver
5. 填 X5 IP，Read TCP Status
6. 选 CH0/CH3，Connect selected RTSP
7. 填棋盘 Inner corners / Square size，Apply board
8. Auto Capture 选择 Guided preset pose，Start guided
9. 按引导移动棋盘，等每步自动入库
10. Dataset 足够后点 Calibrate
11. 检查 RMS、覆盖、异常帧
12. Export JSON / YAML 备份结果
13. EEPROM Provisioning 填 SNID、选 I²C bus、Read
14. Write... → Confirm EEPROM write → Write and verify
```
