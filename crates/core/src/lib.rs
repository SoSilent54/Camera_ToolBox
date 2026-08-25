//! Camera Toolbox 核心领域类型。
//!
//! 本 crate 只包含纯数据结构和纯计算逻辑，不触碰 SSH、进程、GUI 或寄存器 IO。

pub mod analysis;
pub mod asset;
pub mod calibration;
pub mod calibration_eeprom;
pub mod calibration_yaml;
pub mod color;
pub mod eeprom_map_config;
pub mod i2c_map;
pub mod image;
pub mod image_analysis;
pub mod raw;
pub mod raw_packing;
pub mod raw_probe;
pub mod sensor;
pub mod structured_packet;
pub mod yuv;

pub use analysis::{
    AnalysisError, HistogramSeries, RawBayerHistogram, RawRoiAnalysis, Roi, RoiStats,
    analyze_raw_roi, analyze_raw_roi_with_cancel, analyze_roi, analyze_roi_with_cancel,
};
pub use asset::{
    AssetError, AssetId, CaptureMetadata, ChromaOrder, EphemeralAsset, IntegrityState, MediaFormat,
    OwnedMediaPayload, OwnedMediaPlane,
};
pub use calibration::{
    BoardSpec, CalibrationDataError, CalibrationImageSize, CalibrationPoint, CalibrationRequest,
    CalibrationSolution, ChessboardDetection, ChessboardDetectionOutcome, InitialIntrinsics,
    PANGBOT_CALIBRATION_FLAGS, ViewCalibrationResult,
};
pub use calibration_eeprom::{
    BATON_PARAM_RW_IMAGE_BYTES, BATON_PARAM_RW_NATIVE_LP64_LE_V1_MAP_ID, CalibrationStorageMap,
    EepromImageError, EepromProvisionRequest, EepromProvisionRequestError, EepromProvisioningMode,
    EepromTransportSpec, EepromWriteSegment, FullEepromImage, PUEO_EDU_DF9_40_IMAGE_BYTES,
    PUEO_EDU_DF9_40_NATIVE_LP64_LE_V1_MAP_ID, StorageEncoding, StorageField,
    YG_STEREO_P24C64G_FLAG, YG_STEREO_P24C64G_IMAGE_BYTES, YG_STEREO_P24C64G_INTRINSICS_BYTES,
    YG_STEREO_P24C64G_V1_MAP_ID, YgStereoModuleCode, YgStereoSerialIdError, YgStereoSerialIdInput,
    baton_param_rw_native_lp64_le_v1, pueo_edu_df9_40_native_lp64_le_v1, serial_checksum,
    yg_stereo_p24c64g_v1,
};
pub use calibration_yaml::{
    CalibrationYamlError, encode_opencv_pinhole_radtan_yaml, parse_opencv_pinhole_radtan_yaml,
    write_opencv_pinhole_radtan_yaml,
};
pub use color::{
    BayerPrepareDiagnostics, CfaQuad, CfaSite, ColorPipelineParams, ColorPixel,
    ColorRenderDiagnostics, ColorRenderError, DEFAULT_DISPLAY_GAMMA, DisplayTransform, LinearRgb,
    PreparedBayer, Rgb8, render_pixel_at,
};
pub use eeprom_map_config::{
    BuiltInEepromMapConfig, CompiledEepromMapConfig, CompiledEepromMapField, EepromMapConfigError,
    IMX219_EEPROM_CALIBRATION_CONFIG_NAME, PUEO_EDU_DF9_40_PINOUT_CONFIG_NAME,
    builtin_eeprom_map_config_text, canonical_text_from_storage_map,
    compile_builtin_eeprom_map_config, compile_eeprom_map_config_text,
    dump_builtin_eeprom_map_config, list_builtin_eeprom_map_configs,
};
pub use i2c_map::{
    ChecksumAlgorithm, ChecksumContract, I2C_MAP_SCHEMA, I2cMapAccepts, I2cMapDefinition,
    I2cMapDefinitionError, I2cMapFixedBytes, I2cMapImage, I2cMapPage, I2cMapStorageField,
    I2cMapTarget, I2cReadRange, LogicalInputSlot, NumericConversion, PageWritePolicy,
    ReadBeforePolicy, ReadbackPolicy, ReadbackVerification, RoundingMode, StorageTarget,
    builtin_i2c_map, builtin_i2c_maps, parse_i2c_map_yaml, yg_stereo_i2c_map,
};
pub use image::{
    ImageFrameError, ImagePlane, NativeImage, NativePixelSample, Rgba8Frame, Yuv420SpFrame,
    Yuv420SpSpec, YuvMatrix, YuvRange,
};
pub use image_analysis::{
    ChannelAnalysis8, RgbRoiAnalysis, YuvRoiAnalysis, analyze_rgba8_with_cancel,
    analyze_yuv420sp_with_cancel,
};
pub use raw::{BayerPattern, RawEncoding, RawFrame, RawFrameError, RawSpec};
pub use raw_packing::{PackedRawSpec, RawPackingError, decode_le_continuous_raw};
pub use raw_probe::{
    RawContainer, RawEndian, RawProbeCandidate, RawProbeInput, RawProbeReport, probe_raw_candidates,
};
pub use structured_packet::{
    Datum, PacketProvenance, PrimitiveType, StructuredPacket, StructuredPacketError, TypedValue,
};
pub use yuv::{
    ImageConversionError, rgb_to_yuv, rgba8_to_yuv420sp_with_cancel, yuv_to_rgb,
    yuv420sp_to_rgba8_with_cancel,
};
