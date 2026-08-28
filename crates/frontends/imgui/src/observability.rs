//! 标定数据集的数值可观测性评估。
//!
//! 采集 goal 不再依赖姿态 bin，而是在最新 `calibrateCamera` 解附近线性化重投影残差，
//! 用 Schur 补消去每张图的外参 nuisance 参数，得到内参与畸变参数的信息矩阵。
//! 对信息矩阵求逆近似参数协方差，进而给出每类参数是否被当前 dataset 真正约束。

use crate::solve::DetectedDatasetFrame;
use camera_toolbox_core::{BoardSpec, CalibrationSolution};

const DISTORTION_NAMES: [&str; 12] = [
    "k1", "k2", "p1", "p2", "k3", "k4", "k5", "k6", "s1", "s2", "s3", "s4",
];

/// 焦距相对标准差目标；0.5% 是在线采集可解释、不会过早停采的保守门槛。
pub const FOCAL_REL_STDDEV_TARGET: f64 = 0.005;
/// 主点标准差目标；主点漂移超过 2px 时会直接影响去畸变中心与下游几何。
pub const PRINCIPAL_STDDEV_TARGET_PX: f64 = 2.0;
/// 畸变标准差折算到归一化半径 1.0 处的像素影响目标。
pub const DISTORTION_EDGE_STDDEV_TARGET_PX: f64 = 2.0;
/// 采集完成只要求前 5 个主畸变项（k1,k2,p1,p2,k3）达标。
/// k4..k6 与薄棱镜 s1..s4 仍参与求解和明细展示，但不阻塞自动采集完成；
/// 这些高阶项在普通棋盘视场内与低阶径向/切向项高度相关，强制全 D12 达标会导致过度采集。
pub const PRIMARY_DISTORTION_OBSERVABILITY_COUNT: usize = 5;
/// 归一化信息矩阵条件数上限；超过该值说明某些参数方向仍接近退化。
pub const MAX_NORMALIZED_CONDITION: f64 = 1.0e8;
/// 最终重投影 RMS 目标；保持亚像素并给少量实际噪声余量。
pub const MAX_GOAL_RMS_PX: f64 = 0.2;
const CENTRAL_DIFF_STEP: f64 = 1.0e-6;
const MATRIX_DAMPING: f64 = 1.0e-9;

#[derive(Clone, Debug, PartialEq)]
pub struct ObservabilityReport {
    pub view_count: usize,
    pub point_count: usize,
    pub rms_error: f64,
    pub max_view_rmse: f64,
    pub condition_number: f64,
    pub log_det_information: f64,
    pub last_info_gain: Option<f64>,
    pub focal_relative_stddev: [f64; 2],
    pub principal_point_stddev_px: [f64; 2],
    pub distortion_edge_stddev_px: Vec<f64>,
    pub distortion_names: Vec<&'static str>,
}

impl ObservabilityReport {
    #[must_use]
    pub fn focal_ok(&self) -> bool {
        self.focal_relative_stddev
            .iter()
            .all(|value| finite_le(*value, FOCAL_REL_STDDEV_TARGET))
    }

    #[must_use]
    pub fn principal_ok(&self) -> bool {
        self.principal_point_stddev_px
            .iter()
            .all(|value| finite_le(*value, PRINCIPAL_STDDEV_TARGET_PX))
    }

    #[must_use]
    pub fn distortion_ok(&self) -> bool {
        let primary = self.primary_distortion_edge_stddev_px();
        !primary.is_empty()
            && primary
                .iter()
                .all(|value| finite_le(*value, DISTORTION_EDGE_STDDEV_TARGET_PX))
    }

    #[must_use]
    pub fn conditioning_ok(&self) -> bool {
        finite_le(self.condition_number, MAX_NORMALIZED_CONDITION)
    }

    #[must_use]
    pub fn residual_ok(&self) -> bool {
        finite_le(self.rms_error, MAX_GOAL_RMS_PX)
    }

    #[must_use]
    pub fn goal_met(&self) -> bool {
        self.focal_ok()
            && self.principal_ok()
            && self.distortion_ok()
            && self.conditioning_ok()
            && self.residual_ok()
    }

