//! P24C64G 标定 EEPROM 的纯数据布局与编码。
//!
//! 本模块只把标定结果编码成固定字段和写段；I²C、SSH、权限与写入提交顺序由上层
//! provisioning helper 负责。这样离线 BIN 导出和直接远程烧录始终使用同一字段载荷。

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::calibration::CalibrationSolution;

/// 当前 Yg Stereo 标定 EEPROM 映射的稳定标识。
pub const YG_STEREO_P24C64G_V1_MAP_ID: &str = "yg-stereo-p24c64g-v1";
/// 兼容既有解析器的完整 EEPROM 镜像长度。
pub const YG_STEREO_P24C64G_IMAGE_BYTES: usize = 0x134;
/// 相机内参与畸变参数的连续写入区长度。
pub const YG_STEREO_P24C64G_INTRINSICS_BYTES: usize = 72;
/// 标定有效 FLAG；helper 必须在所有载荷回读通过后最后提交它。
pub const YG_STEREO_P24C64G_FLAG: [u8; 8] = *b"hessian\0";

const FLAG_OFFSET: usize = 0x0000;
const WIDTH_OFFSET: usize = 0x0010;
const HEIGHT_OFFSET: usize = 0x0014;
const FX_OFFSET: usize = 0x0018;
const FY_OFFSET: usize = 0x001c;
const CX_OFFSET: usize = 0x0020;
const CY_OFFSET: usize = 0x0024;
const DISTORTION_OFFSET: usize = 0x0028;
const SERIAL_OFFSET: usize = 0x0125;
const SERIAL_BYTES: usize = 14;
const SERIAL_CHECKSUM_OFFSET: usize = 0x0133;

/// EEPROM 字段的基础解析方式；固定宽度类型的 byte_len 必须等于类型宽度。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StorageEncoding {
    Ascii,
    AsciiNulTerminated,
    Raw,
    Reserved,
    U8,
    U16Le,
    I16Le,
    U32Le,
    I32Le,
    F32Le,
    F64Le,
    SerialChecksum,
}

/// 一个可审计的 EEPROM 字段范围；`remark` 是 UI 表格里的短名称，不是长描述。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageField {
    pub name: &'static str,
    pub remark: &'static str,
    pub offset: u16,
    pub byte_len: u16,
    pub encoding: StorageEncoding,
    pub full_provision_writable: bool,
    pub update_writable: bool,
}

/// EEPROM 页写协议参数。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EepromTransportSpec {
    pub i2c_address: u8,
    pub address_width_bits: u8,
    pub page_size_bytes: u16,
    pub write_cycle_ms: u16,
}

/// 一个受支持标定存储设备的不可变布局。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CalibrationStorageMap {
    pub id: &'static str,
    pub display_name: &'static str,
    pub transport: EepromTransportSpec,
    pub fields: &'static [StorageField],
}

const YG_STEREO_FIELDS: [StorageField; 6] = [
    StorageField {
        name: "flag",
        remark: "FLAG",
        offset: FLAG_OFFSET as u16,
        byte_len: YG_STEREO_P24C64G_FLAG.len() as u16,
        encoding: StorageEncoding::Ascii,
        full_provision_writable: true,
        update_writable: false,
    },
    StorageField {
        name: "image_size",
        remark: "width/height",
        offset: WIDTH_OFFSET as u16,
        byte_len: 8,
        encoding: StorageEncoding::U32Le,
        full_provision_writable: true,
        update_writable: true,
    },
    StorageField {
        name: "camera_matrix",
        remark: "fx/fy/cx/cy",
        offset: FX_OFFSET as u16,
        byte_len: 16,
        encoding: StorageEncoding::F32Le,
        full_provision_writable: true,
        update_writable: true,
    },
    StorageField {
        name: "distortion",
        remark: "k1..s4",
        offset: DISTORTION_OFFSET as u16,
        byte_len: 48,
        encoding: StorageEncoding::F32Le,
        full_provision_writable: true,
        update_writable: true,
    },
    StorageField {
        name: "serial_number",
        remark: "SNID",
        offset: SERIAL_OFFSET as u16,
        byte_len: SERIAL_BYTES as u16,
        encoding: StorageEncoding::Ascii,
        full_provision_writable: true,
        update_writable: false,
    },
    StorageField {
        name: "serial_checksum",
        remark: "SNCHK",
        offset: SERIAL_CHECKSUM_OFFSET as u16,
        byte_len: 1,
        encoding: StorageEncoding::SerialChecksum,
        full_provision_writable: true,
        update_writable: false,
    },
];

const YG_STEREO_MAP: CalibrationStorageMap = CalibrationStorageMap {
    id: YG_STEREO_P24C64G_V1_MAP_ID,
    display_name: "Yg Stereo P24C64G v1",
    transport: EepromTransportSpec {
        i2c_address: 0x50,
        address_width_bits: 16,
        page_size_bytes: 32,
        write_cycle_ms: 10,
    },
    fields: &YG_STEREO_FIELDS,
};

/// 返回当前已知 P24C64G 模组的标定 EEPROM 映射。
#[must_use]
pub const fn yg_stereo_p24c64g_v1() -> &'static CalibrationStorageMap {
    &YG_STEREO_MAP
}

/// Baton `param_rw` native ABI EEPROM 映射；该布局依赖 AArch64/LP64/little-endian `double` ABI。
pub const BATON_PARAM_RW_NATIVE_LP64_LE_V1_MAP_ID: &str = "baton-param-rw-native-lp64-le-v1";
/// Baton `param_rw` 当前 `sizeof(eeprom_data)`，未 packed，包含 ABI padding。
pub const BATON_PARAM_RW_IMAGE_BYTES: usize = 1008;

macro_rules! storage_field {
    ($name:literal, $remark:literal, $offset:expr, $encoding:ident, $byte_len:expr, $writable:expr) => {
        StorageField {
            name: $name,
            remark: $remark,
            offset: $offset,
            byte_len: $byte_len,
            encoding: StorageEncoding::$encoding,
            full_provision_writable: $writable,
            update_writable: $writable,
        }
    };
}

const BATON_PARAM_RW_SHARED_PREFIX_FIELD_COUNT: usize = 79;
const BATON_PARAM_RW_LEGACY_SUFFIX_FIELD_COUNT: usize = 52;
const BATON_PARAM_RW_FIELD_COUNT: usize =
    BATON_PARAM_RW_SHARED_PREFIX_FIELD_COUNT + BATON_PARAM_RW_LEGACY_SUFFIX_FIELD_COUNT;

