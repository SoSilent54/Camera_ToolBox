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

use std::collections::BTreeSet;

use serde_json::{Value, json};

use crate::AppState;
use crate::engine_api;
use crate::workflow::{
    NodePosition, WorkflowEdge, WorkflowGraph, WorkflowNode, normalize_workflow, validate_workflow,
};

/// 分发一个 request 信封的 `path` + `payload`，返回 processor 的结果（`Err` 即 `error`）。
///
/// `state` 为连接共享的 `AppState` 克隆（内部均为 `Arc`，克隆廉价）。本函数不写回 socket，
/// 由调用方（main.rs 的 `handle_ws_socket`）把返回值包装成 response 信封写回对应连接。
pub fn dispatch(path: &str, payload: Value, state: &AppState) -> Result<Value, String> {
    match path {
        // —— 后端权威图编辑命令：成功后返回并广播完整 graph snapshot ——
        "graph.current" => graph_current(state),
        "graph.replace" => graph_replace(payload, state),
        "graph.addNode" => graph_add_node(payload, state),
        "graph.addNodeAndEdge" => graph_add_node_and_edge(payload, state),
        "graph.removeNode" => graph_remove_node(payload, state),
        "graph.removeSelection" => graph_remove_selection(payload, state),
        "graph.addEdge" => graph_add_edge(payload, state),
        "graph.removeEdge" => graph_remove_edge(payload, state),
        "graph.updateNode" => graph_update_node(payload, state),
        "graph.updateNodePositions" => graph_update_node_positions(payload, state),
        "graph.patchNode" => graph_patch_node(payload, state),
        "graph.updateExtractorAndTaskBuilderInterface" => {
            graph_update_extractor_and_task_builder_interface(payload, state)
        }
        // —— 运行时动作 ——
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
fn reject_legacy_workflow_schema(value: &Value) -> Result<(), String> {
    let schema = value
        .get("schemaVersion")
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .get("graph")
                .and_then(|graph| graph.get("schemaVersion"))
                .and_then(Value::as_str)
        });
    if let Some("workflow.v1") = schema {
        return Err(
            "unsupported workflow schema `workflow.v1`; migration is disabled, import workflow.v2"
                .to_owned(),
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// graph.* 权威图命令
// ---------------------------------------------------------------------------

pub(crate) fn snapshot_envelope_text(state: &AppState) -> Result<String, String> {
    let graph = authoritative_graph(state)?;
    Ok(snapshot_envelope(&graph).to_string())
}

fn graph_current(state: &AppState) -> Result<Value, String> {
    ensure_engine_loaded(state)?;
    let graph = authoritative_graph(state)?;
    serde_json::to_value(graph).map_err(|e| e.to_string())
}

fn graph_replace(payload: Value, state: &AppState) -> Result<Value, String> {
    reject_legacy_workflow_schema(&payload)?;
    let graph: WorkflowGraph = serde_json::from_value(payload).map_err(deser_err)?;
    let graph = normalize_workflow(graph, crate::next_revision())?;
    rebuild_engine_from_graph(&graph, state)?;
    *state
        .graph_session
        .lock()
        .map_err(|_| "authoritative graph state is unavailable".to_owned())? = graph.clone();
    publish_snapshot(state, &graph);
    serde_json::to_value(graph).map_err(|e| e.to_string())
}

fn graph_add_node(payload: Value, state: &AppState) -> Result<Value, String> {
    let node: WorkflowNode = serde_json::from_value(payload).map_err(deser_err)?;
    let node_id = node.id.clone();
    commit_graph_mutation_with_loaded_engine(
        state,
        |graph| {
            if graph.nodes.iter().any(|existing| existing.id == node.id) {
                return Err(format!("node `{}` already exists", node.id));
            }
            graph.nodes.push(node);
            Ok(())
        },
        |engine, graph| {
            let engine_node = graph
                .nodes
                .iter()
                .find(|candidate| candidate.id == node_id)
                .ok_or_else(|| format!("normalized graph removed node `{node_id}`"))?;
            engine
                .add_node(
                    engine_api::to_node_spec(engine_node),
                    &state.engine_runtime.registry,
                )
                .map_err(|e| e.to_string())
        },
    )
}

fn graph_add_node_and_edge(payload: Value, state: &AppState) -> Result<Value, String> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Body {
        node: WorkflowNode,
        edge: WorkflowEdge,
    }

    let body: Body = serde_json::from_value(payload).map_err(deser_err)?;
    let node = body.node;
    let edge = body.edge;
    let node_id = node.id.clone();
    let edge_id = edge.id.clone();
    let replaced_engine_edge = authoritative_graph(state)?
        .edges
        .into_iter()
        .find(|existing| existing.id == edge_id)
        .as_ref()
        .map(engine_api::to_edge_spec);
    commit_graph_mutation_with_loaded_engine(
        state,
        |graph| {
            if graph.nodes.iter().any(|existing| existing.id == node.id) {
                return Err(format!("node `{}` already exists", node.id));
            }
            graph.nodes.push(node);
            graph.edges.retain(|existing| existing.id != edge.id);
            graph.edges.push(edge);
            Ok(())
        },
        |engine, graph| {
            let engine_node = graph
                .nodes
                .iter()
                .find(|candidate| candidate.id == node_id)
                .ok_or_else(|| format!("normalized graph removed node `{node_id}`"))?;
            engine
                .add_node(
                    engine_api::to_node_spec(engine_node),
                    &state.engine_runtime.registry,
                )
                .map_err(|e| e.to_string())?;
            let _ = engine.remove_edge(&edge_id);
            let Some(engine_edge) = graph
                .edges
                .iter()
                .find(|candidate| candidate.id == edge_id)
                .map(engine_api::to_edge_spec)
            else {
                return Ok(());
            };
            if let Err(error) = engine.add_edge(engine_edge) {
                let _ = engine.remove_node(&node_id);
                if let Some(replaced_edge) = replaced_engine_edge {
                    if let Err(restore_error) = engine.add_edge(replaced_edge) {
                        return Err(format!(
                            "{}; failed to restore previous edge `{}`: {}",
                            error, edge_id, restore_error
                        ));
                    }
                }
                return Err(error.to_string());
            }
            Ok(())
        },
    )
}

fn graph_remove_node(payload: Value, state: &AppState) -> Result<Value, String> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Body {
        node_id: String,
    }
    let body: Body = serde_json::from_value(payload).map_err(deser_err)?;
    let removed_node_id = body.node_id;
    commit_graph_mutation_with_loaded_engine(
        state,
        |graph| {
            let old_len = graph.nodes.len();
            graph.nodes.retain(|node| node.id != removed_node_id);
            if graph.nodes.len() == old_len {
                return Err(format!("node `{}` not found", removed_node_id));
            }
            graph.edges.retain(|edge| {
                edge.source.node_id != removed_node_id && edge.target.node_id != removed_node_id
            });
            Ok(())
        },
        |engine, _graph| {
            engine
                .remove_node(&removed_node_id)
                .map_err(|e| e.to_string())
        },
    )
}

fn graph_remove_selection(payload: Value, state: &AppState) -> Result<Value, String> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Body {
        #[serde(default)]
        node_ids: Vec<String>,
        #[serde(default)]
        edge_ids: Vec<String>,
    }

    let body: Body = serde_json::from_value(payload).map_err(deser_err)?;
    if body.node_ids.is_empty() && body.edge_ids.is_empty() {
        return Err("selection delete must include nodeIds or edgeIds".to_owned());
    }
    let node_ids: BTreeSet<String> = body.node_ids.into_iter().collect();
    let edge_ids: BTreeSet<String> = body.edge_ids.into_iter().collect();
    let engine_node_ids = node_ids.clone();
    let engine_edge_ids = edge_ids.clone();

    commit_graph_mutation_with_loaded_engine(
        state,
        move |graph| {
            let old_node_len = graph.nodes.len();
            let old_edge_len = graph.edges.len();
            graph.nodes.retain(|node| !node_ids.contains(&node.id));
            graph.edges.retain(|edge| {
                !edge_ids.contains(&edge.id)
                    && !node_ids.contains(&edge.source.node_id)
                    && !node_ids.contains(&edge.target.node_id)
            });
            if graph.nodes.len() == old_node_len && graph.edges.len() == old_edge_len {
                return Err("selected graph items were not found".to_owned());
            }
            Ok(())
        },
        move |engine, _graph| {
            for edge_id in &engine_edge_ids {
                let _ = engine.remove_edge(edge_id);
            }
            for node_id in &engine_node_ids {
                engine.remove_node(node_id).map_err(|e| e.to_string())?;
            }
            Ok(())
        },
    )
}