    #[must_use]
    pub fn missing_hint(&self) -> &'static str {
        if !self.residual_ok() {
            "当前 RMS 偏高，请重采模糊/抖动视图"
        } else if !self.focal_ok() {
            "焦距仍未充分约束：请增加远近变化和横竖倾斜"
        } else if !self.principal_ok() {
            "主点仍未充分约束：请把棋盘移到画面边缘/四角并加入 roll"
        } else if !self.distortion_ok() {
            "畸变仍未充分约束：请让角点覆盖画面大半径和四角"
        } else if !self.conditioning_ok() {
            "信息矩阵条件数过高：请采集与已有姿态差异更大的视图"
        } else {
            "数值可观测性已达标"
        }
    }

    #[must_use]
    pub fn focal_progress(&self) -> f32 {
        inverse_threshold_progress(
            max_finite(&self.focal_relative_stddev),
            FOCAL_REL_STDDEV_TARGET,
        )
    }

    #[must_use]
    pub fn principal_progress(&self) -> f32 {
        inverse_threshold_progress(
            max_finite(&self.principal_point_stddev_px),
            PRINCIPAL_STDDEV_TARGET_PX,
        )
    }

    #[must_use]
    pub fn distortion_progress(&self) -> f32 {
        inverse_threshold_progress(
            max_finite(self.primary_distortion_edge_stddev_px()),
            DISTORTION_EDGE_STDDEV_TARGET_PX,
        )
    }

    #[must_use]
    pub fn conditioning_progress(&self) -> f32 {
        inverse_threshold_progress(self.condition_number, MAX_NORMALIZED_CONDITION)
    }

    #[must_use]
    pub fn residual_progress(&self) -> f32 {
        inverse_threshold_progress(self.rms_error, MAX_GOAL_RMS_PX)
    }

    #[must_use]
    pub fn primary_distortion_edge_stddev_px(&self) -> &[f64] {
        let count = self
            .distortion_edge_stddev_px
            .len()
            .min(PRIMARY_DISTORTION_OBSERVABILITY_COUNT);
        &self.distortion_edge_stddev_px[..count]
    }
}

/// 对最新标定解做局部可观测性分析。`detections` 必须是被最终解保留的视图，顺序与
/// `solution.views` 一致；调用方需先移除 `SolveResult::rejected_view_indices`。
pub fn analyze_solution(
    solution: &CalibrationSolution,
    board: BoardSpec,
    detections: &[DetectedDatasetFrame],
    previous: Option<&ObservabilityReport>,
) -> Result<ObservabilityReport, String> {
    board.validate().map_err(|error| error.to_string())?;
    if detections.len() != solution.views.len() {
        return Err(format!(
            "observability 输入视图数不一致：detections={} solution={}",
            detections.len(),
            solution.views.len()
        ));
    }
    if detections.is_empty() {
        return Err("observability 至少需要一个已求解视图".to_owned());
    }
    let params = IntrinsicParams::from_solution(solution)?;
    let scales = params.scales();
    let n = params.len();
    let mut information = zero_matrix(n, n);
    let mut point_count = 0usize;

    for (view_index, (detection, view)) in detections.iter().zip(&solution.views).enumerate() {
        let mut h_kk = zero_matrix(n, n);
        let mut h_ke = zero_matrix(n, 6);
        let mut h_ee = zero_matrix(6, 6);
        let rvec = view.rotation_vector;
        let tvec = view.translation_vector;
        for (corner_index, _) in detection.detection.corners.iter().enumerate() {
            let object = object_point(board, corner_index)
                .ok_or_else(|| format!("view {view_index} corner {corner_index} 超出棋盘拓扑"))?;
            let (j_k, j_e) = numerical_jacobians(&params, &scales, rvec, tvec, object)?;
            for dim in 0..2 {
                accumulate_outer(&mut h_kk, &j_k[dim], &j_k[dim]);
                accumulate_outer(&mut h_ke, &j_k[dim], &j_e[dim]);
                accumulate_outer(&mut h_ee, &j_e[dim], &j_e[dim]);
            }
            point_count = point_count.saturating_add(1);
        }
        damp_diagonal(&mut h_ee, MATRIX_DAMPING);
        let inv_ee =
            invert_matrix(&h_ee).ok_or_else(|| format!("view {view_index} 外参信息矩阵不可逆"))?;
        let marginalized = schur_complement(&h_kk, &h_ke, &inv_ee);
        add_matrix_in_place(&mut information, &marginalized);
    }

    symmetrize(&mut information);
    damp_diagonal(&mut information, MATRIX_DAMPING);
    let eigenvalues = jacobi_eigenvalues(&information);
    let positive: Vec<f64> = eigenvalues
        .iter()
        .copied()
        .filter(|value| value.is_finite() && *value > MATRIX_DAMPING)
        .collect();
    if positive.len() < n {
        return Err("内参信息矩阵未满秩，请继续采集不同姿态".to_owned());
    }
    let min_eigen = positive.iter().copied().fold(f64::INFINITY, f64::min);
    let max_eigen = positive.iter().copied().fold(0.0_f64, f64::max);
    let condition_number = max_eigen / min_eigen;
    let log_det_information = positive.iter().map(|value| value.ln()).sum::<f64>();
    let inv_information = invert_matrix(&information).ok_or("内参信息矩阵不可逆")?;
    let sigma2 = solution.rms_error.max(1.0e-3).powi(2);
    let mut stddev = Vec::with_capacity(n);
    for index in 0..n {
        let variance = (inv_information[index][index] * sigma2).max(0.0);
        stddev.push(variance.sqrt() * scales[index]);
    }

    let focal_relative_stddev = [
        stddev[0] / params.fx.abs().max(1.0),
        stddev[1] / params.fy.abs().max(1.0),
    ];
    let principal_point_stddev_px = [stddev[2], stddev[3]];
    let focal_mean = 0.5 * (params.fx.abs() + params.fy.abs()).max(1.0);
    let distortion_edge_stddev_px = stddev
        .iter()
        .skip(4)
        .map(|value| value.abs() * focal_mean)
        .collect::<Vec<_>>();
    let distortion_names = DISTORTION_NAMES
        .iter()
        .take(params.distortion_len)
        .copied()
        .collect::<Vec<_>>();
    let last_info_gain = previous
        .map(|report| log_det_information - report.log_det_information)
        .filter(|value| value.is_finite());
    let max_view_rmse = solution
        .views
        .iter()
        .map(|view| view.reprojection_rmse)
        .fold(0.0_f64, f64::max);

    Ok(ObservabilityReport {
        view_count: detections.len(),
        point_count,
        rms_error: solution.rms_error,
        max_view_rmse,
        condition_number,
        log_det_information,
        last_info_gain,
        focal_relative_stddev,
        principal_point_stddev_px,
        distortion_edge_stddev_px,
        distortion_names,
    })
}

