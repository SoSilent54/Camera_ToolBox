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
| 初始内参 | 已实现 | 自动生成、GUI 手工填写，或从已安装标定解回写 $K$ |
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
   ├ initial K + editable D12 seed（自动初始内参时 D12=0）
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

导出入口位于 GUI 的 `Calibration result` 折叠区域末尾；`EEPROM Provisioning` 是其下方独立折叠区域，因此收起标定结果不会隐藏 EEPROM 写入状态与确认弹窗。

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
- Viewer overlay 可显示棋盘检测结果和 coverage；`Fit` 右侧的 `Flip X` 只影响 Viewer 显示层，把图像、coverage、检测点、ROI/姿态轴等显示层一起水平镜像，不改变不可变帧、Dataset 图片、检测结果、PnP 或导出内容。权威 Dataset 检测状态以 worker 安装到 item 的 `Found` / `NotFound` / error 为准。

当前仍未修改 `DEMO233`、未部署端侧 helper，也未完成 CH0/CH3 共享 RTCP 或设备时钟证明。在证明双路共享时钟之前，两路时间戳只允许标为近似主机到达或未知，不能形成严格双目标定配对。

### 15.2 已实现的运行时 RTSP 自动准入（非生产资格）

Auto Capture 不再依赖命名 Profile、保存/加载文件或 Apply 动作。每个已显示的直播上下文都在内存中由以下当前值即时构造一对不可变契约：

1. `AutoCaptureBaseline`：精确采集键、完整图像尺寸、`BoardSpec` 和当前 Dataset Acceptance 阈值；
2. `InitialIntrinsicsBinding`：当前 GUI 初始 $K+D12$、完整帧尺寸、upright orientation、OpenCV 像素中心约定和同一采集键。

合法阈值编辑立即替换当前上下文的契约；未完成或无效的临时文本只在同一 source/geometry/board/$K+D12$ 上保留最后一份合法契约。source、图像几何或棋盘变化会先取消在途候选并使旧契约失效，绝不跨上下文复用。旧的 profile 文件不会被读取或写入。

自动候选仍必须经过权威路径，不能因检测到棋盘直接入库：

```text
Viewer displayed_frame
    │
    ▼
冻结同一帧为会话内 PNG，并绑定 source / revision / board / admission revision
    │
    ▼
CalibrationDetectionPipeline 权威棋盘检测
    │
    ├─ NotFound / error / stale ──► 释放候选资产，不改 Dataset
    │
    └─ Found
         │
         ▼
后台 PnP（冻结的当前 K+D12 与 BoardSpec）
         │
         ▼
重新校验 token、采集键、binding digest、图像尺寸和 admission revision
         │
         ├─ 无新 Found / field / depth / pose 目标截断增益 ──► 拒绝，不改 Dataset
         └─ 正覆盖增益 ──► 原子提交 Dataset item、来源无关 PnP 证据与 source-bound PnP 证据
```

PnP 证据必须具有有限的位姿和重投影指标，棋盘所有角点在相机坐标系中的深度必须严格为正，且 RMSE 与最大重投影误差必须不超过当前阈值。手动快门仍走 §15.1；关闭 Auto Capture 时，显式预览仍只更新 Viewer，不会自动入库。

这些阈值由当前操作员会话定义，因而仅支持临时采集，不构成产品资格、`CollectionComplete` 或标定通过声明。

### 15.3 Dataset Acceptance 控件与实时进度

`Dataset acceptance` 使用固定折叠标识；检测完成、进度变化或阈值编辑不会重置其展开状态。展开体位于独立垂直滚动区域内，视口最小约 96 px、最大约 420 px，并为下方 Dataset 表保留约 180 px 可操作高度，避免小窗口中验收控件把表格顶出侧栏。控件按指标与可视化就地分组：

- Found views：当前值、目标和进度条；
- field coverage：当前 field quota 进度、occupied cell 参考值、每个图像归一化网格单元中的棋盘角点数量，以及网格/每单元目标/最小相邻角点间距；非零角点数的单元才算 occupied field cell，但 Gain 在该单元达到 `Field target / cell` 前都会继续增加；
- depth coverage：当前 depth quota 进度、occupied depth bin 参考值、连续深度区间图和每段棋盘内角点深度计数，以及深度范围/bin/每 bin 目标；最后一个区间包含上边界；不再单独列出缺失区间，红到绿的图形颜色直接表达未达标到达标；
- pose coverage：当前 pose quota 进度、occupied pose bin 参考值、中心 front-parallel bin 与 tilt $\times$ azimuth 环形扇区图、每区 view 计数，以及 deadband/最大 tilt/bin/sector/每 bin 目标；显示方向对齐 OpenCV 图像坐标，$0^\circ$ 位于右侧 $+x$，$90^\circ$ 位于下方 $+y$。每段环弧以凸 mesh 四边形填充，并单独描绘内外弧与径向边框；当 deadband 大于 0 时，全部环带绘制完成后，中心 bin 的填充、边框和文字最后作为圆形遮罩绘制；当 deadband 为 0 时，不存在中心 bin，也不绘制中心完整圆，环形扇区从中心直接开始；
- PnP quality gates：RMSE、最大单点重投影误差阈值和 `Minimum auto Gain`。只有同时满足有限值、正深度、当前 Dataset PnP binding 和这两个重投影门限的证据，才能填充 depth 和 pose 覆盖。既有 Dataset 项在每次评估时也必须通过当前图像边界和最小相邻角点间距门限；提高 spacing 阈值会立刻将不满足项移出汇总与边际贡献。

