# Camera Toolbox 相机内参标定原理与采集规范

## 1. 文档目的

本文说明 Camera Toolbox **当前实际使用的单目内参标定算法**，并给出与该实现兼容的棋盘采集、结果检查和补拍方法。内容覆盖：

- 针孔投影、平面标定和畸变模型原理；
- Camera Toolbox 从 PNG 输入到 OpenCV 求解的真实调用链；
- 当前固定 12 参数模型的参数含义和可观测性；
- 在尽量减少快门次数时，如何安排单块棋盘的位置、倾斜和尺度；
- 当前软件已经提供的检查能力，以及仍需人工或外部工具完成的验收。

本文中的“当前实现”均来自仓库代码；“采集建议”和“后续能力”不会被描述成软件已经自动实现的功能。

## 2. 当前实现边界

| 能力 | 当前状态 | 说明 |
|---|---|---|
| 标定图案 | 已实现 | 单块完整棋盘格，默认 `11 x 8` 内角点、相邻角点距离 `40.0 mm` |
| 输入格式 | 已实现 | PNG encoded bytes；其他图像格式不能直接进入当前标定后端 |
| 角点检测 | 已实现 | `findChessboardCorners` + `cornerSubPix` |
| 相机模型 | 已实现 | OpenCV pinhole + rational radial/tangential + thin-prism，共 12 个畸变参数 |
| 初始内参 | 已实现 | 自动生成，也可由 GUI 手工填写 |
| 最少视图门禁 | 已实现 | 至少 3 张启用且成功检测、分辨率一致的图像 |
| 重投影检查 | 已实现 | 全局 RMS、逐帧 RMSE、逐帧最大误差、观测点/投影点及残差向量 |
| 空间覆盖热图 | 已实现 | 角点位置密度热图，不参与优化和自动验收 |
| JSON/YAML/EEPROM | 已实现 | JSON 与 YAML 保留完整 D12；EEPROM 写入 D8，并将 4 个 thin-prism 槽位强制清零，见第 12 节 |
| ChArUco/AprilGrid | 未实现 | 当前检测器要求一块完整棋盘，不支持编码板或部分出画 |
| 鱼眼模型 | 未实现 | 当前不是 OpenCV fisheye/Kannala–Brandt 模型 |
| 自动下一最佳姿态 | 未实现 | 当前不计算信息增益或自动提示补拍姿态 |
| 参数协方差/条件数 | 未实现 | 当前不输出内参协方差、相关矩阵或信息矩阵 |
| 自动质量判定 | 未实现 | 当前没有固定 RMS、姿态分布或覆盖率通过阈值 |
| 多平面同帧标定 | 未实现 | 当前每张图只检测一块棋盘，并为该图建立一个平面姿态 |

> 重要：软件允许 3 张图片开始计算，只说明输入达到当前运行下限，不说明结果已经达到工程精度。

## 3. 坐标系、单位和符号

### 3.1 棋盘坐标系

当前实现按行优先生成棋盘三维点：

$$
\mathbf P_{r,c}=
\begin{bmatrix}
c\,d\\
r\,d\\
0
\end{bmatrix}
$$

其中：

- $c$ 为内角点列号；
- $r$ 为内角点行号；
- $d$ 为相邻内角点的物理距离，即 GUI 中的 `Square size (mm)`；
- 所有点的 $Z=0$，因此每张输入图片都是一个平面标定视图。

默认棋盘为 `11 x 8` 个内角点，共 88 个点。默认 $d=40.0\ \text{mm}$。

### 3.2 外参方向

每帧外参满足：

$$
\mathbf P_c=R_i\mathbf P_b+t_i
$$

即 **board frame → camera frame**。

当前结果中的：

- `rotation_vector` 是 OpenCV Rodrigues 旋转向量；
- `translation_vector` 的单位与 `square_size` 相同；若 `square_size` 使用 mm，则平移也是 mm；
- 错误的方格尺寸通常首先造成平移尺度错误，板材非均匀缩放或翘曲还会进一步污染内参与畸变。

### 3.3 图像坐标

角点坐标 $(u,v)$ 使用 OpenCV 像素坐标：

- 原点位于图像左上；
- $u$ 向右；
- $v$ 向下；
- 单位为 pixel。

GUI 将 OpenCV 像素中心映射到纹理时显式加入半像素偏移，这只影响预览绘制，不改变标定输入和求解结果。

## 4. 针孔投影模型

空间点经过外参变换后：

$$
\begin{bmatrix}
X_c\\Y_c\\Z_c
\end{bmatrix}
=R
\begin{bmatrix}
X\\Y\\Z
\end{bmatrix}+t
$$

归一化成像坐标为：

$$
x=\frac{X_c}{Z_c},\qquad y=\frac{Y_c}{Z_c}
$$

当前 Camera Toolbox/OpenCV 模型的相机矩阵为：

$$
K=
\begin{bmatrix}
f_x&0&c_x\\
0&f_y&c_y\\
0&0&1
\end{bmatrix}
$$

其中：

- $f_x,f_y$：以像素为单位的水平、垂直焦距；
- $c_x,c_y$：主点；
- 当前模型不估计 skew，矩阵 $(0,1)$ 元素保持为 0。

