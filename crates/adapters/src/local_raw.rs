//! 本地 RAW 文件加载 adapter。
//!
//! 当前只做无设备副作用的本地文件读取；sensor 采集、SSH/SFTP、寄存器访问不在本模块范围内。

use std::path::Path;

use camera_toolbox_app::{RawFrameLoadError, RawFrameLoader};
use camera_toolbox_core::{RawEncoding, RawFrame, RawSpec};

#[derive(Debug, Default, Clone, Copy)]
pub struct LocalRawLoader;

impl RawFrameLoader for LocalRawLoader {
    fn load_raw_frame(
        &self,
        path: &Path,
        spec: RawSpec,
        encoding: RawEncoding,
    ) -> Result<RawFrame, RawFrameLoadError> {
        let bytes = std::fs::read(path)?;
        RawFrame::from_bytes(spec, encoding, &bytes).map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::Write,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;
    use camera_toolbox_core::BayerPattern;

    #[test]
    fn loads_local_unpacked_u16le_raw() {
        let dir = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = dir.join(format!(
            "camera-toolbox-local-raw-{}-{nanos}.raw",
            std::process::id()
        ));
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(&[0, 0, 1, 0, 0xff, 0x03, 0, 0x02]).unwrap();

        let loader = LocalRawLoader;
        let frame = loader
            .load_raw_frame(
                &path,
                RawSpec {
                    width: 2,
                    height: 2,
                    bit_depth: 10,
                    stride_pixels: 2,
                    bayer: BayerPattern::Rggb,
                },
                RawEncoding::U16Le,
            )
            .unwrap();
        let _ = std::fs::remove_file(path);

        assert_eq!(frame.pixels(), &[0, 1, 1023, 512]);
    }
}