fn graph_add_edge(payload: Value, state: &AppState) -> Result<Value, String> {
    #[derive(serde::Deserialize)]
    struct Body {
        edge: WorkflowEdge,
    }
    let body: Body = serde_json::from_value(payload).map_err(deser_err)?;
    let edge = body.edge;
    let edge_id = edge.id.clone();
    let replaced_engine_edge = authoritative_graph(state)?
        .edges
        .into_iter()
        .find(|existing| existing.id == edge_id)
        .as_ref()
        .map(engine_api::to_edge_spec);
    commit_graph_mutation_with_loaded_engine(
        state,
        |graph| {
            graph.edges.retain(|existing| existing.id != edge.id);
            graph.edges.push(edge);
            Ok(())
        },
        |engine, graph| {
            let _ = engine.remove_edge(&edge_id);
            let Some(engine_edge) = graph
                .edges
                .iter()
                .find(|candidate| candidate.id == edge_id)
                .map(engine_api::to_edge_spec)
            else {
                return Ok(());
            };
            if let Err(error) = engine.add_edge(engine_edge) {
                if let Some(replaced_edge) = replaced_engine_edge {
                    if let Err(restore_error) = engine.add_edge(replaced_edge) {
                        return Err(format!(
                            "{}; failed to restore previous edge `{}`: {}",
                            error, edge_id, restore_error
                        ));
                    }
                }
                return Err(error.to_string());
            }
            Ok(())
        },
    )
}

fn graph_remove_edge(payload: Value, state: &AppState) -> Result<Value, String> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Body {
        edge_id: String,
    }
    let body: Body = serde_json::from_value(payload).map_err(deser_err)?;
    let removed_edge_id = body.edge_id;
    commit_graph_mutation_with_loaded_engine(
        state,
        |graph| {
            let old_len = graph.edges.len();
            graph.edges.retain(|edge| edge.id != removed_edge_id);
            if graph.edges.len() == old_len {
                return Err(format!("edge `{}` not found", removed_edge_id));
            }
            Ok(())
        },
        |engine, _graph| {
            engine
                .remove_edge(&removed_edge_id)
                .map_err(|e| e.to_string())
        },
    )
}