const BATON_PARAM_RW_SHARED_PREFIX_FIELDS: [StorageField;
    BATON_PARAM_RW_SHARED_PREFIX_FIELD_COUNT] = [
    storage_field!(
        "fish_param.left_cam.fx",
        "FISH.L.fx",
        0x0000,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "fish_param.left_cam.fy",
        "FISH.L.fy",
        0x0008,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "fish_param.left_cam.cx",
        "FISH.L.cx",
        0x0010,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "fish_param.left_cam.cy",
        "FISH.L.cy",
        0x0018,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "fish_param.left_cam.xi",
        "FISH.L.xi",
        0x0020,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "fish_param.left_cam.alpha",
        "FISH.L.alpha",
        0x0028,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "fish_param.right_cam.fx",
        "FISH.R.fx",
        0x0030,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "fish_param.right_cam.fy",
        "FISH.R.fy",
        0x0038,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "fish_param.right_cam.cx",
        "FISH.R.cx",
        0x0040,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "fish_param.right_cam.cy",
        "FISH.R.cy",
        0x0048,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "fish_param.right_cam.xi",
        "FISH.R.xi",
        0x0050,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "fish_param.right_cam.alpha",
        "FISH.R.alpha",
        0x0058,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "fish_param.extrinsic.px",
        "FISH.X.px",
        0x0060,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "fish_param.extrinsic.py",
        "FISH.X.py",
        0x0068,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "fish_param.extrinsic.pz",
        "FISH.X.pz",
        0x0070,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "fish_param.extrinsic.qx",
        "FISH.X.qx",
        0x0078,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "fish_param.extrinsic.qy",
        "FISH.X.qy",
        0x0080,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "fish_param.extrinsic.qz",
        "FISH.X.qz",
        0x0088,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "fish_param.extrinsic.qw",
        "FISH.X.qw",
        0x0090,
        F64Le,
        8,
        true
    ),
    storage_field!("fish_param_check_sum", "FISHCK", 0x0098, U8, 1, true),
    storage_field!(
        "padding.after_fish_checksum",
        "PAD",
        0x0099,
        Reserved,
        7,
        false
    ),
    storage_field!("cam_param.left_cam.fx", "CAM.L.fx", 0x00a0, F64Le, 8, true),
    storage_field!("cam_param.left_cam.fy", "CAM.L.fy", 0x00a8, F64Le, 8, true),
    storage_field!("cam_param.left_cam.cx", "CAM.L.cx", 0x00b0, F64Le, 8, true),
    storage_field!("cam_param.left_cam.cy", "CAM.L.cy", 0x00b8, F64Le, 8, true),
    storage_field!("cam_param.left_cam.k1", "CAM.L.k1", 0x00c0, F64Le, 8, true),
    storage_field!("cam_param.left_cam.k2", "CAM.L.k2", 0x00c8, F64Le, 8, true),
    storage_field!("cam_param.left_cam.k3", "CAM.L.k3", 0x00d0, F64Le, 8, true),
    storage_field!("cam_param.left_cam.p1", "CAM.L.p1", 0x00d8, F64Le, 8, true),
    storage_field!("cam_param.left_cam.p2", "CAM.L.p2", 0x00e0, F64Le, 8, true),
    storage_field!("cam_param.right_cam.fx", "CAM.R.fx", 0x00e8, F64Le, 8, true),
    storage_field!("cam_param.right_cam.fy", "CAM.R.fy", 0x00f0, F64Le, 8, true),
    storage_field!("cam_param.right_cam.cx", "CAM.R.cx", 0x00f8, F64Le, 8, true),
    storage_field!("cam_param.right_cam.cy", "CAM.R.cy", 0x0100, F64Le, 8, true),
    storage_field!("cam_param.right_cam.k1", "CAM.R.k1", 0x0108, F64Le, 8, true),
    storage_field!("cam_param.right_cam.k2", "CAM.R.k2", 0x0110, F64Le, 8, true),
    storage_field!("cam_param.right_cam.k3", "CAM.R.k3", 0x0118, F64Le, 8, true),
    storage_field!("cam_param.right_cam.p1", "CAM.R.p1", 0x0120, F64Le, 8, true),
    storage_field!("cam_param.right_cam.p2", "CAM.R.p2", 0x0128, F64Le, 8, true),
    storage_field!(
        "cam_param.extrinsic.r00",
        "CAM.X.r00",
        0x0130,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_param.extrinsic.r01",
        "CAM.X.r01",
        0x0138,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_param.extrinsic.r02",
        "CAM.X.r02",
        0x0140,
        F64Le,
        8,
        true
    ),
    storage_field!("cam_param.extrinsic.t0", "CAM.X.t0", 0x0148, F64Le, 8, true),
    storage_field!(
        "cam_param.extrinsic.r10",
        "CAM.X.r10",
        0x0150,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_param.extrinsic.r11",
        "CAM.X.r11",
        0x0158,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_param.extrinsic.r12",
        "CAM.X.r12",
        0x0160,
        F64Le,
        8,
        true
    ),
    storage_field!("cam_param.extrinsic.t1", "CAM.X.t1", 0x0168, F64Le, 8, true),
    storage_field!(
        "cam_param.extrinsic.r20",
        "CAM.X.r20",
        0x0170,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_param.extrinsic.r21",
        "CAM.X.r21",
        0x0178,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_param.extrinsic.r22",
        "CAM.X.r22",
        0x0180,
        F64Le,
        8,
        true
    ),
    storage_field!("cam_param.extrinsic.t2", "CAM.X.t2", 0x0188, F64Le, 8, true),
    storage_field!("cam_param_check_sum", "CAMCK", 0x0190, U8, 1, true),
    storage_field!(
        "padding.after_cam_checksum",
        "PAD",
        0x0191,
        Reserved,
        7,
        false
    ),
    storage_field!(
        "cam_imu_extrinsic.left_cam_imu.r00",
        "CIMU.L.r00",
        0x0198,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_imu_extrinsic.left_cam_imu.r01",
        "CIMU.L.r01",
        0x01a0,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_imu_extrinsic.left_cam_imu.r02",
        "CIMU.L.r02",
        0x01a8,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_imu_extrinsic.left_cam_imu.t0",
        "CIMU.L.t0",
        0x01b0,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_imu_extrinsic.left_cam_imu.r10",
        "CIMU.L.r10",
        0x01b8,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_imu_extrinsic.left_cam_imu.r11",
        "CIMU.L.r11",
        0x01c0,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_imu_extrinsic.left_cam_imu.r12",
        "CIMU.L.r12",
        0x01c8,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_imu_extrinsic.left_cam_imu.t1",
        "CIMU.L.t1",
        0x01d0,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_imu_extrinsic.left_cam_imu.r20",
        "CIMU.L.r20",
        0x01d8,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_imu_extrinsic.left_cam_imu.r21",
        "CIMU.L.r21",
        0x01e0,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_imu_extrinsic.left_cam_imu.r22",
        "CIMU.L.r22",
        0x01e8,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_imu_extrinsic.left_cam_imu.t2",
        "CIMU.L.t2",
        0x01f0,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_imu_extrinsic.right_cam_imu.r00",
        "CIMU.R.r00",
        0x01f8,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_imu_extrinsic.right_cam_imu.r01",
        "CIMU.R.r01",
        0x0200,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_imu_extrinsic.right_cam_imu.r02",
        "CIMU.R.r02",
        0x0208,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_imu_extrinsic.right_cam_imu.t0",
        "CIMU.R.t0",
        0x0210,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_imu_extrinsic.right_cam_imu.r10",
        "CIMU.R.r10",
        0x0218,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_imu_extrinsic.right_cam_imu.r11",
        "CIMU.R.r11",
        0x0220,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_imu_extrinsic.right_cam_imu.r12",
        "CIMU.R.r12",
        0x0228,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_imu_extrinsic.right_cam_imu.t1",
        "CIMU.R.t1",
        0x0230,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_imu_extrinsic.right_cam_imu.r20",
        "CIMU.R.r20",
        0x0238,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_imu_extrinsic.right_cam_imu.r21",
        "CIMU.R.r21",
        0x0240,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_imu_extrinsic.right_cam_imu.r22",
        "CIMU.R.r22",
        0x0248,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_imu_extrinsic.right_cam_imu.t2",
        "CIMU.R.t2",
        0x0250,
        F64Le,
        8,
        true
    ),
    storage_field!("cam_imu_extrinsic_check_sum", "CIMUCK", 0x0258, U8, 1, true),
    storage_field!(
        "padding.after_cam_imu_checksum",
        "PAD",
        0x0259,
        Reserved,
        7,
        false
    ),
];