若暂时忽略畸变，像素投影为：

$$
\lambda
\begin{bmatrix}
u\\v\\1
\end{bmatrix}
=
K
\begin{bmatrix}
R&t
\end{bmatrix}
\begin{bmatrix}
X\\Y\\Z\\1
\end{bmatrix}
$$

内参绑定具体成像模式。分辨率、裁剪、binning、电子防抖 ROI、对焦位置或变焦状态改变后，不能默认继续使用同一套参数。

## 5. 当前 12 参数畸变模型

当前固定启用：

```text
CALIB_USE_INTRINSIC_GUESS
| CALIB_RATIONAL_MODEL
| CALIB_THIN_PRISM_MODEL
```

OpenCV 系数顺序固定为：

```text
[k1, k2, p1, p2, k3, k4, k5, k6, s1, s2, s3, s4]
```

令：

$$
r^2=x^2+y^2
$$

rational 径向缩放为：

$$
L(r)=
\frac{1+k_1r^2+k_2r^4+k_3r^6}
     {1+k_4r^2+k_5r^4+k_6r^6}
$$

带切向和 thin-prism 项的归一化畸变坐标为：

$$
\begin{aligned}
x_d={}&xL(r)+2p_1xy+p_2(r^2+2x^2)+s_1r^2+s_2r^4\\
y_d={}&yL(r)+p_1(r^2+2y^2)+2p_2xy+s_3r^2+s_4r^4
\end{aligned}
$$

最终像素坐标：

$$
u=f_xx_d+c_x,\qquad v=f_yy_d+c_y
$$

各参数的主要作用：

| 参数 | 主要含义 | 最需要的观测 |
|---|---|---|
| $k_1,k_2,k_3$ | 径向模型分子 | 从中心到四角的宽半径覆盖 |
| $k_4,k_5,k_6$ | 径向模型分母 | 高质量边缘/四角点和充分姿态冗余 |
| $p_1,p_2$ | 切向畸变 | 四个象限的非对称位置观测 |
| $s_1,s_2$ | thin-prism 水平分量 | 全画面二维分布和高精度角点 |
| $s_3,s_4$ | thin-prism 垂直分量 | 全画面二维分布和高精度角点 |

该模型自由度高。它能描述比普通 5 参数 pinhole-radtan 更复杂的误差，但也更容易把以下问题吸收到高阶系数中：

- 标定板翘曲或打印比例误差；
- 角点模糊、反光或错误定位；
- 图片姿态近似重复；
- 四角没有观测，只依靠中心数据外推；
- 多个对焦或分辨率状态被混在同一数据集中。

因此，当前固定 12 参数模型对采集多样性和留出验证的要求高于普通 5 参数模型。

## 6. 平面标定原理

### 6.1 单应矩阵

对当前平面棋盘，$Z=0$：

$$
\lambda\tilde{\mathbf p}
=K
\begin{bmatrix}
r_1&r_2&t
\end{bmatrix}
\tilde{\mathbf P}
=H\tilde{\mathbf P}
$$

其中：

$$
H=[h_1,h_2,h_3]=K[r_1,r_2,t]
$$

旋转矩阵前两列满足：

$$
r_1^Tr_2=0,\qquad r_1^Tr_1=r_2^Tr_2
$$

令：

$$
B=K^{-T}K^{-1}
$$

每个平面姿态可提供两条内参约束：

$$
h_1^TBh_2=0
$$

$$
h_1^TBh_1=h_2^TBh_2
$$

这解释了为什么多个不平行的棋盘姿态可以恢复内参，也解释了当前应用层为什么至少要求 3 张图片。

但是：

- 3 张只是一般平面内参问题的代数起点；
- 当前又额外估计 12 个畸变参数；
- 若三张图都近似正视、都在中心或姿态相似，约束仍然接近退化。

### 6.2 当前实现与张正友方法的关系

张正友方法通常描述为：

```text
多平面单应矩阵
   → 线性内参初值
   → 每帧外参初值
   → 内参、畸变、外参联合非线性优化
```

Camera Toolbox **没有自行实现单应矩阵闭式求解**。当前流程是：

1. GUI 生成或接收初始 $K$；
2. 12 个畸变参数初始为 0；
3. 将所有平面三维点、角点、初始 $K$ 和固定 flags 交给 OpenCV `calibrateCamera`；
4. 由 OpenCV 完成每帧外参估计和全局非线性优化。

因此，平面单应约束是理解该算法可观测性的理论基础，而实际求解入口是 OpenCV `calibrateCamera`。

## 7. 联合非线性优化

对第 $i$ 张图、第 $j$ 个棋盘角点：

$$
e_{ij}
=
\mathbf z_{ij}
-
\pi(K,D,R_i,t_i,\mathbf P_j)
$$

其中：

- $\mathbf z_{ij}$：检测到的亚像素角点；
- $D$：12 个畸变参数；
- $R_i,t_i$：第 $i$ 张图的棋盘到相机外参；
- $\pi(\cdot)$：第 4、5 节所述完整投影函数。

OpenCV 求解的目标可写成：

