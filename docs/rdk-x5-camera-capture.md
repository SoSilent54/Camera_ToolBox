# RDK X5 摄像头 RAW、YUV 与视频流采集

## 目的与范围

本文说明如何在地瓜机器人 RDK X5 上控制 MIPI 摄像头数据链路，并获取：

- ISP 前的 Sensor RAW；
- ISP 后的 NV12 YUV；
- 连续 YUV 帧；
- H.264 / H.265 等编码码流；
- 需要网络传输时可采用的官方示例入口。

本文只讨论采集、编码和传输链路，不讨论 AE、AWB、CCM、LSC 等 ISP 参数调节。

本文同时使用两组版本边界：公开 `rdk_doc` 的 RDK X5 多媒体 API 概述以 RDK X5 3.5.0 为基线；本机离线 SDK 手册固定为 `user_manual_v1.0.20`。后者的 `_sources/` 原文用于核对 HBN、MediaCodec、Platform SDK 样例和 Sunrise Camera。两组资料与当前公开源码可能不同，因此本文对源码命令另行锁定 commit；实际设备仍以板端 `--help`、安装头文件、`/etc/board_config.json`、Media Controller 拓扑和 Sensor 配置为最终依据。

## 先区分三种输出

```text
MIPI Sensor ──► VIN / SIF              (ISP 前，RAW8/10/12/14/16 或 Sensor YUV422)
                   │
                   ▼
                  ISP                  (ISP 后，通常为 NV12 YUV)
                   │
                   ▼
                  VSE                  (裁剪/缩放后的 NV12 YUV)
                   │
                   ▼
             VENC / Encoder            (H.264/H.265/MJPEG 编码码流)
                   │
                   ├── 本地 elementary stream 文件
                   └── 应用层封装为 WebSocket / RTSP 等网络传输
```

| 目标 | 数据位置 | 常用入口 | 结果 |
|---|---|---|---|
| RAW 单帧/多帧 | VIN / SIF | `vio_capture`、VIN V4L2 节点、`get_vin_data`、`sp_vio_get_raw` | `.raw` |
| ISP 后 YUV | ISP | `vio_capture`、ISP V4L2 节点、`get_isp_data`、`sp_vio_get_yuv` | NV12 `.yuv` |
| 缩放后 YUV | VSE | `sp_vio_get_frame`、`single_pipe_vin_isp_vse` | NV12 `.yuv` 或内存帧 |
| 编码视频 | VENC | `vio2encoder`、Encoder API、`single_pipe_vin_isp_vse_vpu` | `.h264` / `.h265` |
| 浏览器网络画面 | ISP/VSE + Encoder + WebSocket | `09_web_display_camera_sample` | WebSocket 消息，不等同于 RTSP |
| 实时 RTSP | VENC + RTSP Server | `user_manual_v1.0.20` 的 Sunrise Camera | `rtsp://<X5_IP>/stream_chn0.h264` |

## 命令与 API 的使用层级

| 层级 | 是否直接运行 | 适用场景 |
|---|---|---|
| `/app/cdev_demo/vio_capture` | 先在板端 `make`，然后直接运行 | 最快同时获得 RAW 和 YUV |
| `v4l2-ctl` | 配置 V4L2 模式后直接运行 | shell 脚本化抓帧、确认 VIN/ISP/VSE 节点 |
| `/app/pydev_demo/...` | Python 脚本直接运行 | 快速抓 NV12 YUV、持续处理和显示 |
| `hobot-spdev` C/Python API | 需要编写或修改程序 | 集成到业务应用 |
| `x5-multimedia-samples` HBN API | 需要在匹配 X5 SDK 环境编译 | 精确控制 VIN/ISP/VSE/VENC、获取 stride/plane 元数据 |

不要把 API 名称写成 shell 命令。`sp_vio_get_raw`、`hbn_vnode_getframe` 等只能由程序调用；`vio_capture`、`v4l2-ctl` 才是可运行命令。

## 1. 板端准备

### 1.1 硬件和 Sensor 配置

1. 开发板断电。
2. 将官方已适配的 MIPI Sensor 接入正确的 CAM 接口。
3. 上电并通过 SSH 或串口登录。
4. 确认系统镜像、`hobot-spdev`、Sensor 驱动和 Sensor 配置来自同一软件版本。

记录板端版本：

```bash
cat /etc/os-release
dpkg-query -W hobot-spdev 2>/dev/null || true
```

自动探测失败时，检查：

- `/etc/board_config.json` 中的 camera host；
- 启动日志里的 Sensor 名、I2C bus、MIPI RX、配置文件名；
- Sensor 配置中的分辨率、帧率、RAW 有效位深、lane 数和 linear / WDR 模式。

### 1.2 HBN 模式与 V4L2 模式

RDK X5 支持：

- HBN 常规模式：Camera + VIN / ISP / VSE vnode + vflow；
- V4L2 `sif-isp-vse` 模式：通过 `/dev/video*` 获取 VIN、ISP、VSE 数据；
- V4L2 `vse alone` 模式：向 VSE 回灌 NV12。

切换 V4L2 Camera 模式：

```bash
sudo srpi-config
```

依次进入：

```text
3 Interface Options
└── I7 V4L2
    └── I1 V4L2 Enable/disable V4L2 interface for camera
```

配置 CAM0 / CAM1 和 Sensor 后重启。`hobot-spdev` 示例会按当前驱动模式选择 HBN 或 V4L2 路径，但日志可能不同。

## 2. 最快路径：`vio_capture` 同时抓 RAW 和 YUV

这是官方针对 RDK X5 给出的现成示例。它打开一条 `VIN -> ISP -> VSE` 通路，并将 RAW 和 YUV 分别保存到当前目录。

### 2.1 编译和运行

先查看当前镜像中程序的参数：

```bash
cd /app/cdev_demo/vio_capture
sudo make
./capture --help
```

官方 RDK X5 1080p 示例：

```bash
sudo ./capture -b 16 -c 10 -h 1080 -w 1920
```

参数：

| 参数 | 含义 |
|---|---|
| `-w` | Sensor 输出宽度 |
| `-h` | Sensor 输出高度 |
| `-c` | 保存帧数 |
| `-b` | 示例使用的 RAW 存储/缓冲位宽参数；X5 官方 IMX219 / IMX477 / OV5647 示例传 16 |

运行成功后得到：

```text
raw_0.raw ... raw_9.raw
yuv_0.yuv ... yuv_9.yuv
```

1920×1080 官方示例中：

- RAW 文件大小为 `4,147,200 = 1920 × 1080 × 2` 字节；
- YUV 文件大小为 `3,110,400 = 1920 × 1080 × 3 / 2` 字节。

### 2.2 `-b 16` 不表示 Sensor 有 16 个有效位