#[derive(Clone)]
struct IntrinsicParams {
    fx: f64,
    fy: f64,
    cx: f64,
    cy: f64,
    distortion: [f64; 12],
    distortion_len: usize,
}

impl IntrinsicParams {
    fn from_solution(solution: &CalibrationSolution) -> Result<Self, String> {
        let k = solution.camera_matrix;
        if ![k[0], k[2], k[4], k[5]]
            .iter()
            .all(|value| value.is_finite())
        {
            return Err("相机矩阵包含非有限值".to_owned());
        }
        let mut distortion = [0.0; 12];
        let distortion_len = solution.distortion_coefficients.len().min(12);
        for (slot, value) in distortion
            .iter_mut()
            .zip(solution.distortion_coefficients.iter().copied())
        {
            if !value.is_finite() {
                return Err("畸变参数包含非有限值".to_owned());
            }
            *slot = value;
        }
        Ok(Self {
            fx: k[0],
            fy: k[4],
            cx: k[2],
            cy: k[5],
            distortion,
            distortion_len,
        })
    }

    fn len(&self) -> usize {
        4 + self.distortion_len
    }

    fn scales(&self) -> Vec<f64> {
        let mut out = Vec::with_capacity(self.len());
        out.push(self.fx.abs().max(1.0));
        out.push(self.fy.abs().max(1.0));
        out.push(1.0);
        out.push(1.0);
        out.extend(std::iter::repeat_n(1.0, self.distortion_len));
        out
    }

    fn perturb(&self, index: usize, delta: f64) -> Self {
        let mut out = self.clone();
        match index {
            0 => out.fx += delta,
            1 => out.fy += delta,
            2 => out.cx += delta,
            3 => out.cy += delta,
            i => out.distortion[i - 4] += delta,
        }
        out
    }
}