$$
\min_{K,D,\{R_i,t_i\}}
\sum_i\sum_j\left\|e_{ij}\right\|_2^2
$$

当前实现没有给不同角点设置协方差权重，也没有在优化目标外包裹稳健损失。因此，明显错误角点或低质量帧应在进入求解前排除，而不能依赖优化器自动降权。

当前停止条件：

- 最大迭代次数：30；
- epsilon：`f64::EPSILON`；
- 条件类型：`COUNT | EPS`。

由于 epsilon 极小，实际运行通常主要受 30 次迭代上限约束。

## 8. Camera Toolbox 当前调用链

```text
CalibrationWorkspace
   │
   ├ PNG IHDR metadata preflight
   ├ 有界读取 encoded PNG
   ▼
OpenCvCalibrationBackend::detect_png
   ├ 校验 PNG signature 与解码内存预算
   ├ imdecode(IMREAD_COLOR)
   ├ BGR → Gray
   ├ findChessboardCorners
   │    └ ADAPTIVE_THRESH | NORMALIZE_IMAGE
   └ refine_detected_corners
        ├ 相邻角点间距 P10 → h=clamp(round(0.25*P10),3,11)
        ├ cornerSubPix(Size(h,h), zeroZone=(-1,-1), max 100, epsilon 1e-4)
        ├ 未移动点 → 先以当前 h 做 0.25 px 扰动复验；稳定则保留 h
        ├ 接近 0.8h / 复验不稳定且 h<11 → 从初值以 h=11 重试
        └ 最终仍接近阈值或复验不稳定 → 拒绝该帧
   │
   ▼
CalibrationSession::install_detection
   ├ 校验角点数量 = rows × cols
   ├ 校验坐标均为有限值
   └ 只保留当前 source version 对应结果
   │
   ▼
CalibrationSession::calibration_snapshot
   ├ 只选 enabled + Found
   ├ 所有图像必须同分辨率
   └ 至少 3 个视图
   │
   ▼
OpenCvCalibrationBackend::calibrate
   ├ object points = (column*d, row*d, 0)
   ├ initial K + 12 zero distortion coefficients
   ├ calibrateCamera(fixed Pangbot flags)
   ├ projectPoints per view
   └ per-view RMSE / max error
   │
   ▼
CalibrationSession::install_solution
   ├ 再次校验 flags、尺寸、view/point 数量和有限值
   ├ GUI 显示角点、投影点、残差向量、RMSE、热图
   └ JSON / YAML / EEPROM 导出
```

对应实现：

| 层 | 文件 | 职责 |
|---|---|---|
| Core | [`calibration.rs`](../crates/core/src/calibration.rs) | 棋盘、请求、解、固定 flags 和不变量 |
| App | [`calibration.rs`](../crates/app/src/calibration.rs) | 数据集状态、至少 3 帧、同尺寸快照、结果事务安装 |
| App port | [`calibration.rs`](../crates/app/src/ports/calibration.rs) | 检测和标定后端接口 |
| OpenCV adapter | [`calibration.rs`](../crates/adapters/src/calibration.rs) | PNG 解码、棋盘检测、亚像素优化、`calibrateCamera`、重投影 |
| GUI | [`calibration_workspace.rs`](../crates/frontends/gui/src/calibration_workspace.rs) | 参数输入、后台任务、预览、热图和导出 |
| YAML | [`calibration_yaml.rs`](../crates/core/src/calibration_yaml.rs) | 按 OpenCV 顺序保存完整 12 畸变参数的固定布局 YAML |
| EEPROM | [`calibration_eeprom.rs`](../crates/core/src/calibration_eeprom.rs) | 4 个内参、前 8 个畸变参数以及强制为 0 的 `s1..s4` 设备映射 |

## 9. 当前参数和默认值

| 参数 | 当前值 | 对结果的影响 |
|---|---:|---|
| 默认棋盘内角点 | `11 x 8` | 每张成功图产生 88 个对应点 |
| 默认相邻角点距离 | `40.0 mm` | 决定外参平移单位和尺度 |
| 棋盘尺寸合法范围 | 每轴 `2..=256` | 只保证数据结构有效，不保证光学可用 |
| 输入 | PNG | 当前 OpenCV 标定后端的固定输入契约 |
| 检测 flags | adaptive threshold + normalize image | 改善不同亮度下的完整棋盘检测 |
| 亚像素搜索邻域 | 逐帧 `h = clamp(round(0.25*P10),3,11)`；`winSize=Size(h,h)`、`zeroZone=Size(-1,-1)` | `P10` 来自棋盘水平/垂直相邻初始角点间距；实际邻域为 `(2h+1) x (2h+1)`，最大 `23 x 23`；`P10 < 12 px` 时拒绝该帧 |
| 亚像素停止条件 | 100 次或 `1e-4` | 控制角点细化收敛 |
| 最少视图 | 3 | 运行下限，不是质量门槛 |
| 自动 $f_x,f_y$ 初值 | `max(width,height)` | `USE_INTRINSIC_GUESS` 的焦距初值 |
| 自动 $c_x,c_y$ 初值 | 图像中心 | `USE_INTRINSIC_GUESS` 的主点初值 |
| 畸变初值 | 12 个 0 | 无镜头先验时的中性起点 |
| calibration flags | `49153` | 启用 intrinsic guess、rational、thin-prism |
| 标定停止条件 | 30 次或 `f64::EPSILON` | 通常由迭代次数停止 |
| 热图宽度 | 192 | 只影响 GUI 覆盖可视化 |
| 热图 Gaussian sigma | 4.2 个热图像素 | 对角点命中做平滑显示 |