官方 X5 示例日志同时给出：

```text
config_file: linear_1920x1080_raw10_30fps_2lane.c
input_bit_width: 10
raw file size: 1920 × 1080 × 2 bytes
```

这表示该场景中 Sensor 有效数据是 RAW10，但抓取文件使用每像素 2 字节的存储容器。读取时必须区分：

- Sensor 有效位深：例如 RAW10；
- 文件存储容器：例如 16-bit / pixel；
- MIPI packed RAW 与 DDR unpacked RAW；
- 高有效位还是低有效位对齐；
- 行 stride 是否等于 `width × bytes_per_pixel`；
- 端序和 Bayer Pattern。

不能仅根据 `-b 16` 或 `.raw` 文件大小，把数据解释为 16-bit 有效亮度。

### 2.3 YUV 格式

`vio_capture` 的 YUV 是 ISP 处理后的 NV12：

```text
plane 0: Y，width × height bytes
plane 1: 交错 UV，width × height / 2 bytes
```

可在主机上用 FFmpeg 工具预览：

```bash
ffplay -f rawvideo -pixel_format nv12 -video_size 1920x1080 yuv_0.yuv
```

该命令只负责查看裸 NV12，不会补充色彩矩阵、full / limited range 等元数据。做定量 ISP 评价时必须另行记录这些条件。

## 3. V4L2 命令抓取

V4L2 方式适合 shell 脚本和自动化采集。官方 RDK X5 文档给出的开启流程只适用于带 `srpi-config` 的 RDK 镜像：

```text
srpi-config
  -> 3 Interface Options
  -> I7 V4L2
  -> I1 V4L2 Enable/disable V4L2 interface for camera
  -> 选择 sif-isp-vse
  -> 配置 CAM0 / CAM1 Sensor
  -> 重启板卡
```

重启后先验证驱动和节点注册，再尝试采集：

```bash
lsmod
test -e /dev/media0
set -- /sys/class/video4linux/video*
[ -e "$1" ] || { echo "no V4L2 video nodes" >&2; exit 1; }
media-ctl -d /dev/media0 -p
v4l2-ctl --list-devices
v4l2-ctl -d /dev/video0 --all
```

`/dev/video*` 必须由内核 V4L2 驱动注册，手工 `mknod` 不能把 HBN/native vnode 变成 V4L2 设备。官方页面没有给出“缺少 `srpi-config` 的 Buildroot 镜像”对应的手工配置命令；本次实测设备也没有 `srpi-config`、`hobot-config` 或 `/app/cdev_demo/v4l2`，不能把上述菜单流程外推为该设备可直接执行的步骤。离线 `user_manual_v1.0.20` 中检索到的 `/dev/video0` 是 USB UVC gadget 输出节点，不是 VIN/ISP capture。

启用 `sif-isp-vse` 模式并重启后，仍须确认实际拓扑，不能直接假设节点编号。

### 3.1 核对拓扑和节点

```bash
media-ctl -d /dev/media0 -p
v4l2-ctl --list-devices
```

需要图形化拓扑时：

```bash
media-ctl -d /dev/media0 --print-dot > media0.dot
dot -Tpng media0.dot -o media0.png
```

官方默认映射示例：

| CSI | VIN / SIF | ISP | VSE0 |
|---|---|---|---|
| CSI0 | `/dev/video0` | `/dev/video4` | `/dev/video8` |
| CSI1 | `/dev/video1` | `/dev/video5` | `/dev/video14` |
| CSI2 | `/dev/video2` | `/dev/video6` | `/dev/video20` |
| CSI3 | `/dev/video3` | `/dev/video7` | `/dev/video26` |

该表是官方默认映射，不替代板端 `media-ctl` / `v4l2-ctl --list-devices` 的实测结果。

### 3.2 抓 ISP 后 NV12 YUV

官方对 CSI0 ISP 节点给出的命令：

```bash
v4l2-ctl --list-formats-ext --device /dev/video4

v4l2-ctl -d /dev/video4 \
  --set-fmt-video=width=640,height=480,pixelformat=NV12 \
  --stream-mmap=3 \
  --stream-skip=3 \
  --stream-to=/tmp/nv12.yuv \
  --stream-count=1 \
  --stream-poll
```

说明：

- 第一次 `--list-formats-ext` 同时用于确认格式/尺寸并触发驱动初始化；
- `--stream-skip=3` 丢弃启动初期 3 帧；
- `--stream-count=1` 保存 1 帧；
- 连续保存可增大 `--stream-count`，输出文件将按帧连续拼接。

### 3.3 抓 VIN RAW

VIN 节点通常是 RAW，但 Sensor 对应的 FourCC、尺寸和存储布局不同。必须先查询实际 VIN 节点：

```bash
v4l2-ctl --list-formats-ext --device /dev/video0
v4l2-ctl --device /dev/video0 --all
```

然后将查询结果中的精确宽、高和 FourCC 填入以下模板：

```bash
# 模板：<...> 必须替换为板端查询结果，不能原样执行
v4l2-ctl -d <VIN_NODE> \
  --set-fmt-video=width=<WIDTH>,height=<HEIGHT>,pixelformat=<FOURCC_FROM_QUERY> \
  --stream-mmap=3 \
  --stream-skip=3 \
  --stream-to=/tmp/vin.raw \
  --stream-count=1 \
  --stream-poll
```

禁止把 ISP 示例中的 `pixelformat=NV12` 直接照搬到 VIN RAW 节点。抓取后同时保存 `v4l2-ctl --all`、Sensor 配置名和文件字节数。

### 3.4 官方 V4L2 C 示例

源码形式的 V4L2 示例：

```bash
cd /app/cdev_demo/v4l2
sudo make
./v4l2 -w 1920 -h 1080 -c 10 -n 4
```

其中 `-n 4` 表示使用 video node 4。运行前仍应确认当前拓扑中 node 4 是否为目标 ISP 节点。

## 4. Python 快速抓 ISP 后 YUV

官方 RDK X5 Python 示例位于：

```text
/app/pydev_demo/08_mipi_camera_sample/
```

抓取 10 帧 1920×1080、30 fps 的 YUV：

```bash
cd /app/pydev_demo/08_mipi_camera_sample
python 02_mipi_camera_dump.py -f 30 -c 10 -w 1920 -h 1080
```

输出是 NV12 YUV 文件。该脚本不是 RAW 抓取入口。

常用 Python API：

```text
from hobot_vio import libsrcampy

libsrcampy.Camera()
Camera.open_cam(...)
Camera.get_img(module, width, height)   -> NV12
Camera.close_cam()
```

官方文档和不同版本示例对 `get_img` 可选参数的写法存在差异。应用开发应直接参考板端安装的示例和模块签名，不要仅依赖离线文档复制调用参数。