默认值为：

- Found views：3；field grid：$16\times9$，`Field target / cell`：1，最小相邻角点间距：12 px；
- PnP depth：400–2400（4 bins，`Depth target / bin`：1）；深度单位是配置的 `BoardSpec::square_size` 单位，GUI 默认棋盘使用 mm；
- PnP tilt：deadband $5^\circ$、最大 $65^\circ$（3 bins），azimuth：8 sectors，`Pose target / bin`：1；
- PnP RMSE：最多 1.5 px；最大单点重投影误差：最多 4.0 px；`Minimum auto Gain`：1。

编辑 Dataset Acceptance 文本框时允许临时不完整输入；焦点仍在文本框内时不会弹出红色错误或替换当前 runtime admission，而是继续使用上一组完整合法门限。字段补全为合法值后立即安装；离开编辑焦点后仍非法才显示错误。

普通 Dataset 的 PnP binding 不依赖 live source。每次本地/SFTP PNG 读取完成或手动 RTSP 快门帧进入检测时，GUI 用当前可见的 $f_x/f_y/c_x/c_y$ 和可编辑 D12 seed 为该图片尺寸创建来源无关 binding；`auto_intrinsics` 打开时使用 `fx=fy=900`、`cx=w/2`、`cy=h/2`、`D12=0`。如果检测期间 GUI K/D12、图片尺寸或 binding digest 变化，返回的旧 PnP 会被丢弃；修改 $f_x/f_y/c_x/c_y$、D12 seed 或仅 square size 变化后，GUI 会对既有 `Found` Dataset 项异步刷新/补算来源无关 PnP，不需要重新点击 `Detect`。角点布局变化仍会使检测本身失效，必须重新 Detect。

Overlay 与 Input image 预览模式会同时绘制三类图像空间标记：绿色为检测角点，红色为已安装完整标定解的逐点重投影残差向量，蓝色为当前 GUI $K+D12$ 下 Dataset PnP 的重投影点。棋盘姿态坐标轴也使用当前 Dataset PnP binding：原点是标定板中心，+X/+Y/+Z 端点由当前 PnP 位姿和 GUI K/D12 投影到图像，再经过同一 preview/crop 映射显示；没有当前有效 PnP 时不绘制蓝色点和姿态轴，Heatmap-only 模式不叠加输入图像标注。

Dataset 表保留原 `Status` 作为读取/检测流水线状态，并新增 `Acceptance` 列单独显示当前验收状态，避免把 `Found` / `NotFound` 与 Dataset 门限混在一起。`Acceptance` 可显示 `Accepted`、`Depth Gap`、`Pose Gap`、`RMSE ReProj Gap`、`Max ReProj Gap`、`No Gain Gap`、`Geometry Gap`、`PnP Gap` 等。PnP 指标列拆成 `Depth`（棋盘中心深度）、`Angle dir`（棋盘法向 azimuth，OpenCV 图像轴，90° 向下）和 `Angle`（棋盘法向 tilt）；hover 可见 RMSE 和最大重投影误差。后续的 `Found Δ`、`Field Δ`、`Depth Δ`、`Pose Δ`、`Gain` 由当前完整有效 Dataset 的来源无关目标封顶归属计算，而不是记录自动入库瞬间的候选增益，也不是 leave-one-out 删除损失：

$$
\mathrm{Gain}_i = G^{found}_i + G^{field}_i + G^{depth}_i + G^{pose}_i
$$

