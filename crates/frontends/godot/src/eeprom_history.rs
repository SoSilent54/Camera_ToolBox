//! EEPROM write_history YAML：文件名与核心字段对齐原 Camera_Toolbox。

use camera_toolbox_app::platform::EepromWriteResult;
use camera_toolbox_core::{
    CalibrationSolution, EepromProvisionRequest, EepromWriteSegment, FullEepromImage,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, fs};

const EEPROM_SERIAL_BYTES: usize = 14;

/// 保存单路 EEPROM 写入审计 YAML；文件名按 SNID 解码生成。
pub fn persist_write_history(
    channel_label: &str,
    i2c_bus: u16,
    serial_number: &str,
    solution: &CalibrationSolution,
    result: &EepromWriteResult,
) -> Result<String, String> {
    let operation_id = operation_id();
    let request = FullEepromImage::from_solution(solution, serial_number)
        .map_err(|error| format!("构造 EEPROM history 请求镜像失败：{error}"))?
        .full_provision_request(false);
    let document = json!({
        "schema_version": "camera-toolbox.eeprom-write-history.v1",
        "tool": "pongbot-calib-tool-godot",
        "operation_id": format!("{operation_id:016x}"),
        "timestamp_unix_secs": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| format!("system clock before UNIX_EPOCH: {error}"))?
            .as_secs(),
        "target": {
            "label": channel_label,
            "i2c_bus": i2c_bus,
        },
        "request": {
            "action": "provision",
            "expected_before_sha256": result.before.image_sha256,
            "request": eeprom_request_parameters_json(&request),
        },
        "calibration_parameters": calibration_solution_json(solution),
        "result": write_result_json(result),
    });
    let path = new_eeprom_history_path(serial_number)?;
    let bytes = serialize_yaml(&document)?;
    create_new_file(&path, &bytes, operation_id, "EEPROM write history")?;
    Ok(path.display().to_string())
}

fn eeprom_request_parameters_json(request: &EepromProvisionRequest) -> serde_json::Value {
    json!({
        "map_id": request.map_id,
        "mode": request.mode,
        "serial_number": request.serial_number,
        "overwrite_existing_serial": request.overwrite_existing_serial,
        "snid": snid_json(&request.serial_number),
        "calibration_parameters": eeprom_calibration_parameters_json(request),
        "write_segments": request.segments.iter().map(eeprom_write_segment_json).collect::<Vec<_>>(),
    })
}

fn eeprom_write_segment_json(segment: &EepromWriteSegment) -> serde_json::Value {
    json!({
        "offset": format!("0x{:04x}", segment.offset),
        "offset_u16": segment.offset,
        "byte_len": segment.bytes.len(),
        "purpose": eeprom_segment_purpose(segment.offset),
        "payload_sha256": sha256_hex(&segment.bytes),
        "semantic_value": eeprom_segment_semantic_json(segment),
    })
}

fn eeprom_segment_purpose(offset: u16) -> &'static str {
    match offset {
        0x0000 => "valid_flag",
        0x0010 => "calibration_parameters",
        0x0125 => "serial_number_and_checksum",
        _ => "custom_segment",
    }
}

fn eeprom_segment_semantic_json(segment: &EepromWriteSegment) -> serde_json::Value {
    match segment.offset {
        0x0000 if segment.bytes == b"hessian\0" => json!({"flag_ascii": "hessian\\0"}),
        0x0010 => {
            decode_calibration_segment_json(&segment.bytes).unwrap_or(serde_json::Value::Null)
        }
        0x0125 if segment.bytes.len() >= EEPROM_SERIAL_BYTES => {
            let serial = std::str::from_utf8(&segment.bytes[..EEPROM_SERIAL_BYTES]).ok();
            json!({
                "serial_number": serial,
                "snid": serial.map(snid_json),
                "checksum_u8": segment.bytes.get(EEPROM_SERIAL_BYTES).copied(),
                "checksum_hex": segment.bytes.get(EEPROM_SERIAL_BYTES).map(|value| format!("0x{value:02x}")),
            })
        }
        _ => serde_json::Value::Null,
    }
}

fn eeprom_calibration_parameters_json(request: &EepromProvisionRequest) -> serde_json::Value {
    request
        .segments
        .iter()
        .find(|segment| segment.offset == 0x0010)
        .and_then(|segment| decode_calibration_segment_json(&segment.bytes))
        .unwrap_or(serde_json::Value::Null)
}

