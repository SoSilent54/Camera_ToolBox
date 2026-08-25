//! 与当前 OpenCV Pinhole Rational + Thin-Prism D12 标定模型兼容的 YAML 文本导出。
//!
//! 目标文件的 directive、注释、字段顺序和小数精度是外部工具协议的一部分，不能由
//! 通用 YAML serializer 决定；历史 API 名称保留 `pinhole_radtan`。

use std::io::Write;

use thiserror::Error;

use crate::calibration::{
    CalibrationDataError, CalibrationImageSize, CalibrationSolution, InitialIntrinsics,
    PANGBOT_CALIBRATION_FLAGS,
};

const OPENCV_D12_COEFFICIENT_COUNT: usize = 12;

#[derive(Default)]
struct OpenCvPinholeRadtanYamlFields {
    fx: Option<f64>,
    fy: Option<f64>,
    cx: Option<f64>,
    cy: Option<f64>,
    k1: Option<f64>,
    k2: Option<f64>,
    p1: Option<f64>,
    p2: Option<f64>,
    k3: Option<f64>,
    k4: Option<f64>,
    k5: Option<f64>,
    k6: Option<f64>,
    s1: Option<f64>,
    s2: Option<f64>,
    s3: Option<f64>,
    s4: Option<f64>,
    width: Option<u32>,
    height: Option<u32>,
}

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

/// 从固定 OpenCV Pinhole Rational + Thin-Prism D12 YAML 文本恢复 EEPROM 所需标定结果。
///
/// 该导入路径只恢复内参、D12 畸变和图像尺寸；逐图外参/重投影统计不在导出 YAML 中，
/// 因此以空 `views` 和 0 RMS 安装为外部结果，专供复用与 EEPROM 写入。
pub fn parse_opencv_pinhole_radtan_yaml(
    input: &str,
) -> Result<CalibrationSolution, CalibrationYamlError> {
    let mut fields = OpenCvPinholeRadtanYamlFields::default();
    for (line_index, raw_line) in input.lines().enumerate() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with('#')
            || trimmed.starts_with('%')
            || trimmed == "---"
            || trimmed == "..."
        {
            continue;
        }
        let content = trimmed
            .split_once('#')
            .map_or(trimmed, |(before, _)| before.trim());
        if content.is_empty() {
            continue;
        }
        let Some((key, raw_value)) = content.split_once(':') else {
            return Err(CalibrationYamlError::InvalidLine {
                line: line_index + 1,
            });
        };
        let key = key.trim();
        let raw_value = raw_value.trim();
        match key {
            "fx" => set_f64_field(&mut fields.fx, "fx", raw_value)?,
            "fy" => set_f64_field(&mut fields.fy, "fy", raw_value)?,
            "cx" => set_f64_field(&mut fields.cx, "cx", raw_value)?,
            "cy" => set_f64_field(&mut fields.cy, "cy", raw_value)?,
            "k1" => set_f64_field(&mut fields.k1, "k1", raw_value)?,
            "k2" => set_f64_field(&mut fields.k2, "k2", raw_value)?,
            "p1" => set_f64_field(&mut fields.p1, "p1", raw_value)?,
            "p2" => set_f64_field(&mut fields.p2, "p2", raw_value)?,
            "k3" => set_f64_field(&mut fields.k3, "k3", raw_value)?,
            "k4" => set_f64_field(&mut fields.k4, "k4", raw_value)?,
            "k5" => set_f64_field(&mut fields.k5, "k5", raw_value)?,
            "k6" => set_f64_field(&mut fields.k6, "k6", raw_value)?,
            "s1" => set_f64_field(&mut fields.s1, "s1", raw_value)?,
            "s2" => set_f64_field(&mut fields.s2, "s2", raw_value)?,
            "s3" => set_f64_field(&mut fields.s3, "s3", raw_value)?,
            "s4" => set_f64_field(&mut fields.s4, "s4", raw_value)?,
            "width" => set_u32_field(&mut fields.width, "width", raw_value)?,
            "height" => set_u32_field(&mut fields.height, "height", raw_value)?,
            _ => {}
        }
    }

    let fx = required_field(fields.fx, "fx")?;
    let fy = required_field(fields.fy, "fy")?;
    let cx = required_field(fields.cx, "cx")?;
    let cy = required_field(fields.cy, "cy")?;
    let distortion_coefficients = vec![
        required_field(fields.k1, "k1")?,
        required_field(fields.k2, "k2")?,
        required_field(fields.p1, "p1")?,
        required_field(fields.p2, "p2")?,
        required_field(fields.k3, "k3")?,
        required_field(fields.k4, "k4")?,
        required_field(fields.k5, "k5")?,
        required_field(fields.k6, "k6")?,
        required_field(fields.s1, "s1")?,
        required_field(fields.s2, "s2")?,
        required_field(fields.s3, "s3")?,
        required_field(fields.s4, "s4")?,
    ];
    let image_size = CalibrationImageSize::new(
        required_field(fields.width, "width")?,
        required_field(fields.height, "height")?,
    )?;
    let camera_matrix = [fx, 0.0, cx, 0.0, fy, cy, 0.0, 0.0, 1.0];
    InitialIntrinsics {
        camera_matrix,
        distortion_coefficients: distortion_coefficients.clone(),
    }
    .validate()?;

    Ok(CalibrationSolution {
        image_size,
        camera_matrix,
        distortion_coefficients,
        rms_error: 0.0,
        calibration_flags: PANGBOT_CALIBRATION_FLAGS,
        views: Vec::new(),
    })
}