fn graph_update_node_positions(payload: Value, state: &AppState) -> Result<Value, String> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Body {
        nodes: Vec<NodePositionUpdate>,
    }

    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct NodePositionUpdate {
        node_id: String,
        position: NodePosition,
    }

    let body: Body = serde_json::from_value(payload).map_err(deser_err)?;
    if body.nodes.is_empty() {
        return Err("position batch must include at least one node".to_owned());
    }
    commit_graph_mutation(state, move |graph| {
        for update in body.nodes {
            let Some(existing) = graph
                .nodes
                .iter_mut()
                .find(|node| node.id == update.node_id)
            else {
                return Err(format!("node `{}` not found", update.node_id));
            };
            existing.position = update.position;
        }
        Ok(())
    })
}

fn graph_update_node(payload: Value, state: &AppState) -> Result<Value, String> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Body {
        node_id: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        position: Option<NodePosition>,
        #[serde(default)]
        config: Option<Value>,
    }
    let body: Body = serde_json::from_value(payload).map_err(deser_err)?;
    if body.title.is_none() && body.position.is_none() && body.config.is_none() {
        return Err("node update must include title, position, or config".to_owned());
    }
    let node_id = body.node_id.clone();
    let updates_runtime_config = body.config.is_some();
    if updates_runtime_config {
        commit_graph_mutation_with_loaded_engine(
            state,
            |graph| {
                let Some(existing) = graph.nodes.iter_mut().find(|node| node.id == body.node_id)
                else {
                    return Err(format!("node `{}` not found", body.node_id));
                };
                if matches!(
                    existing.kind,
                    crate::workflow::NodeKind::StructuredFieldExtractor
                        | crate::workflow::NodeKind::I2cTaskBuilder
                ) {
                    return Err(
                        "dynamic node interfaces must use graph.updateExtractorAndTaskBuilderInterface"
                            .to_owned(),
                    );
                }
                if let Some(title) = body.title {
                    existing.title = title;
                }
                if let Some(position) = body.position {
                    existing.position = position;
                }
                if let Some(config) = body.config {
                    existing.config = config;
                }
                Ok(())
            },
            |engine, graph| update_engine_node_config(engine, graph, &node_id),
        )
    } else {
        commit_graph_mutation(state, |graph| {
            let Some(existing) = graph.nodes.iter_mut().find(|node| node.id == body.node_id) else {
                return Err(format!("node `{}` not found", body.node_id));
            };
            if let Some(title) = body.title {
                existing.title = title;
            }
            if let Some(position) = body.position {
                existing.position = position;
            }
            Ok(())
        })
    }
}

/// 对既有节点执行字段级更新；配置补丁只覆盖提供的键。
fn graph_patch_node(payload: Value, state: &AppState) -> Result<Value, String> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Body {
        node_id: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        config: Option<serde_json::Map<String, Value>>,
    }

    let body: Body = serde_json::from_value(payload).map_err(deser_err)?;
    if body.title.is_none() && body.config.is_none() {
        return Err("node patch must include title or config".to_owned());
    }
    let node_id = body.node_id.clone();
    let updates_runtime_config = body.config.is_some();
    if updates_runtime_config {
        commit_graph_mutation_with_loaded_engine(
            state,
            |graph| {
                let Some(existing) = graph.nodes.iter_mut().find(|node| node.id == body.node_id)
                else {
                    return Err(format!("node `{}` not found", body.node_id));
                };
                if matches!(
                    existing.kind,
                    crate::workflow::NodeKind::StructuredFieldExtractor
                        | crate::workflow::NodeKind::I2cTaskBuilder
                ) {
                    return Err(
                        "dynamic node interfaces must use graph.updateExtractorAndTaskBuilderInterface"
                            .to_owned(),
                    );
                }
                if let Some(title) = body.title {
                    existing.title = title;
                }
                if let Some(config_patch) = body.config {
                    let Some(config) = existing.config.as_object_mut() else {
                        return Err(format!(
                            "node `{}` config must be a JSON object",
                            body.node_id
                        ));
                    };
                    config.extend(config_patch);
                }
                Ok(())
            },
            |engine, graph| update_engine_node_config(engine, graph, &node_id),
        )
    } else {
        commit_graph_mutation(state, |graph| {
            let Some(existing) = graph.nodes.iter_mut().find(|node| node.id == body.node_id) else {
                return Err(format!("node `{}` not found", body.node_id));
            };
            if let Some(title) = body.title {
                existing.title = title;
            }
            Ok(())
        })
    }
}

/// 原子更新 extractor 输出与 task builder 配置：先投影候选端口并校验全图边，成功后整体替换运行时图。
fn graph_update_extractor_and_task_builder_interface(
    payload: Value,
    state: &AppState,
) -> Result<Value, String> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Body {
        extractor_node_id: String,
        extractor_outputs: Vec<Value>,
        task_builder_node_id: String,
        task_builder_config: serde_json::Map<String, Value>,
    }
    let body: Body = serde_json::from_value(payload).map_err(deser_err)?;
    let current = authoritative_graph(state)?;
    let mut candidate = current.clone();
    let extractor = candidate
        .nodes
        .iter_mut()
        .find(|node| node.id == body.extractor_node_id)
        .ok_or_else(|| format!("node `{}` not found", body.extractor_node_id))?;
    if extractor.kind != crate::workflow::NodeKind::StructuredFieldExtractor {
        return Err(format!(
            "node `{}` is not a structuredFieldExtractor",
            extractor.id
        ));
    }
    extractor.config = json!({ "outputs": body.extractor_outputs });
    let builder = candidate
        .nodes
        .iter_mut()
        .find(|node| node.id == body.task_builder_node_id)
        .ok_or_else(|| format!("node `{}` not found", body.task_builder_node_id))?;
    if builder.kind != crate::workflow::NodeKind::I2cTaskBuilder {
        return Err(format!("node `{}` is not an i2cTaskBuilder", builder.id));
    }
    builder.config = Value::Object(body.task_builder_config);

    let candidate = normalize_workflow(candidate, crate::next_revision())?;
    let diff = interface_port_diff(&current, &candidate);
    rebuild_engine_from_graph(&candidate, state)?;
    *state
        .graph_session
        .lock()
        .map_err(|_| "authoritative graph state is unavailable".to_owned())? = candidate.clone();
    publish_snapshot(state, &candidate);
    Ok(json!({
        "graph": candidate,
        "candidatePortDiff": diff,
        "diagnostics": ["candidate interfaces and all retained edges validated", "runtime graph replaced atomically after candidate build"],
    }))
}

