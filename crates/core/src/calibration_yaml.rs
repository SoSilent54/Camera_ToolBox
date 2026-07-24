//! 与当前 OpenCV Pinhole Rational + Thin-Prism D12 标定模型兼容的 YAML 文本导出。
//!
//! 目标文件的 directive、注释、字段顺序和小数精度是外部工具协议的一部分，不能由
//! 通用 YAML serializer 决定；历史 API 名称保留 `pinhole_radtan`。

use std::io::Write;

use thiserror::Error;

use crate::calibration::CalibrationSolution;

const OPENCV_D12_COEFFICIENT_COUNT: usize = 12;

/// 将完整 D12 标定结果按固定 OpenCV YAML 文本布局写入输出流。
///
/// 文本 directive、注释、字段顺序和小数精度属于外部工具协议；不能使用通用 YAML serializer。
pub fn write_opencv_pinhole_radtan_yaml(
    writer: &mut dyn Write,
    solution: &CalibrationSolution,
) -> Result<(), CalibrationYamlError> {
    let distortion = solution
        .distortion_coefficients
        .get(..OPENCV_D12_COEFFICIENT_COUNT)
        .ok_or(CalibrationYamlError::MissingDistortionCoefficients {
            required: OPENCV_D12_COEFFICIENT_COUNT,
            actual: solution.distortion_coefficients.len(),
        })?;
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
        ("k4", distortion[5]),
        ("k5", distortion[6]),
        ("k6", distortion[7]),
        ("s1", distortion[8]),
        ("s2", distortion[9]),
        ("s3", distortion[10]),
        ("s4", distortion[11]),
    ];
    if let Some((field, _)) = fields.iter().find(|(_, value)| !value.is_finite()) {
        return Err(CalibrationYamlError::NonFiniteField { field });
    }

    write!(
        writer,
        "%YAML:1.0\n# Pinhole-Radtan intrinsics\nfx: {:.4}\nfy: {:.4}\ncx: {:.4}\ncy: {:.4}\nk1: {:.4}\nk2: {:.4}\np1: {:.8}\np2: {:.8}\nk3: {:.4}\nk4: {:.4}\nk5: {:.4}\nk6: {:.4}\ns1: {:.8}\ns2: {:.8}\ns3: {:.8}\ns4: {:.8}\nwidth: {}\nheight: {}\n",
        fields[0].1,
        fields[1].1,
        fields[2].1,
        fields[3].1,
        fields[4].1,
        fields[5].1,
        fields[6].1,
        fields[7].1,
        fields[8].1,
        fields[9].1,
        fields[10].1,
        fields[11].1,
        fields[12].1,
        fields[13].1,
        fields[14].1,
        fields[15].1,
        solution.image_size.width,
        solution.image_size.height,
    )?;
    Ok(())
}

/// 按固定 OpenCV YAML 文本布局导出单目 D12 标定结果。
pub fn encode_opencv_pinhole_radtan_yaml(
    solution: &CalibrationSolution,
) -> Result<Vec<u8>, CalibrationYamlError> {
    let mut bytes = Vec::new();
    write_opencv_pinhole_radtan_yaml(&mut bytes, solution)?;
    Ok(bytes)
}

#[derive(Debug, Error)]
pub enum CalibrationYamlError {
    #[error("Pinhole-Radtan D12 YAML requires {required} distortion coefficients, got {actual}")]
    MissingDistortionCoefficients { required: usize, actual: usize },
    #[error("Pinhole-Radtan D12 YAML field {field} is NaN or infinity")]
    NonFiniteField { field: &'static str },
    #[error("Pinhole-Radtan D12 YAML write failed: {0}")]
    Write(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calibration::{
        CalibrationImageSize, CalibrationSolution, PANGBOT_CALIBRATION_FLAGS,
    };

    #[test]
    fn matches_d12_yaml_text_layout() {
        let solution = CalibrationSolution {
            image_size: CalibrationImageSize::new(1920, 1080).unwrap(),
            camera_matrix: [
                878.7023, 0.0, 955.6284, 0.0, 878.5325, 533.1718, 0.0, 0.0, 1.0,
            ],
            distortion_coefficients: vec![
                0.0345,
                -0.0458,
                -0.000_085_90,
                0.000_153_87,
                0.0119,
                -0.0123,
                0.0234,
                -0.0345,
                0.000_011_11,
                -0.000_022_22,
                0.000_033_33,
                -0.000_044_44,
            ],
            rms_error: 0.0,
            calibration_flags: PANGBOT_CALIBRATION_FLAGS,
            views: Vec::new(),
        };

        let actual =
            String::from_utf8(encode_opencv_pinhole_radtan_yaml(&solution).unwrap()).unwrap();
        let expected = "%YAML:1.0\n# Pinhole-Radtan intrinsics\nfx: 878.7023\nfy: 878.5325\ncx: 955.6284\ncy: 533.1718\nk1: 0.0345\nk2: -0.0458\np1: -0.00008590\np2: 0.00015387\nk3: 0.0119\nk4: -0.0123\nk5: 0.0234\nk6: -0.0345\ns1: 0.00001111\ns2: -0.00002222\ns3: 0.00003333\ns4: -0.00004444\nwidth: 1920\nheight: 1080\n";
        assert_eq!(actual, expected);
    }

    #[test]
    fn rejects_incomplete_d12_or_nonfinite_extended_fields() {
        let mut solution = CalibrationSolution {
            image_size: CalibrationImageSize::new(2, 2).unwrap(),
            camera_matrix: [1.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 0.0, 1.0],
            distortion_coefficients: vec![0.0; 11],
            rms_error: 0.0,
            calibration_flags: PANGBOT_CALIBRATION_FLAGS,
            views: Vec::new(),
        };
        assert!(matches!(
            encode_opencv_pinhole_radtan_yaml(&solution),
            Err(CalibrationYamlError::MissingDistortionCoefficients {
                required: 12,
                actual: 11
            })
        ));

        solution.distortion_coefficients.push(0.0);
        solution.distortion_coefficients[5] = f64::NAN;
        assert!(matches!(
            encode_opencv_pinhole_radtan_yaml(&solution),
            Err(CalibrationYamlError::NonFiniteField { field: "k4" })
        ));

        solution.distortion_coefficients[5] = 0.0;
        solution.distortion_coefficients[8] = f64::INFINITY;
        assert!(matches!(
            encode_opencv_pinhole_radtan_yaml(&solution),
            Err(CalibrationYamlError::NonFiniteField { field: "s1" })
        ));
    }
}