const BATON_PARAM_RW_LEGACY_SUFFIX_FIELDS: [StorageField;
    BATON_PARAM_RW_LEGACY_SUFFIX_FIELD_COUNT] = [
    storage_field!("imu_instrinsic.gyr_n", "IMU.gyr_n", 0x0260, F64Le, 8, true),
    storage_field!("imu_instrinsic.gyr_w", "IMU.gyr_w", 0x0268, F64Le, 8, true),
    storage_field!("imu_instrinsic.acc_n", "IMU.acc_n", 0x0270, F64Le, 8, true),
    storage_field!("imu_instrinsic.acc_w", "IMU.acc_w", 0x0278, F64Le, 8, true),
    storage_field!("imu_instrinsic_check_sum", "IMUCK", 0x0280, U8, 1, true),
    storage_field!(
        "padding.after_imu_instrinsic_checksum",
        "PAD",
        0x0281,
        Reserved,
        7,
        false
    ),
    storage_field!(
        "imu_elliposoid.acc_bias_vector[0]",
        "ELL.AB0",
        0x0288,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "imu_elliposoid.acc_bias_vector[1]",
        "ELL.AB1",
        0x0290,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "imu_elliposoid.acc_bias_vector[2]",
        "ELL.AB2",
        0x0298,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "imu_elliposoid.gyr_bias_vector[0]",
        "ELL.GB0",
        0x02a0,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "imu_elliposoid.gyr_bias_vector[1]",
        "ELL.GB1",
        0x02a8,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "imu_elliposoid.gyr_bias_vector[2]",
        "ELL.GB2",
        0x02b0,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "imu_elliposoid.acc_scale[0]",
        "ELL.AS0",
        0x02b8,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "imu_elliposoid.acc_scale[1]",
        "ELL.AS1",
        0x02c0,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "imu_elliposoid.acc_scale[2]",
        "ELL.AS2",
        0x02c8,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "imu_elliposoid.groy_scale[0]",
        "ELL.GS0",
        0x02d0,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "imu_elliposoid.groy_scale[1]",
        "ELL.GS1",
        0x02d8,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "imu_elliposoid.groy_scale[2]",
        "ELL.GS2",
        0x02e0,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "imu_elliposoid.acc_Misalignment[0]",
        "ELL.AM0",
        0x02e8,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "imu_elliposoid.acc_Misalignment[1]",
        "ELL.AM1",
        0x02f0,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "imu_elliposoid.acc_Misalignment[2]",
        "ELL.AM2",
        0x02f8,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "imu_elliposoid.groy_Misalignment[0]",
        "ELL.GM0",
        0x0300,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "imu_elliposoid.groy_Misalignment[1]",
        "ELL.GM1",
        0x0308,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "imu_elliposoid.groy_Misalignment[2]",
        "ELL.GM2",
        0x0310,
        F64Le,
        8,
        true
    ),
    storage_field!("imu_elliposoid_check_sum", "ELLCK", 0x0318, U8, 1, true),
    storage_field!("md_sn", "SNID", 0x0319, AsciiNulTerminated, 21, true),
    storage_field!("padding.after_md_sn", "PAD", 0x032e, Reserved, 2, false),
    storage_field!(
        "cam_rgb_param.rgb_cam.fx",
        "RGB.K.fx",
        0x0330,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_rgb_param.rgb_cam.fy",
        "RGB.K.fy",
        0x0338,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_rgb_param.rgb_cam.cx",
        "RGB.K.cx",
        0x0340,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_rgb_param.rgb_cam.cy",
        "RGB.K.cy",
        0x0348,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_rgb_param.rgb_cam.k1",
        "RGB.K.k1",
        0x0350,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_rgb_param.rgb_cam.k2",
        "RGB.K.k2",
        0x0358,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_rgb_param.rgb_cam.k3",
        "RGB.K.k3",
        0x0360,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_rgb_param.rgb_cam.p1",
        "RGB.K.p1",
        0x0368,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_rgb_param.rgb_cam.p2",
        "RGB.K.p2",
        0x0370,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_rgb_param.rgb_to_left_extrinsic.r00",
        "RGB.X.r00",
        0x0378,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_rgb_param.rgb_to_left_extrinsic.r01",
        "RGB.X.r01",
        0x0380,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_rgb_param.rgb_to_left_extrinsic.r02",
        "RGB.X.r02",
        0x0388,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_rgb_param.rgb_to_left_extrinsic.t0",
        "RGB.X.t0",
        0x0390,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_rgb_param.rgb_to_left_extrinsic.r10",
        "RGB.X.r10",
        0x0398,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_rgb_param.rgb_to_left_extrinsic.r11",
        "RGB.X.r11",
        0x03a0,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_rgb_param.rgb_to_left_extrinsic.r12",
        "RGB.X.r12",
        0x03a8,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_rgb_param.rgb_to_left_extrinsic.t1",
        "RGB.X.t1",
        0x03b0,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_rgb_param.rgb_to_left_extrinsic.r20",
        "RGB.X.r20",
        0x03b8,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_rgb_param.rgb_to_left_extrinsic.r21",
        "RGB.X.r21",
        0x03c0,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_rgb_param.rgb_to_left_extrinsic.r22",
        "RGB.X.r22",
        0x03c8,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "cam_rgb_param.rgb_to_left_extrinsic.t2",
        "RGB.X.t2",
        0x03d0,
        F64Le,
        8,
        true
    ),
    storage_field!("cam_rgb_param.width", "RGB.W", 0x03d8, F64Le, 8, true),
    storage_field!("cam_rgb_param.height", "RGB.H", 0x03e0, F64Le, 8, true),
    storage_field!("cam_rgb_param_check_sum", "RGBCK", 0x03e8, U8, 1, true),
    storage_field!("padding.tail", "PAD", 0x03e9, Reserved, 7, false),
];

const BATON_PARAM_RW_FIELDS: [StorageField; BATON_PARAM_RW_FIELD_COUNT] = combine_storage_fields::<
    BATON_PARAM_RW_FIELD_COUNT,
    BATON_PARAM_RW_SHARED_PREFIX_FIELD_COUNT,
    BATON_PARAM_RW_LEGACY_SUFFIX_FIELD_COUNT,
>(
    BATON_PARAM_RW_SHARED_PREFIX_FIELDS,
    BATON_PARAM_RW_LEGACY_SUFFIX_FIELDS,
);

const fn combine_storage_fields<const OUT: usize, const PREFIX: usize, const SUFFIX: usize>(
    prefix: [StorageField; PREFIX],
    suffix: [StorageField; SUFFIX],
) -> [StorageField; OUT] {
    let mut fields = [storage_field!("padding.unused", "PAD", 0, Reserved, 0, false); OUT];
    let mut index = 0;
    while index < PREFIX {
        fields[index] = prefix[index];
        index += 1;
    }
    let mut suffix_index = 0;
    while suffix_index < SUFFIX {
        fields[index + suffix_index] = suffix[suffix_index];
        suffix_index += 1;
    }
    fields
}

const BATON_PARAM_RW_MAP: CalibrationStorageMap = CalibrationStorageMap {
    id: BATON_PARAM_RW_NATIVE_LP64_LE_V1_MAP_ID,
    display_name: "Baton param_rw native LP64 LE v1",
    transport: EepromTransportSpec {
        i2c_address: 0x50,
        address_width_bits: 16,
        page_size_bytes: 32,
        write_cycle_ms: 5,
    },
    fields: &BATON_PARAM_RW_FIELDS,
};

/// 返回 Baton `param_rw` 原生结构体 EEPROM 映射。
#[must_use]
pub const fn baton_param_rw_native_lp64_le_v1() -> &'static CalibrationStorageMap {
    &BATON_PARAM_RW_MAP
}