## 5. `hobot-spdev` C API

### 5.1 VIO 接口

以下签名已对照官方 `x5-hobot-spdev` 提交 [`506ec790a80c01ebe552929e6efb572d562b3b14`](https://github.com/D-Robotics/x5-hobot-spdev/commit/506ec790a80c01ebe552929e6efb572d562b3b14) 中的 `src/clang/sp_vio.h`；目标板安装头文件优先级更高。接口子集如下：

```c
void *sp_init_vio_module(void);
int32_t sp_open_camera(void *obj, int32_t pipe_id,
                       int32_t video_index, int32_t chn_num,
                       int32_t *width, int32_t *height);
int32_t sp_open_camera_v2(void *obj, int32_t pipe_id,
                          int32_t video_index, int32_t chn_num,
                          sp_sensors_parameters *parameters,
                          int32_t *width, int32_t *height);
int32_t sp_vio_get_raw(void *obj, char *frame_buffer,
                       int32_t width, int32_t height, int32_t timeout);
int32_t sp_vio_get_yuv(void *obj, char *frame_buffer,
                       int32_t width, int32_t height, int32_t timeout);
int32_t sp_vio_get_frame(void *obj, char *frame_buffer,
                         int32_t width, int32_t height, int32_t timeout);
int32_t sp_vio_close(void *obj);
void sp_release_vio_module(void *obj);
```

调用关系：

```text
sp_init_vio_module
   ▼
sp_open_camera / sp_open_camera_v2
   ├── sp_vio_get_raw    (VIN RAW)
   ├── sp_vio_get_yuv    (ISP NV12)
   └── sp_vio_get_frame  (VSE 指定分辨率 NV12)
   ▼
sp_vio_close
   ▼
sp_release_vio_module
```

约束：

- `video_index=-1` 表示自动探测；多相机时应按 `/etc/board_config.json` 和拓扑显式选择；
- `sp_open_camera_v2` 可通过 `sp_sensors_parameters` 指定 RAW width、height 和 fps；
- `sp_vio_get_frame` 的 width / height 必须是打开 Camera 时配置过的 VSE 输出；
- NV12 理论有效数据大小为 `width × height × 3 / 2`；
- X5 编码输入尺寸要求 16 对齐；图像 stride/plane size 仍应以实际帧元数据或官方样例为准；
- 官方 `sp_vio_get_raw` 文档对整数 width/height 参数写了“传 NULL”，与函数原型不一致。不要复制该表述；以当前安装头文件和 `vio_capture` 源码为准。

`sp_vio_get_raw` 只接收调用者提供的裸指针，不返回实际 plane size/stride。生产工具若需要可靠处理多 Sensor、多位深和 WDR，优先使用下一节 HBN API，因为 `hbn_vnode_image_t` 明确携带 format、stride、plane size 和时间戳。

## 6. HBN vnode API：精确控制 VIN、ISP、VSE

本节命令锁定到官方仓库提交 [`0d9925cef6c41056525ae27383d2d3604f9e4690`](https://github.com/D-Robotics/x5-multimedia-samples/commit/0d9925cef6c41056525ae27383d2d3604f9e4690)。该快照的实际 Git tree、各目录 Makefile `TARGET` 和源码 `print_help()` 对应：

| 功能 | 锁定目录 | Makefile 目标 |
|---|---|---|
| VIN RAW | `sample_vin/get_vin_data` | `get_vin_data` |
| ISP YUV | `sample_isp/get_isp_data` | `get_isp_data` |
| VSE YUV | `sample_pipeline/single_pipe_vin_isp_vse` | `single_pipe_vin_isp_vse` |
| VSE + VPU | `sample_pipeline/single_pipe_vin_isp_vse_vpu` | `single_pipe_vin_isp_vse_vpu` |

同一提交的根 `README.md` 仍写着 `sif/read_raw_data`、`isp/read_isp_data`、`pipelines/...` 和每 60 帧自动保存，和实际 tree 不一致：前两条路径在该提交中不存在；RAW/ISP 实际源码是 `g/l/f/q` 交互，VSE 管线才是每 60 帧自动保存。以下可执行步骤以锁定提交的实际 tree、Makefile 和源码为准，不以滞后的根 README 为准。

在匹配 X5 Platform SDK 的环境中检出该快照：

```bash
git clone https://github.com/D-Robotics/x5-multimedia-samples.git
cd x5-multimedia-samples
git checkout --detach 0d9925cef6c41056525ae27383d2d3604f9e4690
git rev-parse HEAD
make
```

具体可执行文件也可进入下述对应目录单独执行 `make`。Sensor 索引不是固定常量；先运行程序的 `-h`，按该二进制列出的当前 Sensor 列表选择。
若改用其他 commit、tag 或板端预装副本，先重新核对实际 tree、Makefile 目标和程序 `-h`，不要混用本节路径与其他版本 README。

### 6.1 VIN RAW

```bash
cd sample_vin/get_vin_data
make
./get_vin_data -h
sudo ./get_vin_data -s <SENSOR_INDEX>
```

运行后的交互命令：

| 输入 | 行为 |
|---|---|
| `g` | 每路 Sensor 保存 1 帧 |
| `l` | 每路 Sensor 连续保存 12 帧 |
| `f` | 检查实测帧率 |
| `h` | 显示帮助 |
| `q` | 退出 |

RAW 主流程：

```text
选择 sensor_config
   ▼
hbn_camera_create
   ▼
hbn_vnode_open(HB_VIN)
   ├── hbn_vnode_set_attr
   ├── hbn_vnode_set_ichn_attr
   ├── VIN output ddr_en = 1
   ├── hbn_vnode_set_ochn_attr
   └── hbn_vnode_set_ochn_buf_attr
   ▼
hbn_camera_attach_to_vin
   ▼
hbn_vflow_start
   ▼
hbn_vnode_getframe(VIN, channel 0)
   ├── 读取 format / width / height / stride / size / frame_id / timestamp
   └── 保存 RAW plane
   ▼
hbn_vnode_releaseframe
```

`hbn_vnode_releaseframe` 不能遗漏，否则缓冲池会被逐渐占满，后续抓帧超时。

### 6.2 ISP 后 NV12

```bash
cd sample_isp/get_isp_data
make
./get_isp_data -h
sudo ./get_isp_data -s <SENSOR_INDEX>
```

交互命令：

- `g`：保存一帧 ISP YUV；
- `l`：保存 12 帧；
- `q`：退出。

API 主循环：

```c
hbn_vnode_image_t image = {0};

ret = hbn_vnode_getframe(isp_node, 0, timeout_ms, &image);
if (ret == 0) {
    /* 使用 image.buffer.virt_addr[0/1]、size[0/1]、stride */
    hbn_vnode_releaseframe(isp_node, 0, &image);
}
```

输出文件名包含：

- ISP handle 和 channel；
- width、height、stride；
- frame id；
- timestamp。

这些元数据应进入 Camera Toolbox 的采集记录，而不是只保留裸 `.yuv` 文件。

### 6.3 VSE 连续 YUV

```bash
cd sample_pipeline/single_pipe_vin_isp_vse
make
./single_pipe_vin_isp_vse -h
sudo ./single_pipe_vin_isp_vse -s <SENSOR_INDEX>
```

该样例建立 `VIN -> ISP -> VSE`，持续调用：

```text
hbn_vnode_getframe(VSE channel 0..N)
   ▼
消费/保存 NV12 plane
   ▼
hbn_vnode_releaseframe(VSE channel 0..N)
   ▼
循环
```

官方源码默认每 60 帧保存一次各 VSE 输出通道的 NV12 文件。示例支持通过 `-c` 选择 online/offline 拓扑：

| 值 | 含义 |
|---|---|
| `vo` | VIN online ISP |
| `vf` | VIN offline ISP |
| `io` | ISP online VSE |
| `if` | ISP offline VSE |
| `vo:io` 等组合 | 同时指定两段连接方式 |

未明确需要时使用样例默认值，不应为追求低延迟盲目切换拓扑；online/offline 会影响 DDR 输出、可抓取节点和缓冲配置。

## 7. 获取编码视频码流

### 7.1 直接命令：`vio2encoder`

这是最简单的 Camera -> H.264 文件路径：

```bash
cd /app/cdev_demo/vio2encoder
sudo make
sudo ./vio2encoder \
  -w 1920 -h 1080 \
  --iwidth 1920 --iheight 1080 \
  -o stream.h264
```

参数：

| 参数 | 含义 |
|---|---|
| `--iwidth/--iheight` | Sensor 输入尺寸 |
| `-w/-h` | 编码输出尺寸 |
| `-o` | 码流文件路径 |

程序建立：

```text
Camera / VIO ──► ISP ──► VSE ──► Encoder ──► stream.h264
```

`stream.h264` 是 H.264 elementary stream，不是 MP4 文件，也不是 RTSP 地址。它通常不携带容器时间戳、音频或完整颜色元数据。

### 7.2 `hobot-spdev` 编码 API

```c
void *sp_init_encoder_module(void);
int32_t sp_start_encode(void *obj, int32_t chn, int32_t type,
                        int32_t width, int32_t height, int32_t bits);
int32_t sp_encoder_set_frame(void *obj, char *nv12, int32_t size);
int32_t sp_encoder_get_stream(void *obj, char *stream_buffer);
int32_t sp_stop_encode(void *obj);
void sp_release_encoder_module(void *obj);
```

支持 `SP_ENCODER_H264`、`SP_ENCODER_H265` 和 `SP_ENCODER_MJPEG`。输入必须是与编码器配置尺寸一致的 NV12。

不需要调用者逐帧执行 `sp_encoder_set_frame` 的内部流转，可使用模块绑定：

```text
sp_init_vio_module + sp_open_camera
sp_init_encoder_module + sp_start_encode
   ▼
sp_module_bind(VIO, ENCODER)
   ▼
循环 sp_encoder_get_stream
   ▼
sp_module_unbind
   ▼
stop / close / release
```

`user_manual_v1.0.20` 的 MediaCodec API 原文定义 `bit_rate` 为 `hb_u32`，示例传 `5000` / `2000`，但没有声明单位；Sunrise Camera 用户指南只对该应用的 `encode_bitrate` 配置明确使用 `Kbps`。此外，公开 C API 页面写“Mbps”，Python 页面写 `kbps`，`x5-hobot-spdev` 头文件将参数命名为 `bits`。因此不能把 Sunrise Camera 的 UI 单位外推给 `sp_start_encode(bits)` 或 MediaCodec `bit_rate`：其数值语义和单位以当前镜像安装头文件、MediaCodec SDK 及板端输出码率实测为准；未经确认时只记录传入数值，不标注 `kbps` 或 `Mbps`。

### 7.3 HBN + Media Codec 示例

```bash
cd sample_pipeline/single_pipe_vin_isp_vse_vpu
make
./single_pipe_vin_isp_vse_vpu -h
sudo ./single_pipe_vin_isp_vse_vpu -s <SENSOR_INDEX>
```

官方源码流程：

```text
VSE hbn_vnode_getframe
   ▼
hb_mm_mc_dequeue_input_buffer
   ▼
复制/提交 NV12
   ▼
hb_mm_mc_queue_input_buffer
   ▼
hb_mm_mc_dequeue_output_buffer
   ▼
保存 H.264 bytes
   ▼
hb_mm_mc_queue_output_buffer
   ▼
hbn_vnode_releaseframe
```

默认源码创建 H.264 编码器，并写入 `single_pipe_vin_isp_vse_vpu.h264`。按 `Ctrl+C` 退出后检查文件是否包含 SPS/PPS/IDR，并使用解码器实际验证。

## 8. 连续帧、编码文件和网络流不是同一件事

### 8.1 连续 YUV 帧

应用循环调用 `sp_vio_get_frame`、`sp_vio_get_yuv` 或 `hbn_vnode_getframe`，得到的是未压缩 NV12 帧序列。它适合 ISP 分析和算法输入，但带宽高。

### 8.2 H.264/H.265 码流

`vio2encoder` 或 Encoder API 输出的是编码字节流。应用可以写文件、送入解码器，也可以交给网络服务器封装。

### 8.3 官方 WebSocket 浏览器示例

官方示例位置：

```bash
cd /app/pydev_demo/09_web_display_camera_sample
cd webservice
./sbin/nginx -p .
cd ..
python mipi_camera_web_yolov5x.py
```

浏览器访问：

```text
http://<RDK_X5_IP>/
```

该示例使用 Camera、JPEG 编码、检测结果序列化和 WebSocket 推送。它是应用层浏览器展示方案，不是通用 RTSP Camera 服务。

### 8.4 官方实时 RTSP：Sunrise Camera（`user_manual_v1.0.20`）

本地 `user_manual_v1.0.20` 明确给出 Camera 实时 RTSP 路径。Sunrise Camera 建立 Camera 采集、VIO、编码和 RTSP Server；该版本开发指南说明 RTSP Server 当前只支持 H.264。

部署包存在时，按该版本手册执行：

```bash
tar -xvf sunrise_camera_v3.0.0.tar.gz -C /app/
cd /app
sh ./start_app.sh
```

然后在 PC 浏览器访问 `http://<RDK_X5_IP>/`，选择“智能摄像机”，确认实际接入的 CSI / Sensor，设置编码参数并提交。Web 设备信息会显示真实 RTSP URL；该版本默认形式为：

```text
rtsp://<RDK_X5_IP>/stream_chn0.h264
```

PC 端可使用 VLC 打开该 URL。需要命令行录制时，可用标准 FFmpeg 客户端保存为带时间戳的容器：

```bash
ffmpeg -rtsp_transport tcp \
  -i rtsp://<RDK_X5_IP>/stream_chn0.h264 \
  -t 10 -c copy capture.mkv
```

`user_manual_v1.0.20` 建议 4K@30、高码率场景使用千兆网络；其中“8192 Kbps”是 Sunrise Camera 应用参数的说明，不代表底层 MediaCodec `bit_rate` 字段已在 API 中定义为相同单位。

该部署命令只适用于 `user_manual_v1.0.20` 对应的 `sunrise_camera_v3.0.0.tar.gz`。若当前镜像没有该包，不应假设 `/app/start_app.sh` 存在；应使用匹配镜像的软件包，或按 `sunrise_camera_develop_guide` 编译部署。

当前公开 `rdk_doc` 中另有 `rtsp2display` 拉流客户端；其配套 `live555MediaServer` 是把已有 `1080P_test.h264` 文件发布为 RTSP，不是 Camera 实时发布命令。自定义应用则仍需自行处理 Encoder 输出的时间戳、SPS/PPS/VPS、IDR 周期、断线重连和背压。

### 8.5 本机自动化、全程管道且不落盘

必须先区分板端 Camera ABI：只有 `/sys/class/video4linux` 非空、存在目标 `/dev/video*` capture 且 `v4l2-ctl --all` 成功时，才能使用 V4L2 stdout；HBN/native vnode 设备不能直接套用该命令。

| 需求 | 可用前提 | 推荐数据面 | 是否保留原始 ISP 数据 | 板端采集文件 |
|---|---|---|---|---|
| 预览、录像、视频算法 | Sunrise Camera RTSP 已启动 | 本机 FFmpeg 直接拉 RTSP | 否；经历有损编码/解码 | 无 |
| H.264 Annex-B 字节流 | Sunrise Camera RTSP 已启动 | RTSP -> 本机 FFmpeg `pipe:1` | 不适用 | 无 |
| ISP/VSE NV12 原帧 | 对应 V4L2 capture 节点已注册 | 板端 `v4l2-ctl --stream-to=-` -> `ssh -T` | 是 | 无 |
| VIN RAW 原帧 | 对应 V4L2 capture 节点已注册 | 板端 `v4l2-ctl --stream-to=-` -> `ssh -T` | 是 | 无 |
| 无 V4L2 节点或需统一帧元数据 | 尚需实现并部署 HBN producer | HBN producer -> `ssh -T` | 是 | 无 |

#### 8.5.1 编码流：本机直接拉 RTSP

只消费 H.264 字节流，不写文件：

```bash
ffmpeg -hide_banner -loglevel error \
  -rtsp_transport tcp \
  -i rtsp://<RDK_X5_IP>/stream_chn0.h264 \
  -map 0:v:0 -an -c:v copy -f h264 pipe:1 \
  | <LOCAL_STREAM_CONSUMER>
```

解码为本机内存中的紧凑 NV12 帧：

```python
import subprocess

width, height = 1920, 1080  # 必须与 RTSP 实际视频参数一致
frame_bytes = width * height * 3 // 2
cmd = [
    "ffmpeg", "-hide_banner", "-loglevel", "error",
    "-rtsp_transport", "tcp",
    "-i", "rtsp://<RDK_X5_IP>/stream_chn0.h264",
    "-map", "0:v:0", "-an", "-pix_fmt", "nv12",
    "-f", "rawvideo", "pipe:1",
]

def read_exact(stream, size):
    data = bytearray(size)
    view = memoryview(data)
    offset = 0
    while offset < size:
        count = stream.readinto(view[offset:])
        if not count:
            return None
        offset += count
    return data

process = subprocess.Popen(cmd, stdout=subprocess.PIPE, bufsize=0)
try:
    while (frame := read_exact(process.stdout, frame_bytes)) is not None:
        # frame 是本机内存中的一帧紧凑 NV12；在此直接送算法，不产生文件。
        consume_nv12(memoryview(frame), width, height)
finally:
    process.terminate()
    process.wait()
```

这条路径最方便，但得到的是 H.264 解码后的 NV12，不是 ISP 输出的逐字节原始 NV12，不能用于要求像素完全一致的 ISP 定量评价。

#### 8.5.2 RAW/NV12：V4L2 经 SSH stdout 推送

这是**已注册 V4L2 capture 节点时** RAW 和 ISP/VSE NV12 的首选无落盘方案。`v4l2-ctl --stream-to=-` 会把捕获 payload 写到 stdout，并自动启用 silent；该行为可在固定版本的 `v4l2-ctl-streaming.cpp` 中核对：[帮助文本](https://github.com/gjasny/v4l-utils/blob/95ad25f6a77a0a6650f5f657ac2c5046efcd04a0/utils/v4l2-ctl/v4l2-ctl-streaming.cpp#L317-L324)、[选项解析](https://github.com/gjasny/v4l-utils/blob/95ad25f6a77a0a6650f5f657ac2c5046efcd04a0/utils/v4l2-ctl/v4l2-ctl-streaming.cpp#L823-L833) 和 [payload 写出](https://github.com/gjasny/v4l-utils/blob/95ad25f6a77a0a6650f5f657ac2c5046efcd04a0/utils/v4l2-ctl/v4l2-ctl-streaming.cpp#L1432-L1464)。板端版本仍必须先验收：

```bash
ssh -T -o RequestTTY=no -o Compression=no rdk-x5 \
  'v4l2-ctl --help-streaming'
```

输出中必须存在 `--stream-to <file>`，并明确 `-` 表示 stdout。随后沿用 3.1～3.3 节的 query-first 原则，查询真实节点、格式、stride 和 `Size Image`：

```bash
# ISP 示例；VIN/VSE 必须换成拓扑查询得到的节点
ssh -T -o RequestTTY=no -o Compression=no rdk-x5 \
  'v4l2-ctl -d /dev/video4 --list-formats-ext && \
   v4l2-ctl -d /dev/video4 --get-fmt-video && \
   v4l2-ctl -d /dev/video4 --all'
```

下面的 FFplay 命令只适用于**紧凑 NV12**：640×480 时 `bytesperline=640`，单帧 `Size Image` 和实测 payload 都必须为 `640 * 480 * 3 / 2 = 460800` bytes。FFmpeg `rawvideo` demuxer不知道 V4L2 stride，任一条件不满足都不能直接使用该命令。

单帧 NV12 直接送本机 FFplay，不生成板端或本机采集文件：

```bash
ssh -T -o RequestTTY=no -o Compression=no rdk-x5 \
  'exec v4l2-ctl -d /dev/video4 \
    --set-fmt-video=width=640,height=480,pixelformat=NV12 \
    --stream-mmap=3 --stream-skip=3 --stream-count=1 \
    --stream-poll --stream-to=-' \
  | ffplay -loglevel error -f rawvideo -pixel_format nv12 \
      -video_size 640x480 -i pipe:0
```

如果 `bytesperline != width` 或单帧字节数不是 460800，必须由本机 stride-aware consumer 按查询到的 Y/UV stride 逐行读取并重排为紧凑 NV12，再交给 FFplay/算法；否则会按错误行宽切帧并产生错位或花屏。

VIN RAW 使用同一管道，只能把节点、宽高和 FourCC 替换为 3.3 节实际查询值；本机消费者还必须理解对应 Bayer、bit depth、packing 和 stride，不能照搬 NV12 的 FFplay 参数。

连续流没有自描述帧边界。先用一帧测量实际 payload 长度，全程仍不落盘：

```bash
ssh -T -o RequestTTY=no -o Compression=no rdk-x5 \
  'exec v4l2-ctl -d /dev/video4 \
    --set-fmt-video=width=640,height=480,pixelformat=NV12 \
    --stream-mmap=3 --stream-skip=3 --stream-count=1 \
    --stream-poll --stream-to=-' \
  | wc -c
```

只有实测字节数等于查询到的单平面 `Size Image`，并且多帧重复测量都固定时，才可让本机脚本按该长度 `read_exact` 切帧。不要用 `width * height * bits / 8` 猜测 RAW 大小；stride、packing 和对齐都会改变 payload 长度。

如果板端 `v4l2-ctl --help-streaming` 还提供 `--stream-to-hdr`，可以在多平面或 `bytesused` 可能变化时评估：

```bash
ssh -T -o RequestTTY=no -o Compression=no rdk-x5 \
  'exec v4l2-ctl -d <VIDEO_NODE> \
    --set-fmt-video=width=<WIDTH>,height=<HEIGHT>,pixelformat=<FOURCC> \
    --stream-mmap=3 --stream-skip=3 --stream-count=<COUNT> \
    --stream-poll --stream-to-hdr=-' \
  | <LOCAL_HEADER_AWARE_CONSUMER>
```

固定版本上游实现会在每帧前写 `FILE_HDR_ID`，并在每个 plane 前写网络字节序的 `bytesused`；它不携带宽高、stride 或时间戳。plane 数量和静态格式仍来自先前查询，本机解析器必须与板端 `v4l2-ctl` 版本实际头格式联调后才能使用。单平面/固定长度场景优先使用更简单的 `--stream-to=-`。

SSH stderr 不会进入上述 stdout 管道；`-T` / `RequestTTY=no` 避免 PTY 改写二进制，`Compression=no` 避免对 RAW/YUV 做低收益压缩。连续高分辨率 RAW 还必须验收链路带宽、SSH CPU 占用和本机消费者背压。

##### 2026-07-14 板端实测：工具支持，但当前 Buildroot 模式无 V4L2 capture

在一台 X5 Buildroot 设备上进行了不落盘实测，结果必须拆成两层理解：

| 检查层 | 实测结果 |
|---|---|
| `v4l2-ctl` 工具能力 | `v4l2-ctl 1.22.1`；`--help-streaming` 明确支持 `--stream-to=-`、自动 silent 和 `--stream-to-hdr=-` |
| Camera capture ABI | `/sys/class/video4linux` 为空；不存在 `/dev/video0`、`/dev/video4` 或 `/dev/media0`，当前启动镜像/模式没有注册 V4L2 capture 节点 |

设备环境为 Buildroot 2022.08、Linux `6.1.83-DR-PL5.1_V1.1.2`。逐一对 `/dev/vin{0..3}_{src,cap,emb,pdaf}` 共 16 个节点执行 `stat` 和 `v4l2-ctl --all`：

```text
nodes_tested=16
node_type=character special file
node_major=242 (0xf2, /proc/devices 名称为 flow)
video4linux_major=81
v4l2_ctl_all_success=0
v4l2_ctl_all_exit=1  # 所有 16 个节点
stderr: Unable to detect what device /dev/vinN_KIND is, exiting.
```

因此这些 `/dev/vin*` 是 HBN/native flow 字符设备，不是 V4L2 video device；`cap` 只是 native vnode 端点命名，不能据此推断支持 `VIDIOC_QUERYCAP`。`/dev/vs-isp0_cap` 同样返回 exit 1。16 个 VIN 节点无一被 `v4l2-ctl` 识别为 V4L2 device；工具可能在设备类型检测阶段就退出，不能据此声称已经向每个节点发出 `VIDIOC_QUERYCAP`。除先前用于确认失败模式的 `/dev/vin0_cap` 外，没有继续向 `src`、`emb`、`pdaf` 或其他 `cap` 节点发送 streaming ioctl。

实际执行单帧 stdout 测试：

```bash
set -o pipefail
v4l2-ctl -d /dev/video0 \
  --stream-mmap=3 --stream-count=1 --stream-poll --stream-to=- \
  | wc -c
producer_rc=${PIPESTATUS[0]}
```

实测结果：

```text
payload_bytes=0
v4l2_stream_exit=1
stderr: Cannot open device /dev/video0, exiting.
```

因此结论不是“`stream-to` 功能失败”，而是：**工具支持 stdout 输出，但当前启动镜像/Camera 模式没有可通过 `VIDIOC_*` 驱动的 capture 节点，端到端 V4L2 管道不可用。** 多帧边界无法继续验证，因为单帧采集前置条件未成立。

官方 RDK 镜像可通过 `srpi-config` 切到 `V4L2 sif-isp-vse` 并重启；该 Buildroot 设备没有 `srpi-config`、`hobot-config` 或 `/app/cdev_demo/v4l2`。若后续要切换镜像、设备树或启动模式，必须先备份配置、记录恢复方法并安排 Camera 业务停机窗口；切换后只有在 `/dev/media0` / `/dev/video*` 出现且 `--all`、`--get-fmt-video` 成功后，才能重新执行本节 stdout 测试。

对本次实测设备，如果不更换镜像或切换启动模式，RAW/NV12 无落盘传输只能走 8.5.3 节的 HBN producer；该 producer 当前尚未实现。现有 `get_vin_data` / `get_isp_data` dump 样例会写文件，不能直接替代 stdout 数据流。

#### 8.5.3 待实现设计：需要统一元数据时使用 HBN producer

> **当前状态：协议设计，不是现成命令。** `x5-frame-stream` 和 `consume_x5_frames.py` 尚未在 Camera Toolbox 或板端实现、构建、部署，所以下面的命令不能直接运行。RTSP 服务已启动时可直接使用 RTSP + 本机 FFmpeg；V4L2 -> SSH stdout 仅适用于 capture 节点已注册的板卡。本次实测 Buildroot 设备不满足 V4L2 前提，若必须取得 RAW/NV12 无落盘流，需要实现该 HBN producer/consumer 或切换系统模式。

现有 `get_vin_data` / `get_isp_data` 样例会写文件，而且其交互输出不适合作为稳定二进制协议。建议后续基于同一 HBN 初始化流程实现小型 `x5-frame-stream` producer，只向专用数据 fd 写帧记录：

```text
本机 Python
   │  启动一个持久 SSH 进程；不为每帧重连
   ▼
ssh -T, Compression=no
   │
   ▼
x5-frame-stream
   ├── hbn_vnode_getframe(VIN / ISP / VSE)
   ├── 写固定长度 header
   ├── 写各 plane payload
   └── hbn_vnode_releaseframe
```

producer 和 consumer 实现、构建并部署后的目标命令形态：

```bash
ssh -T \
  -o RequestTTY=no \
  -o Compression=no \
  rdk-x5 \
  'exec /usr/local/bin/x5-frame-stream --source isp --sensor 7 --count 0' \
  | python3 consume_x5_frames.py
```

- `--source vin`：发送 RAW；
- `--source isp`：发送 ISP NV12；
- `--source vse --channel N`：发送指定 VSE NV12；
- `--count 1`：自动化单帧抓取；
- `--count 0`：持续推流，直到本机断开或发送信号。

不要分配 SSH PTY：PTY 会改写字节并可能插入控制信息。图像通常不可压缩或压缩收益低，因此关闭 SSH compression，避免额外 CPU 开销。远端权限应通过 `video` 组、设备 ACL 或仅允许该 producer 的 sudoers 规则预先配置；不要让 `sudo` 在二进制会话中交互式询问密码。

#### 8.5.4 自定义帧协议和 producer 约束

裸 payload 本身没有帧边界。建议每帧使用网络字节序的 80-byte 固定头，后接 plane 0、plane 1、plane 2 的有效字节：

```text
magic[4] = "X5FR"
version:u16, header_size:u16
flags:u32
format_namespace:u32, format_code:u32
plane_count:u32
width:u32, height:u32
stride[3]:u32
plane_size[3]:u32
frame_id:u64, timestamp_ns:u64
payload_len:u64
payload[payload_len]
```

本机必须验证：

```text
magic / version / header_size
1 <= plane_count <= 3
payload_len == sum(plane_size[0:plane_count])
分辨率、stride、plane size 位于预期上限内
每次 read 都使用 read_exact，不能假设一次 read 返回完整 header 或 frame
```

producer 必须满足：

- 自己的日志只写 `stderr`；二进制只写专用数据 fd；
- 启动后先 `dup(stdout)` 得到数据 fd，再将普通 stdout 重定向到 stderr，防止底层库 `printf` 污染码流；
- 使用 `write_full` 处理短写和 `EINTR`；忽略 `SIGPIPE`，把 `EPIPE` 作为本机断开；
- 无论发送成功、超时还是断连，都保证已获取的 HBN frame 被 release；
- 单帧/低帧率可以直接发送后 release；连续高带宽 RAW 更适合复制到预分配的有界双缓冲，立即 release HBN frame，再由发送线程写 SSH/TCP；队列满时应明确丢帧或停止，不能无限增长；
- 不创建 `.raw`、`.yuv`、`.h264` 或临时目录。producer 二进制是长期部署组件，不是采集残留。

对于持续 4K RAW，多数场景瓶颈会从 Camera 转到 SSH 加密和网络带宽。此时可以保留同一帧协议，将数据面从 SSH stdout 换成板端监听的 TCP socket；SSH 只负责启动、停止和状态查询。

## 9. 输出格式与归档要求

### 9.1 RAW

每份 RAW 至少绑定：

```text
sensor_model
sensor_config_file
width / height
valid_bit_depth
container_bit_depth
packing
endianness
bayer_pattern
stride
linear_or_wdr
exposure_index_in_wdr
exposure_time
analog_gain
digital_gain
black_level
temperature
frame_id
timestamp
```

文件大小校验应使用实际 stride 和 plane size；不能总是假设 `width × height × valid_bits / 8`。

### 9.2 NV12 YUV

至少记录：

```text
width / height / stride
plane_size_y / plane_size_uv
color_matrix
full_or_limited_range
oetf
isp_config
frame_id / timestamp
```

有效区域没有 stride padding 时，单帧大小为：

$$
size_{NV12}=width\times height\times\frac{3}{2}
$$

出现绿边、错行或画面倾斜时，优先检查 stride、宽度对齐和 Y/UV plane offset。

### 9.3 编码流

至少记录：

```text
codec / profile / level
width / height / frame_rate
rate_control / target_bitrate
GOP / IDR interval
SPS/PPS/VPS
frame timestamp
source YUV format and range
```

## 10. 常见问题

### Camera 未探测到

- 检查是否断电插拔；
- 检查 Sensor 是否在当前镜像支持列表；
- 检查 CAM0 / CAM1、I2C bus、MIPI RX、lane 数和设备树；
- 用示例 `-h` 查看当前 Sensor index，不复用另一版本的 index；
- 检查日志中的 chip ID 是否匹配。

### `hbn_vnode_getframe` 或 `v4l2-ctl` 超时

- 确认 vflow 已启动；
- 确认 VIN 输出 `ddr_en` 和 output buffer 已配置；
- 确认每次成功 get frame 后都 release；
- 检查是否有另一个进程占用 Camera/节点；
- 核对 HBN/V4L2 模式和节点拓扑；
- 先丢弃若干启动帧，等待 Sensor 和 3A 稳定。

### RAW 尺寸或亮度异常

- 不要把 RAW10 的 16-bit 容器当作 RAW16 有效数据；
- 检查 packed/unpacked、有效位对齐和端序；
- 检查 Bayer Pattern；
- 检查 stride padding；
- 检查 linear/WDR 及长短曝光 plane；
- 确认抓取位置确实是目标 VIN，而不是 ISP/VSE。

### YUV 花屏或绿线

- 确认格式是 NV12 而不是 NV21/I420；
- 检查实际 stride，不要只按 width 步进；
- 确认 Y 和 UV plane 大小及顺序；
- 确认输出宽高满足当前模块对齐约束；
- V4L2 抓取前先查看 `--list-formats-ext`。

### 编码文件无法播放

- 确认编码类型和文件扩展名一致；
- 检查是否实际写入 SPS/PPS 和 IDR；
- 确认不是把 NV12 数据直接命名为 `.h264`；
- 确认程序正常收尾或已刷新文件；
- elementary stream 播放时显式提供正确 codec，不按 MP4 容器解析。

## 11. 推荐选择

| 需求 | 推荐入口 |
|---|---|
| 首次确认 Sensor、RAW 和 ISP 同时有数据 | `vio_capture` |
| 脚本化抓 ISP 后单帧 NV12 | ISP 节点 + `v4l2-ctl` |
| 脚本化抓 VIN RAW | 先查询 VIN FourCC，再用 `v4l2-ctl` |
| Python 快速抓 YUV | `02_mipi_camera_dump.py` |
| 集成到简单 C/Python 应用 | `hobot-spdev` |
| 需要完整 stride/plane/timestamp | HBN `hbn_vnode_getframe` |
| 连续多分辨率 NV12 | `single_pipe_vin_isp_vse` |
| 本地 H.264 文件 | `vio2encoder` |
| 自定义编码/码率/GOP | HBN + Media Codec 或 Encoder API |
| 浏览器展示 | 官方 WebSocket 示例 |
| 实时 RTSP 发布/采集 | 有匹配包时用 `user_manual_v1.0.20` Sunrise Camera；否则 Encoder + 自定义 RTSP Server |

## 12. 资料来源与版本边界

本地离线手册固定为 `/media/psf/Home/Documents/user_manual_v1.0.20/_sources/`，本次直接核对以下原文：

- `samples/sample_vin.md.txt`：VIN RAW、`g/l/q` 交互和 stride 文件名；
- `samples/sample_isp.md.txt`：ISP NV12、`g/l/q` 交互和双 plane size；
- `samples/sample_pipeline.md.txt`：VSE 每 60 帧保存及 VPU H.264 输出；
- `samples/sunrise_camera_user_guide.md.txt`：实时 RTSP 部署、URL 和 VLC 拉流；
- `samples/sunrise_camera_develop_guide.md.txt`：RTSP Server 模块和 H.264 边界；
- `multimedia_development/1-Camera_API_zh_CN.rst.txt`：Sensor format、分辨率和模式；
- `multimedia_development/2-HBN_API_zh_CN.rst.txt`：阻塞式 `hbn_vnode_getframe` 和 buffer 归还语义；
- `multimedia_development/9-MediaCodec_API_zh_CN.rst.txt`：MediaCodec 接口、码率控制结构和示例值。

公开官方资料：

- [RDK X5 V4L2 使用](https://github.com/D-Robotics/rdk_doc/blob/main/docs/07_Advanced_development/01_hardware_development/rdk_x5/V4l2.md)
- [RDK X5 `vio_capture` 示例](https://github.com/D-Robotics/rdk_doc/blob/main/docs/03_Basic_Application/02_cdev_demo_sample/vio_capture.md)
- [RDK X5 `vio2encoder` 示例](https://github.com/D-Robotics/rdk_doc/blob/main/docs/03_Basic_Application/02_cdev_demo_sample/vio2encoder.md)
- [RDK X5 C 多媒体示例说明](https://github.com/D-Robotics/rdk_doc/blob/main/docs/03_Basic_Application/06_multi_media_sp_dev_api/RDK_X5/cdev_multimedia_api_x5/cdev_demo.md)
- [RDK X5 VIO API](https://github.com/D-Robotics/rdk_doc/blob/main/docs/03_Basic_Application/06_multi_media_sp_dev_api/RDK_X5/cdev_multimedia_api_x5/vio_api.md)
- [RDK X5 Encoder API](https://github.com/D-Robotics/rdk_doc/blob/main/docs/03_Basic_Application/06_multi_media_sp_dev_api/RDK_X5/cdev_multimedia_api_x5/encoder_api.md)
- [RDK X5 SYS 模块绑定 API](https://github.com/D-Robotics/rdk_doc/blob/main/docs/03_Basic_Application/06_multi_media_sp_dev_api/RDK_X5/cdev_multimedia_api_x5/sys_api.md)
- [RDK X5 Python Camera API](https://github.com/D-Robotics/rdk_doc/blob/main/docs/03_Basic_Application/06_multi_media_sp_dev_api/RDK_X5/pydev_multimedia_api_x5/object_camera.md)
- [RDK X5 MIPI Camera Python 示例](https://github.com/D-Robotics/rdk_doc/blob/main/docs/03_Basic_Application/03_pydev_demo_sample/RDK_X5/08_mipi_camera_sample.md)
- [RDK X5 Web Camera 示例](https://github.com/D-Robotics/rdk_doc/blob/main/docs/03_Basic_Application/03_pydev_demo_sample/RDK_X5/09_web_display_camera_sample.md)
- [D-Robotics `x5-multimedia-samples` 锁定源码快照](https://github.com/D-Robotics/x5-multimedia-samples/tree/0d9925cef6c41056525ae27383d2d3604f9e4690)
- [锁定快照：VIN RAW 源码](https://github.com/D-Robotics/x5-multimedia-samples/blob/0d9925cef6c41056525ae27383d2d3604f9e4690/sample_vin/get_vin_data/get_vin_data.c)
- [锁定快照：ISP YUV 源码](https://github.com/D-Robotics/x5-multimedia-samples/blob/0d9925cef6c41056525ae27383d2d3604f9e4690/sample_isp/get_isp_data/get_isp_data.c)
- [锁定快照：VSE YUV 源码](https://github.com/D-Robotics/x5-multimedia-samples/blob/0d9925cef6c41056525ae27383d2d3604f9e4690/sample_pipeline/single_pipe_vin_isp_vse/single_pipe_vin_isp_vse.c)
- [锁定快照：VSE + VPU 源码](https://github.com/D-Robotics/x5-multimedia-samples/blob/0d9925cef6c41056525ae27383d2d3604f9e4690/sample_pipeline/single_pipe_vin_isp_vse_vpu/single_pipe_vin_isp_vse_vpu.c)

- [D-Robotics `x5-hobot-spdev` 锁定源码快照](https://github.com/D-Robotics/x5-hobot-spdev/tree/506ec790a80c01ebe552929e6efb572d562b3b14)
- [锁定快照：`sp_vio.h`](https://github.com/D-Robotics/x5-hobot-spdev/blob/506ec790a80c01ebe552929e6efb572d562b3b14/src/clang/sp_vio.h)
- [锁定快照：`sp_codec.h`](https://github.com/D-Robotics/x5-hobot-spdev/blob/506ec790a80c01ebe552929e6efb572d562b3b14/src/clang/sp_codec.h)


旧 `multimedia_samples.md` 中的 `get_sif_data` / `get_isp_data` 章节明确描述 X3M / SIF，只可作为历史架构参考；X5 实际命令和接口以本节列出的 RDK X5 文档、`x5-multimedia-samples` 和板端安装内容为准。
