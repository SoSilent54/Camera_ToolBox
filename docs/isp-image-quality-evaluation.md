# ISP 图像质量客观评价与量化

## 目的

本文定义 ISP Tuning 过程中九类核心图像质量指标的可重复测量方法：

1. Distortion
2. Lens Shading
3. Color Shading
4. Brightness
5. Contrast & Gamma
6. AWB
7. Color
8. Resolution
9. Noise

目标是统一测试条件、ROI、公式、单位和报告统计量，为 Camera Toolbox 后续自动分析、结果归档和产品验收提供依据。

ISO / IEC 标准通常规定测量方法和报告口径，不等同于所有产品通用的 Pass / Fail 阈值。产品门限必须结合应用场景、客户规格、Golden Sample 和量产分布另行定义。

## 评价分层

### 线性 RAW

用于定位 Sensor、镜头和 ISP 基础校正问题：

- 分析前逐通道扣除匹配曝光、增益和温度的黑电平。
- 记录 width、height、bit depth、Bayer Pattern、packing、曝光时间、模拟增益、数字增益和温度。
- 关闭或固定会改变被测对象的动态模块。
- 排除坏点、饱和像素和裁剪像素。

主要覆盖：

- LSC 校正前的 Lens Shading；
- RAW Color Shading；
- AWB illuminant angular error；
- Noise Profile、空间噪声和时间噪声。

### ISP 最终输出

用于评价用户实际看到的画面：

- 记录 RGB / YUV 色彩空间、白点、OETF / Gamma、量化范围、位深和编码质量。
- 记录 Crop、Scale、LDC、EIS、WDR、DRC、LDCI、NR、Sharpen 和压缩状态。
- 优先使用最大分辨率、最低压缩或无损输出。
- 需要物理线性量时，先扣除编码 pedestal，再执行逆 OETF。

主要覆盖：

- LDC 后残余畸变；
- LSC / Color Shading 校正后的均匀性；
- 亮度、OECF、AWB 和色彩还原；
- 端到端 SFR / MTF；
- 最终视频链路下的亮度和色度噪声。

同一指标应尽量同时保存“校正前 RAW”和“最终输出”结果。最终输出通过但 RAW 基础质量很差，通常表示后处理补偿较重，需要额外检查边角噪声、颜色断层、跨色温稳定性和模组一致性。

## 通用测试条件

| 条件 | 要求 |
|---|---|
| 光源 | 预热稳定；记录实测 illuminance、CCT、Duv 和 SPD；检查频闪，隔离环境杂散光 |
| 图卡 | 使用有可追溯参考值的图卡；图卡平面与成像面平行；避免污损、反光和过期 |
| 均匀场 | 优先积分球或经过校准的面光源；ISO 17957 建议目标与照明综合均匀性在 2.5% 内，且应在 5% 内 |
| 相机姿态 | 固定位置、焦距、光圈、对焦距离和输出模式；记录图卡距离和 FOV 覆盖 |
| 自动算法 | 静态指标锁定 AE / AWB / AF；评价某个自动算法时，只保留对应算法和必要依赖为自动状态 |
| 输出 | 使用最大分辨率和最低压缩；记录 OETF、色彩矩阵、full / limited range、位深和编码器设置 |
| 帧统计 | 自动算法收敛后取样；静态结果保存多帧均值和标准差，动态结果保存完整逐帧曲线 |
| 样机统计 | 调参机之外覆盖多台、多批次和温度边界；报告均值、标准差、P95 和最差值 |

建议建立如下测试矩阵：

| 维度 | 建议覆盖 |
|---|---|
| Sensor 模式 | Linear、WDR；各主分辨率和帧率 |
| 光源 | A、D50 / D55、D65、F 系列或产品实际 LED；记录实测 SPD |
| 照度 | 高照、常规、低照、极低照；具体 lux 由产品场景定义 |
| 增益 | 低增益及主要 ISO / 系统增益档位 |
| 日夜模式 | 可见光彩色、IR-CUT 切换、红外黑白 |
| 温度 | 常温及产品规定的高低温边界 |
| 动态场景 | 光源切换、明暗切换、静止转运动、运动转静止 |