fn interface_port_diff(current: &WorkflowGraph, candidate: &WorkflowGraph) -> Vec<Value> {
    current
        .nodes
        .iter()
        .filter_map(|old| {
            let next = candidate.nodes.iter().find(|node| node.id == old.id)?;
            let diff = |old: &[crate::workflow::WorkflowPort],
                        next: &[crate::workflow::WorkflowPort]| {
                let old_ids: BTreeSet<&str> = old.iter().map(|port| port.id.as_str()).collect();
                let next_ids: BTreeSet<&str> = next.iter().map(|port| port.id.as_str()).collect();
                (
                    next_ids
                        .difference(&old_ids)
                        .map(|id| (*id).to_owned())
                        .collect::<Vec<_>>(),
                    old_ids
                        .difference(&next_ids)
                        .map(|id| (*id).to_owned())
                        .collect::<Vec<_>>(),
                )
            };
            let (added_inputs, removed_inputs) = diff(&old.inputs, &next.inputs);
            let (added_outputs, removed_outputs) = diff(&old.outputs, &next.outputs);
            (!added_inputs.is_empty()
                || !removed_inputs.is_empty()
                || !added_outputs.is_empty()
                || !removed_outputs.is_empty())
            .then(|| {
                json!({
                    "nodeId": old.id, "addedInputs": added_inputs, "removedInputs": removed_inputs,
                    "addedOutputs": added_outputs, "removedOutputs": removed_outputs,
                })
            })
        })
        .collect()
}

fn commit_graph_mutation<F>(state: &AppState, mutate: F) -> Result<Value, String>
where
    F: FnOnce(&mut WorkflowGraph) -> Result<(), String>,
{
    let current = authoritative_graph(state)?;
    let mut next = current;
    mutate(&mut next)?;
    let next = normalize_workflow(next, crate::next_revision())?;
    *state
        .graph_session
        .lock()
        .map_err(|_| "authoritative graph state is unavailable".to_owned())? = next.clone();
    publish_snapshot(state, &next);
    serde_json::to_value(next).map_err(|e| e.to_string())
}

fn commit_graph_mutation_with_loaded_engine<F, G>(
    state: &AppState,
    mutate: F,
    mutate_engine: G,
) -> Result<Value, String>
where
    F: FnOnce(&mut WorkflowGraph) -> Result<(), String>,
    G: FnOnce(&camera_toolbox_app::engine::GraphEngine, &WorkflowGraph) -> Result<(), String>,
{
    let current = authoritative_graph(state)?;
    let mut next = current;
    mutate(&mut next)?;
    let next = normalize_workflow(next, crate::next_revision())?;
    let engine = state.engine_runtime.engine();
    if let Some(engine) = engine.as_ref() {
        mutate_engine(engine, &next)?;
    } else {
        drop(engine);
        rebuild_engine_from_graph(&next, state)?;
    }
    *state
        .graph_session
        .lock()
        .map_err(|_| "authoritative graph state is unavailable".to_owned())? = next.clone();
    publish_snapshot(state, &next);
    serde_json::to_value(next).map_err(|e| e.to_string())
}

fn update_engine_node_config(
    engine: &camera_toolbox_app::engine::GraphEngine,
    graph: &WorkflowGraph,
    node_id: &str,
) -> Result<(), String> {
    let node = graph
        .nodes
        .iter()
        .find(|candidate| candidate.id == node_id)
        .ok_or_else(|| format!("node `{node_id}` not found"))?;
    engine
        .update_node(node_id, node.config.clone())
        .map_err(|e| e.to_string())
}

fn authoritative_graph(state: &AppState) -> Result<WorkflowGraph, String> {
    state
        .graph_session
        .lock()
        .map_err(|_| "authoritative graph state is unavailable".to_owned())
        .map(|graph| graph.clone())
}

fn ensure_engine_loaded(state: &AppState) -> Result<(), String> {
    let needs_engine = state.engine_runtime.engine().as_ref().is_none();
    if needs_engine {
        let graph = authoritative_graph(state)?;
        rebuild_engine_from_graph(&graph, state)?;
    }
    Ok(())
}

fn rebuild_engine_from_graph(graph: &WorkflowGraph, state: &AppState) -> Result<(), String> {
    let spec = engine_api::to_engine_spec(graph);
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
    Ok(())
}

fn publish_snapshot(state: &AppState, graph: &WorkflowGraph) {
    state
        .ws_hub
        .broadcast_text(snapshot_envelope(graph).to_string());
}

fn snapshot_envelope(graph: &WorkflowGraph) -> Value {
    json!({
        "kind": "snapshot",
        "payload": {
            "graph": graph,
            "statuses": [],
        },
    })
}

// ---------------------------------------------------------------------------
// runtime.*
// ---------------------------------------------------------------------------

fn runtime_status(state: &AppState) -> Result<Value, String> {
    let engine = state.engine_runtime.engine();
    let statuses = engine
        .as_ref()
        .map(camera_toolbox_app::engine::GraphEngine::drain_status)
        .unwrap_or_default();
    Ok(serde_json::to_value(statuses).map_err(|e| e.to_string())?)
}