对每个指标，按稳定 Dataset 顺序扫描当前统一图像尺寸、已启用、`Found` 且通过当前几何门限的数据项；Found view 仍按视图数量封顶归属，Field/Depth/Pose 则按区域 quota 归属：每个区域累计 `min(count, target_per_region)`，在达到该区域目标前，新角点或新 view 都继续产生 Gain。Dataset 总 `Score` 等于所有当前行 `Gain` 的和，也等于 $\min(C_{found},T_{found}) + \sum_r \min(C^r_{field},Q_{field}) + \sum_r \min(C^r_{depth},Q_{depth}) + \sum_r \min(C^r_{pose},Q_{pose})$。Field coverage 的单元数来自角点密度：每个网格单元累计其中的棋盘角点数量，计数非零才成为 occupied cell；`Field Δ` 表示归属给该项的目标截断 per-cell 角点 quota。Depth coverage 的 bin 计数来自该项所有棋盘内角点的相机 $Z$ 深度；`Depth Δ` 表示归属给该项的目标截断 per-bin 角点深度 quota。Pose coverage 仍按单张 view 的棋盘法向 tilt/azimuth bin 计数；`Pose Δ` 表示归属给该项的目标截断 per-bin view quota。PnP quality gates 只决定 Depth/Pose 是否有资格贡献，不作为单独分数。禁用、图像尺寸不兼容、非 Found 或几何门限不兼容的项显示为不属于当前 Dataset Acceptance；缺失、过期或未通过当前门限的 PnP 会显示为 `PnP×`/`×0`，它表示 Depth/Pose 被 PnP 或对应门限阻断，区别于当前 PnP 有效但覆盖冗余的 `+0`。`No Gain Gap` 表示该行参与当前 Dataset Acceptance，但目标封顶归属后没有任何指标分配给它；多勾选一张冗余图片不会把既有图片的 `Gain` 全部清零。

普通 PNG、本地/远端文件、手动 RTSP 快门和自动 RTSP 入库项在 Dataset Acceptance 中按同一规则统计；自动候选准入仍是另一条 source-bound 规则，继续使用精确 acquisition key、完整图像尺寸、棋盘和 source-bound $K+D12$ digest 过滤，不能被其他来源的 Dataset 项提高候选 gain。自动入库项同时保存来源无关 Dataset PnP 与 source-bound admission PnP，普通 PnP 刷新不会覆盖后者。Viewer 的 Live Stream coverage 同样保留精确 acquisition key 和图像尺寸过滤，但使用与 Dataset Image heatmap 相同的低到高调色板、$3\times3$ 引导线和 low/high 图例。

Found views 是采集进度和总分的一部分；自动入库的决定量使用同一 source-bound assessment 中尚未满足目标的 Found、field、depth 和 pose quota 的确定性正增益之和。只有单张候选的 `constraint_gain` 达到可配置的 `Minimum auto Gain` 时才会自动入库；低于阈值会被拒绝且不改 Dataset。面板可能显示 collection milestones，但它只表示当前运行时阈值的覆盖状态，不是生产资格。

### 15.4 将已安装结果回写为初始 $K+D12$

`Use result as initial K+D12` 紧邻 `Calibrate`。它仅在已安装标定解且没有活动标定或直播候选时可用；点击后将解矩阵的 `[0]`、`[4]`、`[2]`、`[5]` 分别复制到可编辑的 $f_x$、$f_y$、$c_x$、$c_y$，并把当前 solution 的 OpenCV 畸变系数复制到 12 个初始畸变输入格（不足 12 项补 0），同时关闭自动初始内参。该动作不会修改 EEPROM、已安装解或导出结果；若存在直播上下文，会据此立即重建运行时自动准入绑定，并触发普通 Dataset PnP 异步刷新。

## 16. 参考资料

- Zhengyou Zhang, [A Flexible New Technique for Camera Calibration](https://www.microsoft.com/en-us/research/wp-content/uploads/2016/02/tr98-71.pdf)。平面单应约束、线性初始化和联合优化的基础。
- OpenCV, [Camera Calibration and 3D Reconstruction](https://docs.opencv.org/4.x/d9/d0c/group__calib3d.html)。`calibrateCamera`、rational 和 thin-prism 模型定义。
- MathWorks, [Using the Single Camera Calibrator App](https://www.mathworks.com/help/vision/ug/using-the-single-camera-calibrator-app.html)。闭式初值与 Levenberg–Marquardt 联合优化说明。
- MathWorks, [Prepare Camera and Capture Images for Camera Calibration](https://www.mathworks.com/help/vision/ug/prepare-camera-and-capture-images-for-camera-calibration.html)。标定板位置、倾斜和图像数量建议。
- Peng and Sturm, [Calibration Wizard: A Guidance System for Camera Calibration Based on Modelling Geometric and Corner Uncertainty](https://openaccess.thecvf.com/content_ICCV_2019/html/Peng_Calibration_Wizard_A_Guidance_System_for_Camera_Calibration_Based_on_ICCV_2019_paper.html)。下一最佳姿态和角点不确定性。
- Calib.io, [Calibration Best Practices](https://calib.io/blogs/knowledge-base/calibration-best-practices)。标定板尺寸、画面覆盖、倾斜和采集实践。
