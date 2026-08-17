//! WebSocket 入站命令分发：request 信封按 `path` 路由到对应处理器，返回 `Result<Value, String>`。
//!
//! 信封（见计划 61-77 行）：
//! - 入站 `{ "id": 42, "kind": "request", "path": "workflow.save", "payload": {...} }`
//! - 回包 `{ "id": 42, "kind": "response", "ok": true, "payload": {...} }` / `ok:false, error`
//!
//! 处理器复用 engine_api 的图投影（`to_node_spec`/`to_edge_spec`）、workflow 的 CRUD 逻辑，
//! 与 t4 落盘的 `GraphEngine` 增量 API；`control.*` 委托给 main.rs 的 `control_dispatch`
//! （控制请求结构体/辅助函数均私有于 main.rs，留在同模块以复用）。
//!
//! 返回值统一为 `Result<serde_json::Value, String>`：`Err` 的字符串即 response 信封的 `error`。

use serde_json::Value;

use crate::engine_api;
use crate::workflow::{
    normalize_workflow, validate_workflow, WorkflowEdge, WorkflowGraph, WorkflowNode,
};
use crate::AppState;

/// 分发一个 request 信封的 `path` + `payload`，返回 processor 的结果（`Err` 即 `error`）。
///
/// `state` 为连接共享的 `AppState` 克隆（内部均为 `Arc`，克隆廉价）。本函数不写回 socket，
/// 由调用方（main.rs 的 `handle_ws_socket`）把返回值包装成 response 信封写回对应连接。
pub fn dispatch(path: &str, payload: Value, state: &AppState) -> Result<Value, String> {
    match path {
        // —— 图编辑增量命令（D6，走 t4 增量 API）——
        "graph.addNode" => graph_add_node(payload, state),
        "graph.removeNode" => graph_remove_node(payload, state),
        "graph.addEdge" => graph_add_edge(payload, state),
        "graph.removeEdge" => graph_remove_edge(payload, state),
        "graph.updateNode" => graph_update_node(payload, state),

        // —— 运行时动作 ——
        "runtime.run" => runtime_run(payload, state),
        "runtime.start" => runtime_start(state),
        "runtime.stop" => runtime_stop(state),
        "runtime.status" => runtime_status(state),
        "runtime.node.action" => runtime_node_action(payload, state),
        "runtime.node.output" => runtime_node_output(payload, state),

        // —— 工作流 CRUD ——
        "workflow.list" => workflow_list(state),
        "workflow.get" => workflow_get(payload, state),
        "workflow.create" => workflow_create(payload, state),
        "workflow.import" => workflow_import(payload, state),
        "workflow.save" => workflow_save(payload, state),
        "workflow.delete" => workflow_delete(payload, state),
        "workflow.validate" => workflow_validate(payload),
        "workflow.export" => workflow_get(payload, state),
        "workflow.seed" => workflow_seed(),
        "workflow.nodeCatalog" => workflow_node_catalog(),
        "workflow.workmodeTemplates" => workflow_workmode_templates(),

        // —— 文件目录 ——
        "file.local.list" => file_local_list(payload),

        // —— 控制命令（i2c/eeprom/x5/calibration），复用 main.rs 内的既有处理逻辑 ——
        path if path.starts_with("control.") => crate::control_dispatch(path, payload, state),

        other => Err(format!("unknown ws path `{other}`")),
    }
}

// ---------------------------------------------------------------------------
// graph.* 增量命令
// ---------------------------------------------------------------------------

fn engine_guard<'a>(
    state: &'a AppState,
    op: &str,
) -> Result<std::sync::MutexGuard<'a, Option<camera_toolbox_app::engine::GraphEngine>>, String> {
    let guard = state.engine_runtime.engine();
    if guard.as_ref().is_none() {
        return Err(format!(
            "engine not running; load a workflow via runtime.run before {op}"
        ));
    }
    Ok(guard)
}

fn graph_add_node(payload: Value, state: &AppState) -> Result<Value, String> {
    let node: WorkflowNode = serde_json::from_value(payload).map_err(deser_err)?;
    let spec = engine_api::to_node_spec(&node);
    let mut guard = engine_guard(state, "graph.addNode")?;
    let engine = guard
        .as_mut()
        .ok_or_else(|| "engine not running".to_owned())?;
    engine
        .add_node(spec, &state.engine_runtime.registry)
        .map_err(|e| e.to_string())?;
    Ok(Value::Object(Default::default()))
}