fn numerical_jacobians(
    params: &IntrinsicParams,
    scales: &[f64],
    rvec: [f64; 3],
    tvec: [f64; 3],
    object: [f64; 3],
) -> Result<([Vec<f64>; 2], [[f64; 6]; 2]), String> {
    let n = params.len();
    let mut j_k = [vec![0.0; n], vec![0.0; n]];
    for index in 0..n {
        let delta = scales[index] * CENTRAL_DIFF_STEP;
        let plus = project_point(&params.perturb(index, delta), rvec, tvec, object)
            .ok_or("内参正扰动投影失败")?;
        let minus = project_point(&params.perturb(index, -delta), rvec, tvec, object)
            .ok_or("内参负扰动投影失败")?;
        j_k[0][index] = (plus[0] - minus[0]) / (2.0 * CENTRAL_DIFF_STEP);
        j_k[1][index] = (plus[1] - minus[1]) / (2.0 * CENTRAL_DIFF_STEP);
    }

    let mut j_e = [[0.0; 6]; 2];
    let translation_scale = tvec
        .iter()
        .map(|value| value.abs())
        .fold(1.0_f64, f64::max)
        .max(1.0);
    for index in 0..6 {
        let scale = if index < 3 { 1.0 } else { translation_scale };
        let delta = scale * CENTRAL_DIFF_STEP;
        let mut r_plus = rvec;
        let mut r_minus = rvec;
        let mut t_plus = tvec;
        let mut t_minus = tvec;
        if index < 3 {
            r_plus[index] += delta;
            r_minus[index] -= delta;
        } else {
            let t_index = index - 3;
            t_plus[t_index] += delta;
            t_minus[t_index] -= delta;
        }
        let plus = project_point(params, r_plus, t_plus, object).ok_or("外参正扰动投影失败")?;
        let minus = project_point(params, r_minus, t_minus, object).ok_or("外参负扰动投影失败")?;
        j_e[0][index] = (plus[0] - minus[0]) / (2.0 * CENTRAL_DIFF_STEP);
        j_e[1][index] = (plus[1] - minus[1]) / (2.0 * CENTRAL_DIFF_STEP);
    }
    Ok((j_k, j_e))
}

fn project_point(
    params: &IntrinsicParams,
    rvec: [f64; 3],
    tvec: [f64; 3],
    object: [f64; 3],
) -> Option<[f64; 2]> {
    let rotation = rodrigues_matrix(rvec)?;
    let camera = [
        rotation[0][0] * object[0]
            + rotation[0][1] * object[1]
            + rotation[0][2] * object[2]
            + tvec[0],
        rotation[1][0] * object[0]
            + rotation[1][1] * object[1]
            + rotation[1][2] * object[2]
            + tvec[1],
        rotation[2][0] * object[0]
            + rotation[2][1] * object[1]
            + rotation[2][2] * object[2]
            + tvec[2],
    ];
    if camera.iter().any(|value| !value.is_finite()) || camera[2].abs() <= f64::EPSILON {
        return None;
    }
    let x = camera[0] / camera[2];
    let y = camera[1] / camera[2];
    let [xd, yd] = distort(params, x, y)?;
    let u = params.fx.mul_add(xd, params.cx);
    let v = params.fy.mul_add(yd, params.cy);
    (u.is_finite() && v.is_finite()).then_some([u, v])
}

fn distort(params: &IntrinsicParams, x: f64, y: f64) -> Option<[f64; 2]> {
    let d = &params.distortion;
    let r2 = x.mul_add(x, y * y);
    let r4 = r2 * r2;
    let r6 = r4 * r2;
    let numerator = 1.0 + d[0] * r2 + d[1] * r4 + d[4] * r6;
    let denominator = 1.0 + d[5] * r2 + d[6] * r4 + d[7] * r6;
    if !denominator.is_finite() || denominator.abs() <= f64::EPSILON {
        return None;
    }
    let radial = numerator / denominator;
    let xy = x * y;
    let x_distorted =
        x * radial + 2.0 * d[2] * xy + d[3] * (r2 + 2.0 * x * x) + d[8] * r2 + d[9] * r4;
    let y_distorted =
        y * radial + d[2] * (r2 + 2.0 * y * y) + 2.0 * d[3] * xy + d[10] * r2 + d[11] * r4;
    [x_distorted, y_distorted]
        .iter()
        .all(|value| value.is_finite())
        .then_some([x_distorted, y_distorted])
}

