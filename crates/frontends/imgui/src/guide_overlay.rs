//! 采集 overlay 的 UI 无关绘制数据。
use std::sync::Arc;

/// 连续角点密度场的 UI 无关快照。
///
/// `samples` 以行优先顺序保存归一化图像坐标中的密度：`0` 表示零密度，`1` 及以上
/// 表示足够密度。渲染器可按自身纹理策略平滑采样，数据层不依赖任何 UI 或 GPU 类型。
#[derive(Clone, Debug, PartialEq)]
pub struct DensityHeatmap {
    pub cols: usize,
    pub rows: usize,
    pub samples: Arc<[f32]>,
}

impl DensityHeatmap {
    /// 创建全零密度场；分辨率下限避免无意义的展示/分析网格。
    #[must_use]
    pub fn zeroed(cols: usize, rows: usize) -> Self {
        assert!(cols >= 8 && rows >= 8, "密度场分辨率至少为 8×8");
        let len = cols.checked_mul(rows).expect("密度场分辨率乘积溢出");
        Self {
            cols,
            rows,
            samples: vec![0.0; len].into(),
        }
    }

    /// 检查快照形状是否可被渲染器安全采样。
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.cols >= 8
            && self.rows >= 8
            && self
                .cols
                .checked_mul(self.rows)
                .is_some_and(|len| len == self.samples.len())
    }
    /// 以密度网格单元中心为采样点，在边界钳制后对渲染/质量分析共用的连续场双线性采样。
    ///
    /// 非法快照或坐标没有密度证据，返回零，避免错误数据伪造绿色覆盖。
    #[must_use]
    pub fn sample_bilinear(&self, u: f32, v: f32) -> f32 {
        if !self.is_valid() || !u.is_finite() || !v.is_finite() {
            return 0.0;
        }
        let max_col = self.cols.saturating_sub(1);
        let max_row = self.rows.saturating_sub(1);
        let source_x = (u * self.cols as f32 - 0.5).clamp(0.0, max_col as f32);
        let source_y = (v * self.rows as f32 - 0.5).clamp(0.0, max_row as f32);
        let left = source_x.floor() as usize;
        let top = source_y.floor() as usize;
        let right = (left + 1).min(max_col);
        let bottom = (top + 1).min(max_row);
        let tx = source_x - left as f32;
        let ty = source_y - top as f32;
        let at = |col: usize, row: usize| self.samples[row * self.cols + col];
        let upper = at(left, top) + (at(right, top) - at(left, top)) * tx;
        let lower = at(left, bottom) + (at(right, bottom) - at(left, bottom)) * tx;
        let density = upper + (lower - upper) * ty;
        if density.is_finite() {
            density.clamp(0.0, 1.0)
        } else {
            0.0
        }
    }
}

impl Default for DensityHeatmap {
    fn default() -> Self {
        Self {
            cols: 0,
            rows: 0,
            samples: Arc::<[f32]>::from(Vec::new()),
        }
    }
}

/// hold 稳定状态面板（worker → UI）。
#[derive(Clone, Default, Debug, PartialEq)]
pub struct OverlayStatus {
    pub hold_frames: u8,
    pub hold_target: u8,
}

/// 一帧检测 / 采集状态的绘制数据（worker → UI 线程）。
#[derive(Clone, Default)]
pub struct OverlayData {
    /// 图像尺寸（坐标映射用）。
    pub image_width: f32,
    pub image_height: f32,
    /// 检测棋盘外框（图像像素坐标）：只画外框，不画内角点连线。
    pub detected_outline_px: Option<[[f32; 2]; 4]>,
    pub status: Option<OverlayStatus>,
    /// 最近一次成功拍摄帧的棋盘内角点（图像像素坐标，行优先 cols×rows；
    /// 拍摄后短暂保留显示，用于确认触发瞬间采到的棋盘姿态）。
    pub captured_corners_px: Option<(usize, usize, Vec<[f32; 2]>)>,
}