/// PUEO-EDU DF9-40 当前 `eeprom_data` 原生 ABI 映射，来自用户提供的新版结构声明。
pub const PUEO_EDU_DF9_40_NATIVE_LP64_LE_V1_MAP_ID: &str = "pueo-edu-df9-40-native-lp64-le-v1";
/// AArch64/LP64、未 packed、`double` 为 IEEE754 little-endian 时的 `sizeof(eeprom_data)`。
pub const PUEO_EDU_DF9_40_IMAGE_BYTES: usize = 0x0388;

#[cfg(test)]
const PUEO_IMU_PARAM_OFFSET: u16 = 0x0260;
const PUEO_IMU_PARAM_CHECKSUM_OFFSET: u16 = 0x0290;
const PUEO_MD_SN_OFFSET: u16 = 0x0291;
#[cfg(test)]
const PUEO_RGB_CAMERA_OFFSET: u16 = 0x02a8;
const PUEO_RGB_CAMERA_CHECKSUM_OFFSET: u16 = 0x0380;

const PUEO_EDU_DF9_40_SUFFIX_FIELD_COUNT: usize = 41;
const PUEO_EDU_DF9_40_FIELD_COUNT: usize =
    BATON_PARAM_RW_SHARED_PREFIX_FIELD_COUNT + PUEO_EDU_DF9_40_SUFFIX_FIELD_COUNT;

const PUEO_EDU_DF9_40_SUFFIX_FIELDS: [StorageField; PUEO_EDU_DF9_40_SUFFIX_FIELD_COUNT] = [
    storage_field!("imu_param.acc_bias[0]", "IMU.AB0", 0x0260, F64Le, 8, true),
    storage_field!("imu_param.acc_bias[1]", "IMU.AB1", 0x0268, F64Le, 8, true),
    storage_field!("imu_param.acc_bias[2]", "IMU.AB2", 0x0270, F64Le, 8, true),
    storage_field!("imu_param.groy_bias[0]", "IMU.GB0", 0x0278, F64Le, 8, true),
    storage_field!("imu_param.groy_bias[1]", "IMU.GB1", 0x0280, F64Le, 8, true),
    storage_field!("imu_param.groy_bias[2]", "IMU.GB2", 0x0288, F64Le, 8, true),
    storage_field!(
        "imu_param_check_sum",
        "IMUCK",
        PUEO_IMU_PARAM_CHECKSUM_OFFSET,
        U8,
        1,
        true
    ),
    storage_field!(
        "md_sn",
        "SNID",
        PUEO_MD_SN_OFFSET,
        AsciiNulTerminated,
        21,
        true
    ),
    storage_field!("padding.after_md_sn", "PAD", 0x02a6, Reserved, 2, false),
    storage_field!("rgb_camera.rgb_cam.fx", "RGB.K.fx", 0x02a8, F64Le, 8, true),
    storage_field!("rgb_camera.rgb_cam.fy", "RGB.K.fy", 0x02b0, F64Le, 8, true),
    storage_field!("rgb_camera.rgb_cam.cx", "RGB.K.cx", 0x02b8, F64Le, 8, true),
    storage_field!("rgb_camera.rgb_cam.cy", "RGB.K.cy", 0x02c0, F64Le, 8, true),
    storage_field!("rgb_camera.rgb_cam.k1", "RGB.K.k1", 0x02c8, F64Le, 8, true),
    storage_field!("rgb_camera.rgb_cam.k2", "RGB.K.k2", 0x02d0, F64Le, 8, true),
    storage_field!("rgb_camera.rgb_cam.k3", "RGB.K.k3", 0x02d8, F64Le, 8, true),
    storage_field!("rgb_camera.rgb_cam.p1", "RGB.K.p1", 0x02e0, F64Le, 8, true),
    storage_field!("rgb_camera.rgb_cam.p2", "RGB.K.p2", 0x02e8, F64Le, 8, true),
    storage_field!(
        "rgb_camera.rgb_to_left_extrinsic.r00",
        "RGB.X.r00",
        0x02f0,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "rgb_camera.rgb_to_left_extrinsic.r01",
        "RGB.X.r01",
        0x02f8,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "rgb_camera.rgb_to_left_extrinsic.r02",
        "RGB.X.r02",
        0x0300,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "rgb_camera.rgb_to_left_extrinsic.t0",
        "RGB.X.t0",
        0x0308,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "rgb_camera.rgb_to_left_extrinsic.r10",
        "RGB.X.r10",
        0x0310,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "rgb_camera.rgb_to_left_extrinsic.r11",
        "RGB.X.r11",
        0x0318,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "rgb_camera.rgb_to_left_extrinsic.r12",
        "RGB.X.r12",
        0x0320,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "rgb_camera.rgb_to_left_extrinsic.t1",
        "RGB.X.t1",
        0x0328,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "rgb_camera.rgb_to_left_extrinsic.r20",
        "RGB.X.r20",
        0x0330,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "rgb_camera.rgb_to_left_extrinsic.r21",
        "RGB.X.r21",
        0x0338,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "rgb_camera.rgb_to_left_extrinsic.r22",
        "RGB.X.r22",
        0x0340,
        F64Le,
        8,
        true
    ),
    storage_field!(
        "rgb_camera.rgb_to_left_extrinsic.t2",
        "RGB.X.t2",
        0x0348,
        F64Le,
        8,
        true
    ),
    storage_field!("rgb_camera.width", "RGB.W", 0x0350, F64Le, 8, true),
    storage_field!("rgb_camera.height", "RGB.H", 0x0358, F64Le, 8, true),
    storage_field!("rgb_camera.fps", "RGB.FPS", 0x0360, F64Le, 8, true),
    storage_field!(
        "rgb_camera.exposure_time",
        "RGB.EXP",
        0x0368,
        F64Le,
        8,
        true
    ),
    storage_field!("rgb_camera.gain", "RGB.GAIN", 0x0370, F64Le, 8, true),
    storage_field!("rgb_camera.auto_exposure", "RGB.AE", 0x0378, U8, 1, true),
    storage_field!("rgb_camera.auto_gain", "RGB.AG", 0x0379, U8, 1, true),
    storage_field!(
        "rgb_camera.auto_white_balance",
        "RGB.AWB",
        0x037a,
        U8,
        1,
        true
    ),
    storage_field!("padding.after_rgb_auto", "PAD", 0x037b, Reserved, 5, false),
    storage_field!(
        "rgb_camera_check_sum",
        "RGBCK",
        PUEO_RGB_CAMERA_CHECKSUM_OFFSET,
        U8,
        1,
        true
    ),
    storage_field!("padding.tail", "PAD", 0x0381, Reserved, 7, false),
];

const PUEO_EDU_DF9_40_FIELDS: [StorageField; PUEO_EDU_DF9_40_FIELD_COUNT] = combine_storage_fields::<
    PUEO_EDU_DF9_40_FIELD_COUNT,
    BATON_PARAM_RW_SHARED_PREFIX_FIELD_COUNT,
    PUEO_EDU_DF9_40_SUFFIX_FIELD_COUNT,
>(
    BATON_PARAM_RW_SHARED_PREFIX_FIELDS,
    PUEO_EDU_DF9_40_SUFFIX_FIELDS,
);

const PUEO_EDU_DF9_40_MAP: CalibrationStorageMap = CalibrationStorageMap {
    id: PUEO_EDU_DF9_40_NATIVE_LP64_LE_V1_MAP_ID,
    display_name: "PUEO-EDU DF9-40 pinout",
    transport: EepromTransportSpec {
        i2c_address: 0x50,
        address_width_bits: 16,
        page_size_bytes: 32,
        write_cycle_ms: 5,
    },
    fields: &PUEO_EDU_DF9_40_FIELDS,
};

/// 返回 PUEO-EDU DF9-40 当前 `eeprom_data` 原生 ABI EEPROM 映射。
#[must_use]
pub const fn pueo_edu_df9_40_native_lp64_le_v1() -> &'static CalibrationStorageMap {
    &PUEO_EDU_DF9_40_MAP
}

