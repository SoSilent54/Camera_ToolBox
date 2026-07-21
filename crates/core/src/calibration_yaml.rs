//! 与既有 OpenCV MATLAB 结果兼容的 Pinhole-Radtan YAML 文本导出。
//!
//! 目标文件的 directive、注释、字段顺序和小数精度是外部工具协议的一部分，不能由
//! 通用 YAML serializer 决定。

use std::io::Write;

use thiserror::Error;

use crate::calibration::CalibrationSolution;

/// 将标定结果按固定 OpenCV YAML 文本布局写入输出流。
///
/// 文本 directive、注释、字段顺序和小数精度属于外部工具协议；不能使用通用 YAML serializer。
pub fn write_opencv_pinhole_radtan_yaml(
    writer: &mut dyn Write,
    solution: &CalibrationSolution,
) -> Result<(), CalibrationYamlError> {
    let distortion = solution.distortion_coefficients.get(..5).ok_or(
        CalibrationYamlError::MissingDistortionCoefficients {
            required: 5,
            actual: solution.distortion_coefficients.len(),
        },
    )?;
    let fields = [
        ("fx", solution.camera_matrix[0]),
        ("fy", solution.camera_matrix[4]),
        ("cx", solution.camera_matrix[2]),
        ("cy", solution.camera_matrix[5]),
        ("k1", distortion[0]),
        ("k2", distortion[1]),
        ("p1", distortion[2]),
        ("p2", distortion[3]),
        ("k3", distortion[4]),
    ];
    if let Some((field, _)) = fields.iter().find(|(_, value)| !value.is_finite()) {
        return Err(CalibrationYamlError::NonFiniteField { field });
    }

    write!(
        writer,
        "%YAML:1.0\n# Pinhole-Radtan intrinsics\nfx: {:.4}\nfy: {:.4}\ncx: {:.4}\ncy: {:.4}\nk1: {:.4}\nk2: {:.4}\np1: {:.8}\np2: {:.8}\nk3: {:.4}\nwidth: {}\nheight: {}\n",
        fields[0].1,
        fields[1].1,
        fields[2].1,
        fields[3].1,
        fields[4].1,
        fields[5].1,
        fields[6].1,
        fields[7].1,
        fields[8].1,
        solution.image_size.width,
        solution.image_size.height,
    )?;
    Ok(())
}

/// 按固定 OpenCV YAML 文本布局导出单目 Pinhole-Radtan 标定结果。
pub fn encode_opencv_pinhole_radtan_yaml(
    solution: &CalibrationSolution,
) -> Result<Vec<u8>, CalibrationYamlError> {
    let mut bytes = Vec::new();
    write_opencv_pinhole_radtan_yaml(&mut bytes, solution)?;
    Ok(bytes)
}

#[derive(Debug, Error)]
pub enum CalibrationYamlError {
    #[error("Pinhole-Radtan YAML requires {required} distortion coefficients, got {actual}")]
    MissingDistortionCoefficients { required: usize, actual: usize },
    #[error("Pinhole-Radtan YAML field {field} is NaN or infinity")]
    NonFiniteField { field: &'static str },
    #[error("Pinhole-Radtan YAML write failed: {0}")]
    Write(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calibration::{
        CalibrationImageSize, CalibrationSolution, PANGBOT_CALIBRATION_FLAGS,
    };

    #[test]
    fn matches_right_yaml_text_layout() {
        let solution = CalibrationSolution {
            image_size: CalibrationImageSize::new(1920, 1080).unwrap(),
            camera_matrix: [
                878.7023, 0.0, 955.6284, 0.0, 878.5325, 533.1718, 0.0, 0.0, 1.0,
            ],
            distortion_coefficients: vec![0.0345, -0.0458, -0.000_085_90, 0.000_153_87, 0.0119],
            rms_error: 0.0,
            calibration_flags: PANGBOT_CALIBRATION_FLAGS,
            views: Vec::new(),
        };

        let actual =
            String::from_utf8(encode_opencv_pinhole_radtan_yaml(&solution).unwrap()).unwrap();
        let expected = "%YAML:1.0\n# Pinhole-Radtan intrinsics\nfx: 878.7023\nfy: 878.5325\ncx: 955.6284\ncy: 533.1718\nk1: 0.0345\nk2: -0.0458\np1: -0.00008590\np2: 0.00015387\nk3: 0.0119\nwidth: 1920\nheight: 1080\n";
        assert_eq!(actual, expected);
    }

    #[test]
    fn rejects_incomplete_or_nonfinite_solution() {
        let mut solution = CalibrationSolution {
            image_size: CalibrationImageSize::new(2, 2).unwrap(),
            camera_matrix: [1.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 0.0, 1.0],
            distortion_coefficients: vec![0.0; 4],
            rms_error: 0.0,
            calibration_flags: PANGBOT_CALIBRATION_FLAGS,
            views: Vec::new(),
        };
        assert!(matches!(
            encode_opencv_pinhole_radtan_yaml(&solution),
            Err(CalibrationYamlError::MissingDistortionCoefficients { .. })
        ));

        solution.distortion_coefficients.push(f64::NAN);
        assert!(matches!(
            encode_opencv_pinhole_radtan_yaml(&solution),
            Err(CalibrationYamlError::NonFiniteField { field: "k3" })
        ));
    }
}
