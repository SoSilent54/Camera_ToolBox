//! 引导 overlay 的 UI 无关绘制数据。

/// 目标引导网格线（UV 坐标，0..1）。
#[derive(Clone, Default, Debug, PartialEq)]
pub struct OverlayGridLine {
    pub start_uv: [f32; 2],
    pub end_uv: [f32; 2],
}

/// guide hold 状态面板。
#[derive(Clone, Default, Debug, PartialEq)]
pub struct OverlayStatus {
    pub hold_frames: u8,
    pub hold_target: u8,
    pub detail_label: String,
    pub detail_value: f64,
    pub detail_limit: f64,
    pub matched: bool,
}

/// 单轴旋转误差弧：base 是参考环，arc 是当前误差扫角。
#[derive(Clone, Default, Debug, PartialEq)]
pub struct OverlayRotationArc {
    pub base_uv: Vec<[f32; 2]>,
    pub arc_uv: Vec<[f32; 2]>,
    pub tick_uv: [f32; 2],
    pub error_degrees: f32,
}

#[derive(Clone, Default, Debug, PartialEq)]
pub struct OverlayRotationRings {
    pub center_uv: [f32; 2],
    pub roll: OverlayRotationArc,
    pub pitch: OverlayRotationArc,
    pub yaw: OverlayRotationArc,
}

#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct OverlayPoseArrow {
    pub start_uv: [f32; 2],
    pub end_uv: [f32; 2],
}

/// 一帧检测 / 目标引导的绘制数据（worker → UI 线程）。
#[derive(Clone, Default)]
pub struct OverlayData {
    /// 图像尺寸（坐标映射用）。
    pub image_width: f32,
    pub image_height: f32,
    /// 检测棋盘外框（图像像素坐标）：只画外框，不画内角点连线。
    pub detected_outline_px: Option<[[f32; 2]; 4]>,
    /// 当前目标姿态的投影中心 / 外框 / 网格。
    pub target_center_uv: Option<[f32; 2]>,
    pub target_outline_uv: Option<[[f32; 2]; 4]>,
    pub target_grid_lines: Vec<OverlayGridLine>,
    pub target_matched: bool,
    pub rotation_rings: Option<OverlayRotationRings>,
    pub pose_arrow: Option<OverlayPoseArrow>,
    pub status: Option<OverlayStatus>,
}
