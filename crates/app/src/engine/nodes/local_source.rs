//! 本地文件源节点：从本地目录加载单张图片帧（按钮节点范式）。
//!
//! `on_action(Trigger)` 时按 `root` + `directory` + `selection` 拼接本地路径，读取文件字节后
//! 按扩展名分派：
//! - `.png` → `RasterImageCodec::decode_rgba8(Png)`
//! - `.jpg`/`.jpeg` → `RasterImageCodec::decode_rgba8(Jpeg)`
//! - `.raw` 或其他 → 上报「RAW/后续」事件，不 demosaic、不 panic。
//!
//! 解码出的 `Rgba8Frame` 转为 `DecodedVideoFrame` 后经 `image`（及可选 `preview`）端口输出
//! `ImageFrame`；`fileRef` 可选输出携带解析后的绝对路径（`Json`）。
//!
//! 未注入 `image_codec`、路径为空或读取失败时按前置/执行条件失败，不 panic。

use std::path::PathBuf;
use std::sync::Arc;

use crate::{
    engine::{
        DataPacket, NodeAction, NodeError, NodeFactory, NodeInstance, NodeRuntime,
        NodeRuntimeState, NodeSpec, PortSpec,
    },
    platform::{DecodedVideoFrame, StreamFrameIdentity, StreamSessionId},
    ports::{RasterFormat, RasterImageCodec},
};

/// 静态图片单帧解码字节预算；超过即拒绝，避免解码器分配失控。
const DECODED_IMAGE_BYTE_LIMIT: usize = 512 * 1024 * 1024;

pub struct LocalFileSourceFactory;

impl NodeFactory for LocalFileSourceFactory {
    fn kind(&self) -> &'static str {
        "localFileSource"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(LocalFileSourceNode { spec }))
    }
}

pub struct LocalFileSourceNode {
    spec: NodeSpec,
}

impl NodeInstance for LocalFileSourceNode {
    fn kind(&self) -> &'static str {
        "localFileSource"
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Ready, "trigger to load file");
        Ok(())
    }

    fn on_input(
        &mut self,
        _port: &str,
        _packet: DataPacket,
        _rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        // 本地文件源无输入端口，忽略任何外来数据。
        Ok(())
    }

    fn on_action(&mut self, action: NodeAction, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        match action {
            NodeAction::Trigger => self.load(rt),
            other => Err(NodeError::UnsupportedAction(other.name().to_owned())),
        }
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}

impl LocalFileSourceNode {
    fn load(&self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        let path = self.resolve_path()?;
        let bytes = std::fs::read(&path)
            .map_err(|error| NodeError::Execution(format!("read {} failed: {error}", path.display())))?;

        // 按扩展名分派；非 PNG/JPEG（含 .raw）暂不 demosaic，诚实上报。
        let format = match extension(&path).as_deref() {
            Some("png") => RasterFormat::Png,
            Some("jpg" | "jpeg") => RasterFormat::Jpeg,
            _ => {
                rt.report_event(format!(
                    "unsupported file `{}`; RAW demosaic is not yet implemented",
                    path.display()
                ));
                rt.report_state(NodeRuntimeState::Idle, "unsupported format");
                return Ok(());
            }
        };

        let image_codec: Arc<dyn RasterImageCodec> = rt.services().image_codec()?;
        rt.report_state(NodeRuntimeState::Running, "loading image");
        let rgba = image_codec
            .decode_rgba8(format, &bytes, DECODED_IMAGE_BYTE_LIMIT)
            .map_err(|error| NodeError::Execution(error.to_string()))?;

        // Rgba8Frame 可能带 stride；DecodedVideoFrame 要求紧密排列 RGBA，统一复制紧凑像素。
        let compact = compact_rgba(&rgba)?;
        let frame = Arc::new(DecodedVideoFrame {
            width: rgba.width,
            height: rgba.height,
            rgba: compact,
            identity: StreamFrameIdentity::unavailable(
                StreamSessionId::new(format!("local-{}", self.spec.id))
                    .map_err(|_| NodeError::Execution("invalid local stream session id".to_owned()))?,
                0,
                0,
                "local-file-source".to_owned(),
            ),
        });

        rt.emit("image", DataPacket::ImageFrame(Arc::clone(&frame)))?;
        if has_output_port(&self.spec, "preview") {
            rt.emit("preview", DataPacket::ImageFrame(Arc::clone(&frame)))?;
        }
        if has_output_port(&self.spec, "fileRef") {
            rt.emit(
                "fileRef",
                DataPacket::Json(Arc::new(serde_json::json!({
                    "kind": "file.ref",
                    "path": path.display().to_string(),
                }))),
            )?;
        }
        rt.report_state(NodeRuntimeState::Idle, "loaded");
        Ok(())
    }

