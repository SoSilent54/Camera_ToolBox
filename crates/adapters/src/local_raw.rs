//! 本地 RAW 文件加载 adapter。
//!
//! 当前只做无设备副作用的本地文件读取；sensor 采集、SSH/SFTP、寄存器访问不在本模块范围内。

use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

use camera_toolbox_app::{RawFrameLoadError, RawFrameLoader};
use camera_toolbox_core::{RawEncoding, RawFrame, RawProbeInput, RawSpec};

#[derive(Debug, Default, Clone, Copy)]
pub struct LocalRawLoader;

const PROBE_WINDOW_BYTES: u64 = 64 * 1024;

impl RawFrameLoader for LocalRawLoader {
    fn inspect_raw(&self, path: &Path) -> Result<RawProbeInput, RawFrameLoadError> {
        let mut file = File::open(path)?;
        let file_len = file.metadata()?.len();
        let window_len = file_len.min(PROBE_WINDOW_BYTES);
        let mut offsets = vec![0];
        if file_len > window_len {
            offsets.push(((file_len - window_len) / 2) & !1);
            offsets.push((file_len - window_len) & !1);
        }
        offsets.sort_unstable();
        offsets.dedup();

        let mut samples = Vec::with_capacity(offsets.len());
        for offset in offsets {
            let readable = (file_len - offset).min(window_len) & !1;
            let mut sample = vec![0; usize::try_from(readable).expect("probe window fits usize")];
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(&mut sample)?;
            samples.push(sample);
        }

        Ok(RawProbeInput {
            file_name: path
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned),
            file_len,
            samples,
        })
    }

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
                    bayer: BayerPattern::Rggb,
                },
                RawEncoding::U16Le,
            )
            .unwrap();

        assert_eq!(frame.pixels(), &[0, 1, 1023, 512]);

        let inspection = loader.inspect_raw(&path).unwrap();
        assert_eq!(inspection.file_len, 8);
        assert_eq!(
            inspection.samples,
            vec![vec![0, 0, 1, 0, 0xff, 0x03, 0, 0x02]]
        );
        let _ = std::fs::remove_file(path);
    }
}