标定 flags、亚像素上下限和质量门禁均由测试锁定。本文不建议在没有回归验证的情况下只为降低训练 RMS 而改变这些契约。

### 9.1 按角点间距动态选择窗口

Camera Toolbox 已根据 `findChessboardCorners` 的初始角点间距，为每张图动态设置 `cornerSubPix` 半窗口；这可适应近景、远景和强透视图像中的投影尺度变化。

对已按棋盘行列排序的初始角点 $p_{r,c}$，先收集所有水平和垂直相邻距离：

$$
S=\left\{\lVert p_{r,c+1}-p_{r,c}\rVert_2,
          \lVert p_{r+1,c}-p_{r,c}\rVert_2\right\}.
$$

当前实现使用 $S$ 的低分位数而不是均值或原始最小值：均值可能忽略强透视下远侧被压缩的格子，原始最小值又容易受异常点影响。具体策略为：

$$
d_{10}=P_{10}(S),\qquad
h=\operatorname{clamp}\!\left(\operatorname{round}(0.25d_{10}),3,11\right).
$$

随后传入 `winSize=Size(h,h)`，实际搜索邻域为 `(2h+1) x (2h+1)`；`zeroZone` 保持 `Size(-1,-1)`。`0.25`、下限 `3`、上限 `11` 是当前固定工程契约；当 $d_{10}<12$ px 时，当前实现将该帧判为 `NotFound`，避免在格子欠采样时强行使用最小窗口。

OpenCV 5.x 在细化结果相对输入初值的任一轴位移超过半窗口时，会把该点静默恢复为初值。当前实现因此保留细化前角点，并统计位移为零及达到 `0.8h` 的点：达到 `0.8h` 时直接进入回退/拒绝路径；未移动点先在当前 `h` 下向图像中心扰动 `0.25` px 复验，只有无法回到最终点的两个轴 `0.05` px 范围内时才从原始初值以 `h=11` 重试。稳定未移动点保留首选动态窗口，不会导致整帧旁路到 `h=11`；重试结果继续使用相同门禁，仍不稳定时拒帧。

OpenCV 一次 `cornerSubPix` 调用只接受一组全局 `winSize`，因此当前实现是逐帧一个窗口，而非逐角点窗口。动态窗口不能补救错误棋盘拓扑、严重模糊或过小格子；这些情况由间距门禁或稳定性复验拒绝。

## 10. 参数可观测性和退化采集

### 10.1 参数需要什么观测

| 参数或参数组 | 主要约束来源 | 典型耦合 |
|---|---|---|
| $f_x,f_y$ | 两个轴上的明显倾斜、不同投影尺度 | 与 $t_z$、棋盘尺度耦合 |
| $c_x,c_y$ | 四象限的非对称观测、相反方向倾斜 | 与横向/纵向平移耦合 |
| $k_1..k_6$ | 中心到四角的连续半径覆盖 | 高阶项互相补偿并与焦距耦合 |
| $p_1,p_2$ | 四象限、偏心且方向多样的角点 | 与主点、板形误差耦合 |
| $s_1..s_4$ | 全画面二维覆盖和高精度边缘角点 | 与切向畸变、板翘曲耦合 |
| 每帧 $R,t$ | 足够大的完整棋盘和透视形变 | 与内参共同优化 |

从 Jacobian 角度，重投影残差为 $r$、参数为 $\theta$：

$$
J=\frac{\partial r}{\partial\theta},\qquad
H=J^TJ
$$

如果 $H$ 存在很小特征值，表示某些参数组合对当前数据产生近似相同的投影，问题接近不可观测。当前软件不计算该矩阵的条件数或内参协方差，因此必须通过采集设计和稳定性复算规避。

### 10.2 常见退化方式

以下数据即使超过 3 张，也可能得到不稳定结果：

1. 所有棋盘几乎正对相机；
2. 只改变棋盘距离，不改变平面法向；
3. 只绕光轴做 roll；
4. 所有倾斜都绕同一个轴；
5. 所有角点集中在画面中心；
6. 四角和最外侧半径没有角点；
7. 所有图片的棋盘投影尺寸几乎一致；
8. 倾斜太小，透视差异不明显；
9. 倾斜太大，使远侧格子严重压缩或模糊；
10. 自动对焦、裁剪或分辨率在数据集中发生变化；
11. 棋盘翘曲、反光、打印比例不准；
12. 只根据低训练 RMS 接受 12 参数高阶模型。

roll 能改变棋盘边缘相对像素方向，有助于均衡检测误差，但不会改变平面法向，不能代替 pitch/yaw。

## 11. 与当前算法兼容的低快门采集方案

### 11.1 快门数量应如何理解