## 1. Distortion

### 测试条件

- 使用点阵图卡或规则网格图卡。
- 相机光轴尽量垂直图卡中心，减小姿态校正对结果的影响。
- 分别测试 LDC 关闭和目标 LDC 配置。
- 同时测量 LDC 前后的水平、垂直 FOV，防止通过过度裁剪换取低畸变。

### 计算

先使用不包含畸变参数的 similarity transform 或 planar homography，仅校正图卡姿态、尺度、平移和必要的平面透视关系。不得使用包含径向或切向畸变系数的模型生成理想点位，否则拟合过程可能吸收被测误差。

对 $r_{ideal}>0$ 的点计算：

$$
D_i=\frac{r_{meas,i}-r_{ideal,i}}{r_{ideal,i}}\times100\%
$$

中心点满足 $r_{ideal}=0$，分母无定义，不代入该公式。

### 报告

- 全场 $D(r)$ 曲线；
- 最大绝对畸变 $\max |D_i|$；
- 水平、垂直和对角边缘值；
- 桶形 / 枕形符号；
- LDC 前后水平、垂直 FOV 损失；
- 点位残差或直线弯曲热力图。

主要依据：[ISO 17850](https://www.iso.org/standard/60819.html)。

## 2. Lens Shading

### 测试条件

- 使用中性均匀场，优先积分球或经过校准的面光源。
- RAW 定位测试关闭 LSC，并逐通道减去黑电平。
- 最终验收开启目标 LSC 配置。
- 固定曝光、增益和白平衡。
- 使用光源校准图修正测试设备自身残余非均匀性。

### 计算

将画面划分为至少 $11\times11$ 个网格，计算每块线性亮度均值 $Y_i$。

相对中心照度：

$$
RI_i=\frac{Y_i}{Y_{center}}\times100\%
$$

亮度非均匀性：

$$
D_Y=\frac{Y_{max}-Y_{min}}{Y_{max}}\times100\%
$$

边角衰减：

$$
Falloff_{EV,i}=\log_2\left(\frac{Y_{center}}{Y_i}\right)
$$

最终输出若采用非线性编码，计算物理衰减前必须先执行逆 OETF。

### 报告

- 中心、四边和四角的 $RI$；
- 最差块 $RI_{min}$；
- $D_Y$ 和最大 Falloff EV；
- 二维亮度热力图；
- 水平、垂直和对角径向曲线；
- LSC 前后对比；
- LSC 开启后边角噪声变化。

主要依据：[ISO 17957](https://www.iso.org/standard/31974.html)。

## 3. Color Shading

### 测试条件

- 使用与 Lens Shading 相同的中性均匀场。
- 覆盖 A、D 类、F 类或产品实际 LED 光源。
- 固定曝光；最终输出测试时使中心区域为目标中性。
- 记录实际 SPD。相同 CCT、不同 SPD 的光源可能产生不同 Color Shading。

### RAW 指标

对每个网格计算：

$$
RG_i=\frac{R_i}{G_i},\qquad BG_i=\frac{B_i}{G_i}
$$

报告 $R/G$、$B/G$ 相对中心或全场均值的偏差百分比。

### 最终输出指标

将输出按声明的色彩空间和白点转换到 CIELAB。色度非均匀性：

$$
D_C=\max_i\sqrt{(a_i^*-\bar a^*)^2+(b_i^*-\bar b^*)^2}
$$

可补充每个区域相对中心或全场参考值的 $\Delta E_{00}$。

### 报告

- $R/G$、$B/G$ 空间偏差；
- $D_C$；
- $\Delta E_{00}$ 均值、P95、最大值和位置；
- $a^*$、$b^*$、$\Delta E_{00}$ 热力图；
- 不同光源下的最差结果；
- Color Shading 表切换时的逐帧连续性。

主要依据：[ISO 17957](https://www.iso.org/standard/31974.html)、[ISO/CIE 11664-6](https://www.iso.org/standard/82662.html)。

## 4. Brightness

### 测试条件

- 使用 18% 中性灰卡、灰阶卡或 OECF 图卡。
- 评价静态亮度时固定曝光。
- 评价 AE 时保持 AE 自动，并记录每帧曝光时间和各级增益。
- 同时保留黑区、中间调和高光 ROI。

### 计算

报告规定 ROI 的 $Y_{mean}$ 或 $L^*$。在线性域计算目标亮度偏差：

$$
\Delta EV=\log_2\left(\frac{Y_{meas,lin}}{Y_{target,lin}}\right)
$$

同时计算：

- 黑电平裁剪率；
- 高光裁剪率；
- 帧间亮度标准差；
- AE 收敛时间、过冲和稳态抖动。

18% 灰卡对应的最终码值取决于 OETF、DRC、WDR 和产品风格，必须由产品目标曲线定义，不能作为通用固定码值。

### 报告

- 目标值、实测均值和 $\Delta EV$；
- 黑白裁剪像素比例；
- 不同照度和增益下的亮度曲线；
- AE 逐帧亮度、曝光时间和增益曲线。

主要依据：[ISO 14524](https://www.iso.org/standard/43527.html) 及产品目标亮度定义。

## 5. Contrast & Gamma

### 测试条件

- 使用反射式或透射式灰阶 / OECF 图卡。
- 关闭 DRC、LDCI 等局部自适应模块，测基础 Gamma / OECF。
- 开启完整目标配置，再测最终 tone mapping。
- 记录目标编码：sRGB、BT.709、PQ、HLG 或自定义曲线。

### 计算

建立输入曝光量或图卡亮度 $E_i$ 与输出码值 $V_i$ 的 OECF：

$$
V_i=f(E_i)
$$

对真正的幂函数区间，可计算编码指数：

$$
g=\frac{d\log V}{d\log E}
$$

若采用显示 Gamma 记法：

$$
\gamma_{display}=\frac{1}{g}
$$

报告时必须说明采用的是编码指数 $g$，还是显示 Gamma $\gamma_{display}$。sRGB、BT.709、PQ 和 HLG 不能简化为全区间单一 Gamma 拟合。

### 报告

- 完整 OECF 曲线；
- 相对目标曲线的 MAE、RMSE 和最大偏差；
- 暗部、中间调和高光的局部斜率；
- 黑白对比度；
- 灰阶单调性和可分辨级数；
- 黑位抬升、高光压缩和裁剪率；
- DRC / LDCI 开启前后对比。

主要依据：[ISO 14524](https://www.iso.org/standard/43527.html)。

## 6. AWB

AWB 需要分别评价最终输出稳态准确性、RAW illuminant estimation 准确性和动态收敛性。

### 测试条件

- 使用带多个中性块的 ColorChecker 或专用灰阶图卡。
- 覆盖 A、D50 / D55、D65、F2 / TL84、产品实际 LED、低照和混合光。
- 记录光谱仪实测的 SPD、CCT 和 Duv。
- AWB 保持自动；其他会影响颜色的自适应模块固定或记录状态。

### 最终输出主指标

最终输出以中性块的 $\Delta u'v'$、Duv 和 $\Delta C_{00}$ 为主：

$$
\Delta u'v'=\sqrt{(u'_{meas}-u'_{target})^2+(v'_{meas}-v'_{target})^2}
$$

计算前必须按声明的 RGB / YUV 矩阵、量化范围和 OETF 正确解码，并转换到 XYZ 后再求 $u'v'$。$(u'_{target},v'_{target})$ 表示声明输出空间及色适应模型下的目标白点，通常是输出中性轴；它不是光源 SPD 的原始色度。若产品有意保留暖光或其他光源氛围，必须把对应产品目标白点显式写入测试 recipe，不能临时改用源光色度。

TL84 / F 类光源不在黑体轨迹上，相同 CCT 仍可能存在明显绿 / 洋红偏差。因此 CCT 只作为接近黑体轨迹的 A / D 类光源辅助描述，不作为跨光源主指标。

### RAW illuminant angular error

在线性 RAW 中，以 Sensor RGB 空间的 ground-truth illuminant $\mathbf e$ 与 AWB 算法估计 $\hat{\mathbf e}$ 计算：

$$
\theta=\arccos\left(\frac{\mathbf e\cdot\hat{\mathbf e}}{\|\mathbf e\|\,\|\hat{\mathbf e}\|}\right)
$$

$\mathbf e$ 和 $\hat{\mathbf e}$ 必须位于同一 Sensor RGB 空间，并采用相同的通道顺序、Gr / Gb 合并和归一化约定。

光谱仪 SPD 不能直接作为 Sensor RGB 真值。ground-truth illuminant 使用以下方法之一构造。

#### Sensor 光谱响应积分

已知 Sensor 各通道光谱灵敏度 $S_c(\lambda)$ 时：

$$
e_c\propto\int SPD(\lambda)\,\rho_{neutral}(\lambda)\,S_c(\lambda)\,d\lambda
$$

其中 $\rho_{neutral}(\lambda)$ 是中性目标的实测光谱反射率或透射率。

#### 中性块 RAW 实测

缺少 Sensor 光谱灵敏度时：

1. 使用参考光源拍摄未裁剪中性块的线性 RAW；
2. 逐通道减去匹配曝光、增益和温度的黑电平；
3. 排除坏点、饱和像素和污染区域；
4. 对有效像素取稳健均值；
5. 按 AWB 算法相同约定合并绿色通道并归一化，得到 $\mathbf e$。

光谱仪 SPD 用于定义和复现实验光源，但不能跳过 Sensor 光谱响应直接变成相机 RGB 向量。

### 动态指标

在光源切换或场景内容切换时统计：

- 10%–90% rise / fall time；
- settling time；
- 最大过冲；
- 稳态帧间标准差；
- 来回摆动；
- AWB Gain、CCM、Color Shading 表切换的同步性和连续性。

### 报告

- 最终输出 $\Delta u'v'$、Duv、$\Delta C_{00}$ 的均值、P95 和最大值；
- RAW angular error，单位 degree；
- A / D 类光源可附 CCT 误差；
- 收敛时间，单位 s 和 frame；
- 逐帧色度、AWB Gain 和状态曲线。

主要依据：ISO/CIE 11664-5、[ISO/CIE 11664-6](https://www.iso.org/standard/82662.html) 及工程 AWB 时序评价方法。

## 7. Color

### 测试条件

- 使用 ColorChecker Classic、ColorChecker SG 或具有光谱参考数据的色卡。
- 均匀照明并记录实际 SPD。
- 优先使用当前图卡在当前光源下的实测 XYZ / CIELAB / 光谱数据，不能无条件套用另一光源下的默认值。
- AWB 先收敛。
- 明确输出工作色域、白点、OETF 和 RGB / YUV 转换矩阵。

### 计算

对每个色块计算 CIEDE2000：

$$
\Delta E_{00}
$$

并分离观察：

- $\Delta L_{00}$：明度误差；
- $\Delta C_{00}$：彩度误差；
- $\Delta H_{00}$：色相误差；
- 饱和度比；
- 肤色、绿色、蓝天等关键色误差。

### 报告

- $\Delta E_{00}$ 均值、中位数、P95 和最大值；
- 每个色块的误差；
- 中性色、肤色、常用色和饱和色分组统计；
- 色相偏移矢量图；
- 不同光源、照度和增益下的误差趋势。

如果产品采用有意的风格色，应同时保留相对标准色和相对产品目标色的两组结果。

主要依据：[ISO 17321-1](https://www.iso.org/standard/56537.html)、[ISO/CIE 11664-6](https://www.iso.org/standard/82662.html)。

## 8. Resolution

### 测试条件

- 使用 ISO 12233 斜边图卡。
- 正确对焦，图卡平面与成像面平行。
- 使用最大分辨率和最低压缩。
- 中心、四边和四角分别布置水平、垂直方向斜边。
- 变焦或多焦距镜头分别测试。
- 最终输出保留目标 Sharpen，但必须同步测量黑白边。

### 计算

```text
斜边 ROI ──► ESF ──► LSF ──► FFT ──► SFR / MTF
```

主要指标：

- MTF50；
- MTF10；
- Nyquist 频率处 MTF；
- edge acutance；
- overshoot / undershoot；
- 中心与边角一致性；
- 水平与垂直一致性。

cycles/pixel 转换为 LW/PH：

$$
LW/PH=2f_{cycles/pixel}H_{pixels}
$$

### 防止假锐

Sharpen 可能提高 MTF50，同时引入黑白边和 ringing。验收不能只看 MTF50，还应报告：

- overshoot；
- undershoot；
- ringing 宽度；
- aliasing；
- 中心 / 边角差异；
- 噪声放大量。

主要依据：[ISO 12233 官方分辨率说明](https://www.iso.org/12233)。

## 9. Noise

### 测试条件

- 使用均匀灰阶 / OECF 图卡和暗场。
- 覆盖多个曝光量、ISO / 系统增益和温度。
- 采集多帧，用于分离空间噪声和时间噪声。
- 静止场景和运动场景分开评价。

### SNR

设 ROI 平均码值为 $\mu$，实测黑电平或编码 pedestal 为 $\mu_0$，噪声标准差为 $\sigma$：

$$
SNR=20\log_{10}\left(\frac{\mu-\mu_0}{\sigma}\right)
$$

$\mu$ 必须扣除黑电平或码值偏置。limited-range Y 的黑位码值不能计入有效信号，否则低照 SNR 会被系统性高估。

跨 RAW 和最终输出比较时：

1. 分别减去 RAW black level 或输出编码 pedestal；
2. 对最终输出执行逆 OETF；
3. 统一到可比较的线性域和归一化范围；
4. 再比较 signal、noise 和 SNR。

如果不进行线性化，最终输出 SNR 只能标记为该 OETF、量化范围、NR、Sharpen 和编码链路下的工程指标，不能与 RAW SNR 直接比较。

### 噪声分解

需要分别测量：

- RAW R / Gr / Gb / B 噪声；
- 最终输出 Y / Cb / Cr 噪声；
- 空间随机噪声；
- 跨帧时间噪声；
- 固定形态噪声 FPN；
- 行列噪声和 banding；
- 坏点、热像素；
- noise-versus-signal 曲线。

空间统计应避免把未校正 shading 当作随机噪声。时间噪声建议先对每个像素计算跨帧标准差，再对 ROI 做稳健汇总；FPN 和 banding 单独报告。

### 时域降噪补充指标

静态 SNR 会奖励过强 3DNR，因此运动场景还应测量：

- 运动边缘残影长度；
- moving-object MTF；
- 纹理保留率；
- 帧间局部亮度拖尾；
- 色彩拖影；
- 静止转运动、运动转静止的恢复时间。

### 报告

- 各灰阶、增益和温度下的 SNR；
- RAW 与最终输出各自的测量域和处理状态；
- Y / C 噪声；
- noise-versus-signal 曲线；
- FPN / banding 热力图；
- 运动残影和纹理损失；
- 与 Resolution 同时报告，防止用强 NR 换取虚假的低噪声。

主要依据：[ISO 15739](https://www.iso.org/standard/82233.html)。

## 指标耦合与取舍

| 调整 | 可能改善 | 必须同步检查 |
|---|---|---|
| LDC 增强 | 几何畸变 | FOV、边角插值分辨率、噪声和裁剪 |
| LSC 增强 | 亮度均匀性 | 边角噪声、饱和、Color Shading |
| Color Shading 增强 | 空间颜色一致性 | 不同 SPD、色温切换连续性、AWB 稳定性 |
| DRC / LDCI 增强 | 暗部和局部对比度 | 噪声、色噪、halo、Gamma / CCM 一致性 |
| Sharpen 增强 | MTF50、视觉清晰度 | overshoot、undershoot、ringing、噪声放大 |
| NR / 3DNR 增强 | 静态噪声和 SNR | 纹理、MTF、运动残影、恢复时间 |
| CCM / Saturation 增强 | 色彩鲜艳度 | $\Delta E_{00}$、肤色、暗部色噪 |

## 门限制定

每个指标建立以下闭环：

```text
标准测量方法
   ├── Product Target      产品设计目标
   ├── Acceptance Limit    客户 / 出厂合格边界
   └── Control Limit       量产统计预警线
```

门限必须绑定完整测试条件，例如：

```text
mode: Linear 1080p30
illuminant: D55, measured CCT/Duv/SPD
illuminance: 500 lux
gain: 1x
temperature: 25 degC
output: BT.709 limited-range, 8-bit, compression disabled
metric: corner color-shading Delta E00
sampling: 5 devices, 30 settled frames per device
statistics: mean, P95, maximum
```

不带模式、光源、照度、增益、温度、输出编码和统计口径的门限不可复用。

## 自动化报告数据模型

每条结果至少包含：

```text
metric
camera_mode
raw_or_output
illuminant_spd
cct
duv
illuminance_lux
exposure_time
analog_gain
digital_gain
temperature
isp_module_state
color_space
oetf
quantization_range
roi
formula
unit
mean
std
p95
worst
target
limit
pass_fail
```

建议输出：

- JSON / CSV 原始指标；
- HTML / PDF 汇总报告；
- Distortion、Lens Shading、Color Shading 热力图；
- OECF、MTF、noise-versus-signal 曲线；
- AE / AWB 逐帧曲线；
- RAW 与最终输出对照；
- Golden Sample 与当前版本差异；
- 输入图像、图卡版本、参数版本和计算程序版本的 hash。

## Camera Toolbox 实现边界

建议沿现有 `core -> app -> adapters -> frontends` 分层实现：

```text
core
├── 纯计算指标、ROI、曲线和报告类型
├── 不访问文件、设备或网络
└── 输入中显式携带测量域、色彩空间、单位和黑电平

app
├── 编排采集、图卡识别、分析和结果归档
├── 校验测试 recipe 与指标前置条件
└── 生成结构化 WorkflowEvent

adapters
├── 图像 / RAW loader
├── 光谱仪、照度计和相机采集接口
└── JSON / CSV / HTML artifact store

frontends
├── GUI 热力图和曲线
├── CLI 批处理与 CI 门限
└── TUI 采集状态和现场诊断
```

实现正式验收工具前，应使用标准参考图像或经过验证的商业工具进行交叉验证，避免公式实现、色彩转换、ROI 识别或单位换算错误进入验收链路。

## 标准参考

- [ISO 17850:2015：Geometric distortion measurements](https://www.iso.org/standard/60819.html)
- [ISO 17957:2015：Shading measurements](https://www.iso.org/standard/31974.html)
- [ISO 14524:2009：OECF measurements](https://www.iso.org/standard/43527.html)
- [ISO 12233：Resolution and spatial frequency responses](https://www.iso.org/12233)
- [ISO 15739:2023：Noise measurements](https://www.iso.org/standard/82233.html)
- [ISO 17321-1:2012：DSC colour characterisation](https://www.iso.org/standard/56537.html)
- [ISO/CIE 11664-6:2022：CIEDE2000](https://www.iso.org/standard/82662.html)
- [IEC 62676-5:2018：Video surveillance camera image quality performance](https://webstore.iec.ch/en/publication/34391)