fn rodrigues_matrix(rvec: [f64; 3]) -> Option<[[f64; 3]; 3]> {
    if rvec.iter().any(|value| !value.is_finite()) {
        return None;
    }
    let theta = (rvec[0].mul_add(rvec[0], rvec[1] * rvec[1]) + rvec[2] * rvec[2]).sqrt();
    if theta < 1.0e-12 {
        return Some([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]);
    }
    let axis = [rvec[0] / theta, rvec[1] / theta, rvec[2] / theta];
    let (sin_t, cos_t) = theta.sin_cos();
    let one_minus_cos = 1.0 - cos_t;
    let [x, y, z] = axis;
    Some([
        [
            cos_t + x * x * one_minus_cos,
            x * y * one_minus_cos - z * sin_t,
            x * z * one_minus_cos + y * sin_t,
        ],
        [
            y * x * one_minus_cos + z * sin_t,
            cos_t + y * y * one_minus_cos,
            y * z * one_minus_cos - x * sin_t,
        ],
        [
            z * x * one_minus_cos - y * sin_t,
            z * y * one_minus_cos + x * sin_t,
            cos_t + z * z * one_minus_cos,
        ],
    ])
}

fn object_point(board: BoardSpec, index: usize) -> Option<[f64; 3]> {
    let columns = usize::from(board.inner_cols);
    let rows = usize::from(board.inner_rows);
    if index >= columns.checked_mul(rows)? {
        return None;
    }
    let row = index / columns;
    let col = index % columns;
    Some([
        col as f64 * board.square_size,
        row as f64 * board.square_size,
        0.0,
    ])
}

fn finite_le(value: f64, threshold: f64) -> bool {
    value.is_finite() && value <= threshold
}

fn max_finite(values: &[f64]) -> f64 {
    values
        .iter()
        .copied()
        .filter(|value| value.is_finite())
        .fold(f64::INFINITY, |acc, value| {
            if acc.is_infinite() {
                value
            } else {
                acc.max(value)
            }
        })
}

fn inverse_threshold_progress(value: f64, target: f64) -> f32 {
    if !value.is_finite() || !target.is_finite() || target <= 0.0 {
        return 0.0;
    }
    (target / value.max(target)).clamp(0.0, 1.0) as f32
}

type Matrix = Vec<Vec<f64>>;

fn zero_matrix(rows: usize, cols: usize) -> Matrix {
    vec![vec![0.0; cols]; rows]
}

fn accumulate_outer(matrix: &mut Matrix, left: &[f64], right: &[f64]) {
    for (row, left_value) in left.iter().copied().enumerate() {
        for (col, right_value) in right.iter().copied().enumerate() {
            matrix[row][col] += left_value * right_value;
        }
    }
}

fn add_matrix_in_place(lhs: &mut Matrix, rhs: &Matrix) {
    for (lhs_row, rhs_row) in lhs.iter_mut().zip(rhs) {
        for (lhs_value, rhs_value) in lhs_row.iter_mut().zip(rhs_row) {
            *lhs_value += rhs_value;
        }
    }
}

fn damp_diagonal(matrix: &mut Matrix, damping: f64) {
    for (index, row) in matrix.iter_mut().enumerate() {
        row[index] += damping;
    }
}

fn symmetrize(matrix: &mut Matrix) {
    for row in 0..matrix.len() {
        for col in row + 1..matrix.len() {
            let value = 0.5 * (matrix[row][col] + matrix[col][row]);
            matrix[row][col] = value;
            matrix[col][row] = value;
        }
    }
}

fn schur_complement(h_kk: &Matrix, h_ke: &Matrix, inv_ee: &Matrix) -> Matrix {
    let n = h_kk.len();
    let mut out = h_kk.clone();
    for row in 0..n {
        for col in 0..n {
            let mut value = 0.0;
            for a in 0..6 {
                for b in 0..6 {
                    value += h_ke[row][a] * inv_ee[a][b] * h_ke[col][b];
                }
            }
            out[row][col] -= value;
        }
    }
    out
}