fn set_f64_field(
    slot: &mut Option<f64>,
    field: &'static str,
    raw_value: &str,
) -> Result<(), CalibrationYamlError> {
    let value = raw_value
        .parse::<f64>()
        .map_err(|_| CalibrationYamlError::InvalidField {
            field,
            value: raw_value.to_owned(),
        })?;
    if !value.is_finite() {
        return Err(CalibrationYamlError::NonFiniteField { field });
    }
    set_field(slot, field, value)
}

fn set_u32_field(
    slot: &mut Option<u32>,
    field: &'static str,
    raw_value: &str,
) -> Result<(), CalibrationYamlError> {
    let value = raw_value
        .parse::<u32>()
        .map_err(|_| CalibrationYamlError::InvalidField {
            field,
            value: raw_value.to_owned(),
        })?;
    set_field(slot, field, value)
}

fn set_field<T>(
    slot: &mut Option<T>,
    field: &'static str,
    value: T,
) -> Result<(), CalibrationYamlError> {
    if slot.replace(value).is_some() {
        return Err(CalibrationYamlError::DuplicateField { field });
    }
    Ok(())
}

fn required_field<T>(value: Option<T>, field: &'static str) -> Result<T, CalibrationYamlError> {
    value.ok_or(CalibrationYamlError::MissingField { field })
}

#[derive(Debug, Error)]
pub enum CalibrationYamlError {
    #[error("Pinhole-Radtan D12 YAML requires {required} distortion coefficients, got {actual}")]
    MissingDistortionCoefficients { required: usize, actual: usize },
    #[error("Pinhole-Radtan D12 YAML line {line} is not a key-value field")]
    InvalidLine { line: usize },
    #[error("Pinhole-Radtan D12 YAML field {field} is duplicated")]
    DuplicateField { field: &'static str },
    #[error("Pinhole-Radtan D12 YAML field {field} is missing")]
    MissingField { field: &'static str },
    #[error("Pinhole-Radtan D12 YAML field {field} has invalid value {value:?}")]
    InvalidField { field: &'static str, value: String },
    #[error("Pinhole-Radtan D12 YAML field {field} is NaN or infinity")]
    NonFiniteField { field: &'static str },
    #[error("Pinhole-Radtan D12 YAML data is invalid: {0}")]
    InvalidData(#[from] CalibrationDataError),
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
    fn parses_d12_yaml_text_layout() {
        let yaml = "%YAML:1.0\n# Pinhole-Radtan intrinsics\nfx: 878.7023\nfy: 878.5325\ncx: 955.6284\ncy: 533.1718\nk1: 0.0345\nk2: -0.0458\np1: -0.00008590\np2: 0.00015387\nk3: 0.0119\nk4: -0.0123\nk5: 0.0234\nk6: -0.0345\ns1: 0.00001111\ns2: -0.00002222\ns3: 0.00003333\ns4: -0.00004444\nwidth: 1920\nheight: 1080\n";

        let solution = parse_opencv_pinhole_radtan_yaml(yaml).unwrap();

        assert_eq!(
            solution.image_size,
            CalibrationImageSize::new(1920, 1080).unwrap()
        );
        assert_eq!(
            solution.camera_matrix,
            [
                878.7023, 0.0, 955.6284, 0.0, 878.5325, 533.1718, 0.0, 0.0, 1.0
            ]
        );
        assert_eq!(solution.distortion_coefficients.len(), 12);
        assert_eq!(solution.distortion_coefficients[8], 0.000_011_11);
        assert_eq!(solution.rms_error, 0.0);
        assert_eq!(solution.calibration_flags, PANGBOT_CALIBRATION_FLAGS);
        assert!(solution.views.is_empty());
    }

    #[test]
    fn rejects_missing_duplicate_or_nonfinite_yaml_fields() {
        assert!(matches!(
            parse_opencv_pinhole_radtan_yaml("fx: 1\n"),
            Err(CalibrationYamlError::MissingField { field: "fy" })
        ));
        assert!(matches!(
            parse_opencv_pinhole_radtan_yaml("fx: 1\nfx: 2\n"),
            Err(CalibrationYamlError::DuplicateField { field: "fx" })
        ));
        assert!(matches!(
            parse_opencv_pinhole_radtan_yaml("fx: NaN\n"),
            Err(CalibrationYamlError::NonFiniteField { field: "fx" })
        ));
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