- 3 张：当前软件和一般平面内参问题的运行/代数起点；不建议作为交付数量。
- 7 张：可作为低快门首轮候选集；只有通过覆盖、残差和稳定性检查时才停止。
- 10–20 张：当镜头畸变强、角点噪声较大或 12 参数不稳定时，更保守的采集范围。
- 另保留至少 1 张不参与最终拟合的验证图；当前 GUI 不能直接计算该留出图的 PnP 重投影误差，需要外部工具或后续功能支持。

不存在对所有镜头都成立的固定最少帧数。当前软件也没有自动信息增益停止条件。

### 11.2 拍摄前固定条件

- 固定分辨率、ROI、binning 和图像方向；
- 固定 focus/zoom；关闭会改变内参的自动对焦流程；
- 标定过程中相机和棋盘在曝光时保持静止；
- 避免过曝、欠曝、反光和 rolling-shutter 运动形变；
- 棋盘刚性、平整、哑光，物理尺寸经过测量；
- 当前检测器要求完整棋盘，所有内角点必须留在画面内；
- 最严重透视压缩方向的格边建议仍不小于约 20 px；
- 外侧角点接近边界时仍保留约 3%–5% 图像边距。

### 11.3 七姿态首轮候选集

以下角度为采集目标，不要求机械装置精确到整数角度。yaw/pitch 正负表示相反方向。

| 编号 | 图案位置 | yaw | pitch | roll | 投影尺度 | 主要作用 |
|---|---|---:|---:|---:|---|---|
| C | 中心 | $0^\circ$ | $+25^\circ$ | $0^\circ$ | 中等 | 第一种法向、中心约束 |
| TL | 左上 | $+30^\circ$ | $+20^\circ$ | $+20^\circ$ | 中等 | 第二种法向、左上边缘 |
| BR | 右下 | $-30^\circ$ | $-20^\circ$ | $+20^\circ$ | 中等 | 第三种法向、与 TL 相反 |
| TR | 右上 | $-30^\circ$ | $+20^\circ$ | $-20^\circ$ | 中等 | 右上径向/切向约束 |
| BL | 左下 | $+30^\circ$ | $-20^\circ$ | $-20^\circ$ | 中等 | 左下径向/切向约束 |
| N | 中心附近 | $+38^\circ$ | $0^\circ$ | $+30^\circ$ | 大 | 近距离、强 yaw、尺度变化 |
| F | 中心附近 | $0^\circ$ | $-38^\circ$ | $-30^\circ$ | 小 | 远距离、强 pitch、尺度变化 |

建议执行顺序：

```text
C + TL + BR
   │
   ▼
第一次标定
   ├ 检查每帧 RMSE / 最大误差
   ├ 检查残差向量
   └ 检查角点密度热图
   │
   ▼
从 TR / BL / N / F 中补最明显缺失项
   │
   ▼
每增加一张立即重新标定
   │
   ▼
直到空间、姿态、尺度和稳定性检查全部通过
```

由于当前没有自动下一最佳姿态功能，补拍选择是人工规则：

1. 热图四角缺失：优先补对应角落；
2. 所有图片透视方向相似：补相反 yaw/pitch；
3. 棋盘投影尺寸接近：补近距离大图案或远距离小图案；
4. 某一象限残差明显偏大：重新拍摄该象限，先排除模糊和板翘曲；
5. 删除某一张后参数变化很大：补与该帧相近但质量更高的独立姿态，而不是重复连拍。

### 11.4 当前算法不支持一帧多平面架

多平面编码标定架可以从理论上在一次快门中提供多个平面法向，但当前 Camera Toolbox：

- 每张 PNG 只调用一次 `findChessboardCorners`；
- 只接受一个 `BoardSpec`；
- 为一张图建立一组平面 object points 和一组外参；
- 不支持 ChArUco/AprilGrid ID，也不支持同帧多个独立平面 observation。

因此，不能把多个不同姿态的小棋盘放进同一张图后直接交给当前程序。若未来支持该方案，需要修改检测模型、数据结构和联合优化变量；在此之前应使用单块棋盘多姿态采集。

## 12. 当前误差、覆盖和导出语义

### 12.1 全局和逐帧误差

当前保存三类重投影信息：

- `solution.rms_error`：OpenCV `calibrateCamera` 返回的全局 RMS；
- `view.reprojection_rmse`：当前代码按一帧所有角点的二维欧氏距离计算：

$$
\operatorname{RMSE}_i=
\sqrt{\frac{1}{N_i}\sum_j
\left((u_{ij}-\hat u_{ij})^2+(v_{ij}-\hat v_{ij})^2\right)}
$$

- `view.max_reprojection_error`：该帧最大的二维欧氏重投影误差。

GUI 还能叠加：

- 观测角点；
- 模型投影点；
- 从观测点到投影点的残差向量。

低 RMS 只能说明模型在训练数据上的拟合程度。高自由度模型可能在姿态不足时通过互相补偿得到低 RMS，因此还必须检查空间分布、残差结构和删帧稳定性。

### 12.2 角点覆盖热图

当前热图流程：

1. 只统计 `enabled + Found` 图片；
2. 将角点归一化映射到宽度 192 的热图；
3. 每个角点位置累加一次命中；
4. 使用 Gaussian blur 平滑；
5. 按当前最大密度归一化着色。