/// 直接烧录或仅更新内参时 helper 的行为模式。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EepromProvisioningMode {
    FullProvision,
    UpdateCalibration,
}

/// helper 可执行的一段 EEPROM 写入载荷。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EepromWriteSegment {
    pub offset: u16,
    pub bytes: Vec<u8>,
}

/// 发送给远程 helper 的受控写入请求。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EepromProvisionRequest {
    pub map_id: String,
    pub mode: EepromProvisioningMode,
    /// FullProvision 时为写入的 SN；UpdateCalibration 时为必须匹配的既有 SN。
    pub serial_number: String,
    /// 只有预读到的非空 SN 与输入不同且用户明确确认后才可为 true。
    pub overwrite_existing_serial: bool,
    pub segments: Vec<EepromWriteSegment>,
}

impl EepromProvisionRequest {
    /// 校验 helper 请求只覆盖当前映射允许的精确字段集合。
    ///
    /// # Errors
    ///
    /// map、SN、模式、段顺序/范围或 checksum 不符合固定协议时返回错误。
    pub fn validate(&self) -> Result<(), EepromProvisionRequestError> {
        if self.map_id != YG_STEREO_P24C64G_V1_MAP_ID {
            return Err(EepromProvisionRequestError::UnsupportedMap(
                self.map_id.clone(),
            ));
        }
        let serial = validate_serial_number(&self.serial_number)
            .map_err(EepromProvisionRequestError::Serial)?;
        match self.mode {
            EepromProvisioningMode::FullProvision => {
                validate_segments(
                    &self.segments,
                    &[
                        (FLAG_OFFSET, YG_STEREO_P24C64G_FLAG.len()),
                        (WIDTH_OFFSET, YG_STEREO_P24C64G_INTRINSICS_BYTES),
                        (SERIAL_OFFSET, SERIAL_BYTES + 1),
                    ],
                )?;
                if self.segments[0].bytes != YG_STEREO_P24C64G_FLAG {
                    return Err(EepromProvisionRequestError::InvalidFlag);
                }
                if self.segments[2].bytes[..SERIAL_BYTES] != serial {
                    return Err(EepromProvisionRequestError::SerialPayloadMismatch);
                }
                if self.segments[2].bytes[SERIAL_BYTES] != serial_checksum(&serial) {
                    return Err(EepromProvisionRequestError::InvalidSerialChecksum);
                }
            }
            EepromProvisioningMode::UpdateCalibration => {
                if self.overwrite_existing_serial {
                    return Err(EepromProvisionRequestError::OverwriteNotAllowedForUpdate);
                }
                validate_segments(
                    &self.segments,
                    &[(WIDTH_OFFSET, YG_STEREO_P24C64G_INTRINSICS_BYTES)],
                )?;
            }
        }
        Ok(())
    }
}

fn validate_segments(
    segments: &[EepromWriteSegment],
    expected: &[(usize, usize)],
) -> Result<(), EepromProvisionRequestError> {
    if segments.len() != expected.len() {
        return Err(EepromProvisionRequestError::UnexpectedSegmentCount {
            expected: expected.len(),
            actual: segments.len(),
        });
    }
    for (index, (segment, (offset, byte_len))) in segments.iter().zip(expected).enumerate() {
        if usize::from(segment.offset) != *offset || segment.bytes.len() != *byte_len {
            return Err(EepromProvisionRequestError::UnexpectedSegment {
                index,
                expected_offset: *offset as u16,
                expected_len: *byte_len,
                actual_offset: segment.offset,
                actual_len: segment.bytes.len(),
            });
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum EepromProvisionRequestError {
    #[error("unsupported calibration EEPROM map: {0}")]
    UnsupportedMap(String),
    #[error(transparent)]
    Serial(#[from] EepromImageError),
    #[error("expected {expected} EEPROM write segments, got {actual}")]
    UnexpectedSegmentCount { expected: usize, actual: usize },
    #[error(
        "EEPROM segment {index} must be offset 0x{expected_offset:04x} length {expected_len}, got offset 0x{actual_offset:04x} length {actual_len}"
    )]
    UnexpectedSegment {
        index: usize,
        expected_offset: u16,
        expected_len: usize,
        actual_offset: u16,
        actual_len: usize,
    },
    #[error("FullProvision FLAG payload must be hessian\\0")]
    InvalidFlag,
    #[error("FullProvision SN payload differs from the request SN")]
    SerialPayloadMismatch,
    #[error("FullProvision SN checksum does not match the request SN")]
    InvalidSerialChecksum,
    #[error("UpdateCalibration cannot overwrite an existing SN")]
    OverwriteNotAllowedForUpdate,
}

/// YgStereo SNID 中允许写入的模组型号码。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum YgStereoModuleCode {
    Model233,
    Model235,
}

impl YgStereoModuleCode {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Model233 => "233",
            Self::Model235 => "235",
        }
    }

    const fn bytes(self) -> [u8; 3] {
        match self {
            Self::Model233 => *b"233",
            Self::Model235 => *b"235",
        }
    }
}

/// GUI 采集的 YgStereo SNID 语义字段；编码后正好覆盖 `0x0125..0x0132`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct YgStereoSerialIdInput {
    pub module: YgStereoModuleCode,
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub optical_axis_class: u8,
    pub sequence: u16,
}

impl YgStereoSerialIdInput {
    #[must_use]
    pub const fn new(
        module: YgStereoModuleCode,
        year: u16,
        month: u8,
        day: u8,
        optical_axis_class: u8,
        sequence: u16,
    ) -> Self {
        Self {
            module,
            year,
            month,
            day,
            optical_axis_class,
            sequence,
        }
    }

    /// 将语义字段编码为 14-byte ASCII SNID。
    ///
    /// Year 输入两位十进制年份并原样写入；序号按 1-based 十进制输入，写入
    /// `0-9a-zA-Z` base-62 高低位。
    pub fn serial_number(self) -> Result<String, YgStereoSerialIdError> {
        let bytes = self.serial_bytes()?;
        Ok(std::str::from_utf8(&bytes)
            .expect("YgStereo SNID encoder only writes ASCII bytes")
            .to_owned())
    }