fn graph_remove_node(payload: Value, state: &AppState) -> Result<Value, String> {
    #[derive(serde::Deserialize)]
    struct Body {
        #[serde(rename = "nodeId")]
        node_id: String,
    }
    let body: Body = serde_json::from_value(payload).map_err(deser_err)?;
    let mut guard = engine_guard(state, "graph.removeNode")?;
    let engine = guard
        .as_mut()
        .ok_or_else(|| "engine not running".to_owned())?;
    engine.remove_node(&body.node_id).map_err(|e| e.to_string())?;
    Ok(Value::Object(Default::default()))
}

fn graph_add_edge(payload: Value, state: &AppState) -> Result<Value, String> {
    #[derive(serde::Deserialize)]
    struct Body {
        edge: WorkflowEdge,
    }
    let body: Body = serde_json::from_value(payload).map_err(deser_err)?;
    let spec = engine_api::to_edge_spec(&body.edge);
    let mut guard = engine_guard(state, "graph.addEdge")?;
    let engine = guard
        .as_mut()
        .ok_or_else(|| "engine not running".to_owned())?;
    engine.add_edge(spec).map_err(|e| e.to_string())?;
    Ok(Value::Object(Default::default()))
}

fn graph_remove_edge(payload: Value, state: &AppState) -> Result<Value, String> {
    #[derive(serde::Deserialize)]
    struct Body {
        #[serde(rename = "edgeId")]
        edge_id: String,
    }
    let body: Body = serde_json::from_value(payload).map_err(deser_err)?;
    let mut guard = engine_guard(state, "graph.removeEdge")?;
    let engine = guard
        .as_mut()
        .ok_or_else(|| "engine not running".to_owned())?;
    engine.remove_edge(&body.edge_id).map_err(|e| e.to_string())?;
    Ok(Value::Object(Default::default()))
}

fn graph_update_node(payload: Value, state: &AppState) -> Result<Value, String> {
    #[derive(serde::Deserialize)]
    struct Body {
        #[serde(rename = "nodeId")]
        node_id: String,
        #[serde(default)]
        config: Value,
    }
    let body: Body = serde_json::from_value(payload).map_err(deser_err)?;
    let mut guard = engine_guard(state, "graph.updateNode")?;
    let engine = guard
        .as_mut()
        .ok_or_else(|| "engine not running".to_owned())?;
    engine
        .update_node(&body.node_id, body.config)
        .map_err(|e| e.to_string())?;
    Ok(Value::Object(Default::default()))
}

// ---------------------------------------------------------------------------
// runtime.*
// ---------------------------------------------------------------------------

fn runtime_run(payload: Value, state: &AppState) -> Result<Value, String> {
    let graph: WorkflowGraph = serde_json::from_value(payload).map_err(deser_err)?;
    let spec = engine_api::to_engine_spec(&graph);
    let services = engine_api::build_services(state);
    let engine = camera_toolbox_app::engine::GraphEngine::build(
        spec,
        &state.engine_runtime.registry,
        services,
    )
    .map_err(|e| e.to_string())?;
    let mut slot = state.engine_runtime.engine();
    if let Some(mut previous) = slot.take() {
        previous.stop();
    }
    *slot = Some(engine);
    Ok(serde_json::json!({ "running": true, "nodes": graph.nodes.len() }))
}

fn runtime_start(state: &AppState) -> Result<Value, String> {
    let engine = state.engine_runtime.engine();
    let engine = engine
        .as_ref()
        .ok_or_else(|| "engine not running".to_owned())?;
    engine.start_all().map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "started": true }))
}

fn runtime_stop(state: &AppState) -> Result<Value, String> {
    let mut slot = state.engine_runtime.engine();
    if let Some(mut engine) = slot.take() {
        engine.stop();
    }
    Ok(serde_json::json!({ "running": false }))
}

fn runtime_status(state: &AppState) -> Result<Value, String> {
    let engine = state.engine_runtime.engine();
    let statuses = engine
        .as_ref()
        .map(camera_toolbox_app::engine::GraphEngine::drain_status)
        .unwrap_or_default();
    Ok(serde_json::to_value(statuses).map_err(|e| e.to_string())?)
}