因此它能回答：

- 哪些图像区域有角点；
- 中心是否过密、边缘或四角是否缺失；
- 启用/禁用某帧后空间覆盖如何变化。

它不能回答：

- 棋盘法向是否多样；
- pitch/yaw 是否充分；
- 尺度是否充分；
- 参数是否可观测；
- 绝对角点数量是否达到固定阈值；
- 当前标定是否通过。

热图按峰值归一化，不同数据集之间的颜色强度不能直接作为绝对数量比较。

### 12.3 导出差异

| 格式 | 保存内容 | 适用范围 | 关键限制 |
|---|---|---|---|
| `camera_intrinsics.json` | schema、算法名、棋盘、初值、全部数据项、完整 solution、逐帧结果 | 审计、复算、完整结果保存 | 文件最大，包含数据集路径/状态信息 |
| `camera_intrinsics.yaml` | $f_x,f_y,c_x,c_y$、完整 $D12=[k_1,k_2,p_1,p_2,k_3,k_4,k_5,k_6,s_1,s_2,s_3,s_4]$、width、height | 当前 OpenCV rational + thin-prism 标定结果交换 | 固定文本布局；消费端必须支持完整 D12 及 OpenCV 系数顺序 |
| `camera_eeprom.bin` | width/height、4 个内参、$k_1,k_2,p_1,p_2,k_3,k_4,k_5,k_6$，以及 4 个值为 0 的 `s1..s4` 槽位 | 当前 Yg Stereo EEPROM 流程 | 输入可以是 D8 或 D12，但 `validated_distortion()` 只复制前 8 项并强制清零 thin-prism；参数转为 `f32`，绑定具体 EEPROM map |

JSON 与 YAML 保存相同的完整 D12，EEPROM 仍是独立的 D8 降阶协议：

```text
D_JSON = D_YAML = [k1, k2, p1, p2, k3, k4, k5, k6, s1, s2, s3, s4]
D_EEPROM = [k1, k2, p1, p2, k3, k4, k5, k6,  0,  0,  0,  0]
```

JSON 和 YAML 均保留当前求解得到的完整 D12。EEPROM 虽然预留 12 个系数槽位，但 `s1..s4` 永远写 0，不能称为完整 D12；直接清零 thin-prism 也不等价于在 D8 模型下重新优化。

EEPROM 必须使用独立数据验证降阶误差。应在完整标定 ROI 上比较相同 $K$ 下 D12 与 D8 去畸变映射：

$$
\Delta(u,v)=\left\|m_{D12}(u,v)-m_{D8}(u,v)\right\|_2
$$

其中 $m_D$ 表示下游实际使用的像素去畸变映射。$\Delta$ 的 P95 和最大值必须低于产品像素误差预算；否则不能把 EEPROM 结果作为完整标定解使用，应调整下游存储/模型，或使用禁用 thin-prism 后重新优化得到的 D8 标定结果。当前 Camera Toolbox 没有 D8 重拟合模式。

## 13. 验收和补拍清单

### 13.1 当前软件自动执行的门禁

- 输入必须是 PNG；
- 解码尺寸必须与 PNG metadata preflight 一致；
- 棋盘规格合法；
- 完整棋盘检测成功；
- 每帧角点数量精确等于 `rows x cols`；
- 所有坐标、初值和结果均为有限数；
- 焦距初值和结果为正；
- 至少 3 张启用且成功检测的图片；
- 参与标定的图片分辨率一致；
- 返回的 view/point 数量与请求一致；
- 返回 flags 与固定 Pangbot flags 一致。

### 13.2 当前需要人工执行的验收

建议至少检查：

- [ ] 中心、四边和四角都有角点；
- [ ] 最外侧半径在四个象限都有观测；
- [ ] yaw、pitch 均有正反方向，且不是只做 roll；
- [ ] 主要倾角约在 $20^\circ$–$40^\circ$，不存在大量近正视重复图；
- [ ] 至少有两个明显投影尺度；
- [ ] 每张图清晰、无反光、无曝光饱和和运动形变；
- [ ] 全局 RMS、逐帧 RMSE 和最大误差满足产品像素预算；
- [ ] 某帧 RMSE 不应明显高于数据集主体；
- [ ] 残差向量不存在明显径向、切向或局部同向结构；
- [ ] 删除任意一张图片重新标定时，内参和全画面畸变曲线保持稳定；
- [ ] 导出模型与下游实际消费的参数数量一致；
- [ ] 使用独立数据验证去畸变直线性或留出重投影误差。

MathWorks 文档把平均重投影误差小于 1 pixel 作为一般经验值，但该数字只适合作为宽松健康检查。实际门槛应由下游测量、定位或拼接误差预算决定，不能只依赖一个通用 RMS 数字。

### 13.3 手工删帧稳定性检查

当前 GUI 可以通过启用/禁用图片后重新标定，执行简化的 leave-one-view-out 检查：

1. 保存完整数据集 JSON；
2. 记录 $f_x,f_y,c_x,c_y$ 和 12 个畸变参数；
3. 每次禁用一张关键姿态，重新标定并导出；
4. 比较参数变化和全画面去畸变位移变化；
5. 若删除一张图导致参数大幅跳变，说明该姿态缺少独立冗余，应补拍，而不是直接接受结果。