    /// 拼接本地路径：`root` + `directory` + `selection`（均为 source-relative，空段跳过）。
    fn resolve_path(&self) -> Result<PathBuf, NodeError> {
        let root = config_string(&self.spec, "root");
        let directory = config_string(&self.spec, "directory");
        let selection = config_string(&self.spec, "selection");

        let mut path = PathBuf::new();
        for segment in [root, directory, selection] {
            let trimmed = segment.trim();
            if trimmed.is_empty() {
                continue;
            }
            path.push(trimmed);
        }
        if path.as_os_str().is_empty() {
            return Err(NodeError::Config(
                "root/directory/selection must select a non-empty local path".to_owned(),
            ));
        }
        Ok(path)
    }
}

/// 把 `Rgba8Frame`（可能带 stride）复制为紧密排列的 RGBA 字节。
fn compact_rgba(frame: &camera_toolbox_core::Rgba8Frame) -> Result<Arc<[u8]>, NodeError> {
    let row_bytes = usize::try_from(frame.width)
        .ok()
        .and_then(|w| w.checked_mul(4))
        .ok_or_else(|| NodeError::Execution("image width overflow".to_owned()))?;
    let total = row_bytes
        .checked_mul(frame.height as usize)
        .ok_or_else(|| NodeError::Execution("image size overflow".to_owned()))?;
    let mut compact = Vec::with_capacity(total);
    let pixels = frame.pixels();
    for row in 0..frame.height as usize {
        let start = row * frame.stride;
        let end = start + row_bytes;
        let Some(row_slice) = pixels.get(start..end) else {
            return Err(NodeError::Execution("image stride/layout inconsistent".to_owned()));
        };
        compact.extend_from_slice(row_slice);
    }
    Ok(compact.into())
}

fn extension(path: &std::path::Path) -> Option<String> {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
}

fn has_output_port(spec: &NodeSpec, id: &str) -> bool {
    spec.outputs.iter().any(|port: &PortSpec| port.id == id)
}

fn config_string<'a>(spec: &'a NodeSpec, key: &str) -> &'a str {
    spec.config
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> NodeSpec {
        NodeSpec {
            id: "local-1".to_owned(),
            kind: "localFileSource".to_owned(),
            title: "Local".to_owned(),
            inputs: vec![],
            outputs: vec![
                PortSpec {
                    id: "image".to_owned(),
                    label: "Image".to_owned(),
                    kind: "image.frame.v1".to_owned(),
                    cardinality: crate::engine::PortCardinality::One,
                    required: false,
                },
                PortSpec {
                    id: "preview".to_owned(),
                    label: "Preview".to_owned(),
                    kind: "image.frame.v1".to_owned(),
                    cardinality: crate::engine::PortCardinality::One,
                    required: false,
                },
            ],
            config: serde_json::json!({"root": "", "directory": "", "selection": "", "filter": "*.png", "reload": "manual"}),
        }
    }

    #[test]
    fn factory_instantiates_with_expected_kind() {
        assert_eq!(LocalFileSourceFactory.kind(), "localFileSource");
        let instance = LocalFileSourceFactory
            .instantiate(spec())
            .expect("instantiate");
        assert_eq!(instance.kind(), "localFileSource");
    }

    #[test]
    fn empty_path_is_config_error() {
        let node = LocalFileSourceNode { spec: spec() };
        let err = node
            .resolve_path()
            .expect_err("empty root/directory/selection must be rejected");
        assert!(matches!(err, NodeError::Config(_)), "got {err:?}");
    }

    #[test]
    fn path_joins_root_directory_selection() {
        let mut s = spec();
        s.config = serde_json::json!({
            "root": "/srv/images",
            "directory": "calib",
            "selection": "board.png",
            "filter": "*.png",
            "reload": "manual",
        });
        let node = LocalFileSourceNode { spec: s };
        let path = node.resolve_path().expect("path resolves");
        assert_eq!(path, std::path::PathBuf::from("/srv/images/calib/board.png"));
    }

    #[test]
    fn extension_dispatch_is_correct() {
        // 验证扩展名分派：png/jpg/jpeg 可解码，raw 走「待后续」路径。
        let ext = |p: &str| extension(std::path::Path::new(p));
        assert_eq!(ext("x.png").as_deref(), Some("png"));
        assert_eq!(ext("x.PNG").as_deref(), Some("png"));
        assert_eq!(ext("x.jpg").as_deref(), Some("jpg"));
        assert_eq!(ext("x.jpeg").as_deref(), Some("jpeg"));
        assert_eq!(ext("x.raw").as_deref(), Some("raw"));
        assert_eq!(ext("noext"), None);
    }
}