fn invert_matrix(matrix: &Matrix) -> Option<Matrix> {
    let n = matrix.len();
    if n == 0 || matrix.iter().any(|row| row.len() != n) {
        return None;
    }
    let mut aug = zero_matrix(n, n * 2);
    for row in 0..n {
        for col in 0..n {
            aug[row][col] = matrix[row][col];
        }
        aug[row][n + row] = 1.0;
    }
    for pivot in 0..n {
        let mut best = pivot;
        let mut best_abs = aug[pivot][pivot].abs();
        for row in pivot + 1..n {
            let candidate = aug[row][pivot].abs();
            if candidate > best_abs {
                best = row;
                best_abs = candidate;
            }
        }
        if !best_abs.is_finite() || best_abs <= 1.0e-18 {
            return None;
        }
        if best != pivot {
            aug.swap(best, pivot);
        }
        let divisor = aug[pivot][pivot];
        for value in &mut aug[pivot] {
            *value /= divisor;
        }
        for row in 0..n {
            if row == pivot {
                continue;
            }
            let factor = aug[row][pivot];
            if factor == 0.0 {
                continue;
            }
            for col in 0..n * 2 {
                aug[row][col] -= factor * aug[pivot][col];
            }
        }
    }
    let mut inverse = zero_matrix(n, n);
    for row in 0..n {
        for col in 0..n {
            inverse[row][col] = aug[row][n + col];
        }
    }
    Some(inverse)
}

fn jacobi_eigenvalues(matrix: &Matrix) -> Vec<f64> {
    let n = matrix.len();
    let mut a = matrix.clone();
    let max_iterations = n.saturating_mul(n).saturating_mul(32).max(1);
    for _ in 0..max_iterations {
        let mut p = 0usize;
        let mut q = 0usize;
        let mut max_offdiag = 0.0_f64;
        for row in 0..n {
            for col in row + 1..n {
                let value = a[row][col].abs();
                if value > max_offdiag {
                    max_offdiag = value;
                    p = row;
                    q = col;
                }
            }
        }
        if max_offdiag < 1.0e-10 {
            break;
        }
        let app = a[p][p];
        let aqq = a[q][q];
        let apq = a[p][q];
        let tau = (aqq - app) / (2.0 * apq);
        let t = tau.signum() / (tau.abs() + (1.0 + tau * tau).sqrt());
        let c = 1.0 / (1.0 + t * t).sqrt();
        let s = t * c;
        for k in 0..n {
            if k != p && k != q {
                let akp = a[k][p];
                let akq = a[k][q];
                a[k][p] = c * akp - s * akq;
                a[p][k] = a[k][p];
                a[k][q] = s * akp + c * akq;
                a[q][k] = a[k][q];
            }
        }
        a[p][p] = c * c * app - 2.0 * s * c * apq + s * s * aqq;
        a[q][q] = s * s * app + 2.0 * s * c * apq + c * c * aqq;
        a[p][q] = 0.0;
        a[q][p] = 0.0;
    }
    (0..n).map(|index| a[index][index]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invert_matrix_recovers_identity() {
        let matrix = vec![vec![4.0, 7.0], vec![2.0, 6.0]];
        let inverse = invert_matrix(&matrix).expect("invertible");
        assert!((inverse[0][0] - 0.6).abs() < 1.0e-12);
        assert!((inverse[0][1] + 0.7).abs() < 1.0e-12);
        assert!((inverse[1][0] + 0.2).abs() < 1.0e-12);
        assert!((inverse[1][1] - 0.4).abs() < 1.0e-12);
    }

    #[test]
    fn report_goal_requires_all_groups() {
        let mut report = ObservabilityReport {
            view_count: 5,
            point_count: 100,
            rms_error: 0.1,
            max_view_rmse: 0.1,
            condition_number: 1.0e3,
            log_det_information: 0.0,
            last_info_gain: Some(1.0),
            focal_relative_stddev: [0.001, 0.001],
            principal_point_stddev_px: [0.5, 0.5],
            distortion_edge_stddev_px: vec![0.5; 12],
            distortion_names: DISTORTION_NAMES.to_vec(),
        };
        assert!(report.goal_met());
        report.focal_relative_stddev[0] = FOCAL_REL_STDDEV_TARGET * 2.0;
        assert!(!report.goal_met());
        assert_eq!(
            report.missing_hint(),
            "焦距仍未充分约束：请增加远近变化和横竖倾斜"
        );
        report.focal_relative_stddev[0] = 0.001;
        report.distortion_edge_stddev_px[PRIMARY_DISTORTION_OBSERVABILITY_COUNT] =
            DISTORTION_EDGE_STDDEV_TARGET_PX * 100.0;
        assert!(
            report.goal_met(),
            "高阶 D12 诊断项不应阻塞 D5 主畸变采集 goal"
        );
        report.distortion_edge_stddev_px[PRIMARY_DISTORTION_OBSERVABILITY_COUNT - 1] =
            DISTORTION_EDGE_STDDEV_TARGET_PX * 2.0;
        assert!(!report.goal_met());
    }
}