参数变化阈值应按产品误差预算制定。不同量纲的系数不能只比较绝对数值，最好比较它们对全画面像素校正量的影响。

## 14. 常见问题诊断

| 现象 | 优先检查 | 不建议的处理 |
|---|---|---|
| 棋盘检测失败 | 行列是否填写为内角点数、整板是否可见、清晰度、反光、黑白格对比 | 直接增加畸变阶数 |
| RMS 很低但四角校正异常 | 四角是否有观测、高阶系数是否不稳定、消费端是否支持完整 D12 与 OpenCV 系数顺序 | 只看全局 RMS 接受结果 |
| 某一帧误差很大 | 模糊、错误角点、板翘曲、运动、分辨率或 focus 状态 | 让高阶参数吸收异常帧 |
| $f_x,f_y$ 随删帧明显变化 | pitch/yaw 不足、距离/尺度单一、姿态重复 | 重复拍相同正视姿态 |
| 主点漂到异常位置 | 数据分布不对称、一个象限缺失、平移与主点耦合 | 无证据地固定主点 |
| $k_4..k_6,s_1..s_4$ 波动很大 | 12 参数模型约束不足、边缘数据不足、板形误差 | 仅凭训练 RMS 保留全部系数 |
| YAML 与 EEPROM 去畸变不同 | EEPROM 仅保留 D8，并将 $s_1..s_4$ 清零 | 把两者视为同一模型 |
| 热图看似均匀但结果不稳 | 热图不包含姿态和尺度信息 | 把热图当作自动验收结论 |

## 15. 后续自动化方向

若要把“最少快门”从人工经验变成可证明的自动停止条件，建议按以下顺序扩展：

1. 从每帧外参计算棋盘法向、yaw/pitch/roll 和投影尺度；
2. 增加径向分桶、四象限和边缘覆盖指标；
3. 输出内参 Jacobian 的 Schur 信息矩阵、归一化条件数和参数协方差；
4. 为候选姿态计算预期信息增益，提示下一最佳姿态；
5. 增加保留集 PnP/重投影验证；
6. 再评估是否增加 ChArUco、多平面 observation 或 fisheye 模型；
7. 为 12 参数模型和 5 参数下游模型分别建立验收结果。

候选姿态的信息增益可使用：

$$
\Delta_D=
\log\det(\Lambda_\theta+\Delta\Lambda)
-
\log\det(\Lambda_\theta)
$$

或：

$$
\Delta_A=
\operatorname{tr}(\Sigma_\theta)
-
\operatorname{tr}\left((\Lambda_\theta+\Delta\Lambda)^{-1}\right)
$$

满足覆盖、协方差、残差和留出验证门槛后才停止，而不是达到固定张数后停止。

### 15.1 已实现的 RTSP Viewer 与手动快门链路

RTSP 已作为 Local、SFTP 之外的第三种输入入口接入 GUI：流帧先进入 Viewer，用户在 Calibration 工作区对当前显示帧显式按快门后，软件把同一不可变帧固化为会话内 PNG，再提交给现有权威检测流水线。它不是伪装成 `FileSystem` 的文件源：实时流没有稳定路径或文件版本，数据集项的统一边界是 Viewer 当前显示帧和 `StreamFrameIdentity`。

已实现的手动链路为：

```text
RTSP 解码帧 ──► Viewer displayed_frame
        │
        └─ 用户显式快门 ──► 会话内 PNG / CaptureStore
                              │
                              ▼
                 CalibrationDetectionPipeline
                              │
                 Found ───────┴──── NotFound / error
                   │
                   ▼
           Calibration Dataset item
```

Dataset 中的 RTSP 快门项保留 stream 来源，而不是本地或远端文件路径：

```text
stream_id
channel
frame_sequence
source_pts = Known { ticks, time_base, provenance } | Unavailable { reason }
host_monotonic_time_ns
```

- `frame_sequence` 是单条 stream 内的解码输出序号，不是 RTP packet sequence。
- `source_pts` 是 demux/decoder 输出的源帧时间戳；未知时必须显式标为 `Unavailable`，不能用主机到达时间或推测值冒充源 PTS。
- `host_monotonic_time_ns` 只用于同一 Camera Toolbox 进程内排序与延迟诊断，不能当作跨机器时钟。
- 内存 PNG 由 `CaptureStore` 持有；Dataset 项存在期间资产不得释放。导出仍是显式用户动作，不会隐式在本机或 X5 落盘截图。
- Viewer overlay 可显示棋盘检测结果和 coverage；权威 Dataset 状态以检测 worker 安装到 item 的 `Found` / `NotFound` / error 为准。

当前仍未修改 `DEMO233`、未部署端侧 helper，也未完成 CH0/CH3 共享 RTCP 或设备时钟证明。在证明双路共享时钟之前，两路时间戳只允许标为近似主机到达或未知，不能形成严格双目标定配对。

### 15.2 仍阻塞的 RTSP 自动准入

自动准入目前是 observe-only：软件可以展示观察指标和拒绝原因，但缺少以下两个契约时不得把 RTSP 帧自动提交到 Dataset，也不得显示“自动收集完成”或“标定通过”：