fn runtime_node_action(payload: Value, state: &AppState) -> Result<Value, String> {
    #[derive(serde::Deserialize)]
    struct Body {
        #[serde(rename = "nodeId")]
        node_id: String,
        action: String,
    }
    let body: Body = serde_json::from_value(payload).map_err(deser_err)?;
    let action = engine_api::parse_action_str(&body.action)?;
    let engine = state.engine_runtime.engine();
    let engine = engine
        .as_ref()
        .ok_or_else(|| "engine not running".to_owned())?;
    engine
        .send_action(&body.node_id, action)
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "ok": true }))
}

fn runtime_node_output(payload: Value, state: &AppState) -> Result<Value, String> {
    #[derive(serde::Deserialize)]
    struct Body {
        #[serde(rename = "nodeId")]
        node_id: String,
    }
    let body: Body = serde_json::from_value(payload).map_err(deser_err)?;
    let packet = {
        let engine = state.engine_runtime.engine();
        engine.as_ref().and_then(|e| e.latest_output(&body.node_id))
    };
    let Some(packet) = packet else {
        return Err("no output available".to_owned());
    };
    Ok(engine_api::packet_to_json(&packet))
}

// ---------------------------------------------------------------------------
// workflow.* CRUD
// ---------------------------------------------------------------------------

fn workflow_list(state: &AppState) -> Result<Value, String> {
    let summaries = state
        .workflow_store
        .list()
        .map_err(|(_, msg)| msg)?;
    Ok(serde_json::to_value(summaries).map_err(|e| e.to_string())?)
}

fn workflow_get(payload: Value, state: &AppState) -> Result<Value, String> {
    #[derive(serde::Deserialize)]
    struct Body {
        id: String,
    }
    let body: Body = serde_json::from_value(payload).map_err(deser_err)?;
    let graph = state
        .workflow_store
        .load(&body.id)
        .map_err(|(_, msg)| msg)?;
    Ok(serde_json::to_value(graph).map_err(|e| e.to_string())?)
}

fn workflow_create(payload: Value, state: &AppState) -> Result<Value, String> {
    let mut graph: WorkflowGraph = serde_json::from_value(payload).map_err(deser_err)?;
    if graph.id.trim().is_empty() {
        graph.id = format!("workflow-{}", crate::next_revision());
    }
    let revision = crate::next_revision();
    let graph = normalize_workflow(graph, revision)?;
    state.workflow_store.save(&graph).map_err(|(_, msg)| msg)?;
    Ok(serde_json::to_value(graph).map_err(|e| e.to_string())?)
}

fn workflow_import(payload: Value, state: &AppState) -> Result<Value, String> {
    let mut graph: WorkflowGraph = serde_json::from_value(payload).map_err(deser_err)?;
    if graph.id.trim().is_empty() {
        graph.id = format!("workflow-{}", crate::next_revision());
    }
    let graph = normalize_workflow(graph, crate::next_revision())?;
    state.workflow_store.save(&graph).map_err(|(_, msg)| msg)?;
    Ok(serde_json::to_value(graph).map_err(|e| e.to_string())?)
}

fn workflow_save(payload: Value, state: &AppState) -> Result<Value, String> {
    #[derive(serde::Deserialize)]
    struct Body {
        graph: WorkflowGraph,
        revision: String,
    }

    let body: Body = serde_json::from_value(payload).map_err(deser_err)?;
    let existing = state
        .workflow_store
        .load(&body.graph.id)
        .map_err(|(_, msg)| msg)?;
    if existing.revision != body.revision {
        return Err(format!(
            "workflow revision conflict: expected `{}`, current `{}`",
            body.revision, existing.revision
        ));
    }
    let graph = normalize_workflow(body.graph, crate::next_revision())?;
    state.workflow_store.save(&graph).map_err(|(_, msg)| msg)?;
    Ok(serde_json::to_value(graph).map_err(|e| e.to_string())?)
}

fn workflow_delete(payload: Value, state: &AppState) -> Result<Value, String> {
    #[derive(serde::Deserialize)]
    struct Body {
        id: String,
    }
    let body: Body = serde_json::from_value(payload).map_err(deser_err)?;
    state
        .workflow_store
        .delete(&body.id)
        .map_err(|(_, msg)| msg)?;
    Ok(serde_json::json!({ "deleted": true }))
}