    pub fn serial_bytes(self) -> Result<[u8; SERIAL_BYTES], YgStereoSerialIdError> {
        if self.year > 99 {
            return Err(YgStereoSerialIdError::YearOutOfRange { value: self.year });
        }
        let month = encode_month(self.month)?;
        let day = encode_day(self.day)?;
        let axis = encode_optical_axis_class(self.optical_axis_class)?;
        let [sequence_high, sequence_low] = encode_sequence(self.sequence)?;
        let year = self.year;
        let module = self.module.bytes();
        Ok([
            b'2',
            b'T',
            module[0],
            module[1],
            module[2],
            b'0' + (year / 10) as u8,
            b'0' + (year % 10) as u8,
            month,
            day,
            axis,
            sequence_high,
            sequence_low,
            b'0',
            b'0',
        ])
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum YgStereoSerialIdError {
    #[error("SNID year must be a two-digit decimal value in 0..=99, got {value}")]
    YearOutOfRange { value: u16 },
    #[error("SNID month must be in 1..=12, got {value}")]
    MonthOutOfRange { value: u8 },
    #[error("SNID day must be in 1..=31, got {value}")]
    DayOutOfRange { value: u8 },
    #[error("SNID optical-axis class must be in 0..=4, got {value}")]
    OpticalAxisClassOutOfRange { value: u8 },
    #[error("SNID sequence must be in 1..=3844, got {value}")]
    SequenceOutOfRange { value: u16 },
}

fn encode_month(value: u8) -> Result<u8, YgStereoSerialIdError> {
    match value {
        1..=9 => Ok(b'0' + value),
        10..=12 => Ok(b'A' + (value - 10)),
        _ => Err(YgStereoSerialIdError::MonthOutOfRange { value }),
    }
}

fn encode_day(value: u8) -> Result<u8, YgStereoSerialIdError> {
    match value {
        1..=9 => Ok(b'0' + value),
        10..=31 => Ok(b'A' + (value - 10)),
        _ => Err(YgStereoSerialIdError::DayOutOfRange { value }),
    }
}

fn encode_optical_axis_class(value: u8) -> Result<u8, YgStereoSerialIdError> {
    match value {
        0..=4 => Ok(b'0' + value),
        _ => Err(YgStereoSerialIdError::OpticalAxisClassOutOfRange { value }),
    }
}

fn encode_sequence(value: u16) -> Result<[u8; 2], YgStereoSerialIdError> {
    if !(1..=3844).contains(&value) {
        return Err(YgStereoSerialIdError::SequenceOutOfRange { value });
    }
    let zero_based = value - 1;
    Ok([base62_digit(zero_based / 62), base62_digit(zero_based % 62)])
}

fn base62_digit(value: u16) -> u8 {
    match value {
        0..=9 => b'0' + value as u8,
        10..=35 => b'a' + (value as u8 - 10),
        36..=61 => b'A' + (value as u8 - 36),
        _ => unreachable!("base62 digit must be in 0..62"),
    }
}

/// 与既有 `make_eeprom_bin.py` 相同布局的完整 EEPROM 镜像。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FullEepromImage {
    bytes: [u8; YG_STEREO_P24C64G_IMAGE_BYTES],
}

impl FullEepromImage {
    /// 从标定解与 14-byte ASCII SN 构造完整 EEPROM 镜像。
    ///
    /// 所有写入字段均使用小端编码；未定义的保留区保持零值。
    pub fn from_solution(
        solution: &CalibrationSolution,
        serial_number: &str,
    ) -> Result<Self, EepromImageError> {
        let serial = validate_serial_number(serial_number)?;
        let distortion = validated_distortion(&solution.distortion_coefficients)?;
        let mut bytes = [0_u8; YG_STEREO_P24C64G_IMAGE_BYTES];

        bytes[FLAG_OFFSET..FLAG_OFFSET + YG_STEREO_P24C64G_FLAG.len()]
            .copy_from_slice(&YG_STEREO_P24C64G_FLAG);
        write_u32(&mut bytes, WIDTH_OFFSET, solution.image_size.width);
        write_u32(&mut bytes, HEIGHT_OFFSET, solution.image_size.height);
        write_f32(&mut bytes, FX_OFFSET, solution.camera_matrix[0], "fx")?;
        write_f32(&mut bytes, FY_OFFSET, solution.camera_matrix[4], "fy")?;
        write_f32(&mut bytes, CX_OFFSET, solution.camera_matrix[2], "cx")?;
        write_f32(&mut bytes, CY_OFFSET, solution.camera_matrix[5], "cy")?;
        for (index, value) in distortion.iter().copied().enumerate() {
            write_f32(
                &mut bytes,
                DISTORTION_OFFSET + index * std::mem::size_of::<f32>(),
                value,
                DISTORTION_FIELD_NAMES[index],
            )?;
        }
        bytes[SERIAL_OFFSET..SERIAL_OFFSET + SERIAL_BYTES].copy_from_slice(&serial);
        bytes[SERIAL_CHECKSUM_OFFSET] = serial_checksum(&serial);

        Ok(Self { bytes })
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; YG_STEREO_P24C64G_IMAGE_BYTES] {
        &self.bytes
    }

    #[must_use]
    pub fn into_bytes(self) -> [u8; YG_STEREO_P24C64G_IMAGE_BYTES] {
        self.bytes
    }

    /// 返回镜像中已校验并编码的 SN；所有烧录请求必须以此作为唯一身份来源。
    #[must_use]
    pub fn serial_number(&self) -> &str {
        std::str::from_utf8(&self.bytes[SERIAL_OFFSET..SERIAL_OFFSET + SERIAL_BYTES])
            .expect("FullEepromImage serial bytes are validated ASCII")
    }

    /// 构造完整烧录请求。FLAG 段仅表示目标值，实际 helper 必须在最后提交。
    #[must_use]
    pub fn full_provision_request(
        &self,
        overwrite_existing_serial: bool,
    ) -> EepromProvisionRequest {
        EepromProvisionRequest {
            map_id: YG_STEREO_P24C64G_V1_MAP_ID.to_owned(),
            mode: EepromProvisioningMode::FullProvision,
            serial_number: self.serial_number().to_owned(),
            overwrite_existing_serial,
            segments: vec![
                segment(&self.bytes, FLAG_OFFSET, YG_STEREO_P24C64G_FLAG.len()),
                segment(
                    &self.bytes,
                    WIDTH_OFFSET,
                    YG_STEREO_P24C64G_INTRINSICS_BYTES,
                ),
                segment(&self.bytes, SERIAL_OFFSET, SERIAL_BYTES + 1),
            ],
        }
    }

    /// 构造只更新内参的请求。SN 只作设备身份校验，永远不在此模式写入。
    #[must_use]
    pub fn update_calibration_request(&self) -> EepromProvisionRequest {
        EepromProvisionRequest {
            map_id: YG_STEREO_P24C64G_V1_MAP_ID.to_owned(),
            mode: EepromProvisioningMode::UpdateCalibration,
            serial_number: self.serial_number().to_owned(),
            overwrite_existing_serial: false,
            segments: vec![segment(
                &self.bytes,
                WIDTH_OFFSET,
                YG_STEREO_P24C64G_INTRINSICS_BYTES,
            )],
        }
    }
}

const DISTORTION_FIELD_NAMES: [&str; 12] = [
    "k1", "k2", "p1", "p2", "k3", "k4", "k5", "k6", "s1", "s2", "s3", "s4",
];

fn segment(bytes: &[u8], offset: usize, byte_len: usize) -> EepromWriteSegment {
    EepromWriteSegment {
        offset: offset as u16,
        bytes: bytes[offset..offset + byte_len].to_vec(),
    }
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + std::mem::size_of::<u32>()].copy_from_slice(&value.to_le_bytes());
}

fn write_f32(
    bytes: &mut [u8],
    offset: usize,
    value: f64,
    field: &'static str,
) -> Result<(), EepromImageError> {
    if !value.is_finite() {
        return Err(EepromImageError::NonFiniteField { field });
    }
    if value.abs() > f64::from(f32::MAX) {
        return Err(EepromImageError::FloatOutOfRange { field, value });
    }
    bytes[offset..offset + std::mem::size_of::<f32>()]
        .copy_from_slice(&(value as f32).to_le_bytes());
    Ok(())
}

fn validated_distortion(values: &[f64]) -> Result<[f64; 12], EepromImageError> {
    if !matches!(values.len(), 8 | 12) {
        return Err(EepromImageError::UnexpectedDistortionCount {
            expected: DISTORTION_FIELD_NAMES.len(),
            actual: values.len(),
        });
    }
    // 与 make_eeprom_bin.py --yml 保持一致：只写 OpenCV 前 8 项，s1..s4 默认清零。
    let mut distortion = [0.0_f64; 12];
    distortion[..8].copy_from_slice(&values[..8]);
    Ok(distortion)
}

fn validate_serial_number(serial_number: &str) -> Result<[u8; SERIAL_BYTES], EepromImageError> {
    if !serial_number.is_ascii() {
        return Err(EepromImageError::NonAsciiSerialNumber);
    }
    let bytes = serial_number.as_bytes();
    if bytes.len() != SERIAL_BYTES {
        return Err(EepromImageError::InvalidSerialNumberLength {
            actual: bytes.len(),
        });
    }
    let mut serial = [0_u8; SERIAL_BYTES];
    serial.copy_from_slice(bytes);
    Ok(serial)
}