fn decode_calibration_segment_json(bytes: &[u8]) -> Option<serde_json::Value> {
    let width = read_u32_le(bytes, 0)?;
    let height = read_u32_le(bytes, 4)?;
    let fx = read_f32_le(bytes, 8)?;
    let fy = read_f32_le(bytes, 12)?;
    let cx = read_f32_le(bytes, 16)?;
    let cy = read_f32_le(bytes, 20)?;
    let distortion = (0..12)
        .map(|index| read_f32_le(bytes, 24 + index * 4))
        .collect::<Option<Vec<_>>>()?;
    Some(json!({
        "image_size": {"width": width, "height": height},
        "camera_matrix": {
            "fx": fx,
            "fy": fy,
            "cx": cx,
            "cy": cy,
            "matrix_3x3": [[fx, 0.0, cx], [0.0, fy, cy], [0.0, 0.0, 1.0]],
        },
        "distortion": {
            "model": "opencv_pinhole_radtan_thin_prism_d12",
            "coefficients": {
                "k1": distortion[0], "k2": distortion[1], "p1": distortion[2], "p2": distortion[3],
                "k3": distortion[4], "k4": distortion[5], "k5": distortion[6], "k6": distortion[7],
                "s1": distortion[8], "s2": distortion[9], "s3": distortion[10], "s4": distortion[11],
            },
        },
    }))
}

fn read_u32_le(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn read_f32_le(bytes: &[u8], offset: usize) -> Option<f32> {
    Some(f32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}
fn write_result_json(result: &EepromWriteResult) -> serde_json::Value {
    json!({
        "before": result.before,
        "after": result.after,
        "backup_sha256": sha256_hex(&result.backup),
        "backup_bytes": result.backup.len(),
        "page_plan": result.page_plan,
        "bytewise_verified": result.bytewise_verified,
        "rollback": result.rollback,
    })
}

fn calibration_solution_json(solution: &CalibrationSolution) -> serde_json::Value {
    json!({
        "image_size": {
            "width": solution.image_size.width,
            "height": solution.image_size.height,
        },
        "camera_matrix_3x3": [
            [solution.camera_matrix[0], solution.camera_matrix[1], solution.camera_matrix[2]],
            [solution.camera_matrix[3], solution.camera_matrix[4], solution.camera_matrix[5]],
            [solution.camera_matrix[6], solution.camera_matrix[7], solution.camera_matrix[8]],
        ],
        "distortion": {
            "model": "opencv_pinhole_radtan_thin_prism_d12",
            "coefficients": solution.distortion_coefficients,
        },
        "rms_error": solution.rms_error,
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut text = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut text, "{byte:02x}");
    }
    text
}

fn operation_id() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    (nanos as u64) ^ std::process::id() as u64
}

fn serialize_yaml(document: &serde_json::Value) -> Result<Vec<u8>, String> {
    let mut text = serde_yaml::to_string(document).map_err(|error| error.to_string())?;
    if !text.ends_with('\n') {
        text.push('\n');
    }
    Ok(text.into_bytes())
}

fn new_eeprom_history_path(serial_number: &str) -> Result<PathBuf, String> {
    let serial = safe_eeprom_history_stem(serial_number)?;
    let history_dir = eeprom_history_dir();
    let target_path = eeprom_history_path_in(&history_dir, &serial)?;
    let target_name = eeprom_file_name_to_string(&target_path)?;
    let entries = match fs::read_dir(&history_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(target_path),
        Err(error) => {
            return Err(format!(
                "Failed to inspect EEPROM write history directory {} before writing SN {serial}: {error}",
                history_dir.display()
            ));
        }
    };
    let mut occupied_target_path = None;
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "Failed to inspect EEPROM write history directory {} before writing SN {serial}: {error}",
                history_dir.display()
            )
        })?;
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str().map(str::to_owned) else {
            continue;
        };
        let path = entry.path();
        if eeprom_history_may_record_snid(&file_name)
            && eeprom_history_recorded_serial_number(&path).as_deref() == Some(serial.as_str())
        {
            return Err(format!(
                "Write history already records SN {serial}: {}. Refusing to start EEPROM write; archive the existing file before retrying.",
                path.display()
            ));
        }
        if file_name.eq_ignore_ascii_case(&target_name) {
            occupied_target_path = Some(path);
        }
    }
    if let Some(path) = occupied_target_path {
        return Err(format!(
            "EEPROM write history filename for SN {serial} is already occupied by {} but that file does not record the same SNID. Refusing to start EEPROM write; archive or repair the history file before retrying.",
            path.display()
        ));
    }
    Ok(target_path)
}