#[derive(serde::Deserialize)]
struct RuntimeNodeActionBody {
    #[serde(rename = "nodeId")]
    node_id: String,
    action: String,
    #[serde(default)]
    payload: Value,
}

fn runtime_node_action(payload: Value, state: &AppState) -> Result<Value, String> {
    let body: RuntimeNodeActionBody = serde_json::from_value(payload).map_err(deser_err)?;
    let action = engine_api::parse_action_str(&body.action, body.payload)?;
    ensure_engine_loaded(state)?;
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
    let summaries = state.workflow_store.list().map_err(|(_, msg)| msg)?;
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
    reject_legacy_workflow_schema(&payload)?;
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
    reject_legacy_workflow_schema(&payload)?;
    let mut graph: WorkflowGraph = serde_json::from_value(payload).map_err(deser_err)?;
    if graph.id.trim().is_empty() {
        graph.id = format!("workflow-{}", crate::next_revision());
    }
    let graph = normalize_workflow(graph, crate::next_revision())?;
    state.workflow_store.save(&graph).map_err(|(_, msg)| msg)?;
    Ok(serde_json::to_value(graph).map_err(|e| e.to_string())?)
}

fn workflow_save(payload: Value, state: &AppState) -> Result<Value, String> {
    reject_legacy_workflow_schema(&payload)?;
    #[derive(serde::Deserialize)]
    struct Body {
        graph: WorkflowGraph,
        revision: String,
    }
    let body: Body = serde_json::from_value(payload).map_err(deser_err)?;
    if let Some(existing) = state
        .workflow_store
        .load_optional(&body.graph.id)
        .map_err(|(_, msg)| msg)?
    {
        if existing.revision != body.revision {
            return Err(format!(
                "workflow revision conflict: expected `{}`, current `{}`",
                body.revision, existing.revision
            ));
        }
    }
    let graph = normalize_workflow(body.graph, crate::next_revision())?;
    state.workflow_store.save(&graph).map_err(|(_, msg)| msg)?;
    *state
        .graph_session
        .lock()
        .map_err(|_| "authoritative graph state is unavailable".to_owned())? = graph.clone();
    publish_snapshot(state, &graph);
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
    reject_legacy_workflow_schema(&payload)?;
    let graph: WorkflowGraph = serde_json::from_value(payload).map_err(deser_err)?;
    validate_workflow(&graph)?;
    Ok(serde_json::json!({ "ok": true }))
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
        let dir =
            std::env::temp_dir().join(format!("ws-router-save-test-{}", crate::next_revision()));
        let state = test_state(dir.clone());
        let mut graph = crate::workflow::seed_workflow_graph();
        graph.id = "ws-save".to_owned();
        graph.revision = "rev-a".to_owned();
        state
            .workflow_store
            .save(&graph)
            .expect("seed workflow saved");

        let mut edited = graph.clone();
        edited.title = "Edited over WS".to_owned();
        let payload = serde_json::json!({ "graph": edited, "revision": "stale-rev" });

        let error = workflow_save(payload, &state).expect_err("stale revision must fail");
        assert!(error.contains("workflow revision conflict"));
        assert_eq!(
            state.workflow_store.load("ws-save").unwrap().title,
            graph.title
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn workflow_save_updates_when_revision_matches() {
        let dir =
            std::env::temp_dir().join(format!("ws-router-save-ok-test-{}", crate::next_revision()));
        let state = test_state(dir.clone());
        let mut graph = crate::workflow::seed_workflow_graph();
        graph.id = "ws-save-ok".to_owned();
        graph.revision = "rev-a".to_owned();
        state
            .workflow_store
            .save(&graph)
            .expect("seed workflow saved");

        let mut edited = graph.clone();
        edited.title = "Edited over WS".to_owned();
        let payload = serde_json::json!({ "graph": edited, "revision": "rev-a" });

        let out = workflow_save(payload, &state).expect("matching revision saves");
        assert_eq!(out["title"], "Edited over WS");
        assert_ne!(out["revision"], "rev-a");
        assert_eq!(
            state.workflow_store.load("ws-save-ok").unwrap().title,
            "Edited over WS"
        );
        std::fs::remove_dir_all(dir).ok();
    }
    #[test]
    fn graph_add_node_updates_authoritative_session_without_global_run() {
        let dir = std::env::temp_dir().join(format!(
            "ws-router-graph-add-node-{}",
            crate::next_revision()
        ));
        let state = test_state(dir.clone());
        let mut node = crate::workflow::seed_workflow_graph().nodes[0].clone();
        node.id = "authoritative-added-node".to_owned();
        node.title = "Authoritative Added".to_owned();

        let out = dispatch(
            "graph.addNode",
            serde_json::to_value(&node).unwrap(),
            &state,
        )
        .expect("graph.addNode succeeds without global run");

        assert!(
            out["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|candidate| candidate["id"] == node.id)
        );
        assert!(
            state
                .graph_session
                .lock()
                .unwrap()
                .nodes
                .iter()
                .any(|candidate| candidate.id == node.id)
        );
        assert!(state.engine_runtime.engine().as_ref().is_some());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn graph_add_node_and_edge_commits_as_one_authoritative_update() {
        let dir = std::env::temp_dir().join(format!(
            "ws-router-graph-add-node-edge-{}",
            crate::next_revision()
        ));
        let state = test_state(dir.clone());
        let seed = crate::workflow::seed_workflow_graph();
        let template_edge = seed.edges.first().expect("seed graph has an edge").clone();
        let mut node = seed
            .nodes
            .iter()
            .find(|candidate| candidate.id == template_edge.target.node_id)
            .expect("edge target exists")
            .clone();
        node.id = "authoritative-connected-node".to_owned();
        node.title = "Authoritative Connected".to_owned();
        let mut edge = template_edge;
        edge.id = "edge-to-authoritative-connected-node".to_owned();
        edge.target.node_id = node.id.clone();

        let out = dispatch(
            "graph.addNodeAndEdge",
            serde_json::json!({ "node": node, "edge": edge }),
            &state,
        )
        .expect("node and edge commit atomically");

        assert!(
            out["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|candidate| candidate["id"] == "authoritative-connected-node")
        );
        assert!(
            out["edges"]
                .as_array()
                .unwrap()
                .iter()
                .any(|candidate| candidate["id"] == "edge-to-authoritative-connected-node")
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn graph_patch_node_merges_config_without_overwriting_other_fields() {
        let dir = std::env::temp_dir().join(format!(
            "ws-router-graph-patch-merge-{}",
            crate::next_revision()
        ));
        let state = test_state(dir.clone());
        let seed = crate::workflow::seed_workflow_graph();
        let driver = seed
            .nodes
            .iter()
            .find(|node| node.id == "x5233-driver-1")
            .expect("seed X5_233 driver exists");
        let node_id = driver.id.clone();
        let expected_tcp_port = driver.config["tcpPort"].clone();

        let out = dispatch(
            "graph.patchNode",
            serde_json::json!({
                "nodeId": node_id.clone(),
                "config": { "host": "x5.example" }
            }),
            &state,
        )
        .expect("field patch succeeds");

        let node = out["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|candidate| candidate["id"] == node_id)
            .expect("patched node remains in graph");
        assert_eq!(node["config"]["host"], "x5.example");
        assert_eq!(node["config"]["tcpPort"], expected_tcp_port);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn graph_patch_node_rejects_full_node_payload() {
        let dir = std::env::temp_dir().join(format!(
            "ws-router-graph-patch-reject-{}",
            crate::next_revision()
        ));
        let state = test_state(dir.clone());
        let mut seed = crate::workflow::seed_workflow_graph();
        let node = seed.nodes.remove(0);
        let node_id = node.id.clone();

        let error = dispatch(
            "graph.patchNode",
            serde_json::json!({ "nodeId": node_id, "node": node }),
            &state,
        )
        .expect_err("full node payload is not a patch");
        assert!(error.contains("unknown field"));
        assert!(error.contains("node"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn graph_patch_title_does_not_rebuild_engine_or_refresh_statuses() {
        let dir = std::env::temp_dir().join(format!(
            "ws-router-graph-patch-title-{}",
            crate::next_revision()
        ));
        let state = test_state(dir.clone());
        let seed = crate::workflow::seed_workflow_graph();

        let node_id = seed
            .nodes
            .iter()
            .find(|node| node.id == "viewer-1")
            .expect("seed viewer exists")
            .id
            .clone();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let _ = runtime_status(&state).expect("drain initial statuses");

        let out = dispatch(
            "graph.patchNode",
            serde_json::json!({ "nodeId": node_id, "title": "Patched Viewer" }),
            &state,
        )
        .expect("title patch succeeds");

        assert!(
            out["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|candidate| candidate["title"] == "Patched Viewer")
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(runtime_status(&state).unwrap(), serde_json::json!([]));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn graph_update_node_updates_runtime_config_without_rebuild() {
        let dir = std::env::temp_dir().join(format!(
            "ws-router-graph-update-config-{}",
            crate::next_revision()
        ));
        let state = test_state(dir.clone());
        let seed = crate::workflow::seed_workflow_graph();
        let node = seed
            .nodes
            .iter()
            .find(|candidate| candidate.id == "x5233-driver-1")
            .expect("seed X5_233 driver exists");
        let node_id = node.id.clone();

        dispatch("graph.current", serde_json::json!(null), &state).expect("engine loaded");
        let _ = runtime_status(&state).expect("drain initial statuses");

        let out = dispatch(
            "graph.updateNode",
            serde_json::json!({
                "nodeId": node_id.clone(),
                "config": { "host": "x5.example", "tcpPort": 9073 }
            }),
            &state,
        )
        .expect("config update succeeds");

        let updated = out["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|candidate| candidate["id"] == node_id)
            .expect("node remains in graph");
        assert_eq!(updated["title"], "X5_233 Driver");
        assert_eq!(updated["config"]["host"], "x5.example");
        assert!(state.engine_runtime.engine().as_ref().is_some());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn graph_update_node_rejects_full_node_payload() {
        let dir = std::env::temp_dir().join(format!(
            "ws-router-graph-update-reject-{}",
            crate::next_revision()
        ));
        let state = test_state(dir.clone());
        let mut seed = crate::workflow::seed_workflow_graph();
        let node = seed.nodes.remove(0);
        let node_id = node.id.clone();

        let error = dispatch(
            "graph.updateNode",
            serde_json::json!({ "nodeId": node_id, "node": node }),
            &state,
        )
        .expect_err("full node payload is not a local update");
        assert!(error.contains("unknown field"));
        assert!(error.contains("node"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn graph_remove_edge_updates_authoritative_state_without_runtime_reset() {
        let dir = std::env::temp_dir().join(format!(
            "ws-router-graph-remove-edge-{}",
            crate::next_revision()
        ));
        let state = test_state(dir.clone());
        let seed = crate::workflow::seed_workflow_graph();
        let edge_id = seed.edges[0].id.clone();

        dispatch("graph.current", serde_json::json!(null), &state).expect("engine loaded");
        std::thread::sleep(std::time::Duration::from_millis(50));
        let _ = runtime_status(&state).expect("drain initial statuses");
        let out = dispatch(
            "graph.removeEdge",
            serde_json::json!({ "edgeId": edge_id.clone() }),
            &state,
        )
        .expect("edge removal succeeds");

        assert!(
            !out["edges"]
                .as_array()
                .unwrap()
                .iter()
                .any(|candidate| candidate["id"] == edge_id)
        );
        assert_eq!(runtime_status(&state).unwrap(), serde_json::json!([]));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn graph_update_node_positions_updates_multiple_nodes_without_runtime_reset() {
        let dir = std::env::temp_dir().join(format!(
            "ws-router-graph-update-positions-{}",
            crate::next_revision()
        ));
        let state = test_state(dir.clone());
        let seed = crate::workflow::seed_workflow_graph();
        let first_id = seed.nodes[0].id.clone();
        let second_id = seed.nodes[1].id.clone();

        dispatch("graph.current", serde_json::json!(null), &state).expect("engine loaded");
        std::thread::sleep(std::time::Duration::from_millis(50));
        let _ = runtime_status(&state).expect("drain initial statuses");

        let out = dispatch(
            "graph.updateNodePositions",
            serde_json::json!({
                "nodes": [
                    { "nodeId": first_id.clone(), "position": { "x": 111.0, "y": 222.0 } },
                    { "nodeId": second_id.clone(), "position": { "x": 333.0, "y": 444.0 } }
                ]
            }),
            &state,
        )
        .expect("batch position update succeeds");

        let nodes = out["nodes"].as_array().unwrap();
        let first = nodes
            .iter()
            .find(|candidate| candidate["id"] == first_id)
            .expect("first node remains");
        let second = nodes
            .iter()
            .find(|candidate| candidate["id"] == second_id)
            .expect("second node remains");
        assert_eq!(
            first["position"],
            serde_json::json!({ "x": 111.0, "y": 222.0 })
        );
        assert_eq!(
            second["position"],
            serde_json::json!({ "x": 333.0, "y": 444.0 })
        );
        assert_eq!(runtime_status(&state).unwrap(), serde_json::json!([]));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn graph_remove_selection_deletes_multiple_nodes_and_incident_edges() {
        let dir = std::env::temp_dir().join(format!(
            "ws-router-graph-remove-selection-{}",
            crate::next_revision()
        ));
        let state = test_state(dir.clone());
        let seed = crate::workflow::seed_workflow_graph();
        let node_ids: Vec<String> = seed
            .nodes
            .iter()
            .take(2)
            .map(|node| node.id.clone())
            .collect();
        assert_eq!(node_ids.len(), 2);
        let removed: std::collections::BTreeSet<String> = node_ids.iter().cloned().collect();

        let out = dispatch(
            "graph.removeSelection",
            serde_json::json!({ "nodeIds": node_ids, "edgeIds": [] }),
            &state,
        )
        .expect("selection removal succeeds");

        assert!(
            out["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .all(|node| { !removed.contains(node["id"].as_str().unwrap()) })
        );
        assert!(out["edges"].as_array().unwrap().iter().all(|edge| {
            !removed.contains(edge["source"]["nodeId"].as_str().unwrap())
                && !removed.contains(edge["target"]["nodeId"].as_str().unwrap())
        }));
        std::fs::remove_dir_all(dir).ok();
    }
    #[test]
    fn workflow_v1_is_rejected_by_import_save_and_validate() {
        let dir = std::env::temp_dir().join(format!("ws-router-v1-{}", crate::next_revision()));
        let state = test_state(dir.clone());
        let mut legacy = serde_json::to_value(crate::workflow::seed_workflow_graph()).unwrap();
        legacy["schemaVersion"] = serde_json::json!("workflow.v1");
        for path in ["workflow.import", "workflow.validate"] {
            let error =
                dispatch(path, legacy.clone(), &state).expect_err("workflow.v1 must be rejected");
            assert!(error.contains("workflow.v1"));
        }
        let error = dispatch(
            "workflow.save",
            serde_json::json!({ "graph": legacy, "revision": "seed" }),
            &state,
        )
        .expect_err("workflow.v1 save must be rejected");
        assert!(error.contains("workflow.v1"));
        std::fs::remove_dir_all(dir).ok();
    }
    #[test]
    fn workflow_get_rejects_persisted_workflow_v1() {
        let dir = std::env::temp_dir().join(format!("ws-router-get-v1-{}", crate::next_revision()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("legacy.ctworkflow.json"),
            br#"{"schemaVersion":"workflow.v1"}"#,
        )
        .unwrap();
        let state = test_state(dir.clone());
        let error = dispatch(
            "workflow.get",
            serde_json::json!({ "id": "legacy" }),
            &state,
        )
        .expect_err("workflow.get must reject v1");
        assert!(error.contains("workflow.v1"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn interface_update_returns_diff_and_replaces_runtime_after_validation() {
        let dir =
            std::env::temp_dir().join(format!("ws-router-interface-{}", crate::next_revision()));
        let state = test_state(dir.clone());
        let template = crate::workflow::workmode_templates()
            .into_iter()
            .find(|template| template.id == "i2c-plan-workflow")
            .unwrap()
            .graph;
        *state.graph_session.lock().unwrap() = template.clone();
        let extractor = template
            .nodes
            .iter()
            .find(|node| node.id == "field-extractor")
            .unwrap();
        let builder = template
            .nodes
            .iter()
            .find(|node| node.id == "task-builder")
            .unwrap();
        let mut outputs = extractor.config["outputs"].as_array().unwrap().clone();
        outputs.push(serde_json::json!({ "id": "calibration.quality.rms_error", "pointer": "/fields/7", "type": "f64" }));
        let output = dispatch(
            "graph.updateExtractorAndTaskBuilderInterface",
            serde_json::json!({
                "extractorNodeId": extractor.id,
                "extractorOutputs": outputs,
                "taskBuilderNodeId": builder.id,
                "taskBuilderConfig": builder.config,
            }),
            &state,
        )
        .expect("validated interface update succeeds");
        assert_eq!(output["diagnostics"].as_array().unwrap().len(), 2);
        assert!(
            output["candidatePortDiff"]
                .as_array()
                .unwrap()
                .iter()
                .any(|diff| diff["nodeId"] == "field-extractor"
                    && diff["addedOutputs"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|id| id == "calibration.quality.rms_error"))
        );
        assert!(state.engine_runtime.engine().as_ref().is_some());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn interface_update_rejects_edges_removed_by_candidate_ports() {
        let dir = std::env::temp_dir().join(format!(
            "ws-router-interface-invalid-{}",
            crate::next_revision()
        ));
        let state = test_state(dir.clone());
        let template = crate::workflow::workmode_templates()
            .into_iter()
            .find(|template| template.id == "i2c-plan-workflow")
            .unwrap()
            .graph;
        *state.graph_session.lock().unwrap() = template.clone();
        rebuild_engine_from_graph(&template, &state).expect("template engine builds");
        let graph_before = state.graph_session.lock().unwrap().clone();
        let engine_before = {
            let engine = state.engine_runtime.engine();
            engine
                .as_ref()
                .map(|engine| engine as *const _)
                .expect("template engine is loaded")
        };
        let extractor = template
            .nodes
            .iter()
            .find(|node| node.id == "field-extractor")
            .unwrap();
        let builder = template
            .nodes
            .iter()
            .find(|node| node.id == "task-builder")
            .unwrap();
        let mut outputs = extractor.config["outputs"].as_array().unwrap().clone();
        outputs.retain(|output| output["id"] != "camera.model.id");
        let error = dispatch(
            "graph.updateExtractorAndTaskBuilderInterface",
            serde_json::json!({
                "extractorNodeId": extractor.id,
                "extractorOutputs": outputs,
                "taskBuilderNodeId": builder.id,
                "taskBuilderConfig": builder.config,
            }),
            &state,
        )
        .expect_err("candidate port removal must reject retained edge");
        assert!(error.contains("camera.model.id"));
        assert_eq!(*state.graph_session.lock().unwrap(), graph_before);
        let engine_after = {
            let engine = state.engine_runtime.engine();
            engine
                .as_ref()
                .map(|engine| engine as *const _)
                .expect("failed candidate retains loaded engine")
        };
        assert_eq!(engine_before, engine_after);
        std::fs::remove_dir_all(dir).ok();
    }
    #[test]
    fn generic_config_updates_reject_dynamic_node_interfaces() {
        let dir = std::env::temp_dir().join(format!(
            "ws-router-interface-generic-patch-{}",
            crate::next_revision()
        ));
        let state = test_state(dir.clone());
        let template = crate::workflow::workmode_templates()
            .into_iter()
            .find(|template| template.id == "i2c-plan-workflow")
            .unwrap()
            .graph;
        *state.graph_session.lock().unwrap() = template.clone();
        let builder = template
            .nodes
            .iter()
            .find(|node| node.id == "task-builder")
            .unwrap();

        let patch_error = dispatch(
            "graph.patchNode",
            serde_json::json!({
                "nodeId": "field-extractor",
                "config": { "outputs": [] },
            }),
            &state,
        )
        .expect_err("generic extractor patch must be rejected");
        assert!(patch_error.contains("graph.updateExtractorAndTaskBuilderInterface"));

        let update_error = dispatch(
            "graph.updateNode",
            serde_json::json!({
                "nodeId": builder.id,
                "config": builder.config.clone(),
            }),
            &state,
        )
        .expect_err("generic task builder update must be rejected");
        assert!(update_error.contains("graph.updateExtractorAndTaskBuilderInterface"));
        assert_eq!(*state.graph_session.lock().unwrap(), template);
        std::fs::remove_dir_all(dir).ok();
    }

    fn test_state(dir: std::path::PathBuf) -> AppState {
        AppState {
            workflow_store: std::sync::Arc::new(crate::WorkflowStore { dir }),
            control_runtime: std::sync::Arc::new(crate::ControlRuntime::production()),
            #[cfg(feature = "calibration-opencv")]
            calibration_backend: std::sync::Arc::new(
                camera_toolbox_adapters::OpenCvCalibrationBackend,
            ),
            engine_runtime: std::sync::Arc::new(crate::engine_api::EngineRuntime::new()),
            ws_hub: std::sync::Arc::new(crate::ws_hub::WsHub::new()),
            graph_session: std::sync::Arc::new(std::sync::Mutex::new(
                crate::workflow::seed_workflow_graph(),
            )),
        }
    }

    #[test]
    fn runtime_node_action_request_defaults_missing_payload_to_null() {
        let body: RuntimeNodeActionBody = serde_json::from_value(serde_json::json!({
            "nodeId": "dataset-1",
            "action": "clear"
        }))
        .expect("legacy action request remains valid");

        assert_eq!(body.node_id, "dataset-1");
        assert_eq!(body.action, "clear");
        assert_eq!(body.payload, Value::Null);
    }

    #[test]
    fn deser_err_wraps_message() {
        let err = deser_err("boom");
        assert_eq!(err, "invalid payload: boom");
    }
}