/// 与既有 EEPROM 解析器一致的 SN 校验和。
#[must_use]
pub fn serial_checksum(serial: &[u8; SERIAL_BYTES]) -> u8 {
    let sum = serial
        .iter()
        .fold(0_u16, |sum, byte| sum + u16::from(*byte));
    ((sum % 0xff) + 1) as u8
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum EepromImageError {
    #[error("EEPROM serial number must contain exactly 14 ASCII bytes, got {actual}")]
    InvalidSerialNumberLength { actual: usize },
    #[error("EEPROM serial number must contain ASCII bytes only")]
    NonAsciiSerialNumber,
    #[error("expected {expected} distortion coefficients, got {actual}")]
    UnexpectedDistortionCount { expected: usize, actual: usize },
    #[error("EEPROM field {field} is NaN or infinity")]
    NonFiniteField { field: &'static str },
    #[error("EEPROM field {field}={value} cannot be represented as f32")]
    FloatOutOfRange { field: &'static str, value: f64 },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calibration::{
        CalibrationImageSize, CalibrationSolution, PANGBOT_CALIBRATION_FLAGS,
    };

    fn solution() -> CalibrationSolution {
        CalibrationSolution {
            image_size: CalibrationImageSize::new(1920, 1080).unwrap(),
            camera_matrix: [1234.56, 0.0, 960.12, 0.0, 1234.78, 540.34, 0.0, 0.0, 1.0],
            distortion_coefficients: vec![
                0.1, -0.05, 0.001, -0.002, 0.003, -0.004, 0.005, -0.006, 0.007, -0.008, 0.009,
                -0.01,
            ],
            rms_error: 0.1,
            calibration_flags: PANGBOT_CALIBRATION_FLAGS,
            views: Vec::new(),
        }
    }

    #[test]
    fn yg_stereo_snid_encoder_converts_date_sequence_and_checksum() {
        let input = YgStereoSerialIdInput::new(YgStereoModuleCode::Model233, 26, 10, 31, 4, 3844);
        let serial = input.serial_number().unwrap();
        assert_eq!(serial, "2T23326AV4ZZ00");

        let bytes = input.serial_bytes().unwrap();
        assert_eq!(serial_checksum(&bytes), 0x69);
        let image = FullEepromImage::from_solution(&solution(), &serial).unwrap();
        assert_eq!(
            &image.as_bytes()[SERIAL_OFFSET..SERIAL_OFFSET + SERIAL_BYTES],
            &bytes
        );
        assert_eq!(image.as_bytes()[SERIAL_CHECKSUM_OFFSET], 0x69);
    }

    #[test]
    fn yg_stereo_snid_encoder_supports_model_235_and_first_sequence() {
        let input = YgStereoSerialIdInput::new(YgStereoModuleCode::Model235, 26, 1, 9, 0, 1);
        assert_eq!(input.serial_number().unwrap(), "2T235261900000");
    }

    #[test]
    fn yg_stereo_snid_encoder_rejects_out_of_range_fields() {
        assert_eq!(
            YgStereoSerialIdInput::new(YgStereoModuleCode::Model233, 100, 1, 1, 0, 1)
                .serial_number()
                .unwrap_err(),
            YgStereoSerialIdError::YearOutOfRange { value: 100 }
        );
        assert_eq!(
            YgStereoSerialIdInput::new(YgStereoModuleCode::Model233, 26, 13, 1, 0, 1)
                .serial_number()
                .unwrap_err(),
            YgStereoSerialIdError::MonthOutOfRange { value: 13 }
        );
        assert_eq!(
            YgStereoSerialIdInput::new(YgStereoModuleCode::Model233, 26, 1, 32, 0, 1)
                .serial_number()
                .unwrap_err(),
            YgStereoSerialIdError::DayOutOfRange { value: 32 }
        );
        assert_eq!(
            YgStereoSerialIdInput::new(YgStereoModuleCode::Model233, 26, 1, 1, 5, 1)
                .serial_number()
                .unwrap_err(),
            YgStereoSerialIdError::OpticalAxisClassOutOfRange { value: 5 }
        );
        assert_eq!(
            YgStereoSerialIdInput::new(YgStereoModuleCode::Model233, 26, 1, 1, 0, 3845)
                .serial_number()
                .unwrap_err(),
            YgStereoSerialIdError::SequenceOutOfRange { value: 3845 }
        );
    }

    #[test]
    fn full_image_matches_make_eeprom_bin_default_byte_for_byte() {
        let image = FullEepromImage::from_solution(&solution(), "2T02D2567K0042").unwrap();
        let golden = include_bytes!("fixtures/yg_stereo_p24c64g_script_default.bin");
        assert_eq!(golden.len(), YG_STEREO_P24C64G_IMAGE_BYTES);
        assert_eq!(image.as_bytes(), &golden[..]);
        let thin_prism_offset = DISTORTION_OFFSET + 8 * std::mem::size_of::<f32>();
        assert!(
            image.as_bytes()[thin_prism_offset..thin_prism_offset + 4 * 4]
                .iter()
                .all(|byte| *byte == 0)
        );
    }

    #[test]
    fn full_and_update_requests_write_the_intended_segments() {
        let image = FullEepromImage::from_solution(&solution(), "2T02D2567K0042").unwrap();
        assert_eq!(image.serial_number(), "2T02D2567K0042");

        let full = image.full_provision_request(false);
        assert_eq!(full.mode, EepromProvisioningMode::FullProvision);
        assert_eq!(full.serial_number, image.serial_number());
        assert_eq!(
            full.segments
                .iter()
                .map(|segment| segment.offset)
                .collect::<Vec<_>>(),
            [0, 0x10, 0x125]
        );
        assert_eq!(
            full.segments
                .iter()
                .map(|segment| segment.bytes.len())
                .collect::<Vec<_>>(),
            [8, 72, 15]
        );

        let update = image.update_calibration_request();
        assert_eq!(update.mode, EepromProvisioningMode::UpdateCalibration);
        assert_eq!(update.serial_number, image.serial_number());
        assert_eq!(update.segments.len(), 1);
        assert_eq!(update.segments[0].offset, WIDTH_OFFSET as u16);
        assert_eq!(
            update.segments[0].bytes.len(),
            YG_STEREO_P24C64G_INTRINSICS_BYTES
        );
    }

    #[test]
    fn rejects_nonportable_serial_and_incomplete_distortion() {
        assert!(matches!(
            FullEepromImage::from_solution(&solution(), "not-ascii-序号"),
            Err(EepromImageError::NonAsciiSerialNumber)
        ));

        let mut invalid = solution();
        invalid.distortion_coefficients.pop();
        assert!(matches!(
            FullEepromImage::from_solution(&invalid, "2T02D2567K0042"),
            Err(EepromImageError::UnexpectedDistortionCount { .. })
        ));
    }

    #[test]
    fn baton_param_rw_map_matches_native_layout_contract() {
        let map = baton_param_rw_native_lp64_le_v1();
        assert_eq!(map.id, BATON_PARAM_RW_NATIVE_LP64_LE_V1_MAP_ID);
        assert_eq!(BATON_PARAM_RW_IMAGE_BYTES, 1008);
        assert_eq!(map.transport.i2c_address, 0x50);
        assert_eq!(map.transport.address_width_bits, 16);
        assert_eq!(map.transport.page_size_bytes, 32);
        assert_eq!(map.transport.write_cycle_ms, 5);

        let field = |name: &str| {
            map.fields
                .iter()
                .find(|field| field.name == name)
                .unwrap_or_else(|| panic!("missing Baton field {name}"))
        };
        assert_eq!(field("cam_rgb_param.width").offset, 0x03d8);
        assert_eq!(field("cam_rgb_param.width").byte_len, 8);
        assert_eq!(
            field("cam_rgb_param.width").encoding,
            StorageEncoding::F64Le
        );
        assert_eq!(field("cam_rgb_param.height").offset, 0x03e0);
        assert_eq!(field("md_sn").offset, 0x0319);
        assert_eq!(field("md_sn").byte_len, 21);
        assert_eq!(field("md_sn").encoding, StorageEncoding::AsciiNulTerminated);
        assert_eq!(
            field("cam_rgb_param.rgb_to_left_extrinsic.r00").offset,
            0x0378
        );
        assert_eq!(
            field("cam_rgb_param.rgb_to_left_extrinsic.t2").offset,
            0x03d0
        );
        assert_eq!(
            field("padding.tail").offset + field("padding.tail").byte_len,
            1008
        );
    }

    #[test]
    fn pueo_edu_df9_40_map_matches_repr_c_layout_contract() {
        use std::mem::{align_of, offset_of, size_of};

        #[repr(C)]
        struct CamIntrinsicParam {
            fx: f64,
            fy: f64,
            cx: f64,
            cy: f64,
            k1: f64,
            k2: f64,
            k3: f64,
            p1: f64,
            p2: f64,
        }

        #[repr(C)]
        struct ExtrinsicMatrix {
            r00: f64,
            r01: f64,
            r02: f64,
            t0: f64,
            r10: f64,
            r11: f64,
            r12: f64,
            t1: f64,
            r20: f64,
            r21: f64,
            r22: f64,
            t2: f64,
        }

        #[repr(C)]
        struct CamParam {
            left_cam: CamIntrinsicParam,
            right_cam: CamIntrinsicParam,
            extrinsic: ExtrinsicMatrix,
        }

        #[repr(C)]
        struct RgbCameraParam {
            rgb_cam: CamIntrinsicParam,
            rgb_to_left_extrinsic: ExtrinsicMatrix,
            width: f64,
            height: f64,
            fps: f64,
            exposure_time: f64,
            gain: f64,
            auto_exposure: u8,
            auto_gain: u8,
            auto_white_balance: u8,
        }

        #[repr(C)]
        struct FishEyeIntrinsicParam {
            fx: f64,
            fy: f64,
            cx: f64,
            cy: f64,
            xi: f64,
            alpha: f64,
        }

        #[repr(C)]
        struct ExtrinsicQuaternion {
            px: f64,
            py: f64,
            pz: f64,
            qx: f64,
            qy: f64,
            qz: f64,
            qw: f64,
        }

        #[repr(C)]
        struct FishEyeParam {
            left_cam: FishEyeIntrinsicParam,
            right_cam: FishEyeIntrinsicParam,
            extrinsic: ExtrinsicQuaternion,
        }

        #[repr(C)]
        struct CamImuExtrinsicParam {
            left_cam_imu: ExtrinsicMatrix,
            right_cam_imu: ExtrinsicMatrix,
        }

        #[repr(C)]
        struct ImuParam {
            acc_bias: [f64; 3],
            groy_bias: [f64; 3],
        }

        #[repr(C)]
        struct EepromData {
            fish_param: FishEyeParam,
            fish_param_check_sum: u8,
            cam_param: CamParam,
            cam_param_check_sum: u8,
            cam_imu_extrinsic: CamImuExtrinsicParam,
            cam_imu_extrinsic_check_sum: u8,
            imu_param: ImuParam,
            imu_param_check_sum: u8,
            md_sn: [u8; 21],
            rgb_camera: RgbCameraParam,
            rgb_camera_check_sum: u8,
        }

        assert_eq!(size_of::<EepromData>(), PUEO_EDU_DF9_40_IMAGE_BYTES);
        assert_eq!(align_of::<EepromData>(), 8);
        assert_eq!(size_of::<f64>(), 8);
        assert_eq!(
            offset_of!(EepromData, imu_param),
            usize::from(PUEO_IMU_PARAM_OFFSET)
        );
        assert_eq!(
            offset_of!(EepromData, imu_param_check_sum),
            usize::from(PUEO_IMU_PARAM_CHECKSUM_OFFSET)
        );
        assert_eq!(
            offset_of!(EepromData, md_sn),
            usize::from(PUEO_MD_SN_OFFSET)
        );
        assert_eq!(
            offset_of!(EepromData, rgb_camera),
            usize::from(PUEO_RGB_CAMERA_OFFSET)
        );
        assert_eq!(
            offset_of!(EepromData, rgb_camera_check_sum),
            usize::from(PUEO_RGB_CAMERA_CHECKSUM_OFFSET)
        );

        let map = pueo_edu_df9_40_native_lp64_le_v1();
        assert_eq!(map.id, PUEO_EDU_DF9_40_NATIVE_LP64_LE_V1_MAP_ID);
        assert_eq!(map.transport.i2c_address, 0x50);
        assert_eq!(map.transport.address_width_bits, 16);
        assert_eq!(map.transport.page_size_bytes, 32);

        let prefix_tail = BATON_PARAM_RW_SHARED_PREFIX_FIELDS
            .last()
            .expect("shared prefix is non-empty");
        assert_eq!(BATON_PARAM_RW_SHARED_PREFIX_FIELDS.len(), 79);
        assert_eq!(prefix_tail.name, "padding.after_cam_imu_checksum");
        assert_eq!(
            prefix_tail.offset + prefix_tail.byte_len,
            PUEO_IMU_PARAM_OFFSET
        );
        assert_eq!(
            PUEO_EDU_DF9_40_FIELDS[BATON_PARAM_RW_SHARED_PREFIX_FIELD_COUNT].name,
            "imu_param.acc_bias[0]"
        );
        assert_eq!(
            BATON_PARAM_RW_FIELDS[BATON_PARAM_RW_SHARED_PREFIX_FIELD_COUNT].name,
            "imu_instrinsic.gyr_n"
        );

        let field = |name: &str| {
            map.fields
                .iter()
                .find(|field| field.name == name)
                .unwrap_or_else(|| panic!("missing PUEO field {name}"))
        };
        let imu_base = offset_of!(EepromData, imu_param);
        assert_eq!(
            usize::from(field("imu_param.acc_bias[0]").offset),
            imu_base + offset_of!(ImuParam, acc_bias)
        );
        assert_eq!(
            usize::from(field("imu_param.groy_bias[0]").offset),
            imu_base + offset_of!(ImuParam, groy_bias)
        );
        assert_eq!(
            field("imu_param.acc_bias[0]").encoding,
            StorageEncoding::F64Le
        );

        let rgb_base = offset_of!(EepromData, rgb_camera);
        assert_eq!(
            usize::from(field("rgb_camera.fps").offset),
            rgb_base + offset_of!(RgbCameraParam, fps)
        );
        assert_eq!(
            usize::from(field("rgb_camera.exposure_time").offset),
            rgb_base + offset_of!(RgbCameraParam, exposure_time)
        );
        assert_eq!(
            usize::from(field("rgb_camera.gain").offset),
            rgb_base + offset_of!(RgbCameraParam, gain)
        );
        assert_eq!(
            usize::from(field("rgb_camera.auto_exposure").offset),
            rgb_base + offset_of!(RgbCameraParam, auto_exposure)
        );
        assert_eq!(
            field("rgb_camera.auto_exposure").encoding,
            StorageEncoding::U8
        );
        assert!(
            !map.fields
                .iter()
                .any(|field| field.name.starts_with("imu_instrinsic"))
        );
    }
}
