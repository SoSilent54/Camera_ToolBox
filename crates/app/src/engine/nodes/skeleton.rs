//! SSH/X5/I²C/EEPROM 节点骨架：只保证「能注册 + instantiate 成功 + 状态上报」。
//!
//! 这 5 个 factory（`sftpFileSource` / `sshSession` / `x5Device` / `i2cTransfer` /
//! `eepromProvision`）仅占位执行体，用于让 `register_builtin` 覆盖全部 19 个 `NodeKind`，
//! 消除 `GraphEngine::build` 的 `UnknownKind`。真实 SSH/SFTP/X5 TCP/I²C helper/EEPROM
//! provisioning 能力依赖复杂 service 注入（credential resolver + transport factory +
//! `helper_payload`）与真实设备，留待 M3 落地；骨架期诚实标注，不伪造外部能力。
//!
//! - `on_start`   → 上报 `Ready`，diagnostic 标明 skeleton。
//! - `on_input`   → 上报事件「not implemented」，不处理数据。
//! - `on_action`  → 一律 `UnsupportedAction`（骨架期不接受触发），不 panic。
//! - `on_stop`    → 上报 `Idle`。

use crate::engine::{
    DataPacket, NodeAction, NodeError, NodeFactory, NodeInstance, NodeRuntime, NodeRuntimeState,
    NodeSpec,
};

/// 骨架 NodeInstance：一个结构体模板，按 kind 复用（kind + 标签由 factory 绑定）。
struct SkeletonNode {
    kind: &'static str,
    skeleton_note: &'static str,
}

impl NodeInstance for SkeletonNode {
    fn kind(&self) -> &'static str {
        self.kind
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Ready, self.skeleton_note);
        Ok(())
    }

    fn on_input(
        &mut self,
        port: &str,
        _packet: DataPacket,
        rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        rt.report_event(format!("`{port}` not implemented (skeleton; real capability M3)"));
        Ok(())
    }

    fn on_action(&mut self, action: NodeAction, _rt: &mut NodeRuntime) -> Result<(), NodeError> {
        Err(NodeError::UnsupportedAction(action.name().to_owned()))
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}

/// 声明一个骨架 factory（kind + `on_start` 的骨架标注）。
macro_rules! skeleton_factory {
    ($factory:ident, $kind:literal, $note:literal) => {
        pub struct $factory;

        impl NodeFactory for $factory {
            fn kind(&self) -> &'static str {
                $kind
            }

            fn instantiate(&self, _spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
                Ok(Box::new(SkeletonNode {
                    kind: $kind,
                    skeleton_note: $note,
                }))
            }
        }
    };
}

// sftpFileSource：输出 image/fileRef/workspace，真实的 SFTP 文件加载留待 M3。
skeleton_factory!(
    SftpFileSourceFactory,
    "sftpFileSource",
    "skeleton; real SFTP file source pending M3"
);
// sshSession：输出 ssh，真实的 SSH 会话/命令留待 M3。
skeleton_factory!(
    SshSessionFactory,
    "sshSession",
    "skeleton; real SSH session pending M3"
);
// x5Device：输出 control/rtsp/snapshot/video，真实的 X5 TCP 控制留待 M3。
skeleton_factory!(
    X5DeviceFactory,
    "x5Device",
    "skeleton; real X5 device control pending M3"
);
// i2cTransfer：输出 result/rawResps，真实的 I²C helper 留待 M3。
skeleton_factory!(
    I2cTransferFactory,
    "i2cTransfer",
    "skeleton; real I2C transfer pending M3"
);
// eepromProvision：输出 result/transfer，真实的 EEPROM provisioning 留待 M3。
skeleton_factory!(
    EepromProvisionFactory,
    "eepromProvision",
    "skeleton; real EEPROM provision pending M3"
);

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(kind: &str) -> NodeSpec {
        NodeSpec {
            id: "n".to_owned(),
            kind: kind.to_owned(),
            title: kind.to_owned(),
            inputs: vec![],
            outputs: vec![],
            config: serde_json::json!({}),
        }
    }

    #[test]
    fn skeleton_factories_instantiate_with_expected_kinds() {
        let cases: [(&dyn NodeFactory, &str); 5] = [
            (&SftpFileSourceFactory, "sftpFileSource"),
            (&SshSessionFactory, "sshSession"),
            (&X5DeviceFactory, "x5Device"),
            (&I2cTransferFactory, "i2cTransfer"),
            (&EepromProvisionFactory, "eepromProvision"),
        ];
        for (factory, kind) in cases {
            assert_eq!(factory.kind(), kind, "factory kind mismatch for {kind}");
            let instance = factory
                .instantiate(spec(kind))
                .expect("instantiate skeleton");
            assert_eq!(instance.kind(), kind);
        }
    }

    #[test]
    fn skeleton_action_is_unsupported_not_panic() {
        // 骨架期不接受任何触发：on_action 返回 UnsupportedAction，而非 panic 或伪造执行。
        let kind = "x5Device";
        let mut instance = X5DeviceFactory
            .instantiate(spec(kind))
            .expect("instantiate");
        let mut rt = crate::engine::NodeRuntime::new(crate::engine::SpawnContext {
            outputs: crate::engine::OutputRegistry::default(),
            reporter: crate::engine::NodeReporter::new(
                "n".to_owned(),
                std::sync::mpsc::channel().0,
                std::sync::mpsc::channel().0,
            ),
            services: std::sync::Arc::new(crate::engine::EngineServices::default()),
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            viewer_slot: None,
        });
        let err = instance
            .on_action(NodeAction::Trigger, &mut rt)
            .expect_err("skeleton action must be unsupported");
        assert!(matches!(err, NodeError::UnsupportedAction(_)));
    }
}