fn eeprom_history_path_in(history_dir: &Path, serial_number: &str) -> Result<PathBuf, String> {
    let file_name = safe_eeprom_history_file_name(serial_number)?;
    Ok(history_dir.join(file_name))
}

fn eeprom_history_dir() -> PathBuf {
    env::var_os("PONGBOT_WRITE_HISTORY_DIR")
        .and_then(|value| {
            let path = PathBuf::from(value);
            (!path.as_os_str().is_empty()).then_some(path)
        })
        .unwrap_or_else(|| PathBuf::from("write_history"))
}

fn safe_eeprom_history_stem(serial_number: &str) -> Result<String, String> {
    let serial = serial_number.trim();
    if serial.is_empty() {
        return Err("EEPROM serial number is empty; cannot create write history file".to_owned());
    }
    if !serial
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(format!(
            "EEPROM serial number {serial:?} cannot be used as a write history filename"
        ));
    }
    Ok(serial.to_owned())
}

fn safe_eeprom_history_file_name(serial_number: &str) -> Result<String, String> {
    let serial = safe_eeprom_history_stem(serial_number)?;
    let bytes = serial.as_bytes();
    if bytes.len() != EEPROM_SERIAL_BYTES {
        return Err(format!(
            "EEPROM serial number {serial:?} must contain exactly {EEPROM_SERIAL_BYTES} ASCII bytes to create write history filename"
        ));
    }
    let prefix = format!(
        "{}{}{}",
        std::str::from_utf8(&bytes[0..5]).expect("safe EEPROM serial stem is ASCII"),
        char::from(bytes[9]),
        std::str::from_utf8(&bytes[12..14]).expect("safe EEPROM serial stem is ASCII")
    );
    let year = std::str::from_utf8(&bytes[5..7]).expect("safe EEPROM serial stem is ASCII");
    let month = decode_snid_month(bytes[7])
        .ok_or_else(|| format!("EEPROM serial number {serial:?} has invalid encoded month"))?;
    let day = decode_snid_day(bytes[8])
        .ok_or_else(|| format!("EEPROM serial number {serial:?} has invalid encoded day"))?;
    let sequence = decode_snid_sequence(bytes[10], bytes[11])
        .ok_or_else(|| format!("EEPROM serial number {serial:?} has invalid encoded sequence"))?;
    Ok(format!("{prefix}_{year}{month:02}{day:02}_{sequence}.yaml"))
}

fn eeprom_file_name_to_string(path: &Path) -> Result<String, String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .ok_or_else(|| {
            format!(
                "EEPROM write history path {} has no UTF-8 file name",
                path.display()
            )
        })
}

fn eeprom_history_may_record_snid(file_name: &str) -> bool {
    file_name.rsplit_once('.').is_some_and(|(_, extension)| {
        extension.eq_ignore_ascii_case("yaml") || extension.eq_ignore_ascii_case("json")
    })
}