fn workflow_validate(payload: Value) -> Result<Value, String> {
    let graph: WorkflowGraph = serde_json::from_value(payload).map_err(deser_err)?;
    match validate_workflow(&graph) {
        Ok(()) => Ok(serde_json::json!({ "ok": true })),
        Err(error) => Err(error),
    }
}

/// 返回内置 seed 工作流图（对应旧 GET /api/workflow）。
fn workflow_seed() -> Result<Value, String> {
    let graph = crate::workflow::seed_workflow_graph();
    serde_json::to_value(graph).map_err(|e| e.to_string())
}

/// 返回节点目录（对应旧 GET /api/node-catalog）。
fn workflow_node_catalog() -> Result<Value, String> {
    let catalog = crate::workflow::node_catalog();
    serde_json::to_value(catalog).map_err(|e| e.to_string())
}

/// 返回工作模式模板（对应旧 GET /api/workmode-templates）。
fn workflow_workmode_templates() -> Result<Value, String> {
    let templates = crate::workflow::workmode_templates();
    serde_json::to_value(templates).map_err(|e| e.to_string())
}

/// 列出本地目录（对应旧 GET /api/files/local/list）。
fn file_local_list(payload: Value) -> Result<Value, String> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Body {
        root: String,
        #[serde(default)]
        path: String,
    }
    let body: Body = serde_json::from_value(payload).map_err(deser_err)?;
    let response = crate::files_api::list_local_files_inner(&body.root, &body.path)?;
    serde_json::to_value(response).map_err(|e| e.to_string())
}

fn deser_err(error: impl std::fmt::Display) -> String {
    format!("invalid payload: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_validate_accepts_seed_graph() {
        let graph = crate::workflow::seed_workflow_graph();
        let payload = serde_json::to_value(&graph).unwrap();
        let out = workflow_validate(payload).unwrap();
        assert_eq!(out["ok"], true);
    }

    #[test]
    fn workflow_save_requires_matching_revision() {
        let dir = std::env::temp_dir().join(format!("ws-router-save-test-{}", crate::next_revision()));
        let state = test_state(dir.clone());
        let mut graph = crate::workflow::seed_workflow_graph();
        graph.id = "ws-save".to_owned();
        graph.revision = "rev-a".to_owned();
        state.workflow_store.save(&graph).expect("seed workflow saved");

        let mut edited = graph.clone();
        edited.title = "Edited over WS".to_owned();
        let payload = serde_json::json!({ "graph": edited, "revision": "stale-rev" });

        let error = workflow_save(payload, &state).expect_err("stale revision must fail");
        assert!(error.contains("workflow revision conflict"));
        assert_eq!(state.workflow_store.load("ws-save").unwrap().title, graph.title);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn workflow_save_updates_when_revision_matches() {
        let dir = std::env::temp_dir().join(format!("ws-router-save-ok-test-{}", crate::next_revision()));
        let state = test_state(dir.clone());
        let mut graph = crate::workflow::seed_workflow_graph();
        graph.id = "ws-save-ok".to_owned();
        graph.revision = "rev-a".to_owned();
        state.workflow_store.save(&graph).expect("seed workflow saved");

        let mut edited = graph.clone();
        edited.title = "Edited over WS".to_owned();
        let payload = serde_json::json!({ "graph": edited, "revision": "rev-a" });

        let out = workflow_save(payload, &state).expect("matching revision saves");
        assert_eq!(out["title"], "Edited over WS");
        assert_ne!(out["revision"], "rev-a");
        assert_eq!(state.workflow_store.load("ws-save-ok").unwrap().title, "Edited over WS");
        std::fs::remove_dir_all(dir).ok();
    }

    fn test_state(dir: std::path::PathBuf) -> AppState {
        AppState {
            workflow_store: std::sync::Arc::new(crate::WorkflowStore { dir }),
            control_runtime: std::sync::Arc::new(crate::ControlRuntime::production()),
            #[cfg(feature = "calibration-opencv")]
            calibration_backend: std::sync::Arc::new(camera_toolbox_adapters::OpenCvCalibrationBackend),
            eeprom_inspects: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            engine_runtime: std::sync::Arc::new(crate::engine_api::EngineRuntime::new()),
            ws_hub: std::sync::Arc::new(crate::ws_hub::WsHub::new()),
        }
    }

    #[test]
    fn deser_err_wraps_message() {
        let err = deser_err("boom");
        assert_eq!(err, "invalid payload: boom");
    }
}