1. 带 schema/version 的 `AutoCaptureBaseline`：定义 field/depth/pose bin、目标计数、RMSE/间距/边距等阈值、近重复容差和来源数据集。
2. source-bound `InitialIntrinsicsBinding`：绑定初始内参、参考分辨率、orientation、crop/ROI、像素坐标约定、采集几何 key 与 digest。

缺少 baseline 或 binding 时的自动路径必须返回明确拒绝状态，例如 `RejectMissingBaseline` / `RejectMissingInitialIntrinsicsBinding`，并保持 Dataset 不变。显式预览和手动快门不受该自动准入阻塞影响：预览只更新 Viewer/候选观察结果；手动快门仍走 §15.1 的 Dataset 权威检测链路。

未来自动准入恢复后，必须仍经过两层边界，避免“检测到棋盘就入库”：

```text
最新 RTSP 解码帧
    │
    ├─ 观察/候选门禁（完整棋盘、边距、角点间距、baseline 阈值、binding digest）
    │
    └─ 通过 ──► 固化同一帧为内存 PNG
                         │
                         ▼
          CalibrationDetectionPipeline 的 PNG preflight + 权威检测
                         │
              Found ────┴──── NotFound / error / stale
                │                         │
                ▼                         └──► 释放候选资产，不改数据集
        提交 CalibrationSession
```

自动收集的分布标准沿用并强化第 11、12 节的人工规范，但阈值必须来自 `AutoCaptureBaseline`，不能在 GUI 中硬编码为通用产品规则。自动路径至少需要约束以下事实：完整棋盘、最小角点间距、图像边距、field coverage、当前内参下的 PnP depth span、互斥 pose bin、多样视图数量、近重复抑制、留出验证和产品误差预算。

### 15.3 自动准入的可执行门禁契约

自动入集不能使用“差异明显”“覆盖不足”等主观描述，也不能只依赖原始图像透视 proxy。计划中的准入计算以按行列排序的角点 $q_{r,c}=(u_{r,c},v_{r,c})$、当前 `InitialIntrinsicsBinding` 和 `AutoCaptureBaseline` 为输入，至少产出以下 observation feature：

- `field_coverage`：由 refined corner footprint 落入固定图像网格得到；用于显示和候选补拍建议。
- `pnp_depth_span`：使用当前绑定内参直接 PnP 后得到的归一化深度 bin；内参 digest 改变时必须 stale。
- `pnp_pose_bin = (tilt_magnitude_bin, azimuth_sector)`：同一 observation 只能贡献一个互斥 pose bin；roll 不能伪装为法向 diversity。
- `raw_perspective_descriptor`：只允许用于 preview、去重辅助或 bootstrap nomination，不能单独满足 `CollectionComplete`。

所有距离、PnP 位姿、重投影误差和派生指标必须有限且在 baseline 允许范围内，否则候选帧只能被拒绝或保持 observe-only。某 candidate 只有在自身至少增加一个未满足的 baseline 目标时才有正增益；若它与已接受 observation 在所有绑定特征上低于 near-identical 容差，则必须拒绝。

`CollectionComplete` 只能在以下条件同时成立时显示：安装了 active baseline；存在可信且 source-bound 的 `InitialIntrinsicsBinding`；baseline digest、binding digest、BoardSpec、图像尺寸、orientation、crop/ROI 与检测证据全部匹配；field/depth/pose 目标和留出验证均通过。对应单元测试必须使用完整 baseline fixture 精确断言 feature、bin、增益、stale/reject 状态和去重结果，不能把“临时测得一组阈值”误写成通用测试契约。

## 16. 参考资料

- Zhengyou Zhang, [A Flexible New Technique for Camera Calibration](https://www.microsoft.com/en-us/research/wp-content/uploads/2016/02/tr98-71.pdf)。平面单应约束、线性初始化和联合优化的基础。
- OpenCV, [Camera Calibration and 3D Reconstruction](https://docs.opencv.org/4.x/d9/d0c/group__calib3d.html)。`calibrateCamera`、rational 和 thin-prism 模型定义。
- MathWorks, [Using the Single Camera Calibrator App](https://www.mathworks.com/help/vision/ug/using-the-single-camera-calibrator-app.html)。闭式初值与 Levenberg–Marquardt 联合优化说明。
- MathWorks, [Prepare Camera and Capture Images for Camera Calibration](https://www.mathworks.com/help/vision/ug/prepare-camera-and-capture-images-for-camera-calibration.html)。标定板位置、倾斜和图像数量建议。
- Peng and Sturm, [Calibration Wizard: A Guidance System for Camera Calibration Based on Modelling Geometric and Corner Uncertainty](https://openaccess.thecvf.com/content_ICCV_2019/html/Peng_Calibration_Wizard_A_Guidance_System_for_Camera_Calibration_Based_on_ICCV_2019_paper.html)。下一最佳姿态和角点不确定性。
- Calib.io, [Calibration Best Practices](https://calib.io/blogs/knowledge-base/calibration-best-practices)。标定板尺寸、画面覆盖、倾斜和采集实践。