fn eeprom_history_recorded_serial_number(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    let document: serde_json::Value = serde_yaml::from_slice(&bytes).ok()?;
    document
        .pointer("/request/request/serial_number")
        .or_else(|| document.pointer("/request/request/snid/raw"))
        .or_else(|| document.pointer("/request/serial_number"))
        .or_else(|| document.pointer("/request/snid/raw"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

fn create_new_file(
    path: &Path,
    bytes: &[u8],
    operation_id: u64,
    label: &str,
) -> Result<(), String> {
    let parent = path.parent().map(Path::to_path_buf);
    if let Some(parent) = parent.as_deref() {
        ensure_directory_durable(parent, label)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| {
            format!(
                "failed to create {label} for operation {operation_id} at {}: {error}",
                path.display()
            )
        })?;
    file.write_all(bytes).map_err(|error| {
        format!(
            "failed to write {label} for operation {operation_id} at {}: {error}",
            path.display()
        )
    })?;
    file.sync_all().map_err(|error| {
        format!(
            "failed to sync {label} for operation {operation_id} at {}: {error}",
            path.display()
        )
    })?;
    if let Some(parent) = parent.as_deref() {
        sync_directory(parent, label)?;
    }
    Ok(())
}

fn ensure_directory_durable(path: &Path, label: &str) -> Result<(), String> {
    let existed = path.exists();
    fs::create_dir_all(path).map_err(|error| {
        format!(
            "failed to create {label} directory {}: {error}",
            path.display()
        )
    })?;
    if !existed {
        sync_directory(path_parent_or_current(path), label)?;
    }
    Ok(())
}

fn path_parent_or_current(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

#[cfg(not(windows))]
fn sync_directory(path: &Path, label: &str) -> Result<(), String> {
    let directory = std::fs::File::open(path).map_err(|error| {
        format!(
            "failed to open {label} directory {} for sync: {error}",
            path.display()
        )
    })?;
    directory.sync_all().map_err(|error| {
        format!(
            "failed to sync {label} directory {}: {error}",
            path.display()
        )
    })
}

#[cfg(windows)]
fn sync_directory(_path: &Path, _label: &str) -> Result<(), String> {
    Ok(())
}

fn snid_json(serial_number: &str) -> serde_json::Value {
    let bytes = serial_number.as_bytes();
    if bytes.len() != EEPROM_SERIAL_BYTES {
        return json!({
            "raw": serial_number,
            "decoded": null,
            "error": "SNID must be 14 ASCII bytes",
        });
    }
    json!({
        "raw": serial_number,
        "resolution": {
            "code": char::from(bytes[0]).to_string(),
            "meaning": if bytes[0] == b'2' { "FHD" } else { "unknown" },
        },
        "vendor": {
            "code": char::from(bytes[1]).to_string(),
            "meaning": if bytes[1] == b'T' { "SmartSens" } else { "unknown" },
        },
        "module": std::str::from_utf8(&bytes[2..5]).unwrap_or(""),
        "year": std::str::from_utf8(&bytes[5..7]).unwrap_or(""),
        "month": {
            "input_decimal": decode_snid_month(bytes[7]),
            "encoded": char::from(bytes[7]).to_string(),
        },
        "day": {
            "input_decimal": decode_snid_day(bytes[8]),
            "encoded": char::from(bytes[8]).to_string(),
        },
        "optical_axis_class": {
            "input": decode_ascii_digit(bytes[9]),
            "encoded": char::from(bytes[9]).to_string(),
        },
        "sequence": {
            "input_decimal": decode_snid_sequence(bytes[10], bytes[11]),
            "encoded_high": char::from(bytes[10]).to_string(),
            "encoded_low": char::from(bytes[11]).to_string(),
        },
        "algorithm_version": char::from(bytes[12]).to_string(),
        "reserved": char::from(bytes[13]).to_string(),
    })
}

fn decode_snid_month(byte: u8) -> Option<u8> {
    match byte {
        b'1'..=b'9' => Some(byte - b'0'),
        b'A'..=b'C' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn decode_snid_day(byte: u8) -> Option<u8> {
    match byte {
        b'1'..=b'9' => Some(byte - b'0'),
        b'A'..=b'V' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn decode_snid_sequence(high: u8, low: u8) -> Option<u16> {
    Some(decode_base62_digit(high)? * 62 + decode_base62_digit(low)? + 1)
}

fn decode_base62_digit(byte: u8) -> Option<u16> {
    match byte {
        b'0'..=b'9' => Some(u16::from(byte - b'0')),
        b'a'..=b'z' => Some(u16::from(byte - b'a') + 10),
        b'A'..=b'Z' => Some(u16::from(byte - b'A') + 36),
        _ => None,
    }
}

fn decode_ascii_digit(byte: u8) -> Option<u8> {
    byte.is_ascii_digit().then_some(byte - b'0')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_file_name_matches_camera_toolbox_snid_layout() {
        assert_eq!(
            safe_eeprom_history_file_name("2T23326AV40000").unwrap(),
            "2T233400_261031_1.yaml"
        );
        assert_eq!(
            safe_eeprom_history_file_name("2T235991V4ZZ00").unwrap(),
            "2T235400_990131_3844.yaml"
        );
    }

    #[test]
    fn history_file_name_rejects_non_snid_text() {
        assert!(safe_eeprom_history_file_name("2T23326AV4ZZ0").is_err());
        assert!(safe_eeprom_history_file_name("2T23326AV4!!00").is_err());
    }
}
